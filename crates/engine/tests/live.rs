use pitcrew_arr::{Client, Kind};
use pitcrew_domain::{media::*, *};
use pitcrew_engine::{live::*, *};
use pitcrew_store::{policy::*, *};
use pitcrew_upstream::{
    Auth, Endpoint, Error, Limits, Method, Request, Response, ResponseFuture, Transport, agent,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

#[derive(Clone, Default)]
struct Fake(Arc<Mutex<FakeState>>);
#[derive(Default)]
struct FakeState {
    bodies: BTreeMap<String, Value>,
    after_read: BTreeMap<String, Value>,
    advance_clock: Option<(String, Arc<AtomicU64>, u64)>,
    requests: Vec<Request>,
    mutation_error: Option<Error>,
    read_error: Option<Error>,
    reset_after_read: Option<(String, Store, String)>,
    accepted_marker: Option<std::path::PathBuf>,
}
impl Fake {
    fn set(&self, path: &str, value: Value) {
        self.0.lock().unwrap().bodies.insert(path.into(), value);
    }
    fn writes(&self) -> usize {
        self.0
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|r| r.method != Method::Get)
            .count()
    }
}
impl Transport for Fake {
    fn send(&self, request: Request) -> ResponseFuture<'_> {
        let mut state = self.0.lock().unwrap();
        let path = if request.url.path() == "/v1/evidence" {
            request
                .url
                .query_pairs()
                .find(|(key, _)| key == "path")
                .unwrap()
                .1
                .to_string()
        } else {
            request.url.path().into()
        };
        let body = state
            .bodies
            .get(&path)
            .cloned()
            .unwrap_or_else(|| panic!("unexpected request {path}"));
        if let Some(next) = state.after_read.remove(&path) {
            state.bodies.insert(path.clone(), next);
        }
        if let Some((trigger, clock, now)) = &state.advance_clock
            && *trigger == path
        {
            clock.store(*now, Ordering::SeqCst);
        }
        let error = if request.method == Method::Get {
            state.read_error
        } else {
            if let Some(path) = &state.accepted_marker {
                std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(path)
                    .expect("a second mutation reached the fake arr")
                    .sync_all()
                    .unwrap();
            }
            state.mutation_error
        };
        let reset = state
            .reset_after_read
            .clone()
            .filter(|(trigger, _, _)| *trigger == path && request.method == Method::Get);
        state.requests.push(request);
        Box::pin(async move {
            if let Some((_, store, client)) = reset {
                store
                    .reset_capabilities(
                        &client,
                        &AdminAction {
                            actor: "admin-one".into(),
                            role: PolicyRole::Admin,
                            note: "revoke capabilities during preflight".into(),
                        },
                        10,
                    )
                    .await
                    .unwrap();
            }
            match error {
                Some(error) => Err(error),
                None => Ok(Response {
                    status: 200,
                    body: serde_json::to_vec(&body).unwrap(),
                }),
            }
        })
    }
}

const ROUTER_CHILD: &str = "pitcrew-owned-router-process-v1";

#[test]
fn router_process_child() {
    if std::env::var("PITCREW_ROUTER_CHILD").as_deref() != Ok(ROUTER_CHILD) {
        return;
    }
    let root = std::path::PathBuf::from(std::env::var_os("PITCREW_ROUTER_ROOT").unwrap());
    assert_eq!(
        std::fs::read_to_string(root.join("owned.marker")).unwrap(),
        ROUTER_CHILD
    );
    let arr = Fake::default();
    let agent = Fake::default();
    arr.0.lock().unwrap().bodies =
        serde_json::from_slice(&std::fs::read(root.join("arr.json")).unwrap()).unwrap();
    agent.0.lock().unwrap().bodies =
        serde_json::from_slice(&std::fs::read(root.join("agent.json")).unwrap()).unwrap();
    arr.0.lock().unwrap().accepted_marker = Some(root.join("accepted"));
    let certificate = certificate(Kind::Sonarr);
    let gate = CertificationGate::from_files(
        certificate,
        true,
        &root.join("certificate.json"),
        &root.join("acknowledgement.json"),
    )
    .unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let store = Store::open(root.join("db")).await.unwrap();
            let lease = store.acquire_lease(1, "child", 11, 20).await.unwrap();
            let router = Router::new(
                store.clone(),
                Client::new(endpoint(false), arr, Kind::Sonarr),
                agent::Client::new(endpoint(true), agent),
                Fallback,
                gate,
                Arc::new(|| 11),
                100,
            )
            .unwrap();
            let worker = Worker::new(
                Some(store),
                router,
                Options {
                    mode: Mode::Live,
                    acknowledge_live_mutations: true,
                    call_timeout: Duration::from_secs(1),
                },
            )
            .unwrap();
            worker
                .drive_with(&lease, 11, |boundary| {
                    let root = &root;
                    async move {
                        if boundary == Boundary::CallAfter {
                            std::fs::write(root.join("call-returned"), "accepted").unwrap();
                            std::future::pending::<()>().await;
                        }
                    }
                })
                .await
                .unwrap();
        });
    panic!("child did not reach durable effect boundary");
}

#[tokio::test]
async fn abrupt_os_process_death_after_arr_acceptance_never_repeats_write() {
    let mut h = Harness::new(Kind::Sonarr).await;
    let intent = h.intent().await;
    bind(&h.store, &intent, &h.route, 10).await.unwrap();
    drop(h.router()); // Materializes the two explicitly required gate files.
    std::fs::write(h.temp.path().join("owned.marker"), ROUTER_CHILD).unwrap();
    std::fs::write(
        h.temp.path().join("arr.json"),
        serde_json::to_vec(&h.arr.0.lock().unwrap().bodies).unwrap(),
    )
    .unwrap();
    std::fs::write(
        h.temp.path().join("agent.json"),
        serde_json::to_vec(&h.agent.0.lock().unwrap().bodies).unwrap(),
    )
    .unwrap();
    h.store.release_lease(&h.lease, 10).await.unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "router_process_child", "--nocapture"])
        .env("PITCREW_ROUTER_CHILD", ROUTER_CHILD)
        .env("PITCREW_ROUTER_ROOT", h.temp.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("router child exited before the crash boundary: {status}");
        }
        if h.temp.path().join("call-returned").exists() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("router child hung");
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
    assert!(h.temp.path().join("accepted").exists());
    h.store = Store::open(h.temp.path().join("db")).await.unwrap();
    assert_eq!(
        h.store.outbox(1).await.unwrap()[0].status,
        OutboxStatus::Started
    );
    h.lease = h
        .store
        .acquire_lease(1, "restarted", 32, 1000)
        .await
        .unwrap();
    h.clock.store(32, Ordering::SeqCst);
    h.arr.0.lock().unwrap().accepted_marker = Some(h.temp.path().join("accepted"));
    let worker = Worker::new(
        Some(h.store.clone()),
        h.router(),
        Options {
            mode: Mode::Live,
            acknowledge_live_mutations: true,
            call_timeout: Duration::from_secs(1),
        },
    )
    .unwrap();
    worker.drive(&h.lease, 32).await.unwrap();
    let intent = h.intent().await;
    bind(&h.store, &intent, &h.route, 32).await.unwrap();
    worker.drive(&h.lease, 32).await.unwrap();
    assert_eq!(
        h.store.load_job(1).await.unwrap().job.state(),
        State::Downloading
    );
    assert_eq!(h.arr.writes(), 0);
}
#[derive(Clone)]
struct Fallback;
impl Upstream for Fallback {
    fn read<'a>(&'a self, _: &'a Intent) -> CallFuture<'a> {
        Box::pin(async { Ok(Outcome::Uncertain) })
    }
    fn write<'a>(&'a self, _: &'a Intent) -> CallFuture<'a> {
        Box::pin(async { Ok(Outcome::Delivered) })
    }
}
fn endpoint(agent: bool) -> Endpoint {
    Endpoint::new(
        "https://synthetic.invalid/",
        if agent {
            Auth::bearer("synthetic").unwrap()
        } else {
            Auth::arr("synthetic").unwrap()
        },
        Limits::default(),
    )
    .unwrap()
}
fn certificate(kind: Kind) -> Certification {
    Certification {
        id: "contract-1".into(),
        client: "client-one".into(),
        kind,
        version: match kind {
            Kind::Sonarr => "4.0.19.2979",
            Kind::Radarr => "6.3.0.10514",
        }
        .into(),
        fixture_digest: "sha256-synthetic-contract".into(),
        certified_at: 1,
        expires_at: 900,
    }
}
fn gate(
    temp: &tempfile::TempDir,
    configured: Certification,
    record: &Certification,
    ack: &OwnerAcknowledgement,
    enabled: bool,
) -> CertificationGate {
    let record_path = temp.path().join("certificate.json");
    let ack_path = temp.path().join("acknowledgement.json");
    std::fs::write(&record_path, serde_json::to_vec(record).unwrap()).unwrap();
    std::fs::write(&ack_path, serde_json::to_vec(ack).unwrap()).unwrap();
    CertificationGate::from_files(configured, enabled, &record_path, &ack_path).unwrap()
}
fn evidence(file: FileIdentity, good: bool) -> Value {
    serde_json::to_value(MediaEvidence {
        file,
        observed_at_unix_ms: 10000,
        symlinks: None,
        mount: None,
        reads: vec![],
        header: None,
        plex_failures: vec![],
        decode: Some(MediaProbe {
            outcome: if good {
                ProbeOutcome::Passed
            } else {
                ProbeOutcome::Failed
            },
            duration_ms: Some(300000),
            elapsed_ms: 1,
            timeout_ms: 1000,
            faults: vec![],
            streams: vec![],
            tested_ranges: vec![DecodeRange {
                start_ms: 0,
                end_ms: Some(10000),
            }],
        }),
    })
    .unwrap()
}
fn source() -> FileIdentity {
    FileIdentity {
        id: 502,
        revision: 2,
    }
}
fn fingerprint(id: u64, agent: FileIdentity) -> IdentityFingerprint {
    IdentityFingerprint {
        arr_file_id: id,
        size: 100000000,
        date_added: "2026-01-01T00:00:00Z".into(),
        quality: json!({"quality":{"id":1}}).to_string(),
        agent,
    }
}
fn media_file(id: u64) -> Value {
    json!({"id":id,"path":"/library/synthetic.mkv","size":100000000,"dateAdded":"2026-01-01T00:00:00Z","quality":{"quality":{"id":1}}})
}
fn page(records: Value) -> Value {
    json!({"page":1,"pageSize":100,"totalRecords":records.as_array().unwrap().len(),"records":records})
}
struct Harness {
    temp: tempfile::TempDir,
    store: Store,
    lease: Lease,
    arr: Fake,
    agent: Fake,
    clock: Arc<AtomicU64>,
    cert: Certification,
    route: Route,
}
impl Harness {
    async fn new(kind: Kind) -> Self {
        Self::with_constraints(kind, Constraints::default()).await
    }
    async fn with_constraints(kind: Kind, constraints: Constraints) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("db")).await.unwrap();
        let cert = certificate(kind);
        store
            .observe_client_version(&cert.client, &cert.version, 1)
            .await
            .unwrap();
        store
            .record_capabilities(
                &cert.client,
                &cert.version,
                &ClientCapabilities {
                    external_queue_actor: true,
                    held_category: None,
                },
                &json!({"fixture":"synthetic"}),
                1,
            )
            .await
            .unwrap();
        let agent_file = FileIdentity {
            id: 501,
            revision: 1,
        };
        let original = store
            .register_file(
                &cert.client,
                "resource-one",
                &fingerprint(11, agent_file),
                1,
            )
            .await
            .unwrap();
        store
            .create_job(
                &NewJob {
                    id: 1,
                    viewer: "viewer-one".into(),
                    resource: "resource-one".into(),
                    library: 3,
                    original,
                    constraints,
                    policy: Policy {
                        max_grabs: 2,
                        stage_timeout: 200,
                        total_timeout: 1000,
                    },
                    timeouts: StageTimeouts {
                        download: 200,
                        import: 200,
                        decode: 200,
                        verify_retry: 180,
                    },
                    created_at: 1,
                },
                "job",
                BudgetLimits {
                    global_hour: 10,
                    global_day: 10,
                    viewer_hour: 10,
                    viewer_day: 10,
                },
            )
            .await
            .unwrap();
        let lease = store.acquire_lease(1, "worker", 1, 1000).await.unwrap();
        let arr = Fake::default();
        let agent = Fake::default();
        arr.set("/api/v3/system/status", json!({"version":cert.version}));
        let (media, file) = match kind {
            Kind::Sonarr => ("episode", "episodefile"),
            Kind::Radarr => ("movie", "moviefile"),
        };
        arr.set(
            &format!("/api/v3/{media}/21"),
            json!({"episodeFileId":11,"movieFileId":11}),
        );
        arr.set(&format!("/api/v3/{file}/11"), media_file(11));
        arr.set("/api/v3/release",json!([{"guid":"release-guid","indexerId":7,"approved":true,"rejected":false,"downloadAllowed":true}]));
        arr.set("/api/v3/queue",page(json!([{"id":31,"downloadId":"download-one","episodeId":21,"movieId":21,"seriesId":41,"status":"completed","outputPath":"/downloads"}])));
        arr.set("/api/v3/queue/31", Value::Null);
        arr.set("/api/v3/manualimport",json!([{"id":51,"path":"/downloads/synthetic.mkv","downloadId":"download-one","size":100000000,"series":{"id":41},"movie":{"id":21},"episodes":[{"id":21}],"quality":{"quality":{"id":1}},"languages":[{"id":1}],"releaseGroup":"Synthetic","rejections":[]}]));
        arr.set("/api/v3/command", json!({"id":61,"status":"queued"}));
        agent.set("/library/synthetic.mkv", evidence(agent_file, true));
        agent.set("/downloads/synthetic.mkv", evidence(source(), true));
        let h = Self {
            temp,
            store,
            lease,
            arr,
            agent,
            clock: Arc::new(AtomicU64::new(10)),
            cert,
            route: Route {
                media_id: 21,
                series_id: Some(41),
                original_arr_file: 11,
                release: Some(ReleaseRoute {
                    release: 71,
                    guid: "release-guid".into(),
                    indexer: 7,
                }),
                download: Some(DownloadRoute {
                    download: 81,
                    external_id: "download-one".into(),
                    queue_id: 31,
                    path: "/downloads/synthetic.mkv".into(),
                }),
            },
        };
        h.apply(Event::SelectRelease {
            release: 71,
            access: h.access(),
        })
        .await;
        h
    }
    fn access(&self) -> Access {
        Access::Granted {
            library: 3,
            expires_at: 800,
        }
    }
    fn router(&self) -> Router<Fake, Fake, Fallback> {
        let clock = self.clock.clone();
        Router::new(
            self.store.clone(),
            Client::new(endpoint(false), self.arr.clone(), self.cert.kind),
            agent::Client::new(endpoint(true), self.agent.clone()),
            Fallback,
            gate(
                &self.temp,
                self.cert.clone(),
                &self.cert,
                &OwnerAcknowledgement {
                    certification: self.cert.clone(),
                    acknowledge_live_mutations: true,
                },
                true,
            ),
            Arc::new(move || clock.load(Ordering::SeqCst)),
            100,
        )
        .unwrap()
    }
    async fn apply(&self, event: Event) {
        let job = self.store.load_job(1).await.unwrap();
        self.store
            .apply_event(
                &self.lease,
                job.revision,
                &format!("event-{}", job.revision),
                event,
                self.clock.load(Ordering::SeqCst),
            )
            .await
            .unwrap();
    }
    async fn observe(&self, observation: Observation) {
        let intent = self
            .store
            .load_job(1)
            .await
            .unwrap()
            .job
            .expected_observation()
            .unwrap();
        self.apply(Event::Observed {
            intent,
            observation,
        })
        .await;
    }
    async fn intent(&self) -> Intent {
        let expected = self
            .store
            .load_job(1)
            .await
            .unwrap()
            .job
            .expected_observation()
            .unwrap();
        self.store
            .outbox(1)
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.intent.sequence == expected.sequence)
            .unwrap()
            .intent
    }
    async fn prepare(&self) -> Intent {
        let intent = self.intent().await;
        bind(&self.store, &intent, &self.route, 10).await.unwrap();
        let job = self.store.load_job(1).await.unwrap();
        self.store
            .start_intent(
                &self.lease,
                job.job.expected_observation().unwrap(),
                job.revision,
                10,
            )
            .await
            .unwrap();
        intent
    }
    async fn downloaded(&self) {
        self.observe(Observation::GrabAccepted { download: 81 })
            .await;
        self.observe(Observation::DownloadCompleted {
            download: 81,
            file: source(),
        })
        .await;
    }
    async fn importing(&self) {
        self.downloaded().await;
        self.observe(Observation::DownloadDecoded {
            file: source(),
            good: true,
        })
        .await;
        self.apply(Event::RequestImport {
            access: self.access(),
        })
        .await;
    }
    async fn corrupt_payload(&self, intent: &Intent) {
        let connection = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(self.temp.path().join("db")),
        )
        .await
        .unwrap();
        assert_eq!(
            sqlx::query("UPDATE outbox SET intent=? WHERE job=? AND sequence=?")
                .bind(serde_json::to_string(intent).unwrap())
                .bind(i64::try_from(intent.job).unwrap())
                .bind(i64::try_from(intent.sequence).unwrap())
                .execute(&connection)
                .await
                .unwrap()
                .rows_affected(),
            1
        );
        connection.close().await;
    }
}

#[tokio::test]
async fn worker_retries_uncertain_decode_after_reopening_store_without_media_writes() {
    let mut h = Harness::new(Kind::Sonarr).await;
    h.downloaded().await;
    let intent = h.intent().await;
    bind(&h.store, &intent, &h.route, 10).await.unwrap();
    let options = Options {
        mode: Mode::Live,
        acknowledge_live_mutations: true,
        call_timeout: Duration::from_secs(1),
    };
    h.agent.0.lock().unwrap().read_error = Some(Error::Timeout);
    Worker::new(Some(h.store.clone()), h.router(), options.clone())
        .unwrap()
        .drive(&h.lease, 10)
        .await
        .unwrap();
    assert_eq!(
        h.store.load_job(1).await.unwrap().job.state(),
        State::Downloaded
    );
    assert_eq!(
        h.store.outbox(1).await.unwrap().last().unwrap().status,
        OutboxStatus::Uncertain
    );
    h.store.release_lease(&h.lease, 11).await.unwrap();
    h.store = Store::open(h.temp.path().join("db")).await.unwrap();
    h.lease = h
        .store
        .acquire_lease(1, "restarted", 12, 1000)
        .await
        .unwrap();
    h.clock.store(12, Ordering::SeqCst);
    h.agent.0.lock().unwrap().read_error = None;
    let calls = h.agent.0.lock().unwrap().requests.len();
    let worker = Worker::new(Some(h.store.clone()), h.router(), options).unwrap();
    assert_eq!(worker.drive(&h.lease, 12).await.unwrap(), 1);
    assert_eq!(h.agent.0.lock().unwrap().requests.len(), calls + 1);
    assert_eq!(
        h.store.load_job(1).await.unwrap().job.state(),
        State::DownloadVerified
    );
    assert_eq!(
        h.store.outbox(1).await.unwrap().last().unwrap().status,
        OutboxStatus::Completed
    );
    assert_eq!(worker.drive(&h.lease, 12).await.unwrap(), 0);
    assert_eq!(h.arr.writes(), 0);
}

#[tokio::test]
async fn corrupted_outbox_reservation_cannot_spend_unreserved_budget() {
    for reservation in [0, 2] {
        let h = Harness::new(Kind::Sonarr).await;
        let mut intent = h.prepare().await;
        assert_eq!(h.store.load_job(1).await.unwrap().job.reserved_budget(), 1);
        intent.effect = Effect::Grab {
            release: 71,
            reservation,
        };
        // Fault injection: the event journal still proves one reserved attempt,
        // while the serialized outbox payload disagrees. Use only this owned DB.
        h.corrupt_payload(&intent).await;
        assert_eq!(h.router().write(&intent).await, Err(CallError::Unavailable));
        assert_eq!(h.arr.writes(), 0);
    }
}

#[tokio::test]
async fn corrupted_outbox_cannot_disable_either_blocklist_safety_flag() {
    for (blocklist, skip_redownload) in [(false, true), (true, false), (false, false)] {
        let h = Harness::new(Kind::Sonarr).await;
        h.downloaded().await;
        h.observe(Observation::DownloadDecoded {
            file: source(),
            good: false,
        })
        .await;
        h.apply(Event::RejectDownload { access: h.access() }).await;
        let mut intent = h.prepare().await;
        intent.effect = Effect::RemoveRejectedDownload {
            download: 81,
            blocklist,
            skip_redownload,
        };
        h.corrupt_payload(&intent).await;
        assert_eq!(h.router().write(&intent).await, Err(CallError::Unavailable));
        assert_eq!(h.arr.writes(), 0);
    }
}

#[tokio::test]
async fn corrupted_outbox_original_cannot_replace_an_unrelated_library_file() {
    let h = Harness::new(Kind::Sonarr).await;
    h.importing().await;
    let mut intent = h.prepare().await;
    let Effect::ImportVerifiedDownload { original, .. } = &mut intent.effect else {
        panic!("expected verified import");
    };
    original.revision += 1;
    h.corrupt_payload(&intent).await;
    assert_eq!(h.router().write(&intent).await, Err(CallError::Unavailable));
    assert_eq!(h.arr.writes(), 0);
}

#[tokio::test]
async fn sequential_intents_keep_routes_as_download_identity_becomes_known() {
    let mut h = Harness::new(Kind::Sonarr).await;
    let download = h.route.download.take();
    let grab = h.prepare().await;
    assert_eq!(
        h.router().write(&grab).await.unwrap(),
        Outcome::Observed(Observation::GrabAmbiguous)
    );
    h.observe(Observation::GrabAmbiguous).await;
    // Trusted intake learns the download identity after the grab. Persisting
    // the reconciliation route must not collide with the prior intent's route.
    h.route.download = download;
    let reconcile = h.prepare().await;
    assert_ne!(grab.sequence, reconcile.sequence);
    assert_eq!(
        h.router().read(&reconcile).await.unwrap(),
        Outcome::Observed(Observation::GrabReconciled(GrabResolution::Accepted {
            download: 81
        }))
    );
    assert_eq!(h.arr.writes(), 1);
}

#[tokio::test]
async fn uncertain_mutation_remains_ineligible_for_router_resend() {
    let h = Harness::new(Kind::Sonarr).await;
    let intent = h.prepare().await;
    h.store
        .finish_intent(
            &h.lease,
            IntentKey {
                job: intent.job,
                sequence: intent.sequence,
            },
            OutboxStatus::Uncertain,
            10,
        )
        .await
        .unwrap();
    assert_eq!(h.router().write(&intent).await, Err(CallError::Unavailable));
    assert!(h.arr.0.lock().unwrap().requests.is_empty());
}

#[tokio::test]
async fn notification_requires_local_certification_but_not_arr_availability() {
    let h = Harness::new(Kind::Sonarr).await;
    h.arr.0.lock().unwrap().read_error = Some(Error::Timeout);
    let intent = Intent {
        job: 1,
        sequence: 500,
        effect: Effect::NotifyAdmin {
            failure: Failure::Upstream,
        },
    };
    for (enabled, acknowledged, now, allowed) in [
        (false, true, 10, false),
        (true, false, 10, false),
        (true, true, 2000, false),
        (true, true, 10, true),
    ] {
        let router = Router::new(
            h.store.clone(),
            Client::new(endpoint(false), h.arr.clone(), Kind::Sonarr),
            agent::Client::new(endpoint(true), h.agent.clone()),
            Fallback,
            gate(
                &h.temp,
                h.cert.clone(),
                &h.cert,
                &OwnerAcknowledgement {
                    certification: h.cert.clone(),
                    acknowledge_live_mutations: acknowledged,
                },
                enabled,
            ),
            Arc::new(move || now),
            100,
        )
        .unwrap();
        assert_eq!(
            router.write(&intent).await,
            if allowed {
                Ok(Outcome::Delivered)
            } else {
                Err(CallError::Unavailable)
            }
        );
    }
    assert!(h.arr.0.lock().unwrap().requests.is_empty());
    assert!(h.agent.0.lock().unwrap().requests.is_empty());
}

#[tokio::test]
async fn fresh_import_retries_after_three_minutes_and_restart_without_another_grab() {
    let mut h = Harness::new(Kind::Sonarr).await;
    h.importing().await;
    h.observe(Observation::ImportAccepted).await;
    let file = h
        .store
        .register_file(
            &h.cert.client,
            "resource-one",
            &fingerprint(12, source()),
            10,
        )
        .await
        .unwrap();
    h.arr.set("/api/v3/episode/21", json!({"episodeFileId":12}));
    h.arr.set("/api/v3/episodefile/12", media_file(12));
    h.agent
        .set("/library/synthetic.mkv", evidence(source(), false));
    h.observe(Observation::ImportCompleted { download: 81, file })
        .await;
    let first = h.intent().await;
    bind(&h.store, &first, &h.route, 10).await.unwrap();
    let options = Options {
        mode: Mode::Live,
        acknowledge_live_mutations: true,
        call_timeout: Duration::from_secs(1),
    };
    Worker::new(Some(h.store.clone()), h.router(), options.clone())
        .unwrap()
        .drive(&h.lease, 10)
        .await
        .unwrap();
    let job = h.store.load_job(1).await.unwrap();
    assert_eq!(job.job.state(), State::VerifyRetry);
    assert_eq!(job.job.deadline(), Some(190));
    h.store.release_lease(&h.lease, 11).await.unwrap();
    h.store = Store::open(h.temp.path().join("db")).await.unwrap();
    h.lease = h
        .store
        .acquire_lease(1, "restarted", 12, 1000)
        .await
        .unwrap();
    h.clock.store(189, Ordering::SeqCst);
    let calls = h.agent.0.lock().unwrap().requests.len();
    Worker::new(Some(h.store.clone()), h.router(), options.clone())
        .unwrap()
        .drive(&h.lease, 189)
        .await
        .unwrap();
    assert_eq!(h.agent.0.lock().unwrap().requests.len(), calls);
    assert_eq!(
        h.store.load_job(1).await.unwrap().job.state(),
        State::VerifyRetry
    );
    h.clock.store(190, Ordering::SeqCst);
    h.apply(Event::Tick).await;
    let retry = h.intent().await;
    assert_ne!(first.sequence, retry.sequence);
    assert_eq!(retry.effect, Effect::DecodeImported { file });
    bind(&h.store, &retry, &h.route, 190).await.unwrap();
    let mut fresh = evidence(source(), true);
    fresh["observed_at_unix_ms"] = json!(190000);
    h.agent.set("/library/synthetic.mkv", fresh);
    Worker::new(Some(h.store.clone()), h.router(), options)
        .unwrap()
        .drive(&h.lease, 190)
        .await
        .unwrap();
    assert_eq!(
        h.intent().await.effect,
        Effect::CheckTarget {
            file,
            constraints: Constraints::default()
        }
    );
    assert_eq!(h.arr.writes(), 0);
    assert_eq!(h.store.load_job(1).await.unwrap().job.reserved_budget(), 1);
}

#[tokio::test]
async fn real_grab_killed_after_acceptance_is_reconciled_once() {
    let mut h = Harness::new(Kind::Sonarr).await;
    let intent = h.intent().await;
    bind(&h.store, &intent, &h.route, 10).await.unwrap();
    let options = Options {
        mode: Mode::Live,
        acknowledge_live_mutations: true,
        call_timeout: Duration::from_secs(1),
    };
    let worker = Worker::new(Some(h.store.clone()), h.router(), options.clone()).unwrap();
    let lease = h.lease.clone();
    let task = tokio::spawn(async move {
        worker
            .drive_with(&lease, 10, |boundary| async move {
                assert_ne!(
                    boundary,
                    Boundary::CallAfter,
                    "simulated process kill after remote acceptance"
                );
            })
            .await
    });
    assert!(task.await.unwrap_err().is_panic());
    assert_eq!(h.arr.writes(), 1);
    assert_eq!(
        h.store.outbox(1).await.unwrap()[0].status,
        OutboxStatus::Started
    );
    h.store.release_lease(&h.lease, 11).await.unwrap();
    h.store = Store::open(h.temp.path().join("db")).await.unwrap();
    h.lease = h
        .store
        .acquire_lease(1, "restarted", 12, 1000)
        .await
        .unwrap();
    let worker = Worker::new(Some(h.store.clone()), h.router(), options).unwrap();
    worker.drive(&h.lease, 12).await.unwrap();
    assert_eq!(
        h.store.load_job(1).await.unwrap().job.state(),
        State::GrabReconciling
    );
    let intent = h.intent().await;
    bind(&h.store, &intent, &h.route, 12).await.unwrap();
    worker.drive(&h.lease, 12).await.unwrap();
    assert_eq!(
        h.store.load_job(1).await.unwrap().job.state(),
        State::Downloading
    );
    assert_eq!(h.arr.writes(), 1);
}

#[tokio::test]
async fn stale_future_and_incomplete_agent_evidence_never_claims_good() {
    for scenario in [
        "stale",
        "future",
        "no_decode",
        "timeout",
        "faults",
        "no_ranges",
    ] {
        let h = Harness::new(Kind::Sonarr).await;
        h.downloaded().await;
        let intent = h.prepare().await;
        let mut body = evidence(source(), true);
        match scenario {
            "stale" => {
                h.clock.store(111, Ordering::SeqCst);
                body["observed_at_unix_ms"] = json!(10000);
            }
            "future" => body["observed_at_unix_ms"] = json!(11000),
            "no_decode" => body["decode"] = Value::Null,
            "timeout" => body["decode"]["outcome"] = json!("TimedOut"),
            "faults" => body["decode"]["faults"] = json!(["DecodeError"]),
            "no_ranges" => body["decode"]["tested_ranges"] = json!([]),
            _ => unreachable!(),
        }
        h.agent.set("/downloads/synthetic.mkv", body);
        let result = h.router().read(&intent).await;
        if ["faults", "no_ranges"].contains(&scenario) {
            assert_eq!(
                result.unwrap(),
                Outcome::Observed(Observation::DownloadDecoded {
                    file: source(),
                    good: false
                }),
                "{scenario}"
            );
        } else {
            assert_eq!(result, Err(CallError::Unavailable), "{scenario}");
        }
    }
}

#[tokio::test]
async fn constructor_ledger_and_context_deny_untrusted_dispatch() {
    let h = Harness::new(Kind::Sonarr).await;
    for (kind, age) in [(Kind::Radarr, 100), (Kind::Sonarr, 0)] {
        assert!(
            Router::new(
                h.store.clone(),
                Client::new(endpoint(false), h.arr.clone(), kind),
                agent::Client::new(endpoint(true), h.agent.clone()),
                Fallback,
                gate(
                    &h.temp,
                    h.cert.clone(),
                    &h.cert,
                    &OwnerAcknowledgement {
                        certification: h.cert.clone(),
                        acknowledge_live_mutations: true
                    },
                    true
                ),
                Arc::new(|| 10),
                age
            )
            .is_err()
        );
    }
    let intent = h.intent().await;
    let job = h.store.load_job(1).await.unwrap();
    h.store
        .start_intent(
            &h.lease,
            job.job.expected_observation().unwrap(),
            job.revision,
            10,
        )
        .await
        .unwrap();
    h.store
        .ingest_observation(
            "untrusted",
            &format!("{}:{}", intent.job, intent.sequence),
            &serde_json::to_value(&h.route).unwrap(),
            10,
        )
        .await
        .unwrap();
    assert!(h.router().write(&intent).await.is_err());
    bind(&h.store, &intent, &h.route, 10).await.unwrap();
    h.store
        .reset_capabilities(
            &h.cert.client,
            &AdminAction {
                actor: "admin-one".into(),
                role: PolicyRole::Admin,
                note: "reset certification".into(),
            },
            10,
        )
        .await
        .unwrap();
    assert!(h.router().write(&intent).await.is_err());
    h.observe(Observation::GrabAccepted { download: 81 }).await;
    assert!(h.router().write(&intent).await.is_err());
    assert_eq!(h.arr.writes(), 0);
}

#[tokio::test]
async fn capability_reset_during_preflight_denies_the_final_send() {
    let h = Harness::new(Kind::Sonarr).await;
    let intent = h.prepare().await;
    h.arr.0.lock().unwrap().reset_after_read = Some((
        "/api/v3/release".into(),
        h.store.clone(),
        h.cert.client.clone(),
    ));
    assert_eq!(h.router().write(&intent).await, Err(CallError::Unavailable));
    assert!(
        h.store
            .capabilities(&h.cert.client, &h.cert.version)
            .await
            .unwrap()
            .unwrap()
            .capabilities
            .is_none()
    );
    assert_eq!(h.arr.writes(), 0);
}

#[tokio::test]
async fn mutation_rechecks_access_original_and_source_after_preflight() {
    let h = Harness::new(Kind::Sonarr).await;
    let intent = h.prepare().await;
    h.arr.0.lock().unwrap().advance_clock = Some(("/api/v3/release".into(), h.clock.clone(), 800));
    assert!(h.router().write(&intent).await.is_err());
    assert_eq!(h.arr.writes(), 0);
    let mut h = Harness::new(Kind::Sonarr).await;
    h.cert.expires_at = 20;
    let intent = h.prepare().await;
    h.arr.0.lock().unwrap().advance_clock = Some(("/api/v3/release".into(), h.clock.clone(), 20));
    assert!(h.router().write(&intent).await.is_err());
    assert_eq!(h.arr.writes(), 0);
    for changed in ["original", "source"] {
        let h = Harness::new(Kind::Sonarr).await;
        h.importing().await;
        let intent = h.prepare().await;
        if changed == "original" {
            h.arr
                .0
                .lock()
                .unwrap()
                .after_read
                .insert("/api/v3/episode/21".into(), json!({"episodeFileId":12}));
        } else {
            h.agent.0.lock().unwrap().after_read.insert(
                "/downloads/synthetic.mkv".into(),
                evidence(
                    FileIdentity {
                        id: 502,
                        revision: 3,
                    },
                    true,
                ),
            );
        }
        assert!(h.router().write(&intent).await.is_err(), "{changed}");
        assert_eq!(h.arr.writes(), 0);
    }
}

#[tokio::test]
async fn delegate_notification_target_and_refuse_superseded_started_effect() {
    let h = Harness::new(Kind::Sonarr).await;
    let intent = Intent {
        job: 1,
        sequence: 500,
        effect: Effect::NotifyAdmin {
            failure: Failure::Upstream,
        },
    };
    assert_eq!(h.router().write(&intent).await.unwrap(), Outcome::Delivered);
    let intent = Intent {
        job: 1,
        sequence: 501,
        effect: Effect::CheckTarget {
            file: source(),
            constraints: Constraints::default(),
        },
    };
    assert_eq!(h.router().read(&intent).await.unwrap(), Outcome::Uncertain);
    let intent = h.prepare().await;
    h.observe(Observation::GrabAccepted { download: 81 }).await;
    assert!(h.router().write(&intent).await.is_err());
    h.observe(Observation::DownloadCompleted {
        download: 81,
        file: source(),
    })
    .await;
    assert!(h.router().write(&intent).await.is_err());
    assert_eq!(h.arr.writes(), 0);
}

#[tokio::test]
async fn queue_and_release_unknown_fields_cannot_authorize_mutation() {
    for field in [
        "approved",
        "rejected",
        "downloadAllowed",
        "guid",
        "indexerId",
    ] {
        let h = Harness::new(Kind::Sonarr).await;
        let intent = h.prepare().await;
        let mut body = h.arr.0.lock().unwrap().bodies["/api/v3/release"].clone();
        body[0][field] = match field {
            "approved" | "downloadAllowed" => json!(false),
            "rejected" => json!(true),
            "guid" => json!("other"),
            _ => json!(9),
        };
        h.arr.set("/api/v3/release", body);
        assert!(h.router().write(&intent).await.is_err(), "{field}");
        assert_eq!(h.arr.writes(), 0);
    }
    for (kind, field, value) in [
        (Kind::Sonarr, "downloadId", json!("other")),
        (Kind::Sonarr, "episodeId", json!(22)),
        (Kind::Radarr, "movieId", json!(22)),
        (Kind::Sonarr, "status", json!("downloading")),
        (Kind::Sonarr, "outputPath", json!("")),
    ] {
        let h = Harness::new(kind).await;
        h.importing().await;
        let intent = h.prepare().await;
        let mut body = h.arr.0.lock().unwrap().bodies["/api/v3/queue"].clone();
        body["records"][0][field] = value;
        h.arr.set("/api/v3/queue", body);
        assert!(h.router().write(&intent).await.is_err(), "{field}");
        assert_eq!(h.arr.writes(), 0);
    }
    let h = Harness::new(Kind::Sonarr).await;
    h.importing().await;
    let intent = h.prepare().await;
    let mut body = h.arr.0.lock().unwrap().bodies["/api/v3/queue"].clone();
    let first = body["records"][0].clone();
    body["records"].as_array_mut().unwrap().push(first);
    h.arr.set("/api/v3/queue", body);
    assert!(h.router().write(&intent).await.is_err());
    assert_eq!(h.arr.writes(), 0);
}

#[tokio::test]
async fn reconciliation_never_infers_absence_or_import_from_missing_correlations() {
    let h = Harness::new(Kind::Sonarr).await;
    h.observe(Observation::GrabAmbiguous).await;
    let intent = h.prepare().await;
    h.arr.set("/api/v3/queue", page(json!([])));
    assert_eq!(
        h.router().read(&intent).await.unwrap(),
        Outcome::Observed(Observation::GrabReconciled(GrabResolution::Unknown))
    );
    let h = Harness::new(Kind::Sonarr).await;
    h.downloaded().await;
    h.observe(Observation::DownloadDecoded {
        file: source(),
        good: false,
    })
    .await;
    h.apply(Event::RejectDownload { access: h.access() }).await;
    h.observe(Observation::DeleteAmbiguous).await;
    let intent = h.prepare().await;
    assert_eq!(
        h.router().read(&intent).await.unwrap(),
        Outcome::Observed(Observation::DeleteReconciled(DeleteResolution::Present))
    );
    h.arr.set("/api/v3/queue", page(json!([])));
    assert_eq!(
        h.router().read(&intent).await.unwrap(),
        Outcome::Observed(Observation::DeleteReconciled(DeleteResolution::Unknown))
    );
    for field in [
        "date",
        "eventType",
        "downloadId",
        "episodeId",
        "importedPath",
        "file_id",
    ] {
        let h = Harness::new(Kind::Sonarr).await;
        h.importing().await;
        h.observe(Observation::ImportAmbiguous).await;
        let intent = h.prepare().await;
        h.arr.set("/api/v3/episode/21", json!({"episodeFileId":12}));
        let mut file = media_file(12);
        if field == "file_id" {
            file["id"] = json!(999);
        }
        h.arr.set("/api/v3/episodefile/12", file);
        let mut event = json!({"id":91,"eventType":"downloadFolderImported","date":"2026-01-01T00:00:00Z","downloadId":"download-one","episodeId":21,"data":{"importedPath":"/library/synthetic.mkv"}});
        match field {
            "importedPath" => event["data"][field] = json!("/library/other.mkv"),
            "episodeId" => event[field] = json!(22),
            "file_id" => {}
            _ => event[field] = json!("other"),
        }
        h.arr.set("/api/v3/history", page(json!([event])));
        let result = h.router().read(&intent).await;
        if field == "file_id" {
            assert!(result.is_err());
        } else {
            assert_eq!(
                result.unwrap(),
                Outcome::Observed(Observation::ImportReconciled(ImportResolution::Unknown)),
                "{field}"
            );
        }
    }
}

#[test]
fn certification_requires_matching_files_exact_version_ack_and_freshness() {
    let temp = tempfile::tempdir().unwrap();
    for kind in [Kind::Sonarr, Kind::Radarr] {
        let cert = certificate(kind);
        let ack = OwnerAcknowledgement {
            certification: cert.clone(),
            acknowledge_live_mutations: true,
        };
        let valid = gate(&temp, cert.clone(), &cert, &ack, true);
        assert!(valid.authorize(&cert.version, 1).is_ok());
        assert!(valid.authorize(&cert.version, 899).is_ok());
        for now in [0, 900, 901] {
            assert!(valid.authorize(&cert.version, now).is_err());
        }
        assert!(valid.authorize("7.1.0", 10).is_err());
        assert!(
            gate(&temp, cert.clone(), &cert, &ack, false)
                .authorize(&cert.version, 10)
                .is_err()
        );
        let mut altered = cert.clone();
        altered.id.push('x');
        assert!(
            gate(&temp, altered, &cert, &ack, true)
                .authorize(&cert.version, 10)
                .is_err()
        );
        let mut ack2 = ack.clone();
        ack2.acknowledge_live_mutations = false;
        assert!(
            gate(&temp, cert.clone(), &cert, &ack2, true)
                .authorize(&cert.version, 10)
                .is_err()
        );
        ack2 = ack.clone();
        ack2.certification.id.push('x');
        assert!(
            gate(&temp, cert.clone(), &cert, &ack2, true)
                .authorize(&cert.version, 10)
                .is_err()
        );
        for field in ["id", "client", "fixture_digest", "version"] {
            let mut raw = serde_json::to_value(&cert).unwrap();
            raw[field] = json!("");
            let broken: Certification = serde_json::from_value(raw).unwrap();
            let ack = OwnerAcknowledgement {
                certification: broken.clone(),
                acknowledge_live_mutations: true,
            };
            assert!(
                gate(&temp, broken.clone(), &broken, &ack, true)
                    .authorize(&broken.version, 10)
                    .is_err()
            );
        }
        let mut unsupported = cert.clone();
        unsupported.version = concat!("4.", "0.20.1").into();
        let ack = OwnerAcknowledgement {
            certification: unsupported.clone(),
            acknowledge_live_mutations: true,
        };
        assert!(
            gate(&temp, unsupported.clone(), &unsupported, &ack, true)
                .authorize(&unsupported.version, 10)
                .is_err()
        );
    }
    assert!(
        CertificationGate::from_files(
            certificate(Kind::Sonarr),
            true,
            &temp.path().join("missing"),
            &temp.path().join("missing")
        )
        .is_err()
    );
}

#[tokio::test]
async fn committed_grab_uses_real_client_and_does_not_turn_http_200_into_receipt() {
    for kind in [Kind::Sonarr, Kind::Radarr] {
        let h = Harness::new(kind).await;
        let intent = h.prepare().await;
        assert_eq!(
            h.router().write(&intent).await.unwrap(),
            Outcome::Observed(Observation::GrabAmbiguous)
        );
        let state = h.arr.0.lock().unwrap();
        let request = state
            .requests
            .iter()
            .find(|r| r.method == Method::Post)
            .unwrap();
        let key = match kind {
            Kind::Sonarr => "episodeId",
            Kind::Radarr => "movieId",
        };
        assert_eq!(request.body.as_ref().unwrap()[key], 21);
        assert_eq!(request.body.as_ref().unwrap()["guid"], "release-guid");
        assert_eq!(request.body.as_ref().unwrap()["indexerId"], 7);
    }
}

#[tokio::test]
async fn no_unstarted_or_unbound_or_stale_identity_mutation() {
    let h = Harness::new(Kind::Sonarr).await;
    let intent = h.intent().await;
    assert!(h.router().write(&intent).await.is_err());
    let mut nonexistent = intent.clone();
    nonexistent.sequence += 1;
    assert!(bind(&h.store, &nonexistent, &h.route, 10).await.is_err());
    let job = h.store.load_job(1).await.unwrap();
    h.store
        .start_intent(
            &h.lease,
            job.job.expected_observation().unwrap(),
            job.revision,
            10,
        )
        .await
        .unwrap();
    assert!(h.router().write(&intent).await.is_err());
    bind(&h.store, &intent, &h.route, 10).await.unwrap();
    h.agent.set(
        "/library/synthetic.mkv",
        evidence(
            FileIdentity {
                id: 501,
                revision: 99,
            },
            true,
        ),
    );
    assert!(h.router().write(&intent).await.is_err());
    h.agent.set(
        "/library/synthetic.mkv",
        evidence(
            FileIdentity {
                id: 501,
                revision: 1,
            },
            true,
        ),
    );
    h.clock.store(102, Ordering::SeqCst);
    assert!(h.router().write(&intent).await.is_err());
    assert_eq!(h.arr.writes(), 0);
}

#[tokio::test]
async fn release_exclusions_and_version_changes_deny_grabs() {
    let h = Harness::new(Kind::Sonarr).await;
    let intent = h.prepare().await;
    h.store
        .exclude_release(
            &NewExclusion {
                client: h.cert.client.clone(),
                release: 71,
                identity: ReleaseIdentity::default(),
                kind: ExclusionKind::DeadRelease,
                reason: "synthetic failure".into(),
                evidence: json!({}),
                pinned: false,
                created_at: 10,
            },
            "exclude",
        )
        .await
        .unwrap();
    assert!(h.router().write(&intent).await.is_err());
    assert_eq!(h.arr.writes(), 0);
    let h = Harness::new(Kind::Sonarr).await;
    let intent = h.prepare().await;
    h.arr.set(
        "/api/v3/system/status",
        json!({"version":concat!("4.", "0.20.1")}),
    );
    assert!(h.router().write(&intent).await.is_err());
    assert!(
        h.store
            .capabilities(&h.cert.client, &h.cert.version)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(h.arr.writes(), 0);
}

#[tokio::test]
async fn exact_healthy_sample_or_unknown_size_never_authorizes_replacement() {
    for (filename, size) in [
        ("synthetic.mkv", Value::Null),
        ("synthetic.mkv", json!(0)),
        (
            "synthetic.mkv",
            json!(pitcrew_arbiter::MIN_EPISODE_BYTES - 1),
        ),
        ("synthetic.sample.mkv", json!(100000000)),
        ("synthetic.NCOP.mkv", json!(100000000)),
        ("synthetic.NCED.mkv", json!(100000000)),
    ] {
        let mut h = Harness::new(Kind::Sonarr).await;
        h.importing().await;
        let path = format!("/downloads/{filename}");
        h.route.download.as_mut().unwrap().path = path.clone();
        h.agent.set(&path, evidence(source(), true));
        let mut candidate = h.arr.0.lock().unwrap().bodies["/api/v3/manualimport"].clone();
        candidate[0]["path"] = json!(path);
        candidate[0]["size"] = size;
        h.arr.set("/api/v3/manualimport", candidate);
        let intent = h.prepare().await;
        assert_eq!(
            h.router().write(&intent).await,
            Err(CallError::Unavailable),
            "{filename}"
        );
        assert_eq!(h.arr.writes(), 0);
    }
}

#[tokio::test]
async fn sample_parent_directory_does_not_hide_a_complete_exact_candidate() {
    for kind in [Kind::Sonarr, Kind::Radarr] {
        let mut h = Harness::new(kind).await;
        h.importing().await;
        let path = "/downloads/sample/synthetic.mkv";
        h.route.download.as_mut().unwrap().path = path.into();
        h.agent.set(path, evidence(source(), true));
        let mut candidate = h.arr.0.lock().unwrap().bodies["/api/v3/manualimport"].clone();
        candidate[0]["path"] = json!(path);
        candidate[0]["size"] = json!(pitcrew_arbiter::MIN_EPISODE_BYTES);
        h.arr.set("/api/v3/manualimport", candidate);
        let intent = h.prepare().await;
        assert_eq!(
            h.router().write(&intent).await,
            Ok(Outcome::Observed(Observation::ImportAccepted))
        );
        assert_eq!(h.arr.writes(), 1);
    }
}

#[tokio::test]
async fn decoded_source_and_arbiter_exact_candidate_gate_manual_import() {
    for kind in [Kind::Sonarr, Kind::Radarr] {
        let h = Harness::new(kind).await;
        h.importing().await;
        let intent = h.prepare().await;
        assert_eq!(
            h.router().write(&intent).await.unwrap(),
            Outcome::Observed(Observation::ImportAccepted)
        );
        let state = h.arr.0.lock().unwrap();
        let request = state
            .requests
            .iter()
            .find(|r| r.method == Method::Post)
            .unwrap();
        let body = request.body.as_ref().unwrap();
        assert_eq!(body["name"], "ManualImport");
        assert_eq!(body["files"][0]["path"], "/downloads/synthetic.mkv");
        assert_eq!(body["files"][0]["downloadId"], "download-one");
        assert_eq!(
            state
                .requests
                .iter()
                .filter(|r| r.method == Method::Delete)
                .count(),
            0
        );
    }
    for bad in [
        "identity",
        "decode",
        "target",
        "rejections",
        "multiple",
        "queue_path",
        "queue_id",
        "candidate_path",
        "candidate_download_id",
    ] {
        let h = Harness::new(Kind::Sonarr).await;
        h.importing().await;
        let intent = h.prepare().await;
        match bad {
            "identity" => h.agent.set(
                "/downloads/synthetic.mkv",
                evidence(
                    FileIdentity {
                        id: 502,
                        revision: 3,
                    },
                    true,
                ),
            ),
            "decode" => h
                .agent
                .set("/downloads/synthetic.mkv", evidence(source(), false)),
            "queue_path" => {
                h.arr
                    .0
                    .lock()
                    .unwrap()
                    .bodies
                    .get_mut("/api/v3/queue")
                    .unwrap()["records"][0]["outputPath"] = json!("/downloads-other")
            }
            "queue_id" => {
                h.arr
                    .0
                    .lock()
                    .unwrap()
                    .bodies
                    .get_mut("/api/v3/queue")
                    .unwrap()["records"][0]["id"] = json!(99)
            }
            _ => {
                let mut state = h.arr.0.lock().unwrap();
                let candidates = state.bodies.get_mut("/api/v3/manualimport").unwrap();
                match bad {
                    "target" => candidates[0]["episodes"] = json!([{"id":22}]),
                    "candidate_path" => candidates[0]["path"] = json!("/downloads/other.mkv"),
                    "candidate_download_id" => {
                        candidates[0]["downloadId"] = json!("download-other")
                    }
                    "rejections" => {
                        candidates[0]["rejections"] = json!([{"reason":"synthetic rejection"}])
                    }
                    _ => {
                        let first = candidates[0].clone();
                        candidates.as_array_mut().unwrap().push(first);
                    }
                }
            }
        }
        assert!(h.router().write(&intent).await.is_err(), "{bad}");
        assert_eq!(h.arr.writes(), 0, "{bad}");
    }
}

#[tokio::test]
async fn blocklist_preserves_download_and_reserves_cross_job_exclusion() {
    let h = Harness::new(Kind::Sonarr).await;
    h.downloaded().await;
    h.observe(Observation::DownloadDecoded {
        file: source(),
        good: false,
    })
    .await;
    h.apply(Event::RejectDownload { access: h.access() }).await;
    let intent = h.prepare().await;
    assert_eq!(
        h.router().write(&intent).await.unwrap(),
        Outcome::Observed(Observation::DeleteAcknowledged)
    );
    assert!(
        h.store
            .release_is_excluded(
                &h.cert.client,
                999,
                &ReleaseIdentity {
                    guid: Some(ReleaseGuid {
                        source: "client-one:7".into(),
                        value: "release-guid".into()
                    }),
                    ..ReleaseIdentity::default()
                },
                10
            )
            .await
            .unwrap()
    );
    let state = h.arr.0.lock().unwrap();
    let request = state
        .requests
        .iter()
        .find(|r| r.method == Method::Delete)
        .unwrap();
    assert_eq!(request.url.path(), "/api/v3/queue/31");
    let query: BTreeMap<_, _> = request.url.query_pairs().collect();
    assert_eq!(query.get("removeFromClient").unwrap(), "false");
    assert_eq!(query.get("skipRedownload").unwrap(), "true");
    assert_eq!(query.get("blocklist").unwrap(), "true");
    assert!(!query.contains_key("markFailed"));
}

#[tokio::test]
async fn timeout_and_process_restart_reconcile_without_repeating_real_executor() {
    for error in [Error::Timeout, Error::Transport, Error::Status(503)] {
        let mut h = Harness::new(Kind::Sonarr).await;
        let intent = h.intent().await;
        bind(&h.store, &intent, &h.route, 10).await.unwrap();
        h.arr.0.lock().unwrap().mutation_error = Some(error);
        let options = Options {
            mode: Mode::Live,
            acknowledge_live_mutations: true,
            call_timeout: Duration::from_secs(1),
        };
        let worker = Worker::new(Some(h.store.clone()), h.router(), options.clone()).unwrap();
        worker.drive(&h.lease, 10).await.unwrap();
        assert_eq!(
            h.store.load_job(1).await.unwrap().job.state(),
            State::GrabReconciling
        );
        assert_eq!(h.arr.writes(), 1);
        drop(worker);
        h.store.release_lease(&h.lease, 11).await.unwrap();
        h.store = Store::open(h.temp.path().join("db")).await.unwrap();
        h.lease = h
            .store
            .acquire_lease(1, "restarted", 12, 1000)
            .await
            .unwrap();
        let reconcile = h.intent().await;
        bind(&h.store, &reconcile, &h.route, 12).await.unwrap();
        let worker = Worker::new(Some(h.store.clone()), h.router(), options).unwrap();
        worker.drive(&h.lease, 12).await.unwrap();
        assert_eq!(
            h.store.load_job(1).await.unwrap().job.state(),
            State::Downloading
        );
        assert_eq!(h.arr.writes(), 1);
    }
}

#[tokio::test]
async fn reads_preserve_exact_decode_identity_and_import_reconciliation() {
    for kind in [Kind::Sonarr, Kind::Radarr] {
        let h = Harness::new(kind).await;
        h.downloaded().await;
        let intent = h.prepare().await;
        assert_eq!(
            h.router().read(&intent).await.unwrap(),
            Outcome::Observed(Observation::DownloadDecoded {
                file: source(),
                good: true
            })
        );
        h.agent
            .set("/downloads/synthetic.mkv", evidence(source(), false));
        assert_eq!(
            h.router().read(&intent).await.unwrap(),
            Outcome::Observed(Observation::DownloadDecoded {
                file: source(),
                good: false
            })
        );
        h.agent.set(
            "/downloads/synthetic.mkv",
            evidence(
                FileIdentity {
                    id: 502,
                    revision: 3,
                },
                true,
            ),
        );
        assert!(h.router().read(&intent).await.is_err());
        let h = Harness::new(kind).await;
        h.importing().await;
        h.observe(Observation::ImportAmbiguous).await;
        let intent = h.prepare().await;
        let (media, file) = match kind {
            Kind::Sonarr => ("episode", "episodefile"),
            Kind::Radarr => ("movie", "moviefile"),
        };
        h.arr.set(
            &format!("/api/v3/{media}/21"),
            json!({"episodeFileId":12,"movieFileId":12}),
        );
        h.arr.set(&format!("/api/v3/{file}/12"), media_file(12));
        h.arr.set("/api/v3/history",page(json!([{"id":91,"eventType":"downloadFolderImported","date":"2026-01-01T00:00:00Z","downloadId":"download-one","episodeId":21,"movieId":21,"data":{"importedPath":"/library/synthetic.mkv"}}])));
        h.agent
            .set("/library/synthetic.mkv", evidence(source(), true));
        let result = h.router().read(&intent).await.unwrap();
        let Outcome::Observed(Observation::ImportReconciled(ImportResolution::Imported {
            download,
            file,
        })) = result
        else {
            panic!("missing exact import proof")
        };
        assert_eq!(download, 81);
        h.observe(Observation::ImportReconciled(ImportResolution::Imported {
            download,
            file,
        }))
        .await;
        let intent = h.prepare().await;
        assert_eq!(
            h.router().read(&intent).await.unwrap(),
            Outcome::Observed(Observation::ImportedDecoded { file, good: true })
        );
    }
}

fn range_evidence(range: Option<&str>) -> Value {
    let mut value = evidence(source(), true);
    value["header"] = json!({"outcome":"Passed","duration_ms":300000,"elapsed_ms":1,"timeout_ms":1000,"faults":[],"tested_ranges":[],
        "streams":[{"Video":{"codec":"hevc","width":3840,"height":2160,"bit_depth":10,"dynamic_range":range}}]});
    value
}

#[tokio::test]
async fn no_profile5_is_checked_before_download_verification_and_import() {
    for kind in [Kind::Sonarr, Kind::Radarr] {
        for (range, allowed) in [
            (Some("HDR10"), Some(true)),
            (Some("DOVI_PROFILE_5"), Some(false)),
            (None, None),
        ] {
            for importing in [false, true] {
                let h = Harness::with_constraints(
                    kind,
                    Constraints {
                        forbid_dolby_vision_profile5: true,
                        ..Constraints::default()
                    },
                )
                .await;
                h.agent
                    .set("/downloads/synthetic.mkv", range_evidence(range));
                if importing {
                    h.importing().await;
                } else {
                    h.downloaded().await;
                }
                let intent = h.prepare().await;
                let result = if importing {
                    h.router().write(&intent).await
                } else {
                    h.router().read(&intent).await
                };
                let expected = if importing {
                    if allowed == Some(true) {
                        Ok(Outcome::Observed(Observation::ImportAccepted))
                    } else {
                        Err(CallError::Unavailable)
                    }
                } else {
                    allowed
                        .map(|good| {
                            Outcome::Observed(Observation::DownloadDecoded {
                                file: source(),
                                good,
                            })
                        })
                        .ok_or(CallError::Unavailable)
                };
                assert_eq!(
                    result, expected,
                    "{kind:?} {range:?}, importing {importing}"
                );
                assert_eq!(
                    h.arr.writes(),
                    usize::from(importing && allowed == Some(true))
                );
            }
        }
        // A second fresh observation at the write boundary cannot erase the constraint.
        let h = Harness::with_constraints(
            kind,
            Constraints {
                forbid_dolby_vision_profile5: true,
                ..Constraints::default()
            },
        )
        .await;
        h.agent
            .set("/downloads/synthetic.mkv", range_evidence(Some("HDR10")));
        h.agent.0.lock().unwrap().after_read.insert(
            "/downloads/synthetic.mkv".into(),
            range_evidence(Some("DOVI_PROFILE_5")),
        );
        h.importing().await;
        let intent = h.prepare().await;
        assert_eq!(h.router().write(&intent).await, Err(CallError::Unavailable));
        assert_eq!(h.arr.writes(), 0);
    }
}
