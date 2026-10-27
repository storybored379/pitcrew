use pitcrew_domain::Constraints;
use serde_json::json;

#[test]
fn no_profile5_constraint_round_trips_without_changing_legacy_commands() {
    let legacy = json!({"language":null,"require_sdr":false});
    let old: Constraints = serde_json::from_value(legacy.clone()).unwrap();
    assert_eq!(serde_json::to_value(old).unwrap(), legacy);
    let guarded = json!({"language":null,"require_sdr":false,"forbid_dolby_vision_profile5":true});
    let new: Constraints = serde_json::from_value(guarded.clone()).unwrap();
    assert_eq!(serde_json::to_value(new).unwrap(), guarded);
}

#[test]
fn probe_proof_rejects_profile5_and_unknown_streams() {
    use pitcrew_domain::media::{MediaFault, MediaProbe, ProbeOutcome, StreamFacts};
    let video = |range: Option<&str>| StreamFacts::Video {
        codec: "hevc".into(),
        width: 3840,
        height: 2160,
        bit_depth: Some(10),
        dynamic_range: range.map(str::to_owned),
    };
    let mut probe = MediaProbe {
        outcome: ProbeOutcome::Passed,
        duration_ms: Some(1000),
        elapsed_ms: 1,
        timeout_ms: 1000,
        faults: vec![],
        streams: vec![],
        tested_ranges: vec![],
    };
    assert_eq!(probe.excludes_dolby_vision_profile5(), None);
    for range in ["SDR", "HDR10", "HLG", "DOVI_PROFILE_7", "DOVI_PROFILE_8"] {
        probe.streams = vec![video(Some(range))];
        assert_eq!(probe.excludes_dolby_vision_profile5(), Some(true));
    }
    for range in [None, Some("DOVI"), Some("DOVI_PROFILE_0"), Some("HDR10+")] {
        probe.streams = vec![video(Some("HDR10")), video(range)];
        assert_eq!(probe.excludes_dolby_vision_profile5(), None);
    }
    probe.streams = vec![video(Some("DOVI_PROFILE_5")), video(Some("HDR10"))];
    assert_eq!(probe.excludes_dolby_vision_profile5(), Some(false));
    probe.streams.reverse();
    assert_eq!(probe.excludes_dolby_vision_profile5(), Some(false));
    probe.streams = vec![
        video(Some("HDR10")),
        StreamFacts::Audio {
            codec: "aac".into(),
            channels: 2,
            language: None,
        },
    ];
    assert_eq!(probe.excludes_dolby_vision_profile5(), Some(true));
    for outcome in [
        ProbeOutcome::Failed,
        ProbeOutcome::TimedOut,
        ProbeOutcome::PermissionDenied,
        ProbeOutcome::Unavailable,
    ] {
        probe.outcome = outcome;
        assert_eq!(probe.excludes_dolby_vision_profile5(), None);
    }
    probe.outcome = ProbeOutcome::Passed;
    probe.faults = vec![MediaFault::InvalidHeader];
    assert_eq!(probe.excludes_dolby_vision_profile5(), None);
}
