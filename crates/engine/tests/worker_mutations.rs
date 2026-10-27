//! Additional fault coverage: every mutating operation must reconcile a lost
//! response or process death without issuing a second mutation.
use pitcrew_domain::*;
use pitcrew_engine::*;
use pitcrew_store::*;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const ORIGINAL: FileIdentity = FileIdentity {
    id: 10,
    revision: 1,
};
const SOURCE: FileIdentity = FileIdentity {
    id: 11,
    revision: 2,
};
const IMPORTED: FileIdentity = FileIdentity {
    id: 12,
    revision: 3,
};
fn access() -> Access {
    Access::Granted {
        library: 3,
        expires_at: 10_000,
    }
}
fn live() -> Options {
    Options {
        mode: Mode::Live,
        acknowledge_live_mutations: true,
        call_timeout: Duration::from_millis(10),
    }
}
#[derive(Clone, Copy, Debug)]
enum Operation {
    Guard,
    Delete,
    Import,
}
impl Operation {
    fn initial(self) -> State {
        match self {
            Self::Guard => State::UpgradeGuardIntent,
            Self::Delete => State::RejectIntent,
            Self::Import => State::ImportIntent,
        }
    }
    fn uncertain(self) -> State {
        match self {
            Self::Guard => State::UpgradeGuardReconciling,
            Self::Delete => State::RejectReconciling,
            Self::Import => State::ImportReconciling,
        }
    }
    fn resolved(self) -> State {
        match self {
            Self::Guard => State::QualityApproved,
            Self::Delete => State::Rejected,
            Self::Import => State::Imported,
        }
    }
    fn reconciliation(self) -> Observation {
        match self {
            Self::Guard => Observation::UpgradeGuardReconciled(UpgradeGuardResolution::Installed),
            Self::Delete => Observation::DeleteReconciled(DeleteResolution::Absent),
            Self::Import => Observation::ImportReconciled(ImportResolution::Imported {
                download: 51,
                file: IMPORTED,
            }),
        }
    }
    fn write_matches(self, e: &Effect) -> bool {
        matches!(
            (self, e),
            (Self::Guard, Effect::InstallUpgradeGuard { .. })
                | (Self::Delete, Effect::RemoveRejectedDownload { .. })
                | (Self::Import, Effect::ImportVerifiedDownload { .. })
        )
    }
    fn read_matches(self, e: &Effect) -> bool {
        matches!(
            (self, e),
            (Self::Guard, Effect::ReconcileUpgradeGuard { .. })
                | (Self::Delete, Effect::ReconcileDelete { .. })
                | (Self::Import, Effect::ReconcileImport { .. })
        )
    }
}
#[derive(Clone)]
struct FaultClient {
    operation: Operation,
    writes: Arc<Mutex<Vec<Effect>>>,
    reads: Arc<Mutex<Vec<Effect>>>,
}
impl Upstream for FaultClient {
    fn write<'a>(&'a self, intent: &'a Intent) -> CallFuture<'a> {
        assert!(
            self.operation.write_matches(&intent.effect),
            "unexpected mutation {:?}",
            intent.effect
        );
        self.writes.lock().unwrap().push(intent.effect.clone());
        Box::pin(async { Err(CallError::ServerError) })
    }
    fn read<'a>(&'a self, intent: &'a Intent) -> CallFuture<'a> {
        assert!(
            self.operation.read_matches(&intent.effect),
            "unexpected read {:?}",
            intent.effect
        );
        self.reads.lock().unwrap().push(intent.effect.clone());
        Box::pin(async move { Ok(Outcome::Observed(self.operation.reconciliation())) })
    }
}
async fn apply(store: &Store, lease: &Lease, event: Event) {
    let current = store.load_job(1).await.unwrap();
    store
        .apply_event(
            lease,
            current.revision,
            &format!("event:{}", current.revision),
            event,
            1,
        )
        .await
        .unwrap();
}
async fn observed(store: &Store, lease: &Lease, observation: Observation) {
    let key = store
        .load_job(1)
        .await
        .unwrap()
        .job
        .expected_observation()
        .unwrap();
    apply(
        store,
        lease,
        Event::Observed {
            intent: key,
            observation,
        },
    )
    .await;
}
async fn selected(store: &Store) -> Lease {
    selected_until(store, 10_000).await
}
async fn selected_until(store: &Store, expires_at: u64) -> Lease {
    store
        .create_job(
            &NewJob {
                id: 1,
                viewer: "viewer-opaque".into(),
                resource: "media-opaque".into(),
                library: 3,
                original: ORIGINAL,
                constraints: Constraints::default(),
                policy: Policy {
                    max_grabs: 3,
                    stage_timeout: 1000,
                    total_timeout: 10_000,
                },
                timeouts: StageTimeouts {
                    download: 1000,
                    import: 1000,
                    decode: 1000,
                    verify_retry: 1000,
                },
                created_at: 0,
            },
            "create",
            BudgetLimits {
                global_hour: 10,
                global_day: 20,
                viewer_hour: 5,
                viewer_day: 10,
            },
        )
        .await
        .unwrap();
    let lease = store.acquire_lease(1, "worker", 0, 5000).await.unwrap();
    apply(
        store,
        &lease,
        Event::SelectRelease {
            release: 8,
            access: Access::Granted {
                library: 3,
                expires_at,
            },
        },
    )
    .await;
    lease
}
async fn prepare(store: &Store, operation: Operation) -> Lease {
    let lease = selected(store).await;
    observed(store, &lease, Observation::GrabAccepted { download: 51 }).await;
    match operation {
        Operation::Guard => {
            apply(
                store,
                &lease,
                Event::DownloadFailed {
                    grab: IntentKey {
                        job: 1,
                        sequence: 1,
                    },
                    download: 51,
                },
            )
            .await;
            apply(store, &lease, Event::BrokenFileConfirmed { file: ORIGINAL }).await;
            apply(
                store,
                &lease,
                Event::RequestLowerQuality {
                    candidate: ReleaseCandidate {
                        release: 41,
                        identity: media::ReleaseIdentity::default(),
                        quality: QualityRelation::Lower,
                    },
                    access: access(),
                },
            )
            .await;
            apply(
                store,
                &lease,
                Event::ApproveLowerQuality {
                    approval: AdminApproval {
                        job: 1,
                        release: 41,
                        broken_file: ORIGINAL,
                        expires_at: 1000,
                    },
                    access: access(),
                },
            )
            .await;
        }
        Operation::Delete | Operation::Import => {
            observed(
                store,
                &lease,
                Observation::DownloadCompleted {
                    download: 51,
                    file: SOURCE,
                },
            )
            .await;
            observed(
                store,
                &lease,
                Observation::DownloadDecoded {
                    file: SOURCE,
                    good: matches!(operation, Operation::Import),
                },
            )
            .await;
            let event = match operation {
                Operation::Delete => Event::RejectDownload { access: access() },
                _ => Event::RequestImport { access: access() },
            };
            apply(store, &lease, event).await;
        }
    }
    assert_eq!(
        store.load_job(1).await.unwrap().job.state(),
        operation.initial()
    );
    lease
}
#[tokio::test]
async fn every_non_grab_mutation_reconciles_faults_and_recovered_started_rows() {
    for operation in [Operation::Guard, Operation::Delete, Operation::Import] {
        for recovered in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("journal.sqlite");
            let store = Store::open(&path).await.unwrap();
            let lease = prepare(&store, operation).await;
            let initial = store.load_job(1).await.unwrap();
            let intent = initial.job.expected_observation().unwrap();
            if recovered {
                assert!(
                    store
                        .start_intent(&lease, intent, initial.revision, 2)
                        .await
                        .unwrap()
                );
            }
            let fake = FaultClient {
                operation,
                writes: Arc::new(Mutex::new(vec![])),
                reads: Arc::new(Mutex::new(vec![])),
            };
            let worker = Worker::new(Some(store.clone()), fake.clone(), live()).unwrap();
            assert_eq!(worker.drive(&lease, 3).await.unwrap(), 1);
            assert_eq!(
                store.load_job(1).await.unwrap().job.state(),
                operation.uncertain()
            );
            assert_eq!(fake.writes.lock().unwrap().len(), usize::from(!recovered));
            let reopened = Store::open(&path).await.unwrap();
            let worker = Worker::new(Some(reopened.clone()), fake.clone(), live()).unwrap();
            assert_eq!(worker.drive(&lease, 4).await.unwrap(), 1);
            assert_eq!(
                reopened.load_job(1).await.unwrap().job.state(),
                operation.resolved()
            );
            assert_eq!(fake.writes.lock().unwrap().len(), usize::from(!recovered));
            assert_eq!(fake.reads.lock().unwrap().len(), 1);
            let rows = reopened.outbox(1).await.unwrap();
            assert_eq!(
                rows.iter()
                    .find(|row| row.intent.sequence == intent.sequence)
                    .unwrap()
                    .status,
                OutboxStatus::Completed
            );
            assert_eq!(reopened.load_job(1).await.unwrap().job.reserved_budget(), 1);
        }
    }
}

struct ForbidCalls;
impl Upstream for ForbidCalls {
    fn read<'a>(&'a self, intent: &'a Intent) -> CallFuture<'a> {
        panic!("stale read dispatched: {:?}", intent.effect)
    }
    fn write<'a>(&'a self, intent: &'a Intent) -> CallFuture<'a> {
        panic!("stale write dispatched: {:?}", intent.effect)
    }
}
async fn assert_stale_suppressed(store: &Store, lease: &Lease) {
    let key = store
        .load_job(1)
        .await
        .unwrap()
        .job
        .expected_observation()
        .unwrap();
    let worker = Worker::new(Some(store.clone()), ForbidCalls, live()).unwrap();
    assert_eq!(worker.drive(lease, 3).await.unwrap(), 0);
    assert_eq!(
        store
            .outbox(1)
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.intent.sequence == key.sequence)
            .unwrap()
            .status,
        OutboxStatus::Suppressed
    );
}
#[tokio::test]
async fn externally_observed_mutations_and_reconciliation_suppress_obsolete_intents() {
    for operation in [Operation::Guard, Operation::Delete, Operation::Import] {
        for reconciliation in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let store = Store::open(temp.path().join("journal.sqlite"))
                .await
                .unwrap();
            let lease = prepare(&store, operation).await;
            if reconciliation {
                let ambiguous = match operation {
                    Operation::Guard => Observation::UpgradeGuardAmbiguous,
                    Operation::Delete => Observation::DeleteAmbiguous,
                    Operation::Import => Observation::ImportAmbiguous,
                };
                observed(&store, &lease, ambiguous).await;
                let confirmed = match operation {
                    Operation::Import => Observation::ImportReconciled(ImportResolution::Absent),
                    _ => operation.reconciliation(),
                };
                observed(&store, &lease, confirmed).await;
            } else {
                let confirmed = match operation {
                    Operation::Guard => Observation::UpgradeGuardInstalled,
                    Operation::Delete => Observation::DeleteAcknowledged,
                    Operation::Import => Observation::ImportAccepted,
                };
                observed(&store, &lease, confirmed).await;
            }
            assert_stale_suppressed(&store, &lease).await;
        }
    }
}
#[tokio::test]
async fn externally_observed_reads_suppress_stale_poll_decode_and_target_checks() {
    for step in 0..4 {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("journal.sqlite"))
            .await
            .unwrap();
        let lease = selected(&store).await;
        if step == 0 {
            observed(&store, &lease, Observation::GrabAmbiguous).await;
            observed(
                &store,
                &lease,
                Observation::GrabReconciled(GrabResolution::Accepted { download: 51 }),
            )
            .await;
        } else {
            observed(&store, &lease, Observation::GrabAccepted { download: 51 }).await;
            if step == 1 {
                observed(
                    &store,
                    &lease,
                    Observation::DownloadCompleted {
                        download: 51,
                        file: SOURCE,
                    },
                )
                .await;
                observed(
                    &store,
                    &lease,
                    Observation::DownloadDecoded {
                        file: SOURCE,
                        good: true,
                    },
                )
                .await;
            } else {
                apply(
                    &store,
                    &lease,
                    Event::AutoImported {
                        grab: IntentKey {
                            job: 1,
                            sequence: 1,
                        },
                        download: 51,
                        file: IMPORTED,
                    },
                )
                .await;
                observed(
                    &store,
                    &lease,
                    Observation::ImportedDecoded {
                        file: IMPORTED,
                        good: step == 3,
                    },
                )
                .await;
                if step == 3 {
                    observed(
                        &store,
                        &lease,
                        Observation::TargetChecked {
                            file: IMPORTED,
                            plex_file: IMPORTED,
                            constraints_satisfied: true,
                        },
                    )
                    .await;
                }
            }
        }
        assert_stale_suppressed(&store, &lease).await;
    }
}

#[tokio::test]
async fn terminal_job_suppresses_pending_mutation_and_converts_started_to_uncertain() {
    for started in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("journal.sqlite"))
            .await
            .unwrap();
        let lease = selected(&store).await;
        let current = store.load_job(1).await.unwrap();
        if started {
            assert!(
                store
                    .start_intent(
                        &lease,
                        IntentKey {
                            job: 1,
                            sequence: 1
                        },
                        current.revision,
                        1
                    )
                    .await
                    .unwrap()
            );
        }
        apply(&store, &lease, Event::Fail(Failure::Cancelled)).await;
        let revision = store.load_job(1).await.unwrap().revision;
        for sequence in [2, 3] {
            let key = IntentKey { job: 1, sequence };
            assert!(store.start_intent(&lease, key, revision, 1).await.unwrap());
            store
                .finish_intent(&lease, key, OutboxStatus::Completed, 1)
                .await
                .unwrap();
        }
        let worker = Worker::new(Some(store.clone()), ForbidCalls, live()).unwrap();
        assert_eq!(worker.drive(&lease, 2).await.unwrap(), 0);
        let rows = store.outbox(1).await.unwrap();
        assert_eq!(
            rows[0].status,
            if started {
                OutboxStatus::Uncertain
            } else {
                OutboxStatus::Suppressed
            }
        );
        assert!(
            rows[1..]
                .iter()
                .all(|row| row.status == OutboxStatus::Completed)
        );
    }
}

struct ReadRecovery;
impl Upstream for ReadRecovery {
    fn read<'a>(&'a self, intent: &'a Intent) -> CallFuture<'a> {
        assert!(matches!(intent.effect, Effect::ReconcileGrab { .. }));
        Box::pin(async {
            Ok(Outcome::Observed(Observation::GrabReconciled(
                GrabResolution::Accepted { download: 51 },
            )))
        })
    }
    fn write<'a>(&'a self, _: &'a Intent) -> CallFuture<'a> {
        panic!("expired access must never cause a new write")
    }
}
#[tokio::test]
async fn expired_access_still_allows_read_only_reconciliation_of_prior_mutation() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("journal.sqlite"))
        .await
        .unwrap();
    let lease = selected_until(&store, 2).await;
    let current = store.load_job(1).await.unwrap();
    assert!(
        store
            .start_intent(
                &lease,
                IntentKey {
                    job: 1,
                    sequence: 1
                },
                current.revision,
                1
            )
            .await
            .unwrap()
    );
    let worker = Worker::new(Some(store.clone()), ReadRecovery, live()).unwrap();
    assert_eq!(worker.drive(&lease, 2).await.unwrap(), 1);
    assert_eq!(
        store.load_job(1).await.unwrap().job.state(),
        State::GrabReconciling
    );
    assert_eq!(worker.drive(&lease, 3).await.unwrap(), 1);
    assert_eq!(
        store.load_job(1).await.unwrap().job.state(),
        State::Downloading
    );
}
