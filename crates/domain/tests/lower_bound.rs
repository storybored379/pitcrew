use pitcrew_domain::*;

fn bounded(minimum: u32, candidate: u32, original: u32) -> LowerQualityBound {
    LowerQualityBound {
        minimum_resolution: minimum,
        candidate_resolution: candidate,
        original_resolution: original,
    }
}

#[test]
fn resolution_bounds_require_an_explicit_floor_and_strict_downgrade() {
    for (minimum, candidate, original, valid) in [
        (1080, 1080, 2160, true),
        (720, 1080, 2160, true),
        (0, 1080, 2160, false),
        (1080, 720, 2160, false),
        (1080, 2160, 2160, false),
        (1080, 2161, 2160, false),
    ] {
        assert_eq!(bounded(minimum, candidate, original).valid(), valid);
    }
}

#[test]
fn bounded_request_preserves_the_floor_and_still_requires_budget_broken_bytes_and_access() {
    let file = FileIdentity { id: 1, revision: 1 };
    let candidate = ReleaseCandidate {
        release: 2,
        identity: Default::default(),
        quality: QualityRelation::Lower,
    };
    let access = Access::Granted {
        library: 1,
        expires_at: 100,
    };
    let bound = bounded(720, 1080, 2160);
    let event = Event::RequestBoundedLowerQuality {
        candidate: candidate.clone(),
        bound,
        access,
    };
    for budget in [0, 1] {
        let job = FixJob::new(
            1,
            1,
            file,
            Constraints::default(),
            Policy {
                max_grabs: budget,
                stage_timeout: 20,
                total_timeout: 100,
            },
            0,
        )
        .unwrap();
        assert_eq!(job.bounded_lower_quality(), None);
        assert!(job.apply(event.clone(), 1).is_err());
        let broken = job
            .apply(Event::BrokenFileConfirmed { file }, 1)
            .unwrap()
            .job;
        let result = broken.apply(event.clone(), 1);
        if budget == 0 {
            assert!(result.is_err());
            continue;
        }
        let pending = result.unwrap();
        assert_eq!(pending.job.state(), State::AwaitingQualityApproval);
        assert!(pending.intents.is_empty());
        assert_eq!(pending.job.reserved_budget(), 0);
        assert_eq!(
            pending.job.bounded_lower_quality(),
            Some((&candidate, bound))
        );
        for (bound, access, quality) in [
            (bounded(1080, 720, 2160), access, QualityRelation::Lower),
            (bound, Access::Denied, QualityRelation::Lower),
            (bound, access, QualityRelation::SameOrBetter),
        ] {
            assert!(
                broken
                    .apply(
                        Event::RequestBoundedLowerQuality {
                            candidate: ReleaseCandidate {
                                quality,
                                ..candidate.clone()
                            },
                            bound,
                            access,
                        },
                        1
                    )
                    .is_err()
            );
        }
        let approved = pending
            .job
            .apply(
                Event::ApproveLowerQuality {
                    approval: AdminApproval {
                        job: 1,
                        release: 2,
                        broken_file: file,
                        expires_at: 90,
                    },
                    access,
                },
                2,
            )
            .unwrap();
        assert_eq!(approved.job.state(), State::UpgradeGuardIntent);
        assert_eq!(
            approved.job.bounded_lower_quality(),
            Some((&candidate, bound))
        );
        assert_eq!(
            serde_json::to_value(&approved.job).unwrap()["lower_quality"]["bound"],
            serde_json::to_value(bound).unwrap()
        );
        assert!(matches!(
            approved.intents[0].effect,
            Effect::InstallUpgradeGuard { .. }
        ));
        assert!(
            approved
                .job
                .apply(Event::GrabApproved { access }, 2)
                .is_err()
        );
        let mut reconciliation = approved
            .job
            .apply(
                Event::Observed {
                    intent: approved.job.expected_observation().unwrap(),
                    observation: Observation::UpgradeGuardAmbiguous,
                },
                3,
            )
            .unwrap()
            .job;
        assert_eq!(
            reconciliation.bounded_lower_quality(),
            Some((&candidate, bound))
        );
        for (at, result, state) in [
            (
                4,
                UpgradeGuardResolution::Unknown,
                State::UpgradeGuardReconciling,
            ),
            (
                5,
                UpgradeGuardResolution::Absent,
                State::AwaitingQualityApproval,
            ),
        ] {
            reconciliation = reconciliation
                .apply(
                    Event::Observed {
                        intent: reconciliation.expected_observation().unwrap(),
                        observation: Observation::UpgradeGuardReconciled(result),
                    },
                    at,
                )
                .unwrap()
                .job;
            assert_eq!(reconciliation.state(), state);
            assert_eq!(
                reconciliation.bounded_lower_quality(),
                Some((&candidate, bound))
            );
            assert!(
                reconciliation
                    .apply(Event::GrabApproved { access }, at)
                    .is_err()
            );
        }
        assert!(
            serde_json::to_value(&reconciliation).unwrap()["lower_quality"]["approved_until"]
                .is_null()
        );
        let guarded = approved
            .job
            .apply(
                Event::Observed {
                    intent: approved.job.expected_observation().unwrap(),
                    observation: Observation::UpgradeGuardInstalled,
                },
                3,
            )
            .unwrap()
            .job;
        assert_eq!(guarded.bounded_lower_quality(), Some((&candidate, bound)));
        let grabbed = guarded.apply(Event::GrabApproved { access }, 4).unwrap();
        assert_eq!(grabbed.job.reserved_budget(), 1);
        assert!(matches!(
            grabbed.intents[0].effect,
            Effect::Grab { reservation: 1, .. }
        ));
    }
}
