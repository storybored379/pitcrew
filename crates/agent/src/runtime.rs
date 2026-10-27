//! Bind generic evidence to the local API. No upstream client is constructed.
use crate::{
    http::{ApiError, InspectFuture, Inspector, Operation},
    media::{MediaAgent, MediaError},
    plex_logs::{PlexLogReader, PlexLogSnapshot},
};
use pitcrew_domain::media::{MediaEvidence, ProbeOutcome, StreamFacts, SymlinkResolution};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Semaphore, oneshot};

pub struct LocalInspector {
    media: MediaAgent,
    logs: Option<Arc<PlexLogReader>>,
    log_slots: Arc<Semaphore>,
    log_timeout: Duration,
}
impl LocalInspector {
    pub fn new(media: MediaAgent, logs: Option<PlexLogReader>, log_timeout: Duration) -> Self {
        Self {
            media,
            logs: logs.map(Arc::new),
            log_slots: Arc::new(Semaphore::new(1)),
            log_timeout,
        }
    }
    async fn logs(&self, cutoff: u64) -> Result<PlexLogSnapshot, ApiError> {
        let reader = self.logs.clone().ok_or(ApiError::Unavailable)?;
        let permit = self
            .log_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| ApiError::Busy)?;
        let (sender, receiver) = oneshot::channel();
        std::thread::Builder::new()
            .name("plex-log-reader".into())
            .spawn(move || {
                let result = reader.read_since(cutoff);
                drop(permit);
                let _ = sender.send(result);
            })
            .map_err(|_| ApiError::Unavailable)?;
        let snapshot = tokio::time::timeout(self.log_timeout, receiver)
            .await
            .map_err(|_| ApiError::Unavailable)?
            .map_err(|_| ApiError::Unavailable)?
            .map_err(|_| ApiError::Unavailable)?;
        if snapshot.files_read == 0 {
            return Err(ApiError::Unavailable);
        }
        Ok(snapshot)
    }
}
impl Inspector for LocalInspector {
    fn inspect(&self, operation: Operation) -> InspectFuture<'_> {
        Box::pin(async move {
            if let Operation::PlexFailures {
                minutes,
                compatibility,
            } = operation
            {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
                    .min(u128::from(u64::MAX)) as u64;
                let snapshot = self
                    .logs(now.saturating_sub(u64::from(minutes) * 60_000))
                    .await?;
                return log_response(snapshot, now, compatibility);
            }
            let (path, decode) = match &operation {
                Operation::Evidence { path, decode } => (path, *decode),
                Operation::Decode { path } => (path, true),
                Operation::Probe { path } | Operation::File { path } => (path, false),
                Operation::PlexFailures { .. } => unreachable!(),
            };
            let evidence = self
                .media
                .inspect(Path::new(path), decode)
                .await
                .map_err(|error| match error {
                    MediaError::Forbidden => ApiError::Forbidden,
                    MediaError::Busy => ApiError::Busy,
                    MediaError::Changed | MediaError::InvalidConfig => ApiError::Unavailable,
                })?;
            Ok(match operation {
                Operation::Evidence { .. } => {
                    serde_json::to_value(&evidence).map_err(|_| ApiError::Unavailable)?
                }
                Operation::Probe { .. } => probe_response(&evidence),
                Operation::Decode { .. } => decode_response(&evidence),
                Operation::File { .. } => file_response(&evidence),
                Operation::PlexFailures { .. } => unreachable!(),
            })
        })
    }
}

pub fn probe_response(e: &MediaEvidence) -> Value {
    let opened = e.header.as_ref().and_then(|h| match h.outcome {
        ProbeOutcome::Passed => Some(true),
        ProbeOutcome::Failed => Some(false),
        _ => None,
    });
    let video = e.header.as_ref().map_or(0, |h| {
        h.streams
            .iter()
            .filter(|s| matches!(s, StreamFacts::Video { .. }))
            .count()
    });
    json!({"opened":opened,"video_streams":video,"duration_s":e.header.as_ref().and_then(|h|h.duration_ms).unwrap_or(0) as f64/1000.0,"evidence":e})
}
pub fn decode_response(e: &MediaEvidence) -> Value {
    let faults = e.header.as_ref().is_some_and(|h| !h.faults.is_empty())
        || e.decode.as_ref().is_some_and(|d| !d.faults.is_empty());
    // A hard read error (not a timeout) on a mount that answered is evidence
    // against the file itself, e.g. missing source data behind a symlink.
    let read_failed = e
        .mount
        .as_ref()
        .is_some_and(|m| m.outcome == ProbeOutcome::Passed)
        && e.reads.iter().any(|r| r.outcome == ProbeOutcome::Failed);
    let verdict = if faults
        || read_failed
        || e.header
            .as_ref()
            .is_some_and(|h| h.outcome == ProbeOutcome::Failed)
        || e.decode
            .as_ref()
            .is_some_and(|d| d.outcome == ProbeOutcome::Failed)
    {
        "bad"
    } else if e.file.id != 0
        && e.header.as_ref().is_some_and(|h| {
            h.outcome == ProbeOutcome::Passed && h.duration_ms.is_some_and(|d| d > 0)
        })
        && e.decode
            .as_ref()
            .is_some_and(|d| d.outcome == ProbeOutcome::Passed && !d.tested_ranges.is_empty())
    {
        "ok"
    } else {
        "unknown"
    };
    json!({"verdict":verdict,"duration_s":e.header.as_ref().and_then(|h|h.duration_ms).unwrap_or(0) as f64/1000.0,"samples":e.decode.as_ref().map(|d|&d.tested_ranges),"evidence":e})
}
pub fn file_response(e: &MediaEvidence) -> Value {
    let resolution = e.symlinks.as_ref().map(|s| &s.resolution);
    let symlink = e.symlinks.as_ref().is_some_and(|s| !s.chain.is_empty());
    let target_exists = match resolution {
        Some(SymlinkResolution::Resolved(_)) => Some(true),
        Some(SymlinkResolution::Dangling(_)) => Some(false),
        _ => None,
    };
    json!({"symlink":symlink,"exists":if symlink {Some(true)}else{target_exists},"target_exists":target_exists,"read_sessions":null,"evidence":e})
}

/// Legacy clients cannot represent a DST fold interval. Omit those rows instead
/// of choosing a precise playback time that the log does not establish.
pub fn log_response(
    snapshot: PlexLogSnapshot,
    now: u64,
    compatibility: bool,
) -> Result<Value, ApiError> {
    if !compatibility {
        return serde_json::to_value(snapshot).map_err(|_| ApiError::Unavailable);
    }
    let omitted_ambiguous = snapshot
        .failures
        .iter()
        .filter(|f| f.earliest_unix_ms != f.latest_unix_ms)
        .count();
    let fails: Vec<_> = snapshot.failures.into_iter()
        .filter(|f| f.earliest_unix_ms == f.latest_unix_ms)
        .map(|f| json!({
            "session":f.session,"user":f.user,"rating_key":f.rating_key,"path":f.path,"errors":f.errors,
            "minutes_ago":now.saturating_sub(f.latest_unix_ms)/60_000,
            "timestamp_ambiguous":false,
        })).collect();
    Ok(
        json!({"count":fails.len(),"fails":fails,"truncated":snapshot.truncated,
        "rejected_timestamps":snapshot.rejected_timestamps,"omitted_ambiguous":omitted_ambiguous}),
    )
}
