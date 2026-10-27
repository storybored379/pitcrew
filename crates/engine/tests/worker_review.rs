use pitcrew_domain::{
    Access, Constraints, Event, FileIdentity, Observation, Policy, StageTimeouts,
};
use pitcrew_engine::{Boundary, CallFuture, EngineError, Mode, Options, Outcome, Upstream, Worker};
use pitcrew_store::{BudgetLimits, NewJob, OutboxStatus, Store};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

#[derive(Clone, Default)]
struct DispatchCounter(Arc<AtomicUsize>);
impl Upstream for DispatchCounter {
    fn read<'a>(&'a self, _: &'a pitcrew_domain::Intent) -> CallFuture<'a> {
        panic!("unexpected read")
    }
    fn write<'a>(&'a self, _: &'a pitcrew_domain::Intent) -> CallFuture<'a> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(Outcome::Observed(Observation::GrabAccepted { download: 9 })) })
    }
}
async fn setup(store: &Store) -> pitcrew_store::Lease {
    store
        .create_job(
            &NewJob {
                id: 1,
                viewer: "viewer".into(),
                resource: "item".into(),
                library: 3,
                original: FileIdentity { id: 2, revision: 1 },
                constraints: Constraints::default(),
                policy: Policy {
                    max_grabs: 1,
                    stage_timeout: 1_000,
                    total_timeout: 2_000,
                },
                timeouts: StageTimeouts {
                    download: 100,
                    import: 100,
                    decode: 100,
                    verify_retry: 100,
                },
                created_at: 0,
            },
            "new",
            BudgetLimits {
                global_hour: 10,
                global_day: 20,
                viewer_hour: 2,
                viewer_day: 4,
            },
        )
        .await
        .unwrap();
    let lease = store.acquire_lease(1, "original", 0, 100).await.unwrap();
    store
        .apply_event(
            &lease,
            0,
            "select",
            Event::SelectRelease {
                release: 8,
                access: Access::Granted {
                    library: 3,
                    expires_at: 1_000,
                },
            },
            0,
        )
        .await
        .unwrap();
    lease
}

#[tokio::test]
async fn expired_lease_after_durable_start_never_reaches_upstream() {
    for point in [Boundary::StartAfter, Boundary::CallBefore] {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("db")).await.unwrap();
        let lease = setup(&store).await;
        let counter = DispatchCounter::default();
        let worker = Worker::new(
            Some(store.clone()),
            counter.clone(),
            Options {
                mode: Mode::Live,
                acknowledge_live_mutations: true,
                call_timeout: Duration::from_secs(1),
            },
        )
        .unwrap();
        let result = worker
            .drive_with(&lease, 0, |boundary| async move {
                if boundary == point {
                    tokio::time::pause();
                    tokio::time::advance(Duration::from_secs(100)).await;
                    tokio::time::resume();
                }
            })
            .await;
        assert_eq!(
            counter.0.load(Ordering::SeqCst),
            0,
            "dispatch after {point:?}"
        );
        assert!(matches!(result, Err(EngineError::LeaseDeadline)));
        assert_eq!(
            store.outbox(1).await.unwrap()[0].status,
            OutboxStatus::Started
        );
    }
}

#[derive(Clone)]
struct ReplyCounter {
    calls: Arc<AtomicUsize>,
    reply: Outcome,
}
impl Upstream for ReplyCounter {
    fn read<'a>(&'a self, _: &'a pitcrew_domain::Intent) -> CallFuture<'a> {
        panic!("unexpected read")
    }
    fn write<'a>(&'a self, _: &'a pitcrew_domain::Intent) -> CallFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let reply = self.reply.clone();
        Box::pin(async move { Ok(reply) })
    }
}
fn live() -> Options {
    Options {
        mode: Mode::Live,
        acknowledge_live_mutations: true,
        call_timeout: Duration::from_secs(1),
    }
}

#[tokio::test]
async fn switching_to_shadow_preserves_started_or_uncertain_live_intents() {
    for status in [OutboxStatus::Started, OutboxStatus::Uncertain] {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("db")).await.unwrap();
        let lease = setup(&store).await;
        let current = store.load_job(1).await.unwrap();
        let key = pitcrew_domain::IntentKey {
            job: 1,
            sequence: 1,
        };
        assert!(
            store
                .start_intent(&lease, key, current.revision, 1)
                .await
                .unwrap()
        );
        if status == OutboxStatus::Uncertain {
            store.finish_intent(&lease, key, status, 1).await.unwrap();
        }
        let counter = DispatchCounter::default();
        let worker = Worker::new(Some(store.clone()), counter.clone(), Options::default()).unwrap();
        assert_eq!(worker.drive(&lease, 2).await.unwrap(), 0);
        assert_eq!(counter.0.load(Ordering::SeqCst), 0);
        assert_eq!(store.outbox(1).await.unwrap()[0].status, status);
        assert!(store.observations().await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn notification_completion_requires_typed_delivery_and_uncertain_is_not_resent() {
    for (reply, expected) in [
        (Outcome::Delivered, OutboxStatus::Completed),
        (Outcome::Uncertain, OutboxStatus::Uncertain),
        (
            Outcome::Observed(Observation::GrabAccepted { download: 9 }),
            OutboxStatus::Uncertain,
        ),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("db")).await.unwrap();
        let lease = setup(&store).await;
        let current = store.load_job(1).await.unwrap();
        store
            .apply_event(
                &lease,
                current.revision,
                "fail",
                Event::Fail(pitcrew_domain::Failure::Cancelled),
                1,
            )
            .await
            .unwrap();
        let counter = ReplyCounter {
            calls: Arc::new(AtomicUsize::new(0)),
            reply,
        };
        let worker = Worker::new(Some(store.clone()), counter.clone(), live()).unwrap();
        assert_eq!(worker.drive(&lease, 2).await.unwrap(), 2);
        assert_eq!(counter.calls.load(Ordering::SeqCst), 2);
        assert_eq!(worker.drive(&lease, 3).await.unwrap(), 0);
        assert_eq!(counter.calls.load(Ordering::SeqCst), 2);
        let messages: Vec<_> = store
            .outbox(1)
            .await
            .unwrap()
            .into_iter()
            .filter(|r| pitcrew_engine::notification(&r.intent.effect))
            .collect();
        assert_eq!(messages.len(), 2);
        assert!(messages.iter().all(|r| r.status == expected));
    }
}

#[tokio::test]
async fn recovered_notification_is_uncertain_without_resending_and_generic_success_cannot_accept_grab()
 {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).await.unwrap();
    let lease = setup(&store).await;
    let current = store.load_job(1).await.unwrap();
    store
        .apply_event(
            &lease,
            current.revision,
            "fail",
            Event::Fail(pitcrew_domain::Failure::Cancelled),
            1,
        )
        .await
        .unwrap();
    let revision = store.load_job(1).await.unwrap().revision;
    for sequence in [2, 3] {
        assert!(
            store
                .start_intent(
                    &lease,
                    pitcrew_domain::IntentKey { job: 1, sequence },
                    revision,
                    1
                )
                .await
                .unwrap()
        );
    }
    let counter = DispatchCounter::default();
    let worker = Worker::new(Some(store.clone()), counter.clone(), live()).unwrap();
    assert_eq!(worker.drive(&lease, 2).await.unwrap(), 0);
    assert_eq!(counter.0.load(Ordering::SeqCst), 0);
    assert!(
        store
            .outbox(1)
            .await
            .unwrap()
            .iter()
            .skip(1)
            .all(|r| r.status == OutboxStatus::Uncertain)
    );

    let store = Store::open(temp.path().join("other-db")).await.unwrap();
    let lease = setup(&store).await;
    let worker = Worker::new(
        Some(store.clone()),
        ReplyCounter {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: Outcome::Delivered,
        },
        live(),
    )
    .unwrap();
    assert_eq!(worker.drive(&lease, 1).await.unwrap(), 1);
    assert_eq!(
        store.load_job(1).await.unwrap().job.state(),
        pitcrew_domain::State::GrabIntent
    );
    assert_eq!(
        store.outbox(1).await.unwrap()[0].status,
        OutboxStatus::Uncertain
    );
}
