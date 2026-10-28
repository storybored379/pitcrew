use pitcrew_agent_core::{
    http::{ApiError, Inspector, Operation},
    media::{MediaAgent, MediaConfig},
    plex_logs::{PlexLogConfig, PlexLogReader},
    runtime::*,
};
use pitcrew_domain::{FileIdentity, media::*};
use serde_json::json;
use std::{path::PathBuf, time::Duration};
fn evidence() -> MediaEvidence {
    MediaEvidence {
        file: FileIdentity { id: 1, revision: 2 },
        observed_at_unix_ms: 3,
        symlinks: Some(SymlinkEvidence {
            chain: vec![],
            resolution: SymlinkResolution::Resolved("/media/example".into()),
            elapsed_ms: 1,
            timeout_ms: 5000,
        }),
        mount: None,
        reads: vec![],
        header: Some(probe()),
        decode: Some(MediaProbe {
            tested_ranges: vec![DecodeRange {
                start_ms: 0,
                end_ms: Some(1000),
            }],
            ..probe()
        }),
        plex_failures: vec![],
    }
}
fn probe() -> MediaProbe {
    MediaProbe {
        outcome: ProbeOutcome::Passed,
        duration_ms: Some(1234),
        elapsed_ms: 1,
        timeout_ms: 180000,
        faults: vec![],
        streams: vec![
            StreamFacts::Video {
                codec: "h264".into(),
                width: 64,
                height: 64,
                bit_depth: None,
                dynamic_range: None,
            },
            StreamFacts::Audio {
                codec: "aac".into(),
                channels: 2,
                language: None,
            },
        ],
        tested_ranges: vec![],
    }
}
#[test]
fn probe_compatibility_preserves_unknown_and_failure() {
    let mut e = evidence();
    let p = probe_response(&e);
    assert_eq!(p["opened"], true);
    assert_eq!(p["video_streams"], 1);
    assert_eq!(p["duration_s"], 1.234);
    assert_eq!(p["evidence"], serde_json::to_value(&e).unwrap());
    for outcome in [
        ProbeOutcome::Unavailable,
        ProbeOutcome::PermissionDenied,
        ProbeOutcome::TimedOut,
    ] {
        e.header.as_mut().unwrap().outcome = outcome;
        assert!(probe_response(&e)["opened"].is_null());
    }
    e.header.as_mut().unwrap().outcome = ProbeOutcome::Failed;
    assert_eq!(probe_response(&e)["opened"], false);
    e.header = None;
    let p = probe_response(&e);
    assert_eq!(p["duration_s"], 0.0);
    assert_eq!(p["video_streams"], 0);
    assert!(p["opened"].is_null());
}
#[test]
fn decode_requires_known_identity_header_duration_and_actual_tested_range() {
    let mut e = evidence();
    let p = decode_response(&e);
    assert_eq!(p["verdict"], "ok");
    assert_eq!(p["duration_s"], 1.234);
    assert_eq!(p["samples"], json!([{"start_ms":0,"end_ms":1000}]));
    assert_eq!(p["evidence"], serde_json::to_value(&e).unwrap());
    for field in 0..8 {
        e = evidence();
        match field {
            0 => e.file.id = 0,
            1 => e.header = None,
            2 => e.decode = None,
            3 => e.header.as_mut().unwrap().duration_ms = None,
            4 => e.header.as_mut().unwrap().duration_ms = Some(0),
            5 => e.decode.as_mut().unwrap().tested_ranges.clear(),
            6 => e.header.as_mut().unwrap().outcome = ProbeOutcome::TimedOut,
            _ => e.decode.as_mut().unwrap().outcome = ProbeOutcome::Unavailable,
        };
        assert_eq!(decode_response(&e)["verdict"], "unknown", "case {field}");
    }
    for field in 0..4 {
        e = evidence();
        match field {
            0 => e
                .header
                .as_mut()
                .unwrap()
                .faults
                .push(MediaFault::NoDuration),
            1 => e
                .decode
                .as_mut()
                .unwrap()
                .faults
                .push(MediaFault::DecodeError),
            2 => e.header.as_mut().unwrap().outcome = ProbeOutcome::Failed,
            _ => e.decode.as_mut().unwrap().outcome = ProbeOutcome::Failed,
        };
        assert_eq!(decode_response(&e)["verdict"], "bad");
    }
}
// Shadow 2026-10-27: cold reads on streaming storage often take over a second
// to the first byte, which marked healthy, fully decodable files "bad". Slow
// storage is evidence for a buffering report, not a defect in the file.
#[test]
fn slow_reads_stay_evidence_but_never_make_a_file_bad() {
    for slow in 0..3 {
        let mut e = evidence();
        if slow != 1 {
            e.header.as_mut().unwrap().faults.push(MediaFault::SlowRead);
        }
        if slow != 0 {
            e.decode.as_mut().unwrap().faults.push(MediaFault::SlowRead);
        }
        let response = decode_response(&e);
        assert_eq!(response["verdict"], "ok", "case {slow}");
        assert!(response["evidence"].to_string().contains("SlowRead"));
    }
    let mut e = evidence();
    e.header.as_mut().unwrap().faults = vec![MediaFault::SlowRead, MediaFault::NoDuration];
    assert_eq!(decode_response(&e)["verdict"], "bad");
}
// Shadow 2026-10-23: a file whose first read failed with an I/O error on a
// healthy mount was reported "unknown"; the old service correctly said "bad".
#[test]
fn hard_read_error_on_a_live_mount_is_bad_but_timeouts_stay_unknown() {
    let read = |outcome| TimedRead {
        offset_bytes: 0,
        requested_bytes: 1_048_576,
        read_bytes: 0,
        first_byte_ms: None,
        elapsed_ms: 4_950,
        timeout_ms: 15_000,
        bytes_per_second: 0,
        outcome,
    };
    let mount = |outcome| MountEvidence {
        mount: "configured-storage".into(),
        elapsed_ms: 0,
        timeout_ms: 5_000,
        outcome,
    };
    let mut e = evidence();
    e.header = None;
    e.decode = None;
    e.mount = Some(mount(ProbeOutcome::Passed));
    e.reads = vec![read(ProbeOutcome::Failed)];
    assert_eq!(decode_response(&e)["verdict"], "bad");
    for (m, r) in [
        (ProbeOutcome::Passed, ProbeOutcome::TimedOut),
        (ProbeOutcome::Passed, ProbeOutcome::PermissionDenied),
        (ProbeOutcome::TimedOut, ProbeOutcome::Failed),
        (ProbeOutcome::Unavailable, ProbeOutcome::Failed),
    ] {
        e.mount = Some(mount(m));
        e.reads = vec![read(r)];
        assert_eq!(decode_response(&e)["verdict"], "unknown", "{m:?} {r:?}");
    }
    e.mount = None;
    e.reads = vec![read(ProbeOutcome::Failed)];
    assert_eq!(decode_response(&e)["verdict"], "unknown");
}
#[test]
fn file_compatibility_does_not_invent_source_metrics() {
    let mut e = evidence();
    assert_eq!(file_response(&e)["target_exists"], true);
    assert_eq!(file_response(&e)["exists"], true);
    assert_eq!(file_response(&e)["symlink"], false);
    assert!(file_response(&e)["read_sessions"].is_null());
    e.symlinks.as_mut().unwrap().chain.push(SymlinkHop {
        link: "/media/link".into(),
        target: "missing".into(),
    });
    e.symlinks.as_mut().unwrap().resolution = SymlinkResolution::Dangling("/media/missing".into());
    let p = file_response(&e);
    assert_eq!(p["symlink"], true);
    assert_eq!(p["exists"], true);
    assert_eq!(p["target_exists"], false);
    e.symlinks.as_mut().unwrap().chain.clear();
    assert_eq!(file_response(&e)["exists"], false);
    e.symlinks.as_mut().unwrap().resolution = SymlinkResolution::TimedOut;
    assert!(file_response(&e)["target_exists"].is_null());
    e.symlinks = None;
    assert!(file_response(&e)["target_exists"].is_null());
    assert_eq!(file_response(&e)["symlink"], false);
}
fn inspector(root: PathBuf, logs: Option<PlexLogReader>) -> LocalInspector {
    LocalInspector::new(
        MediaAgent::new(MediaConfig {
            allowed_roots: vec![root],
            ..Default::default()
        })
        .unwrap(),
        logs,
        Duration::from_secs(1),
    )
}
#[tokio::test]
async fn real_local_evidence_and_compatibility_paths() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = inspector(temp.path().into(), None);
    let path = temp.path().join("random.bin");
    std::fs::write(&path, b"synthetic garbage header").unwrap();
    let path = path.to_str().unwrap().to_owned();
    for op in [
        Operation::Probe { path: path.clone() },
        Operation::Decode { path: path.clone() },
        Operation::File { path: path.clone() },
        Operation::Evidence {
            path: path.clone(),
            decode: true,
        },
    ] {
        let v = runtime.inspect(op.clone()).await.unwrap();
        match op {
            Operation::Probe { .. } => assert_eq!(v["opened"], false),
            Operation::Decode { .. } => assert_eq!(v["verdict"], "bad"),
            Operation::File { .. } => assert_eq!(v["target_exists"], true),
            _ => assert_eq!(v["header"]["outcome"], "Failed"),
        }
    }
    assert_eq!(
        runtime
            .inspect(Operation::Probe {
                path: "/outside".into()
            })
            .await,
        Err(ApiError::Forbidden)
    );
    assert_eq!(
        runtime
            .inspect(Operation::PlexFailures {
                minutes: 60,
                compatibility: false
            })
            .await,
        Err(ApiError::Unavailable)
    );
}
#[tokio::test]
async fn log_api_distinguishes_missing_logs_and_returns_both_formats() {
    let temp = tempfile::tempdir().unwrap();
    let logs = || {
        PlexLogReader::new(PlexLogConfig {
            directory: temp.path().into(),
            ..Default::default()
        })
        .unwrap()
    };
    let runtime = inspector(temp.path().into(), Some(logs()));
    assert_eq!(
        runtime
            .inspect(Operation::PlexFailures {
                minutes: 60,
                compatibility: false
            })
            .await,
        Err(ApiError::Unavailable)
    );
    let date = chrono::Utc::now().format("%b %d, %Y %H:%M:%S");
    let line = format!(
        "{date} DEBUG - [Req#1] Request: GET /video?session=alpha&path=%2Flibrary%2Fmetadata%2F42 Signed-in Token (Synthetic Viewer)\n{date} ERROR - [Req#1/Transcode/alpha/abc] Error opening input file /media/sample.mkv.\n"
    );
    std::fs::write(temp.path().join("Plex Media Server.log"), line).unwrap();
    let v = runtime
        .inspect(Operation::PlexFailures {
            minutes: 60,
            compatibility: false,
        })
        .await
        .unwrap();
    assert_eq!(v["files_read"], 1);
    assert_eq!(v["failures"].as_array().unwrap().len(), 1);
    let v = runtime
        .inspect(Operation::PlexFailures {
            minutes: 1,
            compatibility: true,
        })
        .await
        .unwrap();
    assert_eq!(v["count"], 1);
    let f = &v["fails"][0];
    assert_eq!(f["user"], "Synthetic Viewer");
    assert_eq!(f["rating_key"], "42");
    assert_eq!(f["minutes_ago"], 0);
    assert_eq!(f["timestamp_ambiguous"], false);
    assert_eq!(f["session"], "alpha");
    assert_eq!(f["path"], "/media/sample.mkv");
    assert_eq!(v["truncated"], false);
    assert_eq!(v["rejected_timestamps"], 0);
}

#[test]
fn legacy_log_times_omit_dst_ambiguity_and_preserve_explicit_loss_counts() {
    use pitcrew_agent_core::plex_logs::{CorrelatedFailure, PlexLogSnapshot};
    let failure = CorrelatedFailure {
        session: "a".into(),
        user: None,
        rating_key: None,
        path: None,
        errors: vec!["decoder".into()],
        earliest_unix_ms: 60000,
        latest_unix_ms: 60000,
        timezone: "UTC".into(),
        kind: PlexFailureKind::Decoder,
    };
    let snapshot = PlexLogSnapshot {
        failures: vec![
            failure.clone(),
            CorrelatedFailure {
                session: "first-fold".into(),
                earliest_unix_ms: 0,
                ..failure.clone()
            },
            CorrelatedFailure {
                session: "second-fold".into(),
                earliest_unix_ms: 59_000,
                ..failure
            },
        ],
        truncated: true,
        rejected_timestamps: 7,
        files_read: 2,
    };
    let precise = log_response(snapshot.clone(), 180000, false).unwrap();
    assert_eq!(precise, serde_json::to_value(&snapshot).unwrap());
    let legacy = log_response(snapshot, 180000, true).unwrap();
    assert_eq!(legacy["count"], 1);
    assert_eq!(legacy["omitted_ambiguous"], 2);
    assert_eq!(legacy["fails"][0]["session"], "a");
    assert_eq!(legacy["rejected_timestamps"], 7);
    assert_eq!(legacy["truncated"], true);
    assert_eq!(legacy["fails"][0]["minutes_ago"], 2);
    assert_eq!(legacy["fails"][0]["errors"], json!(["decoder"]));
}

#[tokio::test]
async fn log_time_window_is_minutes_and_excludes_old_observations() {
    let temp = tempfile::tempdir().unwrap();
    let now = chrono::Utc::now();
    let mut fixture = String::new();
    for (minutes_ago, session, request, rating) in [(4, "recent", 1, 41), (8, "older", 2, 42)] {
        let date = (now - chrono::Duration::minutes(minutes_ago)).format("%b %d, %Y %H:%M:%S");
        fixture.push_str(&format!(
            "{date} DEBUG - [Req#{request}] Request: GET /video?session={session}&path=%2Flibrary%2Fmetadata%2F{rating} Signed-in Token (Synthetic Viewer)\n{date} ERROR - [Req#{request}/Transcode/{session}/abc] Error opening input file /media/sample.mkv.\n"
        ));
    }
    std::fs::write(temp.path().join("Plex Media Server.log"), fixture).unwrap();
    let reader = PlexLogReader::new(PlexLogConfig {
        directory: temp.path().into(),
        ..Default::default()
    })
    .unwrap();
    let runtime = inspector(temp.path().into(), Some(reader));
    for (minutes, expected) in [(1, vec![]), (5, vec!["41"]), (10, vec!["41", "42"])] {
        let value = runtime
            .inspect(Operation::PlexFailures {
                minutes,
                compatibility: false,
            })
            .await
            .unwrap();
        let mut ratings: Vec<_> = value["failures"]
            .as_array()
            .unwrap()
            .iter()
            .map(|failure| failure["rating_key"].as_str().unwrap())
            .collect();
        ratings.sort_unstable();
        assert_eq!(ratings, expected, "{minutes}-minute window");
    }
}
