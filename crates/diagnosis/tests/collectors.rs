use pitcrew_diagnosis::collectors::*;
use pitcrew_domain::FileIdentity;
use pitcrew_upstream::{
    Auth, Endpoint, Error, Limits, Method, Request, Response, ResponseFuture, Transport,
};
use serde_json::{Value, json};
use std::{collections::VecDeque, sync::Mutex, time::Duration};

struct Fake {
    responses: Mutex<VecDeque<Result<Response, Error>>>,
    requests: Mutex<Vec<Request>>,
}
impl Fake {
    fn new(values: Vec<Result<Value, Error>>) -> Self {
        Self {
            responses: Mutex::new(
                values
                    .into_iter()
                    .map(|v| {
                        v.map(|body| Response {
                            status: 200,
                            body: serde_json::to_vec(&body).unwrap(),
                        })
                    })
                    .collect(),
            ),
            requests: Mutex::new(vec![]),
        }
    }
}
impl Transport for &Fake {
    fn send(&self, request: Request) -> ResponseFuture<'_> {
        self.requests.lock().unwrap().push(request);
        Box::pin(async {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected request")
        })
    }
}
fn endpoint() -> Endpoint {
    Endpoint::new(
        "https://evidence.invalid/proxy",
        Auth::bearer("synthetic-key").unwrap(),
        Limits::default(),
    )
    .unwrap()
}
fn media(at: u64) -> Value {
    json!({"file":{"id":7,"revision":11},"observed_at_unix_ms":at,
        "symlinks":null,"mount":null,"reads":[],"header":null,
        "decode":{"outcome":"Passed","duration_ms":90000,"elapsed_ms":30,"timeout_ms":30000,
        "faults":[],"streams":[],"tested_ranges":[{"start_ms":0,"end_ms":10000},{"start_ms":40000,"end_ms":50000},{"start_ms":80000,"end_ms":90000}]},"plex_failures":[]})
}
fn logs() -> Value {
    json!({"failures":[],"truncated":false,"rejected_timestamps":0,"files_read":1})
}
fn query() -> AgentQuery<'static> {
    AgentQuery {
        path: "/media/invented.mkv",
        expected_file: Some(FileIdentity {
            id: 7,
            revision: 11,
        }),
        minutes: 1,
    }
}
fn failure(path: Value, earliest: u64, latest: u64) -> Value {
    json!({"session":"private-session","user":"private-viewer","rating_key":"71","errors":["private raw error"],
        "path":path,"earliest_unix_ms":earliest,"latest_unix_ms":latest,"timezone":"UTC","kind":"Decoder"})
}
#[tokio::test]
async fn agent_contract_preserves_decode_ranges_and_redacts_debug() {
    let fake = Fake::new(vec![Ok(media(60000)), Ok(logs())]);
    let result = AgentCollector::new(endpoint(), &fake)
        .collect_with_clock(query(), || Some(120000))
        .await;
    let Observation::Available(ref e) = result.media else {
        panic!("missing media")
    };
    assert_eq!(
        e.file,
        FileIdentity {
            id: 7,
            revision: 11
        }
    );
    assert_eq!(
        e.decode
            .as_ref()
            .unwrap()
            .tested_ranges
            .iter()
            .map(|r| r.start_ms)
            .collect::<Vec<_>>(),
        [0, 40000, 80000]
    );
    assert_eq!(result.plex, Observation::Available(PlexContext::default()));
    let debug = format!("{result:?}");
    assert!(debug.contains("local evidence"));
    assert!(!debug.contains("revision"));
    assert!(!debug.contains("invented"));
    let requests = fake.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for r in requests.iter() {
        assert_eq!(r.method, Method::Get);
        assert!(r.body.is_none());
        assert_eq!(
            r.headers[0],
            ("Authorization".into(), "Bearer synthetic-key".into())
        );
    }
    assert_eq!(requests[0].url.path(), "/proxy/v1/evidence");
    assert_eq!(
        requests[0].url.query_pairs().collect::<Vec<_>>(),
        [
            ("path".into(), "/media/invented.mkv".into()),
            ("decode".into(), "true".into())
        ]
    );
    assert_eq!(requests[1].url.path(), "/proxy/v1/plexfails");
    assert_eq!(requests[1].url.query(), Some("minutes=1"));
}
#[tokio::test]
async fn agent_bounds_identity_freshness_and_independent_failures() {
    for (at, expected) in [
        (59999, Some(Unavailable::Stale)),
        (60000, None),
        (120000, None),
        (120001, Some(Unavailable::Stale)),
    ] {
        let fake = Fake::new(vec![Ok(media(at)), Ok(logs())]);
        let result = AgentCollector::new(endpoint(), &fake)
            .collect_with_clock(query(), || Some(120000))
            .await;
        match expected {
            Some(reason) => assert_eq!(result.media, Observation::Unavailable(reason)),
            None => assert!(matches!(result.media, Observation::Available(_))),
        }
    }
    for expected in [
        None,
        Some(FileIdentity {
            id: 7,
            revision: 11,
        }),
        Some(FileIdentity {
            id: 8,
            revision: 11,
        }),
        Some(FileIdentity {
            id: 7,
            revision: 12,
        }),
    ] {
        let fake = Fake::new(vec![Ok(media(60000)), Ok(logs())]);
        let q = AgentQuery {
            expected_file: expected,
            ..query()
        };
        let result = AgentCollector::new(endpoint(), &fake)
            .collect_with_clock(q, || Some(120000))
            .await;
        if expected.is_none()
            || expected
                == Some(FileIdentity {
                    id: 7,
                    revision: 11,
                })
        {
            assert!(matches!(result.media, Observation::Available(_)));
        } else {
            assert_eq!(
                result.media,
                Observation::Unavailable(Unavailable::IdentityChanged)
            );
        }
    }
    let fake = Fake::new(vec![Ok(media(0)), Ok(logs())]);
    assert!(matches!(
        AgentCollector::new(endpoint(), &fake)
            .collect_with_clock(AgentQuery { ..query() }, || Some(10))
            .await
            .media,
        Observation::Available(_)
    ));
    for (error, reason) in [
        (Error::Timeout, Unavailable::Timeout),
        (Error::Transport, Unavailable::Upstream),
        (Error::Status(401), Unavailable::Unauthorized),
        (Error::Status(403), Unavailable::Unauthorized),
        (Error::Status(500), Unavailable::Upstream),
        (Error::InvalidResponse, Unavailable::InvalidResponse),
        (Error::TooLarge, Unavailable::TooLarge),
        (Error::Configuration("private"), Unavailable::InvalidRequest),
        (Error::ShadowDenied, Unavailable::InvalidRequest),
    ] {
        let fake = Fake::new(vec![Err(error), Ok(logs())]);
        let result = AgentCollector::new(endpoint(), &fake)
            .collect_with_clock(query(), || Some(120000))
            .await;
        assert_eq!(result.media, Observation::Unavailable(reason));
        assert!(matches!(result.plex, Observation::Available(_)));
        let fake = Fake::new(vec![Ok(media(60000)), Err(error)]);
        assert_eq!(
            AgentCollector::new(endpoint(), &fake)
                .collect_with_clock(query(), || Some(120000))
                .await
                .plex,
            Observation::Unavailable(reason)
        );
    }
    for minutes in [0, 1441, u16::MAX] {
        let fake = Fake::new(vec![]);
        let result = AgentCollector::new(endpoint(), &fake)
            .collect_with_clock(AgentQuery { minutes, ..query() }, || Some(120000))
            .await;
        assert_eq!(
            result,
            AgentEvidence {
                media: Observation::Unavailable(Unavailable::InvalidRequest),
                plex: Observation::Unavailable(Unavailable::InvalidRequest)
            }
        );
        assert!(fake.requests.lock().unwrap().is_empty());
    }
    let fake = Fake::new(vec![Ok(media(60000)), Ok(logs())]);
    assert!(matches!(
        AgentCollector::new(endpoint(), &fake)
            .collect_with_clock(
                AgentQuery {
                    minutes: 1440,
                    ..query()
                },
                || Some(120000)
            )
            .await
            .media,
        Observation::Available(_)
    ));
    let fake = Fake::new(vec![]);
    assert_eq!(
        AgentCollector::new(endpoint(), &fake)
            .collect_with_clock(
                AgentQuery {
                    path: "relative",
                    ..query()
                },
                || Some(120000)
            )
            .await
            .media,
        Observation::Unavailable(Unavailable::InvalidRequest)
    );
    assert!(fake.requests.lock().unwrap().is_empty());
    let fake = Fake::new(vec![Err(Error::InvalidInput)]);
    assert_eq!(
        AgentCollector::new(endpoint(), &fake)
            .collect_with_clock(query(), || Some(120000))
            .await
            .plex,
        Observation::Unavailable(Unavailable::InvalidRequest)
    );
    assert_eq!(fake.requests.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn log_correlation_never_fabricates_file_identity_or_dst_time() {
    let mut snapshot = logs();
    snapshot["failures"] = json!([
        failure(json!("/media/invented.mkv"), 60000, 60000),
        failure(json!("/media/invented.mkv"), 120000, 120000),
        failure(json!("/media/other.mkv"), 70000, 70000),
        failure(Value::Null, 70000, 70000),
        failure(json!("/media/invented.mkv"), 70000, 70001),
        failure(json!("/media/invented.mkv"), 59999, 59999),
        failure(json!("/media/invented.mkv"), 120001, 120001),
        failure(json!("/media/invented.mkv"), 80000, 70000)
    ]);
    snapshot["truncated"] = json!(true);
    snapshot["rejected_timestamps"] = json!(4);
    let fake = Fake::new(vec![Ok(media(60000)), Ok(snapshot)]);
    let result = AgentCollector::new(endpoint(), &fake)
        .collect_with_clock(query(), || Some(120000))
        .await;
    assert_eq!(
        result.plex,
        Observation::Available(PlexContext {
            matching_failures: 2,
            unrelated_failures: 1,
            unknown_correlation: 5,
            truncated: true,
            rejected_timestamps: 4
        })
    );
    let Observation::Available(e) = result.media else {
        panic!()
    };
    assert!(e.plex_failures.is_empty());
    for snapshot in [
        json!({}),
        json!({"failures":null,"truncated":false,"rejected_timestamps":0}),
        json!({"failures":vec![failure(Value::Null,0,0);4097],"truncated":false,"rejected_timestamps":0}),
    ] {
        let fake = Fake::new(vec![Ok(media(60000)), Ok(snapshot)]);
        assert_eq!(
            AgentCollector::new(endpoint(), &fake)
                .collect_with_clock(query(), || Some(120000))
                .await
                .plex,
            Observation::Unavailable(Unavailable::InvalidResponse)
        );
    }
    let fake = Fake::new(vec![
        Ok(media(60000)),
        Ok(
            json!({"failures":vec![failure(Value::Null,60000,60000);4096],"truncated":false,"rejected_timestamps":0}),
        ),
    ]);
    let Observation::Available(p) = AgentCollector::new(endpoint(), &fake)
        .collect_with_clock(query(), || Some(120000))
        .await
        .plex
    else {
        panic!()
    };
    assert_eq!(p.unknown_correlation, 4096);
}

#[tokio::test]
async fn each_log_timestamp_must_independently_belong_to_the_requested_window() {
    for (earliest, latest, matching, unknown) in [
        (59_999, 59_999, 0, 1),
        (60_000, 60_000, 1, 0),
        (120_000, 120_000, 1, 0),
        (120_001, 120_001, 0, 1),
        (60_000, 60_001, 0, 1),
    ] {
        let mut snapshot = logs();
        snapshot["failures"] = json!([failure(json!("/media/invented.mkv"), earliest, latest)]);
        let fake = Fake::new(vec![Ok(media(60_000)), Ok(snapshot)]);
        let evidence = AgentCollector::new(endpoint(), &fake)
            .collect_with_clock(query(), || Some(120_000))
            .await;
        assert_eq!(
            evidence.plex,
            Observation::Available(PlexContext {
                matching_failures: matching,
                unknown_correlation: unknown,
                ..PlexContext::default()
            }),
            "timestamp interval {earliest}..{latest}"
        );
    }
}

fn session_query() -> SessionQuery<'static> {
    SessionQuery {
        plex_account_id: 91,
        username: Some("synthetic & viewer"),
        rating_key: "71",
        hours: 6,
    }
}
fn health() -> Value {
    json!({"sessions_24h":9,"latest_min_ago":0,"active_now":1})
}
fn session(key: &str, age: u64, decision: &str) -> Value {
    json!({"rating_key":key,"minutes_ago":age,"product":"Plex synthetic","player_name":"Invented player","video_decision":decision,"bitrate":20000,"wall_seconds":10,"progress_seconds":20,"watched":false})
}
async fn trace(
    health: Result<Value, Error>,
    rows: Value,
    tautulli: Observation<u32>,
) -> Observation<SessionEvidence> {
    let fake = Fake::new(vec![health, Ok(rows)]);
    TracearrBridgeClient::new(endpoint(), &fake)
        .collect(session_query(), tautulli)
        .await
}
#[tokio::test]
async fn tracearr_exact_item_device_window_contract() {
    let mut other_device = session("72", 3, "copy");
    other_device["player_name"] = json!("Different player");
    let fake = Fake::new(vec![
        Ok(health()),
        Ok(json!({"user_known":true,"sessions":[
            session("71",12,"copy"),session("71",0,"transcode"),session("71",360,"directplay"),
            session("71",361,"transcode"),session("72",3,"direct play"),other_device,
            session("72",4,"unknown")
        ]})),
    ]);
    let result = TracearrBridgeClient::new(endpoint(), &fake)
        .collect(session_query(), Observation::Available(1))
        .await;
    assert_eq!(
        result,
        Observation::Available(SessionEvidence {
            presence: Presence::OnServer,
            attempts: 3,
            transcodes: 1,
            direct_plays: 2,
            stalled: 3,
            other_items: 2,
            other_direct_plays: 1,
            other_stalled: 2,
            newest_decision: Playback::Transcode,
            latest_stream_bps: Some(20_000_000)
        })
    );
    let requests = fake.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].url.path(), "/proxy/tracearr-health");
    assert!(requests[0].url.query().is_none());
    assert_eq!(requests[1].url.path(), "/proxy/sessions");
    assert_eq!(
        requests[1].url.query_pairs().collect::<Vec<_>>(),
        [
            ("plex_account_id".into(), "91".into()),
            ("hours".into(), "6".into()),
            ("username".into(), "synthetic & viewer".into())
        ]
    );
    assert!(
        requests
            .iter()
            .all(|r| r.method == Method::Get && r.body.is_none())
    );
}

#[tokio::test]
async fn known_other_item_history_requires_a_direct_play_for_dv_replacement() {
    use pitcrew_diagnosis::{
        facts::EvidenceFacts,
        rules::{RecommendedAction, classify},
    };

    for (decision, direct) in [
        (Some("copy"), true),
        (Some("directplay"), true),
        (Some("direct play"), true),
        (Some("transcode"), false),
        (Some("unknown"), false),
        (None, false),
    ] {
        let mut other_item = session("72", 1, "copy");
        other_item["video_decision"] = json!(decision);
        let Observation::Available(evidence) = trace(
            Ok(health()),
            json!({"sessions": [session("71", 0, "transcode"), other_item]}),
            Observation::default(),
        )
        .await
        else {
            panic!("missing session evidence")
        };
        assert_eq!(evidence.other_items, 1);
        assert_eq!(
            evidence.other_direct_plays,
            u32::from(direct),
            "{decision:?}"
        );
        assert_eq!(evidence.transcodes, 1);
        assert_eq!(evidence.newest_decision, Playback::Transcode);

        let mut facts = EvidenceFacts::default();
        facts.playback.dynamic_range.dolby_vision = true;
        evidence.apply_to(&mut facts);
        assert_eq!(
            classify(&facts).action,
            if direct {
                RecommendedAction::Replace
            } else {
                RecommendedAction::NotifyAdmin
            },
            "other-item decision {decision:?}"
        );
    }
}

#[tokio::test]
async fn absence_requires_fresh_health_known_viewer_and_complete_window() {
    let empty = json!({"user_known":true,"sessions":[]});
    for age in [0, 1440] {
        let mut h = health();
        h["latest_min_ago"] = json!(age);
        let Observation::Available(e) = trace(Ok(h), empty.clone(), Observation::default()).await
        else {
            panic!()
        };
        assert_eq!(e.presence, Presence::NotSeen);
    }
    for (h, rows, tautulli) in [
        (Err(Error::Timeout), empty.clone(), Observation::default()),
        (
            Ok(json!({"sessions_24h":0,"latest_min_ago":0,"active_now":1})),
            empty.clone(),
            Observation::default(),
        ),
        (
            Ok(json!({"sessions_24h":9,"latest_min_ago":1441,"active_now":1})),
            empty.clone(),
            Observation::default(),
        ),
        (
            Ok(json!({"sessions_24h":9,"latest_min_ago":null,"active_now":1})),
            empty.clone(),
            Observation::default(),
        ),
        (
            Ok(json!({"sessions_24h":9,"latest_min_ago":0,"active_now":0})),
            empty.clone(),
            Observation::Available(1),
        ),
        (Ok(health()), json!({"sessions":[]}), Observation::default()),
        (
            Ok(health()),
            json!({"user_known":false,"sessions":[]}),
            Observation::default(),
        ),
        (
            Ok(health()),
            json!({"user_known":true,"sessions":[session("71",361,"copy")]}),
            Observation::default(),
        ),
        (
            Ok(health()),
            json!({"user_known":true,"sessions":[{"rating_key":"71"}]}),
            Observation::default(),
        ),
    ] {
        let Observation::Available(e) = trace(h, rows, tautulli).await else {
            panic!()
        };
        assert_eq!(e.presence, Presence::Unknown);
        assert_eq!(e.attempts, 0);
        assert_eq!(e.newest_decision, Playback::Unknown);
        assert_eq!(e.latest_stream_bps, None);
    }
    for streams in [0, 1] {
        let mut h = health();
        h["active_now"] = json!(streams);
        let Observation::Available(e) =
            trace(Ok(h), empty.clone(), Observation::Available(0)).await
        else {
            panic!()
        };
        assert_eq!(e.presence, Presence::NotSeen);
    }
    let Observation::Available(e) = trace(
        Err(Error::Timeout),
        json!({"sessions":[session("72",0,"copy")]}),
        Observation::default(),
    )
    .await
    else {
        panic!()
    };
    assert_eq!(e.presence, Presence::OnServer);
    assert_eq!(e.attempts, 0);
}
#[tokio::test]
async fn stalled_requires_all_facts_and_ignores_pause_and_short_session_flags() {
    for (wall, progress, watched, expected) in [
        (json!(179), json!(599), json!(false), 1),
        (json!(180), json!(599), json!(false), 0),
        (json!(179), json!(600), json!(false), 0),
        (Value::Null, json!(20), json!(false), 0),
        (json!(10), Value::Null, json!(false), 0),
        (json!(10), json!(20), Value::Null, 0),
        (json!(10), json!(20), json!(true), 0),
    ] {
        let mut row = session("71", 0, "copy");
        row["wall_seconds"] = wall;
        row["progress_seconds"] = progress;
        row["watched"] = watched;
        row["paused_seconds"] = json!(50000);
        row["short_session"] = json!(true);
        let mut other = row.clone();
        other["rating_key"] = json!("72");
        let Observation::Available(e) = trace(
            Ok(health()),
            json!({"sessions":[row,other]}),
            Observation::default(),
        )
        .await
        else {
            panic!()
        };
        assert_eq!(e.stalled, expected);
        assert_eq!(e.other_stalled, expected);
    }
}
#[tokio::test]
async fn missing_device_or_item_identity_never_correlates_other_sessions() {
    for (field, value) in [
        ("product", Value::Null),
        ("player_name", Value::Null),
        ("product", json!("")),
        ("player_name", json!("")),
    ] {
        let mut row = session("71", 0, "unknown");
        row[field] = value;
        let Observation::Available(e) = trace(
            Ok(health()),
            json!({"sessions":[row,session("72",1,"copy")]}),
            Observation::default(),
        )
        .await
        else {
            panic!()
        };
        assert_eq!(e.other_items, 0);
        assert_eq!(e.newest_decision, Playback::Unknown);
        assert_eq!(e.transcodes, 0);
        assert_eq!(e.direct_plays, 0);
    }
    let mut other = session("72", 1, "copy");
    other["product"] = json!("Other product");
    let mut no_id = session("72", 2, "copy");
    no_id["rating_key"] = Value::Null;
    let mut row = session("71", 0, "copy");
    row["bitrate"] = json!(u64::MAX);
    let Observation::Available(e) = trace(
        Ok(health()),
        json!({"sessions":[row,other,no_id]}),
        Observation::default(),
    )
    .await
    else {
        panic!()
    };
    assert_eq!(e.other_items, 0);
    assert_eq!(e.latest_stream_bps, None);
    let mut row = session("71", 0, "copy");
    row["bitrate"] = Value::Null;
    let Observation::Available(e) = trace(
        Ok(health()),
        json!({"sessions":[row]}),
        Observation::default(),
    )
    .await
    else {
        panic!()
    };
    assert_eq!(e.latest_stream_bps, None);
    assert_eq!(e.newest_decision, Playback::Direct);
}
#[tokio::test]
async fn tracearr_validates_bounds_and_never_downgrades_bad_shapes_to_empty() {
    for rows in [
        json!({}),
        json!({"sessions":null}),
        json!({"sessions":[{"minutes_ago":-1}]}),
        json!({"sessions":vec![session("71",0,"copy");41]}),
    ] {
        assert_eq!(
            trace(Ok(health()), rows, Observation::default()).await,
            Observation::Unavailable(Unavailable::InvalidResponse)
        );
    }
    let Observation::Available(e) = trace(
        Ok(health()),
        json!({"sessions":vec![session("71",0,"copy");40]}),
        Observation::default(),
    )
    .await
    else {
        panic!()
    };
    assert_eq!(e.attempts, 40);
    let fake = Fake::new(vec![Ok(health()), Err(Error::Timeout)]);
    assert_eq!(
        TracearrBridgeClient::new(endpoint(), &fake)
            .collect(session_query(), Observation::default())
            .await,
        Observation::Unavailable(Unavailable::Timeout)
    );
    for query in [
        SessionQuery {
            plex_account_id: 0,
            ..session_query()
        },
        SessionQuery {
            rating_key: "",
            ..session_query()
        },
        SessionQuery {
            rating_key: "71x",
            ..session_query()
        },
        SessionQuery {
            hours: 0,
            ..session_query()
        },
        SessionQuery {
            hours: 1441,
            ..session_query()
        },
        SessionQuery {
            username: Some(""),
            ..session_query()
        },
        SessionQuery {
            username: Some("\n"),
            ..session_query()
        },
    ] {
        let fake = Fake::new(vec![]);
        assert_eq!(
            TracearrBridgeClient::new(endpoint(), &fake)
                .collect(query, Observation::default())
                .await,
            Observation::Unavailable(Unavailable::InvalidRequest)
        );
        assert!(fake.requests.lock().unwrap().is_empty());
    }
    for (n, valid) in [(128, true), (129, false)] {
        let key = "1".repeat(n);
        let fake = Fake::new(if valid {
            vec![Ok(health()), Ok(json!({"sessions":[]}))]
        } else {
            vec![]
        });
        let result = TracearrBridgeClient::new(endpoint(), &fake)
            .collect(
                SessionQuery {
                    rating_key: &key,
                    ..session_query()
                },
                Observation::default(),
            )
            .await;
        assert_eq!(matches!(result, Observation::Available(_)), valid);
    }
    for (n, valid) in [(256, true), (257, false)] {
        let name = "a".repeat(n);
        let fake = Fake::new(if valid {
            vec![Ok(health()), Ok(json!({"sessions":[]}))]
        } else {
            vec![]
        });
        let result = TracearrBridgeClient::new(endpoint(), &fake)
            .collect(
                SessionQuery {
                    username: Some(&name),
                    hours: 1440,
                    ..session_query()
                },
                Observation::default(),
            )
            .await;
        assert_eq!(matches!(result, Observation::Available(_)), valid);
    }
    let fake = Fake::new(vec![Ok(health()), Ok(json!({"sessions":[]}))]);
    let _ = TracearrBridgeClient::new(endpoint(), &fake)
        .collect(
            SessionQuery {
                username: None,
                hours: 1,
                ..session_query()
            },
            Observation::default(),
        )
        .await;
    assert_eq!(
        fake.requests.lock().unwrap()[1].url.query_pairs().count(),
        2
    );
}
#[tokio::test]
async fn tautulli_contract_and_strict_count_parsing() {
    for value in [
        json!(0),
        json!(1),
        json!(u32::MAX),
        json!("0"),
        json!("12"),
        json!(u32::MAX.to_string()),
    ] {
        let fake = Fake::new(vec![Ok(
            json!({"response":{"result":"success","data":{"stream_count":value}}}),
        )]);
        let expected = value
            .as_u64()
            .map(|n| n as u32)
            .unwrap_or_else(|| value.as_str().unwrap().parse().unwrap());
        let result = TautulliClient::new(endpoint(), &fake, "query-secret".into())
            .unwrap()
            .stream_count()
            .await;
        assert_eq!(result, Observation::Available(expected));
        let requests = fake.requests.lock().unwrap();
        let r = &requests[0];
        assert_eq!(r.method, Method::Get);
        assert!(r.body.is_none());
        assert_eq!(r.url.path(), "/proxy/api/v2");
        assert_eq!(
            r.url.query_pairs().collect::<Vec<_>>(),
            [
                ("apikey".into(), "query-secret".into()),
                ("cmd".into(), "get_activity".into())
            ]
        );
    }
    for value in [
        Value::Null,
        json!(true),
        json!(-1),
        json!(1.5),
        json!(u64::from(u32::MAX) + 1),
        json!(""),
        json!("+1"),
        json!(" 1"),
        json!("1x"),
        json!("4294967296"),
        json!({}),
    ] {
        let fake = Fake::new(vec![Ok(
            json!({"response":{"result":"success","data":{"stream_count":value}}}),
        )]);
        assert_eq!(
            TautulliClient::new(endpoint(), &fake, "key".into())
                .unwrap()
                .stream_count()
                .await,
            Observation::Unavailable(Unavailable::InvalidResponse)
        );
    }
    for value in [
        json!({"response":{"result":"error","data":{"stream_count":0}}}),
        json!({"response":{"result":"success","data":{}}}),
        json!({}),
    ] {
        let fake = Fake::new(vec![Ok(value)]);
        assert_eq!(
            TautulliClient::new(endpoint(), &fake, "key".into())
                .unwrap()
                .stream_count()
                .await,
            Observation::Unavailable(Unavailable::InvalidResponse)
        );
    }
    let fake = Fake::new(vec![Err(Error::Timeout)]);
    assert_eq!(
        TautulliClient::new(endpoint(), &fake, "key".into())
            .unwrap()
            .stream_count()
            .await,
        Observation::Unavailable(Unavailable::Timeout)
    );
    for key in [
        "".into(),
        "x".repeat(4097),
        "a b".into(),
        "a\n".into(),
        "☃".into(),
    ] {
        assert!(TautulliClient::new(endpoint(), &fake, key).is_err());
    }
    assert!(TautulliClient::new(endpoint(), &fake, "x".repeat(4096)).is_ok());
    assert_eq!(
        Observation::<u32>::default(),
        Observation::Unavailable(Unavailable::NotConfigured)
    );
    assert_eq!(
        format!(
            "{:?}",
            Observation::<u32>::Unavailable(Unavailable::Timeout)
        ),
        "Unavailable(Timeout)"
    );
    assert_eq!(
        format!("{:?}", Observation::Available("synthetic secret")),
        "Available([local evidence])"
    );
}
struct Pending;
impl Transport for Pending {
    fn send(&self, _: Request) -> ResponseFuture<'_> {
        Box::pin(std::future::pending())
    }
}
#[tokio::test(start_paused = true)]
async fn collector_timeouts_are_bounded_and_independent() {
    let ep = Endpoint::new(
        "https://evidence.invalid",
        Auth::bearer("key").unwrap(),
        Limits {
            timeout: Duration::from_secs(1),
            ..Limits::default()
        },
    )
    .unwrap();
    let result = AgentCollector::new(ep, Pending)
        .collect_with_clock(query(), || Some(120000))
        .await;
    assert_eq!(
        result,
        AgentEvidence {
            media: Observation::Unavailable(Unavailable::Timeout),
            plex: Observation::Unavailable(Unavailable::Timeout)
        }
    );
}

fn collected(value: Value) -> AgentEvidence {
    AgentEvidence {
        media: Observation::Available(serde_json::from_value(value).unwrap()),
        plex: Observation::default(),
    }
}
fn good_media() -> Value {
    let mut value = media(60000);
    value["header"] = json!({"outcome":"Passed","duration_ms":90000,"elapsed_ms":1,"timeout_ms":30000,"faults":[],"streams":[{"Video":{"codec":"h264","width":1920,"height":1080,"bit_depth":8,"dynamic_range":null}}],"tested_ranges":[]});
    value
}
fn mount_read(mount: &str, read: &str) -> Value {
    let mut value = media(60000);
    value["decode"] = Value::Null;
    value["mount"] =
        json!({"mount":"synthetic mount","outcome":mount,"elapsed_ms":1,"timeout_ms":10});
    value["reads"] = json!([{"offset_bytes":0,"requested_bytes":100,"read_bytes":0,"first_byte_ms":null,"elapsed_ms":10,"timeout_ms":10,"bytes_per_second":0,"outcome":read}]);
    value
}
#[test]
fn agent_projection_uses_proven_media_facts_without_copying_identifiers() {
    use pitcrew_diagnosis::facts::{DecodeOutcome as Decode, EvidenceFacts, ProbeOutcome as Probe};
    let mut facts = EvidenceFacts::default();
    facts.playback.file_bps = Some(10);
    facts.playback.is_4k = true;
    facts.playback.analyzed_recently = true;
    collected(good_media()).apply_to(&mut facts);
    assert_eq!(facts.playback.probe, Probe::Video);
    assert_eq!(facts.playback.decode, Decode::Good);
    assert_eq!(facts.playback.file_bps, Some(10));
    assert!(facts.playback.is_4k);
    assert!(facts.playback.analyzed_recently);
    for (outcome, expected) in [
        ("Passed", Probe::Video),
        ("Failed", Probe::OpenFailed),
        ("Unavailable", Probe::Unknown),
        ("TimedOut", Probe::Unknown),
        ("PermissionDenied", Probe::Unknown),
    ] {
        let mut value = good_media();
        value["header"]["outcome"] = json!(outcome);
        collected(value).apply_to(&mut facts);
        assert_eq!(facts.playback.probe, expected);
    }
    let mut value = good_media();
    value["header"]["streams"] = json!([]);
    value["header"]["faults"] = json!(["NoVideo"]);
    collected(value.clone()).apply_to(&mut facts);
    assert_eq!(facts.playback.probe, Probe::NoVideo);
    value["header"]["outcome"] = json!("Unavailable");
    collected(value.clone()).apply_to(&mut facts);
    assert_eq!(facts.playback.probe, Probe::Unknown);
    value["header"]["outcome"] = json!("Passed");
    value["header"]["faults"] = json!([]);
    collected(value).apply_to(&mut facts);
    assert_eq!(facts.playback.probe, Probe::Unknown);
    let mut value = good_media();
    value["header"]["streams"] = json!([{"Audio":{"codec":"aac","channels":2,"language":"eng"}}]);
    collected(value).apply_to(&mut facts);
    assert_eq!(facts.playback.probe, Probe::Unknown);
    collected(media(60000)).apply_to(&mut facts);
    assert_eq!(facts.playback.probe, Probe::Unknown);
    assert_eq!(facts.playback.decode, Decode::Unknown);
    facts.playback.plex_looks_broken = true;
    AgentEvidence::default().apply_to(&mut facts);
    assert!(facts.playback.plex_looks_broken);
    assert_eq!(
        facts.plex_logs,
        pitcrew_diagnosis::facts::PlexLogFacts::default()
    );
    facts.playback.plex_looks_broken = false;
    let evidence = AgentEvidence {
        plex: Observation::Available(PlexContext {
            matching_failures: 1,
            unknown_correlation: 2,
            truncated: true,
            ..PlexContext::default()
        }),
        ..AgentEvidence::default()
    };
    evidence.apply_to(&mut facts);
    assert!(!facts.playback.plex_looks_broken);
    assert_eq!(
        facts.plex_logs,
        pitcrew_diagnosis::facts::PlexLogFacts {
            available: true,
            matching_failures: 1,
            unknown_correlation: 2,
            truncated: true,
            rejected_timestamps: 0,
        }
    );
    assert_eq!(facts.playback.probe, Probe::Unknown);
    assert_eq!(facts.playback.decode, Decode::Unknown);
    let json = serde_json::to_string(&facts).unwrap();
    assert!(!json.contains("revision"));
    assert!(!json.contains("synthetic mount"));
}

#[tokio::test]
async fn rejected_log_timestamps_remain_visible_in_provider_safe_facts() {
    use pitcrew_diagnosis::facts::{EvidenceFacts, PlexLogFacts};

    for rejected in [1, 0, u64::MAX] {
        let mut snapshot = logs();
        snapshot["rejected_timestamps"] = json!(rejected);
        let fake = Fake::new(vec![Ok(media(60000)), Ok(snapshot)]);
        let evidence = AgentCollector::new(endpoint(), &fake)
            .collect_with_clock(query(), || Some(120000))
            .await;
        let mut facts = EvidenceFacts::default();
        evidence.apply_to(&mut facts);
        let serialized = serde_json::to_value(facts).unwrap();
        assert_eq!(
            serialized["plex_logs"]["rejected_timestamps"],
            json!(rejected),
            "discarded timestamped lines must not look like complete log evidence"
        );
        assert!(facts.plex_logs.available);
        assert_eq!(facts.plex_logs.matching_failures, 0);
        assert!(!facts.plex_logs.truncated);
        AgentEvidence::default().apply_to(&mut facts);
        assert_eq!(
            serde_json::to_value(facts).unwrap()["plex_logs"]["rejected_timestamps"],
            json!(0),
            "unavailable replacement observations must clear previous counts"
        );
    }
    assert_eq!(
        serde_json::from_value::<PlexLogFacts>(json!({
            "available": false,
            "matching_failures": 0,
            "unknown_correlation": 0,
            "truncated": false
        }))
        .unwrap(),
        PlexLogFacts::default(),
        "previously recorded facts remain readable without the added counter"
    );
}

#[test]
fn good_decode_requires_every_positive_observation_and_hard_errors_remain_bad() {
    use pitcrew_diagnosis::facts::{DecodeOutcome as Decode, EvidenceFacts};
    let mut facts = EvidenceFacts::default();
    for pointer in [
        "/file/id",
        "/header",
        "/header/duration_ms",
        "/decode",
        "/decode/tested_ranges",
    ] {
        let mut value = good_media();
        *value.pointer_mut(pointer).unwrap() = match pointer {
            "/file/id" => json!(0),
            "/decode/tested_ranges" => json!([]),
            _ => Value::Null,
        };
        collected(value).apply_to(&mut facts);
        assert_eq!(facts.playback.decode, Decode::Unknown, "{pointer}");
    }
    for outcome in ["TimedOut", "PermissionDenied", "Unavailable"] {
        for which in ["header", "decode"] {
            let mut value = good_media();
            value[which]["outcome"] = json!(outcome);
            collected(value).apply_to(&mut facts);
            assert_eq!(facts.playback.decode, Decode::Unknown);
        }
    }
    let mut value = good_media();
    value["header"]["duration_ms"] = json!(0);
    collected(value).apply_to(&mut facts);
    assert_eq!(facts.playback.decode, Decode::Unknown);
    let mut value = good_media();
    value["header"]["duration_ms"] = json!(1);
    collected(value).apply_to(&mut facts);
    assert_eq!(facts.playback.decode, Decode::Good);
    for which in ["header", "decode"] {
        let mut value = good_media();
        value[which]["faults"] = json!(["SlowRead"]);
        collected(value).apply_to(&mut facts);
        assert_eq!(facts.playback.decode, Decode::Unknown);
    }
    let mut value = good_media();
    value["decode"]["outcome"] = json!("Failed");
    collected(value).apply_to(&mut facts);
    assert_eq!(facts.playback.decode, Decode::Bad);
    for mount in ["Passed", "Failed", "TimedOut", "Unavailable"] {
        for read in ["Passed", "Failed", "TimedOut", "Unavailable"] {
            collected(mount_read(mount, read)).apply_to(&mut facts);
            assert_eq!(
                facts.playback.decode,
                if mount == "Passed" && read == "Failed" {
                    Decode::Bad
                } else {
                    Decode::Unknown
                }
            );
        }
    }
    let mut value = mount_read("Passed", "Failed");
    value["reads"] = json!([]);
    collected(value).apply_to(&mut facts);
    assert_eq!(facts.playback.decode, Decode::Unknown);
}
#[test]
fn session_projection_preserves_inventory_metadata_and_maps_only_counts() {
    use pitcrew_diagnosis::facts::{EvidenceFacts, PlaybackDecision};
    for (playback, decision) in [
        (Playback::Unknown, PlaybackDecision::Unknown),
        (Playback::Direct, PlaybackDecision::DirectPlay),
        (Playback::Transcode, PlaybackDecision::Transcode),
    ] {
        let evidence = SessionEvidence {
            transcodes: 2,
            stalled: 3,
            other_items: 4,
            other_direct_plays: 5,
            other_stalled: 6,
            newest_decision: playback,
            latest_stream_bps: Some(99),
            ..SessionEvidence::default()
        };
        let mut facts = EvidenceFacts::default();
        facts.playback.file_bps = Some(11);
        evidence.apply_to(&mut facts);
        assert_eq!(facts.playback.sessions.transcodes, 2);
        assert_eq!(facts.playback.sessions.stalled, 3);
        assert_eq!(facts.playback.sessions.other_items, 4);
        assert_eq!(facts.playback.sessions.other_direct_plays, 5);
        assert_eq!(facts.playback.sessions.other_stalled, 6);
        assert_eq!(facts.playback.sessions.newest_decision, decision);
        assert_eq!(facts.playback.file_bps, Some(11));
    }
}

#[tokio::test]
async fn collected_unknown_and_decode_failure_flow_into_real_rules() {
    use pitcrew_diagnosis::{
        facts::EvidenceFacts,
        rules::{RecommendedAction, classify},
    };
    for (response, expected) in [
        (Err(Error::Timeout), false),
        (
            Ok({
                let mut v = good_media();
                v["decode"]["outcome"] = json!("Failed");
                v
            }),
            true,
        ),
    ] {
        let fake = Fake::new(vec![response, Ok(logs())]);
        let evidence = AgentCollector::new(endpoint(), &fake)
            .collect_with_clock(query(), || Some(120000))
            .await;
        let mut facts = EvidenceFacts::default();
        evidence.apply_to(&mut facts);
        assert_eq!(
            classify(&facts).action == RecommendedAction::Replace,
            expected
        );
    }
}

fn tautulli_history(rows: Value) -> Value {
    json!({"response":{"result":"success","data":{"data":rows}}})
}
fn history_row(key: &str, date: u64, decision: &str) -> Value {
    json!({"rating_key":key,"date":date,"transcode_decision":decision})
}
#[tokio::test]
async fn tautulli_history_preserves_recent_exact_item_context_but_never_absence() {
    let fake = Fake::new(vec![Ok(tautulli_history(json!([
        history_row("71",3600,"copy"),history_row("71",7200,"transcode"),history_row("71",3599,"copy"),
        history_row("71",7201,"copy"),history_row("72",7100,"copy"),history_row("71",3700,"unrecognized"),
        {"rating_key":"71"},{"date":4000}
    ])))]);
    let result = TautulliClient::new(endpoint(), &fake, "history-secret".into())
        .unwrap()
        .history(91, "71", 7200, 1)
        .await;
    assert_eq!(
        result,
        Observation::Available(SessionEvidence {
            presence: Presence::OnServer,
            attempts: 3,
            transcodes: 1,
            direct_plays: 1,
            newest_decision: Playback::Transcode,
            ..SessionEvidence::default()
        })
    );
    {
        let requests = fake.requests.lock().unwrap();
        let r = &requests[0];
        assert_eq!(r.method, Method::Get);
        assert!(r.body.is_none());
        assert_eq!(r.url.path(), "/proxy/api/v2");
        assert_eq!(
            r.url.query_pairs().collect::<Vec<_>>(),
            [
                ("apikey".into(), "history-secret".into()),
                ("cmd".into(), "get_history".into()),
                ("user_id".into(), "91".into()),
                ("length".into(), "40".into())
            ]
        );
    }
    for rows in [
        json!([]),
        json!([history_row("72", 100, "copy")]),
        json!([history_row("71", 7201, "copy")]),
    ] {
        let fake = Fake::new(vec![Ok(tautulli_history(rows))]);
        assert_eq!(
            TautulliClient::new(endpoint(), &fake, "key".into())
                .unwrap()
                .history(91, "71", 7200, 1)
                .await,
            Observation::Available(SessionEvidence::default())
        );
    }
    let fake = Fake::new(vec![Ok(tautulli_history(json!([history_row(
        "71", 0, "copy"
    )])))]);
    let Observation::Available(e) = TautulliClient::new(endpoint(), &fake, "key".into())
        .unwrap()
        .history(91, "71", 1, 1440)
        .await
    else {
        panic!()
    };
    assert_eq!(e.presence, Presence::OnServer);
    assert_eq!(e.newest_decision, Playback::Direct);
}
#[tokio::test]
async fn tautulli_history_rejects_unsupported_shapes_and_invalid_lookups() {
    for value in [
        json!({}),
        json!({"response":{"result":"failure","data":{"data":[]}}}),
        json!({"response":{"result":"success","data":{}}}),
        tautulli_history(Value::Null),
        tautulli_history(json!([{"date":-1}])),
        tautulli_history(json!(vec![history_row("71", 0, "copy"); 41])),
    ] {
        let fake = Fake::new(vec![Ok(value)]);
        assert_eq!(
            TautulliClient::new(endpoint(), &fake, "key".into())
                .unwrap()
                .history(91, "71", 7200, 1)
                .await,
            Observation::Unavailable(Unavailable::InvalidResponse)
        );
    }
    let fake = Fake::new(vec![Err(Error::Timeout)]);
    assert_eq!(
        TautulliClient::new(endpoint(), &fake, "key".into())
            .unwrap()
            .history(91, "71", 7200, 1)
            .await,
        Observation::Unavailable(Unavailable::Timeout)
    );
    for (id, key, hours) in [
        (0, "71", 1),
        (91, "", 1),
        (91, "71x", 1),
        (91, "71", 0),
        (91, "71", 1441),
    ] {
        let fake = Fake::new(vec![]);
        assert_eq!(
            TautulliClient::new(endpoint(), &fake, "key".into())
                .unwrap()
                .history(id, key, 7200, hours)
                .await,
            Observation::Unavailable(Unavailable::InvalidRequest)
        );
        assert!(fake.requests.lock().unwrap().is_empty());
    }
    for (n, valid) in [(128, true), (129, false)] {
        let key = "1".repeat(n);
        let fake = Fake::new(if valid {
            vec![Ok(tautulli_history(json!(vec![
                history_row(
                    &key, 0, "copy"
                );
                40
            ])))]
        } else {
            vec![]
        });
        let result = TautulliClient::new(endpoint(), &fake, "key".into())
            .unwrap()
            .history(91, &key, 0, 1)
            .await;
        if valid {
            let Observation::Available(e) = result else {
                panic!()
            };
            assert_eq!(e.attempts, 40);
        } else {
            assert_eq!(
                result,
                Observation::Unavailable(Unavailable::InvalidRequest)
            );
        }
    }
}

struct AdvancingClock {
    fake: Fake,
    clock: std::sync::atomic::AtomicU64,
}
impl Transport for &AdvancingClock {
    fn send(&self, request: Request) -> ResponseFuture<'_> {
        self.clock
            .fetch_add(1000, std::sync::atomic::Ordering::SeqCst);
        self.fake.requests.lock().unwrap().push(request);
        Box::pin(async { self.fake.responses.lock().unwrap().pop_front().unwrap() })
    }
}
#[tokio::test]
async fn agent_observation_created_after_request_start_is_current() {
    let mut snapshot = logs();
    snapshot["failures"] = json!([failure(json!("/media/invented.mkv"), 61_500, 61_500)]);
    let fake = AdvancingClock {
        fake: Fake::new(vec![Ok(media(61_000)), Ok(snapshot)]),
        clock: std::sync::atomic::AtomicU64::new(60_000),
    };
    let result = AgentCollector::new(endpoint(), &fake)
        .collect_with_clock(query(), || {
            Some(fake.clock.load(std::sync::atomic::Ordering::SeqCst))
        })
        .await;
    assert!(matches!(result.media, Observation::Available(_)));
    let Observation::Available(context) = result.plex else {
        panic!()
    };
    assert_eq!(context.matching_failures, 1);
    assert_eq!(context.unknown_correlation, 0);
}
#[tokio::test]
async fn production_clock_and_unavailable_clock_are_explicit() {
    let now = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    let fake = Fake::new(vec![Ok(media(now)), Ok(logs())]);
    let evidence = AgentCollector::new(endpoint(), &fake)
        .collect(query())
        .await;
    assert!(matches!(evidence.media, Observation::Available(_)));
    assert!(matches!(evidence.plex, Observation::Available(_)));
    let fake = Fake::new(vec![Ok(media(120000))]);
    let evidence = AgentCollector::new(endpoint(), &fake)
        .collect_with_clock(query(), || None)
        .await;
    assert_eq!(
        evidence,
        AgentEvidence {
            media: Observation::Unavailable(Unavailable::InvalidRequest),
            plex: Observation::Unavailable(Unavailable::InvalidRequest),
        }
    );
    assert_eq!(fake.requests.lock().unwrap().len(), 1);
    let fake = Fake::new(vec![Ok(media(120000)), Ok(logs())]);
    let calls = std::sync::atomic::AtomicU32::new(0);
    let evidence = AgentCollector::new(endpoint(), &fake)
        .collect_with_clock(query(), || {
            (calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0).then_some(120000)
        })
        .await;
    assert!(matches!(evidence.media, Observation::Available(_)));
    assert_eq!(
        evidence.plex,
        Observation::Unavailable(Unavailable::InvalidRequest)
    );
}

#[test]
fn dynamic_range_projection_requires_one_known_video_stream_and_clean_probe() {
    use pitcrew_diagnosis::facts::{DvProfile, DynamicRange, EvidenceFacts};
    for (range, expected) in [
        ("SDR", DynamicRange::default()),
        (
            "HDR10",
            DynamicRange {
                hdr: true,
                ..DynamicRange::default()
            },
        ),
        (
            "HLG",
            DynamicRange {
                hdr: true,
                ..DynamicRange::default()
            },
        ),
        (
            "DOVI_PROFILE_5",
            DynamicRange {
                dolby_vision: true,
                no_hdr_fallback: true,
                profile: DvProfile::Five,
                ..DynamicRange::default()
            },
        ),
        (
            "DOVI_PROFILE_7",
            DynamicRange {
                dolby_vision: true,
                hdr: true,
                profile: DvProfile::Other,
                ..DynamicRange::default()
            },
        ),
        (
            "DOVI_PROFILE_8",
            DynamicRange {
                dolby_vision: true,
                profile: DvProfile::Eight,
                ..DynamicRange::default()
            },
        ),
        ("DOVI", DynamicRange::default()),
    ] {
        let mut value = good_media();
        value["header"]["streams"][0]["Video"]["dynamic_range"] = json!(range);
        let mut facts = EvidenceFacts::default();
        collected(value).apply_to(&mut facts);
        assert_eq!(facts.playback.dynamic_range, expected, "{range}");
    }
    for case in 0..7 {
        let mut value = good_media();
        value["header"]["streams"][0]["Video"]["dynamic_range"] = json!("DOVI_PROFILE_5");
        match case {
            0 => value["header"]["outcome"] = json!("Failed"),
            1 => value["header"]["faults"] = json!(["InvalidHeader"]),
            2 => value["header"]["streams"] = json!([]),
            3 => value["header"]["streams"].as_array_mut().unwrap().push(json!({"Video":{"codec":"hevc","width":1920,"height":1080,"bit_depth":10,"dynamic_range":"SDR"}})),
            4 => value["header"] = Value::Null,
            5 => value["file"]["id"] = json!(0),
            _ => value["file"]["revision"] = json!(0),
        }
        let mut facts = EvidenceFacts::default();
        facts.playback.dynamic_range.dolby_vision = true;
        collected(value).apply_to(&mut facts);
        assert_eq!(
            facts.playback.dynamic_range,
            DynamicRange::default(),
            "case {case}"
        );
    }
    let mut facts = EvidenceFacts::default();
    facts.playback.dynamic_range.dolby_vision = true;
    AgentEvidence::default().apply_to(&mut facts);
    assert_eq!(facts.playback.dynamic_range, DynamicRange::default());
}
