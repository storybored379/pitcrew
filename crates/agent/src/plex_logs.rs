//! Bounded local Plex log snapshots. User/session/path fields are private local
//! evidence and must pass through redaction before reaching a cloud decider.

use chrono::{LocalResult, NaiveDateTime, TimeZone};
use chrono_tz::Tz;
use pitcrew_domain::media::{PlexFailureKind, PlexLogFailure};
use regex::Regex;
use serde::Serialize;
use std::{
    collections::{BTreeMap, HashSet},
    fs::File,
    io::{self, BufReader, Read, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    path::PathBuf,
    sync::LazyLock,
};

#[derive(Clone, Debug)]
pub struct PlexLogConfig {
    pub directory: PathBuf,
    pub timezone: String,
    pub max_file_bytes: u64,
    pub max_line_bytes: usize,
    pub max_entries: usize,
    pub rotations: u8,
}

impl Default for PlexLogConfig {
    fn default() -> Self {
        Self {
            directory: PathBuf::new(),
            timezone: "UTC".into(),
            max_file_bytes: 4 * 1024 * 1024,
            max_line_bytes: 16 * 1024,
            max_entries: 4096,
            rotations: 5,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CorrelatedFailure {
    pub session: String,
    pub user: Option<String>,
    pub rating_key: Option<String>,
    pub path: Option<PathBuf>,
    pub errors: Vec<String>,
    /// A repeated DST wall-clock time has two candidates, never one guessed UTC.
    pub earliest_unix_ms: u64,
    pub latest_unix_ms: u64,
    pub timezone: String,
    pub kind: PlexFailureKind,
}

impl CorrelatedFailure {
    /// Precise domain evidence is available only for unambiguous wall-clock time.
    /// A log path cannot establish an exact imported FileIdentity.
    pub fn domain_evidence(&self) -> Option<PlexLogFailure> {
        (self.earliest_unix_ms == self.latest_unix_ms).then(|| PlexLogFailure {
            at_unix_ms: self.latest_unix_ms,
            timezone: self.timezone.clone(),
            kind: self.kind.clone(),
            file: None,
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PlexLogSnapshot {
    pub failures: Vec<CorrelatedFailure>,
    /// A byte, line, correlation, or per-session error bound dropped evidence.
    pub truncated: bool,
    /// Invalid dates and nonexistent spring-forward local times are not guessed.
    pub rejected_timestamps: u64,
    pub files_read: usize,
}

pub struct PlexLogReader {
    config: PlexLogConfig,
    timezone: Tz,
}

impl PlexLogReader {
    pub fn new(config: PlexLogConfig) -> io::Result<Self> {
        let timezone = config.timezone.parse().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "invalid IANA log timezone")
        })?;
        if config.directory.as_os_str().is_empty()
            || !(1..=64 * 1024 * 1024).contains(&config.max_file_bytes)
            || !(1..=64 * 1024).contains(&config.max_line_bytes)
            || !(1..=65_536).contains(&config.max_entries)
            || config.rotations > 16
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid log bounds",
            ));
        }
        Ok(Self { config, timezone })
    }

    /// Open the current log first, then pin all fixed rotation names before reading.
    /// Read oldest first. Open descriptors
    /// survive rename rotation; inode deduplication prevents duplicate evidence.
    /// Call through a bounded blocking worker: a local filesystem can also hang.
    pub fn read_since(&self, cutoff_unix_ms: u64) -> io::Result<PlexLogSnapshot> {
        let mut files = Vec::new();
        let mut seen = HashSet::new();
        for rotation in 0..=self.config.rotations {
            let name = if rotation == 0 {
                "Plex Media Server.log".to_owned()
            } else {
                format!("Plex Media Server.{rotation}.log")
            };
            let path = self.config.directory.join(name);
            let file = match open_log(&path) {
                Ok(file) => file,
                Err(rustix::io::Errno::NOENT) => continue,
                Err(error) => return Err(error.into()),
            };
            let metadata = file.metadata()?;
            if !metadata.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "log must be a regular file",
                ));
            }
            if seen.insert((metadata.dev(), metadata.ino())) {
                files.push((file, metadata.len()));
            }
        }
        let mut parser = Parser::new(&self.config, self.timezone);
        for (mut file, length) in files.into_iter().rev() {
            let start = length.saturating_sub(self.config.max_file_bytes);
            file.seek(SeekFrom::Start(start))?;
            parser.snapshot.truncated |= start != 0;
            let mut skip_line = start != 0;
            let mut line = Vec::new();
            // BufReader amortizes byte iteration; take bounds growth during append.
            for byte in BufReader::new(file.take(self.config.max_file_bytes)).bytes() {
                let byte = byte?;
                if byte == b'\n' {
                    if !skip_line {
                        parser.ingest(&String::from_utf8_lossy(&line));
                    }
                    line.clear();
                    skip_line = false;
                } else if !skip_line {
                    if line.len() == self.config.max_line_bytes {
                        line.clear();
                        skip_line = true;
                        parser.snapshot.truncated = true;
                    } else {
                        line.push(byte);
                    }
                }
            }
            // Incomplete trailing lines may be mid-write. Re-read next snapshot.
            parser.snapshot.truncated |= !line.is_empty() || skip_line;
            parser.snapshot.files_read += 1;
        }
        Ok(parser.finish(cutoff_unix_ms))
    }
}

fn open_log(path: &std::path::Path) -> Result<File, rustix::io::Errno> {
    // These disjoint bitflags use the named union operation to make the intent
    // explicit (bitwise XOR would coincidentally produce the same flags).
    let flags = rustix::fs::OFlags::RDONLY
        .union(rustix::fs::OFlags::NOFOLLOW)
        .union(rustix::fs::OFlags::NONBLOCK)
        .union(rustix::fs::OFlags::CLOEXEC);
    rustix::fs::open(path, flags, rustix::fs::Mode::empty()).map(File::from)
}

static TIMESTAMP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\w{3} +\d{1,2}, \d{4} \d{2}:\d{2}:\d{2}(?:\.\d{1,9})?)").unwrap()
});
static REQUEST: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[Req#([A-Za-z0-9]+)").unwrap());
static SESSION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[?&]session=([^&\s]+)").unwrap());
static RATING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:path=)?(?:%2f|/)library(?:%2f|/)metadata(?:%2f|/)(\d+)").unwrap()
});
static USER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Signed-in Token \(([^)]*)\)").unwrap());
static ERROR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r" ERROR - \[Req#[A-Za-z0-9]+/Transcode/([^/\]]+)/[^\]]*\] (.*)$").unwrap()
});

#[derive(Default)]
struct Context {
    session: Option<String>,
    user: Option<String>,
    rating_key: Option<String>,
}

struct Parser<'a> {
    config: &'a PlexLogConfig,
    timezone: Tz,
    // BTreeMap plus FIFO sequence supports a strict cap without unbounded queues.
    requests: BTreeMap<String, (u64, Context)>,
    sessions: BTreeMap<String, (u64, Context)>,
    errors: BTreeMap<String, (u64, CorrelatedFailure)>,
    sequence: u64,
    snapshot: PlexLogSnapshot,
}

fn insert_bounded<T>(
    map: &mut BTreeMap<String, (u64, T)>,
    key: String,
    value: T,
    sequence: u64,
    limit: usize,
) -> bool {
    let evicted = map.len() == limit && !map.contains_key(&key);
    if evicted {
        let oldest = map
            .iter()
            .min_by_key(|(_, (seq, _))| seq)
            .unwrap()
            .0
            .clone();
        map.remove(&oldest);
    }
    map.insert(key, (sequence, value));
    evicted
}

impl<'a> Parser<'a> {
    fn new(config: &'a PlexLogConfig, timezone: Tz) -> Self {
        Self {
            config,
            timezone,
            requests: BTreeMap::new(),
            sessions: BTreeMap::new(),
            errors: BTreeMap::new(),
            sequence: 0,
            snapshot: PlexLogSnapshot::default(),
        }
    }

    fn ingest(&mut self, line: &str) {
        let Some(ts) = TIMESTAMP.captures(line) else {
            return;
        };
        let times = NaiveDateTime::parse_from_str(&ts[1], "%b %e, %Y %H:%M:%S%.f")
            .ok()
            .map(|dt| self.timezone.from_local_datetime(&dt));
        let candidates = match times {
            Some(LocalResult::Single(dt)) => Some((dt.timestamp_millis(), dt.timestamp_millis())),
            Some(LocalResult::Ambiguous(a, b)) => {
                Some((a.timestamp_millis(), b.timestamp_millis()))
            }
            _ => None,
        };
        let Some((earliest, latest)) = candidates.and_then(|(a, b)| {
            Some((u64::try_from(a.min(b)).ok()?, u64::try_from(a.max(b)).ok()?))
        }) else {
            self.snapshot.rejected_timestamps += 1;
            return;
        };
        self.sequence += 1;
        if let Some(error) = ERROR.captures(line) {
            let session = error[1].to_owned();
            let message = error[2].trim();
            let (_, mut failure) = self.errors.remove(&session).unwrap_or_else(|| {
                (
                    0,
                    CorrelatedFailure {
                        session: session.clone(),
                        user: None,
                        rating_key: None,
                        path: None,
                        errors: vec![],
                        earliest_unix_ms: earliest,
                        latest_unix_ms: latest,
                        timezone: self.config.timezone.clone(),
                        kind: PlexFailureKind::Transcoder,
                    },
                )
            });
            failure.earliest_unix_ms = failure.earliest_unix_ms.max(earliest);
            failure.latest_unix_ms = failure.latest_unix_ms.max(latest);
            if let Some(path) = message.strip_prefix("Error opening input file ") {
                let path = path.strip_suffix('.').unwrap_or(path);
                if path.starts_with('/') {
                    failure.path = Some(PathBuf::from(path));
                    failure.kind = PlexFailureKind::ReadError;
                }
            }
            if message.contains("No such file or directory") {
                failure.kind = PlexFailureKind::FileMissing;
            } else if message.contains("decod")
                || message.contains("Invalid data")
                || message.contains("header parsing failed")
            {
                failure.kind = PlexFailureKind::Decoder;
            }
            // At most five bounded, distinct diagnostic lines per session.
            let message: String = message.chars().take(160).collect();
            if !failure.errors.contains(&message) {
                if failure.errors.len() < 5 {
                    failure.errors.push(message);
                } else {
                    self.snapshot.truncated = true;
                }
            }
            self.snapshot.truncated |= insert_bounded(
                &mut self.errors,
                session,
                failure,
                self.sequence,
                self.config.max_entries,
            );
            return;
        }
        let request = REQUEST.captures(line).map(|capture| capture[1].to_owned());
        let mut context = if line.contains("Request:") {
            // Request IDs may be reused; never carry prior viewer metadata forward.
            Context::default()
        } else {
            request
                .as_ref()
                .and_then(|id| self.requests.remove(id))
                .map(|(_, context)| context)
                .unwrap_or_default()
        };
        if let Some(session) = SESSION.captures(line) {
            context.session = Some(session[1].into());
        }
        if let Some(user) = USER.captures(line) {
            context.user = Some(user[1].into());
        }
        if let Some(rating) = RATING.captures(line) {
            context.rating_key = Some(rating[1].into());
        }
        if let Some(session) = &context.session {
            let mut session_context = self
                .sessions
                .remove(session)
                .map(|(_, context)| context)
                .unwrap_or_default();
            if context.user.is_some() {
                session_context.user = context.user.clone();
            }
            if context.rating_key.is_some() {
                session_context.rating_key = context.rating_key.clone();
            }
            self.snapshot.truncated |= insert_bounded(
                &mut self.sessions,
                session.clone(),
                session_context,
                self.sequence,
                self.config.max_entries,
            );
        }
        if let Some(request) = request {
            self.snapshot.truncated |= insert_bounded(
                &mut self.requests,
                request,
                context,
                self.sequence,
                self.config.max_entries,
            );
        }
    }

    fn finish(mut self, cutoff_unix_ms: u64) -> PlexLogSnapshot {
        for (session, (_, mut failure)) in self.errors {
            // Ambiguous times overlap the requested range conservatively.
            if failure.latest_unix_ms < cutoff_unix_ms {
                continue;
            }
            if let Some((_, context)) = self.sessions.remove(&session) {
                failure.user = context.user;
                failure.rating_key = context.rating_key;
            }
            self.snapshot.failures.push(failure);
        }
        self.snapshot.failures.sort_by(|a, b| {
            b.latest_unix_ms
                .cmp(&a.latest_unix_ms)
                .then_with(|| a.session.cmp(&b.session))
        });
        self.snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::fs;

    const DATE: &str = "Jan 15, 2026 12:00:00.123";

    fn config(directory: &std::path::Path) -> PlexLogConfig {
        PlexLogConfig {
            directory: directory.into(),
            ..PlexLogConfig::default()
        }
    }

    fn request(id: &str, session: &str, user: &str, key: &str) -> String {
        format!(
            "{DATE} DEBUG - [Req#{id}] Request: GET /video?session={session}&path=%2Flibrary%2Fmetadata%2F{key} Signed-in Token ({user})\n"
        )
    }

    fn error(session: &str, message: &str) -> String {
        format!("{DATE} ERROR - [Req#42/Transcode/{session}/abc] {message}\n")
    }

    fn parse(lines: &str, config: &PlexLogConfig, cutoff: u64) -> PlexLogSnapshot {
        let mut parser = Parser::new(config, config.timezone.parse().unwrap());
        for line in lines.lines() {
            parser.ingest(line);
        }
        parser.finish(cutoff)
    }

    #[test]
    fn correlates_request_session_user_and_key_even_with_error_before_metadata() {
        let lines = format!(
            "{}{}{}{}{}{}",
            error("alpha", "Error opening input file /synthetic/a.mkv."),
            request("17", "alpha", "Viewer A", "701"),
            format_args!("{DATE} DEBUG - [Req#18] Request: GET /video?session=beta\n"),
            format_args!("{DATE} DEBUG - [Req#18] Signed-in Token (Viewer B)\n"),
            format_args!("{DATE} DEBUG - [Req#18] path=/library/metadata/702\n"),
            error("beta", "Invalid data found when processing input")
        );
        let result = parse(&lines, &config(std::path::Path::new(".")), 0);
        assert_eq!(result.failures.len(), 2);
        let a = &result.failures[0];
        assert_eq!(
            (&a.session, &a.user, &a.rating_key, &a.path),
            (
                &"alpha".into(),
                &Some("Viewer A".into()),
                &Some("701".into()),
                &Some("/synthetic/a.mkv".into())
            )
        );
        assert_eq!(a.kind, PlexFailureKind::ReadError);
        assert_eq!(a.errors, vec!["Error opening input file /synthetic/a.mkv."]);
        assert_eq!(a.earliest_unix_ms, 1_768_478_400_123);
        assert_eq!(a.latest_unix_ms, a.earliest_unix_ms);
        assert_eq!(a.timezone, "UTC");
        assert_eq!(
            a.domain_evidence(),
            Some(PlexLogFailure {
                at_unix_ms: a.latest_unix_ms,
                timezone: "UTC".into(),
                kind: PlexFailureKind::ReadError,
                file: None,
            })
        );
        assert_eq!(result.failures[1].user.as_deref(), Some("Viewer B"));
        assert_eq!(result.failures[1].rating_key.as_deref(), Some("702"));
        assert_eq!(result.failures[1].kind, PlexFailureKind::Decoder);
        assert!(!result.truncated);
        assert_eq!(result.rejected_timestamps, 0);
    }

    #[test]
    fn standalone_request_lines_correlate_without_a_request_token() {
        let line = format!(
            "{DATE} DEBUG - Request: GET /video?session=alpha&path=%2Flibrary%2Fmetadata%2F901 Signed-in Token (Viewer C)\n"
        );
        let result = parse(
            &(line + &error("alpha", "Transcoder exited")),
            &config(std::path::Path::new(".")),
            0,
        );
        assert_eq!(result.failures[0].user.as_deref(), Some("Viewer C"));
        assert_eq!(result.failures[0].rating_key.as_deref(), Some("901"));
    }

    #[test]
    fn reused_request_ids_do_not_leak_viewer_or_item() {
        let result = parse(
            &format!(
                "{}{}{}",
                request("4", "alpha", "Viewer A", "1"),
                format_args!("{DATE} DEBUG - [Req#4] Request: GET /video?session=beta\n"),
                error("beta", "Transcoder exited")
            ),
            &config(std::path::Path::new(".")),
            0,
        );
        assert_eq!(result.failures[0].user, None);
        assert_eq!(result.failures[0].rating_key, None);
        assert_eq!(result.failures[0].path, None);
        assert_eq!(result.failures[0].kind, PlexFailureKind::Transcoder);
    }

    #[test]
    fn dst_fold_is_explicit_and_gap_invalid_dates_and_pre_epoch_are_rejected() {
        let mut conf = config(std::path::Path::new("."));
        conf.timezone = "America/New_York".into();
        let repeated =
            error("fold", "Transcoder exited").replace(DATE, "Nov 01, 2026 01:30:00.000");
        let gap = error("gap", "Transcoder exited").replace(DATE, "Mar 08, 2026 02:30:00");
        let invalid = error("invalid", "Transcoder exited").replace(DATE, "Feb 30, 2026 12:00:00");
        let ancient = error("old", "Transcoder exited").replace(DATE, "Jan 01, 1900 12:00:00");
        let result = parse(
            &(repeated + &gap + &invalid + &ancient + "not a timestamp\n"),
            &conf,
            0,
        );
        assert_eq!(result.rejected_timestamps, 3);
        assert_eq!(result.failures.len(), 1);
        let fold = &result.failures[0];
        assert_eq!(fold.earliest_unix_ms, 1793511000000);
        assert_eq!(fold.latest_unix_ms - fold.earliest_unix_ms, 3_600_000);
        assert!(fold.domain_evidence().is_none());
        assert_eq!(fold.timezone, "America/New_York");
        // Local summer time has the DST offset, independent of process TZ.
        let summer = error("summer", "Transcoder exited").replace(DATE, "Jul 01, 2026 12:00:00");
        let winter = error("winter", "Transcoder exited").replace(DATE, "Jan 01, 2026 12:00:00");
        assert_eq!(
            parse(&summer, &conf, 0).failures[0].latest_unix_ms,
            1782921600000
        );
        assert_eq!(
            parse(&winter, &conf, 0).failures[0].latest_unix_ms,
            1767286800000
        );
    }

    #[test]
    fn out_of_order_errors_keep_the_latest_observation() {
        let lines =
            error("a", "newest") + &error("a", "older").replace(DATE, "Jan 15, 2026 11:59:59.123");
        let result = parse(
            &lines,
            &config(std::path::Path::new(".")),
            1_768_478_400_123,
        );
        assert_eq!(result.failures.len(), 1);
        assert_eq!(result.failures[0].earliest_unix_ms, 1_768_478_400_123);
        assert_eq!(result.failures[0].latest_unix_ms, 1_768_478_400_123);
        assert_eq!(result.failures[0].errors, ["newest", "older"]);
    }

    #[test]
    fn cutoff_includes_boundary_and_errors_sort_newest_first() {
        let lines = format!(
            "{}{}{}",
            error("a", "older").replace(DATE, "Jan 15, 2026 11:59:59.123"),
            error("b", "boundary"),
            error("c", "newest").replace(DATE, "Jan 15, 2026 12:00:01.123")
        );
        let result = parse(
            &lines,
            &config(std::path::Path::new(".")),
            1_768_478_400_123,
        );
        assert_eq!(
            result
                .failures
                .iter()
                .map(|f| f.session.as_str())
                .collect::<Vec<_>>(),
            ["c", "b"]
        );
    }

    #[test]
    fn classification_and_diagnostic_bounds() {
        for (message, kind) in [
            ("No such file or directory", PlexFailureKind::FileMissing),
            ("Error while decoding stream", PlexFailureKind::Decoder),
            ("EBML header parsing failed", PlexFailureKind::Decoder),
            (
                "Error opening input file relative.mkv.",
                PlexFailureKind::Transcoder,
            ),
            (
                "Error opening input file /synthetic/a.mkv",
                PlexFailureKind::ReadError,
            ),
        ] {
            let result = parse(&error("a", message), &config(std::path::Path::new(".")), 0);
            assert_eq!(result.failures[0].kind, kind);
        }
        let lines = (0..6)
            .map(|n| error("a", &format!("{n}{}", "é".repeat(170))))
            .collect::<String>();
        let result = parse(&lines, &config(std::path::Path::new(".")), 0);
        assert!(result.truncated);
        assert_eq!(result.failures[0].errors.len(), 5);
        assert_eq!(result.failures[0].errors[0].chars().count(), 160);
        let duplicate = error("a", "same").repeat(8);
        let result = parse(&duplicate, &config(std::path::Path::new(".")), 0);
        assert_eq!(result.failures[0].errors, ["same"]);
        assert!(!result.truncated);
    }

    #[test]
    fn bounded_correlation_evicts_oldest_and_marks_partial_evidence() {
        let mut conf = config(std::path::Path::new("."));
        conf.max_entries = 2;
        let lines = format!(
            "{}{}{}{}{}{}",
            request("1", "a", "A", "1"),
            request("2", "b", "B", "2"),
            request("3", "c", "C", "3"),
            error("a", "first"),
            error("b", "second"),
            error("c", "third")
        );
        let result = parse(&lines, &conf, 0);
        assert!(result.truncated);
        assert_eq!(result.failures.len(), 2);
        assert_eq!(result.failures[0].session, "b");
        assert_eq!(result.failures[0].user.as_deref(), Some("B"));
        assert_eq!(result.failures[1].session, "c");
        let mut parser = Parser::new(&conf, chrono_tz::UTC);
        parser.ingest(&request("1", "a", "A", "1"));
        parser.ingest(&request("2", "b", "B", "2"));
        parser.ingest(&format!("{DATE} DEBUG - [Req#1] Signed-in Token (A2)"));
        parser.ingest(&request("3", "c", "C", "3"));
        assert!(parser.requests.contains_key("1"));
        assert!(!parser.requests.contains_key("2"));
        assert!(parser.sessions.contains_key("a"));
        assert!(!parser.sessions.contains_key("b"));
    }

    #[test]
    fn either_correlation_limit_alone_reports_incomplete_evidence() {
        let mut conf = config(std::path::Path::new("."));
        conf.max_entries = 2;
        // One reusable request, three distinct sessions: only the session cap hits.
        let sessions = request("1", "a", "A", "1")
            + &request("1", "b", "B", "2")
            + &request("1", "c", "C", "3");
        assert!(parse(&sessions, &conf, 0).truncated);
        // Three request IDs with no known session: only the request cap hits.
        let requests: String = (1..=3)
            .map(|id| format!("{DATE} DEBUG - [Req#{id}] Signed-in Token (Viewer)\n"))
            .collect();
        assert!(parse(&requests, &conf, 0).truncated);
    }

    #[test]
    fn reads_rotation_then_current_and_survives_rename_and_copytruncate() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("Plex Media Server.1.log");
        let current = dir.path().join("Plex Media Server.log");
        fs::write(&old, request("1", "a", "A", "11")).unwrap();
        fs::write(&current, error("a", "No such file or directory")).unwrap();
        let reader = PlexLogReader::new(config(dir.path())).unwrap();
        let first = reader.read_since(0).unwrap();
        assert_eq!(first.files_read, 2);
        assert_eq!(first.failures[0].user.as_deref(), Some("A"));
        fs::rename(&current, &old).unwrap();
        fs::write(
            &current,
            request("2", "b", "B", "12") + &error("b", "decoder failed"),
        )
        .unwrap();
        assert_eq!(reader.read_since(0).unwrap().failures.len(), 2);
        fs::write(&current, error("c", "Transcoder exited")).unwrap();
        let third = reader.read_since(0).unwrap();
        assert_eq!(
            third
                .failures
                .iter()
                .map(|f| f.session.as_str())
                .collect::<Vec<_>>(),
            ["a", "c"]
        );
        fs::remove_file(&old).unwrap();
        fs::hard_link(&current, &old).unwrap();
        let duplicate = reader.read_since(0).unwrap();
        assert_eq!(duplicate.files_read, 1);
        assert_eq!(duplicate.failures.len(), 1);
    }

    #[test]
    fn bounded_reads_drop_partial_oversized_and_unterminated_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Plex Media Server.log");
        let good = error("a", "Transcoder exited");
        let mut conf = config(dir.path());
        conf.max_file_bytes = good.len() as u64 + 1;
        fs::write(&path, "x".repeat(100) + "\n" + &good).unwrap();
        let result = PlexLogReader::new(conf).unwrap().read_since(0).unwrap();
        assert!(result.truncated);
        assert_eq!(result.failures.len(), 1);
        let mut conf = config(dir.path());
        conf.max_line_bytes = good.len() - 1;
        fs::write(&path, "x".repeat(good.len() + 4) + "\n" + &good).unwrap();
        let result = PlexLogReader::new(conf.clone())
            .unwrap()
            .read_since(0)
            .unwrap();
        assert!(result.truncated);
        assert_eq!(result.failures.len(), 1); // exact line byte limit, excluding newline
        conf.max_line_bytes -= 1;
        let result = PlexLogReader::new(conf).unwrap().read_since(0).unwrap();
        assert!(result.truncated);
        assert!(result.failures.is_empty());
        fs::write(&path, good.trim_end()).unwrap();
        let result = PlexLogReader::new(config(dir.path()))
            .unwrap()
            .read_since(0)
            .unwrap();
        assert!(result.truncated);
        assert!(result.failures.is_empty());
    }

    #[test]
    fn only_regular_nonsymlink_fixed_names_are_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Plex Media Server.log");
        let reader = PlexLogReader::new(config(dir.path())).unwrap();
        assert_eq!(reader.read_since(0).unwrap(), PlexLogSnapshot::default());
        fs::write(dir.path().join("unrelated.log"), error("a", "failed")).unwrap();
        assert_eq!(reader.read_since(0).unwrap().files_read, 0);
        fs::create_dir(&path).unwrap();
        assert_eq!(
            reader.read_since(0).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        fs::remove_dir(&path).unwrap();
        std::os::unix::fs::symlink(dir.path().join("unrelated.log"), &path).unwrap();
        assert!(reader.read_since(0).is_err());
    }

    #[test]
    fn log_descriptors_are_nonblocking_readonly_and_not_inherited() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Plex Media Server.log");
        fs::write(&path, "").unwrap();
        let file = open_log(&path).unwrap();
        assert!(
            rustix::io::fcntl_getfd(&file)
                .unwrap()
                .contains(rustix::io::FdFlags::CLOEXEC)
        );
        let flags = rustix::fs::fcntl_getfl(&file).unwrap();
        assert!(flags.contains(rustix::fs::OFlags::NONBLOCK));
        assert_eq!(
            flags & rustix::fs::OFlags::ACCMODE,
            rustix::fs::OFlags::RDONLY
        );
    }

    #[test]
    fn config_bounds_fail_closed() {
        for mutate in [
            |c: &mut PlexLogConfig| c.directory.clear(),
            |c: &mut PlexLogConfig| c.timezone = "invalid".into(),
            |c: &mut PlexLogConfig| c.max_file_bytes = 0,
            |c: &mut PlexLogConfig| c.max_file_bytes = 64 * 1024 * 1024 + 1,
            |c: &mut PlexLogConfig| c.max_line_bytes = 0,
            |c: &mut PlexLogConfig| c.max_line_bytes = 64 * 1024 + 1,
            |c: &mut PlexLogConfig| c.max_entries = 0,
            |c: &mut PlexLogConfig| c.max_entries = 65_537,
            |c: &mut PlexLogConfig| c.rotations = 17,
        ] {
            let mut conf = config(std::path::Path::new("."));
            mutate(&mut conf);
            assert!(PlexLogReader::new(conf).is_err());
        }
        for (bytes, line, entries, rotations) in
            [(1, 1, 1, 0), (64 * 1024 * 1024, 64 * 1024, 65_536, 16)]
        {
            let conf = PlexLogConfig {
                max_file_bytes: bytes,
                max_line_bytes: line,
                max_entries: entries,
                rotations,
                ..config(std::path::Path::new("."))
            };
            assert!(PlexLogReader::new(conf).is_ok());
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn arbitrary_text_is_bounded_and_never_panics(lines in prop::collection::vec(".{0,120}", 0..100)) {
            let mut conf = config(std::path::Path::new("."));
            conf.max_entries = 4;
            let mut parser = Parser::new(&conf, chrono_tz::UTC);
            for (i, text) in lines.iter().enumerate() {
                parser.ingest(text);
                parser.ingest(&request(&i.to_string(), &i.to_string(), text, "1"));
                parser.ingest(&error(&i.to_string(), text));
                prop_assert!(parser.requests.len() <= 4);
                prop_assert!(parser.sessions.len() <= 4);
                prop_assert!(parser.errors.len() <= 4);
            }
            let result = parser.finish(0);
            prop_assert!(result.failures.len() <= 4);
        }
    }
}
