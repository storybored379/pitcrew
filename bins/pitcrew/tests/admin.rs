use pitcrew::App;
use pitcrew_domain::{media::ReleaseIdentity, *};
use pitcrew_engine::webhook::{MAX_BODY, Secret};
use pitcrew_store::{delivery::NotificationPayload, policy::*, *};
use serde_json::{Value, json};
use std::collections::BTreeMap;
const ADMIN: &str = "synthetic-admin-secret-0123456789abcdef";
const CONTROL: &str = "synthetic-replay-secret-0123456789abcd";
async fn app() -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().unwrap();
    let app = App::new(
        Store::open(dir.path().join("db")).await.unwrap(),
        vec![],
        Some(Secret::new(CONTROL).unwrap()),
        BudgetLimits {
            global_hour: 10,
            global_day: 10,
            viewer_hour: 10,
            viewer_day: 10,
        },
        2,
        BTreeMap::new(),
    )
    .unwrap();
    (dir, app)
}
async fn post(app: &App, target: &str, body: Value) -> (u16, Value) {
    app.handle(
        "POST",
        target,
        ADMIN,
        &serde_json::to_vec(&body).unwrap(),
        100,
    )
    .await
}

#[tokio::test]
async fn policy_api_requires_separate_admin_credential_and_ignores_client_authority_claims() {
    let (_dir, app) = app().await;
    assert_eq!(
        app.handle("GET", "/v1/admin/exclusions/client-a", CONTROL, b"", 100)
            .await
            .0,
        401
    );
    let app = app.with_admin(Secret::new(ADMIN).unwrap());
    for secret in ["", CONTROL, "wrong"] {
        assert_eq!(
            app.handle("GET", "/v1/admin/exclusions/client-a", secret, b"", 100)
                .await
                .0,
            401
        );
    }
    let record = app
        .store
        .exclude_release(
            &NewExclusion {
                client: "client-a".into(),
                release: 5,
                identity: ReleaseIdentity::default(),
                kind: ExclusionKind::Quarantine,
                reason: "Failed decode".into(),
                evidence: json!({"intent":7}),
                pinned: false,
                created_at: 1,
            },
            "excluded-a",
        )
        .await
        .unwrap();
    let (status, body) = app
        .handle("GET", "/v1/admin/exclusions/client-a", ADMIN, b"", 100)
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["exclusions"][0]["spec"]["release"], 5);
    assert_eq!(body["exclusions"][0]["spec"]["reason"], "Failed decode");
    assert_eq!(
        body["exclusions"][0]["spec"]["evidence"],
        json!({"intent":7})
    );
    assert_eq!(
        post(
            &app,
            "/v1/admin/exclusions/release",
            json!({"id":record.id,"note":" "})
        )
        .await
        .0,
        400
    );
    assert_eq!(
        post(
            &app,
            "/v1/admin/exclusions/release",
            json!({"id":record.id,"note":"Reviewed","role":"Admin","actor":"forged"})
        )
        .await
        .0,
        400
    );
    assert_eq!(
        post(
            &app,
            "/v1/admin/exclusions/release",
            json!({"id":record.id,"note":"Reviewed source"})
        )
        .await,
        (200, json!({"released":true}))
    );
    assert_eq!(
        post(
            &app,
            "/v1/admin/exclusions/release",
            json!({"id":record.id,"note":"Reviewed source"})
        )
        .await,
        (200, json!({"released":false}))
    );
    assert_eq!(
        post(
            &app,
            "/v1/admin/exclusions/release",
            json!({"id":9999,"note":"Reviewed"})
        )
        .await
        .0,
        404
    );
    let audit = app.store.policy_audit().await.unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].actor, "configured-admin");
    assert_eq!(audit[0].note, "Reviewed source");
    assert_eq!(audit[0].at, 100);
    assert!(
        app.store
            .exclusions("client-a", 100)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        app.handle("GET", "/v1/admin/exclusions/", ADMIN, b"", 100)
            .await
            .0,
        400
    );
    assert_eq!(
        app.handle("GET", "/v1/admin/unknown", ADMIN, b"", 100)
            .await
            .0,
        404
    );
    assert_eq!(
        app.handle("PUT", "/v1/admin/exclusions/client-a", ADMIN, b"", 100)
            .await
            .0,
        404
    );
    assert_eq!(post(&app, "/v1/admin/unknown", json!({})).await.0, 404);
    assert_eq!(
        app.handle("POST", "/v1/admin/exclusions/release", ADMIN, b"{bad", 100)
            .await
            .0,
        400
    );
    assert_eq!(
        app.handle(
            "POST",
            "/v1/admin/exclusions/release",
            ADMIN,
            &vec![b' '; MAX_BODY + 1],
            100
        )
        .await
        .0,
        413
    );
    assert_eq!(
        app.handle(
            "POST",
            "/v1/admin/unknown",
            ADMIN,
            &vec![b' '; MAX_BODY],
            100
        )
        .await
        .0,
        404
    );
}

#[tokio::test]
async fn capabilities_can_be_read_and_reset_with_an_audited_note() {
    let (_dir, app) = app().await;
    let app = app.with_admin(Secret::new(ADMIN).unwrap());
    assert_eq!(
        app.handle("GET", "/v1/admin/capabilities/client-a", ADMIN, b"", 100)
            .await,
        (200, json!({"ledger":null}))
    );
    app.store
        .observe_client_version("client-a", "4.0.19.2979", 1)
        .await
        .unwrap();
    app.store
        .record_capabilities(
            "client-a",
            "4.0.19.2979",
            &ClientCapabilities {
                external_queue_actor: true,
                held_category: None,
            },
            &json!({"history":7}),
            2,
        )
        .await
        .unwrap();
    let (status, body) = app
        .handle("GET", "/v1/admin/capabilities/client-a", ADMIN, b"", 100)
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["ledger"]["capabilities"]["external_queue_actor"], true);
    assert_eq!(
        post(
            &app,
            "/v1/admin/capabilities/reset",
            json!({"client":"client-a","note":" "})
        )
        .await
        .0,
        400
    );
    assert_eq!(
        post(
            &app,
            "/v1/admin/capabilities/reset",
            json!({"client":"client-a","note":"Recertifying this client"})
        )
        .await,
        (200, json!({"reset":true}))
    );
    assert!(
        app.store
            .client_ledger("client-a")
            .await
            .unwrap()
            .unwrap()
            .capabilities
            .is_none()
    );
    assert_eq!(
        app.store.policy_audit().await.unwrap()[0].action,
        "reset_capabilities"
    );
}

#[tokio::test]
async fn delivery_status_and_admin_resend_reuse_the_existing_intent() {
    let (_dir, app) = app().await;
    let app = app.with_admin(Secret::new(ADMIN).unwrap());
    app.store
        .create_job(
            &NewJob {
                id: 1,
                viewer: "viewer-a".into(),
                resource: "item-a".into(),
                library: 1,
                original: FileIdentity { id: 2, revision: 3 },
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
            "new",
            BudgetLimits {
                global_hour: 4,
                global_day: 4,
                viewer_hour: 4,
                viewer_day: 4,
            },
        )
        .await
        .unwrap();
    let lease = app.store.acquire_lease(1, "worker", 1, 500).await.unwrap();
    app.store
        .apply_event(
            &lease,
            0,
            "grab",
            Event::SelectRelease {
                release: 5,
                access: Access::Granted {
                    library: 1,
                    expires_at: 900,
                },
            },
            1,
        )
        .await
        .unwrap();
    app.store
        .apply_event(&lease, 1, "fail", Event::Fail(Failure::Cancelled), 2)
        .await
        .unwrap();
    let key = IntentKey {
        job: 1,
        sequence: 2,
    };
    app.store
        .prepare_notification(
            key,
            &NotificationPayload::Admin {
                notifier: "admin-a".into(),
                title: "Failed".into(),
                body: "Review required".into(),
            },
        )
        .await
        .unwrap();
    app.store
        .finish_intent(&lease, key, OutboxStatus::Uncertain, 3)
        .await
        .unwrap();
    let (status, body) = app
        .handle("GET", "/v1/admin/deliveries/1", ADMIN, b"", 100)
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["deliveries"].as_array().unwrap().len(), 2);
    assert_eq!(body["deliveries"][0]["status"], "uncertain");
    assert_eq!(
        app.handle("GET", "/v1/admin/deliveries/999", ADMIN, b"", 100)
            .await
            .0,
        404
    );
    assert_eq!(
        app.handle("GET", "/v1/admin/deliveries/abc", ADMIN, b"", 100)
            .await
            .0,
        400
    );
    let body = json!({"job":1,"sequence":2,"request_key":"resend-a","note":"Owner requested another send"});
    assert_eq!(
        post(&app, "/v1/admin/notifications/resend", body.clone()).await,
        (200, json!({"scheduled":true}))
    );
    assert_eq!(
        post(&app, "/v1/admin/notifications/resend", body).await,
        (200, json!({"scheduled":false}))
    );
    assert_eq!(post(&app,"/v1/admin/notifications/resend",json!({"job":1,"sequence":3,"request_key":"resend-a","note":"Owner requested another send"})).await.0,409);
    assert_eq!(
        app.store.outbox(1).await.unwrap()[1].status,
        OutboxStatus::Pending
    );
    assert_eq!(
        app.store.policy_audit().await.unwrap()[0].action,
        "notification_resend"
    );
    assert_eq!(
        app.handle("GET", "/v1/shadow/invalid", ADMIN, b"", 100)
            .await
            .0,
        401
    );
}

#[test]
fn administrator_secret_is_optional_and_validated_without_resolving_it() {
    let config = pitcrew_config::Config::parse("version=1").unwrap();
    assert!(config.engine.admin_secret.is_none());
    assert!(
        pitcrew_config::Config::parse(
            "version=1\n[engine]\nadmin_secret={env='PITCREW_SYNTHETIC_ADMIN_SECRET'}"
        )
        .unwrap()
        .engine
        .admin_secret
        .is_some()
    );
    let error = pitcrew_config::Config::parse(
        "version=1\n[engine]\nadmin_secret={env='invalid secret name'}",
    )
    .unwrap_err();
    assert!(error.to_string().contains("engine.admin_secret"));
    assert!(!error.to_string().contains("invalid secret name"));
}

#[tokio::test]
async fn actual_shadow_binary_wires_dedicated_admin_credential() {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    let secret = dir.path().join("admin-secret");
    let control = dir.path().join("control-secret");
    std::fs::write(&secret, ADMIN).unwrap();
    std::fs::write(&control, CONTROL).unwrap();
    std::fs::write(&config, format!("version=1\ndatabase={:?}\n[engine]\nlisten=\"127.0.0.1:0\"\nadmin_secret={{file={secret:?}}}\ncontrol_secret={{file={control:?}}}\n", dir.path().join("db"))).unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_pitcrew"))
        .arg("--config")
        .arg(&config)
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = tokio::io::BufReader::new(child.stderr.take().unwrap()).lines();
    let line = tokio::time::timeout(std::time::Duration::from_secs(30), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let startup: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(startup["effects_enabled"], false);
    for (credential, status) in [(ADMIN, 200), (CONTROL, 401)] {
        let mut stream = tokio::net::TcpStream::connect(startup["listen"].as_str().unwrap())
            .await
            .unwrap();
        stream.write_all(format!("GET /v1/admin/exclusions/client-a HTTP/1.1\r\nHost: local.test\r\nAuthorization: Bearer {credential}\r\n\r\n").as_bytes()).await.unwrap();
        let mut response = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            stream.read_to_string(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            response.starts_with(&format!("HTTP/1.1 {status}")),
            "{response}"
        );
        assert!(!response.contains(ADMIN));
    }
    child.kill().await.unwrap();
    child.wait().await.unwrap();
}
