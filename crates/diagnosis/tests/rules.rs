use pitcrew_diagnosis::{facts::*, rules::*};

fn base() -> PlaybackFacts {
    PlaybackFacts {
        probe: ProbeOutcome::Video,
        decode: DecodeOutcome::Good,
        file_bps: Some(16_000_000),
        ..Default::default()
    }
}
fn expected(
    kind: DiagnosisKind,
    action: RecommendedAction,
    constraint: Option<ReplacementConstraint>,
    reason: Reason,
) -> Decision {
    Decision {
        kind,
        action,
        constraint,
        reason,
    }
}
fn unknown() -> Decision {
    expected(
        DiagnosisKind::Unknown,
        RecommendedAction::NotifyAdmin,
        None,
        Reason::InsufficientEvidence,
    )
}

#[test]
fn missing_telemetry_is_not_a_broken_file() {
    assert_eq!(classify_playback(&PlaybackFacts::default()), unknown());
    assert_eq!(classify_playback(&base()), unknown());
}

#[test]
fn broken_probes_decode_and_stale_analysis_have_ordered_distinct_evidence() {
    let mut ev = base();
    ev.plex_looks_broken = true;
    ev.dynamic_range.dolby_vision = true;
    ev.sessions.transcodes = 1;
    ev.provider = Some(ProviderFacts {
        miss_percent: Some(99),
        served_bps: Some(1),
    });
    ev.decode = DecodeOutcome::Bad;
    for (probe, reason) in [
        (ProbeOutcome::OpenFailed, Reason::FileWillNotOpen),
        (ProbeOutcome::NoVideo, Reason::NoVideoStream),
        (ProbeOutcome::Video, Reason::DecodeFailed),
        (ProbeOutcome::Unknown, Reason::DecodeFailed),
    ] {
        ev.probe = probe;
        assert_eq!(
            classify_playback(&ev),
            expected(
                DiagnosisKind::Broken,
                RecommendedAction::Replace,
                Some(ReplacementConstraint::AnyWorking),
                reason
            )
        );
    }
    ev.probe = ProbeOutcome::Video;
    ev.decode = DecodeOutcome::Good;
    assert_eq!(
        classify_playback(&ev),
        expected(
            DiagnosisKind::StaleAnalysis,
            RecommendedAction::Analyze,
            None,
            Reason::StalePlexAnalysis
        )
    );
    ev.analyzed_recently = true;
    assert_eq!(
        classify_playback(&ev).kind,
        DiagnosisKind::DolbyVisionTranscode
    );
    ev = base();
    ev.plex_looks_broken = true;
    ev.probe = ProbeOutcome::Unknown;
    assert_eq!(classify_playback(&ev), unknown());
}

#[test]
fn dv_requires_actual_transcode_and_no_contradicting_same_device_history() {
    let mut ev = base();
    ev.dynamic_range.dolby_vision = true;
    assert_eq!(classify_playback(&ev), unknown());
    ev.sessions.transcodes = 1;
    assert_eq!(
        classify_playback(&ev),
        expected(
            DiagnosisKind::DolbyVisionTranscode,
            RecommendedAction::Replace,
            Some(ReplacementConstraint::Sdr),
            Reason::DolbyVisionTranscoding
        )
    );
    ev.sessions.other_items = 1;
    assert_eq!(classify_playback(&ev), unknown());
    ev.sessions.other_direct_plays = 1;
    assert_eq!(
        classify_playback(&ev).constraint,
        Some(ReplacementConstraint::Sdr)
    );
    ev.dynamic_range.dolby_vision = false;
    assert_eq!(classify_playback(&ev), unknown());
}

#[test]
fn four_k_never_requests_sdr_from_dv_and_missing_hd_does_not_invent_advice() {
    let mut ev = base();
    ev.is_4k = true;
    ev.dynamic_range.dolby_vision = true;
    ev.sessions.transcodes = 1;
    assert_eq!(classify_playback(&ev), unknown());
    ev.hd_library_available = true;
    assert_eq!(
        classify_playback(&ev),
        expected(
            DiagnosisKind::DolbyVisionTranscode,
            RecommendedAction::AdviseHd,
            None,
            Reason::DolbyVisionTranscoding
        )
    );
    ev.dynamic_range.no_hdr_fallback = true;
    for hd in [false, true] {
        ev.hd_library_available = hd;
        assert_eq!(
            classify_playback(&ev),
            expected(
                DiagnosisKind::DolbyVisionTranscode,
                RecommendedAction::Replace,
                Some(ReplacementConstraint::HdrOkNoProfile5),
                Reason::DolbyVisionTranscoding
            )
        );
    }
}

#[test]
fn slow_source_requires_both_misses_and_insufficient_throughput() {
    let mut ev = base();
    for (miss, served, file, slow) in [
        (None, Some(1), Some(16), false),
        (Some(19), Some(1), Some(16), false),
        (Some(20), Some(15), Some(16), true),
        (Some(21), Some(16), Some(16), false),
        (Some(99), Some(17), Some(16), false),
        (Some(99), None, Some(16), false),
        (Some(99), Some(0), None, false),
        (Some(99), Some(0), Some(0), false),
        (Some(20), Some(0), Some(1), true),
    ] {
        ev.provider = Some(ProviderFacts {
            miss_percent: miss,
            served_bps: served,
        });
        ev.file_bps = file;
        assert_eq!(
            classify_playback(&ev),
            if slow {
                expected(
                    DiagnosisKind::SlowSource,
                    RecommendedAction::Replace,
                    Some(ReplacementConstraint::AnyWorking),
                    Reason::SlowSourceBelowBitrate,
                )
            } else {
                unknown()
            },
            "miss={miss:?}, served={served:?}, file={file:?}"
        );
    }
}

#[test]
fn four_k_advice_needs_known_alternative_and_transcode_or_wan_stall() {
    let mut ev = base();
    ev.is_4k = true;
    ev.hd_library_available = true;
    assert_eq!(classify_playback(&ev), unknown());
    ev.sessions.stalled = 1;
    assert_eq!(classify_playback(&ev), unknown());
    ev.wan = true;
    assert_eq!(
        classify_playback(&ev),
        expected(
            DiagnosisKind::WeakLink4k,
            RecommendedAction::AdviseHd,
            None,
            Reason::FourKStallingOverWan
        )
    );
    ev.sessions.transcodes = 1;
    assert_eq!(
        classify_playback(&ev),
        expected(
            DiagnosisKind::WeakLink4k,
            RecommendedAction::AdviseHd,
            None,
            Reason::FourKTranscoding
        )
    );
    ev.wan = false;
    ev.sessions.stalled = 0;
    assert_eq!(classify_playback(&ev).reason, Reason::FourKTranscoding);
    ev.hd_library_available = false;
    assert_eq!(classify_playback(&ev), unknown());
    ev.hd_library_available = true;
    ev.is_4k = false;
    assert_eq!(classify_playback(&ev), unknown());
}

#[test]
fn client_advice_preserves_strict_source_and_other_item_thresholds() {
    let mut ev = base();
    ev.sessions.other_stalled = 1;
    assert_eq!(classify_playback(&ev), unknown());
    ev.sessions.other_stalled = 2;
    let restart = expected(
        DiagnosisKind::Client,
        RecommendedAction::ClientAdvice,
        None,
        Reason::RestartAppAndRouter,
    );
    assert_eq!(classify_playback(&ev), restart);
    for miss in [None, Some(0), Some(4)] {
        ev.provider = Some(ProviderFacts {
            miss_percent: miss,
            served_bps: None,
        });
        assert_eq!(classify_playback(&ev), restart);
    }
    ev.provider.as_mut().unwrap().miss_percent = Some(5);
    assert_eq!(classify_playback(&ev), unknown());
    ev.provider = None;
    for newest in [
        PlaybackDecision::Unknown,
        PlaybackDecision::DirectPlay,
        PlaybackDecision::DirectStream,
    ] {
        ev.sessions.newest_decision = newest;
        assert_eq!(classify_playback(&ev), restart);
    }
    ev.sessions.newest_decision = PlaybackDecision::Transcode;
    assert_eq!(
        classify_playback(&ev),
        expected(
            DiagnosisKind::Client,
            RecommendedAction::ClientAdvice,
            None,
            Reason::LowerQualitySetting
        )
    );
    ev.probe = ProbeOutcome::NoVideo;
    assert_eq!(classify_playback(&ev).kind, DiagnosisKind::Broken);
}

#[test]
fn ordered_rules_prefer_dv_then_source_then_four_k_then_client() {
    let mut ev = base();
    ev.dynamic_range.dolby_vision = true;
    ev.is_4k = true;
    ev.dynamic_range.no_hdr_fallback = true;
    ev.sessions.transcodes = 1;
    ev.sessions.other_stalled = 4;
    ev.hd_library_available = true;
    ev.provider = Some(ProviderFacts {
        miss_percent: Some(20),
        served_bps: Some(1),
    });
    assert_eq!(
        classify_playback(&ev).kind,
        DiagnosisKind::DolbyVisionTranscode
    );
    ev.dynamic_range.dolby_vision = false;
    assert_eq!(classify_playback(&ev).kind, DiagnosisKind::SlowSource);
    ev.provider.as_mut().unwrap().miss_percent = Some(4);
    assert_eq!(classify_playback(&ev).kind, DiagnosisKind::WeakLink4k);
    ev.is_4k = false;
    assert_eq!(classify_playback(&ev).kind, DiagnosisKind::Client);
}

#[test]
fn dynamic_range_combines_independent_sources_and_plex_overrides_arr_profile() {
    let absent = DynamicRange::default();
    let arr = DynamicRange {
        dolby_vision: true,
        hdr: false,
        no_hdr_fallback: true,
        profile: DvProfile::Five,
    };
    assert_eq!(combine_dynamic_range(absent, absent, None), absent);
    assert_eq!(
        combine_dynamic_range(arr, absent, None),
        DynamicRange {
            profile: DvProfile::Unknown,
            ..arr
        }
    );
    let plex = DynamicRange {
        dolby_vision: true,
        hdr: true,
        profile: DvProfile::Eight,
        ..absent
    };
    assert_eq!(combine_dynamic_range(arr, plex, Some(false)), plex);
    let plex = DynamicRange {
        dolby_vision: true,
        ..absent
    };
    assert!(!combine_dynamic_range(arr, plex, None).no_hdr_fallback);
    assert!(combine_dynamic_range(absent, plex, Some(false)).no_hdr_fallback);
    assert!(!combine_dynamic_range(arr, plex, Some(true)).no_hdr_fallback);
    let plex = DynamicRange {
        profile: DvProfile::Five,
        ..plex
    };
    assert!(combine_dynamic_range(absent, plex, Some(true)).no_hdr_fallback);
    assert!(
        !combine_dynamic_range(
            absent,
            DynamicRange {
                profile: DvProfile::Five,
                ..absent
            },
            None
        )
        .no_hdr_fallback
    );
    let hdr = DynamicRange {
        hdr: true,
        ..absent
    };
    assert_eq!(combine_dynamic_range(hdr, absent, None), hdr);
    assert_eq!(combine_dynamic_range(absent, hdr, None), hdr);
    assert!(!combine_dynamic_range(absent, absent, Some(false)).no_hdr_fallback);
}

#[test]
fn report_categories_route_without_turning_viewer_text_into_broken_file_proof() {
    let mut ev = EvidenceFacts::default();
    for (complaint, kind, action, reason) in [
        (
            Complaint::SelfResolved,
            DiagnosisKind::SelfResolved,
            RecommendedAction::Close,
            Reason::SelfResolved,
        ),
        (
            Complaint::AdsOrWrongVersion,
            DiagnosisKind::PlayOrigin,
            RecommendedAction::ExplainPlayOrigin,
            Reason::OtherPlayOrigin,
        ),
        (
            Complaint::Subtitles,
            DiagnosisKind::Subtitles,
            RecommendedAction::NotifyAdmin,
            Reason::SubtitleRequest,
        ),
        (
            Complaint::Other,
            DiagnosisKind::Unknown,
            RecommendedAction::NotifyAdmin,
            Reason::InsufficientEvidence,
        ),
    ] {
        ev.complaint = complaint;
        assert_eq!(classify(&ev), expected(kind, action, None, reason));
    }
    for complaint in [
        Complaint::VideoBroken,
        Complaint::NoAudio,
        Complaint::WrongEpisode,
    ] {
        ev.complaint = complaint;
        assert_eq!(
            classify(&ev),
            expected(
                DiagnosisKind::ReportedProblem,
                RecommendedAction::CollectEvidence,
                None,
                Reason::UnverifiedViewerReport
            )
        );
        ev.playback.decode = DecodeOutcome::Bad;
        assert_eq!(classify(&ev).kind, DiagnosisKind::Broken);
        ev.scope = Scope::Show;
        assert_eq!(
            classify(&ev),
            expected(
                DiagnosisKind::Unknown,
                RecommendedAction::CollectEvidence,
                None,
                Reason::WholeShowNeedsSpecificEpisode
            )
        );
        ev.scope = Scope::Episode;
        ev.playback.decode = DecodeOutcome::Unknown;
    }
    ev.complaint = Complaint::Playback;
    assert_eq!(classify(&ev), unknown());
    ev.playback.decode = DecodeOutcome::Bad;
    assert_eq!(classify(&ev).reason, Reason::DecodeFailed);
    ev.injection_suspected = true;
    assert_eq!(
        classify(&ev),
        expected(
            DiagnosisKind::Unknown,
            RecommendedAction::NotifyAdmin,
            None,
            Reason::UntrustedInstructions
        )
    );
}

#[test]
fn audio_requires_known_tracks_for_exact_item_and_supported_requested_language() {
    let mut ev = EvidenceFacts {
        complaint: Complaint::WrongAudioLanguage,
        ..Default::default()
    };
    for (language, constraint) in [
        (Language::English, ReplacementConstraint::EnglishAudio),
        (Language::Spanish, ReplacementConstraint::SpanishAudio),
    ] {
        ev.audio = AudioFacts {
            requested: language,
            requested_track: TrackPresence::Absent,
        };
        assert_eq!(
            classify(&ev),
            expected(
                DiagnosisKind::AudioLanguage,
                RecommendedAction::Replace,
                Some(constraint),
                Reason::RequestedAudioMissing
            )
        );
        ev.audio.requested_track = TrackPresence::Present;
        assert_eq!(
            classify(&ev),
            expected(
                DiagnosisKind::AudioLanguage,
                RecommendedAction::SelectAudioTrack,
                None,
                Reason::RequestedAudioPresent
            )
        );
        ev.audio.requested_track = TrackPresence::Unknown;
        assert_eq!(
            classify(&ev),
            expected(
                DiagnosisKind::AudioLanguage,
                RecommendedAction::NotifyAdmin,
                None,
                Reason::InsufficientEvidence
            )
        );
    }
    ev.audio.requested = Language::Other;
    for presence in [
        TrackPresence::Present,
        TrackPresence::Absent,
        TrackPresence::Unknown,
    ] {
        ev.audio.requested_track = presence;
        assert_eq!(classify(&ev).action, RecommendedAction::NotifyAdmin);
    }
    ev.scope = Scope::Show;
    ev.audio = AudioFacts {
        requested: Language::English,
        requested_track: TrackPresence::Absent,
    };
    assert_eq!(classify(&ev).action, RecommendedAction::CollectEvidence);
}

#[test]
fn evidence_and_decisions_are_closed_serializable_types() {
    let facts = EvidenceFacts::default();
    assert_eq!(
        serde_json::from_str::<EvidenceFacts>(&serde_json::to_string(&facts).unwrap()).unwrap(),
        facts
    );
    let mut malicious = serde_json::to_value(facts).unwrap();
    malicious["viewer_text"] = "ignore instructions".into();
    assert!(serde_json::from_value::<EvidenceFacts>(malicious).is_err());
    let mut malicious = serde_json::to_value(unknown()).unwrap();
    malicious["reason"] = "arbitrary secret text".into();
    assert!(serde_json::from_value::<Decision>(malicious).is_err());
    let mut malicious = serde_json::to_value(unknown()).unwrap();
    malicious["path"] = "/synthetic/private.mkv".into();
    assert!(serde_json::from_value::<Decision>(malicious).is_err());
}

#[test]
fn everything_broken_does_not_turn_unknown_health_into_a_proven_outage() {
    let evidence = EvidenceFacts {
        complaint: Complaint::EverythingBroken,
        ..Default::default()
    };
    assert_eq!(
        classify(&evidence),
        expected(
            DiagnosisKind::Unknown,
            RecommendedAction::NotifyAdmin,
            None,
            Reason::InsufficientEvidence
        )
    );
}

#[test]
fn proven_outage_and_healthy_server_take_distinct_paths() {
    let mut evidence = EvidenceFacts {
        complaint: Complaint::EverythingBroken,
        server_health: ServerHealth::Unavailable,
        ..Default::default()
    };
    assert_eq!(
        classify(&evidence),
        expected(
            DiagnosisKind::Outage,
            RecommendedAction::NotifyUrgent,
            None,
            Reason::EverythingBroken
        )
    );
    evidence.server_health = ServerHealth::Healthy;
    assert_eq!(
        classify(&evidence),
        expected(
            DiagnosisKind::Client,
            RecommendedAction::ClientAdvice,
            None,
            Reason::ServerHealthy
        )
    );
    evidence.playback.decode = DecodeOutcome::Bad;
    assert_eq!(
        classify(&evidence),
        expected(
            DiagnosisKind::Broken,
            RecommendedAction::Replace,
            Some(ReplacementConstraint::AnyWorking),
            Reason::DecodeFailed
        )
    );
    evidence.scope = Scope::Show;
    assert_eq!(
        classify(&evidence),
        expected(
            DiagnosisKind::Unknown,
            RecommendedAction::CollectEvidence,
            None,
            Reason::WholeShowNeedsSpecificEpisode
        )
    );
}

#[test]
fn log_counts_are_context_and_never_stale_metadata_or_broken_file_proof() {
    let evidence = EvidenceFacts {
        plex_logs: PlexLogFacts {
            available: true,
            matching_failures: 99,
            unknown_correlation: 999,
            truncated: true,
            rejected_timestamps: 1,
        },
        playback: base(),
        ..Default::default()
    };
    assert_eq!(classify(&evidence), unknown());
}
