//! Physical delivery uses owned loopback peers and the real immutable outbox.
use pitcrew::live_delivery::{self, HttpFactory, TransportFactory};
use pitcrew_bot::channels::{Platform, Transport};
use pitcrew_bridge::{Authority, Bridge, Diagnosis, ViewerRoute};
use pitcrew_config::Config;
use pitcrew_diagnosis::{facts::*, rules::classify};
use pitcrew_domain::{channels::ChannelIdentity, *};
use pitcrew_engine::{Mode, Options, Worker};
use pitcrew_notify::{Notifier, http::Protocol};
use pitcrew_store::{BudgetLimits, Lease, OutboxStatus, Store};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone, Debug)]
struct Request {
    method: String,
    path: String,
    key: Option<String>,
    body: Value,
}
struct Peer {
    base: String,
    requests: Arc<Mutex<Vec<Request>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Peer {
    async fn start(notifier_down: bool) -> Self {
        Self::with_outages(notifier_down, false).await
    }
    async fn with_outages(notifier_down: bool, viewer_down: bool) -> Self {
        use axum::{
            body::{Body, to_bytes},
            extract::State,
            http::{Request as HttpRequest, StatusCode},
            response::{IntoResponse, Response},
        };
        #[derive(Clone)]
        struct StateData {
            requests: Arc<Mutex<Vec<Request>>>,
            down: bool,
            viewer_down: bool,
        }
        async fn handle(State(state): State<StateData>, request: HttpRequest<Body>) -> Response {
            let method = request.method().to_string();
            let path = request.uri().path().to_owned();
            let key = request
                .headers()
                .get("x-api-key")
                .map(|value| value.to_str().unwrap().to_owned());
            let body: Value =
                serde_json::from_slice(&to_bytes(request.into_body(), 1_000_000).await.unwrap())
                    .unwrap();
            state.requests.lock().unwrap().push(Request {
                method,
                path: path.clone(),
                key,
                body: body.clone(),
            });
            if path == "/notify" {
                if state.down {
                    return StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
                axum::Json(json!({"accepted":true,"key":body["key"]})).into_response()
            } else if state.viewer_down {
                StatusCode::SERVICE_UNAVAILABLE.into_response()
            } else {
                axum::Json(json!({"id":11})).into_response()
            }
        }
        let requests = Arc::new(Mutex::new(Vec::new()));
        let app = axum::Router::new().fallback(handle).with_state(StateData {
            requests: requests.clone(),
            down: notifier_down,
            viewer_down,
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            base,
            requests,
            task,
        }
    }
}
fn config(temp: &tempfile::TempDir, base: &str) -> Value {
    let token = temp.path().join("token");
    let url = temp.path().join("notifier-url");
    std::fs::write(&token, "test-credential").unwrap();
    std::fs::write(&url, format!("{base}/notify")).unwrap();
    json!({"version":1,
        "channels":[{"id":"channel-a","adapter":{"kind":"seerr","url":base,"api_key":{"file":token}}}],
        "notifiers":[{"id":"admin-a","adapter":{"kind":"webhook","url":{"file":url}}}]})
}
async fn engine(temp: &tempfile::TempDir) -> Store {
    Store::open(temp.path().join("engine.sqlite"))
        .await
        .unwrap()
}
fn worker(
    store: Store,
    adapters: live_delivery::DeliveryAdapters,
    shadow: bool,
) -> Worker<pitcrew_engine::delivery::Notifications> {
    Worker::new(
        Some(store),
        adapters.notifications,
        Options {
            mode: if shadow { Mode::Shadow } else { Mode::Live },
            acknowledge_live_mutations: !shadow,
            call_timeout: Duration::from_secs(2),
        },
    )
    .unwrap()
}
struct Fixture {
    _temp: tempfile::TempDir,
    store: Store,
    bridge: Bridge,
    config: Config,
    report: i64,
    authority: Authority,
    lease: Lease,
}
impl Fixture {
    async fn new(base: &str, ready: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let config = serde_json::from_value(config(&temp, base)).unwrap();
        let store = engine(&temp).await;
        let intake_path = temp.path().join("intake.sqlite");
        let intake = pitcrew_intake_store::Store::open(&intake_path)
            .await
            .unwrap();
        let bridge = Bridge::open(
            &intake_path,
            store.clone(),
            pitcrew_bridge::Options {
                policy: Policy {
                    max_grabs: 2,
                    stage_timeout: 300,
                    total_timeout: 1000,
                },
                timeouts: StageTimeouts {
                    download: 300,
                    import: 300,
                    decode: 300,
                    verify_retry: 180,
                },
                budgets: BudgetLimits {
                    global_hour: 8,
                    global_day: 8,
                    viewer_hour: 4,
                    viewer_day: 4,
                },
                cooldown_seconds: 3600,
                evidence_max_age: 300,
            },
        )
        .await
        .unwrap();
        intake
            .persist_intake(
                "channel-a",
                "message-a",
                "account-a",
                "7",
                &json!({"outcome":"report"}),
                10,
            )
            .await
            .unwrap();
        let report = intake
            .stored_intake("channel-a", "message-a", "account-a")
            .await
            .unwrap()
            .unwrap()
            .id;
        let mut facts = EvidenceFacts::default();
        facts.playback.decode = DecodeOutcome::Bad;
        let diagnosis = Diagnosis {
            report_id: report,
            decided_at: 10,
            decision: classify(&facts),
            evidence_summary: facts,
            resource: "library-a:episode-a".into(),
            library_id: 1,
            arr_instance_id: "series-a".into(),
            arr_item_id: 2,
            arr_file_id: 3,
            agent_file: FileIdentity { id: 4, revision: 5 },
        };
        let authority = Authority {
            account: "account-a".into(),
            resource: diagnosis.resource.clone(),
            library: 1,
            arr_instance_id: "series-a".into(),
            arr_item_id: 2,
            arr_file_id: 3,
            file: diagnosis.agent_file,
            observed_at: 10,
            access: Access::Granted {
                library: 1,
                expires_at: 1000,
            },
            probe: ProbeOutcome::Unknown,
            decode: DecodeOutcome::Bad,
            route: Some(ViewerRoute {
                recipient: ChannelIdentity {
                    channel_id: "channel-a".into(),
                    platform_user_id: "3".into(),
                },
                conversation: "7".into(),
                last_inbound: Some(10),
            }),
        };
        bridge.record_authority(report, &authority).await.unwrap();
        store
            .register_file(
                "series-a",
                &diagnosis.resource,
                &pitcrew_store::policy::IdentityFingerprint {
                    arr_file_id: 3,
                    size: 1024,
                    date_added: "2026-01-01T00:00:00Z".into(),
                    quality: "synthetic".into(),
                    agent: diagnosis.agent_file,
                },
                10,
            )
            .await
            .unwrap();
        let job = bridge
            .consume(&diagnosis, 10)
            .await
            .unwrap()
            .job_id
            .unwrap();
        let lease = store
            .acquire_lease(job, "delivery-test", 11, 900)
            .await
            .unwrap();
        let fixture = Self {
            _temp: temp,
            store,
            bridge,
            config,
            report,
            authority,
            lease,
        };
        if ready {
            fixture
                .event(Event::SelectRelease {
                    release: 7,
                    access: Access::Granted {
                        library: 1,
                        expires_at: 1000,
                    },
                })
                .await;
            let file = FileIdentity { id: 8, revision: 9 };
            fixture
                .event(Event::AutoImported {
                    grab: IntentKey { job, sequence: 1 },
                    download: 10,
                    file,
                })
                .await;
            fixture
                .event(Event::Observed {
                    intent: IntentKey { job, sequence: 2 },
                    observation: Observation::ImportedDecoded { file, good: true },
                })
                .await;
            fixture
                .event(Event::Observed {
                    intent: IntentKey { job, sequence: 3 },
                    observation: Observation::TargetChecked {
                        file,
                        plex_file: file,
                        constraints_satisfied: true,
                    },
                })
                .await;
            fixture.event(Event::QueueReady).await;
        } else {
            fixture.event(Event::Fail(Failure::Cancelled)).await;
        }
        fixture
    }
    async fn event(&self, event: Event) {
        let current = self.store.load_job(self.lease.job).await.unwrap();
        self.store
            .apply_event(
                &self.lease,
                current.revision,
                &format!("event-{}", current.revision),
                event,
                11,
            )
            .await
            .unwrap();
    }
    async fn restarted(&self) -> Worker<pitcrew_engine::delivery::Notifications> {
        let store = engine(&self._temp).await;
        let adapters =
            live_delivery::compose(&self.config, store.clone(), Arc::new(|| 20)).unwrap();
        worker(store, adapters, false)
    }
    fn compose(&self) -> live_delivery::DeliveryAdapters {
        live_delivery::compose(&self.config, self.store.clone(), Arc::new(|| 20)).unwrap()
    }
}

#[tokio::test]
async fn bridge_freezes_verified_reply_before_real_channel_delivery_and_restart() {
    let peer = Peer::start(false).await;
    let mut fixture = Fixture::new(&peer.base, true).await;
    let adapters = fixture.compose();
    assert_eq!(adapters.admin_notifier, "admin-a");
    assert_eq!(
        fixture
            .bridge
            .prepare_notifications(fixture.lease.job, Some(&adapters.admin_notifier))
            .await
            .unwrap(),
        1
    );
    let expected = fixture
        .bridge
        .reply_for_job(fixture.lease.job)
        .await
        .unwrap();
    fixture.authority.route.as_mut().unwrap().conversation = "99".into();
    fixture
        .bridge
        .record_authority(fixture.report, &fixture.authority)
        .await
        .unwrap();
    assert_eq!(
        fixture
            .bridge
            .prepare_notifications(fixture.lease.job, Some("admin-a"))
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        worker(fixture.store.clone(), adapters, false)
            .drive(&fixture.lease, 20)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        fixture
            .store
            .load_job(fixture.lease.job)
            .await
            .unwrap()
            .job
            .state(),
        State::Done
    );
    assert_eq!(
        fixture
            .restarted()
            .await
            .drive(&fixture.lease, 20)
            .await
            .unwrap(),
        0
    );
    let requests = peer.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, "/api/v1/issue/7/comment");
    assert_eq!(requests[0].key.as_deref(), Some("test-credential"));
    assert_eq!(requests[0].body, json!({"message":expected}));
}

#[tokio::test]
async fn failed_notifier_stays_uncertain_without_duplicate_after_restart_and_viewer_receives_failure()
 {
    for down in [false, true] {
        let peer = Peer::start(down).await;
        let fixture = Fixture::new(&peer.base, false).await;
        assert_eq!(
            fixture
                .bridge
                .prepare_notifications(fixture.lease.job, Some("admin-a"))
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            worker(fixture.store.clone(), fixture.compose(), false)
                .drive(&fixture.lease, 20)
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            fixture
                .restarted()
                .await
                .drive(&fixture.lease, 20)
                .await
                .unwrap(),
            0
        );
        let rows = fixture.store.outbox(fixture.lease.job).await.unwrap();
        let admin = rows
            .iter()
            .find(|r| matches!(r.intent.effect, Effect::NotifyAdmin { .. }))
            .unwrap();
        assert_eq!(
            admin.status,
            if down {
                OutboxStatus::Uncertain
            } else {
                OutboxStatus::Completed
            }
        );
        assert_eq!(
            fixture
                .store
                .load_job(fixture.lease.job)
                .await
                .unwrap()
                .job
                .state(),
            State::Failed
        );
        let requests = peer.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let admin = requests.iter().find(|r| r.path == "/notify").unwrap();
        assert_eq!(admin.method, "POST");
        assert_eq!(
            admin.body,
            json!({"key":{"job":fixture.lease.job,"purpose":"AdminFailure","recipient":"admin-a"},
            "title":"Repair needs review","body":"A repair could not complete. Review the job and its evidence in Pit Crew."})
        );
        let viewer = requests
            .iter()
            .find(|r| r.path == "/api/v1/issue/7/comment")
            .unwrap();
        assert_eq!(
            viewer.body,
            json!({"message": "This one needs the server owner to take a look. Your report has the details."})
        );
    }
}

#[tokio::test]
async fn shadow_worker_never_contacts_composed_transports() {
    let peer = Peer::start(false).await;
    let fixture = Fixture::new(&peer.base, false).await;
    fixture
        .bridge
        .prepare_notifications(fixture.lease.job, Some("admin-a"))
        .await
        .unwrap();
    assert_eq!(
        worker(fixture.store.clone(), fixture.compose(), true)
            .drive(&fixture.lease, 20)
            .await
            .unwrap(),
        2
    );
    assert!(peer.requests.lock().unwrap().is_empty());
}

#[derive(Default)]
struct Factory {
    channels: Mutex<Vec<(Platform, String, String)>>,
    notifiers: Mutex<Vec<(Protocol, String, Option<String>)>>,
}
impl TransportFactory for Factory {
    fn channel(
        &self,
        platform: Platform,
        base: &str,
        credential: &str,
    ) -> Result<Arc<dyn Transport>, &'static str> {
        self.channels
            .lock()
            .unwrap()
            .push((platform, base.into(), credential.into()));
        HttpFactory.channel(platform, base, credential)
    }
    fn notifier(
        &self,
        protocol: Protocol,
        endpoint: &str,
        bearer: Option<&str>,
    ) -> Result<Arc<dyn Notifier>, &'static str> {
        self.notifiers
            .lock()
            .unwrap()
            .push((protocol, endpoint.into(), bearer.map(str::to_owned)));
        HttpFactory.notifier(protocol, endpoint, bearer)
    }
}
#[tokio::test]
async fn composes_all_supported_protocols_and_validates_telegram_url_token() {
    let temp = tempfile::tempdir().unwrap();
    let store = engine(&temp).await;
    let value = config(&temp, "https://example.invalid");
    let token = value["channels"][0]["adapter"]["api_key"].clone();
    let mut value = value;
    value["notifiers"][0]["adapter"] =
        json!({"kind":"ntfy","url":"https://example.invalid/topic","bearer_token":token});
    value["channels"] = json!([
        {"id":"discord-a","adapter":{"kind":"discord","token":token,"control_plane_url":"https://example.invalid","control_plane_secret":token}},
        {"id":"telegram-a","adapter":{"kind":"telegram","token":token,"control_plane_url":"https://example.invalid","control_plane_secret":token}},
        {"id":"web-a","adapter":{"kind":"web","public_url":"https://example.invalid","control_plane_url":"https://example.invalid","control_plane_secret":token,"session_secret":token}}
    ]);
    let factory = Factory::default();
    let config: Config = serde_json::from_value(value.clone()).unwrap();
    let result =
        live_delivery::compose_with(&config, store.clone(), Arc::new(|| 20), &factory).unwrap();
    assert_eq!(result.notifications.channels.len(), 2);
    assert_eq!(
        factory.channels.lock().unwrap().as_slice(),
        &[
            (
                Platform::Discord,
                "https://discord.com/api/v10".into(),
                "test-credential".into()
            ),
            (
                Platform::Telegram,
                "https://api.telegram.org/bottest-credential".into(),
                "test-credential".into()
            )
        ]
    );
    assert_eq!(
        factory.notifiers.lock().unwrap().as_slice(),
        &[(
            Protocol::Ntfy,
            "https://example.invalid/topic".into(),
            Some("test-credential".into())
        )]
    );
    std::fs::write(temp.path().join("token"), "aZ01_:-").unwrap();
    assert!(live_delivery::compose(&config, store.clone(), Arc::new(|| 20)).is_ok());
    value["notifiers"][0]["adapter"]["bearer_token"] = Value::Null;
    value["channels"] = json!([value["channels"][1].clone()]);
    for bad in [
        "bad/token",
        "bad?token",
        "bad#token",
        "bad token",
        "bad.token",
        "é",
    ] {
        std::fs::write(temp.path().join("token"), bad).unwrap();
        // Remove bearer and Discord: their independent checks must not mask this one.
        let config = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(
            live_delivery::compose(&config, store.clone(), Arc::new(|| 20)).err(),
            Some("Telegram credential contains invalid URL characters")
        );
    }
}

#[tokio::test]
async fn missing_and_unsupported_prerequisites_refuse_specific_protocols() {
    let temp = tempfile::tempdir().unwrap();
    let store = engine(&temp).await;
    let base = config(&temp, "https://example.invalid");
    let token = base["channels"][0]["adapter"]["api_key"].clone();
    let missing = json!({"file":temp.path().join("absent")});
    let mail = json!({"smtp_host":"mail.example.invalid","smtp_port":465,"username":"test","password":token,"from_address":"unused-sender"});
    let cases = [
        (
            "notifiers",
            json!([]),
            "live delivery requires exactly one configured admin notifier",
        ),
        (
            "notifiers",
            json!([base["notifiers"][0], base["notifiers"][0]]),
            "live delivery requires exactly one configured admin notifier",
        ),
        (
            "notifiers",
            json!([{"id":"admin-a","adapter":{"kind":"pushover","relay_url":"https://example.invalid"}}]),
            "live Pushover delivery requires a certified relay receipt protocol",
        ),
        (
            "notifiers",
            json!([{"id":"admin-a","adapter":{"kind":"email","mail":mail,"to_address":"unused-recipient"}}]),
            "live email notifier transport is not implemented",
        ),
        (
            "channels",
            json!([]),
            "live delivery requires a supported authenticated viewer channel",
        ),
        (
            "channels",
            json!([base["channels"][0], base["channels"][0]]),
            "live channel IDs must be unique",
        ),
        (
            "channels",
            json!([{"id":"email-a","adapter":{"kind":"email","mail":mail,"imap_host":"mail.example.invalid","imap_port":993}}]),
            "live email channel transport is not implemented",
        ),
        (
            "channels",
            json!([{"id":"matrix-a","adapter":{"kind":"matrix","homeserver_url":"https://example.invalid","access_token":token}}]),
            "live Matrix channel transport is not implemented",
        ),
        (
            "channels",
            json!([{"id":"channel-a","adapter":{"kind":"seerr","url":"https://example.invalid","api_key":missing}}]),
            "channel credential is unavailable",
        ),
        (
            "notifiers",
            json!([{"id":"admin-a","adapter":{"kind":"webhook","url":missing}}]),
            "notifier URL secret is unavailable",
        ),
        (
            "notifiers",
            json!([{"id":"admin-a","adapter":{"kind":"ntfy","url":"https://example.invalid/topic","bearer_token":missing}}]),
            "notifier bearer secret is unavailable",
        ),
    ];
    for (field, value, expected) in cases {
        let mut value_config = base.clone();
        value_config[field] = value;
        let parsed = serde_json::from_value(value_config).unwrap();
        assert_eq!(
            live_delivery::compose(&parsed, store.clone(), Arc::new(|| 20)).err(),
            Some(expected)
        );
    }
    let mut bad_channel = base.clone();
    bad_channel["channels"][0]["adapter"]["url"] = json!("not a URL");
    let parsed = serde_json::from_value(bad_channel).unwrap();
    assert_eq!(
        live_delivery::compose(&parsed, store.clone(), Arc::new(|| 20)).err(),
        Some("invalid channel API URL")
    );
    let mut bad_notifier = base;
    bad_notifier["notifiers"][0]["adapter"] = json!({"kind":"ntfy","url":"not a URL"});
    let parsed = serde_json::from_value(bad_notifier).unwrap();
    assert_eq!(
        live_delivery::compose(&parsed, store, Arc::new(|| 20)).err(),
        Some("notifier URL is invalid")
    );
}

#[tokio::test]
async fn physical_ready_channel_outage_stays_uncertain_after_reopening_the_store() {
    let peer = Peer::with_outages(false, true).await;
    let fixture = Fixture::new(&peer.base, true).await;
    fixture
        .bridge
        .prepare_notifications(fixture.lease.job, Some("admin-a"))
        .await
        .unwrap();
    assert_eq!(
        worker(fixture.store.clone(), fixture.compose(), false)
            .drive(&fixture.lease, 20)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        fixture
            .store
            .load_job(fixture.lease.job)
            .await
            .unwrap()
            .job
            .state(),
        State::Done
    );
    assert_eq!(
        fixture
            .store
            .outbox(fixture.lease.job)
            .await
            .unwrap()
            .last()
            .unwrap()
            .status,
        OutboxStatus::Uncertain
    );
    assert_eq!(
        fixture
            .restarted()
            .await
            .drive(&fixture.lease, 20)
            .await
            .unwrap(),
        0
    );
    assert_eq!(peer.requests.lock().unwrap().len(), 1);
}
