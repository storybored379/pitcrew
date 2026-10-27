use axum::{
    body::{Body, to_bytes},
    http::{Request as HttpRequest, StatusCode},
};
use pitcrew::{runtime::Runtime, stack};
use pitcrew_config::Config;
use pitcrew_domain::{
    FileIdentity,
    media::{MediaEvidence, MediaProbe, ProbeOutcome},
};
use pitcrew_upstream::{Method, Request, Response, ResponseFuture, Transport};
use serde_json::json;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use tower::ServiceExt;

#[derive(Clone, Copy)]
struct AccessReply {
    owner: bool,
    status: u16,
    padded_bytes: Option<usize>,
    unavailable: bool,
}

#[derive(Clone)]
struct MediaReply {
    file: FileIdentity,
    observed_at_unix_ms: u64,
    clock: Arc<AtomicU64>,
    after_evidence: Option<u64>,
    after_logs: Option<u64>,
    after_second_file: Option<u64>,
}

#[derive(Clone)]
struct Fake {
    calls: Arc<Mutex<Vec<String>>>,
    mode: Arc<Mutex<String>>,
    on_search: Arc<Mutex<Option<pitcrew_store::Store>>>,
    advance_on_second_file: Arc<Mutex<Option<Arc<AtomicU64>>>>,
    access_reply: Arc<Mutex<Option<AccessReply>>>,
    media_reply: Arc<Mutex<Option<MediaReply>>>,
}
impl Fake {
    fn new() -> Self {
        Self {
            calls: Default::default(),
            mode: Default::default(),
            on_search: Default::default(),
            advance_on_second_file: Default::default(),
            access_reply: Default::default(),
            media_reply: Default::default(),
        }
    }
}
impl Transport for Fake {
    fn send(&self, request: Request) -> ResponseFuture<'_> {
        assert_eq!(request.method, Method::Get, "shadow called a write");
        assert!(request.body.is_none());
        let path = request.url.path().to_owned();
        if matches!(
            path.as_str(),
            "/api/v2/user" | "/api/servers/server/shared_servers"
        ) {
            assert_eq!(request.limits.timeout, std::time::Duration::from_secs(10));
            assert_eq!(request.limits.max_response_bytes, 1024 * 1024);
        }
        self.calls.lock().unwrap().push(path.clone());
        let mode = self.mode.lock().unwrap().clone();
        if let Some(reply) = *self.access_reply.lock().unwrap() {
            let access_path = if reply.owner {
                "/api/v2/user"
            } else {
                "/api/servers/server/shared_servers"
            };
            if path == access_path {
                let mut body = if reply.owner {
                    br#"{"id":7}"#.to_vec()
                } else {
                    br#"<MediaContainer size="1"><SharedServer userID="7"><Section key="1" shared="1"/></SharedServer></MediaContainer>"#.to_vec()
                };
                if let Some(bytes) = reply.padded_bytes {
                    body.resize(bytes, b' ');
                }
                return Box::pin(async move {
                    if reply.unavailable {
                        Err(pitcrew_upstream::Error::Transport)
                    } else {
                        Ok(Response {
                            status: reply.status,
                            body,
                        })
                    }
                });
            }
        }
        let mut media = MediaEvidence {
            file: FileIdentity {
                id: 77,
                revision: if mode == "changed_agent" { 2 } else { 1 },
            },
            observed_at_unix_ms: if mode == "stale_agent" { 1 } else { 100_000 },
            symlinks: None,
            mount: None,
            reads: vec![],
            header: None,
            decode: Some(MediaProbe {
                outcome: ProbeOutcome::Failed,
                duration_ms: Some(90_000),
                elapsed_ms: 1,
                timeout_ms: 1000,
                faults: vec![],
                streams: vec![],
                tested_ranges: vec![],
            }),
            plex_failures: vec![],
        };
        if let Some(reply) = self.media_reply.lock().unwrap().clone() {
            media.file = reply.file;
            media.observed_at_unix_ms = reply.observed_at_unix_ms;
            let after = match path.as_str() {
                "/v1/evidence" => reply.after_evidence,
                "/v1/plexfails" => reply.after_logs,
                "/api/v3/moviefile/12"
                    if self
                        .calls
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|p| p.as_str() == path)
                        .count()
                        == 2 =>
                {
                    reply.after_second_file
                }
                _ => None,
            };
            if let Some(now) = after {
                reply.clock.store(now, Ordering::SeqCst);
            }
        }
        let mut body = match path.as_str() {
            "/library/sections" => {
                json!({"MediaContainer":{"size":1,"Directory":[{"key":"1","title":"Films","type":"movie"}]}})
            }
            "/library/sections/1/all" => {
                json!({"MediaContainer":{"size":1,"offset":0,"totalSize":1,"Metadata":[{"ratingKey":"5","type":"movie","title":"Synthetic Film","Media":[{"videoResolution":"1080","Part":[{"file":"/media/film.mkv"}]}]}]}})
            }
            "/api/v3/movie" => {
                json!([{"id":9,"title":"Synthetic Film","movieFile":{"id":12,"path":"/media/film.mkv","movieId":9}}])
            }
            "/api/v3/movie/9" => json!({"movieFileId":if mode=="changed_arr" { 13 } else { 12 }}),
            "/api/v3/moviefile/12" => {
                json!({"id":12,"path":"/media/film.mkv","movieId":9,"size":100000000,"dateAdded":"2026-01-01T00:00:00Z","quality":{"quality":{"id":7},"revision":{"version":1}}})
            }
            "/api/v3/release" => {
                json!([{"guid":"synthetic-release","indexerId":1,"approved":true,"rejected":false,"downloadAllowed":true,"rejections":[],"size":100000000,"quality":{"quality":{"id":7},"revision":{"version":1}}}])
            }
            "/api/v2/user" => json!({"id":if mode == "owner_revoked" {7} else {99}}),
            "/api/servers/server/shared_servers" => {
                let shared = if mode == "denied" { "0" } else { "1" };
                let viewer = if mode == "viewer_switch" { "8" } else { "7" };
                let xml = format!(
                    "<MediaContainer size=\"1\"><SharedServer userID=\"{viewer}\"><Section key=\"1\" shared=\"{shared}\"/></SharedServer></MediaContainer>"
                );
                return Box::pin(async move {
                    Ok(Response {
                        status: 200,
                        body: xml.into_bytes(),
                    })
                });
            }
            "/v1/evidence" => {
                assert_eq!(
                    request
                        .url
                        .query_pairs()
                        .find(|(k, _)| k == "path")
                        .unwrap()
                        .1,
                    "/media/film.mkv"
                );
                serde_json::to_value(media).unwrap()
            }
            "/v1/plexfails" => json!({"failures":[],"truncated":false,"rejected_timestamps":0}),
            _ => panic!("unexpected fake request {path}"),
        };
        if path == "/library/sections" && mode == "owner_revoked" {
            body = json!({"MediaContainer":{"size":0,"Directory":[]}});
        }
        if path == "/api/v3/release" {
            match mode.as_str() {
                "quality_unknown" => {
                    body[0]["quality"] = serde_json::Value::Null;
                }
                "quality_different" => {
                    body[0]["quality"]["quality"]["id"] = json!(8);
                }
                "not_approved" => {
                    body[0]["approved"] = json!(false);
                }
                "rejected" => {
                    body[0]["rejected"] = json!(true);
                }
                "not_allowed" => {
                    body[0]["downloadAllowed"] = json!(false);
                }
                "reasons_unknown" => {
                    body[0]["rejections"] = serde_json::Value::Null;
                }
                "reasons_present" => {
                    body[0]["rejections"] = json!(["rejected"]);
                }
                "at_size_limit" => body[0]["size"] = json!(70 * 1024 * 1024),
                "below_size_limit" => body[0]["size"] = json!(70 * 1024 * 1024 - 1),
                "at_release_limit" | "over_release_limit" => {
                    let count = if mode == "at_release_limit" {
                        1000
                    } else {
                        1001
                    };
                    body = json!(
                        (0..count)
                            .map(|index| {
                                let mut release = body[0].clone();
                                release["guid"] = json!(format!("synthetic-release-{index}"));
                                release
                            })
                            .collect::<Vec<_>>()
                    );
                }
                "tiny" => {
                    body[0]["size"] = json!(1);
                }
                "no_indexer" => {
                    body[0]["indexerId"] = json!(0);
                }
                "no_guid" => {
                    body[0]["guid"] = json!(" ");
                }
                _ => {}
            }
        }
        if matches!(path.as_str(), "/api/v3/moviefile/12" | "/api/v3/release") {
            let unknown = match mode.as_str() {
                "quality_empty" => Some(json!({})),
                "quality_id_missing" => Some(json!({"quality":{}})),
                "quality_id_zero" => Some(json!({"quality":{"id":0}})),
                _ => None,
            };
            if let Some(quality) = unknown {
                if path == "/api/v3/release" {
                    body[0]["quality"] = quality;
                } else {
                    body["quality"] = quality;
                }
            }
        }
        if path == "/api/v3/moviefile/12"
            && self
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|p| p.as_str() == path)
                .count()
                == 2
            && let Some(clock) = self.advance_on_second_file.lock().unwrap().as_ref()
        {
            clock.store(131_000, Ordering::SeqCst);
        }
        let on_search = self.on_search.lock().unwrap().clone();
        Box::pin(async move {
            if path == "/api/v3/release"
                && let Some(store) = on_search
            {
                let mut current = store
                    .registered_file("films", "arr:films:movie:9")
                    .await
                    .unwrap()
                    .unwrap();
                current.fingerprint.agent.revision += 1;
                store
                    .register_file("films", "arr:films:movie:9", &current.fingerprint, 100)
                    .await
                    .unwrap();
            }
            Ok(Response {
                status: 200,
                body: serde_json::to_vec(&body).unwrap(),
            })
        })
    }
}
struct Setup {
    dir: tempfile::TempDir,
    config: Config,
    runtime: Runtime<Fake>,
    fake: Fake,
    store: pitcrew_store::Store,
    intake: pitcrew_intake_store::Store,
    clock: Arc<AtomicU64>,
}
async fn setup() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("secret");
    std::fs::write(&secret, "synthetic-token").unwrap();
    let config = Config::parse(&format!(
        r#"version=1
database={engine:?}
[stack]
enabled=true
intake_database={intake:?}
plex_auth_url="https://auth.invalid/"
[runtime]
search_enabled=true
[plex]
url="https://plex.invalid/"
owner_token={{file={secret:?}}}
server_id="server"
[[arr]]
id="films"
kind="radarr"
url="https://arr.invalid/"
api_key={{file={secret:?}}}
libraries=["1"]
[agent]
url="https://agent.invalid/"
shared_secret={{file={secret:?}}}
media_roots=["/media"]
storage_roots=["/storage"]
plex_log_timezone="UTC"
"#,
        engine = dir.path().join("engine.sqlite").to_str().unwrap(),
        intake = dir.path().join("intake.sqlite").to_str().unwrap(),
        secret = secret.to_str().unwrap()
    ))
    .unwrap();
    let store = pitcrew_store::Store::open(&config.database).await.unwrap();
    let intake = pitcrew_intake_store::Store::open(&config.stack.intake_database)
        .await
        .unwrap();
    let clock = Arc::new(AtomicU64::new(100_000));
    let time = clock.clone();
    let fake = Fake::new();
    let runtime = Runtime::open(
        &config,
        store.clone(),
        fake.clone(),
        Arc::new(move || time.load(Ordering::SeqCst)),
    )
    .await
    .unwrap()
    .unwrap();
    Setup {
        dir,
        config,
        runtime,
        fake,
        store,
        intake,
        clock,
    }
}
struct FakeAuth;
impl pitcrew_web::auth::Auth for FakeAuth {
    fn create_pin(&self) -> pitcrew_web::auth::AuthFuture<'_, pitcrew_web::auth::LoginPin> {
        Box::pin(async {
            Ok(pitcrew_web::auth::LoginPin {
                id: 1,
                code: "SYNTHETIC".into(),
            })
        })
    }
    fn poll(
        &self,
        _: u64,
    ) -> pitcrew_web::auth::AuthFuture<'_, Option<pitcrew_web::auth::Account>> {
        Box::pin(async {
            Ok(Some(pitcrew_web::auth::Account {
                id: "7".into(),
                token: "synthetic".into(),
            }))
        })
    }
    fn access<'a>(&'a self, _: &'a str) -> pitcrew_web::auth::AuthFuture<'a, Vec<String>> {
        Box::pin(async { Ok(vec!["1".into()]) })
    }
}
async fn call(
    app: &Arc<pitcrew_web::App>,
    method: &str,
    path: &str,
    cookie: &str,
    body: &str,
) -> (StatusCode, axum::http::HeaderMap, String) {
    let response = app
        .clone()
        .router()
        .oneshot(
            HttpRequest::builder()
                .method(method)
                .uri(path)
                .header("cookie", cookie)
                .header("origin", "http://127.0.0.1:7401")
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap();
    let (parts, body) = response.into_parts();
    (
        parts.status,
        parts.headers,
        String::from_utf8(to_bytes(body, 2_000_000).await.unwrap().to_vec()).unwrap(),
    )
}
fn cookie(headers: &axum::http::HeaderMap, name: &str) -> String {
    headers
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with(name))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .into()
}
fn hidden(body: &str, name: &str) -> String {
    body.split(&format!("name=\"{name}\" value=\""))
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .into()
}
async fn report(s: &Setup) -> (Arc<pitcrew_web::App>, String, i64) {
    let app = Arc::new(pitcrew_web::App::new(
        s.intake.clone(),
        Arc::new(FakeAuth),
        stack::web_config(&s.config).unwrap(),
        Arc::new(|| 100),
    ));
    let (_, headers, _) = call(&app, "POST", "/auth/start", "", "").await;
    let (_, headers, _) = call(
        &app,
        "POST",
        "/auth/poll",
        &cookie(&headers, "pitcrew_login"),
        "",
    )
    .await;
    let session = cookie(&headers, "pitcrew_session");
    let (status, _, body) = call(
        &app,
        "GET",
        "/report?item=plex:primary:1:5&issue=playback",
        &session,
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs([
            ("csrf", hidden(&body, "csrf")),
            ("nonce", hidden(&body, "nonce")),
            ("item", "plex:primary:1:5".into()),
            ("issue", "playback".into()),
            ("answer", "just_this_one".into()),
            ("text", "does not play".into()),
        ])
        .finish();
    let (status, _, body) = call(&app, "POST", "/report", &session, &body).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("Report received"));
    let id = s.intake.reports("7").await.unwrap()[0].id;
    (app, session, id)
}

#[tokio::test]
async fn web_report_to_diagnosis_budgeted_job_shadow_effect_and_progress() {
    let mut s = setup().await;
    assert!(s.fake.calls.lock().unwrap().is_empty());
    s.runtime.sync().await.unwrap();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(stack_path(&s))
            .unwrap()
            .permissions()
            .mode()
            & 0o077,
        0
    );
    let (app, session, id) = report(&s).await;
    let summary = s.runtime.tick().await.unwrap();
    assert_eq!(
        (
            summary.reports_examined,
            summary.authority_recorded,
            summary.reports_held,
            summary.effects_shadowed
        ),
        (1, 1, 0, 1)
    );
    let diagnosis = s.intake.diagnosis(id).await.unwrap().unwrap();
    assert_eq!(
        diagnosis.agent_file,
        Some(FileIdentity {
            id: 77,
            revision: 1
        })
    );
    assert_eq!(diagnosis.arr_file_id, 12);
    let jobs = s.store.jobs().await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        s.store.outbox(jobs[0]).await.unwrap()[0].status,
        pitcrew_store::OutboxStatus::Shadowed
    );
    let progress = s.intake.report_progress("7").await.unwrap();
    assert_eq!(progress[0].state, "working");
    let observations = s.store.observations().await.unwrap();
    for source in ["engine_decision", "decider:comparison", "decider:usage"] {
        assert_eq!(
            observations.iter().filter(|o| o.source == source).count(),
            1
        );
    }
    let (status, _, html) = call(&app, "GET", "/my-reports", &session, "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!html.contains("Fixed ·"));
    s.runtime.health("catalog", true).await.unwrap();
    assert!(
        s.store
            .observations()
            .await
            .unwrap()
            .iter()
            .any(|r| r.source == "admin:health"
                && r.payload
                    == json!({
                        "component":"catalog", "available":true,
                        "observed_at":100, "mode":"shadow"
                    }))
    );
    s.runtime.tick().await.unwrap(); // cursor wraps
    s.runtime.tick().await.unwrap(); // retry uses immutable saved decision
    assert_eq!(s.store.jobs().await.unwrap(), jobs);
    assert_eq!(
        s.store
            .observations()
            .await
            .unwrap()
            .iter()
            .filter(|o| o.source == "decider:usage")
            .count(),
        1
    );
}
fn stack_path(s: &Setup) -> std::path::PathBuf {
    s.dir.path().join("intake.catalog.json")
}

#[tokio::test]
async fn missing_access_changed_arr_and_stale_catalog_never_create_jobs() {
    for mode in ["denied", "changed_arr", "stale_catalog"] {
        let mut s = setup().await;
        s.runtime.sync().await.unwrap();
        let (_, _, id) = report(&s).await;
        *s.fake.mode.lock().unwrap() = mode.into();
        if mode == "stale_catalog" {
            s.clock.store(1_000_000, Ordering::SeqCst);
        }
        let summary = s.runtime.tick().await.unwrap();
        assert_eq!(summary.authority_recorded, 0, "{mode}");
        assert_eq!(summary.reports_held, 1, "{mode}");
        assert!(s.store.jobs().await.unwrap().is_empty(), "{mode}");
        assert!(s.intake.diagnosis(id).await.unwrap().is_none(), "{mode}");
    }
}

#[tokio::test]
async fn restart_recovers_saved_diagnosis_and_exports_without_rewriting_identity() {
    let mut s = setup().await;
    s.runtime.sync().await.unwrap();
    report(&s).await;
    s.runtime.tick().await.unwrap();
    let registered = s
        .store
        .registered_file("films", "arr:films:movie:9")
        .await
        .unwrap()
        .unwrap();
    let mut resumed = Runtime::open(
        &s.config,
        s.store.clone(),
        s.fake.clone(),
        Arc::new(|| 100_000),
    )
    .await
    .unwrap()
    .unwrap();
    resumed.sync().await.unwrap();
    resumed.tick().await.unwrap();
    assert_eq!(
        s.store
            .registered_file("films", "arr:films:movie:9")
            .await
            .unwrap()
            .unwrap(),
        registered
    );
    assert_eq!(s.store.jobs().await.unwrap().len(), 1);
}

#[tokio::test]
async fn owner_token_identity_alone_does_not_prove_library_access() {
    let mut s = setup().await;
    s.runtime.sync().await.unwrap();
    report(&s).await;
    *s.fake.mode.lock().unwrap() = "owner_revoked".into();
    let summary = s.runtime.tick().await.unwrap();
    assert_eq!(summary.authority_recorded, 0);
    assert!(s.store.jobs().await.unwrap().is_empty());
}

#[tokio::test]
async fn search_never_guesses_quality_or_release_approval() {
    for mode in [
        "quality_unknown",
        "quality_different",
        "not_approved",
        "rejected",
        "not_allowed",
        "reasons_unknown",
        "reasons_present",
        "tiny",
        "no_indexer",
        "no_guid",
    ] {
        let mut s = setup().await;
        s.runtime.sync().await.unwrap();
        report(&s).await;
        *s.fake.mode.lock().unwrap() = mode.into();
        let summary = s.runtime.tick().await.unwrap();
        assert_eq!(summary.effects_shadowed, 0, "{mode}");
        let job = s.store.jobs().await.unwrap()[0];
        assert!(s.store.outbox(job).await.unwrap().is_empty(), "{mode}");
    }
}

#[tokio::test]
async fn default_search_opt_out_still_diagnoses_without_spending_indexer_quota() {
    let mut s = setup().await;
    s.config.runtime.search_enabled = false;
    let mut runtime = Runtime::open(
        &s.config,
        s.store.clone(),
        s.fake.clone(),
        Arc::new(|| 100_000),
    )
    .await
    .unwrap()
    .unwrap();
    runtime.sync().await.unwrap();
    report(&s).await;
    let summary = runtime.tick().await.unwrap();
    assert_eq!(summary.authority_recorded, 1);
    assert_eq!(summary.effects_shadowed, 0);
    assert!(
        !s.fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|p| p == "/api/v3/release")
    );
}

#[tokio::test]
async fn another_viewers_grant_never_authorizes_the_initiating_viewers_job() {
    let mut s = setup().await;
    s.config.runtime.search_enabled = false;
    s.runtime = Runtime::open(
        &s.config,
        s.store.clone(),
        s.fake.clone(),
        Arc::new(|| 100_000),
    )
    .await
    .unwrap()
    .unwrap();
    s.runtime.sync().await.unwrap();
    let (_, _, id) = report(&s).await;
    s.runtime.tick().await.unwrap();
    let original = s.intake.report_for_diagnosis(id).await.unwrap().unwrap();
    s.intake
        .persist_intake(
            "web",
            "second-viewer",
            "8",
            "browser",
            &original.intake.response,
            100,
        )
        .await
        .unwrap();
    *s.fake.mode.lock().unwrap() = "viewer_switch".into();
    s.config.runtime.search_enabled = true;
    let mut resumed = Runtime::open(
        &s.config,
        s.store.clone(),
        s.fake.clone(),
        Arc::new(|| 100_000),
    )
    .await
    .unwrap()
    .unwrap();
    resumed.sync().await.unwrap();
    resumed.tick().await.unwrap();
    let job = s.store.jobs().await.unwrap()[0];
    assert_eq!(s.store.load_job(job).await.unwrap().spec.viewer, "7");
    assert!(
        s.store.outbox(job).await.unwrap().is_empty(),
        "another viewer's access authorized the initiating viewer's job"
    );
}

#[tokio::test]
async fn registry_change_during_release_search_prevents_a_proposal() {
    let mut s = setup().await;
    s.runtime.sync().await.unwrap();
    report(&s).await;
    *s.fake.on_search.lock().unwrap() = Some(s.store.clone());
    let summary = s.runtime.tick().await.unwrap();
    assert_eq!(summary.effects_shadowed, 0);
    let job = s.store.jobs().await.unwrap()[0];
    assert!(s.store.outbox(job).await.unwrap().is_empty());
}

#[tokio::test]
async fn admin_journal_cannot_target_another_stacks_numeric_ids() {
    let mut s = setup().await;
    let journal = s.dir.path().join("admin.sqlite");
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&journal)
                .create_if_missing(true),
        )
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../../pitcrew-admin/src/schema.sql"))
        .execute(&pool)
        .await
        .unwrap();
    s.config.admin = Some(pitcrew_config::admin::AdminUi {
        database: journal,
        engine_database: s.config.database.clone(),
        intake_database: s.config.stack.intake_database.clone(),
        bind: "127.0.0.1:0".parse().unwrap(),
        public_origin: "https://owner.invalid".into(),
        plex_auth_url: "https://auth.invalid".into(),
        client_id: "test".into(),
        admin_account_ids: vec!["99".into()],
        api: None,
        rates: Default::default(),
    });
    assert!(
        stack::admin_consumer(&s.config, Some(&s.runtime.bridge))
            .await
            .unwrap()
            .is_some()
    );
    let other = s.dir.path().join("another.sqlite");
    pitcrew_store::Store::open(&other).await.unwrap();
    s.config.admin.as_mut().unwrap().engine_database = other;
    assert!(
        stack::admin_consumer(&s.config, Some(&s.runtime.bridge))
            .await
            .is_err(),
        "mismatched engine source accepted"
    );
    s.config.admin.as_mut().unwrap().engine_database = s.config.database.clone();
    s.config.admin.as_mut().unwrap().intake_database = s.config.database.clone();
    assert!(
        stack::admin_consumer(&s.config, Some(&s.runtime.bridge))
            .await
            .is_err(),
        "mismatched intake source accepted"
    );
}

#[tokio::test]
async fn independent_report_and_job_paging_cannot_starve_selection() {
    let mut s = setup().await;
    s.config.runtime.batch_size = 1;
    let options = stack::bridge_options(&s.config);
    s.store
        .create_job(
            &pitcrew_store::NewJob {
                id: 1,
                viewer: "unrelated".into(),
                resource: "unrelated".into(),
                library: 1,
                original: FileIdentity {
                    id: 99,
                    revision: 1,
                },
                constraints: Default::default(),
                policy: options.policy,
                timeouts: options.timeouts,
                created_at: 100,
            },
            "unrelated",
            options.budgets,
        )
        .await
        .unwrap();
    s.runtime = Runtime::open(
        &s.config,
        s.store.clone(),
        s.fake.clone(),
        Arc::new(|| 100_000),
    )
    .await
    .unwrap()
    .unwrap();
    s.runtime.sync().await.unwrap();
    report(&s).await;
    s.intake
        .persist_intake("web", "ignored", "7", "browser", &json!({}), 100)
        .await
        .unwrap();
    let mut effects = 0;
    for _ in 0..9 {
        effects += s.runtime.tick().await.unwrap().effects_shadowed;
    }
    assert_eq!(
        effects, 1,
        "out-of-phase paging never selected the authorized job"
    );
}

#[tokio::test]
async fn catalog_expiring_during_final_file_read_cannot_record_authority() {
    let mut s = setup().await;
    s.config.runtime.snapshot_ttl_seconds = 30;
    let clock = s.clock.clone();
    s.runtime = Runtime::open(
        &s.config,
        s.store.clone(),
        s.fake.clone(),
        Arc::new(move || clock.load(Ordering::SeqCst)),
    )
    .await
    .unwrap()
    .unwrap();
    s.runtime.sync().await.unwrap();
    report(&s).await;
    *s.fake.advance_on_second_file.lock().unwrap() = Some(s.clock.clone());
    let summary = s.runtime.tick().await.unwrap();
    assert_eq!(summary.authority_recorded, 0);
    assert!(s.store.jobs().await.unwrap().is_empty());
}

#[tokio::test]
async fn access_refresh_requires_successful_bounded_responses_for_shared_and_owner_accounts() {
    for owner in [false, true] {
        for (status, padded_bytes, unavailable, allowed) in [
            (199, None, false, false),
            (200, None, false, true),
            (299, None, false, true),
            (300, None, false, false),
            (401, None, false, false),
            (503, None, false, false),
            (200, None, true, false),
            (200, Some(1024 * 1024), false, true),
            (200, Some(1024 * 1024 + 1), false, false),
        ] {
            let mut s = setup().await;
            s.runtime.sync().await.unwrap();
            report(&s).await;
            // An owner response must stand on its own; shared access grants nothing.
            if owner {
                *s.fake.mode.lock().unwrap() = "denied".into();
            }
            *s.fake.access_reply.lock().unwrap() = Some(AccessReply {
                owner,
                status,
                padded_bytes,
                unavailable,
            });
            let summary = s.runtime.tick().await.unwrap();
            let context = format!(
                "owner={owner} status={status} bytes={padded_bytes:?} unavailable={unavailable}"
            );
            assert_eq!(
                summary.authority_recorded,
                usize::from(allowed),
                "{context}"
            );
            assert_eq!(summary.reports_held, usize::from(!allowed), "{context}");
            assert_eq!(summary.effects_shadowed, usize::from(allowed), "{context}");
            assert_eq!(
                s.store.jobs().await.unwrap().len(),
                usize::from(allowed),
                "{context}"
            );
        }
    }
}

#[tokio::test]
async fn release_count_and_size_bounds_preserve_only_eligible_shadow_proposals() {
    for (mode, allowed) in [
        ("at_release_limit", true),
        ("over_release_limit", false),
        ("at_size_limit", true),
        ("below_size_limit", false),
    ] {
        let mut s = setup().await;
        s.runtime.sync().await.unwrap();
        report(&s).await;
        *s.fake.mode.lock().unwrap() = mode.into();
        let summary = s.runtime.tick().await.unwrap();
        assert_eq!(summary.authority_recorded, 1, "{mode}");
        assert_eq!(summary.effects_shadowed, usize::from(allowed), "{mode}");
        let jobs = s.store.jobs().await.unwrap();
        assert_eq!(jobs.len(), 1, "{mode}");
        assert_eq!(
            s.store.outbox(jobs[0]).await.unwrap().len(),
            usize::from(allowed),
            "{mode}"
        );
    }
}

#[tokio::test]
async fn matching_unknown_quality_objects_never_establish_same_quality() {
    for mode in ["quality_empty", "quality_id_missing", "quality_id_zero"] {
        let mut s = setup().await;
        s.runtime.sync().await.unwrap();
        report(&s).await;
        *s.fake.mode.lock().unwrap() = mode.into();
        let summary = s.runtime.tick().await.unwrap();
        assert_eq!(summary.effects_shadowed, 0, "{mode}");
        for job in s.store.jobs().await.unwrap() {
            assert!(s.store.outbox(job).await.unwrap().is_empty(), "{mode}");
        }
    }
}

#[tokio::test]
async fn runtime_revalidates_media_access_and_catalog_at_each_authority_boundary() {
    for case in [
        "zero_identity",
        "future_after_clock_correction",
        "stale_after_logs",
        "age_exact",
        "older_subsecond",
        "grant_expired",
        "catalog_expired",
        "final_age_exact",
        "final_age_stale",
        "final_future",
        "final_grant_expired",
    ] {
        let mut s = setup().await;
        assert_eq!(s.config.stack.evidence_max_age_seconds, 300);
        assert_eq!(s.config.runtime.snapshot_ttl_seconds, 900);
        let mut started = 100_000;
        let mut reply = MediaReply {
            file: FileIdentity {
                id: 77,
                revision: 1,
            },
            observed_at_unix_ms: 100_000,
            clock: s.clock.clone(),
            after_evidence: None,
            after_logs: None,
            after_second_file: None,
        };
        match case {
            "zero_identity" => reply.file.id = 0,
            "future_after_clock_correction" => {
                // The collector accepts the sample, then the wall clock moves
                // backwards while the independent Plex log read is in flight.
                reply.observed_at_unix_ms = 100_001;
                reply.after_evidence = Some(100_001);
                reply.after_logs = Some(100_000);
            }
            "stale_after_logs" => {
                // Cross the collector's five-minute boundary after it checked
                // the media response, so runtime revalidation must reject it.
                started = 400_000;
                reply.after_logs = Some(400_001);
            }
            "age_exact" => started = 400_000,
            "older_subsecond" => started = 100_123,
            "grant_expired" => {
                s.config.stack.evidence_max_age_seconds = 1;
                reply.after_logs = Some(101_000);
            }
            "catalog_expired" => {
                s.config.runtime.snapshot_ttl_seconds = 30;
                reply.after_logs = Some(130_000);
            }
            "final_age_exact" | "final_age_stale" => {
                reply.observed_at_unix_ms = 50_000;
                reply.after_second_file = Some(if case == "final_age_exact" {
                    350_000
                } else {
                    350_001
                });
            }
            "final_future" => {
                started = 100_001;
                reply.observed_at_unix_ms = 100_001;
                reply.after_second_file = Some(100_000);
            }
            "final_grant_expired" => reply.after_second_file = Some(400_000),
            _ => unreachable!(),
        }
        let final_check = reply.after_second_file.is_some();
        let clock = s.clock.clone();
        s.runtime = Runtime::open(
            &s.config,
            s.store.clone(),
            s.fake.clone(),
            Arc::new(move || clock.load(Ordering::SeqCst)),
        )
        .await
        .unwrap()
        .unwrap();
        s.runtime.sync().await.unwrap();
        let (_, _, report_id) = report(&s).await;
        // Refresh the catalog after setting the test's initial time. An old
        // snapshot must not reject the sample before the evidence checks do.
        s.clock.store(started, Ordering::SeqCst);
        s.runtime.sync().await.unwrap();
        *s.fake.media_reply.lock().unwrap() = Some(reply);
        s.fake.calls.lock().unwrap().clear();
        let summary = s.runtime.tick().await.unwrap();
        let allowed = matches!(case, "age_exact" | "older_subsecond" | "final_age_exact");
        assert_eq!(summary.reports_examined, 1, "{case}");
        assert_eq!(summary.authority_recorded, usize::from(allowed), "{case}");
        assert_eq!(summary.reports_held, usize::from(!allowed), "{case}");
        assert_eq!(summary.effects_shadowed, usize::from(allowed), "{case}");
        assert_eq!(
            s.store.jobs().await.unwrap().len(),
            usize::from(allowed),
            "{case}"
        );
        if !allowed {
            let observations = s.store.observations().await.unwrap();
            let held: Vec<_> = observations
                .iter()
                .filter(|o| {
                    o.source == "admin:health" && o.payload["report_id"] == json!(report_id)
                })
                .collect();
            assert_eq!(held.len(), 1, "{case}");
            let reason = if final_check {
                "evidence expired before authority commit"
            } else {
                "evidence expired or changed during diagnosis"
            };
            assert_eq!(held[0].payload["reason"], reason, "{case}");
            let calls = s.fake.calls.lock().unwrap();
            assert_eq!(
                calls
                    .iter()
                    .filter(|path| path.as_str() == "/api/v3/moviefile/12")
                    .count(),
                if final_check { 2 } else { 1 },
                "{case}: evidence reached a later fingerprint check"
            );
            assert!(
                !calls.iter().any(|path| path == "/api/v3/release"),
                "{case}"
            );
        } else {
            let registered = s
                .store
                .registered_file("films", "arr:films:movie:9")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                registered.observed_at,
                if case == "final_age_exact" { 50 } else { 100 },
                "{case}: registry freshness must retain the oldest observation"
            );
        }
    }
}
