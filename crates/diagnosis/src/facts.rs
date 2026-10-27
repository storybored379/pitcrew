//! Finite, non-identifying facts. Paths, titles, viewer text and upstream identifiers
//! belong to local collectors, never to these provider-safe records.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Movie,
    #[default]
    Episode,
    Show,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Complaint {
    #[default]
    Playback,
    VideoBroken,
    NoAudio,
    WrongEpisode,
    WrongAudioLanguage,
    Subtitles,
    EverythingBroken,
    AdsOrWrongVersion,
    SelfResolved,
    Other,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    English,
    Spanish,
    #[default]
    Other,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackPresence {
    Present,
    Absent,
    #[default]
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioFacts {
    pub requested: Language,
    /// Only set after inspecting the audio streams of the exact reported file.
    pub requested_track: TrackPresence,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeOutcome {
    #[default]
    Unknown,
    OpenFailed,
    NoVideo,
    Video,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecodeOutcome {
    #[default]
    Unknown,
    Good,
    Bad,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DvProfile {
    #[default]
    Unknown,
    Five,
    Eight,
    Other,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DynamicRange {
    pub dolby_vision: bool,
    pub hdr: bool,
    pub no_hdr_fallback: bool,
    pub profile: DvProfile,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackDecision {
    #[default]
    Unknown,
    DirectPlay,
    DirectStream,
    Transcode,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionFacts {
    /// Counts must be correlated to the reported item and viewer locally.
    pub transcodes: u32,
    pub stalled: u32,
    /// Other-item counts must also match the same device.
    pub other_items: u32,
    pub other_direct_plays: u32,
    pub other_stalled: u32,
    pub newest_decision: PlaybackDecision,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderFacts {
    pub miss_percent: Option<u8>,
    pub served_bps: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaybackFacts {
    pub is_4k: bool,
    pub probe: ProbeOutcome,
    pub decode: DecodeOutcome,
    pub plex_looks_broken: bool,
    pub analyzed_recently: bool,
    pub dynamic_range: DynamicRange,
    pub sessions: SessionFacts,
    pub provider: Option<ProviderFacts>,
    pub file_bps: Option<u64>,
    pub wan: bool,
    pub hd_library_available: bool,
}

/// Healthy requires a reachable server AND positive playback by other viewers.
/// An unavailable optional telemetry source is Unknown, never Unavailable here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerHealth {
    #[default]
    Unknown,
    Healthy,
    Unavailable,
}

/// Log context has path/window correlation only, never proof of exact bytes or
/// stale Plex metadata. Counts cannot authorize a replacement by themselves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlexLogFacts {
    pub available: bool,
    pub matching_failures: u32,
    pub unknown_correlation: u32,
    pub truncated: bool,
    /// Discarded timestamped log lines make this observation incomplete even
    /// when the scan reached its end without hitting the size limit.
    #[serde(default)]
    pub rejected_timestamps: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceFacts {
    pub server_health: ServerHealth,
    pub plex_logs: PlexLogFacts,
    pub complaint: Complaint,
    pub scope: Scope,
    pub playback: PlaybackFacts,
    pub audio: AudioFacts,
    pub injection_suspected: bool,
}
