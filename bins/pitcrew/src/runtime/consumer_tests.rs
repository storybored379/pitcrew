use super::*;
use pitcrew_domain::{
    FileIdentity,
    media::{MediaEvidence, MediaProbe, ProbeOutcome},
};
use pitcrew_intake::{IntakeOutcome, IntakeResponse, IssueKind, ParsedIssue};
use pitcrew_upstream::{Method, Response};
use serde_json::json;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::sync::Mutex;

// The real readers, collectors and controller run against an in-process
// transport. No request opens a socket or accepts an upstream mutation.
struct Evidence {
    owner_id: u64,
    calls: Arc<Mutex<Vec<String>>>,
}
impl Transport for Evidence {
    fn send(&self, request: Request) -> ResponseFuture<'_> {
        assert_eq!(request.method, Method::Get);
        assert!(request.body.is_none());
        self.calls.lock().unwrap().push(request.url.path().into());
        let body = match request.url.path() {
            "/library/sections" => {
                json!({"MediaContainer":{"size":1,"Directory":[{"key":"1","title":"Films","type":"movie"}]}})
            }
            "/library/sections/1/all" => {
                json!({"MediaContainer":{"size":1,"offset":0,"totalSize":1,"Metadata":[{"ratingKey":"5","type":"movie","title":"Synthetic Film","Media":[{"videoResolution":"1080","Part":[{"file":"/media/film.mkv"}]}]}]}})
            }
            "/api/v3/movie" => {
                json!([{"id":9,"title":"Synthetic Film","movieFile":{"id":12,"path":"/media/film.mkv","movieId":9}}])
            }
            "/api/v3/movie/9" => json!({"movieFileId":12}),
            "/api/v3/moviefile/12" => {
                json!({"id":12,"path":"/media/film.mkv","movieId":9,"size":100000000,"dateAdded":"2026-01-01T00:00:00Z","quality":{"quality":{"id":7},"revision":{"version":1}}})
            }
            "/api/v2/user" => json!({"id":self.owner_id}),
            "/api/servers/server/shared_servers" => {
                return Box::pin(async {
                    Ok(Response {
                        status: 200,
                        body: br#"<MediaContainer size="1"><SharedServer userID="7"><Section key="1" shared="1"/></SharedServer></MediaContainer>"#.to_vec(),
                    })
                });
            }
            "/v1/evidence" => serde_json::to_value(MediaEvidence {
                file: FileIdentity {
                    id: 77,
                    revision: 1,
                },
                observed_at_unix_ms: 100_123,
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
            })
            .unwrap(),
            "/v1/plexfails" => {
                json!({"failures":[],"truncated":false,"rejected_timestamps":0})
            }
            path => panic!("unexpected fake request {path}"),
        };
        Box::pin(async move {
            Ok(Response {
                status: 200,
                body: serde_json::to_vec(&body).unwrap(),
            })
        })
    }
}

fn config(dir: &tempfile::TempDir) -> Config {
    let secret = dir.path().join("secret");
    std::fs::write(&secret, "synthetic-test-token").unwrap();
    Config::parse(&format!(
        r#"version=1
database={engine:?}
[stack]
enabled=true
intake_database={intake:?}
plex_auth_url="https://auth.invalid/"
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
        secret = secret.to_str().unwrap(),
    ))
    .unwrap()
}

#[tokio::test]
async fn tick_consumes_owner_intent_after_refresh_and_audits_in_seconds() {
    for action in ["deny", "approve"] {
        let dir = tempfile::tempdir().unwrap();
        let config = config(&dir);
        let store = Store::open(&config.database).await.unwrap();
        let mut runtime = Runtime::open(
            &config,
            store.clone(),
            Evidence {
                owner_id: 99,
                calls: Arc::default(),
            },
            Arc::new(|| 100_123),
        )
        .await
        .unwrap()
        .unwrap();
        runtime.sync().await.unwrap();
        let intake = &runtime.intake;
        intake
            .persist_intake(
                "web",
                "report",
                "7",
                "browser",
                &serde_json::to_value(IntakeResponse {
                    outcome: IntakeOutcome::Report,
                    issues: vec![ParsedIssue {
                        item_id: "plex:primary:1:5".into(),
                        kinds: vec![IssueKind::Playback],
                    }],
                    clarification: None,
                    report_ids: vec![],
                    reply: "Report received".into(),
                    suspicious_input: false,
                })
                .unwrap(),
                100,
            )
            .await
            .unwrap();
        let report = intake
            .stored_intake("web", "report", "7")
            .await
            .unwrap()
            .unwrap()
            .id;
        assert!(intake.diagnosis(report).await.unwrap().is_none());
        assert!(store.jobs().await.unwrap().is_empty());

        let journal = dir.path().join("admin.sqlite");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&journal)
                    .create_if_missing(true),
            )
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../../../pitcrew-admin/src/schema.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO admin_requests VALUES ('owner-intent','owner',?,'Reviewed the report','pending_controller',100,100)")
            .bind(json!({"action":action,"report":report,"decided_at":100}).to_string())
            .execute(&pool).await.unwrap();
        let consumer = pitcrew_bridge::admin::Consumer::open(
            journal,
            runtime.bridge.clone(),
            vec!["owner".into()],
        )
        .await
        .unwrap();

        let summary = runtime.tick_with_consumer(Some(&consumer)).await.unwrap();
        assert_eq!(summary.reports_examined, 1, "{action}");
        assert_eq!(summary.authority_recorded, 1, "{action}");
        assert_eq!(summary.reports_held, 0, "{action}");
        let diagnosis = runtime.intake.diagnosis(report).await.unwrap().unwrap();
        assert_eq!(diagnosis.decided_at, 100);
        let request: (String, i64) = sqlx::query_as(
            "SELECT state,updated_at FROM admin_requests WHERE request_key='owner-intent'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(request, ("controller_applied".into(), 100), "{action}");
        let audit: Vec<(String, i64)> =
            sqlx::query_as("SELECT event,at FROM admin_audit ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(audit.len(), 2, "{action}");
        assert_eq!(audit[0], ("controller_claimed".into(), 100));
        assert!(audit[1].0.starts_with("controller_applied: "));
        assert_eq!(audit[1].1, 100);
        assert_eq!(
            store.jobs().await.unwrap().len(),
            usize::from(action == "approve")
        );

        // A later pass cannot repeat the owner action or bypass a denial.
        runtime.tick_with_consumer(Some(&consumer)).await.unwrap();
        assert_eq!(
            store.jobs().await.unwrap().len(),
            usize::from(action == "approve")
        );
        assert_eq!(consumer.tick(100).await.unwrap(), 0);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM admin_audit")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 2);
    }
}

#[tokio::test]
async fn zero_owner_account_does_not_bypass_shared_library_denial() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir);
    let store = Store::open(&config.database).await.unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = Runtime::open(
        &config,
        store.clone(),
        Evidence {
            owner_id: 0,
            calls: calls.clone(),
        },
        Arc::new(|| 100_123),
    )
    .await
    .unwrap()
    .unwrap();
    runtime.sync().await.unwrap();
    calls.lock().unwrap().clear();
    runtime
        .intake
        .persist_intake(
            "web",
            "zero-account",
            "0",
            "browser",
            &serde_json::to_value(IntakeResponse {
                outcome: IntakeOutcome::Report,
                issues: vec![ParsedIssue {
                    item_id: "plex:primary:1:5".into(),
                    kinds: vec![IssueKind::Playback],
                }],
                clarification: None,
                report_ids: vec![],
                reply: "Report received".into(),
                suspicious_input: false,
            })
            .unwrap(),
            100,
        )
        .await
        .unwrap();
    let report = runtime
        .intake
        .stored_intake("web", "zero-account", "0")
        .await
        .unwrap()
        .unwrap()
        .id;

    let summary = runtime.tick().await.unwrap();
    assert_eq!(summary.reports_examined, 1);
    assert_eq!(summary.authority_recorded, 0);
    assert_eq!(summary.reports_held, 1);
    // An id of zero must not trigger even an owner section lookup, let alone
    // the arr fingerprint or valid agent evidence provided by this transport.
    assert_eq!(
        *calls.lock().unwrap(),
        vec!["/api/servers/server/shared_servers", "/api/v2/user",]
    );
    assert!(runtime.intake.diagnosis(report).await.unwrap().is_none());
    assert!(store.jobs().await.unwrap().is_empty());
    let held = store
        .observations()
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.source == "admin:health" && row.payload["report_id"] == report)
        .unwrap();
    assert_eq!(held.payload["reason"], "current access required");
    assert_eq!(held.payload["available"], false);
}

#[tokio::test]
async fn malformed_saved_report_is_counted_as_held_without_authority_or_repair() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir);
    let store = Store::open(&config.database).await.unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = Runtime::open(
        &config,
        store.clone(),
        Evidence {
            owner_id: 99,
            calls: calls.clone(),
        },
        Arc::new(|| 100_123),
    )
    .await
    .unwrap()
    .unwrap();
    runtime.sync().await.unwrap();
    calls.lock().unwrap().clear();
    // This represents a malformed legacy row: JSON storage is valid, while
    // its report response cannot be decoded as the current IntakeResponse.
    let response = json!({
        "outcome":"report",
        "issues":"plex:primary:1:5",
        "clarification":null,
        "report_ids":[],
        "reply":"Report received",
        "suspicious_input":false,
    });
    assert!(serde_json::from_value::<IntakeResponse>(response.clone()).is_err());
    runtime
        .intake
        .persist_intake("web", "malformed", "7", "browser", &response, 100)
        .await
        .unwrap();
    let report = runtime
        .intake
        .stored_intake("web", "malformed", "7")
        .await
        .unwrap()
        .unwrap()
        .id;

    let summary = runtime.tick().await.unwrap();
    assert_eq!(summary.reports_examined, 1);
    assert_eq!(summary.reports_held, 1);
    assert_eq!(summary.authority_recorded, 0);
    assert_eq!(summary.effects_shadowed, 0);
    assert!(runtime.intake.diagnosis(report).await.unwrap().is_none());
    assert!(store.jobs().await.unwrap().is_empty());
    assert_eq!(
        *calls.lock().unwrap(),
        vec!["/api/servers/server/shared_servers", "/api/v2/user",]
    );
}
