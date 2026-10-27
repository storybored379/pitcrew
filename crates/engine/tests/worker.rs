use pitcrew_domain::*;
use pitcrew_engine::*;
use pitcrew_store::*;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Default)]
struct Fake {
    writes: Arc<Mutex<Vec<Effect>>>,
    reads: Arc<Mutex<Vec<Effect>>>,
    result: Arc<Mutex<Option<std::result::Result<Outcome, CallError>>>>,
    forbid: bool,
    hang: bool,
}
impl Upstream for Fake {
    fn read<'a>(&'a self, intent: &'a Intent) -> CallFuture<'a> {
        assert!(!self.forbid, "shadow called read effect");
        self.reads.lock().unwrap().push(intent.effect.clone());
        Box::pin(async move {
            if let Some(result) = self.result.lock().unwrap().clone() {
                return result;
            }
            Ok(Outcome::Observed(Observation::GrabReconciled(
                GrabResolution::Accepted { download: 51 },
            )))
        })
    }
    fn write<'a>(&'a self, intent: &'a Intent) -> CallFuture<'a> {
        assert!(!self.forbid, "shadow called write effect");
        self.writes.lock().unwrap().push(intent.effect.clone());
        Box::pin(async move {
            if self.hang {
                std::future::pending::<()>().await;
            }
            self.result
                .lock()
                .unwrap()
                .clone()
                .unwrap_or(Ok(Outcome::Observed(Observation::GrabAccepted {
                    download: 51,
                })))
        })
    }
}
fn spec() -> NewJob {
    NewJob {
        id: 1,
        viewer: "viewer-opaque".into(),
        resource: "media-opaque".into(),
        library: 3,
        original: FileIdentity {
            id: 10,
            revision: 1,
        },
        constraints: Constraints::default(),
        policy: Policy {
            max_grabs: 2,
            stage_timeout: 100,
            total_timeout: 1000,
        },
        timeouts: StageTimeouts {
            download: 500,
            import: 200,
            decode: 100,
            verify_retry: 100,
        },
        created_at: 0,
    }
}
fn limits() -> BudgetLimits {
    BudgetLimits {
        global_hour: 10,
        global_day: 20,
        viewer_hour: 4,
        viewer_day: 8,
    }
}
fn access() -> Access {
    Access::Granted {
        library: 3,
        expires_at: 900,
    }
}
fn live() -> Options {
    Options {
        mode: Mode::Live,
        acknowledge_live_mutations: true,
        call_timeout: Duration::from_secs(1),
    }
}
async fn setup(store: &Store) -> Lease {
    store.create_job(&spec(), "create", limits()).await.unwrap();
    let lease = store.acquire_lease(1, "worker", 0, 500).await.unwrap();
    store
        .apply_event(
            &lease,
            0,
            "select",
            Event::SelectRelease {
                release: 8,
                access: access(),
            },
            0,
        )
        .await
        .unwrap();
    lease
}

#[tokio::test]
async fn explicit_live_and_store_required() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).await.unwrap();
    assert!(Worker::new(None, Fake::default(), Options::default()).is_err());
    assert!(
        Worker::new(
            Some(store.clone()),
            Fake::default(),
            Options {
                mode: Mode::Live,
                ..Options::default()
            }
        )
        .is_err()
    );
    assert!(
        Worker::new(
            Some(store.clone()),
            Fake::default(),
            Options {
                call_timeout: Duration::ZERO,
                ..Options::default()
            }
        )
        .is_err()
    );
    Worker::new(Some(store), Fake::default(), live()).unwrap();
    assert_eq!(Mode::default(), Mode::Shadow);
    assert_eq!(Options::default().call_timeout, Duration::from_secs(30));
}

#[tokio::test]
async fn shadow_records_once_without_any_calls_and_restarts() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let store = Store::open(&path).await.unwrap();
    let lease = setup(&store).await;
    let worker = Worker::new(
        Some(store.clone()),
        Fake {
            forbid: true,
            ..Fake::default()
        },
        Options::default(),
    )
    .unwrap();
    assert_eq!(worker.drive(&lease, 1).await.unwrap(), 1);
    assert_eq!(worker.drive(&lease, 1).await.unwrap(), 0);
    assert_eq!(
        store.outbox(1).await.unwrap()[0].status,
        OutboxStatus::Shadowed
    );
    let records = store.observations().await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].source, "engine_decision");
    assert_eq!(records[0].payload["intent"]["effect"]["Grab"]["release"], 8);
    let reopened = Store::open(&path).await.unwrap();
    assert_eq!(
        reopened.load_job(1).await.unwrap().job.state(),
        State::GrabIntent
    );
}

#[tokio::test]
async fn successful_effect_commits_result_atomically() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).await.unwrap();
    let lease = setup(&store).await;
    let fake = Fake::default();
    let worker = Worker::new(Some(store.clone()), fake.clone(), live()).unwrap();
    assert_eq!(worker.drive(&lease, 1).await.unwrap(), 1);
    assert_eq!(
        store.load_job(1).await.unwrap().job.state(),
        State::Downloading
    );
    assert_eq!(
        store.outbox(1).await.unwrap()[0].status,
        OutboxStatus::Completed
    );
    assert_eq!(worker.drive(&lease, 2).await.unwrap(), 0);
    assert_eq!(fake.writes.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn timeout_5xx_and_lost_response_reconcile_without_second_grab() {
    for fault in [
        CallError::Timeout,
        CallError::ServerError,
        CallError::Unavailable,
    ] {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("db")).await.unwrap();
        let lease = setup(&store).await;
        let fake = Fake::default();
        *fake.result.lock().unwrap() = Some(Err(fault));
        let worker = Worker::new(Some(store.clone()), fake.clone(), live()).unwrap();
        worker.drive(&lease, 1).await.unwrap();
        assert_eq!(
            store.load_job(1).await.unwrap().job.state(),
            State::GrabReconciling
        );
        *fake.result.lock().unwrap() = None;
        worker.drive(&lease, 2).await.unwrap();
        assert_eq!(
            store.load_job(1).await.unwrap().job.state(),
            State::Downloading
        );
        assert_eq!(fake.writes.lock().unwrap().len(), 1);
        assert_eq!(fake.reads.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn hung_call_has_hard_deadline_and_unknown_outcome() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).await.unwrap();
    let lease = setup(&store).await;
    let worker = Worker::new(
        Some(store.clone()),
        Fake {
            hang: true,
            ..Fake::default()
        },
        live(),
    )
    .unwrap();
    worker.drive(&lease, 1).await.unwrap();
    assert_eq!(
        store.load_job(1).await.unwrap().job.state(),
        State::GrabReconciling
    );
}

#[tokio::test]
async fn lease_expiry_and_deadline_cannot_dispatch_mutation() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).await.unwrap();
    let lease = setup(&store).await;
    let fake = Fake::default();
    let worker = Worker::new(Some(store.clone()), fake.clone(), live()).unwrap();
    let mut short = lease.clone();
    short.expires_at = 3;
    assert!(matches!(
        worker.drive(&short, 1).await,
        Err(EngineError::LeaseDeadline)
    ));
    assert!(fake.writes.lock().unwrap().is_empty());
    *fake.result.lock().unwrap() = Some(Ok(Outcome::Delivered));
    worker.drive(&lease, 100).await.unwrap();
    assert_eq!(
        store.load_job(1).await.unwrap().job.failure(),
        Some(Failure::Deadline)
    );
    let effects = fake.writes.lock().unwrap().clone();
    assert_eq!(
        effects,
        [
            Effect::NotifyAdmin {
                failure: Failure::Deadline
            },
            Effect::NotifyViewerFailure {
                failure: Failure::Deadline
            }
        ]
    );
    assert_eq!(worker.drive(&lease, 101).await.unwrap(), 0);
}

#[tokio::test]
async fn uncertain_notifications_are_never_automatically_repeated() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).await.unwrap();
    let lease = setup(&store).await;
    let fake = Fake::default();
    *fake.result.lock().unwrap() = Some(Err(CallError::Timeout));
    let worker = Worker::new(Some(store.clone()), fake.clone(), live()).unwrap();
    worker
        .apply(&lease, "fail", Event::Fail(Failure::Cancelled), 1)
        .await
        .unwrap();
    assert_eq!(worker.drive(&lease, 2).await.unwrap(), 2);
    assert_eq!(worker.drive(&lease, 3).await.unwrap(), 0);
    assert_eq!(fake.writes.lock().unwrap().len(), 2);
    assert!(
        store
            .outbox(1)
            .await
            .unwrap()
            .iter()
            .skip(1)
            .all(|r| r.status == OutboxStatus::Uncertain)
    );
}

#[tokio::test]
async fn restart_at_every_worker_await_never_duplicates_a_grab() {
    let points = [
        Boundary::LoadBefore,
        Boundary::LoadAfter,
        Boundary::OutboxBefore,
        Boundary::OutboxAfter,
        Boundary::StartBefore,
        Boundary::StartAfter,
        Boundary::CallBefore,
        Boundary::CallAfter,
        Boundary::ResultBefore,
        Boundary::ResultAfter,
    ];
    for point in points {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("db");
        let store = Store::open(&path).await.unwrap();
        store.create_job(&spec(), "create", limits()).await.unwrap();
        let lease = store.acquire_lease(1, "worker", 0, 50).await.unwrap();
        store
            .apply_event(
                &lease,
                0,
                "select",
                Event::SelectRelease {
                    release: 8,
                    access: access(),
                },
                0,
            )
            .await
            .unwrap();
        let fake = Fake::default();
        let worker = Worker::new(Some(store.clone()), fake.clone(), live()).unwrap();
        let reached = Arc::new(tokio::sync::Notify::new());
        let notify = reached.clone();
        let task = tokio::spawn(async move {
            worker
                .drive_with(&lease, 1, move |boundary| {
                    let notify = notify.clone();
                    async move {
                        if boundary == point {
                            notify.notify_one();
                            std::future::pending::<()>().await;
                        }
                    }
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), reached.notified())
            .await
            .expect("worker did not reach the requested crash boundary");
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let reopened = Store::open(&path).await.unwrap();
        let lease = reopened
            .acquire_lease(1, "restart", 50, 1000)
            .await
            .unwrap();
        // Restart before the stage deadline, with a fresh fencing token.
        let worker = Worker::new(Some(reopened.clone()), fake.clone(), live()).unwrap();
        worker.drive(&lease, 50).await.unwrap();
        worker.drive(&lease, 51).await.unwrap();
        assert_eq!(
            reopened.load_job(1).await.unwrap().job.state(),
            State::Downloading
        );
        assert!(
            fake.writes
                .lock()
                .unwrap()
                .iter()
                .filter(|e| matches!(e, Effect::Grab { .. }))
                .count()
                <= 1,
            "{point:?}"
        );
        assert_eq!(reopened.load_job(1).await.unwrap().job.reserved_budget(), 1);
    }
}

#[tokio::test]
async fn live_full_lifecycle_checks_exact_import_before_ready() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).await.unwrap();
    let lease = setup(&store).await;
    let fake = Fake::default();
    let worker = Worker::new(Some(store.clone()), fake.clone(), live()).unwrap();
    worker.drive(&lease, 1).await.unwrap();
    let downloaded = FileIdentity {
        id: 11,
        revision: 1,
    };
    let imported = FileIdentity {
        id: 12,
        revision: 1,
    };
    worker
        .apply(
            &lease,
            "completed",
            Event::Observed {
                intent: IntentKey {
                    job: 1,
                    sequence: 1,
                },
                observation: Observation::DownloadCompleted {
                    download: 51,
                    file: downloaded,
                },
            },
            2,
        )
        .await
        .unwrap();
    *fake.result.lock().unwrap() = Some(Ok(Outcome::Observed(Observation::DownloadDecoded {
        file: downloaded,
        good: true,
    })));
    worker.drive(&lease, 3).await.unwrap();
    worker
        .apply(
            &lease,
            "import",
            Event::RequestImport { access: access() },
            4,
        )
        .await
        .unwrap();
    *fake.result.lock().unwrap() = Some(Ok(Outcome::Observed(Observation::ImportAccepted)));
    worker.drive(&lease, 5).await.unwrap();
    worker
        .apply(
            &lease,
            "imported",
            Event::Observed {
                intent: IntentKey {
                    job: 1,
                    sequence: 3,
                },
                observation: Observation::ImportCompleted {
                    download: 51,
                    file: imported,
                },
            },
            6,
        )
        .await
        .unwrap();
    *fake.result.lock().unwrap() = Some(Ok(Outcome::Observed(Observation::ImportedDecoded {
        file: imported,
        good: true,
    })));
    worker.drive(&lease, 7).await.unwrap();
    *fake.result.lock().unwrap() = Some(Ok(Outcome::Observed(Observation::TargetChecked {
        file: imported,
        plex_file: imported,
        constraints_satisfied: true,
    })));
    worker.drive(&lease, 8).await.unwrap();
    assert_eq!(
        store.load_job(1).await.unwrap().job.state(),
        State::Verified
    );
    worker
        .apply(&lease, "ready", Event::QueueReady, 9)
        .await
        .unwrap();
    *fake.result.lock().unwrap() = Some(Ok(Outcome::Delivered));
    worker.drive(&lease, 10).await.unwrap();
    assert_eq!(store.load_job(1).await.unwrap().job.state(), State::Done);
    assert_eq!(fake.reads.lock().unwrap().len(), 3);
    assert_eq!(fake.writes.lock().unwrap().len(), 3);
    assert_eq!(
        fake.writes.lock().unwrap()[2],
        Effect::NotifyReady { file: imported }
    );
}

#[tokio::test]
async fn externally_accepted_grab_is_not_dispatched_from_pending_outbox() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).await.unwrap();
    let lease = setup(&store).await;
    let fake = Fake::default();
    let worker = Worker::new(Some(store.clone()), fake.clone(), live()).unwrap();
    worker
        .apply(
            &lease,
            "external",
            Event::Observed {
                intent: IntentKey {
                    job: 1,
                    sequence: 1,
                },
                observation: Observation::GrabAccepted { download: 51 },
            },
            1,
        )
        .await
        .unwrap();
    worker.drive(&lease, 2).await.unwrap();
    assert!(fake.writes.lock().unwrap().is_empty());
    assert_eq!(
        store.outbox(1).await.unwrap()[0].status,
        OutboxStatus::Suppressed
    );
}

#[tokio::test]
async fn access_must_still_be_current_at_mutation_dispatch() {
    for dispatch_at in [2, 3] {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("db")).await.unwrap();
        store.create_job(&spec(), "create", limits()).await.unwrap();
        let lease = store.acquire_lease(1, "worker", 0, 500).await.unwrap();
        store
            .apply_event(
                &lease,
                0,
                "select",
                Event::SelectRelease {
                    release: 8,
                    access: Access::Granted {
                        library: 3,
                        expires_at: 2,
                    },
                },
                1,
            )
            .await
            .unwrap();
        let fake = Fake::default();
        let worker = Worker::new(Some(store.clone()), fake.clone(), live()).unwrap();
        assert!(worker.drive(&lease, dispatch_at).await.is_err());
        assert!(fake.writes.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn recorded_shadow_events_keep_domain_time_and_use_current_lease_clock() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).await.unwrap();
    store.create_job(&spec(), "create", limits()).await.unwrap();
    let lease = store
        .acquire_lease(1, "recorded-worker", 1000, 100)
        .await
        .unwrap();
    let worker = Worker::new(
        Some(store.clone()),
        Fake {
            forbid: true,
            ..Fake::default()
        },
        Options::default(),
    )
    .unwrap();
    worker
        .apply_recorded(
            &lease,
            "select",
            Event::SelectRelease {
                release: 8,
                access: access(),
            },
            1,
            1001,
        )
        .await
        .unwrap();
    assert_eq!(worker.record_decisions(&lease, 1001).await.unwrap(), 1);
    assert_eq!(worker.record_decisions(&lease, 1002).await.unwrap(), 0);
    assert_eq!(
        store.load_job(1).await.unwrap().job.state(),
        State::GrabIntent
    );
    let rows = store.observations().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].source, "engine_decision");
    assert_eq!(rows[0].key, "1:1");
    assert_eq!(rows[0].payload["intent"]["effect"]["Grab"]["release"], 8);
    assert_eq!(
        store.outbox(1).await.unwrap()[0].status,
        OutboxStatus::Shadowed
    );
    let live_worker = Worker::new(Some(store.clone()), Fake::default(), live()).unwrap();
    assert!(
        live_worker
            .apply_recorded(&lease, "forbidden", Event::Tick, 2, 1002)
            .await
            .is_err()
    );
    assert!(live_worker.record_decisions(&lease, 1002).await.is_err());
    assert!(
        worker
            .apply_recorded(&lease, "expired", Event::Tick, 2, 1100)
            .await
            .is_err()
    );
}
