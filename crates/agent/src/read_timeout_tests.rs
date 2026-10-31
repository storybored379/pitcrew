use super::*;
use crate::{http::Operation, runtime::inspection_response};
use read_timeout::CONTROL_BYTES;
use std::sync::{Condvar, Mutex};

#[derive(Clone, Copy)]
enum Behavior {
    Block,
    Pass,
    Fail,
    Short,
    Late,
}

struct FakeReader {
    targets: Mutex<VecDeque<Behavior>>,
    control: Behavior,
    calls: Mutex<Vec<(bool, u64, usize)>>,
    control_started: tokio::sync::Notify,
    release: Arc<(Mutex<bool>, Condvar)>,
    change_file: Option<PathBuf>,
}

// Release all deliberately blocked fake workers even when an assertion panics.
struct Release(Arc<(Mutex<bool>, Condvar)>);
impl Drop for Release {
    fn drop(&mut self) {
        *self.0.0.lock().unwrap() = true;
        self.0.1.notify_all();
    }
}

impl FakeReader {
    fn new(targets: &[Behavior], control: Behavior) -> (Self, Release) {
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        (
            Self {
                targets: Mutex::new(targets.iter().copied().collect()),
                control,
                calls: Mutex::new(Vec::new()),
                control_started: tokio::sync::Notify::new(),
                release: Arc::clone(&release),
                change_file: None,
            },
            Release(release),
        )
    }

    fn execute(&self, behavior: Behavior, file: &File, offset: u64, count: usize) -> TimedRead {
        if matches!(behavior, Behavior::Block) {
            drop(
                self.release
                    .1
                    .wait_while(self.release.0.lock().unwrap(), |released| !*released)
                    .unwrap(),
            );
        }
        let mut read = timed_read(file, offset, count);
        match behavior {
            Behavior::Fail => read.outcome = ProbeOutcome::Failed,
            Behavior::Short => read.read_bytes = 0,
            Behavior::Late => read.elapsed_ms = 50,
            _ => {}
        }
        read
    }
}

impl RangeReader for FakeReader {
    fn read(&self, file: &File, offset: u64, count: usize) -> TimedRead {
        self.calls.lock().unwrap().push((false, offset, count));
        let behavior = self
            .targets
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Behavior::Block);
        self.execute(behavior, file, offset, count)
    }

    fn control(&self, file: &File, offset: u64, count: usize) -> TimedRead {
        self.calls.lock().unwrap().push((true, offset, count));
        self.control_started.notify_one();
        if let Some(path) = &self.change_file {
            fs::write(path, b"changed during timeout confirmation").unwrap();
        }
        self.execute(self.control, file, offset, count)
    }
}

fn timeout_config(root: &Path) -> MediaConfig {
    MediaConfig {
        allowed_roots: vec![root.into()],
        read_timeout: Duration::from_millis(100),
        read_timeout_policy: pitcrew_config::ReadTimeoutPolicy {
            enabled: true,
            attempts: 3,
            control_timeout_ms: 50,
        },
        read_offsets_bytes: vec![4096],
        read_bytes: 4096,
        // These paths must never be executed on a read timeout.
        ffprobe: "/unavailable-probe".into(),
        ffmpeg: "/unavailable-decode".into(),
        ..Default::default()
    }
}

fn test_file(root: &Path) -> PathBuf {
    let path = root.join("synthetic.bin");
    fs::write(&path, vec![7; 4 * CONTROL_BYTES]).unwrap();
    path
}

async fn inspect_fake(
    agent: &MediaAgent,
    path: &Path,
    reader: Arc<FakeReader>,
) -> Result<MediaInspection, MediaError> {
    agent
        .inspect_using(
            path,
            true,
            |path, roots, hops| secure_open(&path, &roots, hops),
            reader,
        )
        .await
}

fn response(report: &MediaInspection) -> Value {
    inspection_response(
        Operation::Decode {
            path: "/media/synthetic.bin".into(),
        },
        report,
    )
    .unwrap()
}

#[tokio::test]
async fn default_read_timeout_remains_unknown_without_extra_reads() {
    let temp = tempfile::tempdir().unwrap();
    let path = test_file(temp.path());
    let mut config = timeout_config(temp.path());
    config.read_timeout_policy = pitcrew_config::ReadTimeoutPolicy::default();
    let agent = MediaAgent::new(config).unwrap();
    let (reader, _release) = FakeReader::new(&[], Behavior::Pass);
    let reader = Arc::new(reader);
    let report = inspect_fake(&agent, &path, Arc::clone(&reader))
        .await
        .unwrap();
    let response = response(&report);
    assert_eq!(response["verdict"], "unknown");
    assert!(response.get("read_timeout_confirmation").is_none());
    assert_eq!(*reader.calls.lock().unwrap(), [(false, 4096, 4096)]);
}

#[tokio::test]
async fn uniformly_slow_healthy_storage_is_never_classified_bad() {
    struct SameThroughput;
    impl RangeReader for SameThroughput {
        fn read(&self, file: &File, offset: u64, count: usize) -> TimedRead {
            let started = Instant::now();
            std::thread::sleep(Duration::from_micros(count as u64 * 5));
            let mut read = timed_read(file, offset, count);
            read.elapsed_ms = millis(started.elapsed());
            read
        }
        fn control(&self, file: &File, offset: u64, count: usize) -> TimedRead {
            self.read(file, offset, count)
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("healthy-slow.bin");
    fs::write(&path, vec![7; 128 * 1024]).unwrap();
    let mut config = timeout_config(temp.path());
    config.read_bytes = 32 * 1024;
    config.read_offsets_bytes = vec![0];
    let agent = MediaAgent::new(config).unwrap();
    let report = agent
        .inspect_using(
            &path,
            true,
            |path, roots, hops| secure_open(&path, &roots, hops),
            Arc::new(SameThroughput),
        )
        .await
        .unwrap();
    assert_eq!(response(&report)["verdict"], "unknown");
}

#[tokio::test]
async fn repeated_same_range_timeout_with_prompt_controls_is_bad_and_auditable() {
    let temp = tempfile::tempdir().unwrap();
    let path = test_file(temp.path());
    for attempts in [2, 3, 5] {
        let mut config = timeout_config(temp.path());
        config.read_timeout_policy.attempts = attempts;
        config.max_filesystem_workers = usize::from(attempts) + 1;
        let agent = MediaAgent::new(config).unwrap();
        let (reader, _release) = FakeReader::new(&[], Behavior::Pass);
        let reader = Arc::new(reader);
        let report = inspect_fake(&agent, &path, Arc::clone(&reader))
            .await
            .unwrap();
        let response = response(&report);
        assert_eq!(response["verdict"], "bad");
        let proof = &response["read_timeout_confirmation"];
        assert_eq!(proof["status"], "confirmed");
        assert_eq!(proof["required_attempts"], attempts);
        assert_eq!(proof["control_timeout_ms"], 50);
        let reads = proof["attempts"].as_array().unwrap();
        assert_eq!(reads.len(), usize::from(attempts));
        assert!(reads.iter().all(|read| {
            read["offset_bytes"] == 4096
                && read["requested_bytes"] == 4096
                && read["timeout_ms"] == 100
                && read["outcome"] == "TimedOut"
        }));
        let controls = proof["controls"].as_array().unwrap();
        assert_eq!(controls.len(), reads.len());
        assert!(controls.iter().all(|read| {
            read["offset_bytes"] == 0
                && read["requested_bytes"] == CONTROL_BYTES
                && read["read_bytes"] == CONTROL_BYTES
                && read["outcome"] == "Passed"
                && read["timeout_ms"] == 50
        }));
        assert_eq!(
            *reader.calls.lock().unwrap(),
            [(false, 4096, 4096), (true, 0, CONTROL_BYTES)].repeat(usize::from(attempts))
        );
        assert_eq!(agent.workers.0.available_permits(), 1);
        assert!(report.evidence.header.is_none());
        assert!(report.evidence.decode.is_none());
        assert_eq!(report.evidence.reads.len(), 1);
        assert_eq!(report.evidence.reads[0].outcome, ProbeOutcome::TimedOut);
        assert_eq!(
            crate::runtime::decode_response(&report.evidence)["verdict"],
            "unknown",
            "generic evidence without the local confirmation never promotes a timeout"
        );
        for operation in [
            Operation::Probe {
                path: String::new(),
            },
            Operation::File {
                path: String::new(),
            },
            Operation::Evidence {
                path: String::new(),
                decode: true,
            },
        ] {
            let value = inspection_response(operation, &report).unwrap();
            assert!(value.get("verdict").is_none());
            assert_eq!(value["read_timeout_confirmation"], *proof);
        }
    }
}

#[tokio::test]
async fn hung_slow_short_and_failed_controls_never_make_a_file_bad() {
    let temp = tempfile::tempdir().unwrap();
    let path = test_file(temp.path());
    for behavior in [
        Behavior::Block,
        Behavior::Fail,
        Behavior::Short,
        Behavior::Late,
    ] {
        let agent = MediaAgent::new(timeout_config(temp.path())).unwrap();
        let (reader, _release) = FakeReader::new(&[], behavior);
        let reader = Arc::new(reader);
        let report = inspect_fake(&agent, &path, Arc::clone(&reader))
            .await
            .unwrap();
        let value = response(&report);
        assert_eq!(value["verdict"], "unknown");
        assert_eq!(
            value["read_timeout_confirmation"]["status"],
            "control_unavailable"
        );
        assert_eq!(reader.calls.lock().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn recovered_or_failed_retry_does_not_complete_timeout_confirmation() {
    let temp = tempfile::tempdir().unwrap();
    let path = test_file(temp.path());
    for behavior in [Behavior::Pass, Behavior::Fail] {
        let agent = MediaAgent::new(timeout_config(temp.path())).unwrap();
        let (reader, _release) = FakeReader::new(&[Behavior::Block, behavior], Behavior::Pass);
        let reader = Arc::new(reader);
        let report = inspect_fake(&agent, &path, Arc::clone(&reader))
            .await
            .unwrap();
        let value = response(&report);
        assert_eq!(value["verdict"], "unknown");
        assert_eq!(
            value["read_timeout_confirmation"]["status"],
            "retry_not_timed_out"
        );
        assert_eq!(reader.calls.lock().unwrap().len(), 3);
    }
}

#[tokio::test]
async fn missing_disjoint_range_and_exhausted_workers_stay_unknown() {
    let temp = tempfile::tempdir().unwrap();
    let path = test_file(temp.path());
    for (offset, count, workers, expected, calls) in [
        (0, 4 * CONTROL_BYTES, 4, "no_control_range", 1),
        (0, CONTROL_BYTES, 1, "capacity_exhausted", 1),
        (0, CONTROL_BYTES, 2, "capacity_exhausted", 3),
    ] {
        let mut config = timeout_config(temp.path());
        config.read_offsets_bytes = vec![offset];
        config.read_bytes = count;
        config.max_filesystem_workers = workers;
        let agent = MediaAgent::new(config).unwrap();
        let (reader, _release) = FakeReader::new(&[], Behavior::Pass);
        let reader = Arc::new(reader);
        let report = inspect_fake(&agent, &path, Arc::clone(&reader))
            .await
            .unwrap();
        let value = response(&report);
        assert_eq!(value["verdict"], "unknown");
        assert_eq!(value["read_timeout_confirmation"]["status"], expected);
        assert_eq!(reader.calls.lock().unwrap().len(), calls);
    }
}

#[tokio::test]
async fn confirmed_timeouts_revalidate_the_exact_file_identity() {
    let temp = tempfile::tempdir().unwrap();
    let path = test_file(temp.path());
    let agent = MediaAgent::new(timeout_config(temp.path())).unwrap();
    let (mut reader, _release) = FakeReader::new(&[], Behavior::Pass);
    reader.change_file = Some(path.clone());
    // Supply a full control even after changing the file so final identity
    // revalidation, rather than a short read, is responsible for the refusal.
    struct ChangingReader(FakeReader);
    impl RangeReader for ChangingReader {
        fn read(&self, file: &File, offset: u64, count: usize) -> TimedRead {
            self.0.read(file, offset, count)
        }
        fn control(&self, file: &File, offset: u64, count: usize) -> TimedRead {
            let mut read = self.0.control(file, offset, count);
            read.read_bytes = count as u64;
            read
        }
    }
    let result = agent
        .inspect_using(
            &path,
            true,
            |path, roots, hops| secure_open(&path, &roots, hops),
            Arc::new(ChangingReader(reader)),
        )
        .await;
    assert!(matches!(result, Err(MediaError::Changed)));
}

#[tokio::test]
async fn cancelling_confirmation_keeps_bounded_workers_until_the_reads_return() {
    let temp = tempfile::tempdir().unwrap();
    let path = test_file(temp.path());
    let agent = MediaAgent::new(timeout_config(temp.path())).unwrap();
    let (reader, release) = FakeReader::new(&[], Behavior::Block);
    let reader = Arc::new(reader);
    let mut inspection = Box::pin(inspect_fake(&agent, &path, Arc::clone(&reader)));
    tokio::time::timeout(Duration::from_secs(2), async {
        tokio::select! {
            _ = &mut inspection => panic!("inspection finished before cancellation"),
            _ = reader.control_started.notified() => {}
        }
    })
    .await
    .unwrap();
    drop(inspection);
    assert_eq!(reader.calls.lock().unwrap().len(), 2);
    assert_eq!(agent.workers.0.available_permits(), 2);
    drop(release);
    tokio::time::timeout(Duration::from_secs(2), async {
        while agent.workers.0.available_permits() < 4 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
