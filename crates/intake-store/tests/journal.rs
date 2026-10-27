use pitcrew_intake_store::{Error, LINK_TTL, LOGIN_TTL, Store};
use serde_json::json;
async fn setup() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("intake.sqlite")).await.unwrap();
    (dir, store)
}
#[tokio::test]
async fn browser_proofs_are_bound_single_use_expire_and_survive_restart() {
    let (dir, store) = setup().await;
    let a = store.begin_login(91, 100).await.unwrap();
    let b = store.begin_login(92, 100).await.unwrap();
    assert_ne!(a, b);
    assert_eq!(a.len(), 64);
    assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(store.login_pin(&a, 100).await.unwrap(), Some(91));
    assert_eq!(
        store.login_pin(&a, 100 + LOGIN_TTL - 1).await.unwrap(),
        Some(91)
    );
    assert_eq!(store.login_pin(&b, 100 + LOGIN_TTL).await.unwrap(), None);
    assert_eq!(store.login_pin("wrong", 100).await.unwrap(), None);
    assert!(matches!(
        store
            .complete_login(&b, "viewer-b", "private-token-b", 100 + LOGIN_TTL)
            .await,
        Err(Error::Proof)
    ));
    let session = store
        .complete_login(&a, "viewer-a", "private-token-a", 101)
        .await
        .unwrap();
    assert_eq!(session.token.len(), 64);
    assert_eq!(session.session.csrf.len(), 64);
    assert_ne!(session.token, session.session.csrf);
    assert!(matches!(
        store
            .complete_login(&a, "viewer-b", "wrong-token", 102)
            .await,
        Err(Error::Proof)
    ));
    assert_eq!(store.login_pin(&a, 102).await.unwrap(), None);
    assert_eq!(
        store.session(&session.token, 604900).await.unwrap(),
        Some(session.session.clone())
    );
    assert_eq!(store.session(&session.token, 604901).await.unwrap(), None);
    assert_eq!(store.session("wrong", 102).await.unwrap(), None);
    assert_eq!(
        store.account_token("viewer-a").await.unwrap().as_deref(),
        Some("private-token-a")
    );
    assert_eq!(store.account_token("viewer-b").await.unwrap(), None);
    let reopened = Store::open(dir.path().join("intake.sqlite")).await.unwrap();
    assert_eq!(
        reopened.session(&session.token, 102).await.unwrap(),
        Some(session.session)
    );
    reopened.logout("wrong").await.unwrap();
    assert!(
        reopened
            .session(&session.token, 102)
            .await
            .unwrap()
            .is_some()
    );
    reopened.logout(&session.token).await.unwrap();
    assert_eq!(store.session(&session.token, 102).await.unwrap(), None);
    let state = store.begin_login(93, 200).await.unwrap();
    store
        .complete_login(&state, "viewer-a", "rotated-token", 201)
        .await
        .unwrap();
    assert_eq!(
        store.account_token("viewer-a").await.unwrap().as_deref(),
        Some("rotated-token")
    );
    for n in [u64::MAX, i64::MAX as u64] {
        assert!(matches!(store.begin_login(n, n).await, Err(Error::Invalid)));
        assert!(matches!(
            store.issue_link("a", n).await,
            Err(Error::Invalid)
        ));
    }
    assert!(matches!(
        store.login_pin(&a, u64::MAX).await,
        Err(Error::Invalid)
    ));
    assert!(matches!(
        store.session(&a, u64::MAX).await,
        Err(Error::Invalid)
    ));
}
#[tokio::test]
async fn link_codes_are_atomic_scoped_replaced_and_expiring() {
    let (_dir, store) = setup().await;
    let old = store.issue_link("account-a", 0).await.unwrap();
    let code = store.issue_link("account-a", 0).await.unwrap();
    assert_eq!(code.len(), 12);
    assert_ne!(old, code);
    assert!(matches!(
        store.redeem_link(&old, "discord", "user", 0).await,
        Err(Error::Proof)
    ));
    let upper = format!(" {} ", code.to_uppercase());
    let (first, second) = tokio::join!(
        store.redeem_link(&upper, "discord", "user", 1),
        store.redeem_link(&code, "telegram", "user", 1)
    );
    assert_ne!(first.is_ok(), second.is_ok());
    let linked = if first.is_ok() { "discord" } else { "telegram" };
    let absent = if first.is_ok() { "telegram" } else { "discord" };
    assert_eq!(
        store
            .linked_account(linked, "user")
            .await
            .unwrap()
            .as_deref(),
        Some("account-a")
    );
    assert_eq!(store.linked_account(absent, "user").await.unwrap(), None);
    assert_eq!(store.linked_account(linked, "other").await.unwrap(), None);
    assert!(matches!(
        store.redeem_link(&code, "new", "user", 1).await,
        Err(Error::Proof)
    ));
    let expired = store.issue_link("account-b", 2).await.unwrap();
    assert!(matches!(
        store
            .redeem_link(&expired, "new", "user", 2 + LINK_TTL)
            .await,
        Err(Error::Proof)
    ));
    let fresh = store.issue_link("account-b", 2).await.unwrap();
    assert!(matches!(
        store
            .redeem_link(&fresh, linked, "user", 2 + LINK_TTL - 1)
            .await,
        Err(Error::Proof)
    ));
    assert_eq!(
        store
            .linked_account(linked, "user")
            .await
            .unwrap()
            .as_deref(),
        Some("account-a")
    );
    assert_eq!(
        store
            .redeem_link(&fresh, linked, "other-user", 2 + LINK_TTL - 1)
            .await
            .unwrap(),
        "account-b"
    );
    let same = store.issue_link("account-a", 2).await.unwrap();
    assert_eq!(
        store
            .redeem_link(&same, linked, "user", 2 + LINK_TTL - 1)
            .await
            .unwrap(),
        "account-a"
    );
}
#[tokio::test]
async fn failed_attempts_consume_exact_limits_across_restarts_and_reset() {
    let (dir, store) = setup().await;
    for _ in 0..5 {
        assert!(matches!(
            store.redeem_link("bad", "discord", "user", 0).await,
            Err(Error::Proof)
        ));
    }
    let reopened = Store::open(dir.path().join("intake.sqlite")).await.unwrap();
    let code = store.issue_link("a", 0).await.unwrap();
    assert!(matches!(
        reopened.redeem_link(&code, "discord", "user", 0).await,
        Err(Error::RateLimited)
    ));
    assert!(matches!(
        reopened.redeem_link(&code, "discord", "user", 1).await,
        Err(Error::RateLimited)
    ));
    assert_eq!(
        reopened
            .redeem_link(&code, "discord", "different", 0)
            .await
            .unwrap(),
        "a"
    );
    for i in 0..94 {
        assert!(matches!(
            store
                .redeem_link("bad", "telegram", &i.to_string(), 0)
                .await,
            Err(Error::Proof)
        ));
    }
    assert!(matches!(
        store.redeem_link("bad", "telegram", "last", 0).await,
        Err(Error::RateLimited)
    ));
    let code = store.issue_link("a", LINK_TTL).await.unwrap();
    assert_eq!(
        store
            .redeem_link(&code, "discord", "user", LINK_TTL)
            .await
            .unwrap(),
        "a"
    );
}
#[tokio::test]
async fn intake_is_durable_idempotent_and_account_conversation_scoped() {
    let (dir, store) = setup().await;
    let response = json!({"outcome":"report","reply":"Received","item":"invented"});
    assert!(
        store
            .persist_intake("test", "msg-1", "viewer", "room-1", &response, 8)
            .await
            .unwrap()
    );
    assert!(
        !store
            .persist_intake("test", "msg-1", "viewer", "room-1", &json!({}), 9)
            .await
            .unwrap()
    );
    assert!(
        store
            .persist_intake(
                "test",
                "msg-2",
                "viewer",
                "room-2",
                &json!({"reply":"second"}),
                9
            )
            .await
            .unwrap()
    );
    assert!(
        store
            .persist_intake("test2", "msg-1", "other", "room-1", &json!({}), 10)
            .await
            .unwrap()
    );
    assert!(matches!(
        store
            .persist_intake("test", "invalid", "viewer", "room-1", &json!({}), u64::MAX)
            .await,
        Err(Error::Invalid)
    ));
    let reopened = Store::open(dir.path().join("intake.sqlite")).await.unwrap();
    let reports = reopened.reports("viewer").await.unwrap();
    assert_eq!(reports.len(), 2);
    assert_eq!(reports[0].message_id, "msg-2");
    assert_eq!(reports[1].response, response);
    assert_eq!(reports[1].created_at, 8);
    assert_eq!(reports[1].channel_id, "test");
    assert_eq!(reports[1].conversation_id, "room-1");
    assert_eq!(
        reopened
            .stored_intake("test", "msg-1", "viewer")
            .await
            .unwrap(),
        Some(reports[1].clone())
    );
    assert_eq!(
        reopened
            .stored_intake("test", "msg-1", "other")
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        reopened
            .latest_intake("test", "room-1", "viewer")
            .await
            .unwrap(),
        Some(reports[1].clone())
    );
    assert_eq!(
        reopened
            .latest_intake("test2", "room-1", "viewer")
            .await
            .unwrap(),
        None
    );
    assert!(reopened.reports("missing").await.unwrap().is_empty());
}
#[tokio::test]
async fn delivery_reserves_before_send_and_never_retries_uncertainty() {
    let (dir, store) = setup().await;
    assert_eq!(store.delivery_state("test", "m", "a").await.unwrap(), None);
    let (a, b) = tokio::join!(
        store.claim_delivery("test", "m", "a"),
        store.claim_delivery("test", "m", "a")
    );
    assert_ne!(a.unwrap(), b.unwrap());
    assert_eq!(
        store
            .delivery_state("test", "m", "a")
            .await
            .unwrap()
            .as_deref(),
        Some("uncertain")
    );
    assert_eq!(store.delivery_state("test", "m", "b").await.unwrap(), None);
    let reopened = Store::open(dir.path().join("intake.sqlite")).await.unwrap();
    assert!(!reopened.claim_delivery("test", "m", "a").await.unwrap());
    for state in [
        "confirmed",
        "not_accepted",
        "uncertain",
        "permanent_failure",
    ] {
        store
            .observe_delivery("test", "m", "a", state)
            .await
            .unwrap();
        assert_eq!(
            store
                .delivery_state("test", "m", "a")
                .await
                .unwrap()
                .as_deref(),
            Some(state)
        );
        assert!(!store.claim_delivery("test", "m", "a").await.unwrap());
    }
    assert!(matches!(
        store.observe_delivery("test", "m", "a", "invented").await,
        Err(Error::Invalid)
    ));
    assert!(store.claim_delivery("test", "m", "b").await.unwrap());
}
#[tokio::test]
async fn rate_reservation_has_exact_limits_and_actor_action_windows() {
    let (_dir, store) = setup().await;
    assert!(!store.reserve_rate("x", "a", 0, 60, 0).await.unwrap());
    for _ in 0..3 {
        assert!(store.reserve_rate("y", "a", 3, 60, 59).await.unwrap());
    }
    assert!(!store.reserve_rate("y", "a", 3, 60, 59).await.unwrap());
    assert!(!store.reserve_rate("y", "a", 3, 60, 58).await.unwrap());
    assert!(store.reserve_rate("y", "a", 3, 60, 60).await.unwrap());
    assert!(store.reserve_rate("y", "b", 3, 60, 59).await.unwrap());
    assert!(matches!(
        store.reserve_rate("y", "a", 3, 0, 0).await,
        Err(Error::Invalid)
    ));
    assert!(matches!(
        store.reserve_rate("y", "a", u64::MAX, 60, 0).await,
        Err(Error::Invalid)
    ));
}
#[cfg(unix)]
#[tokio::test]
async fn credentials_database_is_private_and_insecure_existing_file_rejected() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, _store) = setup().await;
    let path = dir.path().join("intake.sqlite");
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(Store::open(&path).await, Err(Error::Invalid)));
    assert!(matches!(Store::open(dir.path()).await, Err(Error::Invalid)));
}
#[tokio::test]
async fn keyed_proofs_survive_restart_lookup_does_not_consume_and_missing_key_fails_safe() {
    let (dir, store) = setup().await;
    let code = store.issue_link("a", 0).await.unwrap();
    assert_eq!(
        store
            .link_account(&format!(" {} ", code.to_uppercase()), 0)
            .await
            .unwrap()
            .as_deref(),
        Some("a")
    );
    assert_eq!(store.link_account("missing", 0).await.unwrap(), None);
    assert_eq!(store.link_account(&code, LINK_TTL).await.unwrap(), None);
    let reopened = Store::open(dir.path().join("intake.sqlite")).await.unwrap();
    assert_eq!(
        reopened.link_account(&code, 0).await.unwrap().as_deref(),
        Some("a")
    );
    assert_eq!(
        reopened
            .redeem_link(&code, "test", "viewer", 0)
            .await
            .unwrap(),
        "a"
    );
    assert_eq!(reopened.link_account(&code, 0).await.unwrap(), None);
    for _ in 0..100 {
        assert_eq!(store.link_account("missing", LINK_TTL).await.unwrap(), None);
    }
    assert!(matches!(
        store.link_account("missing", LINK_TTL).await,
        Err(Error::RateLimited)
    ));
    let key = dir.path().join("intake.sqlite.link-key");
    assert_eq!(std::fs::read(&key).unwrap().len(), 32);
    std::fs::remove_file(&key).unwrap();
    assert!(matches!(
        Store::open(dir.path().join("intake.sqlite")).await,
        Err(Error::Invalid)
    ));
    assert!(!key.exists());
}
#[cfg(unix)]
#[tokio::test]
async fn companion_key_rejects_insecure_permissions_or_corruption() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, _store) = setup().await;
    let path = dir.path().join("intake.sqlite");
    let key = dir.path().join("intake.sqlite.link-key");
    assert_eq!(
        std::fs::metadata(&key).unwrap().permissions().mode() & 0o777,
        0o600
    );
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(Store::open(&path).await, Err(Error::Invalid)));
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    for size in [0, 31, 33, 4096] {
        std::fs::write(&key, vec![1; size]).unwrap();
        assert!(matches!(Store::open(&path).await, Err(Error::Invalid)));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn an_empty_private_database_can_be_initialized() {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty.sqlite");
    std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    let store = Store::open(&path).await.unwrap();
    let code = store.issue_link("viewer", 0).await.unwrap();
    assert_eq!(
        store.link_account(&code, 0).await.unwrap().as_deref(),
        Some("viewer")
    );
}

#[tokio::test]
async fn browser_quota_and_report_are_atomic_and_retries_are_free() {
    let (dir, store) = setup().await;
    let response = json!({"issues": [{"item_id":"episode"}]});
    let (first, retry) = tokio::join!(
        store.persist_web_report("a", "nonce", &response, 100),
        store.persist_web_report("a", "nonce", &response, 100)
    );
    assert_ne!(first.unwrap(), retry.unwrap());
    let reopened = Store::open(dir.path().join("intake.sqlite")).await.unwrap();
    assert!(
        !reopened
            .persist_web_report("a", "nonce", &json!({}), 101)
            .await
            .unwrap()
    );
    assert_eq!(store.reports("a").await.unwrap()[0].response, response);
    for i in 0..4 {
        assert!(
            store
                .persist_web_report("a", &i.to_string(), &response, 102 + i)
                .await
                .unwrap()
        );
    }
    for now in [106, 86399] {
        assert!(matches!(
            store.persist_web_report("a", "sixth", &response, now).await,
            Err(Error::RateLimited)
        ));
    }
    assert_eq!(store.reports("a").await.unwrap().len(), 5);
    assert!(
        !store
            .persist_web_report("a", "nonce", &response, 86399)
            .await
            .unwrap()
    );
    assert!(
        store
            .persist_web_report("b", "nonce", &response, 106)
            .await
            .unwrap()
    );
    assert!(
        store
            .persist_web_report("a", "sixth", &response, 86400)
            .await
            .unwrap()
    );
    let rows = store.reports("a").await.unwrap();
    assert_eq!(rows.len(), 6);
    assert_eq!(rows[0].created_at, 86400);
    assert_eq!(rows[0].conversation_id, "web:a");
    assert_eq!(rows[0].channel_id, "web");
    assert!(matches!(
        store
            .persist_web_report("a", "overflow", &response, u64::MAX)
            .await,
        Err(Error::Invalid)
    ));
}
