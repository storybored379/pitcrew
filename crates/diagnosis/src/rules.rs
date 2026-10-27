//! Ordered port of the reference playcheck and audio-language decisions.
//! Every result is a recommendation: replacement candidates, authorization,
//! budgets and verified import remain the engine's responsibility.
use crate::facts::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosisKind {
    Broken,
    StaleAnalysis,
    DolbyVisionTranscode,
    SlowSource,
    WeakLink4k,
    Client,
    AudioLanguage,
    ReportedProblem,
    Outage,
    Subtitles,
    PlayOrigin,
    SelfResolved,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecommendedAction {
    Replace,
    Analyze,
    AdviseHd,
    ClientAdvice,
    SelectAudioTrack,
    NotifyAdmin,
    NotifyUrgent,
    ExplainPlayOrigin,
    Close,
    CollectEvidence,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplacementConstraint {
    AnyWorking,
    Sdr,
    HdrOkNoProfile5,
    EnglishAudio,
    SpanishAudio,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    FileWillNotOpen,
    NoVideoStream,
    DecodeFailed,
    StalePlexAnalysis,
    DolbyVisionTranscoding,
    SlowSourceBelowBitrate,
    FourKTranscoding,
    FourKStallingOverWan,
    LowerQualitySetting,
    RestartAppAndRouter,
    RequestedAudioMissing,
    RequestedAudioPresent,
    UnverifiedViewerReport,
    WholeShowNeedsSpecificEpisode,
    EverythingBroken,
    ServerHealthy,
    SubtitleRequest,
    OtherPlayOrigin,
    SelfResolved,
    InsufficientEvidence,
    ProviderAssessment,
    UntrustedInstructions,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub kind: DiagnosisKind,
    pub action: RecommendedAction,
    pub constraint: Option<ReplacementConstraint>,
    pub reason: Reason,
}

fn decision(kind: DiagnosisKind, action: RecommendedAction, reason: Reason) -> Decision {
    Decision {
        kind,
        action,
        constraint: None,
        reason,
    }
}

fn replace(kind: DiagnosisKind, constraint: ReplacementConstraint, reason: Reason) -> Decision {
    Decision {
        kind,
        action: RecommendedAction::Replace,
        constraint: Some(constraint),
        reason,
    }
}

/// Combine arr and Plex dynamic-range facts with the reference's precedence.
/// Plex profile 8 proves an HDR fallback even when arr labels the copy profile 5.
/// A Plex DV stream's compatibility bit takes precedence over arr's fallback claim.
pub fn combine_dynamic_range(
    arr: DynamicRange,
    plex: DynamicRange,
    plex_base_layer_compatible: Option<bool>,
) -> DynamicRange {
    let dolby_vision = arr.dolby_vision || plex.dolby_vision;
    let no_hdr_fallback = plex.profile != DvProfile::Eight
        && (plex.profile == DvProfile::Five
            || (plex.dolby_vision && plex_base_layer_compatible == Some(false))
            || (arr.no_hdr_fallback && !plex.dolby_vision));
    DynamicRange {
        dolby_vision,
        hdr: arr.hdr || plex.hdr,
        no_hdr_fallback: dolby_vision && no_hdr_fallback,
        profile: plex.profile,
    }
}

/// Classify a normalized report. A vague whole-show complaint never requests
/// replacement of a guessed episode. Resolve the exact episode before re-running.
pub fn classify(evidence: &EvidenceFacts) -> Decision {
    use Complaint::*;
    use DiagnosisKind as K;
    use Reason as R;
    use RecommendedAction as A;
    if evidence.injection_suspected {
        return decision(K::Unknown, A::NotifyAdmin, R::UntrustedInstructions);
    }
    match evidence.complaint {
        EverythingBroken => match evidence.server_health {
            ServerHealth::Unavailable => decision(K::Outage, A::NotifyUrgent, R::EverythingBroken),
            ServerHealth::Unknown => decision(K::Unknown, A::NotifyAdmin, R::InsufficientEvidence),
            ServerHealth::Healthy if evidence.scope == Scope::Show => decision(
                K::Unknown,
                A::CollectEvidence,
                R::WholeShowNeedsSpecificEpisode,
            ),
            ServerHealth::Healthy => {
                let observed = classify_playback(&evidence.playback);
                if observed.kind == K::Unknown {
                    decision(K::Client, A::ClientAdvice, R::ServerHealthy)
                } else {
                    observed
                }
            }
        },
        SelfResolved => decision(K::SelfResolved, A::Close, R::SelfResolved),
        AdsOrWrongVersion => decision(K::PlayOrigin, A::ExplainPlayOrigin, R::OtherPlayOrigin),
        Subtitles => decision(K::Subtitles, A::NotifyAdmin, R::SubtitleRequest),
        Other => decision(K::Unknown, A::NotifyAdmin, R::InsufficientEvidence),
        _ if evidence.scope == Scope::Show => decision(
            K::Unknown,
            A::CollectEvidence,
            R::WholeShowNeedsSpecificEpisode,
        ),
        WrongAudioLanguage => match (evidence.audio.requested, evidence.audio.requested_track) {
            (Language::English, TrackPresence::Absent) => replace(
                K::AudioLanguage,
                ReplacementConstraint::EnglishAudio,
                R::RequestedAudioMissing,
            ),
            (Language::Spanish, TrackPresence::Absent) => replace(
                K::AudioLanguage,
                ReplacementConstraint::SpanishAudio,
                R::RequestedAudioMissing,
            ),
            (Language::English | Language::Spanish, TrackPresence::Present) => decision(
                K::AudioLanguage,
                A::SelectAudioTrack,
                R::RequestedAudioPresent,
            ),
            _ => decision(K::AudioLanguage, A::NotifyAdmin, R::InsufficientEvidence),
        },
        Playback => classify_playback(&evidence.playback),
        VideoBroken | NoAudio | WrongEpisode => {
            // The reference triage terminal is 'autofix'. A viewer report alone is
            // not positive broken-file evidence for the Rust execution boundary.
            let observed = classify_playback(&evidence.playback);
            if observed.kind == K::Unknown {
                decision(
                    K::ReportedProblem,
                    A::CollectEvidence,
                    R::UnverifiedViewerReport,
                )
            } else {
                observed
            }
        }
    }
}

/// First matching rule wins. Missing telemetry remains unknown; it is never a
/// probe failure. Rule ordering and thresholds remain stable across collectors.
pub fn classify_playback(ev: &PlaybackFacts) -> Decision {
    use DiagnosisKind as K;
    use Reason as R;
    use RecommendedAction as A;
    use ReplacementConstraint as C;
    match ev.probe {
        ProbeOutcome::OpenFailed => return replace(K::Broken, C::AnyWorking, R::FileWillNotOpen),
        ProbeOutcome::NoVideo => return replace(K::Broken, C::AnyWorking, R::NoVideoStream),
        _ => {}
    }
    if ev.decode == DecodeOutcome::Bad {
        return replace(K::Broken, C::AnyWorking, R::DecodeFailed);
    }
    if ev.plex_looks_broken && ev.probe == ProbeOutcome::Video && !ev.analyzed_recently {
        return decision(K::StaleAnalysis, A::Analyze, R::StalePlexAnalysis);
    }
    let session = ev.sessions;
    let transcodes = session.transcodes != 0;
    let device_ok = session.other_items == 0 || session.other_direct_plays != 0;
    if ev.dynamic_range.dolby_vision && transcodes && device_ok {
        if !ev.is_4k {
            return replace(K::DolbyVisionTranscode, C::Sdr, R::DolbyVisionTranscoding);
        }
        if ev.dynamic_range.no_hdr_fallback {
            return replace(
                K::DolbyVisionTranscode,
                C::HdrOkNoProfile5,
                R::DolbyVisionTranscoding,
            );
        }
        if ev.hd_library_available {
            return decision(
                K::DolbyVisionTranscode,
                A::AdviseHd,
                R::DolbyVisionTranscoding,
            );
        }
    }
    if let Some(provider) = ev.provider
        && provider.miss_percent.unwrap_or(0) >= 20
        && let (Some(served), Some(file)) = (provider.served_bps, ev.file_bps)
        && served < file
    {
        return replace(K::SlowSource, C::AnyWorking, R::SlowSourceBelowBitrate);
    }
    if ev.is_4k && ev.hd_library_available {
        if transcodes {
            return decision(K::WeakLink4k, A::AdviseHd, R::FourKTranscoding);
        }
        if session.stalled != 0 && ev.wan {
            return decision(K::WeakLink4k, A::AdviseHd, R::FourKStallingOverWan);
        }
    }
    // Absent source telemetry does not contradict same-device
    // failures. This only yields advice, never a mutation or proof of clean bytes.
    let clean_source = ev.provider.is_none_or(|p| p.miss_percent.unwrap_or(0) < 5);
    if clean_source && session.other_stalled >= 2 {
        let reason = if session.newest_decision == PlaybackDecision::Transcode {
            R::LowerQualitySetting
        } else {
            R::RestartAppAndRouter
        };
        return decision(K::Client, A::ClientAdvice, reason);
    }
    decision(K::Unknown, A::NotifyAdmin, R::InsufficientEvidence)
}
