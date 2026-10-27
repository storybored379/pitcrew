//! Read-only, bounded evidence collectors. Local identities and paths are never
//! serializable or printable through these wrappers. A missing source is unknown.
use pitcrew_domain::{FileIdentity, media::MediaEvidence};
use pitcrew_upstream::{Endpoint, Error, JsonClient, Transport, agent};
use serde::Deserialize;
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unavailable {
    NotConfigured,
    InvalidRequest,
    Timeout,
    Unauthorized,
    Upstream,
    InvalidResponse,
    TooLarge,
    Stale,
    IdentityChanged,
}

#[derive(Clone, PartialEq, Eq)]
pub enum Observation<T> {
    Available(T),
    Unavailable(Unavailable),
}
// Even T's Debug may contain usernames, identities, paths or raw log lines.
impl<T> fmt::Debug for Observation<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Available(_) => f.write_str("Available([local evidence])"),
            Self::Unavailable(reason) => f.debug_tuple("Unavailable").field(reason).finish(),
        }
    }
}
impl<T> Default for Observation<T> {
    fn default() -> Self {
        Self::Unavailable(Unavailable::NotConfigured)
    }
}
fn unavailable(error: Error) -> Unavailable {
    match error {
        Error::Configuration(_) | Error::InvalidInput | Error::ShadowDenied => {
            Unavailable::InvalidRequest
        }
        Error::Timeout => Unavailable::Timeout,
        Error::Status(401 | 403) => Unavailable::Unauthorized,
        Error::TooLarge => Unavailable::TooLarge,
        Error::InvalidResponse => Unavailable::InvalidResponse,
        Error::Transport | Error::Status(_) => Unavailable::Upstream,
    }
}

/// Caller supplies a trusted local path, never a viewer-provided path.
/// `expected_file` is an optional identity from the caller's current inventory.
pub struct AgentQuery<'a> {
    pub path: &'a str,
    pub expected_file: Option<FileIdentity>,
    pub minutes: u16,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentEvidence {
    pub media: Observation<MediaEvidence>,
    pub plex: Observation<PlexContext>,
}
impl AgentEvidence {
    /// Replace collector-owned facts, including exact-file dynamic range.
    /// Plex metadata and arr bitrate remain their own observations.
    pub fn apply_to(&self, facts: &mut crate::facts::EvidenceFacts) {
        use crate::facts::{DecodeOutcome as Decode, DynamicRange, ProbeOutcome as Probe};
        use pitcrew_domain::media::{MediaFault, ProbeOutcome as Outcome, StreamFacts};
        facts.playback.probe = Probe::Unknown;
        facts.playback.decode = Decode::Unknown;
        facts.playback.dynamic_range = DynamicRange::default();
        facts.plex_logs = match &self.plex {
            Observation::Available(logs) => crate::facts::PlexLogFacts {
                available: true,
                matching_failures: logs.matching_failures,
                unknown_correlation: logs.unknown_correlation,
                truncated: logs.truncated,
                rejected_timestamps: logs.rejected_timestamps,
            },
            Observation::Unavailable(_) => crate::facts::PlexLogFacts::default(),
        };
        let Observation::Available(media) = &self.media else {
            return;
        };
        if let Some(header) = &media.header {
            if media.file.id != 0
                && media.file.revision != 0
                && header.outcome == Outcome::Passed
                && header.faults.is_empty()
            {
                let ranges: Vec<_> = header
                    .streams
                    .iter()
                    .filter_map(|stream| match stream {
                        StreamFacts::Video { dynamic_range, .. } => Some(dynamic_range.as_deref()),
                        _ => None,
                    })
                    .collect();
                if let [Some(range)] = ranges.as_slice() {
                    facts.playback.dynamic_range = observed_dynamic_range(range);
                }
            }
            facts.playback.probe = match header.outcome {
                Outcome::Failed => Probe::OpenFailed,
                Outcome::Passed
                    if header
                        .streams
                        .iter()
                        .any(|s| matches!(s, StreamFacts::Video { .. })) =>
                {
                    Probe::Video
                }
                Outcome::Passed if header.faults.contains(&MediaFault::NoVideo) => Probe::NoVideo,
                _ => Probe::Unknown,
            };
        }
        if media
            .decode
            .as_ref()
            .is_some_and(|d| d.outcome == Outcome::Failed)
            || (media
                .mount
                .as_ref()
                .is_some_and(|m| m.outcome == Outcome::Passed)
                && media.reads.iter().any(|r| r.outcome == Outcome::Failed))
        {
            facts.playback.decode = Decode::Bad;
        } else if media.file.id != 0
            && media.header.as_ref().is_some_and(|h| {
                h.outcome == Outcome::Passed
                    && h.duration_ms.is_some_and(|d| d > 0)
                    && h.faults.is_empty()
            })
            && media.decode.as_ref().is_some_and(|d| {
                d.outcome == Outcome::Passed && d.faults.is_empty() && !d.tested_ranges.is_empty()
            })
        {
            facts.playback.decode = Decode::Good;
        }
    }
}
fn observed_dynamic_range(range: &str) -> crate::facts::DynamicRange {
    use crate::facts::{DvProfile, DynamicRange};
    match range {
        "HDR10" | "HLG" => DynamicRange {
            hdr: true,
            ..DynamicRange::default()
        },
        "DOVI_PROFILE_5" => DynamicRange {
            dolby_vision: true,
            no_hdr_fallback: true,
            profile: DvProfile::Five,
            ..DynamicRange::default()
        },
        "DOVI_PROFILE_7" => DynamicRange {
            dolby_vision: true,
            hdr: true,
            profile: DvProfile::Other,
            ..DynamicRange::default()
        },
        // Profile 8 has several compatibility modes. Its number alone does
        // not establish whether the base layer is HDR10, HLG, or SDR.
        "DOVI_PROFILE_8" => DynamicRange {
            dolby_vision: true,
            profile: DvProfile::Eight,
            ..DynamicRange::default()
        },
        _ => DynamicRange::default(),
    }
}
/// Path correlation supplies item context, never an exact imported FileIdentity.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlexContext {
    pub matching_failures: u32,
    pub unrelated_failures: u32,
    pub unknown_correlation: u32,
    pub truncated: bool,
    pub rejected_timestamps: u64,
}
pub struct AgentCollector<T>(agent::Client<T>);
impl<T: Transport> AgentCollector<T> {
    pub fn new(endpoint: Endpoint, transport: T) -> Self {
        Self(agent::Client::new(endpoint, transport))
    }
    pub async fn collect(&self, query: AgentQuery<'_>) -> AgentEvidence {
        self.collect_with_clock(query, || {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        })
        .await
    }
    /// The clock is sampled after each response, so observations created during
    /// the request are not mistaken for future data. None means no usable clock.
    pub async fn collect_with_clock(
        &self,
        query: AgentQuery<'_>,
        clock: impl Fn() -> Option<u64>,
    ) -> AgentEvidence {
        if !(1..=1440).contains(&query.minutes) {
            return AgentEvidence {
                media: Observation::Unavailable(Unavailable::InvalidRequest),
                plex: Observation::Unavailable(Unavailable::InvalidRequest),
            };
        }
        let response = self.0.evidence(query.path, true).await;
        let Some(now) = clock() else {
            return AgentEvidence {
                media: Observation::Unavailable(Unavailable::InvalidRequest),
                plex: Observation::Unavailable(Unavailable::InvalidRequest),
            };
        };
        let since = now.saturating_sub(u64::from(query.minutes) * 60_000);
        let media = match response {
            Ok(e) if e.observed_at_unix_ms < since || e.observed_at_unix_ms > now => {
                Observation::Unavailable(Unavailable::Stale)
            }
            Ok(e) if query.expected_file.is_some_and(|file| file != e.file) => {
                Observation::Unavailable(Unavailable::IdentityChanged)
            }
            Ok(e) => Observation::Available(e),
            Err(Error::InvalidInput) => {
                return AgentEvidence {
                    media: Observation::Unavailable(Unavailable::InvalidRequest),
                    plex: Observation::Unavailable(Unavailable::InvalidRequest),
                };
            }
            Err(error) => Observation::Unavailable(unavailable(error)),
        };
        let response = self.0.plex_failures(query.minutes).await;
        let Some(now) = clock() else {
            return AgentEvidence {
                media,
                plex: Observation::Unavailable(Unavailable::InvalidRequest),
            };
        };
        let since = now.saturating_sub(u64::from(query.minutes) * 60_000);
        let plex = match response {
            Ok(value) => match serde_json::from_value::<LogSnapshot>(value) {
                Ok(snapshot) if snapshot.failures.len() <= 4096 => {
                    let mut context = PlexContext {
                        truncated: snapshot.truncated,
                        rejected_timestamps: snapshot.rejected_timestamps,
                        ..PlexContext::default()
                    };
                    for failure in snapshot.failures {
                        if failure.earliest_unix_ms != failure.latest_unix_ms
                            || failure.earliest_unix_ms < since
                            || failure.latest_unix_ms > now
                        {
                            context.unknown_correlation += 1;
                        } else {
                            match failure.path.as_deref() {
                                Some(path) if path == query.path => context.matching_failures += 1,
                                Some(_) => context.unrelated_failures += 1,
                                None => context.unknown_correlation += 1,
                            }
                        }
                    }
                    Observation::Available(context)
                }
                _ => Observation::Unavailable(Unavailable::InvalidResponse),
            },
            Err(error) => Observation::Unavailable(unavailable(error)),
        };
        AgentEvidence { media, plex }
    }
}
#[derive(Deserialize)]
struct LogSnapshot {
    failures: Vec<LogFailure>,
    truncated: bool,
    rejected_timestamps: u64,
}
#[derive(Deserialize)]
struct LogFailure {
    path: Option<String>,
    earliest_unix_ms: u64,
    latest_unix_ms: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Presence {
    OnServer,
    /// Healthy recording, known viewer, no session in the queried window. This
    /// never claims the viewer chose the wrong server or authorizes a mutation.
    NotSeen,
    #[default]
    Unknown,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Playback {
    Direct,
    Transcode,
    #[default]
    Unknown,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionEvidence {
    pub presence: Presence,
    pub attempts: u32,
    pub transcodes: u32,
    pub direct_plays: u32,
    pub stalled: u32,
    /// Only other items from the newest matching session's exact device pair.
    pub other_items: u32,
    pub other_direct_plays: u32,
    pub other_stalled: u32,
    pub newest_decision: Playback,
    pub latest_stream_bps: Option<u64>,
}
impl SessionEvidence {
    /// The observed bitrate is the stream's, not the library file's. Keep it out
    /// of `file_bps`, which source-throughput decisions compare against.
    pub fn apply_to(&self, facts: &mut crate::facts::EvidenceFacts) {
        use crate::facts::{PlaybackDecision, SessionFacts};
        facts.playback.sessions = SessionFacts {
            transcodes: self.transcodes,
            stalled: self.stalled,
            other_items: self.other_items,
            other_direct_plays: self.other_direct_plays,
            other_stalled: self.other_stalled,
            newest_decision: match self.newest_decision {
                Playback::Direct => PlaybackDecision::DirectPlay,
                Playback::Transcode => PlaybackDecision::Transcode,
                Playback::Unknown => PlaybackDecision::Unknown,
            },
        };
    }
}
/// Session lookup is local. Neither viewer identities nor rating keys are cloud
/// evidence; only the normalized counters returned by `collect` are suitable.
pub struct SessionQuery<'a> {
    pub plex_account_id: u64,
    /// Exact-username fallback for rows without a Plex account mapping.
    pub username: Option<&'a str>,
    pub rating_key: &'a str,
    pub hours: u16,
}
/// Explicitly implements the external collector's read-only Tracearr metrics
/// bridge contract, not an assumed native Tracearr HTTP or database interface.
pub struct TracearrBridgeClient<T>(JsonClient<T>);
impl<T: Transport> TracearrBridgeClient<T> {
    pub fn new(endpoint: Endpoint, transport: T) -> Self {
        Self(JsonClient::new(endpoint, transport))
    }
    pub async fn collect(
        &self,
        query: SessionQuery<'_>,
        tautulli_streams: Observation<u32>,
    ) -> Observation<SessionEvidence> {
        if query.plex_account_id == 0
            || query.rating_key.is_empty()
            || query.rating_key.len() > 128
            || !query.rating_key.bytes().all(|c| c.is_ascii_digit())
            || !(1..=1440).contains(&query.hours)
            || query
                .username
                .is_some_and(|s| s.is_empty() || s.len() > 256 || s.chars().any(char::is_control))
        {
            return Observation::Unavailable(Unavailable::InvalidRequest);
        }
        let health = self
            .0
            .get::<TracearrHealth>("tracearr-health", vec![])
            .await;
        let mut params = vec![
            ("plex_account_id".into(), query.plex_account_id.to_string()),
            ("hours".into(), query.hours.to_string()),
        ];
        if let Some(username) = query.username {
            params.push(("username".into(), username.into()));
        }
        let mut rows = match self.0.get::<TracearrSessions>("sessions", params).await {
            Ok(rows) if rows.sessions.len() <= 40 => rows,
            Ok(_) => return Observation::Unavailable(Unavailable::InvalidResponse),
            Err(error) => return Observation::Unavailable(unavailable(error)),
        };
        let original_len = rows.sessions.len();
        rows.sessions.retain(|row| {
            row.minutes_ago
                .is_some_and(|age| age <= u64::from(query.hours) * 60)
        });
        rows.sessions.sort_by_key(|row| row.minutes_ago);
        let healthy = health.is_ok_and(|h| {
            h.sessions_24h > 0
                && h.latest_min_ago.is_some_and(|age| age <= 1440)
                && !matches!(tautulli_streams, Observation::Available(n) if n > 0 && h.active_now == 0)
        });
        let presence = if !rows.sessions.is_empty() {
            Presence::OnServer
        } else if healthy && rows.user_known == Some(true) && original_len == 0 {
            Presence::NotSeen
        } else {
            Presence::Unknown
        };
        let same: Vec<_> = rows
            .sessions
            .iter()
            .filter(|row| row.rating_key.as_deref() == Some(query.rating_key))
            .collect();
        let mut evidence = SessionEvidence {
            presence,
            ..SessionEvidence::default()
        };
        for row in &same {
            evidence.attempts += 1;
            match playback(row.video_decision.as_deref()) {
                Playback::Direct => evidence.direct_plays += 1,
                Playback::Transcode => evidence.transcodes += 1,
                Playback::Unknown => {}
            }
            evidence.stalled += u32::from(stalled(row));
        }
        if let Some(newest) = same.first() {
            evidence.newest_decision = playback(newest.video_decision.as_deref());
            evidence.latest_stream_bps = newest.bitrate.and_then(|kbps| kbps.checked_mul(1000));
            if let (Some(product), Some(player)) = (&newest.product, &newest.player_name)
                && !product.is_empty()
                && !player.is_empty()
            {
                for row in &rows.sessions {
                    if row
                        .rating_key
                        .as_deref()
                        .is_some_and(|key| key != query.rating_key)
                        && row.product.as_ref() == Some(product)
                        && row.player_name.as_ref() == Some(player)
                    {
                        evidence.other_items += 1;
                        evidence.other_direct_plays +=
                            u32::from(playback(row.video_decision.as_deref()) == Playback::Direct);
                        evidence.other_stalled += u32::from(stalled(row));
                    }
                }
            }
        }
        Observation::Available(evidence)
    }
}
#[derive(Deserialize)]
struct TracearrHealth {
    sessions_24h: u64,
    latest_min_ago: Option<u64>,
    active_now: u64,
}
#[derive(Deserialize)]
struct TracearrSessions {
    sessions: Vec<TracearrSession>,
    user_known: Option<bool>,
}
#[derive(Deserialize)]
struct TracearrSession {
    minutes_ago: Option<u64>,
    rating_key: Option<String>,
    product: Option<String>,
    player_name: Option<String>,
    video_decision: Option<String>,
    bitrate: Option<u64>,
    wall_seconds: Option<u64>,
    progress_seconds: Option<u64>,
    watched: Option<bool>,
}
fn playback(value: Option<&str>) -> Playback {
    match value {
        Some("directplay" | "direct play" | "copy") => Playback::Direct,
        Some("transcode") => Playback::Transcode,
        _ => Playback::Unknown,
    }
}
fn stalled(row: &TracearrSession) -> bool {
    // A long pause, missing watch state, or missing progress proves no stall.
    row.wall_seconds.is_some_and(|s| s < 180)
        && row.watched == Some(false)
        && row.progress_seconds.is_some_and(|s| s < 600)
}

/// Native Tautulli GET commands. Its API requires an `apikey` query parameter;
/// credentials stay in this non-Debug wrapper and are never returned in errors.
pub struct TautulliClient<T> {
    client: JsonClient<T>,
    api_key: String,
}
impl<T: Transport> TautulliClient<T> {
    pub fn new(endpoint: Endpoint, transport: T, api_key: String) -> Result<Self, Error> {
        if api_key.is_empty()
            || api_key.len() > 4096
            || !api_key.bytes().all(|b| b.is_ascii_graphic())
        {
            return Err(Error::InvalidInput);
        }
        Ok(Self {
            client: JsonClient::new(endpoint, transport),
            api_key,
        })
    }
    /// Recent matching plays, as supporting context only. Tautulli can discard
    /// short failed starts, so an empty history never proves `NotSeen`.
    pub async fn history(
        &self,
        plex_account_id: u64,
        rating_key: &str,
        now_unix_seconds: u64,
        hours: u16,
    ) -> Observation<SessionEvidence> {
        if plex_account_id == 0
            || rating_key.is_empty()
            || rating_key.len() > 128
            || !rating_key.bytes().all(|b| b.is_ascii_digit())
            || !(1..=1440).contains(&hours)
        {
            return Observation::Unavailable(Unavailable::InvalidRequest);
        }
        let response = self
            .client
            .get::<TautulliResponse>(
                "api/v2",
                vec![
                    ("apikey".into(), self.api_key.clone()),
                    ("cmd".into(), "get_history".into()),
                    ("user_id".into(), plex_account_id.to_string()),
                    ("length".into(), "40".into()),
                ],
            )
            .await;
        let rows = match response {
            Ok(response) if response.response.result == "success" => {
                match serde_json::from_value::<TautulliHistory>(response.response.data) {
                    Ok(history) if history.data.len() <= 40 => history.data,
                    _ => return Observation::Unavailable(Unavailable::InvalidResponse),
                }
            }
            Ok(_) => return Observation::Unavailable(Unavailable::InvalidResponse),
            Err(error) => return Observation::Unavailable(unavailable(error)),
        };
        let since = now_unix_seconds.saturating_sub(u64::from(hours) * 3600);
        let mut matching: Vec<_> = rows
            .into_iter()
            .filter(|row| {
                row.rating_key.as_deref() == Some(rating_key)
                    && row
                        .date
                        .is_some_and(|date| date >= since && date <= now_unix_seconds)
            })
            .collect();
        matching.sort_by_key(|row| std::cmp::Reverse(row.date));
        let mut evidence = SessionEvidence::default();
        for row in &matching {
            evidence.attempts += 1;
            match playback(row.transcode_decision.as_deref()) {
                Playback::Direct => evidence.direct_plays += 1,
                Playback::Transcode => evidence.transcodes += 1,
                Playback::Unknown => {}
            }
        }
        if let Some(newest) = matching.first() {
            evidence.presence = Presence::OnServer;
            evidence.newest_decision = playback(newest.transcode_decision.as_deref());
        }
        Observation::Available(evidence)
    }
    pub async fn stream_count(&self) -> Observation<u32> {
        let response = self
            .client
            .get::<TautulliResponse>(
                "api/v2",
                vec![
                    ("apikey".into(), self.api_key.clone()),
                    ("cmd".into(), "get_activity".into()),
                ],
            )
            .await;
        match response {
            Ok(response) if response.response.result == "success" => {
                match response.response.data.get("stream_count").and_then(count) {
                    Some(n) => Observation::Available(n),
                    None => Observation::Unavailable(Unavailable::InvalidResponse),
                }
            }
            Ok(_) => Observation::Unavailable(Unavailable::InvalidResponse),
            Err(error) => Observation::Unavailable(unavailable(error)),
        }
    }
}
#[derive(Deserialize)]
struct TautulliHistory {
    data: Vec<TautulliHistoryRow>,
}
#[derive(Deserialize)]
struct TautulliHistoryRow {
    rating_key: Option<String>,
    date: Option<u64>,
    transcode_decision: Option<String>,
}
#[derive(Deserialize)]
struct TautulliResponse {
    response: TautulliEnvelope,
}
#[derive(Deserialize)]
struct TautulliEnvelope {
    result: String,
    data: serde_json::Value,
}
fn count(value: &serde_json::Value) -> Option<u32> {
    match value {
        serde_json::Value::Number(n) => n.as_u64().and_then(|n| u32::try_from(n).ok()),
        serde_json::Value::String(n) if n.bytes().all(|b| b.is_ascii_digit()) => n.parse().ok(),
        _ => None,
    }
}
