mod common;
use common::*;
use pitcrew_admin::{
    Error,
    actions::{Command, MessageTemplate, Outcome},
    journal::{Journal, token},
};
use serde_json::json;

#[tokio::test]
async fn sessions_are_hashed_single_use_and_expire_at_exact_boundaries() {
    let s = Setup::new(false).await;
    let journal = &s.app.journal;
    let state = journal.begin(42, 100).await.unwrap();
    assert_eq!(state.len(), 64);
    assert!(state.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_ne!(state, token().unwrap());
    assert_eq!(journal.pin(&state, 699).await.unwrap(), 42);
    assert!(matches!(
        journal.pin(&state, 700).await,
        Err(Error::Unauthorized)
    ));
    assert!(matches!(
        journal.complete(&state, "1001", 700).await,
        Err(Error::Unauthorized)
    ));
    let state = journal.begin(43, 100).await.unwrap();
    let session = journal.complete(&state, "1001", 101).await.unwrap();
    assert_ne!(session.token, session.csrf);
    assert_eq!(session.csrf.len(), 64);
    assert_eq!(
        journal
            .session(&session.token, 28_900)
            .await
            .unwrap()
            .account,
        "1001"
    );
    assert_eq!(
        journal.session(&session.token, 101).await.unwrap().csrf,
        session.csrf
    );
    assert!(matches!(
        journal.session(&session.token, 28_901).await,
        Err(Error::Unauthorized)
    ));
    assert!(matches!(
        journal.complete(&state, "1001", 101).await,
        Err(Error::Unauthorized)
    ));
    let saved: String = sqlx::query_scalar("SELECT digest FROM admin_sessions")
        .fetch_one(&s.admin_sql)
        .await
        .unwrap();
    assert_ne!(saved, session.token);
    assert_eq!(saved.len(), 64);
    let reopened = Journal::open(&s.dir.path().join("admin.sqlite"))
        .await
        .unwrap();
    assert_eq!(
        reopened.session(&session.token, 102).await.unwrap().csrf,
        session.csrf
    );
    reopened.logout(&session.token).await.unwrap();
    assert!(journal.session(&session.token, 102).await.is_err());
    for value in [u64::MAX, i64::MAX as u64] {
        assert!(journal.begin(42, value).await.is_err());
        assert!(journal.complete("bad", "1001", value).await.is_err());
    }
    assert!(journal.begin(u64::MAX, 1).await.is_err());
}
#[tokio::test]
async fn login_rate_window_is_shared_persistent_and_boundary_exact() {
    let s = Setup::new(false).await;
    for _ in 0..30 {
        s.app.journal.reserve_login(119).await.unwrap();
    }
    assert!(matches!(
        s.app.journal.reserve_login(119).await,
        Err(Error::Rate)
    ));
    let reopened = Journal::open(&s.dir.path().join("admin.sqlite"))
        .await
        .unwrap();
    assert!(matches!(
        reopened.reserve_login(119).await,
        Err(Error::Rate)
    ));
    reopened.reserve_login(120).await.unwrap();
}
#[tokio::test]
async fn login_rate_limit_accumulates_different_seconds_within_the_same_minute() {
    let s = Setup::new(false).await;
    for _ in 0..15 {
        s.app.journal.reserve_login(60).await.unwrap();
    }
    let reopened = Journal::open(&s.dir.path().join("admin.sqlite"))
        .await
        .unwrap();
    for _ in 0..15 {
        reopened.reserve_login(118).await.unwrap();
    }
    assert!(matches!(
        s.app.journal.reserve_login(119).await,
        Err(Error::Rate)
    ));
    reopened.reserve_login(120).await.unwrap();
}
#[tokio::test]
async fn requests_are_deduplicated_by_author_action_and_note_with_atomic_audit() {
    let s = Setup::new(false).await;
    let j = &s.app.journal;
    let command = Command::Close { report: 7 };
    assert!(
        !j.existing_request("1001", "a", &command, "reviewed")
            .await
            .unwrap()
    );
    assert!(
        j.request("1001", "a", &command, "reviewed", 100)
            .await
            .unwrap()
    );
    assert!(
        !j.request("1001", "a", &command, "reviewed", 101)
            .await
            .unwrap()
    );
    assert!(
        j.existing_request("1001", "a", &command, "reviewed")
            .await
            .unwrap()
    );
    for (actor, cmd, note) in [
        ("1002", command.clone(), "reviewed"),
        ("1001", Command::Close { report: 8 }, "reviewed"),
        ("1001", command.clone(), "changed"),
    ] {
        assert!(matches!(
            j.request(actor, "a", &cmd, note, 102).await,
            Err(Error::Conflict)
        ));
        assert!(matches!(
            j.existing_request(actor, "a", &cmd, note).await,
            Err(Error::Conflict)
        ));
    }
    assert_eq!(
        j.requests().await.unwrap(),
        vec![
            json!({"request_key":"a","actor":"1001","command":{"action":"close","report":7},"note":"reviewed","state":"pending_controller","created_at":100,"updated_at":100})
        ]
    );
    assert_eq!(
        scalar(&s.admin_sql, "SELECT COUNT(*) FROM admin_audit").await,
        1
    );
    assert!(
        sqlx::query("DELETE FROM admin_audit")
            .execute(&s.admin_sql)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE admin_audit SET event='forged'")
            .execute(&s.admin_sql)
            .await
            .is_err()
    );
    assert!(matches!(
        j.finish("a", Outcome::Accepted, 110).await,
        Err(Error::Conflict)
    ));
}
#[tokio::test]
async fn interrupted_sends_recover_as_uncertain_and_never_resubmit() {
    let s = Setup::new(false).await;
    let j = &s.app.journal;
    let command = Command::Reset {
        client: "arr-a".into(),
    };
    j.request("1001", "a", &command, "reviewed", 100)
        .await
        .unwrap();
    assert_eq!(j.requests().await.unwrap()[0]["state"], "dispatching");
    let reopened = Journal::open(&s.dir.path().join("admin.sqlite"))
        .await
        .unwrap();
    reopened.recover(110).await.unwrap();
    assert_eq!(reopened.requests().await.unwrap()[0]["state"], "uncertain");
    assert_eq!(reopened.requests().await.unwrap()[0]["updated_at"], 110);
    assert!(
        !reopened
            .request("1001", "a", &command, "reviewed", 111)
            .await
            .unwrap()
    );
    assert_eq!(
        scalar(&s.admin_sql, "SELECT COUNT(*) FROM admin_audit").await,
        2
    );
    reopened.recover(112).await.unwrap();
    assert_eq!(
        scalar(&s.admin_sql, "SELECT COUNT(*) FROM admin_audit").await,
        2
    );
    assert!(matches!(
        reopened.finish("a", Outcome::Accepted, 120).await,
        Err(Error::Conflict)
    ));
}
#[tokio::test]
async fn finish_commits_each_outcome_once_and_recovery_leaves_completed_requests_alone() {
    let s = Setup::new(false).await;
    let j = &s.app.journal;
    let command = Command::Release { id: 1 };
    for (index, outcome) in [Outcome::Accepted, Outcome::Rejected, Outcome::Uncertain]
        .into_iter()
        .enumerate()
    {
        let key = format!("request-{index}");
        j.request("1001", &key, &command, "reason", 100)
            .await
            .unwrap();
        j.finish(&key, outcome, 101).await.unwrap();
        assert!(matches!(
            j.finish(&key, outcome, 102).await,
            Err(Error::Conflict)
        ));
    }
    let before = j.requests().await.unwrap();
    j.recover(105).await.unwrap();
    assert_eq!(before, j.requests().await.unwrap());
    assert_eq!(
        before
            .iter()
            .map(|v| v["state"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["accepted", "rejected", "uncertain"]
    );
    assert_eq!(
        scalar(&s.admin_sql, "SELECT COUNT(*) FROM admin_audit").await,
        6
    );
    assert!(matches!(
        j.finish("missing", Outcome::Accepted, 1).await,
        Err(Error::Conflict)
    ));
}
#[tokio::test]
async fn audit_projection_retains_exact_transitions_actors_and_clocks_in_reverse_order() {
    let s = Setup::new(false).await;
    let j = &s.app.journal;
    j.request(
        "1001",
        "release",
        &Command::Release { id: 7 },
        "Reviewed",
        100,
    )
    .await
    .unwrap();
    j.finish("release", Outcome::Accepted, 101).await.unwrap();
    j.request(
        "1002",
        "close",
        &Command::Close { report: 8 },
        "Resolved",
        102,
    )
    .await
    .unwrap();
    assert_eq!(
        j.audit().await.unwrap(),
        vec![
            json!({"id":3,"request_key":"close","actor":"1002","audit_event":"pending_controller","at":102}),
            json!({"id":2,"request_key":"release","actor":"1001","audit_event":"accepted","at":101}),
            json!({"id":1,"request_key":"release","actor":"1001","audit_event":"dispatching","at":100}),
        ]
    );
}
#[tokio::test]
async fn request_validation_does_not_write_partial_records() {
    let s = Setup::new(false).await;
    let command = Command::Close { report: 1 };
    let long_key = "x".repeat(129);
    let long_note = "x".repeat(1001);
    for (actor, key, note) in [
        ("", "a", "note"),
        ("1001", "", "note"),
        ("1001", long_key.as_str(), "note"),
        ("1001", "a", ""),
        ("1001", "a", " \n"),
        ("1001", "a", long_note.as_str()),
    ] {
        assert!(matches!(
            s.app.journal.request(actor, key, &command, note, 1).await,
            Err(Error::Input)
        ));
    }
    assert_eq!(
        scalar(&s.admin_sql, "SELECT COUNT(*) FROM admin_requests").await,
        0
    );
    assert_eq!(
        scalar(&s.admin_sql, "SELECT COUNT(*) FROM admin_audit").await,
        0
    );
    assert!(
        s.app
            .journal
            .request("1001", "a", &command, "note", u64::MAX)
            .await
            .is_err()
    );
    assert_eq!(
        scalar(&s.admin_sql, "SELECT COUNT(*) FROM admin_requests").await,
        0
    );
    s.app
        .journal
        .request("1001", &"k".repeat(128), &command, &"n".repeat(1000), 1)
        .await
        .unwrap();
}
#[test]
fn fixed_templates_and_api_payloads_never_accept_ready_or_arbitrary_recipients() {
    assert_eq!(
        MessageTemplate::Received.text(),
        "We received your report and will review it."
    );
    assert_eq!(
        MessageTemplate::NeedDetails.text(),
        "Please reply with the episode or movie, your device, and what happened when you tried to play it."
    );
    assert_eq!(
        MessageTemplate::UnderReview.text(),
        "Your report is under review. We will update you when we have a verified result."
    );
    assert_eq!(
        Command::Release { id: 7 }.api("key", "note"),
        Some((
            "/v1/admin/exclusions/release",
            json!({"id":7,"note":"note"})
        ))
    );
    assert_eq!(
        Command::Reset {
            client: "arr-a".into()
        }
        .api("key", "note"),
        Some((
            "/v1/admin/capabilities/reset",
            json!({"client":"arr-a","note":"note"})
        ))
    );
    assert_eq!(
        Command::Resend {
            job: 7,
            sequence: 8
        }
        .api("key", "note"),
        Some((
            "/v1/admin/notifications/resend",
            json!({"job":7,"sequence":8,"request_key":"key","note":"note"})
        ))
    );
    for cmd in [
        Command::Approve {
            report: 1,
            decided_at: 2,
        },
        Command::Deny {
            report: 1,
            decided_at: 2,
        },
        Command::LowerQuality {
            job: 1,
            revision: 2,
        },
        Command::Close { report: 1 },
        Command::Pin { id: 1 },
        Command::Message {
            report: 1,
            template: MessageTemplate::Received,
        },
    ] {
        assert_eq!(cmd.api("key", "note"), None);
    }
}
