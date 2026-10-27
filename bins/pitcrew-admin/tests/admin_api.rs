use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::State,
    http::{Request, StatusCode, header},
    response::{IntoResponse, Response},
};
use pitcrew::admin::AdminApi as P3AdminApi;
use pitcrew_admin::{
    Error,
    actions::{AdminApi, Command, HttpAdminApi, Outcome},
    auth::{Auth, AuthFuture, LoginPin},
    config::Config,
    journal::Journal,
    read::Sources,
    web::App,
};
use pitcrew_domain::{
    Constraints, Event, Failure, FileIdentity, IntentKey, Policy, StageTimeouts,
    channels::ChannelIdentity,
};
use pitcrew_engine::webhook::{MAX_BODY, Secret};
use pitcrew_store::{
    BudgetLimits, Lease, NewJob, OutboxStatus, Store,
    delivery::NotificationPayload,
    policy::{ClientCapabilities, ExclusionKind, NewExclusion},
};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tokio::{net::TcpListener, task::JoinHandle};
use tower::ServiceExt;

const ADMIN: &str = "synthetic-admin-secret-0123456789abcdef";
const CONTROL: &str = "synthetic-control-secret-0123456789abcd";
const NOW: u64 = 100;
const ORIGIN: &str = "https://admin.example.invalid";

struct HandlerState {
    api: P3AdminApi,
    store: Store,
    requests: AtomicUsize,
    lose_next_response: AtomicBool,
}

async fn handle_p3(State(state): State<Arc<HandlerState>>, request: Request<Body>) -> Response {
    state.requests.fetch_add(1, Ordering::SeqCst);
    let (parts, body) = request.into_parts();
    let body = to_bytes(body, MAX_BODY + 1).await.unwrap();
    let credential = parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default();
    let (status, payload) = state
        .api
        .handle(
            &state.store,
            parts.method.as_str(),
            parts.uri.path(),
            credential,
            &body,
            NOW,
        )
        .await;
    if state.lose_next_response.swap(false, Ordering::SeqCst) {
        // The real transaction has committed, but its acknowledgement cannot
        // reach the caller. The client must retain ambiguity without retrying.
        (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error":"synthetic acknowledgement lost"})),
        )
            .into_response()
    } else {
        (StatusCode::from_u16(status).unwrap(), Json(payload)).into_response()
    }
}

struct LocalApi {
    base: String,
    state: Arc<HandlerState>,
    task: JoinHandle<()>,
}
impl LocalApi {
    async fn start(store: Store) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let state = Arc::new(HandlerState {
            api: P3AdminApi {
                secret: Secret::new(ADMIN).unwrap(),
            },
            store,
            requests: AtomicUsize::new(0),
            lose_next_response: AtomicBool::new(false),
        });
        let router = Router::new().fallback(handle_p3).with_state(state.clone());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self { base, state, task }
    }
    fn client(&self) -> HttpAdminApi {
        HttpAdminApi::new(&self.base, ADMIN).unwrap()
    }
    fn requests(&self) -> usize {
        self.state.requests.load(Ordering::SeqCst)
    }
}
impl Drop for LocalApi {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn exclusion(store: &Store) -> u64 {
    store
        .exclude_release(
            &NewExclusion {
                client: "arr-a".into(),
                release: 17,
                identity: Default::default(),
                kind: ExclusionKind::DeadRelease,
                reason: "Failed decode".into(),
                evidence: json!({"decode":"failed"}),
                pinned: false,
                created_at: 1,
            },
            "excluded-release",
        )
        .await
        .unwrap()
        .id
}

async fn capabilities(store: &Store) {
    store
        .observe_client_version("arr-a", "1.0", 1)
        .await
        .unwrap();
    store
        .record_capabilities(
            "arr-a",
            "1.0",
            &ClientCapabilities {
                external_queue_actor: true,
                held_category: None,
            },
            &json!({"outside_queue_actor":true}),
            2,
        )
        .await
        .unwrap();
}

async fn uncertain_notification(store: &Store) -> (Lease, IntentKey, NotificationPayload) {
    store
        .create_job(
            &NewJob {
                id: 1,
                viewer: "viewer-a".into(),
                resource: "movie-a".into(),
                library: 7,
                original: FileIdentity { id: 8, revision: 9 },
                constraints: Constraints::default(),
                policy: Policy {
                    max_grabs: 1,
                    stage_timeout: 300,
                    total_timeout: 900,
                },
                timeouts: StageTimeouts {
                    download: 300,
                    import: 300,
                    decode: 300,
                    verify_retry: 180,
                },
                created_at: 1,
            },
            "notification-job",
            BudgetLimits {
                global_hour: 4,
                global_day: 4,
                viewer_hour: 4,
                viewer_day: 4,
            },
        )
        .await
        .unwrap();
    let lease = store
        .acquire_lease(1, "synthetic-worker", 1, 900)
        .await
        .unwrap();
    store
        .apply_event(&lease, 0, "cancelled", Event::Fail(Failure::Cancelled), 2)
        .await
        .unwrap();
    let key = IntentKey {
        job: 1,
        sequence: 2,
    };
    let payload = NotificationPayload::Viewer {
        recipient: ChannelIdentity {
            channel_id: "channel-a".into(),
            platform_user_id: "viewer-a".into(),
        },
        conversation: "conversation-a".into(),
        text: "Your report needs review by the server owner.".into(),
        last_inbound: Some(1),
    };
    store.prepare_notification(key, &payload).await.unwrap();
    store
        .finish_intent(&lease, key, OutboxStatus::Uncertain, 3)
        .await
        .unwrap();
    (lease, key, payload)
}

#[tokio::test]
async fn release_and_reset_cross_http_into_real_audited_admin_transactions() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("engine.sqlite")).await.unwrap();
    let id = exclusion(&store).await;
    capabilities(&store).await;
    let server = LocalApi::start(store.clone()).await;
    let client = server.client();
    let release_note = "Reviewed the failed decode and replacement evidence.";
    assert_eq!(
        client
            .submit(&Command::Release { id }, "release-one", release_note)
            .await,
        Outcome::Accepted
    );
    assert!(store.exclusions("arr-a", NOW).await.unwrap().is_empty());
    // Already-released returns released:false; the acknowledgement remains
    // accepted and does not add a second policy mutation or audit record.
    assert_eq!(
        client
            .submit(&Command::Release { id }, "release-one", release_note)
            .await,
        Outcome::Accepted
    );
    assert_eq!(store.policy_audit().await.unwrap().len(), 1);
    let reset_note = "Reset client trust before recertification.";
    assert_eq!(
        client
            .submit(
                &Command::Reset {
                    client: "arr-a".into()
                },
                "reset-one",
                reset_note
            )
            .await,
        Outcome::Accepted
    );
    let ledger = store.client_ledger("arr-a").await.unwrap().unwrap();
    assert_eq!(ledger.version, "1.0");
    assert!(ledger.capabilities.is_none());
    assert_eq!(ledger.evidence, Value::Null);
    let audit = store.policy_audit().await.unwrap();
    assert_eq!(audit.len(), 2);
    assert_eq!(audit[0].action, "release_exclusion");
    assert_eq!(audit[0].subject, id.to_string());
    assert_eq!(audit[0].note, release_note);
    assert_eq!(audit[1].action, "reset_capabilities");
    assert_eq!(audit[1].subject, "arr-a");
    assert_eq!(audit[1].note, reset_note);
    assert!(
        audit
            .iter()
            .all(|entry| entry.actor == "configured-admin" && entry.at == NOW)
    );
    assert_eq!(server.requests(), 3);
}

#[tokio::test]
async fn resend_reuses_immutable_payload_and_never_requeues_a_replayed_request_key() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("engine.sqlite")).await.unwrap();
    let (lease, key, payload) = uncertain_notification(&store).await;
    let before = store.outbox(key.job).await.unwrap();
    let server = LocalApi::start(store.clone()).await;
    let client = server.client();
    let command = Command::Resend {
        job: key.job,
        sequence: key.sequence,
    };
    let note = "Delivery was uncertain; owner requested this retry.";
    assert_eq!(
        client.submit(&command, "resend-once", note).await,
        Outcome::Accepted
    );
    let rows = store.outbox(key.job).await.unwrap();
    assert_eq!(rows.len(), before.len());
    assert!(
        rows.iter()
            .zip(&before)
            .all(|(after, before)| after.intent == before.intent)
    );
    assert_eq!(rows[1].status, OutboxStatus::Pending);
    assert_eq!(rows[1].fence, None);
    assert_eq!(store.notification_payload(key).await.unwrap(), payload);
    assert_eq!(
        client.submit(&command, "resend-once", note).await,
        Outcome::Accepted
    );
    assert_eq!(store.policy_audit().await.unwrap().len(), 1);
    assert_eq!(
        client
            .submit(&command, "resend-once", "Changed reason")
            .await,
        Outcome::Rejected
    );
    assert_eq!(store.policy_audit().await.unwrap().len(), 1);
    // Even if the explicitly requested retry later becomes uncertain again,
    // replaying the original key cannot schedule a third send.
    assert!(store.start_intent(&lease, key, 1, 101).await.unwrap());
    store
        .finish_intent(&lease, key, OutboxStatus::Uncertain, 102)
        .await
        .unwrap();
    assert_eq!(
        client.submit(&command, "resend-once", note).await,
        Outcome::Accepted
    );
    assert_eq!(
        store.outbox(key.job).await.unwrap()[1].status,
        OutboxStatus::Uncertain
    );
    assert_eq!(store.notification_payload(key).await.unwrap(), payload);
    let audit = store.policy_audit().await.unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].action, "notification_resend");
    assert_eq!(audit[0].subject, "1:2");
    assert_eq!(audit[0].note, note);
    assert_eq!(server.requests(), 4);
}

#[tokio::test]
async fn admin_credentials_and_request_errors_are_rejected_without_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("engine.sqlite")).await.unwrap();
    let id = exclusion(&store).await;
    capabilities(&store).await;
    let server = LocalApi::start(store.clone()).await;
    let wrong = HttpAdminApi::new(&server.base, CONTROL).unwrap();
    assert_eq!(
        wrong
            .submit(&Command::Release { id }, "wrong-key", "Reviewed")
            .await,
        Outcome::Rejected
    );
    assert_eq!(
        wrong
            .submit(
                &Command::Reset {
                    client: "arr-a".into()
                },
                "wrong-reset",
                "Reviewed"
            )
            .await,
        Outcome::Rejected
    );
    let response = reqwest::Client::new()
        .post(format!("{}v1/admin/exclusions/release", server.base))
        .json(&json!({"id":id,"note":"Reviewed"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    let body = response.text().await.unwrap();
    assert!(!body.contains(ADMIN));
    assert!(!body.contains(CONTROL));
    let client = server.client();
    assert_eq!(
        client
            .submit(&Command::Release { id }, "bad-note", " ")
            .await,
        Outcome::Rejected
    );
    assert_eq!(
        client
            .submit(&Command::Release { id: 999 }, "missing", "Reviewed")
            .await,
        Outcome::Rejected
    );
    assert_eq!(store.exclusions("arr-a", NOW).await.unwrap().len(), 1);
    assert!(
        store
            .client_ledger("arr-a")
            .await
            .unwrap()
            .unwrap()
            .capabilities
            .is_some()
    );
    assert!(store.policy_audit().await.unwrap().is_empty());
    assert_eq!(server.requests(), 5);
}

struct UnusedAuth;
impl Auth for UnusedAuth {
    fn start(&self) -> AuthFuture<'_, LoginPin> {
        Box::pin(async { Err(Error::Upstream) })
    }
    fn poll(&self, _: u64) -> AuthFuture<'_, Option<String>> {
        Box::pin(async { Err(Error::Upstream) })
    }
}
fn ui_config(dir: &Path) -> Config {
    Config {
        database: dir.join("admin.sqlite"),
        engine_database: dir.join("engine.sqlite"),
        intake_database: dir.join("intake.sqlite"),
        bind: "127.0.0.1:0".parse().unwrap(),
        public_origin: ORIGIN.into(),
        plex_auth_url: "https://plex.example.invalid/".into(),
        client_id: "synthetic-installation".into(),
        admin_account_ids: vec!["1001".into()],
        api: None,
        rates: Default::default(),
    }
}
async fn ui_request(
    app: &Arc<App>,
    method: &str,
    path: &str,
    cookie: &str,
    body: &str,
) -> StatusCode {
    app.clone()
        .router()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header(header::ORIGIN, ORIGIN)
                .header(header::COOKIE, cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn ui_duplicate_submission_and_refresh_do_not_retry_a_committed_but_unacknowledged_resend() {
    let dir = tempfile::tempdir().unwrap();
    let config = ui_config(dir.path());
    let store = Store::open(&config.engine_database).await.unwrap();
    let (_, key, payload) = uncertain_notification(&store).await;
    pitcrew_intake_store::Store::open(&config.intake_database)
        .await
        .unwrap();
    let sources = Sources::open(&config.engine_database, &config.intake_database)
        .await
        .unwrap();
    let journal = Journal::open(&config.database).await.unwrap();
    let login = journal.begin(42, NOW).await.unwrap();
    let session = journal.complete(&login, "1001", NOW).await.unwrap();
    let cookie = format!("pitcrew_admin_session={}", session.token);
    let server = LocalApi::start(store.clone()).await;
    server
        .state
        .lose_next_response
        .store(true, Ordering::SeqCst);
    let app = Arc::new(
        App::new(
            journal,
            sources,
            Arc::new(UnusedAuth),
            Some(Arc::new(server.client())),
            config,
            Arc::new(|| NOW),
        )
        .unwrap(),
    );
    let command = Command::Resend {
        job: key.job,
        sequence: key.sequence,
    };
    let note = "Retry the previously uncertain delivery once.";
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &session.csrf)
        .append_pair("request_key", "one-browser-request")
        .append_pair("command", &serde_json::to_string(&command).unwrap())
        .append_pair("note", note)
        .finish();
    assert_eq!(
        ui_request(&app, "POST", "/actions", &cookie, &body).await,
        StatusCode::SEE_OTHER
    );
    assert_eq!(server.requests(), 1);
    assert_eq!(
        app.journal.requests().await.unwrap()[0]["state"],
        "uncertain"
    );
    assert_eq!(
        store.outbox(key.job).await.unwrap()[1].status,
        OutboxStatus::Pending
    );
    assert_eq!(store.notification_payload(key).await.unwrap(), payload);
    assert_eq!(store.policy_audit().await.unwrap().len(), 1);
    assert_eq!(
        ui_request(&app, "POST", "/actions", &cookie, &body).await,
        StatusCode::SEE_OTHER
    );
    for _ in 0..2 {
        assert_eq!(
            ui_request(&app, "GET", "/requests", &cookie, "").await,
            StatusCode::OK
        );
    }
    let reopened = Journal::open(&dir.path().join("admin.sqlite"))
        .await
        .unwrap();
    reopened.recover(NOW + 1).await.unwrap();
    assert!(
        reopened
            .existing_request("1001", "one-browser-request", &command, note)
            .await
            .unwrap()
    );
    assert_eq!(reopened.requests().await.unwrap()[0]["state"], "uncertain");
    assert_eq!(server.requests(), 1);
    assert_eq!(store.policy_audit().await.unwrap().len(), 1);
    assert_eq!(store.notification_payload(key).await.unwrap(), payload);
}
