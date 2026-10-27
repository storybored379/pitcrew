use pitcrew::App;
use pitcrew_domain::{Access, Constraints, Event, FileIdentity, Policy, StageTimeouts};
use pitcrew_engine::webhook::Secret;
use pitcrew_store::{BudgetLimits, NewJob, Store};
use serde_json::{Value, json};
use std::collections::BTreeMap;

const SECRET: &str = "synthetic-review-secret-0123456789abcdef";
fn limits() -> BudgetLimits {
    BudgetLimits {
        global_hour: 10,
        global_day: 20,
        viewer_hour: 4,
        viewer_day: 8,
    }
}
fn job() -> NewJob {
    NewJob {
        id: 1,
        viewer: "viewer-a".into(),
        resource: "media-a".into(),
        library: 2,
        original: FileIdentity { id: 3, revision: 1 },
        constraints: Constraints::default(),
        policy: Policy {
            max_grabs: 2,
            stage_timeout: 30,
            total_timeout: 300,
        },
        timeouts: StageTimeouts {
            download: 60,
            import: 60,
            decode: 60,
            verify_retry: 30,
        },
        created_at: 0,
    }
}
fn select() -> Event {
    Event::SelectRelease {
        release: 4,
        access: Access::Granted {
            library: 2,
            expires_at: 100,
        },
    }
}
fn replay() -> Value {
    json!({"key":"review-replay","job":job(),"events":[{"at":1,"key":"select","event":select()}]})
}
async fn app(path: &std::path::Path) -> App {
    App::new(
        Store::open(path).await.unwrap(),
        vec![],
        Some(Secret::new(SECRET).unwrap()),
        limits(),
        2,
        BTreeMap::new(),
    )
    .unwrap()
}
async fn submit(app: &App, replay: &Value) -> (u16, Value) {
    app.handle(
        "POST",
        "/v1/shadow/replay",
        SECRET,
        &serde_json::to_vec(replay).unwrap(),
        100,
    )
    .await
}

#[tokio::test]
async fn acknowledged_replay_retry_returns_same_durable_reports() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(&dir.path().join("db")).await;
    let first = submit(&app, &replay()).await;
    assert_eq!(first.0, 200, "{}", first.1);
    let retry = submit(&app, &replay()).await;
    assert_eq!(retry.0, 200, "{}", retry.1);
    assert_eq!(retry.1, first.1);
    assert_eq!(app.reports().await.unwrap().len(), 1);
    assert_eq!(app.store.outbox(1).await.unwrap().len(), 1);
}

#[tokio::test]
async fn crash_after_event_commit_recovers_missing_report_from_durable_intent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    {
        let store = Store::open(&path).await.unwrap();
        store
            .create_job(&job(), "review-replay", limits())
            .await
            .unwrap();
        let lease = store
            .acquire_lease(1, "crashed-worker", 0, 2)
            .await
            .unwrap();
        store
            .apply_event(&lease, 0, "select", select(), 1)
            .await
            .unwrap();
        // Model abrupt death between the committed event and report projection.
        assert!(store.observations().await.unwrap().is_empty());
    }
    let restarted = app(&path).await;
    let response = submit(&restarted, &replay()).await;
    assert_eq!(response.0, 200, "{}", response.1);
    assert_eq!(response.1["reports"].as_array().unwrap().len(), 1);
    assert_eq!(restarted.reports().await.unwrap().len(), 1);
    assert_eq!(restarted.store.outbox(1).await.unwrap().len(), 1);
}

#[tokio::test]
async fn invalid_recorded_clock_does_not_publish_a_job_or_reserve_budget() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(&dir.path().join("db")).await;
    let mut invalid = replay();
    invalid["job"]["created_at"] = json!(20);
    assert_eq!(submit(&app, &invalid).await.0, 409);
    assert!(app.store.jobs().await.unwrap().is_empty());
}

#[tokio::test]
async fn altered_retry_cannot_rewrite_recorded_comparison_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(&dir.path().join("db")).await;
    let first = submit(&app, &replay()).await;
    assert_eq!(first.0, 200);
    let mut altered = replay();
    altered["observed"] = json!({"1":{"action":"grab","actor":"python","correlated":true,"safety":{"access":true,"budget":true,"reconciled":true}}});
    assert_eq!(submit(&app, &altered).await.0, 409);
    assert_eq!(
        serde_json::to_value(app.reports().await.unwrap()).unwrap(),
        first.1["reports"]
    );
    assert_eq!(app.store.load_job(1).await.unwrap().revision, 1);
}

#[tokio::test]
async fn invalid_later_event_leaves_no_partial_job_or_input_envelope() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(&dir.path().join("db")).await;
    let mut invalid = replay();
    invalid["events"]
        .as_array_mut()
        .unwrap()
        .push(json!({"at":2,"key":"second-select","event":select()}));
    assert_eq!(submit(&app, &invalid).await.0, 409);
    assert!(app.store.jobs().await.unwrap().is_empty());
    assert!(app.store.observations().await.unwrap().is_empty());
}

#[tokio::test]
async fn projection_automatically_resumes_a_durable_replay_input() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(&dir.path().join("db")).await;
    app.store
        .create_job(&job(), "review-replay", limits())
        .await
        .unwrap();
    let input: pitcrew::Replay = serde_json::from_value(replay()).unwrap();
    app.store
        .ingest_observation(
            "replay_input",
            "review-replay",
            &serde_json::to_value(input).unwrap(),
            0,
        )
        .await
        .unwrap();
    app.project(0).await.unwrap();
    app.project(0).await.unwrap();
    assert_eq!(app.reports().await.unwrap().len(), 1);
    assert_eq!(
        app.store.load_job(1).await.unwrap().job.state(),
        pitcrew_domain::State::GrabIntent
    );
    assert_eq!(
        app.store.outbox(1).await.unwrap()[0].status,
        pitcrew_store::OutboxStatus::Shadowed
    );
}
