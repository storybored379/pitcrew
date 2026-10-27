use pitcrew_domain::{media::*, *};
use pitcrew_engine::{
    target::{self, TargetBinding, TargetVerifier},
    *,
};
use pitcrew_plex::Client;
use pitcrew_store::{policy::IdentityFingerprint, *};
use pitcrew_upstream::{
    Auth, Endpoint, Limits, Method, Request, Response, ResponseFuture, Transport, agent,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

const PATH: &str = "/media/synthetic-target.mkv";
#[derive(Clone)]
struct HttpFake(Arc<Mutex<HttpState>>);
struct HttpState {
    plex: Value,
    agent: Value,
    error: Option<pitcrew_upstream::Error>,
    requests: Vec<(String, Vec<(String, String)>)>,
}
impl Transport for HttpFake {
    fn send(&self, request: Request) -> ResponseFuture<'_> {
        assert_eq!(request.method, Method::Get);
        assert!(request.body.is_none());
        let mut state = self.0.lock().unwrap();
        let path = request.url.path().to_owned();
        let query = request
            .url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        state.requests.push((path.clone(), query));
        let response = if let Some(error) = state.error {
            Err(error)
        } else {
            Ok(Response {
                status: 200,
                body: serde_json::to_vec(if path == "/library/metadata/81" {
                    &state.plex
                } else {
                    assert_eq!(path, "/v1/evidence");
                    &state.agent
                })
                .unwrap(),
            })
        };
        Box::pin(async move { response })
    }
}
#[derive(Clone, Default)]
struct Sent(Arc<Mutex<Vec<Intent>>>);
impl Upstream for Sent {
    fn read<'a>(&'a self, _: &'a Intent) -> CallFuture<'a> {
        Box::pin(async { Err(CallError::Unavailable) })
    }
    fn write<'a>(&'a self, intent: &'a Intent) -> CallFuture<'a> {
        self.0.lock().unwrap().push(intent.clone());
        Box::pin(async { Ok(Outcome::Delivered) })
    }
}
struct Rig {
    _temp: tempfile::TempDir,
    store: Store,
    lease: Lease,
    intent: Intent,
    binding: TargetBinding,
    http: HttpFake,
    sent: Sent,
}
impl Rig {
    async fn new(constraints: Constraints) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("db")).await.unwrap();
        let fingerprint = IdentityFingerprint {
            arr_file_id: 77,
            size: 10000,
            date_added: "2026-01-01T00:00:00Z".into(),
            quality: "synthetic-quality-1".into(),
            agent: FileIdentity {
                id: 1234,
                revision: 5678,
            },
        };
        let imported = store
            .register_file("client-a", "item-a", &fingerprint, 10)
            .await
            .unwrap();
        store
            .create_job(
                &NewJob {
                    id: 1,
                    viewer: "viewer-a".into(),
                    resource: "item-a".into(),
                    library: 2,
                    original: FileIdentity {
                        id: 20,
                        revision: 30,
                    },
                    constraints,
                    policy: Policy {
                        max_grabs: 1,
                        stage_timeout: 300,
                        total_timeout: 1000,
                    },
                    timeouts: StageTimeouts {
                        download: 300,
                        import: 300,
                        decode: 300,
                        verify_retry: 180,
                    },
                    created_at: 0,
                },
                "create",
                BudgetLimits {
                    global_hour: 4,
                    global_day: 4,
                    viewer_hour: 4,
                    viewer_day: 4,
                },
            )
            .await
            .unwrap();
        let lease = store.acquire_lease(1, "worker", 1, 900).await.unwrap();
        for (revision, event) in [
            Event::SelectRelease {
                release: 40,
                access: Access::Granted {
                    library: 2,
                    expires_at: 900,
                },
            },
            Event::AutoImported {
                grab: IntentKey {
                    job: 1,
                    sequence: 1,
                },
                download: 50,
                file: imported,
            },
            Event::Observed {
                intent: IntentKey {
                    job: 1,
                    sequence: 2,
                },
                observation: Observation::ImportedDecoded {
                    file: imported,
                    good: true,
                },
            },
        ]
        .into_iter()
        .enumerate()
        {
            store
                .apply_event(
                    &lease,
                    revision as u64,
                    &format!("event-{revision}"),
                    event,
                    1,
                )
                .await
                .unwrap();
        }
        for sequence in 1..3 {
            store
                .finish_intent(
                    &lease,
                    IntentKey { job: 1, sequence },
                    OutboxStatus::Completed,
                    1,
                )
                .await
                .unwrap();
        }
        let intent = store
            .outbox(1)
            .await
            .unwrap()
            .last()
            .unwrap()
            .intent
            .clone();
        assert!(matches!(intent.effect, Effect::CheckTarget { .. }));
        let binding = TargetBinding {
            client: "client-a".into(),
            rating_key: 81,
            path: PATH.into(),
            fingerprint: fingerprint.clone(),
        };
        let evidence = MediaEvidence {
            file: fingerprint.agent,
            observed_at_unix_ms: 10000,
            symlinks: None,
            mount: None,
            reads: vec![],
            decode: None,
            plex_failures: vec![],
            header: Some(MediaProbe {
                outcome: ProbeOutcome::Passed,
                duration_ms: Some(100000),
                elapsed_ms: 1,
                timeout_ms: 1000,
                faults: vec![],
                tested_ranges: vec![],
                streams: vec![
                    StreamFacts::Video {
                        codec: "h264".into(),
                        width: 1920,
                        height: 1080,
                        bit_depth: Some(8),
                        dynamic_range: Some("SDR".into()),
                    },
                    StreamFacts::Audio {
                        codec: "aac".into(),
                        channels: 2,
                        language: Some("eng".into()),
                    },
                    StreamFacts::Subtitle {
                        codec: "text".into(),
                        language: Some("spa".into()),
                    },
                ],
            }),
        };
        Self {
            _temp: temp,
            store,
            lease,
            intent,
            binding,
            sent: Sent::default(),
            http: HttpFake(Arc::new(Mutex::new(HttpState {
                plex: json!({"MediaContainer":{"size":1,"Metadata":[{"ratingKey":"81","Media":[{"Part":[{"file":PATH}]}]}]}}),
                agent: serde_json::to_value(evidence).unwrap(),
                error: None,
                requests: vec![],
            }))),
        }
    }
    fn adapter(&self, now: u64) -> TargetVerifier<HttpFake, HttpFake, Sent> {
        TargetVerifier {
            store: self.store.clone(),
            plex: Client::new(
                Endpoint::new(
                    "https://plex.invalid",
                    Auth::plex("synthetic-plex-secret").unwrap(),
                    Limits::default(),
                )
                .unwrap(),
                self.http.clone(),
            ),
            agent: agent::Client::new(
                Endpoint::new(
                    "https://agent.invalid",
                    Auth::bearer("synthetic-agent-secret").unwrap(),
                    Limits::default(),
                )
                .unwrap(),
                self.http.clone(),
            ),
            notifications: self.sent.clone(),
            clock: Arc::new(move || now),
            max_identity_age: 30,
        }
    }
    async fn bind(&self) {
        target::bind(&self.store, &self.intent, &self.binding, 10)
            .await
            .unwrap();
    }
    async fn claim(&self) {
        self.store
            .start_intent(
                &self.lease,
                IntentKey {
                    job: 1,
                    sequence: 3,
                },
                3,
                10,
            )
            .await
            .unwrap();
    }
    fn expected(&self, satisfied: bool) -> Outcome {
        let Effect::CheckTarget { file, .. } = self.intent.effect else {
            panic!()
        };
        Outcome::Observed(Observation::TargetChecked {
            file,
            plex_file: file,
            constraints_satisfied: satisfied,
        })
    }
}
fn constraints() -> Constraints {
    Constraints {
        language: Some("ENG".into()),
        require_sdr: true,
        ..Constraints::default()
    }
}

#[tokio::test]
async fn worker_retries_uncertain_target_check_after_store_reopen() {
    let mut rig = Rig::new(constraints()).await;
    rig.bind().await;
    let options = Options {
        mode: Mode::Live,
        acknowledge_live_mutations: true,
        call_timeout: std::time::Duration::from_secs(1),
    };
    rig.http.0.lock().unwrap().error = Some(pitcrew_upstream::Error::Timeout);
    Worker::new(Some(rig.store.clone()), rig.adapter(10), options.clone())
        .unwrap()
        .drive(&rig.lease, 10)
        .await
        .unwrap();
    assert_eq!(
        rig.store.load_job(1).await.unwrap().job.state(),
        State::Imported
    );
    assert_eq!(
        rig.store.outbox(1).await.unwrap().last().unwrap().status,
        OutboxStatus::Uncertain
    );
    rig.store.release_lease(&rig.lease, 11).await.unwrap();
    rig.store = Store::open(rig._temp.path().join("db")).await.unwrap();
    rig.lease = rig
        .store
        .acquire_lease(1, "restarted", 12, 900)
        .await
        .unwrap();
    rig.http.0.lock().unwrap().error = None;
    let calls = rig.http.0.lock().unwrap().requests.len();
    let worker = Worker::new(Some(rig.store.clone()), rig.adapter(12), options).unwrap();
    assert_eq!(worker.drive(&rig.lease, 12).await.unwrap(), 1);
    assert_eq!(rig.http.0.lock().unwrap().requests.len(), calls + 2);
    assert_eq!(
        rig.store.load_job(1).await.unwrap().job.state(),
        State::Verified
    );
    assert_eq!(
        rig.store.outbox(1).await.unwrap().last().unwrap().status,
        OutboxStatus::Completed
    );
    assert_eq!(worker.drive(&rig.lease, 12).await.unwrap(), 0);
    assert!(rig.sent.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn real_worker_checks_exact_target_then_ready_delegates_once() {
    let rig = Rig::new(constraints()).await;
    rig.bind().await;
    let worker = Worker::new(
        Some(rig.store.clone()),
        rig.adapter(10),
        Options {
            mode: Mode::Live,
            acknowledge_live_mutations: true,
            call_timeout: std::time::Duration::from_secs(1),
        },
    )
    .unwrap();
    assert_eq!(worker.drive(&rig.lease, 10).await.unwrap(), 1);
    assert_eq!(
        rig.store.load_job(1).await.unwrap().job.state(),
        State::Verified
    );
    {
        let requests = &rig.http.0.lock().unwrap().requests;
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].0, "/library/metadata/81");
        assert_eq!(
            requests[1],
            (
                "/v1/evidence".into(),
                vec![
                    ("path".into(), PATH.into()),
                    ("decode".into(), "false".into())
                ]
            )
        );
    }
    rig.store
        .apply_event(&rig.lease, 4, "ready", Event::QueueReady, 10)
        .await
        .unwrap();
    assert_eq!(worker.drive(&rig.lease, 10).await.unwrap(), 1);
    assert_eq!(worker.drive(&rig.lease, 10).await.unwrap(), 0);
    assert_eq!(rig.sent.0.lock().unwrap().len(), 1);
    assert!(matches!(
        rig.sent.0.lock().unwrap()[0].effect,
        Effect::NotifyReady { .. }
    ));
}

#[tokio::test]
async fn stale_or_future_agent_observations_cannot_certify_target() {
    for at in [0, 41_000] {
        let rig = Rig::new(constraints()).await;
        rig.bind().await;
        rig.claim().await;
        rig.http.0.lock().unwrap().agent["observed_at_unix_ms"] = json!(at);
        assert_eq!(
            rig.adapter(40).read(&rig.intent).await,
            Err(CallError::Unavailable)
        );
    }
}

#[tokio::test]
async fn registry_fingerprint_revision_age_and_instance_must_still_match() {
    for case in 0..7 {
        let mut rig = Rig::new(constraints()).await;
        if case == 0 {
            rig.binding.client = "another-client".into();
        }
        if case == 1 {
            rig.binding.fingerprint.size += 1;
        }
        if case == 2 {
            rig.binding.fingerprint.date_added = "changed".into();
        }
        if case == 3 {
            rig.binding.fingerprint.quality = "changed".into();
        }
        if case == 4 {
            rig.http.0.lock().unwrap().agent["file"]["revision"] = json!(9999);
        }
        if case == 5 {
            let mut changed = rig.binding.fingerprint.clone();
            changed.size += 1;
            rig.store
                .register_file("client-a", "item-a", &changed, 10)
                .await
                .unwrap();
        }
        rig.bind().await;
        rig.claim().await;
        let now = if case == 6 { 41 } else { 10 };
        assert_eq!(
            rig.adapter(now).read(&rig.intent).await,
            Err(CallError::Unavailable),
            "case {case}"
        );
    }
    let rig = Rig::new(constraints()).await;
    rig.bind().await;
    rig.claim().await;
    assert_eq!(
        rig.adapter(40).read(&rig.intent).await,
        Ok(rig.expected(true))
    );
    assert_eq!(
        rig.adapter(9).read(&rig.intent).await,
        Err(CallError::Unavailable)
    );
}

#[tokio::test]
async fn ambiguous_plex_versions_parts_paths_and_keys_are_never_ready() {
    let baseline = json!({"ratingKey":"81","Media":[{"Part":[{"file":PATH}]}]});
    let cases = vec![
        json!({"size":0,"Metadata":[]}),
        json!({"size":2,"Metadata":[baseline.clone(),baseline]}),
        json!({"size":1,"Metadata":[{"ratingKey":"82","Media":[{"Part":[{"file":PATH}]}]}]}),
        json!({"size":1,"Metadata":[{"ratingKey":"81"}]}),
        json!({"size":1,"Metadata":[{"ratingKey":"81","Media":[]}]}),
        json!({"size":1,"Metadata":[{"ratingKey":"81","Media":[{"Part":[{"file":PATH}]},{"Part":[{"file":"/media/ghost.mkv"}]}]}]}),
        json!({"size":1,"Metadata":[{"ratingKey":"81","Media":[{}]}]}),
        json!({"size":1,"Metadata":[{"ratingKey":"81","Media":[{"Part":[]}]}]}),
        json!({"size":1,"Metadata":[{"ratingKey":"81","Media":[{"Part":[{"file":PATH},{"file":PATH}]}]}]}),
        json!({"size":1,"Metadata":[{"ratingKey":"81","Media":[{"Part":[{"file":"/media/other.mkv"}]}]}]}),
        json!({"size":1,"Metadata":[{"ratingKey":"81","Media":[{"Part":[{}]}]}]}),
    ];
    for (index, body) in cases.into_iter().enumerate() {
        let rig = Rig::new(constraints()).await;
        rig.bind().await;
        rig.claim().await;
        rig.http.0.lock().unwrap().plex = json!({"MediaContainer":body});
        assert_eq!(
            rig.adapter(10).read(&rig.intent).await,
            Err(CallError::Unavailable),
            "case {index}"
        );
        assert_eq!(rig.http.0.lock().unwrap().requests.len(), 1);
    }
}

#[tokio::test]
async fn constraints_need_positive_header_and_complete_language_range_evidence() {
    for case in 0..12 {
        let rig = Rig::new(constraints()).await;
        rig.bind().await;
        rig.claim().await;
        {
            let agent = &mut rig.http.0.lock().unwrap().agent;
            match case {
                0=>agent["header"]=Value::Null,
                1=>agent["header"]["outcome"]=json!("Unavailable"),
                2=>agent["header"]["faults"]=json!(["NoAudio"]),
                3=>agent["header"]["streams"]=json!([]),
                4=>agent["header"]["streams"][1]["Audio"]["language"]=Value::Null,
                5=>agent["header"]["streams"][0]["Video"]["dynamic_range"]=Value::Null,
                6=>agent["header"]["streams"][1]["Audio"]["language"]=json!("spa"),
                7=>agent["header"]["streams"][0]["Video"]["dynamic_range"]=json!("HDR10"),
                8=>agent["header"]["streams"].as_array_mut().unwrap().retain(|s|s.get("Video").is_none()),
                9=>agent["header"]["streams"].as_array_mut().unwrap().retain(|s|s.get("Audio").is_none()),
                10=>agent["header"]["streams"].as_array_mut().unwrap().push(json!({"Audio":{"codec":"aac","channels":2,"language":null}})),
                _=>agent["header"]["streams"].as_array_mut().unwrap().push(json!({"Video":{"codec":"hevc","width":1920,"height":1080,"bit_depth":10,"dynamic_range":"HDR10"}})),
            }
        }
        let expected = if matches!(case, 6 | 7 | 11) {
            Ok(rig.expected(false))
        } else {
            Err(CallError::Unavailable)
        };
        assert_eq!(
            rig.adapter(10).read(&rig.intent).await,
            expected,
            "case {case}"
        );
    }
    for constraints in [
        Constraints::default(),
        Constraints {
            language: Some("ENG".into()),
            require_sdr: false,
            ..Constraints::default()
        },
        Constraints {
            language: None,
            require_sdr: true,
            ..Constraints::default()
        },
    ] {
        let rig = Rig::new(constraints.clone()).await;
        rig.bind().await;
        rig.claim().await;
        if constraints == Constraints::default() {
            rig.http.0.lock().unwrap().agent["header"] = Value::Null;
        }
        assert_eq!(
            rig.adapter(10).read(&rig.intent).await,
            Ok(rig.expected(true))
        );
    }
}

#[tokio::test]
async fn independent_jobs_keep_distinct_plex_target_routes() {
    let rig = Rig::new(constraints()).await;
    rig.bind().await;
    rig.claim().await;
    let mut second = rig.store.load_job(1).await.unwrap().spec;
    second.id = 2;
    second.resource = "item-b".into();
    second.original.id = 21;
    rig.store
        .create_job(
            &second,
            "create-second",
            BudgetLimits {
                global_hour: 4,
                global_day: 4,
                viewer_hour: 4,
                viewer_day: 4,
            },
        )
        .await
        .unwrap();
    let mut binding = rig.binding.clone();
    binding.rating_key = 82;
    binding.path = "/media/second-target.mkv".into();
    binding.fingerprint.arr_file_id = 78;
    binding.fingerprint.agent.id = 1235;
    let imported = rig
        .store
        .register_file("client-a", "item-b", &binding.fingerprint, 10)
        .await
        .unwrap();
    let lease = rig.store.acquire_lease(2, "worker", 1, 900).await.unwrap();
    for (revision, event) in [
        Event::SelectRelease {
            release: 41,
            access: Access::Granted {
                library: 2,
                expires_at: 900,
            },
        },
        Event::AutoImported {
            grab: IntentKey {
                job: 2,
                sequence: 1,
            },
            download: 51,
            file: imported,
        },
        Event::Observed {
            intent: IntentKey {
                job: 2,
                sequence: 2,
            },
            observation: Observation::ImportedDecoded {
                file: imported,
                good: true,
            },
        },
    ]
    .into_iter()
    .enumerate()
    {
        rig.store
            .apply_event(
                &lease,
                revision as u64,
                &format!("second-event-{revision}"),
                event,
                1,
            )
            .await
            .unwrap();
    }
    let intent = rig
        .store
        .outbox(2)
        .await
        .unwrap()
        .last()
        .unwrap()
        .intent
        .clone();
    assert_eq!(intent.sequence, rig.intent.sequence);
    target::bind(&rig.store, &intent, &binding, 10)
        .await
        .unwrap();
    assert_eq!(
        rig.adapter(10).read(&rig.intent).await.unwrap(),
        rig.expected(true)
    );
}

#[tokio::test]
async fn target_route_requires_both_trusted_source_and_exact_intent_key() {
    for (source, key) in [
        ("untrusted", "1:3"),
        ("target_route", "1:4"),
        ("target_route", "2:3"),
    ] {
        let rig = Rig::new(constraints()).await;
        rig.claim().await;
        rig.store
            .ingest_observation(
                source,
                key,
                &serde_json::to_value(&rig.binding).unwrap(),
                10,
            )
            .await
            .unwrap();
        assert_eq!(
            rig.adapter(10).read(&rig.intent).await,
            Err(CallError::Unavailable),
            "{source}/{key}"
        );
        assert!(rig.http.0.lock().unwrap().requests.is_empty());
    }
}

#[tokio::test]
async fn persisted_target_intent_must_match_the_current_journal_key() {
    let rig = Rig::new(constraints()).await;
    rig.claim().await;
    let mut forged = rig.intent.clone();
    forged.sequence += 1;
    // Fault injection into this owned fixture: a claimed outbox row is moved
    // ahead of the event journal. Its matching route is not authorization.
    let connection = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new().filename(rig._temp.path().join("db")),
    )
    .await
    .unwrap();
    assert_eq!(
        sqlx::query("UPDATE outbox SET sequence=?,intent=? WHERE job=1 AND sequence=3")
            .bind(i64::try_from(forged.sequence).unwrap())
            .bind(serde_json::to_string(&forged).unwrap())
            .execute(&connection)
            .await
            .unwrap()
            .rows_affected(),
        1
    );
    target::bind(&rig.store, &forged, &rig.binding, 10)
        .await
        .unwrap();
    assert_eq!(
        rig.adapter(10).read(&forged).await,
        Err(CallError::Unavailable)
    );
    assert!(rig.http.0.lock().unwrap().requests.is_empty());
    connection.close().await;
}

#[tokio::test]
async fn a_single_requested_constraint_is_still_enforced() {
    for constraints in [
        Constraints {
            language: Some("eng".into()),
            require_sdr: false,
            ..Constraints::default()
        },
        Constraints {
            language: None,
            require_sdr: true,
            ..Constraints::default()
        },
    ] {
        let rig = Rig::new(constraints).await;
        rig.bind().await;
        rig.claim().await;
        {
            let agent = &mut rig.http.0.lock().unwrap().agent;
            agent["header"]["streams"][0]["Video"]["dynamic_range"] = json!("HDR10");
            agent["header"]["streams"][1]["Audio"]["language"] = json!("spa");
        }
        assert_eq!(
            rig.adapter(10).read(&rig.intent).await,
            Ok(rig.expected(false))
        );
        rig.http.0.lock().unwrap().agent["header"] = Value::Null;
        assert_eq!(
            rig.adapter(10).read(&rig.intent).await,
            Err(CallError::Unavailable)
        );
    }
}

#[tokio::test]
async fn unbound_unclaimed_forged_and_stale_intents_do_not_read_upstreams() {
    let pending = Rig::new(constraints()).await;
    pending.bind().await;
    assert_eq!(
        pending.adapter(10).read(&pending.intent).await,
        Err(CallError::Unavailable)
    );
    assert!(pending.http.0.lock().unwrap().requests.is_empty());
    let actual_grab = pending.store.outbox(1).await.unwrap()[0].intent.clone();
    assert_eq!(
        target::bind(&pending.store, &actual_grab, &pending.binding, 10).await,
        Err(CallError::Unavailable)
    );
    let rig = Rig::new(constraints()).await;
    assert_eq!(
        rig.adapter(10).read(&rig.intent).await,
        Err(CallError::Unavailable)
    );
    rig.claim().await;
    assert_eq!(
        rig.adapter(10).read(&rig.intent).await,
        Err(CallError::Unavailable)
    );
    let mut forged = rig.intent.clone();
    forged.effect = Effect::Grab {
        release: 40,
        reservation: 1,
    };
    assert_eq!(
        target::bind(&rig.store, &forged, &rig.binding, 10).await,
        Err(CallError::Unavailable)
    );
    assert_eq!(
        rig.adapter(10).read(&forged).await,
        Err(CallError::Unavailable)
    );
    forged = rig.intent.clone();
    forged.sequence += 1;
    assert_eq!(
        target::bind(&rig.store, &forged, &rig.binding, 10).await,
        Err(CallError::Unavailable)
    );
    assert_eq!(
        rig.adapter(10).read(&forged).await,
        Err(CallError::Unavailable)
    );
    forged = rig.intent.clone();
    forged.job = 99;
    assert_eq!(
        rig.adapter(10).read(&forged).await,
        Err(CallError::Unavailable)
    );
    rig.bind().await;
    let mut other = rig.binding.clone();
    other.path = "/media/other.mkv".into();
    assert_eq!(
        target::bind(&rig.store, &rig.intent, &other, 10).await,
        Err(CallError::Unavailable)
    );
    forged = rig.intent.clone();
    if let Effect::CheckTarget { constraints, .. } = &mut forged.effect {
        constraints.require_sdr = false;
    }
    assert_eq!(
        rig.adapter(10).read(&forged).await,
        Err(CallError::Unavailable)
    );
    assert!(rig.http.0.lock().unwrap().requests.is_empty());
    rig.store
        .apply_event(
            &rig.lease,
            3,
            "checked",
            Event::Observed {
                intent: IntentKey {
                    job: 1,
                    sequence: 3,
                },
                observation: match rig.expected(true) {
                    Outcome::Observed(o) => o,
                    _ => panic!(),
                },
            },
            10,
        )
        .await
        .unwrap();
    assert_eq!(
        rig.adapter(10).read(&rig.intent).await,
        Err(CallError::Unavailable)
    );
}

#[tokio::test]
async fn upstream_failures_are_unavailable_and_cannot_queue_ready() {
    for error in [
        pitcrew_upstream::Error::Timeout,
        pitcrew_upstream::Error::Transport,
        pitcrew_upstream::Error::InvalidResponse,
    ] {
        let rig = Rig::new(constraints()).await;
        rig.bind().await;
        rig.claim().await;
        rig.http.0.lock().unwrap().error = Some(error);
        assert_eq!(
            rig.adapter(10).read(&rig.intent).await,
            Err(CallError::Unavailable)
        );
        assert_eq!(
            rig.store.load_job(1).await.unwrap().job.state(),
            State::Imported
        );
        assert!(rig.sent.0.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn no_profile5_requires_positive_dynamic_range_for_every_video_stream() {
    for (range, expected) in [
        (Some("SDR"), Some(true)),
        (Some("HDR10"), Some(true)),
        (Some("HLG"), Some(true)),
        (Some("DOVI_PROFILE_5"), Some(false)),
        (Some("DOVI_PROFILE_7"), Some(true)),
        (Some("DOVI_PROFILE_8"), Some(true)),
        (Some("DOVI"), None),
        (Some("DOVI_PROFILE_0"), None),
        (Some("HDR10+"), None),
        (Some("unknown"), None),
        (None, None),
    ] {
        let constraints: Constraints = serde_json::from_value(json!({
            "language": null, "require_sdr": false, "forbid_dolby_vision_profile5": true
        }))
        .unwrap();
        let rig = Rig::new(constraints).await;
        rig.bind().await;
        rig.claim().await;
        rig.http.0.lock().unwrap().agent["header"]["streams"][0]["Video"]["dynamic_range"] =
            json!(range);
        assert_eq!(
            rig.adapter(10).read(&rig.intent).await,
            expected
                .map(|ok| rig.expected(ok))
                .ok_or(CallError::Unavailable),
            "{range:?}"
        );
    }
    for case in 0..5 {
        let constraints: Constraints = serde_json::from_value(json!({
            "language": null, "require_sdr": false, "forbid_dolby_vision_profile5": true
        }))
        .unwrap();
        let rig = Rig::new(constraints).await;
        rig.bind().await;
        rig.claim().await;
        let expected = {
            let agent = &mut rig.http.0.lock().unwrap().agent;
            match case {
                0 => {
                    agent["header"] = Value::Null;
                    None
                }
                1 => {
                    agent["header"]["outcome"] = json!("Unavailable");
                    None
                }
                2 => {
                    agent["header"]["streams"] = json!([]);
                    None
                }
                _ => {
                    agent["header"]["streams"].as_array_mut().unwrap().push(json!({
                        "Video": {"codec":"hevc","width":1920,"height":1080,"bit_depth":10,
                        "dynamic_range":if case == 3 {json!("DOVI_PROFILE_5")} else {Value::Null}}
                    }));
                    if case == 3 { Some(false) } else { None }
                }
            }
        };
        assert_eq!(
            rig.adapter(10).read(&rig.intent).await,
            expected
                .map(|ok| rig.expected(ok))
                .ok_or(CallError::Unavailable),
            "case {case}"
        );
    }
}
