use super::*;
use pitcrew_domain::{Access, Effect, Failure, Observation, State};
use proptest::prelude::*;

fn spec(id: u64) -> NewJob {
    NewJob {
        id,
        viewer: "viewer-a".into(),
        resource: format!("media-{id}"),
        library: 7,
        original: FileIdentity { id: 4, revision: 1 },
        constraints: Constraints::default(),
        policy: Policy {
            max_grabs: 2,
            stage_timeout: 30,
            total_timeout: 300,
        },
        timeouts: StageTimeouts {
            download: 60,
            import: 40,
            decode: 50,
            verify_retry: 20,
        },
        created_at: 0,
    }
}
fn limits() -> BudgetLimits {
    BudgetLimits {
        global_hour: 100,
        global_day: 100,
        viewer_hour: 100,
        viewer_day: 100,
    }
}
fn select() -> Event {
    Event::SelectRelease {
        release: 88,
        access: Access::Granted {
            library: 7,
            expires_at: 200,
        },
    }
}
async fn setup() -> (tempfile::TempDir, Store, Lease) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("journal.db")).await.unwrap();
    store
        .create_job(&spec(1), "create-1", limits())
        .await
        .unwrap();
    let lease = store.acquire_lease(1, "worker-a", 0, 100).await.unwrap();
    (dir, store, lease)
}

#[tokio::test]
async fn replay_survives_reopen_and_creation_is_idempotent() {
    let (dir, store, lease) = setup().await;
    assert_eq!(store.jobs().await.unwrap(), vec![1]);
    let first = store.load_job(1).await.unwrap();
    assert_eq!(first.spec, spec(1));
    assert_eq!(first.revision, 0);
    assert_eq!(first.job.state(), State::Picked);
    let applied = store
        .apply_event(&lease, 0, "select", select(), 1)
        .await
        .unwrap();
    assert!(!applied.duplicate);
    assert_eq!(applied.stored.revision, 1);
    assert_eq!(
        applied.intents,
        vec![Intent {
            job: 1,
            sequence: 1,
            effect: Effect::Grab {
                release: 88,
                reservation: 1
            }
        }]
    );
    assert_eq!(applied.stored.job.deadline(), Some(31));
    let duplicate = store
        .apply_event(&lease, 0, "select", select(), 2)
        .await
        .unwrap();
    assert!(duplicate.duplicate);
    assert!(duplicate.intents.is_empty());
    assert_eq!(duplicate.stored, applied.stored);
    assert!(matches!(
        store.apply_event(&lease, 1, "select", Event::Tick, 3).await,
        Err(StoreError::IdempotencyConflict)
    ));
    assert!(matches!(
        store.apply_event(&lease, 0, "other", Event::Tick, 3).await,
        Err(StoreError::RevisionConflict)
    ));
    assert_eq!(
        store
            .create_job(&spec(1), "create-1", limits())
            .await
            .unwrap(),
        applied.stored
    );
    assert!(matches!(
        store.create_job(&spec(1), "different", limits()).await,
        Err(StoreError::IdempotencyConflict)
    ));
    assert!(matches!(
        store.create_job(&spec(2), "create-1", limits()).await,
        Err(StoreError::IdempotencyConflict)
    ));
    let mut changed = spec(1);
    changed.constraints.require_sdr = true;
    assert!(matches!(
        store.create_job(&changed, "create-1", limits()).await,
        Err(StoreError::IdempotencyConflict)
    ));
    store.pool.close().await;
    let reopened = Store::open(dir.path().join("journal.db")).await.unwrap();
    assert_eq!(reopened.load_job(1).await.unwrap(), applied.stored);
    assert_eq!(
        reopened.outbox(1).await.unwrap(),
        vec![OutboxEntry {
            intent: applied.intents[0].clone(),
            status: OutboxStatus::Pending,
            fence: None
        }]
    );
}

#[tokio::test]
async fn leases_fence_all_writes_and_same_owner_cannot_steal() {
    let (_, store, lease) = setup().await;
    for owner in ["worker-a", "worker-b"] {
        assert!(matches!(
            store.acquire_lease(1, owner, 99, 10).await,
            Err(StoreError::LeaseBusy)
        ));
    }
    assert!(matches!(
        store
            .apply_event(&lease, 0, "expired", Event::Tick, 100)
            .await,
        Err(StoreError::Fenced)
    ));
    let next = store.acquire_lease(1, "worker-a", 100, 11).await.unwrap();
    assert_eq!(
        next,
        Lease {
            job: 1,
            owner: "worker-a".into(),
            fence: 2,
            expires_at: 111
        }
    );
    let intent = IntentKey {
        job: 1,
        sequence: 1,
    };
    assert!(matches!(
        store.start_intent(&lease, intent, 1, 100).await,
        Err(StoreError::Fenced)
    ));
    assert!(matches!(
        store
            .finish_intent(&lease, intent, OutboxStatus::Completed, 100)
            .await,
        Err(StoreError::Fenced)
    ));
    let mut wrong = next.clone();
    wrong.owner = "worker-b".into();
    assert!(matches!(
        store
            .apply_event(&wrong, 0, "wrong", Event::Tick, 100)
            .await,
        Err(StoreError::Fenced)
    ));
    wrong = next.clone();
    wrong.fence = 1;
    assert!(matches!(
        store
            .apply_event(&wrong, 0, "wrong", Event::Tick, 100)
            .await,
        Err(StoreError::Fenced)
    ));
    assert!(matches!(
        store.apply_event(&next, 0, "equal", Event::Tick, 111).await,
        Err(StoreError::Fenced)
    ));
    assert_eq!(
        store
            .apply_event(&next, 0, "deadline", Event::Tick, 110)
            .await
            .unwrap()
            .stored
            .job
            .failure(),
        Some(Failure::Deadline)
    );
    assert!(matches!(
        store.acquire_lease(99, "worker-a", 0, 10).await,
        Err(StoreError::Missing)
    ));
    assert!(matches!(store.load_job(99).await, Err(StoreError::Missing)));
}

#[tokio::test]
async fn started_effect_is_never_claimed_twice_and_result_is_atomic() {
    let (dir, store, lease) = setup().await;
    store
        .apply_event(&lease, 0, "select", select(), 1)
        .await
        .unwrap();
    let intent = IntentKey {
        job: 1,
        sequence: 1,
    };
    assert!(
        !store
            .start_intent(
                &lease,
                IntentKey {
                    job: 1,
                    sequence: 9
                },
                1,
                2
            )
            .await
            .unwrap()
    );
    assert!(store.start_intent(&lease, intent, 1, 2).await.unwrap());
    assert!(!store.start_intent(&lease, intent, 1, 2).await.unwrap());
    let row = &store.outbox(1).await.unwrap()[0];
    assert_eq!(row.status, OutboxStatus::Started);
    assert_eq!(row.fence, Some(1));
    // Crash with a started mutation: the durable marker remains after reopening.
    store.pool.close().await;
    let reopened = Store::open(dir.path().join("journal.db")).await.unwrap();
    let lease2 = reopened
        .acquire_lease(1, "recovery", 100, 20)
        .await
        .unwrap();
    assert!(
        !reopened
            .start_intent(&lease2, intent, 1, 100)
            .await
            .unwrap()
    );
    assert_eq!(
        reopened.outbox(1).await.unwrap()[0].status,
        OutboxStatus::Started
    );
    // A result after the stage deadline fails safely, preserving unresolved ownership.
    let result = reopened
        .apply_result(
            &lease2,
            1,
            "recovery",
            Event::Observed {
                intent,
                observation: Observation::GrabAmbiguous,
            },
            100,
            intent,
        )
        .await
        .unwrap();
    assert_eq!(result.stored.job.failure(), Some(Failure::Deadline));
    assert_eq!(
        reopened.outbox(1).await.unwrap()[0].status,
        OutboxStatus::Completed
    );
    assert_eq!(reopened.outbox(1).await.unwrap().len(), 3);
    assert!(
        reopened
            .apply_result(
                &lease2,
                1,
                "recovery",
                Event::Observed {
                    intent,
                    observation: Observation::GrabAmbiguous
                },
                101,
                intent
            )
            .await
            .unwrap()
            .duplicate
    );
}

#[tokio::test]
async fn rejected_results_roll_back_and_valid_result_advances_reducer() {
    let (_, store, lease) = setup().await;
    store
        .apply_event(&lease, 0, "select", select(), 1)
        .await
        .unwrap();
    let intent = IntentKey {
        job: 1,
        sequence: 1,
    };
    let event = Event::Observed {
        intent,
        observation: Observation::GrabAccepted { download: 91 },
    };
    assert!(matches!(
        store
            .apply_result(&lease, 1, "result", event.clone(), 2, intent)
            .await,
        Err(StoreError::InvalidInput)
    ));
    assert_eq!(store.load_job(1).await.unwrap().revision, 1);
    store.start_intent(&lease, intent, 1, 2).await.unwrap();
    for bad in [
        IntentKey {
            job: 2,
            sequence: 1,
        },
        IntentKey {
            job: 1,
            sequence: 2,
        },
    ] {
        assert!(matches!(
            store
                .apply_result(&lease, 1, "result", event.clone(), 2, bad)
                .await,
            Err(StoreError::InvalidInput)
        ));
    }
    let result = store
        .apply_result(&lease, 1, "result", event, 2, intent)
        .await
        .unwrap();
    assert_eq!(result.stored.job.state(), State::Downloading);
    assert_eq!(result.stored.job.deadline(), Some(62));
    assert!(result.intents.is_empty());
    assert_eq!(
        store.outbox(1).await.unwrap()[0].status,
        OutboxStatus::Completed
    );
}

#[tokio::test]
async fn completion_statuses_are_final_and_scoped() {
    for status in [
        OutboxStatus::Completed,
        OutboxStatus::Uncertain,
        OutboxStatus::Suppressed,
        OutboxStatus::Shadowed,
    ] {
        let (_, store, lease) = setup().await;
        store
            .apply_event(&lease, 0, "select", select(), 1)
            .await
            .unwrap();
        let intent = IntentKey {
            job: 1,
            sequence: 1,
        };
        for bad in [OutboxStatus::Pending, OutboxStatus::Started] {
            assert!(matches!(
                store.finish_intent(&lease, intent, bad, 2).await,
                Err(StoreError::InvalidInput)
            ));
        }
        assert!(matches!(
            store
                .finish_intent(
                    &lease,
                    IntentKey {
                        job: 2,
                        sequence: 1
                    },
                    status,
                    2
                )
                .await,
            Err(StoreError::InvalidInput)
        ));
        assert!(matches!(
            store
                .start_intent(
                    &lease,
                    IntentKey {
                        job: 2,
                        sequence: 1
                    },
                    1,
                    2
                )
                .await,
            Err(StoreError::InvalidInput)
        ));
        store
            .finish_intent(&lease, intent, status, 2)
            .await
            .unwrap();
        assert_eq!(store.outbox(1).await.unwrap()[0].status, status);
        assert!(!store.start_intent(&lease, intent, 1, 2).await.unwrap());
        if status == OutboxStatus::Uncertain {
            store
                .finish_intent(&lease, intent, OutboxStatus::Completed, 3)
                .await
                .unwrap();
        }
        assert!(matches!(
            store
                .finish_intent(&lease, intent, OutboxStatus::Completed, 3)
                .await,
            Err(StoreError::InvalidInput)
        ));
    }
}

#[tokio::test]
async fn terminal_failure_suppresses_pending_effect_and_quarantines_unresolved_item() {
    let (_, store, lease) = setup().await;
    store
        .apply_event(&lease, 0, "select", select(), 1)
        .await
        .unwrap();
    let fail = store
        .apply_event(&lease, 1, "fail", Event::Fail(Failure::Cancelled), 2)
        .await
        .unwrap();
    assert_eq!(fail.intents.len(), 2);
    let rows = store.outbox(1).await.unwrap();
    assert_eq!(rows[0].status, OutboxStatus::Suppressed);
    assert_eq!(rows[1].status, OutboxStatus::Pending);
    assert_eq!(rows[2].status, OutboxStatus::Pending);
    assert!(matches!(
        rows[1].intent.effect,
        Effect::NotifyAdmin {
            failure: Failure::Cancelled
        }
    ));
    assert!(matches!(
        rows[2].intent.effect,
        Effect::NotifyViewerFailure {
            failure: Failure::Cancelled
        }
    ));
    assert!(matches!(
        store
            .apply_event(&lease, 2, "second-fail", Event::Fail(Failure::Cancelled), 3)
            .await,
        Err(StoreError::Domain(_))
    ));
    let mut next = spec(2);
    next.resource = spec(1).resource;
    assert!(matches!(
        store.create_job(&next, "create-2", limits()).await,
        Err(StoreError::ResourceBusy)
    ));
    let (_, store, lease) = setup().await;
    store
        .apply_event(&lease, 0, "fail", Event::Fail(Failure::Cancelled), 2)
        .await
        .unwrap();
    store.create_job(&next, "create-2", limits()).await.unwrap();
    assert_eq!(store.jobs().await.unwrap(), vec![1, 2]);
}

#[tokio::test]
async fn write_failure_rolls_back_event_revision_and_every_intent() {
    let (_, store, lease) = setup().await;
    sqlx::query("CREATE TRIGGER reject_outbox BEFORE INSERT ON outbox BEGIN SELECT RAISE(ABORT, 'injected fault'); END;")
        .execute(&store.pool).await.unwrap();
    assert!(matches!(
        store.apply_event(&lease, 0, "select", select(), 1).await,
        Err(StoreError::Database(_))
    ));
    assert_eq!(store.load_job(1).await.unwrap().revision, 0);
    assert!(store.outbox(1).await.unwrap().is_empty());
    sqlx::query("DROP TRIGGER reject_outbox")
        .execute(&store.pool)
        .await
        .unwrap();
    assert_eq!(
        store
            .apply_event(&lease, 0, "select", select(), 1)
            .await
            .unwrap()
            .stored
            .revision,
        1
    );
}

#[tokio::test]
async fn observation_ingest_is_append_only_idempotent_and_source_scoped() {
    let (_, store, _) = setup().await;
    let payload = serde_json::json!({"event":"download","download":12});
    assert!(
        store
            .ingest_observation("arr-a", "event-1", &payload, 9)
            .await
            .unwrap()
    );
    assert!(
        !store
            .ingest_observation("arr-a", "event-1", &payload, 2)
            .await
            .unwrap()
    );
    assert!(matches!(
        store
            .ingest_observation("arr-a", "event-1", &serde_json::json!({}), 10)
            .await,
        Err(StoreError::IdempotencyConflict)
    ));
    assert!(
        store
            .ingest_observation("arr-b", "event-1", &payload, 10)
            .await
            .unwrap()
    );
    assert_eq!(
        store.observations().await.unwrap(),
        vec![
            ObservationRecord {
                id: 1,
                source: "arr-a".into(),
                key: "event-1".into(),
                payload: payload.clone(),
                at: 9
            },
            ObservationRecord {
                id: 2,
                source: "arr-b".into(),
                key: "event-1".into(),
                payload,
                at: 10
            }
        ]
    );
    for sql in ["UPDATE observations SET at=0", "DELETE FROM observations"] {
        assert!(sqlx::query(sql).execute(&store.pool).await.is_err());
    }
}

#[tokio::test]
async fn invalid_input_and_corrupt_journal_fail_closed() {
    let (_, store, lease) = setup().await;
    assert!(matches!(
        store.acquire_lease(1, "", 0, 10).await,
        Err(StoreError::InvalidInput)
    ));
    assert!(matches!(
        store.acquire_lease(1, "owner", 0, 0).await,
        Err(StoreError::InvalidInput)
    ));
    assert!(matches!(
        store.acquire_lease(1, "owner", u64::MAX, 2).await,
        Err(StoreError::InvalidInput)
    ));
    assert!(matches!(
        store.load_job(u64::MAX).await,
        Err(StoreError::InvalidInput)
    ));
    assert!(matches!(
        store.apply_event(&lease, 0, "", Event::Tick, 1).await,
        Err(StoreError::InvalidInput)
    ));
    for (source, key) in [("", "key"), ("source", "")] {
        assert!(matches!(
            store
                .ingest_observation(source, key, &serde_json::json!(null), 0)
                .await,
            Err(StoreError::InvalidInput)
        ));
    }
    for field in 0..3 {
        let mut invalid = spec(2);
        let mut key = "create-2";
        match field {
            0 => key = "",
            1 => invalid.viewer.clear(),
            _ => invalid.resource.clear(),
        }
        assert!(matches!(
            store.create_job(&invalid, key, limits()).await,
            Err(StoreError::InvalidInput)
        ));
    }
    assert!(matches!(unsigned(-1), Err(StoreError::Corrupt)));
    assert_eq!(unsigned(0).unwrap(), 0);
    store
        .apply_event(&lease, 0, "select", select(), 1)
        .await
        .unwrap();
    for sql in ["UPDATE events SET at=9", "DELETE FROM events"] {
        assert!(sqlx::query(sql).execute(&store.pool).await.is_err());
    }
    sqlx::query("UPDATE jobs SET revision=2 WHERE id=1")
        .execute(&store.pool)
        .await
        .unwrap();
    assert!(matches!(store.load_job(1).await, Err(StoreError::Corrupt)));
    sqlx::query("UPDATE jobs SET revision=1,spec=json_set(spec,'$.id',2) WHERE id=1")
        .execute(&store.pool)
        .await
        .unwrap();
    assert!(matches!(store.load_job(1).await, Err(StoreError::Corrupt)));
}

#[tokio::test]
async fn rolling_hour_and_day_budgets_include_zero_and_exact_boundaries() {
    for scope in 0..4 {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("budget.db")).await.unwrap();
        let mut caps = limits();
        match scope {
            0 => caps.global_hour = 2,
            1 => caps.global_day = 2,
            2 => caps.viewer_hour = 2,
            _ => caps.viewer_day = 2,
        }
        store.create_job(&spec(1), "a", caps).await.unwrap();
        let lease = store.acquire_lease(1, "worker", 0, 10).await.unwrap();
        store
            .apply_event(&lease, 0, "cancel", Event::Fail(Failure::Cancelled), 0)
            .await
            .unwrap();
        let duration = if scope % 2 == 0 { 3600 } else { 86400 };
        for at in [0, 1, duration - 1] {
            let mut next = spec(2);
            next.created_at = at;
            assert!(
                matches!(
                    store.create_job(&next, "b", caps).await,
                    Err(StoreError::BudgetExhausted)
                ),
                "scope {scope} at {at}"
            );
            assert_eq!(store.jobs().await.unwrap(), vec![1]);
        }
        let mut next = spec(2);
        next.created_at = duration;
        store.create_job(&next, "b", caps).await.unwrap();
        let mut rollback = spec(3);
        rollback.created_at = duration - 1;
        assert!(matches!(
            store.create_job(&rollback, "c", caps).await,
            Err(StoreError::BudgetExhausted)
        ));
    }
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("viewers.db")).await.unwrap();
    let caps = BudgetLimits {
        global_hour: 4,
        global_day: 4,
        viewer_hour: 2,
        viewer_day: 2,
    };
    store.create_job(&spec(1), "a", caps).await.unwrap();
    let mut other = spec(2);
    other.viewer = "viewer-b".into();
    store.create_job(&other, "b", caps).await.unwrap();
    other.id = 3;
    other.resource = "media-3".into();
    other.viewer = "viewer-c".into();
    assert!(matches!(
        store.create_job(&other, "c", caps).await,
        Err(StoreError::BudgetExhausted)
    ));
}

#[tokio::test]
async fn concurrent_creates_cannot_overspend_or_double_own() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("concurrent.db");
    let one = Store::open(&path).await.unwrap();
    let two = Store::open(&path).await.unwrap();
    let caps = BudgetLimits {
        global_hour: 2,
        global_day: 2,
        viewer_hour: 2,
        viewer_day: 2,
    };
    let a = spec(1);
    let b = spec(2);
    let (left, right) = tokio::join!(one.create_job(&a, "a", caps), two.create_job(&b, "b", caps));
    assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
    assert_eq!(one.jobs().await.unwrap().len(), 1);
}

#[tokio::test]
async fn migrations_upgrade_each_committed_schema_and_preserve_data() {
    // Every historical migration is replayed as a prefix, followed by current open.
    for prefix in 0..=MIGRATOR.migrations.len() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("migration.db");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        let mut migration = sqlx::migrate::Migrator::DEFAULT;
        migration.migrations = std::borrow::Cow::Owned(MIGRATOR.migrations[..prefix].to_vec());
        migration.run(&pool).await.unwrap();
        pool.close().await;
        let store = Store::open(&path).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM _sqlx_migrations")
                .fetch_one(&store.pool)
                .await
                .unwrap(),
            MIGRATOR.migrations.len() as i64
        );
        store.create_job(&spec(1), "one", limits()).await.unwrap();
        store.pool.close().await;
        assert_eq!(
            Store::open(&path)
                .await
                .unwrap()
                .load_job(1)
                .await
                .unwrap()
                .spec,
            spec(1)
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]
    #[test]
    fn reservation_sequences_never_exceed_envelope(caps in 0u32..12, requests in prop::collection::vec((0u32..5,any::<bool>()),1..18)) {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let dir=tempfile::tempdir().unwrap();let store=Store::open(dir.path().join("property.db")).await.unwrap();
            let limits=BudgetLimits{global_hour:caps,global_day:caps,viewer_hour:caps/2,viewer_day:caps/2};
            let (mut global,mut a,mut b)=(0,0,0);
            for (i,(units,viewer_b)) in requests.into_iter().enumerate() {
                let mut next=spec(i as u64+1);next.policy.max_grabs=units;if viewer_b {next.viewer="viewer-b".into();}
                let used=if viewer_b {b} else {a}; let expected=global+units<=caps && used+units<=caps/2;
                let actual=store.create_job(&next,&format!("create-{i}"),limits).await;
                assert_eq!(actual.is_ok(),expected);
                if expected {global+=units;if viewer_b {b+=units;} else {a+=units;}}
            }
        });
    }
}

// A child process is terminated without dropping its SQLite connection at each
// observed suspension of the real transition future. The OS closes descriptors;
// reopening must see the entire commit or none of it, including after COMMIT was
// sent but its acknowledgement had not reached the Rust future yet.
#[test]
fn crash_child() {
    let Ok(path) = std::env::var("PITCREW_STORE_CRASH_PATH") else {
        return;
    };
    let cut: usize = std::env::var("PITCREW_STORE_CRASH_CUT")
        .unwrap()
        .parse()
        .unwrap();
    let operation = std::env::var("PITCREW_STORE_CRASH_OPERATION").unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let store = Store::open(path).await.unwrap();
            let lease = if operation == "create" {
                None
            } else {
                Some(store.acquire_lease(1, "child", 100, 200).await.unwrap())
            };
            let future = async {
                match operation.as_str() {
                    "create" => {
                        store
                            .create_job_with_cooldown(&spec(1), "create", limits(), 10)
                            .await
                            .unwrap();
                    }
                    "select" => {
                        store
                            .apply_event(lease.as_ref().unwrap(), 0, "select", select(), 101)
                            .await
                            .unwrap();
                    }
                    "start" => {
                        store
                            .start_intent(
                                lease.as_ref().unwrap(),
                                IntentKey {
                                    job: 1,
                                    sequence: 1,
                                },
                                1,
                                101,
                            )
                            .await
                            .unwrap();
                    }
                    "result" => {
                        store
                            .apply_result(
                                lease.as_ref().unwrap(),
                                1,
                                "result",
                                Event::Observed {
                                    intent: IntentKey {
                                        job: 1,
                                        sequence: 1,
                                    },
                                    observation: Observation::GrabAccepted { download: 91 },
                                },
                                101,
                                IntentKey {
                                    job: 1,
                                    sequence: 1,
                                },
                            )
                            .await
                            .unwrap();
                    }
                    _ => unreachable!(),
                }
            };
            let mut future = Box::pin(future);
            let mut suspensions = 0;
            std::future::poll_fn(|cx| {
                let result = std::future::Future::poll(future.as_mut(), cx);
                if result.is_pending() {
                    suspensions += 1;
                    if suspensions == cut {
                        std::process::exit(77);
                    }
                }
                result
            })
            .await;
        });
}

#[test]
fn process_death_at_store_suspensions_preserves_atomicity() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for operation in ["create", "select", "start", "result"] {
        let mut finished = false;
        for cut in 1..80 {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("crash.db");
            runtime.block_on(async {
                let store = Store::open(&path).await.unwrap();
                if operation != "create" {
                    let mut input = spec(1);
                    input.created_at = 90;
                    store.create_job(&input, "create", limits()).await.unwrap();
                    let lease = store.acquire_lease(1, "parent", 90, 10).await.unwrap();
                    if operation == "start" || operation == "result" {
                        store
                            .apply_event(&lease, 0, "select", select(), 91)
                            .await
                            .unwrap();
                    }
                    if operation == "result" {
                        store
                            .start_intent(
                                &lease,
                                IntentKey {
                                    job: 1,
                                    sequence: 1,
                                },
                                1,
                                92,
                            )
                            .await
                            .unwrap();
                    }
                }
                store.pool.close().await;
            });
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "tests::crash_child"])
                .env("PITCREW_STORE_CRASH_PATH", &path)
                .env("PITCREW_STORE_CRASH_CUT", cut.to_string())
                .env("PITCREW_STORE_CRASH_OPERATION", operation)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(
                status.success() || status.code() == Some(77),
                "unexpected crash child: {status}"
            );
            runtime.block_on(async {
                let store = Store::open(&path).await.unwrap();
                let ids = store.jobs().await.unwrap();
                if operation == "create" {
                    assert_eq!(
                        ids.len(),
                        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM item_cooldowns")
                            .fetch_one(&store.pool)
                            .await
                            .unwrap() as usize
                    );
                    assert_eq!(
                        ids.len(),
                        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM budget_reservations")
                            .fetch_one(&store.pool)
                            .await
                            .unwrap() as usize
                    );
                    assert_eq!(
                        ids.len(),
                        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM ownership")
                            .fetch_one(&store.pool)
                            .await
                            .unwrap() as usize
                    );
                    if status.success() {
                        assert_eq!(ids, vec![1]);
                    }
                } else {
                    let job = store.load_job(1).await.unwrap();
                    let outbox = store.outbox(1).await.unwrap();
                    match operation {
                        "select" => {
                            assert!(job.revision <= 1);
                            assert_eq!(outbox.len(), job.revision as usize);
                            if job.revision == 1 {
                                assert_eq!(job.job.state(), State::GrabIntent);
                            }
                        }
                        "start" => {
                            assert_eq!(job.revision, 1);
                            assert_eq!(outbox.len(), 1);
                            assert!(matches!(
                                outbox[0].status,
                                OutboxStatus::Pending | OutboxStatus::Started
                            ));
                        }
                        "result" => {
                            assert!(matches!(job.revision, 1 | 2));
                            assert_eq!(outbox.len(), 1);
                            if job.revision == 2 {
                                assert_eq!(job.job.state(), State::Downloading);
                                assert_eq!(outbox[0].status, OutboxStatus::Completed);
                            } else {
                                assert_eq!(outbox[0].status, OutboxStatus::Started);
                            }
                        }
                        _ => unreachable!(),
                    }
                }
                store.pool.close().await;
            });
            if status.success() {
                assert!(cut > 1);
                finished = true;
                break;
            }
        }
        assert!(finished, "no completion reached for {operation}");
    }
}

#[tokio::test]
async fn active_envelope_does_not_age_out_and_spending_refreshes_released_window() {
    let (_, store, lease) = setup().await;
    let caps = BudgetLimits {
        global_hour: 2,
        global_day: 2,
        viewer_hour: 2,
        viewer_day: 2,
    };
    let mut next = spec(2);
    next.created_at = 86400;
    assert!(matches!(
        store.create_job(&next, "next", caps).await,
        Err(StoreError::BudgetExhausted)
    ));
    store
        .apply_event(&lease, 0, "select", select(), 20)
        .await
        .unwrap();
    let intent = IntentKey {
        job: 1,
        sequence: 1,
    };
    store
        .apply_event(
            &lease,
            1,
            "missing",
            Event::Observed {
                intent,
                observation: Observation::GrabAmbiguous,
            },
            21,
        )
        .await
        .unwrap();
    store
        .apply_event(
            &lease,
            2,
            "absent",
            Event::Observed {
                intent: IntentKey {
                    job: 1,
                    sequence: 2,
                },
                observation: Observation::GrabReconciled(pitcrew_domain::GrabResolution::Absent),
            },
            22,
        )
        .await
        .unwrap();
    store
        .apply_event(&lease, 3, "cancel", Event::Fail(Failure::Cancelled), 23)
        .await
        .unwrap();
    next.created_at = 86419;
    assert!(matches!(
        store.create_job(&next, "next", caps).await,
        Err(StoreError::BudgetExhausted)
    ));
    next.created_at = 86420;
    store.create_job(&next, "next", caps).await.unwrap();
}

#[tokio::test]
async fn bounded_observation_cursor_and_health_check() {
    let (_, store, _) = setup().await;
    store.health().await.unwrap();
    for i in 0..3 {
        store
            .ingest_observation("source", &i.to_string(), &serde_json::json!({"i":i}), i + 9)
            .await
            .unwrap();
    }
    let all = store.observations().await.unwrap();
    assert_eq!(store.observations_after(0, 1).await.unwrap(), all[..1]);
    assert_eq!(store.observations_after(1, 1000).await.unwrap(), all[1..]);
    assert!(store.observations_after(3, 5).await.unwrap().is_empty());
    for limit in [0, 1001] {
        assert!(matches!(
            store.observations_after(0, limit).await,
            Err(StoreError::InvalidInput)
        ));
    }
    store.pool.close().await;
    assert!(matches!(store.health().await, Err(StoreError::Database(_))));
}

#[tokio::test]
async fn guard_or_active_download_alone_keeps_terminal_ownership() {
    for guard in [true, false] {
        let (_, store, lease) = setup().await;
        if guard {
            store
                .apply_event(
                    &lease,
                    0,
                    "broken",
                    Event::BrokenFileConfirmed {
                        file: spec(1).original,
                    },
                    1,
                )
                .await
                .unwrap();
            store
                .apply_event(
                    &lease,
                    1,
                    "request",
                    Event::RequestLowerQuality {
                        candidate: pitcrew_domain::ReleaseCandidate {
                            release: 8,
                            identity: Default::default(),
                            quality: pitcrew_domain::QualityRelation::Lower,
                        },
                        access: Access::Granted {
                            library: 7,
                            expires_at: 99,
                        },
                    },
                    2,
                )
                .await
                .unwrap();
            store
                .apply_event(
                    &lease,
                    2,
                    "approve",
                    Event::ApproveLowerQuality {
                        approval: pitcrew_domain::AdminApproval {
                            job: 1,
                            release: 8,
                            broken_file: spec(1).original,
                            expires_at: 99,
                        },
                        access: Access::Granted {
                            library: 7,
                            expires_at: 99,
                        },
                    },
                    3,
                )
                .await
                .unwrap();
            store
                .apply_event(&lease, 3, "fail", Event::Fail(Failure::Cancelled), 4)
                .await
                .unwrap();
            let failed = store.load_job(1).await.unwrap();
            assert_eq!(failed.job.active_grabs(), 0);
            assert!(failed.job.unresolved_mutation().is_some());
        } else {
            store
                .apply_event(&lease, 0, "select", select(), 1)
                .await
                .unwrap();
            store
                .apply_event(
                    &lease,
                    1,
                    "accepted",
                    Event::Observed {
                        intent: IntentKey {
                            job: 1,
                            sequence: 1,
                        },
                        observation: Observation::GrabAccepted { download: 91 },
                    },
                    2,
                )
                .await
                .unwrap();
            store
                .apply_event(&lease, 2, "fail", Event::Fail(Failure::Cancelled), 3)
                .await
                .unwrap();
            let failed = store.load_job(1).await.unwrap();
            assert_eq!(failed.job.active_grabs(), 1);
            assert!(failed.job.unresolved_mutation().is_none());
        }
        let mut next = spec(2);
        next.resource = spec(1).resource;
        assert!(matches!(
            store.create_job(&next, "next", limits()).await,
            Err(StoreError::ResourceBusy)
        ));
    }
}

#[tokio::test]
async fn replay_reads_one_snapshot_while_another_connection_commits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snapshot.db");
    let reader = Store::open(&path).await.unwrap();
    let writer = Store::open(&path).await.unwrap();
    writer
        .create_job(&spec(1), "create", limits())
        .await
        .unwrap();
    let lease = writer.acquire_lease(1, "writer", 0, 100).await.unwrap();
    let (read, write) = tokio::join!(
        async {
            for _ in 0..120 {
                reader.load_job(1).await.unwrap();
            }
        },
        async {
            for revision in 0..120 {
                writer
                    .apply_event(
                        &lease,
                        revision,
                        &format!("tick-{revision}"),
                        Event::Tick,
                        1,
                    )
                    .await
                    .unwrap();
            }
        }
    );
    assert_eq!((read, write), ((), ()));
    assert_eq!(reader.load_job(1).await.unwrap().revision, 120);
}

#[tokio::test]
async fn intent_claim_is_fenced_by_the_revision_used_for_dispatch_validation() {
    let (_, store, lease) = setup().await;
    store
        .apply_event(&lease, 0, "select", select(), 1)
        .await
        .unwrap();
    let checked = store.load_job(1).await.unwrap();
    store
        .apply_event(&lease, 1, "racing-observation", Event::Tick, 2)
        .await
        .unwrap();
    let intent = IntentKey {
        job: 1,
        sequence: 1,
    };
    assert!(matches!(
        store
            .start_intent(&lease, intent, checked.revision, 3)
            .await,
        Err(StoreError::RevisionConflict)
    ));
    assert_eq!(
        store.outbox(1).await.unwrap()[0].status,
        OutboxStatus::Pending
    );
    assert!(store.start_intent(&lease, intent, 2, 3).await.unwrap());
}

#[tokio::test]
async fn durable_connections_enforce_foreign_keys_and_wal_commit_policy() {
    let (_, store, _) = setup().await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA foreign_keys")
            .fetch_one(&store.pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("PRAGMA journal_mode")
            .fetch_one(&store.pool)
            .await
            .unwrap(),
        "wal"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA synchronous")
            .fetch_one(&store.pool)
            .await
            .unwrap(),
        2
    );
    assert!(
        sqlx::query("INSERT INTO ownership(resource,job) VALUES('orphan',999)")
            .execute(&store.pool)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn recorded_events_use_real_lease_clock_and_preserve_original_event_time() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("recorded.db")).await.unwrap();
    store
        .create_job(&spec(1), "create", limits())
        .await
        .unwrap();
    let lease = store.acquire_lease(1, "replay", 1000, 20).await.unwrap();
    let applied = store
        .apply_recorded_event(&lease, 0, "select", select(), 1, 1001)
        .await
        .unwrap();
    assert_eq!(applied.stored.job.state(), State::GrabIntent);
    assert_eq!(applied.stored.job.deadline(), Some(31));
    assert_eq!(store.load_job(1).await.unwrap(), applied.stored);
    assert!(matches!(
        store
            .apply_recorded_event(&lease, 1, "expired", Event::Tick, 2, 1020)
            .await,
        Err(StoreError::Fenced)
    ));
    assert_eq!(store.load_job(1).await.unwrap().revision, 1);
}

#[tokio::test]
async fn lease_release_allows_immediate_reacquisition_and_cannot_release_new_owner() {
    let (_, store, lease) = setup().await;
    store.release_lease(&lease, 1).await.unwrap();
    assert!(matches!(
        store.apply_event(&lease, 0, "stale", Event::Tick, 1).await,
        Err(StoreError::Fenced)
    ));
    let next = store.acquire_lease(1, "replay", 1, 10).await.unwrap();
    assert_eq!(next.fence, lease.fence + 1);
    assert!(matches!(
        store.release_lease(&lease, 2).await,
        Err(StoreError::Fenced)
    ));
    assert!(matches!(
        store.acquire_lease(1, "thief", 2, 10).await,
        Err(StoreError::LeaseBusy)
    ));
    assert!(matches!(
        store.release_lease(&next, 11).await,
        Err(StoreError::Fenced)
    ));
    store.release_lease(&next, 10).await.unwrap();
    assert_eq!(
        store.acquire_lease(1, "replay", 10, 5).await.unwrap().fence,
        next.fence + 1
    );
}

#[tokio::test]
async fn scheduler_pages_are_bounded_and_resource_owner_tracks_terminal_release() {
    let (_dir, store, lease) = setup().await;
    assert_eq!(store.owned_job("media-1").await.unwrap(), Some(1));
    assert_eq!(store.owned_job("absent").await.unwrap(), None);
    let limits = BudgetLimits {
        global_hour: 1000,
        global_day: 1000,
        viewer_hour: 1000,
        viewer_day: 1000,
    };
    for id in 2..=102 {
        store
            .create_job(&spec(id), &format!("page-{id}"), limits)
            .await
            .unwrap();
    }
    assert!(store.jobs_page(0, 0).await.unwrap().is_empty());
    assert_eq!(
        store.jobs_page(0, u32::MAX).await.unwrap(),
        (1..=100).collect::<Vec<_>>()
    );
    assert_eq!(store.jobs_page(100, 2).await.unwrap(), vec![101, 102]);
    assert!(store.jobs_page(102, 100).await.unwrap().is_empty());
    assert!(store.jobs_page(u64::MAX, 1).await.is_err());
    store
        .apply_event(&lease, 0, "stop", Event::Fail(Failure::Deadline), 1)
        .await
        .unwrap();
    assert_eq!(store.owned_job("media-1").await.unwrap(), None);
    // Historical jobs remain pageable so final notifications can still be driven.
    assert_eq!(store.jobs_page(0, 2).await.unwrap(), vec![1, 2]);
}
