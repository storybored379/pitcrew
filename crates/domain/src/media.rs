//! Storage-independent observations. Paths and source identifiers stay local;
//! cloud evidence must use a separately redacted representation.

use crate::{FileIdentity, IntentKey};
use serde::{Deserialize, Serialize};
use std::{future::Future, path::PathBuf, pin::Pin};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseGuid {
    /// Canonical indexer/arr namespace supplied by the adapter, not a hostname.
    pub source: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InfoHash(pub String);

/// A client-local download ID has meaning only inside that client instance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadIdentity {
    pub client_id: String,
    pub download_id: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseIdentity {
    pub guid: Option<ReleaseGuid>,
    pub infohash: Option<InfoHash>,
    pub download: Option<DownloadIdentity>,
}

impl ReleaseIdentity {
    /// Any known identifier match excludes the candidate even if its title or
    /// release group changed. Missing/empty identifiers never establish a match.
    pub fn overlaps(&self, other: &Self) -> bool {
        let guid = match (&self.guid, &other.guid) {
            (Some(a), Some(b)) => !a.source.is_empty() && !a.value.is_empty() && a == b,
            _ => false,
        };
        let hash = match (&self.infohash, &other.infohash) {
            (Some(a), Some(b)) => !a.0.is_empty() && a.0.eq_ignore_ascii_case(&b.0),
            _ => false,
        };
        let download = match (&self.download, &other.download) {
            (Some(a), Some(b)) => !a.client_id.is_empty() && !a.download_id.is_empty() && a == b,
            _ => false,
        };
        guid || hash || download
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymlinkHop {
    pub link: PathBuf,
    pub target: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SymlinkResolution {
    Resolved(PathBuf),
    Dangling(PathBuf),
    Cycle,
    HopLimit,
    PermissionDenied,
    TimedOut,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymlinkEvidence {
    pub chain: Vec<SymlinkHop>,
    pub resolution: SymlinkResolution,
    pub elapsed_ms: u64,
    pub timeout_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProbeOutcome {
    Passed,
    Failed,
    TimedOut,
    PermissionDenied,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountEvidence {
    /// Opaque local mount identifier; no particular storage technology required.
    pub mount: String,
    pub outcome: ProbeOutcome,
    pub elapsed_ms: u64,
    pub timeout_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimedRead {
    pub offset_bytes: u64,
    pub requested_bytes: u64,
    pub read_bytes: u64,
    /// None means no byte arrived before completion/timeout.
    pub first_byte_ms: Option<u64>,
    pub elapsed_ms: u64,
    pub timeout_ms: u64,
    /// Measured throughput, retained with raw timings (zero is real evidence).
    pub bytes_per_second: u64,
    pub outcome: ProbeOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaFault {
    NoDuration,
    SlowRead,
    DecodeError,
    InvalidHeader,
    NoVideo,
    NoAudio,
    Truncated,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamFacts {
    Video {
        codec: String,
        width: u32,
        height: u32,
        bit_depth: Option<u8>,
        /// Normalized probe values: SDR, HDR10, HLG, DOVI_PROFILE_5,
        /// DOVI_PROFILE_7 or DOVI_PROFILE_8. Missing/unrecognized values are unknown.
        dynamic_range: Option<String>,
    },
    Audio {
        codec: String,
        channels: u16,
        language: Option<String>,
    },
    Subtitle {
        codec: String,
        language: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecodeRange {
    pub start_ms: u64,
    /// None means testing through EOF, including media with no known duration.
    pub end_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaProbe {
    pub outcome: ProbeOutcome,
    pub duration_ms: Option<u64>,
    pub elapsed_ms: u64,
    pub timeout_ms: u64,
    /// Header success does not erase no-duration, slow-read, or decode evidence.
    pub faults: Vec<MediaFault>,
    #[serde(default)]
    pub streams: Vec<StreamFacts>,
    /// Actual tested media-time ranges, not a claim that the whole file passed.
    #[serde(default)]
    pub tested_ranges: Vec<DecodeRange>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlexFailureKind {
    FileMissing,
    ReadError,
    Decoder,
    Transcoder,
    PlaybackStart,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlexLogFailure {
    pub at_unix_ms: u64,
    /// The explicitly configured timezone used when parsing a zone-free log.
    pub timezone: String,
    pub kind: PlexFailureKind,
    pub file: Option<FileIdentity>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaEvidence {
    pub file: FileIdentity,
    pub observed_at_unix_ms: u64,
    pub symlinks: Option<SymlinkEvidence>,
    pub mount: Option<MountEvidence>,
    pub reads: Vec<TimedRead>,
    pub header: Option<MediaProbe>,
    pub decode: Option<MediaProbe>,
    pub plex_failures: Vec<PlexLogFailure>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceKind {
    NzbdavInfinidysk,
    Debrid,
    Qbittorrent,
    Sabnzbd,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceFact {
    ProviderAvailability {
        provider: String,
        available: bool,
    },
    ProviderMisses {
        provider: String,
        requests: u64,
        missing: u64,
        window_ms: u64,
    },
    DownloadProgress {
        downloaded_bytes: u64,
        total_bytes: Option<u64>,
        bytes_per_second: u64,
        complete: bool,
    },
    UnpackFailed,
    RepairAvailable(bool),
    Cached(bool),
    InAccount(bool),
    ClientAvailable(bool),
    DownloadFailed,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceFacts {
    pub source: SourceKind,
    pub observed_at_unix_ms: u64,
    pub release: ReleaseIdentity,
    pub facts: Vec<SourceFact>,
}

/// Repair is a separately authorized, committed side effect, never a probe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceRepairRequest {
    pub intent: IntentKey,
    pub file: FileIdentity,
    pub path: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceRepairOutcome {
    Completed,
    Ambiguous,
    Rejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceError {
    Unsupported,
    Unavailable,
    TimedOut,
}

pub type SourceFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, SourceError>> + Send + 'a>>;

/// Optional backend facts enrich generic evidence. Normalized facts adapters may
/// exist before their version-specific upstream transports are certified.
pub trait SourceAdapter: Send + Sync {
    fn kind(&self) -> SourceKind;
    fn facts<'a>(&'a self, path: &'a std::path::Path) -> SourceFuture<'a, SourceFacts>;
    /// None declares a facts-only adapter. A repair-capable adapter must still
    /// require a persisted intent, access validation and explicit repair opt-in.
    fn repair(&self) -> Option<&dyn SourceRepair>;
}

pub trait SourceRepair: Send + Sync {
    fn repair<'a>(
        &'a self,
        request: &'a SourceRepairRequest,
    ) -> SourceFuture<'a, SourceRepairOutcome>;
}

impl MediaProbe {
    /// Exact-file proof for the no-Dolby-Vision-profile-5 replacement invariant.
    /// Unknown metadata never authorizes a replacement. HDR and SDR are allowed.
    pub fn excludes_dolby_vision_profile5(&self) -> Option<bool> {
        if self.outcome != ProbeOutcome::Passed || !self.faults.is_empty() {
            return None;
        }
        let mut saw_video = false;
        let mut allowed = true;
        for stream in &self.streams {
            if let StreamFacts::Video { dynamic_range, .. } = stream {
                saw_video = true;
                allowed &= match dynamic_range.as_deref()? {
                    "SDR" | "HDR10" | "HLG" | "DOVI_PROFILE_7" | "DOVI_PROFILE_8" => true,
                    "DOVI_PROFILE_5" => false,
                    _ => return None,
                };
            }
        }
        saw_video.then_some(allowed)
    }
}
