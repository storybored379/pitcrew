use super::*;
use std::{
    os::unix::fs::{PermissionsExt, symlink},
    sync::atomic::{AtomicBool, Ordering},
};
use tempfile::TempDir;

fn config(root: &Path) -> MediaConfig {
    MediaConfig {
        allowed_roots: vec![root.to_path_buf()],
        filesystem_timeout: Duration::from_millis(500),
        process_timeout: Duration::from_secs(4),
        header_timeout: Duration::from_secs(4),
        read_timeout: Duration::from_millis(500),
        ..MediaConfig::default()
    }
}

fn fixture(root: &Path, name: &str, extra: &[&str]) -> PathBuf {
    let path = root.join(name);
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-nostdin",
            "-f",
            "lavfi",
            "-i",
            "color=c=blue:s=32x24:r=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=8000",
            "-t",
            "0.5",
            "-threads",
            "1",
            "-c:v",
            "mpeg4",
            "-c:a",
            "aac",
        ])
        .args(extra)
        .arg(&path)
        .status()
        .expect("ffmpeg is a required test dependency");
    assert!(status.success());
    path
}

fn script(root: &Path, name: &str, body: &str) -> PathBuf {
    let path = root.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    std::thread::sleep(Duration::from_millis(10));
    path
}

fn probe() -> MediaProbe {
    MediaProbe {
        outcome: ProbeOutcome::Passed,
        duration_ms: None,
        elapsed_ms: 0,
        timeout_ms: 1,
        faults: vec![],
        streams: vec![],
        tested_ranges: vec![],
    }
}

#[tokio::test]
async fn real_media_good_random_truncated_and_no_duration() {
    let dir = TempDir::new().unwrap();
    let good = fixture(dir.path(), "good.mkv", &[]);
    let agent = MediaAgent::new(config(dir.path())).unwrap();
    let result = agent.inspect(&good, true).await.unwrap();
    assert_ne!(result.file, FileIdentity { id: 0, revision: 0 });
    assert_eq!(result.mount.unwrap().outcome, ProbeOutcome::Passed);
    assert!(
        result
            .reads
            .iter()
            .all(|r| r.outcome == ProbeOutcome::Passed
                && r.read_bytes > 0
                && r.first_byte_ms.is_some()
                && r.bytes_per_second > 0)
    );
    let header = result.header.unwrap();
    assert_eq!(header.outcome, ProbeOutcome::Passed);
    assert!(
        header
            .duration_ms
            .is_some_and(|ms| (400..1000).contains(&ms))
    );
    assert!(header.streams.iter().any(|s| matches!(
        s,
        StreamFacts::Video {
            width: 32,
            height: 24,
            ..
        }
    )));
    let decoded = result.decode.unwrap();
    assert_eq!(decoded.outcome, ProbeOutcome::Passed);
    assert_eq!(decoded.tested_ranges.len(), 3);
    assert_eq!(decoded.tested_ranges[0].start_ms, 0);
    assert!(
        decoded.tested_ranges[0]
            .end_ms
            .is_some_and(|ms| ms > 400 && ms < 1000)
    );
    let random = dir.path().join("random.mkv");
    fs::write(&random, vec![0x3a; 4096]).unwrap();
    let bad = agent.inspect(&random, true).await.unwrap();
    assert_eq!(bad.header.unwrap().faults, vec![MediaFault::InvalidHeader]);
    assert!(bad.decode.is_none());
    let truncated = dir.path().join("truncated.mkv");
    let mut bytes = fs::read(&good).unwrap();
    bytes.truncate(bytes.len() / 2);
    fs::write(&truncated, bytes).unwrap();
    let bad = agent.inspect(&truncated, true).await.unwrap();
    assert!(
        bad.header.unwrap().outcome != ProbeOutcome::Passed
            || bad
                .decode
                .is_some_and(|d| d.outcome != ProbeOutcome::Passed)
    );
    let raw = dir.path().join("raw.h264");
    assert!(
        std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-nostdin",
                "-f",
                "lavfi",
                "-i",
                "color=s=32x24:r=10",
                "-t",
                "0.5",
                "-threads",
                "1",
                "-c:v",
                "libx264",
                "-f",
                "h264"
            ])
            .arg(&raw)
            .status()
            .unwrap()
            .success()
    );
    let raw = agent.inspect(&raw, true).await.unwrap();
    let header = raw.header.unwrap();
    assert_eq!(header.outcome, ProbeOutcome::Passed);
    assert!(header.faults.contains(&MediaFault::NoDuration));
    assert!(header.faults.contains(&MediaFault::NoAudio));
}

#[tokio::test]
async fn final_symlink_allowlist_chain_dangling_cycle_and_hop_limit() {
    let dir = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let target = dir.path().join("target");
    fs::write(&target, b"hello").unwrap();
    symlink("target", dir.path().join("second")).unwrap();
    symlink("second", dir.path().join("first")).unwrap();
    let opened = secure_open(&dir.path().join("first"), &[dir.path().into()], 40).unwrap();
    assert_eq!(opened.symlinks.chain.len(), 2);
    assert_eq!(opened.symlinks.chain[0].target, PathBuf::from("second"));
    assert_eq!(opened.symlinks.chain[1].target, PathBuf::from("target"));
    assert_eq!(
        opened.symlinks.resolution,
        SymlinkResolution::Resolved(target.clone())
    );
    assert_eq!(
        resolve(&dir.path().join("first"), 1).1,
        SymlinkResolution::HopLimit
    );
    symlink("missing", dir.path().join("dangling")).unwrap();
    let agent = MediaAgent::new(config(dir.path())).unwrap();
    let missing = agent
        .inspect(&dir.path().join("dangling"), true)
        .await
        .unwrap();
    assert_eq!(
        missing.symlinks.unwrap().resolution,
        SymlinkResolution::Dangling(dir.path().join("missing"))
    );
    assert_eq!(missing.mount.unwrap().outcome, ProbeOutcome::Unavailable);
    assert!(missing.header.is_none());
    symlink("loop_b", dir.path().join("loop_a")).unwrap();
    symlink("loop_a", dir.path().join("loop_b")).unwrap();
    assert_eq!(
        resolve(&dir.path().join("loop_a"), 40).1,
        SymlinkResolution::Cycle
    );
    let outside_file = outside.path().join("secret");
    fs::write(&outside_file, b"not media").unwrap();
    symlink(&outside_file, dir.path().join("escape")).unwrap();
    assert_eq!(
        agent
            .inspect(&dir.path().join("escape"), false)
            .await
            .unwrap_err(),
        MediaError::Forbidden
    );
    assert_eq!(
        agent.inspect(&outside_file, false).await.unwrap_err(),
        MediaError::Forbidden
    );
    assert_eq!(
        agent
            .inspect(&dir.path().join("../bad"), false)
            .await
            .unwrap_err(),
        MediaError::Forbidden
    );
    let mut c = config(dir.path());
    c.storage_roots = vec![outside.path().into()];
    let split = MediaAgent::new(c).unwrap();
    assert_eq!(
        split.inspect(&outside_file, false).await.unwrap_err(),
        MediaError::Forbidden
    );
    assert!(
        split
            .inspect(&dir.path().join("escape"), false)
            .await
            .unwrap()
            .header
            .is_some()
    );
    assert_eq!(
        split.inspect(&target, false).await.unwrap_err(),
        MediaError::Forbidden
    );
}

#[test]
fn descriptor_pins_old_file_and_detects_move_outside_root() {
    let dir = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let path = dir.path().join("file");
    fs::write(&path, b"safe").unwrap();
    let opened = secure_open(&path, &[dir.path().into()], 40).unwrap();
    let file = opened.file.unwrap();
    fs::rename(&path, dir.path().join("original")).unwrap();
    symlink(outside.path().join("secret"), &path).unwrap();
    let mut bytes = [0; 4];
    file.read_at(&mut bytes, 0).unwrap();
    assert_eq!(&bytes, b"safe");
    fs::rename(dir.path().join("original"), outside.path().join("moved")).unwrap();
    assert_eq!(
        verify_descriptor(&file, &[dir.path().into()]),
        Err(MediaError::Forbidden)
    );
    fs::write(outside.path().join("moved"), b"changed length").unwrap();
    assert_ne!(
        Stamp::from(&file.metadata().unwrap()),
        opened.stamp.unwrap()
    );
}

#[test]
fn no_symlink_root_or_special_file_opens() {
    let dir = TempDir::new().unwrap();
    let real = TempDir::new().unwrap();
    fs::write(real.path().join("file"), b"data").unwrap();
    let alias = dir.path().join("alias");
    symlink(real.path(), &alias).unwrap();
    assert!(matches!(
        secure_open(&alias.join("file"), &[alias], 40),
        Err(MediaError::Forbidden)
    ));
    let directory = dir.path().join("directory");
    fs::create_dir(&directory).unwrap();
    assert!(matches!(
        secure_open(&directory, &[dir.path().into()], 40),
        Err(MediaError::Forbidden)
    ));
}

#[tokio::test]
async fn abandoned_filesystem_workers_are_bounded_and_never_awaited() {
    let pool = WorkerPool(Arc::new(Semaphore::new(1)));
    let (release, wait) = std::sync::mpsc::channel();
    let done = Arc::new(AtomicBool::new(false));
    let completed = done.clone();
    let start = Instant::now();
    assert_eq!(
        pool.run(Duration::from_millis(20), move || {
            wait.recv_timeout(Duration::from_secs(2)).unwrap();
            completed.store(true, Ordering::SeqCst);
            42
        })
        .await
        .unwrap(),
        None
    );
    assert!(start.elapsed() < Duration::from_millis(200));
    assert_eq!(
        pool.run(Duration::from_millis(20), || 1).await,
        Err(MediaError::Busy)
    );
    assert!(!done.load(Ordering::SeqCst));
    release.send(()).unwrap();
    for _ in 0..100 {
        if done.load(Ordering::SeqCst) && pool.0.available_permits() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(
        pool.run(Duration::from_secs(1), || 7).await.unwrap(),
        Some(7)
    );
}

#[tokio::test]
async fn process_deadline_output_limit_missing_binary_and_reap() {
    let dir = TempDir::new().unwrap();
    let media = dir.path().join("file");
    fs::write(&media, b"data").unwrap();
    let pidfile = dir.path().join("pid");
    let sleeper = script(
        dir.path(),
        "sleepy",
        &format!("echo $$ > '{}'\nexec sleep 30", pidfile.display()),
    );
    let mut c = config(dir.path());
    c.ffprobe = sleeper;
    c.header_timeout = Duration::from_millis(100);
    let agent = MediaAgent::new(c).unwrap();
    let start = Instant::now();
    let result = agent.inspect(&media, true).await.unwrap();
    assert_eq!(result.header.unwrap().outcome, ProbeOutcome::TimedOut);
    assert!(result.decode.is_none());
    assert!(start.elapsed() < Duration::from_secs(1));
    let pid = fs::read_to_string(&pidfile).unwrap();
    let proc = PathBuf::from(format!("/proc/{}", pid.trim()));
    for _ in 0..100 {
        if !proc.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert!(!proc.exists(), "timed-out child must be reaped");
    let noisy = script(dir.path(), "noisy", "head -c 100000 /dev/zero");
    let mut c = config(dir.path());
    c.ffprobe = noisy;
    assert_eq!(
        MediaAgent::new(c)
            .unwrap()
            .inspect(&media, true)
            .await
            .unwrap()
            .header
            .unwrap()
            .outcome,
        ProbeOutcome::Failed
    );
    let mut c = config(dir.path());
    c.ffprobe = dir.path().join("absent");
    assert_eq!(
        MediaAgent::new(c)
            .unwrap()
            .inspect(&media, true)
            .await
            .unwrap()
            .header
            .unwrap()
            .outcome,
        ProbeOutcome::Unavailable
    );
}

#[tokio::test]
async fn reads_offsets_latency_throughput_and_slow_evidence() {
    let dir = TempDir::new().unwrap();
    let media = fixture(dir.path(), "good.mkv", &[]);
    let mut c = config(dir.path());
    c.read_bytes = 128;
    c.read_offsets_bytes = vec![100, 0, 100];
    c.slow_bytes_per_second = u64::MAX;
    let evidence = MediaAgent::new(c)
        .unwrap()
        .inspect(&media, false)
        .await
        .unwrap();
    assert_eq!(
        evidence
            .reads
            .iter()
            .map(|r| r.offset_bytes)
            .collect::<Vec<_>>(),
        vec![0, 100]
    );
    assert!(
        evidence
            .reads
            .iter()
            .all(|r| r.requested_bytes == 128 && r.read_bytes == 128 && r.timeout_ms == 500)
    );
    assert!(
        evidence
            .header
            .unwrap()
            .faults
            .contains(&MediaFault::SlowRead)
    );
    let file = File::open(media).unwrap();
    let read = timed_read(&file, u64::MAX, 1);
    assert_eq!(read.outcome, ProbeOutcome::Failed);
    assert_eq!(read.read_bytes, 0);
    assert_eq!(read.bytes_per_second, 0);
    let eof = timed_read(&file, file.metadata().unwrap().len(), 64);
    assert_eq!(eof.outcome, ProbeOutcome::Passed);
    assert_eq!(eof.first_byte_ms, None);
    assert_eq!(eof.read_bytes, 0);
    let single = timed_read(&file, 0, 1);
    assert_eq!(single.read_bytes, 1);
}

#[test]
fn parses_streams_and_preserves_absent_invalid_duration() {
    let mut actual = probe();
    parse_header(br#"{"format":{"duration":"1.234"},"streams":[{"codec_type":"video","codec_name":"h264","width":1280,"height":720},{"codec_type":"audio","codec_name":"aac","channels":2,"tags":{"language":"eng"}},{"codec_type":"subtitle","codec_name":"srt","tags":{"language":"spa"}},{"codec_type":"data"}]}"#, &mut actual);
    assert_eq!(actual.duration_ms, Some(1234));
    assert!(actual.faults.is_empty());
    assert_eq!(
        actual.streams,
        vec![
            StreamFacts::Video {
                codec: "h264".into(),
                width: 1280,
                height: 720,
                bit_depth: None,
                dynamic_range: None
            },
            StreamFacts::Audio {
                codec: "aac".into(),
                channels: 2,
                language: Some("eng".into())
            },
            StreamFacts::Subtitle {
                codec: "srt".into(),
                language: Some("spa".into())
            }
        ]
    );
    for duration in ["NaN", "inf", "-1", "0", "bad", "1e30"] {
        let mut p = probe();
        parse_header(
            format!(
                r#"{{"format":{{"duration":"{duration}"}},"streams":[{{"codec_type":"audio"}}]}}"#
            )
            .as_bytes(),
            &mut p,
        );
        assert_eq!(p.duration_ms, None);
        assert_eq!(p.outcome, ProbeOutcome::Passed);
        assert_eq!(p.faults, vec![MediaFault::NoDuration, MediaFault::NoVideo]);
        assert_eq!(
            p.streams,
            vec![StreamFacts::Audio {
                codec: "unknown".into(),
                channels: 0,
                language: None
            }]
        );
    }
    let mut p = probe();
    parse_header(b"not json", &mut p);
    assert_eq!(p.outcome, ProbeOutcome::Failed);
    assert_eq!(p.faults, vec![MediaFault::InvalidHeader]);
    let mut p = probe();
    parse_header(b"{}", &mut p);
    assert_eq!(p.outcome, ProbeOutcome::Failed);
    assert_eq!(
        p.faults,
        vec![
            MediaFault::NoDuration,
            MediaFault::NoVideo,
            MediaFault::NoAudio,
            MediaFault::InvalidHeader
        ]
    );
}

#[test]
fn configuration_is_fail_closed_and_defaults_are_bounded() {
    assert!(matches!(
        MediaAgent::new(MediaConfig::default()),
        Err(MediaError::InvalidConfig)
    ));
    let dir = TempDir::new().unwrap();
    for root in [
        PathBuf::from("/"),
        PathBuf::from("relative"),
        PathBuf::from("/allowed/../other"),
    ] {
        let mut c = config(dir.path());
        c.allowed_roots = vec![root];
        assert!(matches!(MediaAgent::new(c), Err(MediaError::InvalidConfig)));
    }
    let setters: &[fn(&mut MediaConfig)] = &[
        |c| c.filesystem_timeout = Duration::ZERO,
        |c| c.process_timeout = Duration::ZERO,
        |c| c.header_timeout = Duration::ZERO,
        |c| c.read_timeout = Duration::ZERO,
        |c| c.slow_read_threshold = Duration::ZERO,
        |c| c.read_bytes = 0,
        |c| c.read_bytes = 1024 * 1024 + 1,
        |c| c.decode_seconds = 0,
        |c| c.decode_seconds = 301,
        |c| c.decode_offsets_percent = vec![],
        |c| c.decode_offsets_percent = vec![0; 17],
        |c| c.decode_offsets_percent = vec![100],
        |c| c.decode_offsets_percent = vec![80, 40],
        |c| c.decode_offsets_percent = vec![0, 0],
        |c| c.max_requests = 0,
        |c| c.max_requests = 65,
        |c| c.max_filesystem_workers = 0,
        |c| c.max_filesystem_workers = 65,
        |c| c.ffprobe = PathBuf::new(),
        |c| c.ffmpeg = PathBuf::new(),
        |c| c.symlink_hop_limit = 0,
        |c| c.symlink_hop_limit = 41,
        |c| c.read_offsets_bytes = vec![0; 17],
    ];
    for setter in setters {
        let mut c = config(dir.path());
        setter(&mut c);
        assert!(matches!(MediaAgent::new(c), Err(MediaError::InvalidConfig)));
    }
    let mut largest_plan = config(dir.path());
    largest_plan.decode_offsets_percent = (0..16).collect();
    assert!(MediaAgent::new(largest_plan).is_ok());
    assert_eq!(millis(Duration::from_micros(1234)), 1);
    assert_eq!(millis(Duration::MAX), u64::MAX);
}

proptest::proptest! {
    #[test]
    fn path_component_allowlist_never_accepts_prefix_siblings(suffix in "[a-zA-Z0-9_-]{1,32}") {
        let root = PathBuf::from("/media/library");
        proptest::prop_assert!(within(&root.join(&suffix), std::slice::from_ref(&root)));
        let sibling = PathBuf::from(format!("/media/library{suffix}/file"));
        proptest::prop_assert!(!within(&sibling, &[root]));
    }
}

#[test]
fn swap_after_resolution_cannot_escape_through_file_or_parent() {
    let dir = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    fs::write(outside.path().join("file"), b"secret").unwrap();
    let parent = dir.path().join("folder");
    fs::create_dir(&parent).unwrap();
    let path = parent.join("file");
    fs::write(&path, b"safe").unwrap();
    let (_, resolution) = resolve(&path, 40);
    let SymlinkResolution::Resolved(resolved) = resolution else {
        panic!()
    };
    fs::remove_file(&path).unwrap();
    symlink(outside.path().join("file"), &path).unwrap();
    assert!(matches!(
        open_resolved(&resolved, &[dir.path().into()]),
        Err(MediaError::Forbidden)
    ));
    fs::remove_file(&path).unwrap();
    fs::remove_dir(&parent).unwrap();
    symlink(outside.path(), &parent).unwrap();
    assert!(matches!(
        open_resolved(&resolved, &[dir.path().into()]),
        Err(MediaError::Forbidden)
    ));
}

#[tokio::test]
async fn throttled_offset_reader_and_hung_read_preserve_latency_and_deadline() {
    let read = timed_read_with(12, 10, |bytes, offset| {
        std::thread::sleep(Duration::from_millis(15));
        assert!(offset == 12 || offset == 13);
        Ok(bytes.len())
    });
    assert!(read.first_byte_ms.is_some_and(|ms| ms >= 15));
    assert!(read.elapsed_ms >= 30);
    assert!(read.bytes_per_second <= 333);
    assert_eq!(read.read_bytes, 10);
    let pool = WorkerPool(Arc::new(Semaphore::new(1)));
    let started = Instant::now();
    let read = pool
        .run(Duration::from_millis(15), || {
            timed_read_with(0, 1, |_, _| {
                std::thread::sleep(Duration::from_millis(200));
                Ok(1)
            })
        })
        .await
        .unwrap();
    assert!(read.is_none());
    assert!(started.elapsed() < Duration::from_millis(150));
}

#[tokio::test]
async fn request_and_process_concurrency_are_fail_fast() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("file");
    fs::write(&path, b"data").unwrap();
    let agent = MediaAgent::new(config(dir.path())).unwrap();
    let _request = agent.requests.acquire_many(2).await.unwrap();
    assert_eq!(
        agent.inspect(&path, false).await.unwrap_err(),
        MediaError::Busy
    );
    drop(_request);
    let _process = agent.processes.acquire_many(2).await.unwrap();
    assert_eq!(
        agent.inspect(&path, false).await.unwrap_err(),
        MediaError::Busy
    );
}

#[tokio::test]
async fn request_cancellation_kills_and_reaps_its_child() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("file");
    fs::write(&path, b"data").unwrap();
    let pidfile = dir.path().join("pid");
    let executable = script(
        dir.path(),
        "sleeper",
        &format!("echo $$ > '{}'\nexec sleep 30", pidfile.display()),
    );
    let mut c = config(dir.path());
    c.ffprobe = executable;
    let agent = MediaAgent::new(c).unwrap();
    let child_agent = agent.clone();
    let request = tokio::spawn(async move { child_agent.inspect(&path, false).await });
    // The shell creates the file before `echo` writes to it; wait for a whole pid,
    // or an empty read would make the check below look at `/proc/` itself.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let pid = loop {
        let pid = fs::read_to_string(&pidfile).unwrap_or_default();
        if pid.ends_with('\n') && pid.trim().parse::<u32>().is_ok() {
            break pid;
        }
        assert!(std::time::Instant::now() < deadline, "child never started");
        tokio::time::sleep(Duration::from_millis(2)).await;
    };
    request.abort();
    let _ = request.await;
    let proc = PathBuf::from(format!("/proc/{}", pid.trim()));
    // Reaping is prompt, but a loaded machine can take well over a second.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while (proc.exists() || agent.processes.available_permits() != 2)
        && std::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert!(!proc.exists());
    assert_eq!(agent.processes.available_permits(), 2);
}

#[tokio::test]
async fn filesystem_deadline_and_permission_denied_are_typed_evidence() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("file");
    fs::write(&path, b"data").unwrap();
    let mut c = config(dir.path());
    c.filesystem_timeout = Duration::from_millis(30);
    c.max_filesystem_workers = 1;
    let agent = MediaAgent::new(c).unwrap();
    let evidence = agent
        .inspect_with(&path, false, |path, roots, hops| {
            std::thread::sleep(Duration::from_millis(200));
            secure_open(&path, &roots, hops)
        })
        .await
        .unwrap();
    let links = evidence.symlinks.unwrap();
    assert_eq!(links.resolution, SymlinkResolution::TimedOut);
    assert_eq!(links.timeout_ms, 30);
    assert!(links.elapsed_ms >= 30);
    let mount = evidence.mount.unwrap();
    assert_eq!(mount.outcome, ProbeOutcome::TimedOut);
    assert_eq!(mount.timeout_ms, 30);
    assert!(evidence.reads.is_empty());
    assert!(evidence.header.is_none());
    let inaccessible = dir.path().join("closed");
    fs::create_dir(&inaccessible).unwrap();
    fs::write(inaccessible.join("file"), b"data").unwrap();
    fs::set_permissions(&inaccessible, fs::Permissions::from_mode(0o0)).unwrap();
    let evidence = MediaAgent::new(config(dir.path()))
        .unwrap()
        .inspect(&inaccessible.join("file"), false)
        .await
        .unwrap();
    fs::set_permissions(&inaccessible, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        evidence.symlinks.unwrap().resolution,
        SymlinkResolution::PermissionDenied
    );
    assert_eq!(
        evidence.mount.unwrap().outcome,
        ProbeOutcome::PermissionDenied
    );
    let evidence = MediaAgent::new(config(dir.path()))
        .unwrap()
        .inspect_with(&path, false, |path, roots, hops| {
            std::thread::sleep(Duration::from_millis(20));
            secure_open(&path, &roots, hops)
        })
        .await
        .unwrap();
    let links = evidence.symlinks.unwrap();
    assert!(links.elapsed_ms >= 20);
    assert_eq!(links.timeout_ms, 500);
}

#[tokio::test]
async fn logical_path_length_boundary_and_relative_paths() {
    let dir = TempDir::new().unwrap();
    let agent = MediaAgent::new(config(dir.path())).unwrap();
    assert_eq!(
        agent
            .inspect(Path::new("relative"), false)
            .await
            .unwrap_err(),
        MediaError::Forbidden
    );
    let prefix = format!("{}/", dir.path().display());
    let max = PathBuf::from(format!("{}{}", prefix, "x".repeat(4096 - prefix.len())));
    assert!(agent.inspect(&max, false).await.is_ok());
    let excessive = PathBuf::from(format!("{}x", max.display()));
    assert_eq!(
        agent.inspect(&excessive, false).await.unwrap_err(),
        MediaError::Forbidden
    );
}

#[tokio::test]
async fn bounded_output_and_decisive_process_errors() {
    assert_eq!(
        capture(&vec![4; OUTPUT_LIMIT][..]).await.unwrap().len(),
        OUTPUT_LIMIT
    );
    assert!(capture(&vec![4; OUTPUT_LIMIT + 1][..]).await.is_err());
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("file");
    fs::write(&path, b"data").unwrap();
    let file = Arc::new(File::open(path).unwrap());
    for (index, body) in [
        "echo out_time_us=0",
        "echo out_time_us=1000000; echo error >&2",
        "echo out_time_us=1000000; exit 1",
    ]
    .into_iter()
    .enumerate()
    {
        let mut c = config(dir.path());
        c.ffmpeg = script(dir.path(), &format!("decoder{index}"), body);
        let evidence = MediaAgent::new(c)
            .unwrap()
            .probe(Arc::clone(&file), Some((0, Duration::from_secs(4))))
            .await
            .unwrap();
        assert_eq!(evidence.outcome, ProbeOutcome::Failed);
        assert_eq!(evidence.faults, vec![MediaFault::DecodeError]);
        assert_eq!(
            evidence.tested_ranges,
            vec![DecodeRange {
                start_ms: 0,
                end_ms: Some(0)
            }]
        );
    }
}

#[test]
fn slow_thresholds_and_read_boundaries() {
    let mut read = timed_read_with(9, 1, |bytes, offset| {
        assert_eq!(offset, 9);
        assert_eq!(bytes.len(), 1);
        Ok(1)
    });
    read.first_byte_ms = Some(5);
    read.bytes_per_second = 100;
    assert!(!read_is_slow(&read, 6, 100));
    assert!(read_is_slow(&read, 5, 100));
    assert!(read_is_slow(&read, 6, 101));
    read.first_byte_ms = None;
    assert!(!read_is_slow(&read, 0, 100));
    let mut calls = Vec::new();
    let read = timed_read_with(9, 5, |bytes, at| {
        calls.push((bytes.len(), at));
        Ok(bytes.len())
    });
    assert_eq!(calls, vec![(1, 9), (4, 10)]);
    assert_eq!(read.read_bytes, 5);
    let mut p = probe();
    let bound = u64::MAX as f64 / 1000.0;
    parse_header(
        format!(r#"{{"format":{{"duration":"{bound}"}}}}"#).as_bytes(),
        &mut p,
    );
    assert_eq!(p.duration_ms, None);
    let mut p = probe();
    parse_header(br#"{"format":{"duration":"1e19"}}"#, &mut p);
    assert_eq!(p.duration_ms, None);
}

#[test]
fn symlinks_with_dot_and_parent_components_resolve_like_the_kernel() {
    let dir = TempDir::new().unwrap();
    let nested = dir.path().join("nested");
    fs::create_dir(&nested).unwrap();
    fs::write(dir.path().join("file"), b"data").unwrap();
    symlink("../file", nested.join("parent")).unwrap();
    assert_eq!(
        resolve(&nested.join("parent"), 40).1,
        SymlinkResolution::Resolved(dir.path().join("file"))
    );
    symlink(".", nested.join("here")).unwrap();
    assert_eq!(
        resolve(&nested.join("here"), 40).1,
        SymlinkResolution::Resolved(nested)
    );
}

#[tokio::test]
async fn first_decisive_stderr_exits_before_the_process_deadline() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("file");
    fs::write(&path, b"data").unwrap();
    let mut c = config(dir.path());
    c.ffprobe = script(
        dir.path(),
        "error_then_hang",
        "echo error >&2; exec sleep 30",
    );
    let start = Instant::now();
    let evidence = MediaAgent::new(c)
        .unwrap()
        .inspect(&path, true)
        .await
        .unwrap();
    assert!(start.elapsed() < Duration::from_secs(1));
    assert_eq!(evidence.header.unwrap().outcome, ProbeOutcome::Failed);
    assert!(evidence.decode.is_none());
    assert!(reject_stderr(&b""[..]).await.is_ok());
    assert!(reject_stderr(&b"x"[..]).await.is_err());
}

#[test]
fn symlink_cannot_traverse_parent_through_a_regular_file() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("file"), b"data").unwrap();
    symlink("file/../file", dir.path().join("invalid")).unwrap();
    assert!(matches!(
        resolve(&dir.path().join("invalid"), 40).1,
        SymlinkResolution::Dangling(_)
    ));
}

#[tokio::test]
async fn exact_file_reverification_rejects_modified_bytes_and_moved_descriptors() {
    let dir = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let path = dir.path().join("file");
    fs::write(&path, b"original").unwrap();
    let header = r#"{"format":{"duration":"1"},"streams":[{"codec_type":"audio","codec_name":"aac","channels":1}]}"#;
    let mut c = config(dir.path());
    c.ffprobe = script(
        dir.path(),
        "mutator",
        &format!(
            "echo modified-longer > '{}'\nprintf '%s' '{}'",
            path.display(),
            header
        ),
    );
    assert_eq!(
        MediaAgent::new(c)
            .unwrap()
            .inspect(&path, false)
            .await
            .unwrap_err(),
        MediaError::Changed
    );
    let mut c = config(dir.path());
    c.ffprobe = script(dir.path(), "header", &format!("printf '%s' '{header}'"));
    let destination = outside.path().join("moved");
    assert_eq!(
        MediaAgent::new(c)
            .unwrap()
            .inspect_with(&path, false, move |path, roots, hops| {
                let mut opened = secure_open(&path, &roots, hops)?;
                fs::rename(path, destination).unwrap();
                // Isolate the allow-list predicate from changes in stat metadata.
                opened.stamp = Some(Stamp::from(
                    &opened.file.as_ref().unwrap().metadata().unwrap(),
                ));
                Ok(opened)
            })
            .await
            .unwrap_err(),
        MediaError::Changed
    );
}

#[tokio::test]
async fn failed_read_stops_before_header_or_decode() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("file");
    let evidence = MediaAgent::new(config(dir.path()))
        .unwrap()
        .inspect_with(&path, true, |path, _, _| {
            let file = File::open(path.parent().unwrap()).unwrap();
            let stamp = Stamp::from(&file.metadata().unwrap());
            Ok(Opened {
                file: Some(Arc::new(file)),
                stamp: Some(stamp),
                symlinks: SymlinkEvidence {
                    chain: vec![],
                    resolution: SymlinkResolution::Resolved(path),
                    elapsed_ms: 0,
                    timeout_ms: 0,
                },
            })
        })
        .await
        .unwrap();
    assert_eq!(evidence.reads.len(), 1);
    assert_eq!(evidence.reads[0].outcome, ProbeOutcome::Failed);
    assert!(evidence.header.is_none());
    assert!(evidence.decode.is_none());
}

#[tokio::test]
async fn dangling_symlink_outside_storage_roots_is_forbidden() {
    let dir = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let link = dir.path().join("escape");
    symlink(outside.path().join("missing"), &link).unwrap();
    assert_eq!(
        MediaAgent::new(config(dir.path()))
            .unwrap()
            .inspect(&link, false)
            .await
            .unwrap_err(),
        MediaError::Forbidden
    );
}

#[tokio::test]
async fn shutdown_closes_admission_and_waits_for_cancelled_children_to_reap() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("file");
    fs::write(&path, b"data").unwrap();
    let pidfile = dir.path().join("pid");
    let executable = script(
        dir.path(),
        "sleeper",
        &format!("echo $$ > '{}'\nexec sleep 30", pidfile.display()),
    );
    let mut c = config(dir.path());
    c.ffprobe = executable;
    let agent = MediaAgent::new(c).unwrap();
    let request_agent = agent.clone();
    let request_path = path.clone();
    let request = tokio::spawn(async move { request_agent.inspect(&request_path, false).await });
    // The shell creates the file before writing the pid; wait for the content.
    for _ in 0..500 {
        if fs::read_to_string(&pidfile).is_ok_and(|pid| !pid.trim().is_empty()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let pid = fs::read_to_string(pidfile).unwrap();
    assert!(!pid.trim().is_empty());
    assert!(!agent.shutdown(Duration::from_millis(20)).await);
    request.abort();
    let _ = request.await;
    assert!(agent.shutdown(Duration::from_secs(1)).await);
    assert!(!PathBuf::from(format!("/proc/{}", pid.trim())).exists());
    assert_eq!(
        agent.inspect(&path, false).await.unwrap_err(),
        MediaError::Busy
    );
    assert!(agent.processes.is_closed());
}

#[tokio::test]
async fn shutdown_never_joins_abandoned_filesystem_workers() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("file");
    let mut c = config(dir.path());
    c.filesystem_timeout = Duration::from_millis(10);
    let agent = MediaAgent::new(c).unwrap();
    agent
        .inspect_with(&path, false, |_, _, _| {
            std::thread::sleep(Duration::from_millis(300));
            Err(MediaError::Forbidden)
        })
        .await
        .unwrap();
    assert!(agent.workers.0.available_permits() < agent.config.max_filesystem_workers);
    assert!(agent.shutdown(Duration::from_millis(20)).await);
}

#[tokio::test]
async fn cyclic_or_missing_media_does_not_mean_the_mount_is_dead() {
    let dir = TempDir::new().unwrap();
    symlink("loop", dir.path().join("loop")).unwrap();
    let agent = MediaAgent::new(config(dir.path())).unwrap();
    for path in [dir.path().join("loop"), dir.path().join("missing")] {
        let evidence = agent.inspect(&path, false).await.unwrap();
        assert_eq!(evidence.mount.unwrap().outcome, ProbeOutcome::Unavailable);
    }
}

#[tokio::test]
async fn configured_offsets_past_eof_do_not_label_good_small_media_slow() {
    let dir = TempDir::new().unwrap();
    let path = fixture(dir.path(), "short.mkv", &[]);
    let mut c = config(dir.path());
    c.read_offsets_bytes = vec![0, 1024 * 1024, 16 * 1024 * 1024];
    let evidence = MediaAgent::new(c)
        .unwrap()
        .inspect(&path, true)
        .await
        .unwrap();
    assert!(
        !evidence
            .header
            .unwrap()
            .faults
            .contains(&MediaFault::SlowRead)
    );
    assert_eq!(evidence.reads.len(), 1);
    assert_eq!(evidence.reads[0].offset_bytes, 0);
    assert_eq!(evidence.decode.unwrap().outcome, ProbeOutcome::Passed);
    let size = fs::metadata(&path).unwrap().len();
    let mut c = config(dir.path());
    c.read_offsets_bytes = vec![0, size, size + 1];
    let evidence = MediaAgent::new(c)
        .unwrap()
        .inspect(&path, false)
        .await
        .unwrap();
    assert_eq!(evidence.reads.len(), 1);
}

#[test]
fn realistic_feature_duration_is_not_mistaken_for_an_overflow() {
    let mut p = probe();
    parse_header(
        br#"{"format":{"duration":"7200.5"},"streams":[{"codec_type":"audio"}]}"#,
        &mut p,
    );
    assert_eq!(p.duration_ms, Some(7_200_500));
    assert!(!p.faults.contains(&MediaFault::NoDuration));
}

proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config::with_cases(48))]
    #[test]
    fn symlink_chain_and_traversal_fuzz_remain_confined(name in "[a-z]{1,12}", hops in 1usize..9, cycle in proptest::bool::ANY) {
        let dir = TempDir::new().unwrap(); let outside = TempDir::new().unwrap();
        let secret = outside.path().join("file"); fs::write(&secret, b"outside").unwrap();
        let links: Vec<_> = (0..hops).map(|index| dir.path().join(format!("{name}-{index}"))).collect();
        for (index, link) in links.iter().enumerate() {
            let target = links.get(index + 1).unwrap_or(if cycle { &links[0] } else { &secret });
            symlink(target, link).unwrap();
        }
        if cycle {
            proptest::prop_assert_eq!(resolve(&links[0], 40).1, SymlinkResolution::Cycle);
        } else {
            let forbidden = matches!(secure_open(&links[0], &[dir.path().into()], 40), Err(MediaError::Forbidden));
            proptest::prop_assert!(forbidden);
        }
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let agent = MediaAgent::new(config(dir.path())).unwrap();
        let traversal = dir.path().join(format!("{name}/../{name}"));
        proptest::prop_assert_eq!(runtime.block_on(agent.inspect(&traversal, false)).unwrap_err(), MediaError::Forbidden);
    }
}

#[tokio::test]
async fn replacing_logical_path_during_inspection_cannot_certify_the_old_inode() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("file");
    fs::write(&path, b"original").unwrap();
    let mut c = config(dir.path());
    c.ffprobe = script(
        dir.path(),
        "header",
        r#"printf '%s' '{"format":{"duration":"1"},"streams":[{"codec_type":"audio"}]}'"#,
    );
    let evidence = MediaAgent::new(c)
        .unwrap()
        .inspect_with(&path, false, |path, roots, hops| {
            let mut opened = secure_open(&path, &roots, hops)?;
            fs::rename(&path, path.with_extension("old")).unwrap();
            fs::write(&path, b"replacement").unwrap();
            opened.stamp = Some(Stamp::from(
                &opened.file.as_ref().unwrap().metadata().unwrap(),
            ));
            Ok(opened)
        })
        .await;
    assert_eq!(evidence.unwrap_err(), MediaError::Changed);
}

#[tokio::test]
async fn default_decode_detects_late_corruption_and_reports_attempted_ranges() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("file");
    fs::write(&path, b"data").unwrap();
    let mut c = config(dir.path());
    c.ffprobe = script(
        dir.path(),
        "header",
        r#"echo '{"format":{"duration":"100"},"streams":[{"codec_type":"video"},{"codec_type":"audio"}]}'"#,
    );
    c.ffmpeg = script(
        dir.path(),
        "decoder",
        r#"
        seek=0
        while [ "$#" -gt 0 ]; do
            if [ "$1" = '-ss' ]; then shift; seek="$1"; fi
            shift
        done
        if [ "$seek" = '80.000' ]; then echo corrupt >&2; exit 1; fi
        echo out_time_us=10000000
    "#,
    );
    let result = MediaAgent::new(c)
        .unwrap()
        .inspect(&path, true)
        .await
        .unwrap();
    let decode = result.decode.unwrap();
    assert_eq!(decode.outcome, ProbeOutcome::Failed);
    assert_eq!(decode.faults, vec![MediaFault::DecodeError]);
    assert_eq!(
        decode.tested_ranges,
        vec![
            DecodeRange {
                start_ms: 0,
                end_ms: Some(10_000)
            },
            DecodeRange {
                start_ms: 40_000,
                end_ms: Some(50_000)
            },
            DecodeRange {
                start_ms: 80_000,
                end_ms: Some(80_000)
            },
        ]
    );
}

#[tokio::test]
async fn configured_offsets_preserve_milliseconds_bounds_and_unknown_duration() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("file");
    fs::write(&path, b"data").unwrap();
    let file = Arc::new(File::open(&path).unwrap());
    let mut c = config(dir.path());
    c.decode_offsets_percent = vec![40];
    c.decode_seconds = 1;
    c.ffmpeg = script(
        dir.path(),
        "decoder",
        r#"
        while [ "$#" -gt 0 ]; do
            if [ "$1" = '-ss' ]; then shift; [ "$1" = '4.938' ] || exit 1; fi
            shift
        done
        echo out_time_us=2000000
    "#,
    );
    let d = MediaAgent::new(c.clone())
        .unwrap()
        .decode(Arc::clone(&file), Some(12_345))
        .await
        .unwrap();
    assert_eq!(d.outcome, ProbeOutcome::Passed);
    assert_eq!(d.duration_ms, Some(12_345));
    assert_eq!(d.timeout_ms, 4_000);
    assert_eq!(
        d.tested_ranges,
        vec![DecodeRange {
            start_ms: 4_938,
            end_ms: Some(5_938)
        }]
    );

    c.decode_offsets_percent = vec![0, 99];
    c.ffmpeg = script(dir.path(), "unknown-duration", "echo out_time_us=1000");
    let agent = MediaAgent::new(c).unwrap();
    let d = agent.decode(Arc::clone(&file), None).await.unwrap();
    assert_eq!(d.duration_ms, None);
    assert_eq!(
        d.tested_ranges,
        vec![DecodeRange {
            start_ms: 0,
            end_ms: Some(1)
        }]
    );
    let d = agent.decode(file, Some(u64::MAX)).await.unwrap();
    let last = (u128::from(u64::MAX) * 99 / 100) as u64;
    assert_eq!(
        d.tested_ranges,
        vec![
            DecodeRange {
                start_ms: 0,
                end_ms: Some(1)
            },
            DecodeRange {
                start_ms: last,
                end_ms: Some(last + 1)
            },
        ]
    );
}

#[tokio::test]
async fn decode_deadline_covers_all_samples_and_stops_early() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("file");
    fs::write(&path, b"data").unwrap();
    let file = Arc::new(File::open(&path).unwrap());
    let mut c = config(dir.path());
    c.process_timeout = Duration::from_millis(300);
    c.ffmpeg = script(dir.path(), "decoder", "sleep 0.2; echo out_time_us=1000000");
    let d = MediaAgent::new(c.clone())
        .unwrap()
        .decode(Arc::clone(&file), Some(100_000))
        .await
        .unwrap();
    assert_eq!(d.outcome, ProbeOutcome::TimedOut);
    assert_eq!(d.timeout_ms, 300);
    assert!(d.elapsed_ms >= 290 && d.elapsed_ms < 550, "{d:?}");
    assert_eq!(
        d.tested_ranges,
        vec![
            DecodeRange {
                start_ms: 0,
                end_ms: Some(1_000)
            },
            DecodeRange {
                start_ms: 40_000,
                end_ms: Some(40_000)
            },
        ]
    );
    c.process_timeout = Duration::from_nanos(1);
    let d = MediaAgent::new(c)
        .unwrap()
        .decode(file, Some(100_000))
        .await
        .unwrap();
    assert_eq!(d.outcome, ProbeOutcome::TimedOut);
    assert!(d.tested_ranges.is_empty());
}

#[tokio::test]
async fn first_failed_decode_never_launches_later_samples() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("file");
    fs::write(&path, b"data").unwrap();
    let file = Arc::new(File::open(&path).unwrap());
    let mut c = config(dir.path());
    c.ffmpeg = script(dir.path(), "decoder", "exit 1");
    let d = MediaAgent::new(c)
        .unwrap()
        .decode(file, Some(100_000))
        .await
        .unwrap();
    assert_eq!(d.outcome, ProbeOutcome::Failed);
    assert_eq!(d.faults, vec![MediaFault::DecodeError]);
    assert_eq!(
        d.tested_ranges,
        vec![DecodeRange {
            start_ms: 0,
            end_ms: Some(0)
        }]
    );
}

#[tokio::test]
async fn real_late_truncation_passes_start_but_fails_default_distributed_decode() {
    let dir = TempDir::new().unwrap();
    let path = fixture(dir.path(), "late.mkv", &["-t", "10", "-g", "10"]);
    let bytes = fs::read(&path).unwrap();
    fs::write(&path, &bytes[..bytes.len() * 7 / 10]).unwrap();
    let mut c = config(dir.path());
    c.decode_seconds = 1;
    c.decode_offsets_percent = vec![0];
    let shallow = MediaAgent::new(c.clone())
        .unwrap()
        .inspect(&path, true)
        .await
        .unwrap();
    assert_eq!(shallow.decode.unwrap().outcome, ProbeOutcome::Passed);
    c.decode_offsets_percent = vec![0, 40, 80];
    let deep = MediaAgent::new(c)
        .unwrap()
        .inspect(&path, true)
        .await
        .unwrap();
    let decode = deep.decode.unwrap();
    assert_eq!(decode.outcome, ProbeOutcome::Failed);
    assert_eq!(decode.tested_ranges.len(), 3);
    assert!(decode.tested_ranges[2].start_ms >= 8_000);
    assert_eq!(
        decode.tested_ranges[2].end_ms,
        Some(decode.tested_ranges[2].start_ms)
    );
}

// Shadow 2026-10-23: a real file with seven audio tracks produced ~75 KB of
// ffprobe JSON, the old 64 KiB cap turned that into InvalidHeader and the file
// was reported "bad". Large valid metadata must parse; output beyond the cap is
// a tool limit (Unavailable), never evidence against the file.
#[tokio::test]
async fn large_header_metadata_parses_and_oversized_output_is_not_a_media_fault() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("file");
    fs::write(&path, b"data").unwrap();
    let file = Arc::new(File::open(path).unwrap());
    let streams = (0..400)
        .map(|i| {
            format!(
                r#"{{"index":{i},"codec_type":"audio","codec_name":"eac3","channels":6,"tags":{{"language":"eng","title":"{}"}}}}"#,
                "x".repeat(120)
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let json = format!(
        r#"{{"streams":[{{"index":0,"codec_type":"video","codec_name":"hevc","width":1920,"height":1080}},{streams}],"format":{{"duration":"2095.808"}}}}"#
    );
    assert!(json.len() > 65_536);
    fs::write(dir.path().join("probe.json"), &json).unwrap();
    let mut c = config(dir.path());
    c.ffprobe = script(
        dir.path(),
        "big-probe",
        &format!("cat '{}'", dir.path().join("probe.json").display()),
    );
    let header = MediaAgent::new(c)
        .unwrap()
        .probe(Arc::clone(&file), None)
        .await
        .unwrap();
    assert_eq!(header.outcome, ProbeOutcome::Passed);
    assert!(header.faults.is_empty());
    assert_eq!(header.duration_ms, Some(2_095_808));

    let mut c = config(dir.path());
    c.ffprobe = script(dir.path(), "endless-probe", "yes x | head -c 3000000");
    let header = MediaAgent::new(c).unwrap().probe(file, None).await.unwrap();
    assert_eq!(header.outcome, ProbeOutcome::Unavailable);
    assert!(header.faults.is_empty());
}

// Shadow 2026-10-23: `-map 0:v?` also selected an embedded cover image, which
// made a healthy 10 s sample take ~175 s and fail. Decode real video only.
#[tokio::test]
async fn decode_maps_real_video_streams_only() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("file");
    fs::write(&path, b"data").unwrap();
    let file = Arc::new(File::open(path).unwrap());
    let args = dir.path().join("args");
    let mut c = config(dir.path());
    c.ffmpeg = script(
        dir.path(),
        "record-args",
        &format!(
            "echo \"$@\" > '{}'; echo out_time_us=1000000",
            args.display()
        ),
    );
    let d = MediaAgent::new(c)
        .unwrap()
        .probe(file, Some((0, Duration::from_secs(4))))
        .await
        .unwrap();
    assert_eq!(d.outcome, ProbeOutcome::Passed);
    let args = fs::read_to_string(args).unwrap();
    assert!(args.contains("-map 0:V?"), "{args}");
    assert!(!args.contains("0:v?"), "{args}");
}

#[tokio::test]
async fn real_file_with_cover_art_decodes_quickly() {
    let dir = TempDir::new().unwrap();
    let cover = dir.path().join("cover.jpg");
    assert!(
        std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-nostdin",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:s=1920x1080",
                "-frames:v",
                "1"
            ])
            .arg(&cover)
            .status()
            .unwrap()
            .success()
    );
    let base = fixture(dir.path(), "base.mkv", &[]);
    let path = dir.path().join("covered.mkv");
    assert!(
        std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-nostdin", "-i"])
            .arg(&base)
            .arg("-i")
            .arg(&cover)
            .args([
                "-map",
                "0",
                "-map",
                "1",
                "-c",
                "copy",
                "-disposition:v:1",
                "attached_pic"
            ])
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    let mut c = config(dir.path());
    c.decode_seconds = 1;
    let e = MediaAgent::new(c)
        .unwrap()
        .inspect(&path, true)
        .await
        .unwrap();
    let d = e.decode.unwrap();
    assert_eq!(d.outcome, ProbeOutcome::Passed, "{d:?}");
    assert!(d.faults.is_empty());
}

#[test]
fn dynamic_range_uses_positive_transfer_and_dolby_profile_evidence() {
    for (metadata, expected) in [
        (serde_json::json!({"color_transfer":"bt709"}), Some("SDR")),
        (
            serde_json::json!({"color_transfer":"smpte170m"}),
            Some("SDR"),
        ),
        (serde_json::json!({"color_transfer":"bt470m"}), Some("SDR")),
        (serde_json::json!({"color_transfer":"bt470bg"}), Some("SDR")),
        (
            serde_json::json!({"color_transfer":"bt709","side_data_list":[]}),
            Some("SDR"),
        ),
        (
            serde_json::json!({"color_transfer":"bt709","side_data_list":[{"side_data_type":"Unrelated metadata"}]}),
            Some("SDR"),
        ),
        (
            serde_json::json!({"color_transfer":"bt709","side_data_list":null}),
            None,
        ),
        (
            serde_json::json!({"color_transfer":"bt709","side_data_list":[{"dv_profile":5}]}),
            Some("DOVI_PROFILE_5"),
        ),
        (
            serde_json::json!({"color_transfer":"bt709","side_data_list":[{"dv_profile":"5"}]}),
            None,
        ),
        (
            serde_json::json!({"color_transfer":"smpte2084"}),
            Some("HDR10"),
        ),
        (
            serde_json::json!({"color_transfer":"arib-std-b67"}),
            Some("HLG"),
        ),
        (serde_json::json!({"color_transfer":"unknown"}), None),
        (
            serde_json::json!({"color_transfer":"smpte2084","side_data_list":[{"side_data_type":"Dolby Vision RPU Data"}]}),
            None,
        ),
        (
            serde_json::json!({"color_transfer":"smpte2084","side_data_list":[{"side_data_type":"DOVI metadata"}]}),
            None,
        ),
        (serde_json::json!({}), None),
        (
            serde_json::json!({"color_transfer":"smpte2084","side_data_list":[{"side_data_type":"DOVI configuration record","dv_profile":5}]}),
            Some("DOVI_PROFILE_5"),
        ),
        (
            serde_json::json!({"color_transfer":"smpte2084","side_data_list":[{"side_data_type":"DOVI configuration record","dv_profile":7}]}),
            Some("DOVI_PROFILE_7"),
        ),
        (
            serde_json::json!({"color_transfer":"smpte2084","side_data_list":[{"side_data_type":"DOVI configuration record","dv_profile":8}]}),
            Some("DOVI_PROFILE_8"),
        ),
        (
            serde_json::json!({"color_transfer":"bt709","side_data_list":[{"side_data_type":"DOVI configuration record"}]}),
            None,
        ),
        (
            serde_json::json!({"color_transfer":"bt709","side_data_list":[{"side_data_type":"DOVI configuration record","dv_profile":0}]}),
            None,
        ),
        (
            serde_json::json!({"color_transfer":"bt709","side_data_list":[{"side_data_type":"DOVI configuration record","dv_profile":5},{"side_data_type":"DOVI configuration record","dv_profile":8}]}),
            None,
        ),
    ] {
        let mut stream = metadata;
        stream["codec_type"] = serde_json::json!("video");
        let mut actual = probe();
        parse_header(
            &serde_json::to_vec(&serde_json::json!({"format":{"duration":"1"},"streams":[stream]}))
                .unwrap(),
            &mut actual,
        );
        let StreamFacts::Video { dynamic_range, .. } = &actual.streams[0] else {
            panic!("video")
        };
        assert_eq!(dynamic_range.as_deref(), expected);
    }
}
