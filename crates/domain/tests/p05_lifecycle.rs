use pitcrew_domain::*;
use serde_json::json;

const ORIGINAL: FileIdentity = FileIdentity {
    id: 10,
    revision: 1,
};
const IMPORTED: FileIdentity = FileIdentity {
    id: 30,
    revision: 1,
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
        Constraints::default(),
        Policy {
            max_grabs: 3,
            stage_timeout: 10,
            total_timeout: 100,
        },
        0,
    )
    .unwrap()
}
fn observed(job: &FixJob, observation: Observation, now: u64) -> Transition {
    job.apply(
        Event::Observed {
            intent: job.expected_observation().unwrap(),
            observation,
        },
        now,
    )
    .unwrap()
}
fn downloading() -> FixJob {
    let grab = job()
        .apply(
            Event::SelectRelease {
                release: 40,
                access: ACCESS,
            },
            1,
        )
        .unwrap()
        .job;
    observed(&grab, Observation::GrabAccepted { download: 50 }, 1).job
}
fn imported() -> FixJob {
    let download = FileIdentity {
        id: 20,
        revision: 1,
    };
    let complete = observed(
        &downloading(),
        Observation::DownloadCompleted {
            download: 50,
            file: download,
        },
        1,
    )
    .job;
    let good = observed(
        &complete,
        Observation::DownloadDecoded {
            file: download,
            good: true,
        },
        1,
    )
    .job;
    let import = good
        .apply(Event::RequestImport { access: ACCESS }, 1)
        .unwrap()
        .job;
    let accepted = observed(&import, Observation::ImportAccepted, 1).job;
    observed(
        &accepted,
        Observation::ImportCompleted {
            download: 50,
            file: IMPORTED,
        },
        1,
    )
    .job
}

#[test]
fn fresh_bad_import_waits_before_counting_as_failed() {
    let retry = observed(
        &imported(),
        Observation::ImportedDecoded {
            file: IMPORTED,
            good: false,
        },
        2,
    );
    assert_eq!(
        serde_json::to_value(retry.job.state()).unwrap(),
        json!("VerifyRetry")
    );
    assert!(retry.intents.is_empty());
    assert_eq!(retry.job.failure(), None);
    assert_eq!(retry.job.deadline(), Some(12));
    assert!(retry.job.apply(Event::Tick, 11).unwrap().intents.is_empty());
    let check = retry.job.apply(Event::Tick, 12).unwrap();
    assert_eq!(check.job.state(), State::Imported);
    assert_eq!(
        check.intents[0].effect,
        Effect::DecodeImported { file: IMPORTED }
    );
}

#[test]
fn failed_download_ends_wait_immediately() {
    let current = downloading();
    let event = serde_json::from_value(json!({"DownloadFailed": {
        "grab": {"job": 1, "sequence": 1}, "download": 50
    }}))
    .unwrap();
    let failed = current.apply(event, 2).unwrap();
    assert_eq!(failed.job.state(), State::Rejected);
    assert_eq!(failed.job.active_grabs(), 0);
    assert_eq!(failed.job.reserved_budget(), 1);
    assert_eq!(failed.job.failure(), None);
    assert!(
        failed
            .job
            .apply(
                Event::SelectRelease {
                    release: 40,
                    access: ACCESS
                },
                2
            )
            .is_err()
    );
}

#[test]
fn arr_auto_import_starts_post_import_verification() {
    let current = downloading();
    let event = serde_json::from_value(json!({"AutoImported": {
        "grab": {"job": 1, "sequence": 1}, "download": 50, "file": IMPORTED
    }}))
    .unwrap();
    let imported = current.apply(event, 2).unwrap();
    assert_eq!(imported.job.state(), State::Imported);
    assert_eq!(imported.job.active_grabs(), 0);
    assert_eq!(
        imported.intents[0].effect,
        Effect::DecodeImported { file: IMPORTED }
    );
}

#[test]
fn lower_quality_requires_broken_evidence_admin_approval_and_confirmed_upgrade_guard() {
    let event = serde_json::from_value(json!({"RequestLowerQuality": {
        "candidate": {"release": 41, "identity": {}, "quality": "Lower"}, "access": ACCESS
    }}))
    .unwrap();
    assert!(job().apply(event, 1).is_err());
}

fn candidate(release: u64, quality: QualityRelation) -> ReleaseCandidate {
    ReleaseCandidate {
        release,
        identity: media::ReleaseIdentity::default(),
        quality,
    }
}
fn approve() -> AdminApproval {
    AdminApproval {
        job: 1,
        release: 41,
        broken_file: ORIGINAL,
        expires_at: 100,
    }
}
fn approval_request() -> FixJob {
    let dead = downloading()
        .apply(
            Event::DownloadFailed {
                grab: IntentKey {
                    job: 1,
                    sequence: 1,
                },
                download: 50,
            },
            1,
        )
        .unwrap()
        .job;
    let broken = dead
        .apply(Event::BrokenFileConfirmed { file: ORIGINAL }, 1)
        .unwrap()
        .job;
    broken
        .apply(
            Event::RequestLowerQuality {
                candidate: candidate(41, QualityRelation::Lower),
                access: ACCESS,
            },
            1,
        )
        .unwrap()
        .job
}
#[test]
fn failed_release_exclusions_match_identifiers_across_renames() {
    let first = ReleaseCandidate {
        release: 40,
        quality: QualityRelation::SameOrBetter,
        identity: media::ReleaseIdentity {
            guid: Some(media::ReleaseGuid {
                source: "indexer-a".into(),
                value: "guid-a".into(),
            }),
            infohash: Some(media::InfoHash("abc".into())),
            download: Some(media::DownloadIdentity {
                client_id: "client-a".into(),
                download_id: "download-a".into(),
            }),
        },
    };
    let grabbed = job()
        .apply(
            Event::SelectCandidate {
                candidate: first.clone(),
                access: ACCESS,
            },
            1,
        )
        .unwrap()
        .job;
    let failed = grabbed
        .apply(
            Event::DownloadFailed {
                grab: IntentKey {
                    job: 1,
                    sequence: 1,
                },
                download: 50,
            },
            1,
        )
        .unwrap();
    assert_eq!(failed.job.excluded_releases(), std::slice::from_ref(&first));
    assert_eq!(failed.job.expected_observation(), None);
    assert_eq!(failed.job.unresolved_mutation(), None);
    assert!(failed.intents.is_empty());
    for identity in [
        media::ReleaseIdentity {
            guid: first.identity.guid.clone(),
            ..Default::default()
        },
        media::ReleaseIdentity {
            infohash: Some(media::InfoHash("ABC".into())),
            ..Default::default()
        },
        media::ReleaseIdentity {
            download: first.identity.download.clone(),
            ..Default::default()
        },
    ] {
        let alias = ReleaseCandidate {
            release: 41,
            quality: QualityRelation::SameOrBetter,
            identity,
        };
        assert_eq!(
            failed.job.apply(
                Event::SelectCandidate {
                    candidate: alias,
                    access: ACCESS
                },
                2
            ),
            Err(TransitionError::ExcludedRelease)
        );
    }
    let next = failed
        .job
        .apply(
            Event::SelectCandidate {
                candidate: candidate(41, QualityRelation::SameOrBetter),
                access: ACCESS,
            },
            2,
        )
        .unwrap();
    assert_eq!(
        next.intents[0].effect,
        Effect::Grab {
            release: 41,
            reservation: 2
        }
    );
    assert_eq!(next.job.active_grabs(), 1);
}

#[test]
fn asynchronous_download_events_keep_original_grab_correlation() {
    let initial = job()
        .apply(
            Event::SelectRelease {
                release: 40,
                access: ACCESS,
            },
            1,
        )
        .unwrap()
        .job;
    let downloading = observed(&initial, Observation::GrabAccepted { download: 50 }, 1).job;
    let download_file = FileIdentity {
        id: 20,
        revision: 1,
    };
    let downloaded = observed(
        &downloading,
        Observation::DownloadCompleted {
            download: 50,
            file: download_file,
        },
        1,
    )
    .job;
    let reconciling = observed(&initial, Observation::GrabAmbiguous, 1).job;
    let good = observed(
        &downloaded,
        Observation::DownloadDecoded {
            file: download_file,
            good: true,
        },
        1,
    )
    .job;
    let bad = observed(
        &downloaded,
        Observation::DownloadDecoded {
            file: download_file,
            good: false,
        },
        1,
    )
    .job;
    for before in [initial, reconciling, downloading, downloaded, good, bad] {
        for event in [
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
        ] {
            let next = before.apply(event.clone(), 2).unwrap();
            if matches!(event, Event::DownloadFailed { .. }) {
                assert_eq!(next.job.excluded_releases().len(), 1);
            }
            assert_eq!(next.job.unresolved_mutation(), None);
            assert_eq!(next.job.active_grabs(), 0);
            for key in [
                IntentKey {
                    job: 2,
                    sequence: 1,
                },
                IntentKey {
                    job: 1,
                    sequence: 99,
                },
            ] {
                let stale = match event {
                    Event::DownloadFailed { download, .. } => Event::DownloadFailed {
                        grab: key,
                        download,
                    },
                    Event::AutoImported { download, file, .. } => Event::AutoImported {
                        grab: key,
                        download,
                        file,
                    },
                    _ => unreachable!(),
                };
                assert_eq!(
                    before.apply(stale, 2),
                    Err(TransitionError::StaleObservation)
                );
            }
        }
    }
    for event in [
        Event::DownloadFailed {
            grab: IntentKey {
                job: 1,
                sequence: 1,
            },
            download: 51,
        },
        Event::AutoImported {
            grab: IntentKey {
                job: 1,
                sequence: 1,
            },
            download: 51,
            file: IMPORTED,
        },
        Event::AutoImported {
            grab: IntentKey {
                job: 1,
                sequence: 1,
            },
            download: 50,
            file: ORIGINAL,
        },
    ] {
        assert_eq!(
            self::downloading().apply(event, 2),
            Err(TransitionError::IdentityMismatch)
        );
    }
}

#[test]
fn confirmed_bad_auto_import_rolls_forward_and_preserves_current_library_file() {
    let auto = downloading()
        .apply(
            Event::AutoImported {
                grab: IntentKey {
                    job: 1,
                    sequence: 1,
                },
                download: 50,
                file: IMPORTED,
            },
            2,
        )
        .unwrap()
        .job;
    let first = observed(
        &auto,
        Observation::ImportedDecoded {
            file: IMPORTED,
            good: false,
        },
        3,
    )
    .job;
    assert!(
        first
            .apply(
                Event::Observed {
                    intent: auto.expected_observation().unwrap(),
                    observation: Observation::ImportedDecoded {
                        file: IMPORTED,
                        good: false
                    }
                },
                4
            )
            .is_err()
    );
    let retry = first.apply(Event::Tick, 13).unwrap().job;
    assert_ne!(retry.expected_observation(), auto.expected_observation());
    assert_eq!(
        retry.apply(
            Event::Observed {
                intent: auto.expected_observation().unwrap(),
                observation: Observation::ImportedDecoded {
                    file: IMPORTED,
                    good: true
                }
            },
            13
        ),
        Err(TransitionError::StaleObservation)
    );
    let rejected = observed(
        &retry,
        Observation::ImportedDecoded {
            file: IMPORTED,
            good: false,
        },
        14,
    );
    assert_eq!(rejected.job.state(), State::Rejected);
    assert_eq!(
        rejected.job.excluded_releases(),
        &[candidate(40, QualityRelation::SameOrBetter)]
    );
    assert!(rejected.intents.is_empty());
    assert_eq!(rejected.job.active_grabs(), 0);
    assert_eq!(rejected.job.failure(), None);
    let next = rejected
        .job
        .apply(
            Event::SelectRelease {
                release: 41,
                access: ACCESS,
            },
            14,
        )
        .unwrap()
        .job;
    let next = observed(&next, Observation::GrabAccepted { download: 51 }, 14).job;
    let replacement = FileIdentity {
        id: 21,
        revision: 1,
    };
    let next = observed(
        &next,
        Observation::DownloadCompleted {
            download: 51,
            file: replacement,
        },
        14,
    )
    .job;
    let next = observed(
        &next,
        Observation::DownloadDecoded {
            file: replacement,
            good: true,
        },
        14,
    )
    .job;
    let importing = next
        .apply(Event::RequestImport { access: ACCESS }, 14)
        .unwrap();
    assert_eq!(
        importing.intents[0].effect,
        Effect::ImportVerifiedDownload {
            download: 51,
            file: replacement,
            original: IMPORTED
        }
    );
    assert_eq!(importing.job.reserved_budget(), 2);
}

#[test]
fn fresh_import_can_recover_after_delay_and_never_sends_ready_before_exact_target() {
    let first = observed(
        &imported(),
        Observation::ImportedDecoded {
            file: IMPORTED,
            good: false,
        },
        2,
    )
    .job;
    let retry = first.apply(Event::Tick, 12).unwrap().job;
    let good = observed(
        &retry,
        Observation::ImportedDecoded {
            file: IMPORTED,
            good: true,
        },
        13,
    );
    assert_eq!(good.job.state(), State::Imported);
    assert_eq!(
        good.intents[0].effect,
        Effect::CheckTarget {
            file: IMPORTED,
            constraints: Constraints::default()
        }
    );
    assert!(good.job.apply(Event::QueueReady, 13).is_err());
    let verified = observed(
        &good.job,
        Observation::TargetChecked {
            file: IMPORTED,
            plex_file: IMPORTED,
            constraints_satisfied: true,
        },
        13,
    )
    .job;
    let done = verified.apply(Event::QueueReady, 13).unwrap();
    assert_eq!(
        done.intents[0].effect,
        Effect::NotifyReady { file: IMPORTED }
    );
    assert!(done.job.apply(Event::QueueReady, 13).is_err());
}

#[test]
fn retry_delay_does_not_swallow_cancellation_or_extend_total_deadline() {
    let retry = observed(
        &imported(),
        Observation::ImportedDecoded {
            file: IMPORTED,
            good: false,
        },
        2,
    )
    .job;
    for now in [11, 12, 13] {
        let cancelled = retry.apply(Event::Fail(Failure::Cancelled), now).unwrap();
        assert_eq!(cancelled.job.failure(), Some(Failure::Cancelled));
        assert_eq!(cancelled.intents.len(), 2);
    }
    for now in [100, u64::MAX] {
        let expired = retry.apply(Event::Tick, now).unwrap();
        assert_eq!(expired.job.failure(), Some(Failure::Deadline));
        assert_eq!(expired.intents.len(), 2);
    }
}

#[test]
fn admin_guard_is_committed_reconciled_and_authorized_before_lower_quality_grab() {
    let request = approval_request();
    assert_eq!(request.state(), State::AwaitingQualityApproval);
    assert_eq!(request.reserved_budget(), 1);
    let guarded = request
        .apply(
            Event::ApproveLowerQuality {
                approval: approve(),
                access: ACCESS,
            },
            2,
        )
        .unwrap();
    assert_eq!(guarded.job.state(), State::UpgradeGuardIntent);
    assert_eq!(
        guarded.job.unresolved_mutation(),
        Some(Mutation::UpgradeGuard)
    );
    assert_eq!(
        guarded.intents[0].effect,
        Effect::InstallUpgradeGuard {
            excluded: vec![candidate(40, QualityRelation::SameOrBetter)]
        }
    );
    assert!(
        guarded
            .job
            .apply(Event::GrabApproved { access: ACCESS }, 2)
            .is_err()
    );
    let ambiguous = observed(&guarded.job, Observation::UpgradeGuardAmbiguous, 3);
    assert_eq!(ambiguous.job.state(), State::UpgradeGuardReconciling);
    assert_eq!(
        ambiguous.job.unresolved_mutation(),
        Some(Mutation::UpgradeGuard)
    );
    assert_eq!(
        ambiguous.intents[0].effect,
        Effect::ReconcileUpgradeGuard {
            excluded: request.excluded_releases().to_vec()
        }
    );
    let unknown = observed(
        &ambiguous.job,
        Observation::UpgradeGuardReconciled(UpgradeGuardResolution::Unknown),
        4,
    );
    assert_eq!(unknown.job.deadline(), ambiguous.job.deadline());
    assert_eq!(
        unknown.job.unresolved_mutation(),
        Some(Mutation::UpgradeGuard)
    );
    assert!(unknown.intents.is_empty());
    let absent = observed(
        &ambiguous.job,
        Observation::UpgradeGuardReconciled(UpgradeGuardResolution::Absent),
        4,
    )
    .job;
    assert_eq!(absent.state(), State::AwaitingQualityApproval);
    assert_eq!(absent.unresolved_mutation(), None);
    assert_eq!(
        serde_json::to_value(&absent).unwrap()["lower_quality"]["approved_until"],
        serde_json::Value::Null
    );
    assert!(
        absent
            .apply(Event::GrabApproved { access: ACCESS }, 4)
            .is_err()
    );
    for ready in [
        observed(&guarded.job, Observation::UpgradeGuardInstalled, 4).job,
        observed(
            &ambiguous.job,
            Observation::UpgradeGuardReconciled(UpgradeGuardResolution::Installed),
            4,
        )
        .job,
    ] {
        assert_eq!(ready.state(), State::QualityApproved);
        assert_eq!(ready.unresolved_mutation(), None);
        let grab = ready
            .apply(Event::GrabApproved { access: ACCESS }, 5)
            .unwrap();
        assert_eq!(grab.job.state(), State::GrabIntent);
        assert_eq!(
            grab.intents[0].effect,
            Effect::Grab {
                release: 41,
                reservation: 2
            }
        );
        assert_eq!(grab.job.reserved_budget(), 2);
        assert_eq!(grab.job.active_grabs(), 1);
        let accepted = observed(&grab.job, Observation::GrabAccepted { download: 51 }, 5).job;
        let failed = accepted
            .apply(
                Event::DownloadFailed {
                    grab: grab.job.expected_observation().unwrap(),
                    download: 51,
                },
                5,
            )
            .unwrap()
            .job;
        assert_eq!(
            failed.excluded_releases(),
            &[
                candidate(40, QualityRelation::SameOrBetter),
                candidate(41, QualityRelation::Lower)
            ]
        );
    }
}

#[test]
fn lower_quality_checks_exact_broken_file_approval_scope_expiry_and_access() {
    assert_eq!(
        job().apply(Event::BrokenFileConfirmed { file: IMPORTED }, 1),
        Err(TransitionError::IdentityMismatch)
    );
    assert_eq!(
        job().apply(
            Event::SelectCandidate {
                candidate: candidate(41, QualityRelation::Lower),
                access: ACCESS
            },
            1
        ),
        Err(TransitionError::ApprovalRequired)
    );
    let broken = job()
        .apply(Event::BrokenFileConfirmed { file: ORIGINAL }, 1)
        .unwrap()
        .job;
    assert_eq!(
        broken.apply(
            Event::RequestLowerQuality {
                candidate: candidate(41, QualityRelation::SameOrBetter),
                access: ACCESS
            },
            1
        ),
        Err(TransitionError::ApprovalRequired)
    );
    let request = approval_request();
    for approval in [
        AdminApproval {
            job: 2,
            ..approve()
        },
        AdminApproval {
            release: 42,
            ..approve()
        },
        AdminApproval {
            broken_file: IMPORTED,
            ..approve()
        },
        AdminApproval {
            expires_at: 1,
            ..approve()
        },
        AdminApproval {
            expires_at: 0,
            ..approve()
        },
    ] {
        assert_eq!(
            request.apply(
                Event::ApproveLowerQuality {
                    approval,
                    access: ACCESS
                },
                1
            ),
            Err(TransitionError::ApprovalRequired)
        );
    }
    let short = request
        .apply(
            Event::ApproveLowerQuality {
                approval: AdminApproval {
                    expires_at: 3,
                    ..approve()
                },
                access: ACCESS,
            },
            1,
        )
        .unwrap()
        .job;
    let ready = observed(&short, Observation::UpgradeGuardInstalled, 2).job;
    assert!(
        ready
            .apply(Event::GrabApproved { access: ACCESS }, 2)
            .is_ok()
    );
    for now in [3, 4] {
        assert_eq!(
            ready.apply(Event::GrabApproved { access: ACCESS }, now),
            Err(TransitionError::ApprovalRequired)
        );
    }
    for access in [
        Access::Unknown,
        Access::Denied,
        Access::Granted {
            library: 8,
            expires_at: 100,
        },
        Access::Granted {
            library: 7,
            expires_at: 1,
        },
    ] {
        for (before, event) in [
            (
                job(),
                Event::SelectCandidate {
                    candidate: candidate(41, QualityRelation::SameOrBetter),
                    access,
                },
            ),
            (
                broken.clone(),
                Event::RequestLowerQuality {
                    candidate: candidate(41, QualityRelation::Lower),
                    access,
                },
            ),
            (
                request.clone(),
                Event::ApproveLowerQuality {
                    approval: approve(),
                    access,
                },
            ),
            (ready.clone(), Event::GrabApproved { access }),
        ] {
            assert_eq!(before.apply(event, 2), Err(TransitionError::Unauthorized));
        }
    }
}

#[test]
fn client_specific_deadlines_are_used_for_download_import_decode_and_retry() {
    let initial = FixJob::new_with_timeouts(
        1,
        7,
        ORIGINAL,
        Constraints::default(),
        Policy {
            max_grabs: 3,
            stage_timeout: 10,
            total_timeout: 1000,
        },
        StageTimeouts {
            download: 180,
            import: 120,
            decode: 190,
            verify_retry: 200,
        },
        0,
    )
    .unwrap();
    let grab = initial
        .apply(
            Event::SelectRelease {
                release: 40,
                access: ACCESS,
            },
            1,
        )
        .unwrap()
        .job;
    assert_eq!(grab.deadline(), Some(11));
    let downloading = observed(&grab, Observation::GrabAccepted { download: 50 }, 2).job;
    assert_eq!(downloading.deadline(), Some(182));
    let downloaded = observed(
        &downloading,
        Observation::DownloadCompleted {
            download: 50,
            file: IMPORTED,
        },
        3,
    )
    .job;
    assert_eq!(downloaded.deadline(), Some(193));
    let good = observed(
        &downloaded,
        Observation::DownloadDecoded {
            file: IMPORTED,
            good: true,
        },
        4,
    )
    .job;
    assert_eq!(good.deadline(), Some(14));
    let import = good
        .apply(Event::RequestImport { access: ACCESS }, 5)
        .unwrap()
        .job;
    assert_eq!(import.deadline(), Some(125));
    let uncertain = observed(&import, Observation::ImportAmbiguous, 6).job;
    assert_eq!(uncertain.deadline(), Some(126));
    let accepted = observed(&import, Observation::ImportAccepted, 6).job;
    assert_eq!(accepted.deadline(), Some(126));
    let imported = observed(
        &accepted,
        Observation::ImportCompleted {
            download: 50,
            file: IMPORTED,
        },
        7,
    )
    .job;
    assert_eq!(imported.deadline(), Some(197));
    let retry = observed(
        &imported,
        Observation::ImportedDecoded {
            file: IMPORTED,
            good: false,
        },
        8,
    )
    .job;
    assert_eq!(retry.deadline(), Some(208));
    let retry = retry.apply(Event::Tick, 208).unwrap().job;
    assert_eq!(retry.deadline(), Some(398));
}

#[test]
fn every_client_timeout_is_positive_and_fits_the_clock() {
    for (now, bad) in [(0, 0), (u64::MAX - 1, 2)] {
        for timeouts in [
            StageTimeouts {
                download: bad,
                import: 1,
                decode: 1,
                verify_retry: 1,
            },
            StageTimeouts {
                download: 1,
                import: bad,
                decode: 1,
                verify_retry: 1,
            },
            StageTimeouts {
                download: 1,
                import: 1,
                decode: bad,
                verify_retry: 1,
            },
            StageTimeouts {
                download: 1,
                import: 1,
                decode: 1,
                verify_retry: bad,
            },
        ] {
            assert_eq!(
                FixJob::new_with_timeouts(
                    1,
                    7,
                    ORIGINAL,
                    Constraints::default(),
                    Policy {
                        max_grabs: 1,
                        stage_timeout: 1,
                        total_timeout: 1
                    },
                    timeouts,
                    now
                ),
                Err(TransitionError::InvalidPolicy)
            );
        }
    }
}

use proptest::prelude::*;
proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]
    #[test]
    fn failed_and_auto_import_paths_preserve_budget_exclusions_and_ready_evidence(
        attempts in prop::collection::vec((any::<bool>(), any::<bool>(), any::<bool>()), 1..8)
    ) {
        let mut current = job();
        let mut now = 1;
        let mut grabs = 0;
        let mut ready_count = 0;
        let mut failures = 0;
        for (index, (download_failed, first_good, retry_good)) in attempts.into_iter().enumerate() {
            if current.is_terminal() { break; }
            let release = 40 + index as u64;
            let selected = current.apply(Event::SelectRelease {release, access: ACCESS}, now).unwrap();
            for intent in &selected.intents {
                match intent.effect {
                    Effect::Grab {reservation, ..} => {grabs += 1; prop_assert_eq!(reservation, grabs);},
                    Effect::NotifyAdmin {..} => failures += 1,
                    _ => (),
                }
            }
            current = selected.job;
            if current.is_terminal() { break; }
            let grab = current.expected_observation().unwrap();
            current = observed(&current, Observation::GrabAccepted {download: release + 10}, now).job;
            if download_failed {
                let failed = current.apply(Event::DownloadFailed {grab, download: release + 10}, now).unwrap();
                prop_assert!(failed.intents.is_empty());
                current = failed.job;
            } else {
                let file = FileIdentity {id: release + 100, revision: 1};
                current = current.apply(Event::AutoImported {grab, download: release + 10, file}, now).unwrap().job;
                current = observed(&current, Observation::ImportedDecoded {file, good: first_good}, now).job;
                if !first_good {
                    prop_assert_eq!(current.state(), State::VerifyRetry);
                    let due = current.deadline().unwrap();
                    let early = current.apply(Event::Tick, due - 1).unwrap();
                    prop_assert!(early.intents.is_empty());
                    prop_assert_eq!(early.job.state(), State::VerifyRetry);
                    now = due;
                    current = current.apply(Event::Tick, now).unwrap().job;
                    let retry = observed(&current, Observation::ImportedDecoded {file, good: retry_good}, now);
                    prop_assert!(!retry.intents.iter().any(|i| matches!(i.effect, Effect::NotifyReady {..} | Effect::NotifyAdmin {..})), "retry sends no terminal notification");
                    current = retry.job;
                }
                if first_good || retry_good {
                    prop_assert_eq!(current.state(), State::Imported);
                    current = observed(&current, Observation::TargetChecked {file, plex_file: file, constraints_satisfied: true}, now).job;
                    let ready = current.apply(Event::QueueReady, now).unwrap();
                    prop_assert_eq!(&ready.intents[0].effect, &Effect::NotifyReady {file});
                    ready_count += 1;
                    current = ready.job;
                }
            }
            prop_assert_eq!(current.reserved_budget(), grabs);
            prop_assert_eq!(current.active_grabs(), 0);
            if current.state() == State::Rejected {
                prop_assert!(current.excluded_releases().iter().any(|dead| dead.release == release));
                prop_assert_eq!(current.apply(Event::SelectRelease {release, access: ACCESS}, now), Err(TransitionError::ExcludedRelease));
            }
            now += 1;
        }
        prop_assert!(grabs <= 3);
        prop_assert!(ready_count <= 1);
        prop_assert_eq!(failures, u32::from(current.state() == State::Failed));
    }
}

#[test]
fn negative_import_evidence_revokes_any_prior_decode_success_before_retry() {
    let good = observed(
        &imported(),
        Observation::ImportedDecoded {
            file: IMPORTED,
            good: true,
        },
        1,
    )
    .job;
    let bad = observed(
        &good,
        Observation::ImportedDecoded {
            file: IMPORTED,
            good: false,
        },
        2,
    )
    .job;
    let retry = bad.apply(Event::Tick, 12).unwrap().job;
    assert_eq!(
        retry.apply(
            Event::Observed {
                intent: retry.expected_observation().unwrap(),
                observation: Observation::TargetChecked {
                    file: IMPORTED,
                    plex_file: IMPORTED,
                    constraints_satisfied: true
                }
            },
            12
        ),
        Err(TransitionError::IdentityMismatch)
    );
}
