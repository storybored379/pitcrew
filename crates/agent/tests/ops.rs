use pitcrew_agent_core::ops::*;
use pitcrew_arbiter::*;
use pitcrew_domain::{Access, FileIdentity, IntentKey};

fn policy() -> OpsPolicy {
    OpsPolicy {
        enabled: true,
        dry_run: false,
        cooldown_seconds: 900,
        daily_limit: 4,
    }
}
fn bad(now: u64) -> OpsCheck {
    OpsCheck {
        now,
        health: OpsHealth::Unhealthy,
        paused: Some(false),
        active_sessions: Some(0),
    }
}
fn job() -> OpsJob {
    OpsJob::new(41, OpsKind::MountHeal, 0)
}
fn first() -> OpsJob {
    let transition = job().check(policy(), bad(1)).unwrap();
    assert_eq!(
        transition.decision,
        OpsDecision::Hold(OpsHold::FirstFailure)
    );
    transition.job
}
fn intent(job: &OpsJob) -> OpsIntent {
    match job.state() {
        OpsState::Intent(intent) => intent,
        _ => panic!("expected intent"),
    }
}
fn acted() -> OpsJob {
    let transition = first().check(policy(), bad(2)).unwrap();
    assert_eq!(
        transition.decision,
        OpsDecision::PersistIntent(OpsIntent {
            key: IntentKey {
                job: 41,
                sequence: 1
            },
            kind: OpsKind::MountHeal,
            created_at: 2
        })
    );
    assert_eq!(transition.job.reserved_today(), 1);
    transition.job
}

#[test]
fn both_watchdogs_disabled_and_dry_run_by_default() {
    assert_eq!(
        OpsPolicy::for_kind(OpsKind::MountHeal),
        OpsPolicy {
            enabled: false,
            dry_run: true,
            cooldown_seconds: 900,
            daily_limit: 4
        }
    );
    assert_eq!(
        OpsPolicy::for_kind(OpsKind::PublishWatchdog),
        OpsPolicy {
            enabled: false,
            dry_run: true,
            cooldown_seconds: 1800,
            daily_limit: 4
        }
    );
    assert_eq!(
        ConfirmerPolicy::default(),
        ConfirmerPolicy {
            enabled: false,
            dry_run: true
        }
    );
    for kind in [OpsKind::MountHeal, OpsKind::PublishWatchdog] {
        let job = OpsJob::new(41, kind, 0);
        let disabled = job.check(OpsPolicy::for_kind(kind), bad(1)).unwrap();
        assert_eq!(disabled.decision, OpsDecision::Hold(OpsHold::Disabled));
        let one = disabled.job.check(policy(), bad(2)).unwrap();
        assert_eq!(one.decision, OpsDecision::Hold(OpsHold::FirstFailure));
        let dry = one
            .job
            .check(
                OpsPolicy {
                    dry_run: true,
                    ..policy()
                },
                bad(3),
            )
            .unwrap();
        assert_eq!(dry.decision, OpsDecision::DryRun(kind));
        assert_eq!(dry.job.state(), OpsState::Watching);
        assert_eq!(dry.job.reserved_today(), 0);
        let live = dry.job.check(policy(), bad(4)).unwrap();
        let expected = OpsIntent {
            key: IntentKey {
                job: 41,
                sequence: 1,
            },
            kind,
            created_at: 4,
        };
        assert_eq!(live.decision, OpsDecision::PersistIntent(expected));
        assert_eq!(live.job.state(), OpsState::Intent(expected));
    }
}

#[test]
fn pause_health_unknown_and_disabled_break_consecutive_failures() {
    let checks = [
        (
            OpsCheck {
                paused: Some(true),
                ..bad(2)
            },
            OpsHold::Paused,
        ),
        (
            OpsCheck {
                paused: None,
                ..bad(2)
            },
            OpsHold::PauseUnknown,
        ),
        (
            OpsCheck {
                health: OpsHealth::Healthy,
                ..bad(2)
            },
            OpsHold::Healthy,
        ),
        (
            OpsCheck {
                health: OpsHealth::Unknown,
                ..bad(2)
            },
            OpsHold::HealthUnknown,
        ),
    ];
    for (check, hold) in checks {
        let reset = first().check(policy(), check).unwrap();
        assert_eq!(reset.decision, OpsDecision::Hold(hold));
        assert_eq!(
            reset.job.check(policy(), bad(3)).unwrap().decision,
            OpsDecision::Hold(OpsHold::FirstFailure)
        );
    }
    let reset = first()
        .check(
            OpsPolicy {
                enabled: false,
                ..policy()
            },
            bad(2),
        )
        .unwrap();
    assert_eq!(
        reset.job.check(policy(), bad(3)).unwrap().decision,
        OpsDecision::Hold(OpsHold::FirstFailure)
    );
}

#[test]
fn sessions_are_positive_idle_only_and_duplicate_samples_never_count_twice() {
    for (sessions, reason) in [
        (Some(1), OpsHold::SessionsActive),
        (Some(20), OpsHold::SessionsActive),
        (None, OpsHold::SessionsUnknown),
    ] {
        let blocked = first()
            .check(
                policy(),
                OpsCheck {
                    active_sessions: sessions,
                    ..bad(2)
                },
            )
            .unwrap();
        assert_eq!(blocked.decision, OpsDecision::Hold(reason));
        assert_eq!(blocked.job.reserved_today(), 0);
        assert!(matches!(
            blocked.job.check(policy(), bad(3)).unwrap().decision,
            OpsDecision::PersistIntent(_)
        ));
    }
    assert_eq!(
        first().check(policy(), bad(1)).unwrap().decision,
        OpsDecision::Hold(OpsHold::DuplicateSample)
    );
    assert_eq!(
        first().check(policy(), bad(0)),
        Err(OpsError::ClockRegression)
    );
    assert_eq!(
        OpsJob::new(1, OpsKind::MountHeal, 20).check(policy(), bad(19)),
        Err(OpsError::ClockRegression)
    );
    for invalid in [
        OpsPolicy {
            cooldown_seconds: 0,
            ..policy()
        },
        OpsPolicy {
            daily_limit: 0,
            ..policy()
        },
    ] {
        assert_eq!(first().check(invalid, bad(2)), Err(OpsError::InvalidPolicy));
    }
}

#[test]
fn lost_response_reconciles_and_failure_notifies_once_with_same_intent() {
    let job = acted();
    let intent = intent(&job);
    assert_eq!(
        job.check(policy(), bad(3)).unwrap().decision,
        OpsDecision::Hold(OpsHold::Pending)
    );
    let recon = job.resolve(intent.key, OpsHealth::Unknown).unwrap();
    assert_eq!(recon.decision, OpsDecision::Reconcile(intent));
    assert_eq!(recon.job.state(), OpsState::Reconciling(intent));
    assert_eq!(recon.job.reserved_today(), 1);
    assert_eq!(
        recon.job.check(policy(), bad(4)).unwrap().decision,
        OpsDecision::Hold(OpsHold::Pending)
    );
    assert_eq!(
        recon
            .job
            .resolve(intent.key, OpsHealth::Unknown)
            .unwrap()
            .job,
        recon.job
    );
    for key in [
        IntentKey {
            job: 99,
            ..intent.key
        },
        IntentKey {
            sequence: 99,
            ..intent.key
        },
    ] {
        assert_eq!(
            job.resolve(key, OpsHealth::Healthy),
            Err(OpsError::WrongIntent)
        );
    }
    let failure = recon.job.resolve(intent.key, OpsHealth::Unhealthy).unwrap();
    assert_eq!(failure.job.state(), OpsState::NeedsAdmin);
    assert_eq!(failure.decision, OpsDecision::NotifyAdmin(intent));
    assert_eq!(
        failure.job.resolve(intent.key, OpsHealth::Unhealthy),
        Err(OpsError::WrongIntent)
    );
    assert_eq!(
        failure.job.check(policy(), bad(5)).unwrap().decision,
        OpsDecision::Hold(OpsHold::NeedsAdmin)
    );
    assert_eq!(
        job.resolve(intent.key, OpsHealth::Healthy)
            .unwrap()
            .decision,
        OpsDecision::Recovered
    );
    assert_eq!(
        first().resolve(intent.key, OpsHealth::Healthy),
        Err(OpsError::WrongIntent)
    );
}

#[test]
fn reservation_survives_restart_cooldown_boundaries_daily_cap_and_day_change() {
    let job = acted();
    let mut job = job
        .resolve(intent(&job).key, OpsHealth::Healthy)
        .unwrap()
        .job;
    assert_eq!(job.state(), OpsState::Watching);
    assert_eq!(job.reserved_today(), 1);
    job = job.check(policy(), bad(3)).unwrap().job;
    assert_eq!(
        job.check(policy(), bad(901)).unwrap().decision,
        OpsDecision::Hold(OpsHold::Cooldown)
    );
    for now in [902, 1802, 2702] {
        let transition = job.check(policy(), bad(now)).unwrap();
        let next = transition.job;
        let expected = OpsIntent {
            key: IntentKey {
                job: 41,
                sequence: u64::from(next.reserved_today()),
            },
            kind: OpsKind::MountHeal,
            created_at: now,
        };
        assert_eq!(transition.decision, OpsDecision::PersistIntent(expected));
        job = next.resolve(expected.key, OpsHealth::Healthy).unwrap().job;
        job = job.check(policy(), bad(now + 1)).unwrap().job;
    }
    assert_eq!(job.reserved_today(), 4);
    assert_eq!(
        job.check(policy(), bad(3602)).unwrap().decision,
        OpsDecision::Hold(OpsHold::DailyCap)
    );
    assert_eq!(
        job.check(policy(), bad(86399)).unwrap().decision,
        OpsDecision::Hold(OpsHold::DailyCap)
    );
    let next_day = job.check(policy(), bad(86400)).unwrap();
    assert_eq!(next_day.job.reserved_today(), 1);
    assert_eq!(intent(&next_day.job).key.sequence, 5);
    assert!(matches!(next_day.decision, OpsDecision::PersistIntent(_)));
    assert_eq!(
        OpsJob::new(1, OpsKind::MountHeal, 86400).check(policy(), bad(86399)),
        Err(OpsError::ClockRegression)
    );
}

#[test]
fn publish_watchdog_requires_authoritative_resources_and_server_capability() {
    for present in [None, Some(false), Some(true)] {
        for provides in [false, true] {
            for remote in [false, true] {
                assert_eq!(
                    publish_health(present, provides, remote),
                    match present {
                        None => OpsHealth::Unknown,
                        Some(true) if provides && remote => OpsHealth::Healthy,
                        Some(_) => OpsHealth::Unhealthy,
                    }
                );
            }
        }
    }
}

struct FakeArbiter {
    calls: Vec<VerifiedImportProposal>,
    receipt: ArbiterReceipt,
}
impl ImportArbiter for FakeArbiter {
    fn propose(&mut self, proposal: VerifiedImportProposal) -> ArbiterReceipt {
        self.calls.push(proposal);
        self.receipt
    }
}
fn request() -> MatchedIdRequest {
    let source = FileIdentity { id: 3, revision: 4 };
    MatchedIdRequest {
        job: 1,
        library: 2,
        access: Access::Granted {
            library: 2,
            expires_at: 10,
        },
        now: 1,
        download: 3,
        owned_download: 3,
        import_blocked: true,
        warnings: vec![ImportWarning::MatchedById],
        queue_series: 4,
        proposed_series: 4,
        proposed_episodes: vec![Episode {
            season: 1,
            number: 2,
        }],
        rejection_count: 0,
        filename: "Show.S01E02.mkv".into(),
        bytes: MIN_EPISODE_BYTES,
        source,
        decoded_source: Some(source),
    }
}

#[test]
fn confirmer_dry_run_and_denials_never_call_an_arr_or_arbiter() {
    let mut arbiter = FakeArbiter {
        calls: vec![],
        receipt: ArbiterReceipt::Queued,
    };
    assert_eq!(
        confirm_matched_by_id(ConfirmerPolicy::default(), &request(), &mut arbiter),
        ConfirmDecision::Disabled
    );
    let dry = ConfirmerPolicy {
        enabled: true,
        dry_run: true,
    };
    assert_eq!(
        confirm_matched_by_id(dry, &request(), &mut arbiter),
        ConfirmDecision::DryRun(matched_by_id(&request()).unwrap())
    );
    let live = ConfirmerPolicy {
        dry_run: false,
        ..dry
    };
    assert_eq!(
        confirm_matched_by_id(
            live,
            &MatchedIdRequest {
                access: Access::Unknown,
                ..request()
            },
            &mut arbiter
        ),
        ConfirmDecision::Denied(ImportDenial::Access)
    );
    assert!(arbiter.calls.is_empty());
    for receipt in [
        ArbiterReceipt::Queued,
        ArbiterReceipt::Uncertain,
        ArbiterReceipt::Rejected,
    ] {
        arbiter.receipt = receipt;
        assert_eq!(
            confirm_matched_by_id(live, &request(), &mut arbiter),
            ConfirmDecision::Arbiter(receipt)
        );
    }
    assert_eq!(arbiter.calls, vec![matched_by_id(&request()).unwrap(); 3]);
}

#[test]
fn starting_midday_keeps_daily_reservations_until_the_next_utc_day() {
    let one_per_day = OpsPolicy {
        daily_limit: 1,
        cooldown_seconds: 1,
        ..policy()
    };
    let job = OpsJob::new(41, OpsKind::MountHeal, 90000);
    let job = job.check(one_per_day, bad(90000)).unwrap().job;
    let job = job.check(one_per_day, bad(90001)).unwrap().job;
    let job = job
        .resolve(intent(&job).key, OpsHealth::Healthy)
        .unwrap()
        .job;
    let job = job.check(one_per_day, bad(90002)).unwrap().job;
    assert_eq!(
        job.check(one_per_day, bad(172799)).unwrap().decision,
        OpsDecision::Hold(OpsHold::DailyCap)
    );
    let transition = job.check(one_per_day, bad(172800)).unwrap();
    assert!(matches!(transition.decision, OpsDecision::PersistIntent(_)));
    assert_eq!(transition.job.reserved_today(), 1);
}
