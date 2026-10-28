//! Linux media checks over pinned descriptors. Filesystem workers are detached on
//! deadline and keep their permits until they return; a hung mount cannot create
//! an unbounded thread backlog or delay a Tokio runtime shutdown.

use pitcrew_domain::{FileIdentity, media::*};
use rustix::fs::{CWD, Mode, OFlags, ResolveFlags, openat2};
use serde_json::Value;
use std::{
    collections::{HashSet, VecDeque, hash_map::DefaultHasher},
    ffi::OsString,
    fs::{self, File, Metadata},
    hash::{Hash, Hasher},
    io,
    os::{
        fd::AsRawFd,
        unix::{
            fs::{FileExt, MetadataExt},
            process::CommandExt,
        },
    },
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::{Semaphore, oneshot},
};

#[derive(Clone, Debug)]
pub struct MediaConfig {
    /// Absolute directories for caller-visible paths. Logical aliases may be
    /// symlinks when separate physical `storage_roots` authorize their targets.
    pub allowed_roots: Vec<PathBuf>,
    /// Final symlink destinations. Empty means `allowed_roots`.
    pub storage_roots: Vec<PathBuf>,
    pub filesystem_timeout: Duration,
    pub process_timeout: Duration,
    pub header_timeout: Duration,
    pub read_timeout: Duration,
    pub read_offsets_bytes: Vec<u64>,
    pub slow_bytes_per_second: u64,
    pub symlink_hop_limit: usize,
    pub slow_read_threshold: Duration,
    pub read_bytes: usize,
    pub decode_seconds: u32,
    /// Ordered percentage offsets in 0..100; the total decode deadline covers all samples.
    pub decode_offsets_percent: Vec<u8>,
    pub max_requests: usize,
    pub max_filesystem_workers: usize,
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            allowed_roots: Vec::new(),
            storage_roots: Vec::new(),
            filesystem_timeout: Duration::from_secs(5),
            process_timeout: Duration::from_secs(180),
            header_timeout: Duration::from_secs(180),
            read_timeout: Duration::from_secs(5),
            read_offsets_bytes: Vec::new(),
            slow_bytes_per_second: 1024,
            symlink_hop_limit: 40,
            slow_read_threshold: Duration::from_secs(1),
            read_bytes: 64 * 1024,
            decode_seconds: 10,
            decode_offsets_percent: vec![0, 40, 80],
            max_requests: 2,
            max_filesystem_workers: 4,
            ffmpeg: PathBuf::from("ffmpeg"),
            ffprobe: PathBuf::from("ffprobe"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MediaError {
    #[error("invalid media configuration")]
    InvalidConfig,
    #[error("path is outside the configured roots")]
    Forbidden,
    #[error("diagnostic capacity is exhausted")]
    Busy,
    #[error("file changed during inspection")]
    Changed,
}

#[derive(Clone)]
pub struct MediaAgent {
    config: Arc<MediaConfig>,
    requests: Arc<Semaphore>,
    workers: WorkerPool,
    processes: Arc<Semaphore>,
}

impl MediaAgent {
    /// Validation performs no filesystem I/O, including on possibly hung roots.
    pub fn new(config: MediaConfig) -> Result<Self, MediaError> {
        if config.allowed_roots.is_empty()
            || config
                .allowed_roots
                .iter()
                .chain(&config.storage_roots)
                .any(|p| !valid_root(p))
            || config.filesystem_timeout.is_zero()
            || config.process_timeout.is_zero()
            || config.header_timeout.is_zero()
            || config.read_timeout.is_zero()
            || config.symlink_hop_limit == 0
            || config.symlink_hop_limit > 40
            || config.read_offsets_bytes.len() > 16
            || config.slow_read_threshold.is_zero()
            || config.read_bytes == 0
            || config.read_bytes > 1024 * 1024
            || config.decode_seconds == 0
            || config.decode_seconds > 300
            || config.decode_offsets_percent.is_empty()
            || config.decode_offsets_percent.len() > 16
            || config.decode_offsets_percent.iter().any(|p| *p >= 100)
            || config
                .decode_offsets_percent
                .windows(2)
                .any(|p| p[0] >= p[1])
            || config.max_requests == 0
            || config.max_requests > 64
            || config.max_filesystem_workers == 0
            || config.max_filesystem_workers > 64
            || config.ffmpeg.as_os_str().is_empty()
            || config.ffprobe.as_os_str().is_empty()
        {
            return Err(MediaError::InvalidConfig);
        }
        Ok(Self {
            requests: Arc::new(Semaphore::new(config.max_requests)),
            processes: Arc::new(Semaphore::new(config.max_requests)),
            workers: WorkerPool(Arc::new(Semaphore::new(config.max_filesystem_workers))),
            config: Arc::new(config),
        })
    }

    /// Stop admission, then wait for process supervisors to reap their children.
    /// Cancel request futures first to signal active supervisors. Detached
    /// filesystem workers are deliberately never joined, even at shutdown.
    pub async fn shutdown(&self, deadline: Duration) -> bool {
        self.requests.close();
        self.processes.close();
        tokio::time::timeout(deadline, async {
            while self.processes.available_permits() < self.config.max_requests {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .is_ok()
    }

    /// The decode outcome covers only `tested_ranges`; it is never whole-file
    /// certification. Unknown identities (failed opens) use the reserved zero ID.
    pub async fn inspect(&self, path: &Path, decode: bool) -> Result<MediaEvidence, MediaError> {
        self.inspect_with(path, decode, |path, roots, hops| {
            secure_open(&path, &roots, hops)
        })
        .await
    }

    async fn inspect_with(
        &self,
        path: &Path,
        decode: bool,
        open: impl FnOnce(PathBuf, Vec<PathBuf>, usize) -> Result<Opened, MediaError> + Send + 'static,
    ) -> Result<MediaEvidence, MediaError> {
        let _request = self
            .requests
            .clone()
            .try_acquire_owned()
            .map_err(|_| MediaError::Busy)?;
        if !path.is_absolute()
            || path.as_os_str().len() > 4096
            || path.components().any(|c| c == Component::ParentDir)
            || !within(path, &self.config.allowed_roots)
        {
            return Err(MediaError::Forbidden);
        }
        let mut evidence = MediaEvidence {
            file: FileIdentity { id: 0, revision: 0 },
            observed_at_unix_ms: millis(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default(),
            ),
            symlinks: None,
            mount: None,
            reads: Vec::new(),
            header: None,
            decode: None,
            plex_failures: Vec::new(),
        };
        let started = Instant::now();
        let owned_path = path.to_path_buf();
        let roots = self.storage_roots().to_vec();
        let hops = self.config.symlink_hop_limit;
        let opened = self
            .workers
            .run(self.config.filesystem_timeout, move || {
                open(owned_path, roots, hops)
            })
            .await?;
        let timeout_ms = millis(self.config.filesystem_timeout);
        let elapsed_ms = millis(started.elapsed());
        let opened = match opened {
            Some(result) => result?,
            None => Opened {
                file: None,
                stamp: None,
                symlinks: SymlinkEvidence {
                    chain: Vec::new(),
                    resolution: SymlinkResolution::TimedOut,
                    elapsed_ms,
                    timeout_ms,
                },
            },
        };
        let outcome = match opened.symlinks.resolution {
            SymlinkResolution::Resolved(_) => ProbeOutcome::Passed,
            SymlinkResolution::TimedOut => ProbeOutcome::TimedOut,
            SymlinkResolution::PermissionDenied => ProbeOutcome::PermissionDenied,
            // Missing media, cycles and exhausted hop budgets do not establish
            // that storage is dead. Only the successful final open proves that
            // this request reached responsive storage.
            _ => ProbeOutcome::Unavailable,
        };
        evidence.mount = Some(MountEvidence {
            mount: "configured-storage".into(),
            outcome,
            elapsed_ms,
            timeout_ms,
        });
        evidence.symlinks = Some(SymlinkEvidence {
            elapsed_ms,
            timeout_ms,
            ..opened.symlinks
        });
        let (Some(file), Some(stamp)) = (opened.file, opened.stamp) else {
            return Ok(evidence);
        };
        evidence.file = stamp.identity();
        let size = stamp.len;
        let mut offsets = if self.config.read_offsets_bytes.is_empty() {
            vec![
                0,
                size / 2,
                size.saturating_sub(self.config.read_bytes as u64),
            ]
        } else {
            self.config.read_offsets_bytes.clone()
        };
        offsets.sort_unstable();
        offsets.dedup();
        // Configured samples beyond this exact file's EOF carry no throughput
        // information. A short healthy file must not become a slow-read fault.
        offsets.retain(|offset| *offset < size);
        let timeout_ms = millis(self.config.read_timeout);
        for offset in offsets {
            let file = Arc::clone(&file);
            let count = self.config.read_bytes;
            let start = Instant::now();
            let read = self
                .workers
                .run(self.config.read_timeout, move || {
                    timed_read(&file, offset, count)
                })
                .await?;
            let elapsed = start.elapsed();
            let mut read = read.unwrap_or(TimedRead {
                offset_bytes: offset,
                requested_bytes: count as u64,
                read_bytes: 0,
                first_byte_ms: None,
                elapsed_ms: millis(elapsed),
                timeout_ms,
                bytes_per_second: 0,
                outcome: ProbeOutcome::TimedOut,
            });
            read.timeout_ms = timeout_ms;
            let passed = read.outcome == ProbeOutcome::Passed;
            evidence.reads.push(read);
            if !passed {
                return Ok(evidence);
            }
        }
        let mut header = self.probe(Arc::clone(&file), None).await?;
        if evidence.reads.iter().any(|r| {
            read_is_slow(
                r,
                millis(self.config.slow_read_threshold),
                self.config.slow_bytes_per_second,
            )
        }) {
            header.faults.push(MediaFault::SlowRead);
        }
        let header_ok = header.outcome == ProbeOutcome::Passed;
        let duration_ms = header.duration_ms;
        evidence.header = Some(header);
        if decode && header_ok {
            evidence.decode = Some(self.decode(Arc::clone(&file), duration_ms).await?);
        }
        let final_file = Arc::clone(&file);
        let roots = self.storage_roots().to_vec();
        let logical_path = path.to_path_buf();
        let unchanged = self
            .workers
            .run(self.config.filesystem_timeout, move || {
                verify_descriptor(&final_file, &roots).is_ok()
                    && final_file
                        .metadata()
                        .is_ok_and(|m| Stamp::from(&m) == stamp)
                    && secure_open(&logical_path, &roots, hops)
                        .is_ok_and(|current| current.stamp == Some(stamp))
            })
            .await?;
        if unchanged != Some(true) {
            return Err(MediaError::Changed);
        }
        Ok(evidence)
    }

    fn storage_roots(&self) -> &[PathBuf] {
        if self.config.storage_roots.is_empty() {
            &self.config.allowed_roots
        } else {
            &self.config.storage_roots
        }
    }

    async fn decode(
        &self,
        file: Arc<File>,
        duration_ms: Option<u64>,
    ) -> Result<MediaProbe, MediaError> {
        let started = Instant::now();
        let mut combined = MediaProbe {
            outcome: ProbeOutcome::Passed,
            duration_ms,
            elapsed_ms: 0,
            timeout_ms: millis(self.config.process_timeout),
            faults: Vec::new(),
            streams: Vec::new(),
            tested_ranges: Vec::new(),
        };
        let offsets = match duration_ms {
            Some(duration) => self
                .config
                .decode_offsets_percent
                .iter()
                .map(|percent| (u128::from(duration) * u128::from(*percent) / 100) as u64)
                .collect::<Vec<_>>(),
            None => vec![0],
        };
        for offset in offsets {
            let remaining = self
                .config
                .process_timeout
                .saturating_sub(started.elapsed());
            if remaining.is_zero() {
                combined.outcome = ProbeOutcome::TimedOut;
                break;
            }
            let sample = self
                .probe(Arc::clone(&file), Some((offset, remaining)))
                .await?;
            combined.outcome = sample.outcome;
            combined.faults.extend(sample.faults);
            combined.tested_ranges.extend(sample.tested_ranges);
            if combined.outcome != ProbeOutcome::Passed {
                break;
            }
        }
        combined.elapsed_ms = millis(started.elapsed());
        Ok(combined)
    }

    async fn probe(
        &self,
        file: Arc<File>,
        sample: Option<(u64, Duration)>,
    ) -> Result<MediaProbe, MediaError> {
        let decode = sample.is_some();
        let mut command = Command::new(if decode {
            &self.config.ffmpeg
        } else {
            &self.config.ffprobe
        });
        command.args(["-v", "error", "-protocol_whitelist", "file", "-format_whitelist", "matroska,webm,mov,mp4,m4a,3gp,3g2,mj2,avi,mpegts,mpegvideo,mp3,flac,wav,aac,ogg,h264,hevc"]);
        let input = format!("/proc/{}/fd/{}", std::process::id(), file.as_raw_fd());
        if decode {
            command.args([
                "-nostdin",
                // Stop at real decoder errors only. `-err_detect explode` also
                // aborted on harmless noise right after a seek (missing
                // pre-seek reference frames, truncated SEI) in healthy remuxes.
                "-xerror",
                "-threads",
                "1",
                "-ss",
                &format!(
                    "{}.{:03}",
                    sample.unwrap().0 / 1000,
                    sample.unwrap().0 % 1000
                ),
                "-i",
                &input,
                "-t",
                &self.config.decode_seconds.to_string(),
                "-progress",
                "pipe:1",
                "-nostats",
                "-map",
                // Capital V excludes attached pictures (cover art), which made
                // healthy samples crawl and fail. Only the first such stream: a
                // still stored as an ordinary video track ends at once, and
                // ffmpeg 8 then reports the sample's decoded time as N/A.
                "0:V:0?",
                "-map",
                "0:a?",
                "-threads",
                "1",
                "-f",
                "null",
                "-",
            ]);
        } else {
            command.args([
                "-show_format",
                "-show_streams",
                "-print_format",
                "json",
                &input,
            ]);
        }
        let started = Instant::now();
        let deadline = sample.map_or(self.config.header_timeout, |(_, timeout)| timeout);
        let output = run_process(command, file, deadline, Arc::clone(&self.processes)).await?;
        let mut probe = MediaProbe {
            outcome: output.outcome,
            duration_ms: None,
            elapsed_ms: millis(started.elapsed()),
            timeout_ms: millis(deadline),
            faults: Vec::new(),
            streams: Vec::new(),
            tested_ranges: Vec::new(),
        };
        if let Some((start, _)) = sample {
            // A zero-length range means the sample was attempted but no decoded
            // progress was confirmed (including timeout or tool failure).
            probe.tested_ranges.push(DecodeRange {
                start_ms: start,
                end_ms: Some(start),
            });
        }
        if probe.outcome != ProbeOutcome::Passed {
            if probe.outcome == ProbeOutcome::Failed {
                probe.faults.push(if decode {
                    MediaFault::DecodeError
                } else {
                    MediaFault::InvalidHeader
                });
            }
            return Ok(probe);
        }
        if decode {
            let end_ms = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.strip_prefix("out_time_us=")?.parse::<u64>().ok())
                .max()
                .map(|us| us / 1000);
            if let Some(end) = end_ms.filter(|end| *end > 0) {
                let range = &mut probe.tested_ranges[0];
                range.end_ms = Some(
                    range
                        .start_ms
                        .saturating_add(end.min(u64::from(self.config.decode_seconds) * 1000)),
                );
            } else {
                probe.outcome = ProbeOutcome::Failed;
                probe.faults.push(MediaFault::DecodeError);
            }
        } else {
            parse_header(&output.stdout, &mut probe);
        }
        Ok(probe)
    }
}

fn read_is_slow(read: &TimedRead, first_byte_limit: u64, bytes_per_second_limit: u64) -> bool {
    read.first_byte_ms.is_some_and(|ms| ms >= first_byte_limit)
        || read.bytes_per_second < bytes_per_second_limit
}

fn valid_root(path: &Path) -> bool {
    path.is_absolute()
        && path != Path::new("/")
        && !path.components().any(|c| c == Component::ParentDir)
}

fn within(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}
fn millis(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

#[derive(Clone)]
struct WorkerPool(Arc<Semaphore>);

impl WorkerPool {
    async fn run<T: Send + 'static>(
        &self,
        deadline: Duration,
        operation: impl FnOnce() -> T + Send + 'static,
    ) -> Result<Option<T>, MediaError> {
        let permit = self
            .0
            .clone()
            .try_acquire_owned()
            .map_err(|_| MediaError::Busy)?;
        let (tx, rx) = oneshot::channel();
        std::thread::Builder::new()
            .name("media-filesystem".into())
            .spawn(move || {
                let result = operation();
                drop(permit);
                let _ = tx.send(result);
            })
            .map_err(|_| MediaError::Busy)?;
        Ok(tokio::time::timeout(deadline, rx)
            .await
            .ok()
            .and_then(Result::ok))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Stamp {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: i64,
    mtime_ns: i64,
    ctime: i64,
    ctime_ns: i64,
}

impl From<&Metadata> for Stamp {
    fn from(m: &Metadata) -> Self {
        Self {
            dev: m.dev(),
            ino: m.ino(),
            len: m.len(),
            mtime: m.mtime(),
            mtime_ns: m.mtime_nsec(),
            ctime: m.ctime(),
            ctime_ns: m.ctime_nsec(),
        }
    }
}

impl Stamp {
    fn identity(self) -> FileIdentity {
        let mut id = DefaultHasher::new();
        (self.dev, self.ino).hash(&mut id);
        let mut revision = DefaultHasher::new();
        self.hash(&mut revision);
        FileIdentity {
            id: id.finish(),
            revision: revision.finish(),
        }
    }
}

struct Opened {
    file: Option<Arc<File>>,
    stamp: Option<Stamp>,
    symlinks: SymlinkEvidence,
}

fn secure_open(path: &Path, roots: &[PathBuf], hops: usize) -> Result<Opened, MediaError> {
    let (chain, resolution) = resolve(path, hops);
    if let SymlinkResolution::Dangling(target) = &resolution
        && !within(target, roots)
    {
        return Err(MediaError::Forbidden);
    }
    let mut opened = Opened {
        file: None,
        stamp: None,
        symlinks: SymlinkEvidence {
            chain,
            resolution,
            elapsed_ms: 0,
            timeout_ms: 0,
        },
    };
    let SymlinkResolution::Resolved(final_path) = &opened.symlinks.resolution else {
        return Ok(opened);
    };
    let (file, stamp) = open_resolved(final_path, roots)?;
    opened.stamp = Some(stamp);
    opened.file = Some(file);
    Ok(opened)
}

fn open_resolved(final_path: &Path, roots: &[PathBuf]) -> Result<(Arc<File>, Stamp), MediaError> {
    let root = roots
        .iter()
        .find(|root| final_path.starts_with(root))
        .ok_or(MediaError::Forbidden)?;
    // Pin the physical root and reject all subsequent symlinks. A swap after
    // resolution cannot redirect the open outside this root (including parents).
    let root_fd = openat2(
        CWD,
        root,
        OFlags::PATH.union(OFlags::DIRECTORY).union(OFlags::CLOEXEC),
        Mode::empty(),
        ResolveFlags::NO_SYMLINKS.union(ResolveFlags::NO_MAGICLINKS),
    )
    .map_err(|_| MediaError::Forbidden)?;
    let relative = final_path
        .strip_prefix(root)
        .map_err(|_| MediaError::Forbidden)?;
    let fd = openat2(
        root_fd,
        relative,
        OFlags::RDONLY
            .union(OFlags::NONBLOCK)
            .union(OFlags::CLOEXEC),
        Mode::empty(),
        ResolveFlags::BENEATH
            .union(ResolveFlags::NO_SYMLINKS)
            .union(ResolveFlags::NO_MAGICLINKS),
    )
    .map_err(|_| MediaError::Forbidden)?;
    let file = File::from(fd);
    verify_descriptor(&file, roots)?;
    let metadata = file.metadata().map_err(|_| MediaError::Forbidden)?;
    if !metadata.is_file() {
        return Err(MediaError::Forbidden);
    }
    Ok((Arc::new(file), Stamp::from(&metadata)))
}

fn verify_descriptor(file: &File, roots: &[PathBuf]) -> Result<(), MediaError> {
    let actual = fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
        .map_err(|_| MediaError::Forbidden)?;
    if within(&actual, roots) {
        Ok(())
    } else {
        Err(MediaError::Forbidden)
    }
}

fn resolve(path: &Path, hops: usize) -> (Vec<SymlinkHop>, SymlinkResolution) {
    let mut pending = components(path);
    let mut current = PathBuf::new();
    let mut chain = Vec::new();
    let mut visited = HashSet::new();
    while let Some(part) = pending.pop_front() {
        if part == "/" {
            current = PathBuf::from("/");
            continue;
        }
        if part == "." {
            continue;
        }
        if part == ".." {
            current.pop();
            continue;
        }
        current.push(part);
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) => {
                return (
                    chain,
                    if error.kind() == io::ErrorKind::PermissionDenied {
                        SymlinkResolution::PermissionDenied
                    } else {
                        SymlinkResolution::Dangling(current)
                    },
                );
            }
        };
        if !metadata.file_type().is_symlink() && !metadata.is_dir() && !pending.is_empty() {
            return (chain, SymlinkResolution::Dangling(current));
        }
        if metadata.file_type().is_symlink() {
            if !visited.insert((current.clone(), pending.clone())) {
                return (chain, SymlinkResolution::Cycle);
            }
            if chain.len() >= hops {
                return (chain, SymlinkResolution::HopLimit);
            }
            let target = match fs::read_link(&current) {
                Ok(target) => target,
                Err(_) => return (chain, SymlinkResolution::Dangling(current)),
            };
            chain.push(SymlinkHop {
                link: current.clone(),
                target: target.clone(),
            });
            current.pop();
            let mut next = components(&target);
            next.append(&mut pending);
            pending = next;
        }
    }
    (chain, SymlinkResolution::Resolved(current))
}

fn components(path: &Path) -> VecDeque<OsString> {
    path.components()
        .map(|c| c.as_os_str().to_os_string())
        .collect()
}

fn timed_read(file: &File, offset: u64, count: usize) -> TimedRead {
    timed_read_with(offset, count, |bytes, at| file.read_at(bytes, at))
}

fn timed_read_with(
    offset: u64,
    count: usize,
    mut read_at: impl FnMut(&mut [u8], u64) -> io::Result<usize>,
) -> TimedRead {
    let start = Instant::now();
    let mut data = vec![0; count];
    let first = read_at(&mut data[..1], offset);
    let first_byte_ms = first
        .as_ref()
        .ok()
        .filter(|n| **n > 0)
        .map(|_| millis(start.elapsed()));
    let result = first.and_then(|n| {
        if n == 1 && count > 1 {
            read_at(&mut data[1..], offset + 1).map(|rest| rest + 1)
        } else {
            Ok(n)
        }
    });
    let elapsed = start.elapsed();
    let (read_bytes, outcome) = match result {
        Ok(n) => (n as u64, ProbeOutcome::Passed),
        Err(_) => (0, ProbeOutcome::Failed),
    };
    TimedRead {
        offset_bytes: offset,
        requested_bytes: count as u64,
        read_bytes,
        first_byte_ms,
        elapsed_ms: millis(elapsed),
        timeout_ms: 0,
        bytes_per_second: ((u128::from(read_bytes) * 1_000_000_000) / elapsed.as_nanos().max(1))
            .try_into()
            .unwrap_or(u64::MAX),
        outcome,
    }
}

struct ProcessOutput {
    outcome: ProbeOutcome,
    stdout: Vec<u8>,
}

// Large enough for files with many audio/subtitle tracks (a real seven-audio
// release produced ~75 KB of ffprobe JSON). Exceeding it is a tool limit,
// reported as Unavailable rather than as a fault in the media.
const OUTPUT_LIMIT: usize = 1_048_576;

async fn capture(reader: impl AsyncRead + Unpin) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(OUTPUT_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > OUTPUT_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            "process output exceeded limit",
        ));
    }
    Ok(bytes)
}

// Both tools run at loglevel=error: the first stderr byte is decisive.
// Never retain tool diagnostics, which can contain local paths and metadata.
/// Diagnostics are discarded, never a verdict: healthy files routinely log
/// warnings (cover-art quirks, muxer timestamps after a seek) and exit 0, while
/// real decode errors exit nonzero under `-xerror`. Draining keeps a chatty tool
/// from blocking on a full pipe; the deadline still bounds it.
async fn drain_stderr(mut reader: impl AsyncRead + Unpin) -> io::Result<()> {
    let mut buffer = [0; 8192];
    while reader.read(&mut buffer).await? != 0 {}
    Ok(())
}

async fn run_process(
    mut command: Command,
    file: Arc<File>,
    deadline: Duration,
    permits: Arc<Semaphore>,
) -> Result<ProcessOutput, MediaError> {
    let permit = permits.try_acquire_owned().map_err(|_| MediaError::Busy)?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command.as_std_mut().process_group(0);
    let (mut tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let _file = file;
        let Ok(mut child) = command.spawn() else {
            let _ = tx.send(ProcessOutput {
                outcome: ProbeOutcome::Unavailable,
                stdout: Vec::new(),
            });
            return;
        };
        let pid = child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .and_then(rustix::process::Pid::from_raw);
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let result = tokio::select! {
            result = tokio::time::timeout(deadline, async {
                let (stdout, ()) = tokio::try_join!(capture(stdout), drain_stderr(stderr))?;
                let status = child.wait().await?;
                Ok::<_, io::Error>((status, stdout))
            }) => result,
            _ = tx.closed() => {
                if let Some(pid) = pid { let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL); }
                let _ = child.kill().await;
                let _ = child.wait().await;
                return;
            }
        };
        let output = match result {
            Ok(Ok((status, stdout))) => {
                let output = if status.success() {
                    ProcessOutput {
                        outcome: ProbeOutcome::Passed,
                        stdout,
                    }
                } else {
                    ProcessOutput {
                        outcome: ProbeOutcome::Failed,
                        stdout: Vec::new(),
                    }
                };
                // wait() already reaped this child. Release capacity before the
                // caller can start its next sequential probe.
                drop(permit);
                let _ = tx.send(output);
                return;
            }
            Ok(Err(error)) => ProcessOutput {
                outcome: if error.kind() == io::ErrorKind::FileTooLarge {
                    ProbeOutcome::Unavailable
                } else {
                    ProbeOutcome::Failed
                },
                stdout: Vec::new(),
            },
            Err(_) => ProcessOutput {
                outcome: ProbeOutcome::TimedOut,
                stdout: Vec::new(),
            },
        };
        // Return a bounded result even if an uninterruptible child cannot yet
        // be reaped. Its supervisor retains capacity until wait completes.
        if let Some(pid) = pid {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
        let _ = child.start_kill();
        let _ = tx.send(output);
        let _ = child.wait().await;
        drop(permit);
    });
    Ok(rx.await.unwrap_or(ProcessOutput {
        outcome: ProbeOutcome::Unavailable,
        stdout: Vec::new(),
    }))
}

fn parse_header(bytes: &[u8], probe: &mut MediaProbe) {
    let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
        probe.outcome = ProbeOutcome::Failed;
        probe.faults.push(MediaFault::InvalidHeader);
        return;
    };
    probe.duration_ms = value["format"]["duration"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|s| s.is_finite() && *s > 0.0 && *s < u64::MAX as f64 / 1000.0)
        .map(|s| (s * 1000.0) as u64);
    if probe.duration_ms.is_none() {
        probe.faults.push(MediaFault::NoDuration);
    }
    if let Some(streams) = value["streams"].as_array() {
        for stream in streams {
            let codec = stream["codec_name"]
                .as_str()
                .unwrap_or("unknown")
                .to_owned();
            let language = stream["tags"]["language"].as_str().map(str::to_owned);
            let fact = match stream["codec_type"].as_str() {
                Some("video") => StreamFacts::Video {
                    codec,
                    width: stream["width"]
                        .as_u64()
                        .and_then(|n| n.try_into().ok())
                        .unwrap_or(0),
                    height: stream["height"]
                        .as_u64()
                        .and_then(|n| n.try_into().ok())
                        .unwrap_or(0),
                    bit_depth: None,
                    dynamic_range: dynamic_range(stream).map(str::to_owned),
                },
                Some("audio") => StreamFacts::Audio {
                    codec,
                    channels: stream["channels"]
                        .as_u64()
                        .and_then(|n| n.try_into().ok())
                        .unwrap_or(0),
                    language,
                },
                Some("subtitle") => StreamFacts::Subtitle { codec, language },
                _ => continue,
            };
            probe.streams.push(fact);
        }
    }
    if !probe
        .streams
        .iter()
        .any(|s| matches!(s, StreamFacts::Video { .. }))
    {
        probe.faults.push(MediaFault::NoVideo);
    }
    if !probe
        .streams
        .iter()
        .any(|s| matches!(s, StreamFacts::Audio { .. }))
    {
        probe.faults.push(MediaFault::NoAudio);
    }
    if probe.streams.is_empty() {
        probe.outcome = ProbeOutcome::Failed;
        probe.faults.push(MediaFault::InvalidHeader);
    }
}

/// Only known ffprobe transfer functions and a single explicit Dolby Vision
/// configuration record are evidence. Dolby metadata always takes precedence
/// over the base layer's transfer; profile 5 may otherwise look like HDR10.
fn dynamic_range(stream: &Value) -> Option<&'static str> {
    if let Some(side_data) = stream.get("side_data_list") {
        let entries = side_data.as_array()?;
        let mut profiles = entries.iter().filter(|entry| {
            let kind = entry["side_data_type"]
                .as_str()
                .unwrap_or_default()
                .to_ascii_lowercase();
            entry.get("dv_profile").is_some() || kind.contains("dovi") || kind.contains("dolby")
        });
        if let Some(record) = profiles.next() {
            if profiles.next().is_some() {
                return None;
            }
            return match record["dv_profile"].as_u64()? {
                5 => Some("DOVI_PROFILE_5"),
                7 => Some("DOVI_PROFILE_7"),
                8 => Some("DOVI_PROFILE_8"),
                _ => None,
            };
        }
    }
    match stream["color_transfer"].as_str()? {
        "bt709" | "smpte170m" | "bt470m" | "bt470bg" => Some("SDR"),
        "smpte2084" => Some("HDR10"),
        "arib-std-b67" => Some("HLG"),
        _ => None,
    }
}

#[cfg(test)]
#[path = "media_tests.rs"]
mod tests;
