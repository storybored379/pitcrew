use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use pitcrew_bot::server::*;
use pitcrew_domain::channels::{ChannelIdentity, IncomingMessage};
use pitcrew_intake::{AuthorizedViewer, CatalogItem, IntakeOutcome, Resolution};
use pitcrew_intake_store::Store;
use serde_json::json;
use std::sync::Arc;
use tower::ServiceExt;
const TOKEN: &str = "synthetic-token-with-at-least-32-bytes";
fn config(path: &std::path::Path) -> TestConfig {
    TestConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        database: path.into(),
        catalog: catalog(),
        principals: vec![TestPrincipal {
            bearer_token: TOKEN.into(),
            viewer: viewer(),
            expires_at: 1000,
        }],
    }
}
fn viewer() -> AuthorizedViewer {
    AuthorizedViewer {
        account_id: "viewer-a".into(),
        library_ids: vec!["films".into()],
    }
}
fn catalog() -> Vec<CatalogItem> {
    vec![CatalogItem {
        id: "glass".into(),
        title: "The Glass Harbor".into(),
        aliases: vec![],
        library_id: "films".into(),
        library_name: "Films".into(),
        resolution: Resolution::Hd,
        season: None,
        episode: None,
    }]
}
fn message() -> TestMessage {
    TestMessage {
        message_id: "one".into(),
        conversation_id: "conversation".into(),
        text: "The Glass Harbor won't play".into(),
    }
}
#[test]
fn configuration_is_loopback_and_secret_bounded() {
    let path = std::path::Path::new("unused.sqlite");
    assert!(config(path).validate().is_ok());
    for address in ["0.0.0.0:3000", "[::]:3000"] {
        let mut c = config(path);
        c.bind = address.parse().unwrap();
        assert!(c.validate().is_err());
    }
    let mut c = config(path);
    c.principals.clear();
    assert!(c.validate().is_err());
    for token in [
        "x".repeat(31),
        "x".repeat(257),
        format!("{} ", "x".repeat(32)),
        format!("{}\u{0000}", "x".repeat(32)),
    ] {
        let mut c = config(path);
        c.principals[0].bearer_token = token;
        assert!(c.validate().is_err());
    }
    for length in [32, 256] {
        let mut c = config(path);
        c.principals[0].bearer_token = "x".repeat(length);
        assert!(c.validate().is_ok());
    }
    let mut c = config(path);
    c.principals.push(TestPrincipal {
        bearer_token: TOKEN.into(),
        viewer: viewer(),
        expires_at: 1000,
    });
    assert!(c.validate().is_err());
}
#[tokio::test]
async fn authenticated_journal_survives_restart_and_isolates_conversations() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("intake.sqlite");
    let server = TestServer::open(config(&path)).await.unwrap();
    for (token, at) in [("wrong", 100), (TOKEN, 1000), (TOKEN, 1001)] {
        assert_eq!(
            server.message(token, message(), at).await,
            Err(StatusCode::UNAUTHORIZED)
        );
    }
    assert_eq!(
        server.message(&"x".repeat(257), message(), 100).await,
        Err(StatusCode::UNAUTHORIZED)
    );
    let original = server.message(TOKEN, message(), 999).await.unwrap();
    assert_eq!(original.outcome, IntakeOutcome::Report);
    assert_eq!(original.report_ids.len(), 1);
    let mut changed = message();
    changed.text = "anything else".into();
    assert_eq!(server.message(TOKEN, changed, 999).await.unwrap(), original);
    drop(server);
    let server = TestServer::open(config(&path)).await.unwrap();
    assert_eq!(
        server.message(TOKEN, message(), 999).await.unwrap(),
        original
    );
    let mut follow = message();
    follow.message_id = "follow".into();
    follow.text = "is it fixed yet?".into();
    let reply = server.message(TOKEN, follow.clone(), 999).await.unwrap();
    assert_eq!(reply.outcome, IntakeOutcome::FollowUp);
    assert_eq!(reply.report_ids, original.report_ids);
    follow.message_id = "again".into();
    assert_eq!(
        server
            .message(TOKEN, follow.clone(), 999)
            .await
            .unwrap()
            .report_ids,
        original.report_ids
    );
    follow.conversation_id = "different".into();
    assert_eq!(
        server.message(TOKEN, follow, 999).await.unwrap().outcome,
        IntakeOutcome::Clarify
    );
    let mut fresh = message();
    fresh.conversation_id = "different".into();
    assert_ne!(
        server.message(TOKEN, fresh, 999).await.unwrap().report_ids,
        original.report_ids
    );
    let mut bad = message();
    bad.message_id = String::new();
    assert_eq!(
        server.message(TOKEN, bad, 999).await,
        Err(StatusCode::BAD_REQUEST)
    );
    let store = Store::open(&path).await.unwrap();
    assert_eq!(store.reports("viewer-a").await.unwrap().len(), 5);
}
#[tokio::test]
async fn http_has_no_request_side_authority_and_auth_before_dispatch() {
    let tmp = tempfile::tempdir().unwrap();
    let mut c = config(&tmp.path().join("http.sqlite"));
    c.principals[0].expires_at = u64::MAX;
    let app = router(Arc::new(TestServer::open(c).await.unwrap()));
    let bearer = format!("Bearer {TOKEN}");
    for (auth, body, want) in [
        (None, json!(message()), StatusCode::UNAUTHORIZED),
        (
            Some("Bearer wrong"),
            json!(message()),
            StatusCode::UNAUTHORIZED,
        ),
        (Some(TOKEN), json!(message()), StatusCode::UNAUTHORIZED),
        (
            Some(bearer.as_str()),
            json!({"message_id":"one","conversation_id":"c","text":"broken","library_ids":["films"]}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (Some(bearer.as_str()), json!(message()), StatusCode::OK),
    ] {
        let mut req = Request::post("/test/messages").header("content-type", "application/json");
        if let Some(auth) = auth {
            req = req.header("authorization", auth);
        }
        let response = app
            .clone()
            .oneshot(req.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), want);
        if want == StatusCode::OK {
            let bytes = to_bytes(response.into_body(), 10000).await.unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["outcome"],
                "report"
            );
        }
    }
    // A normal message may exceed one KiB while remaining below the intake
    // text bound and the larger JSON envelope limit.
    let mut larger = message();
    larger.message_id = "larger".into();
    larger.text.push_str(&" extra".repeat(400));
    let response = app
        .clone()
        .oneshot(
            Request::post("/test/messages")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::from(serde_json::to_string(&larger).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = app
        .oneshot(
            Request::post("/test/messages")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::from("x".repeat(12 * 1024 + 1)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}
#[tokio::test]
async fn replay_and_followup_require_current_library_permission() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path().join("scope.sqlite")).await.unwrap();
    let mut msg = IncomingMessage {
        sender: ChannelIdentity {
            channel_id: "test".into(),
            platform_user_id: "person".into(),
        },
        message_id: "same".into(),
        conversation_id: "one".into(),
        text: message().text,
        received_at: 10,
    };
    let first = journal_message(&store, &msg, &viewer(), &catalog())
        .await
        .unwrap();
    assert_eq!(first.outcome, IntakeOutcome::Report);
    let denied = AuthorizedViewer {
        library_ids: vec![],
        ..viewer()
    };
    assert_eq!(
        journal_message(&store, &msg, &denied, &catalog())
            .await
            .unwrap()
            .outcome,
        IntakeOutcome::Unauthorized
    );
    msg.message_id = "followup".into();
    msg.text = "is it fixed yet?".into();
    let follow = journal_message(&store, &msg, &denied, &catalog())
        .await
        .unwrap();
    assert!(follow.report_ids.is_empty());
    let base = message_key(&msg);
    for part in 0..4 {
        let mut other = msg.clone();
        match part {
            0 => other.sender.channel_id = "other".into(),
            1 => other.sender.platform_user_id = "other".into(),
            2 => other.conversation_id = "other".into(),
            _ => other.message_id = "other".into(),
        };
        assert_ne!(message_key(&other), base);
    }
    msg.text = "different text same envelope".into();
    assert_eq!(message_key(&msg), base);
    // Corrupt/missing stored context cannot disclose an old selection.
    msg.sender.channel_id = "other".into();
    let key = message_key(&msg);
    let value = json!({"outcome":"report","issues":[],"clarification":null,"report_ids":["secret"],"reply":"private","suspicious_input":false});
    store
        .persist_intake("other", &key, "viewer-a", "one", &value, 10)
        .await
        .unwrap();
    assert_eq!(
        journal_message(&store, &msg, &viewer(), &catalog())
            .await
            .unwrap()
            .outcome,
        IntakeOutcome::Unauthorized
    );
}

#[tokio::test]
async fn authenticated_token_accepts_maximum_length() {
    let tmp = tempfile::tempdir().unwrap();
    let mut c = config(&tmp.path().join("token.sqlite"));
    let token = "x".repeat(256);
    c.principals[0].bearer_token = token.clone();
    let server = TestServer::open(c).await.unwrap();
    assert_eq!(
        server.message(&token, message(), 1).await.unwrap().outcome,
        IntakeOutcome::Report
    );
}

#[test]
fn bot_catalog_identity_must_be_unique_and_nonempty() {
    let path = std::path::Path::new("unused.sqlite");
    for invalid in 0..3 {
        let mut c = config(path);
        match invalid {
            0 => {
                let mut duplicate = c.catalog[0].clone();
                duplicate.library_id = "restricted".into();
                c.catalog.push(duplicate);
            }
            1 => c.catalog[0].id.clear(),
            _ => c.catalog[0].library_id.clear(),
        }
        assert_eq!(
            c.validate(),
            Err("catalog requires unique item IDs and nonempty library IDs")
        );
        let runtime = pitcrew_bot::runtime::RuntimeConfig {
            database: path.into(),
            catalog: c.catalog,
            plex_auth_url: "https://auth.example.invalid".into(),
            plex_server_url: "https://server.example.invalid".into(),
            plex_server_id: "synthetic".into(),
            client_id: "test".into(),
            discord: Some(pitcrew_bot::runtime::DiscordConfig {
                channel_id: "discord".into(),
                bot_token: "synthetic".into(),
            }),
            telegram: None,
            seerr: None,
        };
        assert_eq!(
            pitcrew_bot::runtime::validate_channels(&runtime),
            Err("catalog requires unique item IDs and nonempty library IDs")
        );
    }
}
