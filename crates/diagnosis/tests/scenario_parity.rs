//! Synthetic playback-rule and wizard contracts, independent of any external app.
use pitcrew_diagnosis::{facts::*, rules::*};
use serde_json::Value;

fn playback_facts(ev: &Value) -> PlaybackFacts {
    PlaybackFacts {
        is_4k: ev["is_4k"].as_bool().unwrap(),
        probe: match (
            ev["probe"]["opened"].as_bool(),
            ev["probe"]["video_streams"].as_u64(),
        ) {
            (Some(false), _) => ProbeOutcome::OpenFailed,
            (Some(true), Some(0)) => ProbeOutcome::NoVideo,
            (Some(true), Some(_)) => ProbeOutcome::Video,
            _ => ProbeOutcome::Unknown,
        },
        sessions: SessionFacts {
            transcodes: ev["sess"]["transcodes"].as_u64().unwrap() as u32,
            stalled: ev["sess"]["stalled_sessions"].as_u64().unwrap() as u32,
            ..Default::default()
        },
        dynamic_range: DynamicRange {
            dolby_vision: ev["dr"]["dv"].as_bool().unwrap(),
            hdr: ev["dr"]["hdr"].as_bool().unwrap(),
            no_hdr_fallback: ev["dr"]["p5"].as_bool().unwrap(),
            profile: match ev["dr"]["profile"].as_u64() {
                Some(5) => DvProfile::Five,
                Some(8) => DvProfile::Eight,
                Some(_) => DvProfile::Other,
                None => DvProfile::Unknown,
            },
        },
        file_bps: ev["file_mbps"].as_u64().map(|n| n * 1_000_000),
        wan: ev["wan"].as_bool().unwrap(),
        hd_library_available: ev["hd_library"].is_string(),
        ..Default::default()
    }
}

#[test]
fn synthetic_playback_constraints_match() {
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/playback-constraints.json")).unwrap();
    assert_eq!(cases.len(), 10);
    for case in cases {
        let result = classify_playback(&playback_facts(&case["evidence"]));
        let constraint = match result.constraint {
            Some(ReplacementConstraint::HdrOkNoProfile5) => Value::String("hdr_ok_no_p5".into()),
            Some(ReplacementConstraint::Sdr) => Value::String("sdr".into()),
            None => Value::Null,
            other => panic!("unexpected fixture constraint {other:?}"),
        };
        assert_eq!(constraint, case["constraint"], "{}", case["id"]);
    }
}

#[test]
fn committed_wizard_inputs_route_through_same_playcheck_terminal() {
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/wizard-playcheck.json")).unwrap();
    assert_eq!(cases.len(), 2);
    for case in cases {
        let answers: Vec<_> = case["answers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            pitcrew_diagnosis::triage::validate_path(
                case["issue_type"].as_str().unwrap(),
                "show",
                "episode",
                &answers,
                false
            ),
            Ok(pitcrew_diagnosis::triage::TriageAction::Playcheck)
        );
    }
}
