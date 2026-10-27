use pitcrew_domain::*;
use proptest::prelude::*;

fn observation(observation: Observation) -> Event {
    Event::Observed {
        intent: IntentKey {
            job: 0,
            sequence: 0,
        },
        observation,
    }
}

// Most lifecycle tests are about transition semantics. Bind synthetic observations
// to the currently dispatched work; stale-observation tests retain explicit keys.
fn bind(job: &FixJob, event: Event) -> Event {
    match event {
        Event::SelectRelease {
            mut release,
            access,
        } => {
            while job
                .excluded_releases()
                .iter()
                .any(|dead| dead.release == release)
            {
                release += 1;
            }
            Event::SelectRelease { release, access }
        }
        Event::Observed {
            intent: IntentKey {
                job: 0,
                sequence: 0,
            },
            observation,
        } => Event::Observed {
            intent: job.expected_observation().unwrap_or(IntentKey {
                job: 0,
                sequence: 0,
            }),
            observation,
        },
        event => event,
    }
}

trait ApplyTest {
    fn apply_test(&self, event: Event, now: u64) -> Result<Transition, TransitionError>;
}

impl ApplyTest for FixJob {
    fn apply_test(&self, event: Event, now: u64) -> Result<Transition, TransitionError> {
        self.apply(bind(self, event), now)
    }
}

const ORIGINAL: FileIdentity = FileIdentity {
    id: 10,
    revision: 1,
};
const DOWNLOAD: FileIdentity = FileIdentity {
    id: 20,
    revision: 1,
};
const IMPORTED: FileIdentity = FileIdentity {
    id: 30,
    revision: 1,
};
const OTHER: FileIdentity = FileIdentity {
    id: 30,
    revision: 2,
};
const ACCESS: Access = Access::Granted {
    library: 7,
    expires_at: 1000,
};

fn job() -> FixJob {
    FixJob::new(
        1,
        7,
        ORIGINAL,
        Constraints {
            language: Some("eng".into()),
            require_sdr: true,
            ..Constraints::default()
        },
        Policy {
            max_grabs: 3,
            stage_timeout: 10,
            total_timeout: 100,
        },
        0,
    )
    .unwrap()
}

fn events() -> Vec<Event> {
    vec![
        Event::SelectRelease {
            release: 40,
            access: ACCESS,
        }, // 0
        observation(Observation::GrabAccepted { download: 50 }), // 1
        observation(Observation::GrabAmbiguous),                 // 2
        observation(Observation::GrabReconciled(GrabResolution::Accepted {
            download: 50,
        })), // 3
        observation(Observation::GrabReconciled(GrabResolution::Absent)), // 4
        observation(Observation::GrabReconciled(GrabResolution::Unknown)), // 5
        observation(Observation::DownloadCompleted {
            download: 50,
            file: DOWNLOAD,
        }), // 6
        observation(Observation::DownloadDecoded {
            file: DOWNLOAD,
            good: true,
        }), // 7
        observation(Observation::DownloadDecoded {
            file: DOWNLOAD,
            good: false,
        }), // 8
        Event::RejectDownload { access: ACCESS },                // 9
        observation(Observation::DeleteAcknowledged),            // 10
        observation(Observation::DeleteAmbiguous),               // 11
        observation(Observation::DeleteReconciled(DeleteResolution::Absent)), // 12
        observation(Observation::DeleteReconciled(DeleteResolution::Present)), // 13
        observation(Observation::DeleteReconciled(DeleteResolution::Unknown)), // 14
        Event::RequestImport { access: ACCESS },                 // 15
        observation(Observation::ImportAccepted),                // 16
        observation(Observation::ImportAmbiguous),               // 17
        observation(Observation::ImportReconciled(ImportResolution::Imported {
            download: 50,
            file: IMPORTED,
        })), // 18
        observation(Observation::ImportReconciled(ImportResolution::Absent)), // 19
        observation(Observation::ImportReconciled(ImportResolution::Unknown)), // 20
        observation(Observation::ImportCompleted {
            download: 50,
            file: IMPORTED,
        }), // 21
        observation(Observation::ImportedDecoded {
            file: IMPORTED,
            good: true,
        }), // 22
        observation(Observation::ImportedDecoded {
            file: IMPORTED,
            good: false,
        }), // 23
        observation(Observation::TargetChecked {
            file: IMPORTED,
            plex_file: IMPORTED,
            constraints_satisfied: true,
        }), // 24
        observation(Observation::TargetChecked {
            file: IMPORTED,
            plex_file: IMPORTED,
            constraints_satisfied: false,
        }), // 25
        Event::QueueReady,                                       // 26
        Event::Fail(Failure::Upstream),                          // 27
        Event::Tick,                                             // 28
    ]
}

fn sequence(indices: &[usize]) -> FixJob {
    let mut job = job();
    for index in indices {
        job = job.apply_test(events()[*index].clone(), 1).unwrap().job;
    }
    job
}

fn at(state: State) -> FixJob {
    use State::*;
    if matches!(
        state,
        AwaitingQualityApproval | UpgradeGuardIntent | UpgradeGuardReconciling | QualityApproved
    ) {
        let requested = job()
            .apply(Event::BrokenFileConfirmed { file: ORIGINAL }, 1)
            .unwrap()
            .job
            .apply(
                Event::RequestLowerQuality {
                    candidate: ReleaseCandidate {
                        release: 41,
                        identity: Default::default(),
                        quality: QualityRelation::Lower,
                    },
                    access: ACCESS,
                },
                1,
            )
            .unwrap()
            .job;
        if state == AwaitingQualityApproval {
            return requested;
        }
        let approved = requested
            .apply(
                Event::ApproveLowerQuality {
                    approval: AdminApproval {
                        job: 1,
                        release: 41,
                        broken_file: ORIGINAL,
                        expires_at: 100,
                    },
                    access: ACCESS,
                },
                1,
            )
            .unwrap()
            .job;
        if state == UpgradeGuardIntent {
            return approved;
        }
        return approved
            .apply_test(
                observation(if state == QualityApproved {
                    Observation::UpgradeGuardInstalled
                } else {
                    Observation::UpgradeGuardAmbiguous
                }),
                1,
            )
            .unwrap()
            .job;
    }
    let path: &[usize] = match state {
        AwaitingQualityApproval
        | UpgradeGuardIntent
        | UpgradeGuardReconciling
        | QualityApproved => unreachable!(),
        Picked => &[],
        GrabIntent => &[0],
        GrabReconciling => &[0, 2],
        Downloading => &[0, 1],
        Downloaded => &[0, 1, 6],
        DownloadRejected => &[0, 1, 6, 8],
        DownloadVerified => &[0, 1, 6, 7],
        RejectIntent => &[0, 1, 6, 8, 9],
        RejectReconciling => &[0, 1, 6, 8, 9, 11],
        RejectRetryable => &[0, 1, 6, 8, 9, 11, 13],
        Rejected => &[0, 1, 6, 8, 9, 10],
        ImportIntent => &[0, 1, 6, 7, 15],
        ImportReconciling => &[0, 1, 6, 7, 15, 17],
        Importing => &[0, 1, 6, 7, 15, 16],
        Imported => &[0, 1, 6, 7, 15, 16, 21, 22],
        VerifyRetry => &[0, 1, 6, 7, 15, 16, 21, 23],
        Verified => &[0, 1, 6, 7, 15, 16, 21, 22, 24],
        Done => &[0, 1, 6, 7, 15, 16, 21, 22, 24, 26],
        Failed => &[27],
    };
    let job = sequence(path);
    assert_eq!(job.state(), state);
    job
}

/// Every State × Event discriminant, including every reconciliation outcome.
/// Expected transitions are a declarative specification separate from the reducer.
#[test]
fn exhaustive_transition_table() {
    use State::*;
    let rows: &[(State, &[(usize, State)])] = &[
        (AwaitingQualityApproval, &[]),
        (UpgradeGuardIntent, &[]),
        (UpgradeGuardReconciling, &[]),
        (QualityApproved, &[]),
        (Picked, &[(0, GrabIntent)]),
        (GrabIntent, &[(1, Downloading), (2, GrabReconciling)]),
        (
            GrabReconciling,
            &[(3, Downloading), (4, Rejected), (5, GrabReconciling)],
        ),
        (Downloading, &[(6, Downloaded)]),
        (Downloaded, &[(7, DownloadVerified), (8, DownloadRejected)]),
        (DownloadRejected, &[(9, RejectIntent)]),
        (DownloadVerified, &[(15, ImportIntent)]),
        (RejectIntent, &[(10, Rejected), (11, RejectReconciling)]),
        (
            RejectReconciling,
            &[
                (12, Rejected),
                (13, RejectRetryable),
                (14, RejectReconciling),
            ],
        ),
        (RejectRetryable, &[(9, RejectIntent)]),
        (Rejected, &[(0, GrabIntent)]),
        (ImportIntent, &[(16, Importing), (17, ImportReconciling)]),
        (
            ImportReconciling,
            &[
                (18, Imported),
                (19, DownloadVerified),
                (20, ImportReconciling),
            ],
        ),
        (Importing, &[(21, Imported)]),
        (
            Imported,
            &[
                (22, Imported),
                (23, VerifyRetry),
                (24, Verified),
                (25, Failed),
            ],
        ),
        (VerifyRetry, &[]),
        (Verified, &[(26, Done)]),
        (Done, &[]),
        (Failed, &[]),
    ];
    for (state, allowed) in rows {
        let before = at(*state);
        for (index, event) in events().into_iter().enumerate() {
            let expected = if matches!(state, Done | Failed) {
                None
            } else if index == 27 {
                Some(Failed)
            } else if index == 28 {
                Some(*state)
            } else {
                allowed
                    .iter()
                    .find_map(|(i, s)| (*i == index).then_some(*s))
            };
            let result = before.apply_test(event.clone(), 1);
            match expected {
                Some(state) => {
                    let transition = result.unwrap_or_else(|e| panic!("{state:?} {event:?}: {e}"));
                    assert_eq!(transition.job.state(), state, "{event:?}");
                    assert_eq!(transition.job.constraints(), before.constraints());
                    let failures = transition
                        .intents
                        .iter()
                        .filter(|i| matches!(i.effect, Effect::NotifyAdmin { .. }))
                        .count();
                    assert_eq!(failures, usize::from(state == Failed));
                }
                None => {
                    let expected =
                        if matches!(state, State::Picked | State::AwaitingQualityApproval)
                            && matches!(event, Event::Observed { .. })
                        {
                            TransitionError::StaleObservation
                        } else {
                            TransitionError::InvalidTransition(*state)
                        };
                    assert_eq!(result, Err(expected), "{event:?}");
                }
            }
        }
    }
}

#[test]
fn intents_are_exact_and_committed_state_precedes_every_effect() {
    let mut current = job();
    let path = [0, 2, 3, 6, 7, 15, 17, 18, 22, 24, 26];
    let expected = [
        vec![Effect::Grab {
            release: 40,
            reservation: 1,
        }],
        vec![Effect::ReconcileGrab { release: 40 }],
        vec![],
        vec![Effect::DecodeDownload {
            download: 50,
            file: DOWNLOAD,
        }],
        vec![],
        vec![Effect::ImportVerifiedDownload {
            download: 50,
            file: DOWNLOAD,
            original: ORIGINAL,
        }],
        vec![Effect::ReconcileImport { download: 50 }],
        vec![Effect::DecodeImported { file: IMPORTED }],
        vec![Effect::CheckTarget {
            file: IMPORTED,
            constraints: current.constraints().clone(),
        }],
        vec![],
        vec![Effect::NotifyReady { file: IMPORTED }],
    ];
    let mut sequence = 0;
    for (index, effects) in path.into_iter().zip(expected) {
        let before = current.clone();
        let transition = current.apply_test(events()[index].clone(), 1).unwrap();
        assert_eq!(current, before, "proposing never changes the committed job");
        assert_eq!(transition.intents.len(), effects.len());
        for (intent, expected) in transition.intents.iter().zip(effects) {
            sequence += 1;
            assert_eq!(
                *intent,
                Intent {
                    job: 1,
                    sequence,
                    effect: expected
                }
            );
        }
        current = transition.job;
    }
    assert_eq!(current.active_grabs(), 0);
    assert_eq!(current.reserved_budget(), 1);
    assert_eq!(current.unresolved_mutation(), None);
    assert_eq!(current.failure(), None);
    assert!(current.is_terminal());
    assert_eq!(current.deadline(), None);
}

#[test]
fn lost_delete_requires_reconciliation_and_new_grab_spends_another_reservation() {
    let bad = at(State::DownloadRejected);
    let remove = bad
        .apply_test(Event::RejectDownload { access: ACCESS }, 1)
        .unwrap();
    assert_eq!(
        remove.intents[0].effect,
        Effect::RemoveRejectedDownload {
            download: 50,
            blocklist: true,
            skip_redownload: true
        }
    );
    assert_eq!(remove.job.unresolved_mutation(), Some(Mutation::Delete));
    let uncertain = remove
        .job
        .apply_test(observation(Observation::DeleteAmbiguous), 1)
        .unwrap();
    assert_eq!(
        uncertain.intents[0].effect,
        Effect::ReconcileDelete { download: 50 }
    );
    assert_eq!(uncertain.job.active_grabs(), 1);
    assert!(uncertain.job.apply_test(events()[0].clone(), 1).is_err());
    let present = uncertain
        .job
        .apply_test(
            observation(Observation::DeleteReconciled(DeleteResolution::Present)),
            1,
        )
        .unwrap()
        .job;
    assert_eq!(present.active_grabs(), 1);
    assert_eq!(present.unresolved_mutation(), None);
    let retried = present
        .apply_test(Event::RejectDownload { access: ACCESS }, 1)
        .unwrap();
    assert_eq!(retried.intents[0].effect, remove.intents[0].effect);
    assert_eq!(retried.job.reserved_budget(), 1);
    let gone = uncertain
        .job
        .apply_test(
            observation(Observation::DeleteReconciled(DeleteResolution::Absent)),
            1,
        )
        .unwrap()
        .job;
    assert_eq!(gone.active_grabs(), 0);
    assert_eq!(gone.unresolved_mutation(), None);
    let next = gone.apply_test(events()[0].clone(), 1).unwrap();
    assert_eq!(next.job.reserved_budget(), 2);
    assert_eq!(
        next.intents[0].effect,
        Effect::Grab {
            release: 41,
            reservation: 2
        }
    );
    assert_eq!(next.job.constraints(), bad.constraints());
}

#[test]
fn lost_post_never_reposts_until_authoritatively_absent_and_never_refunds_budget() {
    let uncertain = at(State::GrabReconciling);
    assert_eq!(uncertain.unresolved_mutation(), Some(Mutation::Grab));
    assert_eq!(uncertain.active_grabs(), 1);
    let unknown = uncertain
        .apply_test(
            observation(Observation::GrabReconciled(GrabResolution::Unknown)),
            2,
        )
        .unwrap();
    assert!(unknown.intents.is_empty());
    assert_eq!(unknown.job.deadline(), uncertain.deadline());
    assert_eq!(unknown.job.unresolved_mutation(), Some(Mutation::Grab));
    let found = unknown
        .job
        .apply_test(
            observation(Observation::GrabReconciled(GrabResolution::Accepted {
                download: 51,
            })),
            3,
        )
        .unwrap()
        .job;
    assert_eq!(found.reserved_budget(), 1);
    assert_eq!(found.active_grabs(), 1);
    assert_eq!(found.unresolved_mutation(), None);
    assert!(
        found
            .apply_test(
                observation(Observation::DownloadCompleted {
                    download: 50,
                    file: DOWNLOAD
                }),
                3
            )
            .is_err()
    );
    assert!(
        found
            .apply_test(
                observation(Observation::DownloadCompleted {
                    download: 51,
                    file: DOWNLOAD
                }),
                3
            )
            .is_ok()
    );
    let absent = uncertain
        .apply_test(
            observation(Observation::GrabReconciled(GrabResolution::Absent)),
            2,
        )
        .unwrap()
        .job;
    assert_eq!(absent.active_grabs(), 0);
    let retry = absent.apply_test(events()[0].clone(), 3).unwrap();
    assert_eq!(retry.job.reserved_budget(), 2);
}

#[test]
fn import_reconciliation_preserves_verified_candidate_without_spending_grab_budget() {
    let uncertain = at(State::ImportReconciling);
    assert_eq!(uncertain.unresolved_mutation(), Some(Mutation::Import));
    let absent = uncertain
        .apply_test(
            observation(Observation::ImportReconciled(ImportResolution::Absent)),
            2,
        )
        .unwrap()
        .job;
    assert_eq!(absent.unresolved_mutation(), None);
    assert_eq!(absent.active_grabs(), 1);
    let retry = absent
        .apply_test(Event::RequestImport { access: ACCESS }, 3)
        .unwrap();
    assert_eq!(retry.job.reserved_budget(), 1);
    assert_eq!(
        retry.intents[0].effect,
        Effect::ImportVerifiedDownload {
            download: 50,
            file: DOWNLOAD,
            original: ORIGINAL
        }
    );
}

#[test]
fn unknown_denied_wrong_library_and_expired_access_never_authorize_mutation() {
    for (state, index) in [
        (State::Picked, 0),
        (State::Rejected, 0),
        (State::DownloadRejected, 9),
        (State::RejectRetryable, 9),
        (State::DownloadVerified, 15),
    ] {
        for access in [
            Access::Unknown,
            Access::Denied,
            Access::Granted {
                library: 8,
                expires_at: 1000,
            },
            Access::Granted {
                library: 7,
                expires_at: 1,
            },
            Access::Granted {
                library: 7,
                expires_at: 0,
            },
        ] {
            let event = match index {
                0 => Event::SelectRelease {
                    release: 40,
                    access,
                },
                9 => Event::RejectDownload { access },
                _ => Event::RequestImport { access },
            };
            assert_eq!(
                at(state).apply_test(event, 1),
                Err(TransitionError::Unauthorized)
            );
        }
    }
}

#[test]
fn exact_file_decode_and_import_history_are_required_for_ready() {
    let undecoded = sequence(&[0, 1, 6, 7, 15, 16, 21]);
    assert_eq!(
        undecoded.apply_test(events()[24].clone(), 1),
        Err(TransitionError::IdentityMismatch)
    );
    let cases = [
        (
            State::Downloading,
            observation(Observation::DownloadCompleted {
                download: 51,
                file: DOWNLOAD,
            }),
        ),
        (
            State::Downloaded,
            observation(Observation::DownloadDecoded {
                file: OTHER,
                good: true,
            }),
        ),
        (
            State::Importing,
            observation(Observation::ImportCompleted {
                download: 51,
                file: IMPORTED,
            }),
        ),
        (
            State::Importing,
            observation(Observation::ImportCompleted {
                download: 50,
                file: ORIGINAL,
            }),
        ),
        (
            State::ImportReconciling,
            observation(Observation::ImportReconciled(ImportResolution::Imported {
                download: 51,
                file: IMPORTED,
            })),
        ),
        (
            State::ImportReconciling,
            observation(Observation::ImportReconciled(ImportResolution::Imported {
                download: 50,
                file: ORIGINAL,
            })),
        ),
        (
            State::Imported,
            observation(Observation::ImportedDecoded {
                file: OTHER,
                good: true,
            }),
        ),
        (
            State::Imported,
            observation(Observation::TargetChecked {
                file: OTHER,
                plex_file: OTHER,
                constraints_satisfied: true,
            }),
        ),
        (
            State::Imported,
            observation(Observation::TargetChecked {
                file: IMPORTED,
                plex_file: OTHER,
                constraints_satisfied: true,
            }),
        ),
    ];
    for (state, event) in cases {
        assert_eq!(
            at(state).apply_test(event, 1),
            Err(TransitionError::IdentityMismatch)
        );
    }
    let failure = at(State::Imported)
        .apply_test(events()[23].clone(), 1)
        .unwrap();
    assert_eq!(failure.job.state(), State::VerifyRetry);
    assert_eq!(failure.job.failure(), None);
    let failure = at(State::Imported)
        .apply_test(events()[25].clone(), 1)
        .unwrap();
    assert_eq!(failure.job.failure(), Some(Failure::ConstraintsUnsatisfied));
}

#[test]
fn budget_is_reserved_at_boundary_including_retries_and_zero_envelope() {
    let mut current = job();
    for expected in 1..=3 {
        let transition = current.apply_test(events()[0].clone(), 1).unwrap();
        assert_eq!(transition.job.reserved_budget(), expected);
        assert_eq!(
            transition.intents[0].effect,
            Effect::Grab {
                release: 40,
                reservation: expected
            }
        );
        current = transition
            .job
            .apply_test(observation(Observation::GrabAmbiguous), 1)
            .unwrap()
            .job
            .apply_test(
                observation(Observation::GrabReconciled(GrabResolution::Absent)),
                1,
            )
            .unwrap()
            .job;
    }
    let failure = current.apply_test(events()[0].clone(), 1).unwrap();
    assert_eq!(failure.job.failure(), Some(Failure::BudgetExhausted));
    assert_eq!(failure.job.reserved_budget(), 3);
    assert_eq!(failure.job.active_grabs(), 0);
    assert_eq!(
        failure
            .intents
            .iter()
            .map(|i| &i.effect)
            .collect::<Vec<_>>(),
        vec![
            &Effect::NotifyAdmin {
                failure: Failure::BudgetExhausted
            },
            &Effect::NotifyViewerFailure {
                failure: Failure::BudgetExhausted
            }
        ]
    );
    let empty = FixJob::new(
        1,
        7,
        ORIGINAL,
        Constraints::default(),
        Policy {
            max_grabs: 0,
            stage_timeout: 10,
            total_timeout: 100,
        },
        0,
    )
    .unwrap();
    assert_eq!(
        empty
            .apply_test(events()[0].clone(), 1)
            .unwrap()
            .job
            .failure(),
        Some(Failure::BudgetExhausted)
    );
}

#[test]
fn failure_has_one_notification_intent_and_preserves_outstanding_work() {
    for state in [
        State::Picked,
        State::AwaitingQualityApproval,
        State::UpgradeGuardIntent,
        State::UpgradeGuardReconciling,
        State::QualityApproved,
        State::GrabIntent,
        State::GrabReconciling,
        State::Downloading,
        State::Downloaded,
        State::DownloadRejected,
        State::DownloadVerified,
        State::RejectIntent,
        State::RejectReconciling,
        State::RejectRetryable,
        State::Rejected,
        State::ImportIntent,
        State::ImportReconciling,
        State::Importing,
        State::Imported,
        State::VerifyRetry,
        State::Verified,
    ] {
        for reason in [
            Failure::Deadline,
            Failure::BudgetExhausted,
            Failure::Upstream,
            Failure::ImportedDecodeFailed,
            Failure::ConstraintsUnsatisfied,
            Failure::Cancelled,
        ] {
            let before = at(state);
            let transition = before.apply_test(Event::Fail(reason), 1).unwrap();
            assert_eq!(transition.job.state(), State::Failed);
            assert_eq!(transition.job.failure(), Some(reason));
            assert_eq!(
                transition.job.unresolved_mutation(),
                before.unresolved_mutation()
            );
            assert_eq!(transition.job.active_grabs(), before.active_grabs());
            assert_eq!(
                transition.job.expected_observation(),
                before.expected_observation()
            );
            assert_eq!(transition.intents.len(), 2);
            assert_eq!(
                transition.intents[0].effect,
                Effect::NotifyAdmin { failure: reason }
            );
            assert_eq!(
                transition.intents[1].effect,
                Effect::NotifyViewerFailure { failure: reason }
            );
            assert!(transition.job.apply_test(Event::Fail(reason), 2).is_err());
            assert_eq!(transition.job.deadline(), None);
        }
    }
}

#[test]
fn deadlines_are_absolute_bounded_and_unknown_results_cannot_extend_them() {
    let initial = job();
    assert_eq!(initial.deadline(), Some(10));
    assert_eq!(
        initial.apply_test(Event::Tick, 9).unwrap().job.state(),
        State::Picked
    );
    for time in [10, 11, u64::MAX] {
        let expired = initial.apply_test(events()[0].clone(), time).unwrap();
        assert_eq!(expired.job.failure(), Some(Failure::Deadline));
        assert_eq!(expired.job.reserved_budget(), 0);
        assert_eq!(expired.intents.len(), 2);
    }
    let current = initial.apply_test(events()[0].clone(), 9).unwrap().job;
    assert_eq!(current.deadline(), Some(19));
    assert_eq!(
        current.apply_test(Event::Tick, 8),
        Err(TransitionError::ClockWentBackwards)
    );
    assert_eq!(
        current.apply_test(Event::Tick, 9).unwrap().job.deadline(),
        Some(19)
    );
    for (state, event) in [
        (
            State::GrabReconciling,
            observation(Observation::GrabReconciled(GrabResolution::Unknown)),
        ),
        (
            State::RejectReconciling,
            observation(Observation::DeleteReconciled(DeleteResolution::Unknown)),
        ),
        (
            State::ImportReconciling,
            observation(Observation::ImportReconciled(ImportResolution::Unknown)),
        ),
    ] {
        let before = at(state);
        let next = before.apply_test(event, 10).unwrap();
        assert_eq!(next.job.deadline(), Some(11));
        assert_eq!(next.job.unresolved_mutation(), before.unresolved_mutation());
        assert_eq!(
            next.job.apply_test(Event::Tick, 11).unwrap().job.failure(),
            Some(Failure::Deadline)
        );
    }
    let bounded = FixJob::new(
        1,
        7,
        ORIGINAL,
        Constraints::default(),
        Policy {
            max_grabs: 3,
            stage_timeout: 10,
            total_timeout: 12,
        },
        0,
    )
    .unwrap();
    let bounded = bounded.apply_test(events()[0].clone(), 9).unwrap().job;
    assert_eq!(bounded.deadline(), Some(12));
    assert_eq!(
        bounded
            .apply_test(observation(Observation::GrabAccepted { download: 50 }), 11)
            .unwrap()
            .job
            .deadline(),
        Some(12)
    );
    assert_eq!(
        bounded.apply_test(Event::Tick, 12).unwrap().job.failure(),
        Some(Failure::Deadline)
    );
    let shorter = FixJob::new(
        1,
        7,
        ORIGINAL,
        Constraints::default(),
        Policy {
            max_grabs: 3,
            stage_timeout: 20,
            total_timeout: 12,
        },
        0,
    )
    .unwrap();
    assert_eq!(shorter.deadline(), Some(12));
}

#[test]
fn invalid_policy_and_clock_overflow_are_errors() {
    for (stage_timeout, total_timeout, now) in [
        (0, 1, 0),
        (1, 0, 0),
        (2, 1, u64::MAX - 1),
        (1, 2, u64::MAX - 1),
    ] {
        assert_eq!(
            FixJob::new(
                1,
                7,
                ORIGINAL,
                Constraints::default(),
                Policy {
                    max_grabs: 1,
                    stage_timeout,
                    total_timeout
                },
                now
            ),
            Err(TransitionError::InvalidPolicy)
        );
    }
    assert!(
        FixJob::new(
            1,
            7,
            ORIGINAL,
            Constraints::default(),
            Policy {
                max_grabs: 1,
                stage_timeout: 1,
                total_timeout: 1
            },
            u64::MAX - 1
        )
        .is_ok()
    );
}

#[test]
fn restart_replay_reproduces_state_and_outbox_keys_without_dispatching_again() {
    let mut live = job();
    let mut log = Vec::new();
    for index in [0, 2, 3, 6, 7, 15, 17, 18, 22, 24, 26] {
        let event = bind(&live, events()[index].clone());
        let transition = live.apply(event.clone(), 1).unwrap();
        log.push((
            serde_json::to_string(&event).unwrap(),
            transition.intents.clone(),
        ));
        live = transition.job;
        // Restoration replays accepted events but inserts no new outbox rows.
        let mut restored = job();
        for (event, expected_intents) in &log {
            let transition = restored
                .apply_test(serde_json::from_str(event).unwrap(), 1)
                .unwrap();
            assert_eq!(&transition.intents, expected_intents);
            restored = transition.job;
        }
        assert_eq!(live, restored);
        assert_eq!(
            serde_json::to_value(&live).unwrap(),
            serde_json::to_value(&restored).unwrap()
        );
    }
}

fn guided_event(state: State, selector: u8) -> Event {
    let index = match state {
        State::Picked | State::Rejected => 0,
        State::AwaitingQualityApproval
        | State::UpgradeGuardIntent
        | State::UpgradeGuardReconciling
        | State::QualityApproved => 28,
        State::GrabIntent => {
            if selector.is_multiple_of(2) {
                1
            } else {
                2
            }
        }
        State::GrabReconciling => [3, 4, 5][usize::from(selector % 3)],
        State::Downloading => 6,
        State::Downloaded => {
            if selector.is_multiple_of(2) {
                7
            } else {
                8
            }
        }
        State::DownloadRejected | State::RejectRetryable => 9,
        State::DownloadVerified => 15,
        State::RejectIntent => {
            if selector.is_multiple_of(2) {
                10
            } else {
                11
            }
        }
        State::RejectReconciling => [12, 13, 14][usize::from(selector % 3)],
        State::ImportIntent => {
            if selector.is_multiple_of(2) {
                16
            } else {
                17
            }
        }
        State::ImportReconciling => [18, 19, 20][usize::from(selector % 3)],
        State::Importing => 21,
        State::Imported => {
            if selector.is_multiple_of(2) {
                22
            } else {
                24
            }
        }
        State::Verified => 26,
        State::VerifyRetry | State::Done | State::Failed => 28,
    };
    events()[index].clone()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]
    #[test]
    fn arbitrary_event_orders_preserve_all_safety_rails(input in prop::collection::vec((any::<u8>(), any::<u8>()), 1..160)) {
        let mut current = job();
        let mut admin_notifications = 0;
        let mut total_grabs = 0;
        let mut last_intent = 0;
        let mut verified_download = false;
        let mut imported_decode = false;
        let mut target_checked = false;
        for (choice, selector) in input {
            let event = if choice % 4 == 0 { events()[usize::from(selector) % events().len()].clone() }
                else { guided_event(current.state(), selector) };
            let event = if choice % 8 == 0 {
                match event {
                    Event::Observed { observation, .. } => Event::Observed { intent: IntentKey { job: 1, sequence: u64::from(selector) }, observation },
                    event => event,
                }
            } else { bind(&current, event) };
            let now = if choice == 255 { 100 } else { 1 };
            if let Ok(transition) = current.apply(event.clone(), now) {
                for intent in &transition.intents {
                    prop_assert_eq!(intent.job, 1);
                    prop_assert_eq!(intent.sequence, last_intent + 1);
                    last_intent = intent.sequence;
                    match intent.effect {
                        Effect::Grab { reservation, .. } => {
                            prop_assert_eq!(current.active_grabs(), 0);
                            total_grabs += 1;
                            prop_assert_eq!(reservation, total_grabs);
                            prop_assert_eq!(transition.job.reserved_budget(), total_grabs);
                            prop_assert_eq!(transition.job.unresolved_mutation(), Some(Mutation::Grab));
                            prop_assert_eq!(transition.job.state(), State::GrabIntent);
                            verified_download = false;
                            imported_decode = false;
                            target_checked = false;
                        },
                        Effect::ImportVerifiedDownload { file, .. } => {
                            prop_assert!(verified_download);
                            prop_assert_eq!(file, DOWNLOAD);
                            prop_assert_eq!(current.state(), State::DownloadVerified);
                            prop_assert_eq!(transition.job.unresolved_mutation(), Some(Mutation::Import));
                        },
                        Effect::RemoveRejectedDownload { blocklist, skip_redownload, .. } => {
                            prop_assert!(blocklist && skip_redownload);
                            prop_assert_eq!(transition.job.unresolved_mutation(), Some(Mutation::Delete));
                        },
                        Effect::NotifyAdmin { .. } => admin_notifications += 1,
                        Effect::NotifyReady { file } => {
                            prop_assert!(imported_decode && target_checked);
                            prop_assert_eq!(file, IMPORTED);
                            prop_assert_eq!(transition.job.state(), State::Done);
                        },
                        _ => {},
                    }
                }
                if transition.job.state() != State::Failed {
                    match event {
                        Event::Observed { observation: Observation::DownloadDecoded { good: true, .. }, .. } => verified_download = true,
                        Event::Observed { observation: Observation::ImportedDecoded { good: true, .. }, .. } => imported_decode = true,
                        Event::Observed { observation: Observation::TargetChecked { constraints_satisfied: true, .. }, .. } => target_checked = true,
                        _ => {},
                    }
                }
                current = transition.job;
            }
            prop_assert!(current.active_grabs() <= 1);
            prop_assert_eq!(current.reserved_budget(), total_grabs);
            prop_assert!(total_grabs <= 3);
            prop_assert_eq!(admin_notifications, u32::from(current.state() == State::Failed));
        }
    }

    #[test]
    fn arbitrary_access_snapshots_cannot_bypass_authorization(library in any::<u64>(), expires_at in any::<u64>(), now in 0u64..10) {
        let event = Event::SelectRelease { release: 40, access: Access::Granted { library, expires_at } };
        let result = job().apply_test(event, now);
        if library == 7 && now < expires_at { prop_assert!(result.is_ok()); }
        else { prop_assert_eq!(result, Err(TransitionError::Unauthorized)); }
    }
}

#[test]
fn stale_delete_acknowledgment_cannot_release_a_later_attempts_live_slot() {
    let first_deleted = at(State::Rejected);
    let second_file = FileIdentity {
        id: 21,
        revision: 1,
    };
    let second = first_deleted
        .apply_test(
            Event::SelectRelease {
                release: 41,
                access: ACCESS,
            },
            1,
        )
        .unwrap()
        .job
        .apply_test(observation(Observation::GrabAccepted { download: 51 }), 1)
        .unwrap()
        .job
        .apply_test(
            observation(Observation::DownloadCompleted {
                download: 51,
                file: second_file,
            }),
            1,
        )
        .unwrap()
        .job
        .apply_test(
            observation(Observation::DownloadDecoded {
                file: second_file,
                good: false,
            }),
            1,
        )
        .unwrap()
        .job
        .apply_test(Event::RejectDownload { access: ACCESS }, 1)
        .unwrap()
        .job;
    // This delayed acknowledgement belongs to the first removal, not the second.
    assert!(
        second
            .apply(
                Event::Observed {
                    intent: first_deleted.expected_observation().unwrap(),
                    observation: Observation::DeleteAcknowledged
                },
                1
            )
            .is_err()
    );
    assert_eq!(second.active_grabs(), 1);
    assert!(
        second
            .apply_test(
                Event::SelectRelease {
                    release: 42,
                    access: ACCESS
                },
                1
            )
            .is_err()
    );
}

#[test]
fn stale_reconciliation_cannot_authorize_a_new_import_within_the_same_attempt() {
    let first_reconciliation = at(State::ImportReconciling);
    let retry = first_reconciliation
        .apply_test(
            observation(Observation::ImportReconciled(ImportResolution::Absent)),
            1,
        )
        .unwrap()
        .job
        .apply_test(Event::RequestImport { access: ACCESS }, 1)
        .unwrap()
        .job
        .apply_test(observation(Observation::ImportAmbiguous), 1)
        .unwrap()
        .job;
    assert_eq!(retry.state(), State::ImportReconciling);
    assert_eq!(
        retry.apply(
            Event::Observed {
                intent: first_reconciliation.expected_observation().unwrap(),
                observation: Observation::ImportReconciled(ImportResolution::Absent),
            },
            1
        ),
        Err(TransitionError::StaleObservation)
    );
}

#[test]
fn stale_delete_reconciliation_cannot_clear_another_delete_operation() {
    let first = at(State::RejectReconciling);
    let retry = first
        .apply_test(
            observation(Observation::DeleteReconciled(DeleteResolution::Present)),
            1,
        )
        .unwrap()
        .job
        .apply_test(Event::RejectDownload { access: ACCESS }, 1)
        .unwrap()
        .job
        .apply_test(observation(Observation::DeleteAmbiguous), 1)
        .unwrap()
        .job;
    assert_eq!(retry.state(), State::RejectReconciling);
    assert_eq!(
        retry.apply(
            Event::Observed {
                intent: first.expected_observation().unwrap(),
                observation: Observation::DeleteReconciled(DeleteResolution::Absent),
            },
            1
        ),
        Err(TransitionError::StaleObservation)
    );
    assert_eq!(retry.active_grabs(), 1);
}

#[test]
fn reused_download_and_file_identity_does_not_reuse_previous_attempt_evidence() {
    let old_download = at(State::Downloaded);
    let old_grab = at(State::GrabIntent);
    let new_grab = at(State::Rejected)
        .apply_test(events()[0].clone(), 1)
        .unwrap()
        .job;
    assert_eq!(
        new_grab.apply(
            Event::Observed {
                intent: old_grab.expected_observation().unwrap(),
                observation: Observation::GrabAccepted { download: 50 },
            },
            1
        ),
        Err(TransitionError::StaleObservation)
    );
    let new_download = new_grab
        .apply_test(events()[1].clone(), 1)
        .unwrap()
        .job
        .apply_test(events()[6].clone(), 1)
        .unwrap()
        .job;
    assert_eq!(
        new_download.apply(
            Event::Observed {
                intent: old_download.expected_observation().unwrap(),
                observation: Observation::DownloadDecoded {
                    file: DOWNLOAD,
                    good: true
                },
            },
            1
        ),
        Err(TransitionError::StaleObservation)
    );
    assert_eq!(new_download.state(), State::Downloaded);
}

#[test]
fn exact_intent_sequence_survives_acceptance_until_download_or_import_completion() {
    assert_eq!(job().expected_observation(), None);
    let grab = job().apply(events()[0].clone(), 1).unwrap();
    assert_eq!(
        grab.job.expected_observation(),
        Some(IntentKey {
            job: grab.intents[0].job,
            sequence: grab.intents[0].sequence
        })
    );
    let accepted = grab
        .job
        .apply(
            Event::Observed {
                intent: IntentKey {
                    job: grab.intents[0].job,
                    sequence: grab.intents[0].sequence,
                },
                observation: Observation::GrabAccepted { download: 50 },
            },
            1,
        )
        .unwrap();
    assert_eq!(
        accepted.job.expected_observation(),
        Some(IntentKey {
            job: 1,
            sequence: 1
        })
    );
    let complete = accepted
        .job
        .apply(
            Event::Observed {
                intent: IntentKey {
                    job: 1,
                    sequence: 1,
                },
                observation: Observation::DownloadCompleted {
                    download: 50,
                    file: DOWNLOAD,
                },
            },
            1,
        )
        .unwrap();
    assert_eq!(
        complete.job.expected_observation(),
        Some(IntentKey {
            job: 1,
            sequence: 2
        })
    );
    assert_eq!(
        complete.job.apply(
            Event::Observed {
                intent: IntentKey {
                    job: 1,
                    sequence: 1
                },
                observation: Observation::DownloadDecoded {
                    file: DOWNLOAD,
                    good: true
                }
            },
            1
        ),
        Err(TransitionError::StaleObservation)
    );
    let imported = at(State::Imported);
    let expected = imported.expected_observation().unwrap();
    for sequence in [0, expected.sequence - 1, expected.sequence + 1, u64::MAX] {
        let intent = IntentKey {
            job: expected.job,
            sequence,
        };
        assert_eq!(
            imported.apply(
                Event::Observed {
                    intent,
                    observation: Observation::TargetChecked {
                        file: IMPORTED,
                        plex_file: IMPORTED,
                        constraints_satisfied: true
                    }
                },
                1
            ),
            Err(TransitionError::StaleObservation)
        );
    }
    assert!(
        imported
            .apply(
                Event::Observed {
                    intent: expected,
                    observation: Observation::TargetChecked {
                        file: IMPORTED,
                        plex_file: IMPORTED,
                        constraints_satisfied: true
                    }
                },
                1
            )
            .is_ok()
    );
}

#[test]
fn identical_sequence_from_another_job_cannot_supply_evidence() {
    let current = at(State::GrabIntent);
    let other = FixJob::new(
        2,
        7,
        ORIGINAL,
        Constraints::default(),
        Policy {
            max_grabs: 3,
            stage_timeout: 10,
            total_timeout: 100,
        },
        0,
    )
    .unwrap()
    .apply(events()[0].clone(), 1)
    .unwrap();
    assert_eq!(
        current.apply(
            Event::Observed {
                intent: IntentKey {
                    job: other.intents[0].job,
                    sequence: other.intents[0].sequence
                },
                observation: Observation::GrabAccepted { download: 50 },
            },
            1
        ),
        Err(TransitionError::StaleObservation)
    );
}

/// P0.5 discriminants against every lifecycle state. Preconditions requiring
/// broken-file evidence are exercised separately in the approval-path tests.
#[test]
fn p05_transition_table_covers_new_events_in_all_states() {
    use State::*;
    let candidate = ReleaseCandidate {
        release: 41,
        identity: Default::default(),
        quality: QualityRelation::SameOrBetter,
    };
    let extra = [
        Event::SelectCandidate {
            candidate: candidate.clone(),
            access: ACCESS,
        },
        Event::BrokenFileConfirmed { file: ORIGINAL },
        Event::RequestLowerQuality {
            candidate: ReleaseCandidate {
                quality: QualityRelation::Lower,
                ..candidate
            },
            access: ACCESS,
        },
        Event::ApproveLowerQuality {
            approval: AdminApproval {
                job: 1,
                release: 41,
                broken_file: ORIGINAL,
                expires_at: 100,
            },
            access: ACCESS,
        },
        Event::GrabApproved { access: ACCESS },
        Event::DownloadFailed {
            grab: IntentKey {
                job: 1,
                sequence: 1,
            },
            download: 50,
        },
        Event::AutoImported {
            grab: IntentKey {
                job: 1,
                sequence: 1,
            },
            download: 50,
            file: IMPORTED,
        },
        observation(Observation::UpgradeGuardInstalled),
        observation(Observation::UpgradeGuardAmbiguous),
        observation(Observation::UpgradeGuardReconciled(
            UpgradeGuardResolution::Installed,
        )),
        observation(Observation::UpgradeGuardReconciled(
            UpgradeGuardResolution::Absent,
        )),
        observation(Observation::UpgradeGuardReconciled(
            UpgradeGuardResolution::Unknown,
        )),
    ];
    let rows: &[(State, &[(usize, State)])] = &[
        (Picked, &[(0, GrabIntent), (1, Picked)]),
        (AwaitingQualityApproval, &[(3, UpgradeGuardIntent)]),
        (
            UpgradeGuardIntent,
            &[(7, QualityApproved), (8, UpgradeGuardReconciling)],
        ),
        (
            UpgradeGuardReconciling,
            &[
                (9, QualityApproved),
                (10, AwaitingQualityApproval),
                (11, UpgradeGuardReconciling),
            ],
        ),
        (QualityApproved, &[(4, GrabIntent)]),
        (GrabIntent, &[(5, Rejected), (6, Imported)]),
        (GrabReconciling, &[(5, Rejected), (6, Imported)]),
        (Downloading, &[(5, Rejected), (6, Imported)]),
        (Downloaded, &[(5, Rejected), (6, Imported)]),
        (DownloadRejected, &[(5, Rejected), (6, Imported)]),
        (DownloadVerified, &[(5, Rejected), (6, Imported)]),
        (RejectIntent, &[]),
        (RejectReconciling, &[]),
        (RejectRetryable, &[]),
        (Rejected, &[(0, GrabIntent), (1, Rejected)]),
        (ImportIntent, &[]),
        (ImportReconciling, &[]),
        (Importing, &[]),
        (Imported, &[]),
        (VerifyRetry, &[]),
        (Verified, &[]),
        (Done, &[]),
        (Failed, &[]),
    ];
    for (state, allowed) in rows {
        let current = at(*state);
        for (index, event) in extra.iter().enumerate() {
            let result = current.apply_test(event.clone(), 1);
            if let Some(expected) = allowed
                .iter()
                .find_map(|(i, s)| (*i == index).then_some(*s))
            {
                let transition = result.unwrap_or_else(|e| panic!("{state:?} {event:?}: {e}"));
                assert_eq!(transition.job.state(), expected);
                assert_eq!(transition.job.constraints(), current.constraints());
            } else {
                assert!(result.is_err(), "{state:?} unexpectedly accepted {event:?}");
            }
        }
    }
}

#[test]
fn mutation_authorization_is_replayed_and_expires_at_the_exact_boundary() {
    let job = pitcrew_domain::FixJob::new(
        1,
        2,
        pitcrew_domain::FileIdentity { id: 3, revision: 1 },
        pitcrew_domain::Constraints::default(),
        pitcrew_domain::Policy {
            max_grabs: 1,
            stage_timeout: 30,
            total_timeout: 100,
        },
        0,
    )
    .unwrap();
    assert!(!job.mutation_authorized(0));
    let next = job
        .apply(
            pitcrew_domain::Event::SelectRelease {
                release: 4,
                access: pitcrew_domain::Access::Granted {
                    library: 2,
                    expires_at: 10,
                },
            },
            1,
        )
        .unwrap()
        .job;
    assert!(next.mutation_authorized(9));
    assert!(!next.mutation_authorized(10));
    assert!(!next.mutation_authorized(11));
    assert!(!job.mutation_authorized(1));
}
