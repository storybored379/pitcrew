use pitcrew_domain::{
    Access, Constraints, Effect, Event, FileIdentity, FixJob, Observation, Policy,
};
use pitcrew_fakes::holding::{FakeDownload, HoldingArr, HoldingSettings, ImportCall, ImportRoute};

const OLD: FileIdentity = FileIdentity { id: 1, revision: 1 };
const NEW: FileIdentity = FileIdentity { id: 2, revision: 1 };

fn stack(category: &str) -> HoldingArr {
    HoldingArr {
        settings: HoldingSettings {
            watched_categories: vec!["arr-managed".into()],
            submission_category: category.into(),
            can_choose_category_before_submission: true,
            can_manual_import_exact_file: true,
        },
        library_file: OLD,
        download: Some(FakeDownload {
            id: 5,
            file: NEW,
            category: category.into(),
            complete: true,
        }),
        calls: vec![],
    }
}

fn access() -> Access {
    Access::Granted {
        library: 2,
        expires_at: 100,
    }
}

fn downloaded_job() -> FixJob {
    let job = FixJob::new(
        1,
        2,
        OLD,
        Constraints::default(),
        Policy {
            max_grabs: 2,
            stage_timeout: 10,
            total_timeout: 100,
        },
        0,
    )
    .unwrap();
    let job = job
        .apply(
            Event::SelectRelease {
                release: 3,
                access: access(),
            },
            1,
        )
        .unwrap()
        .job;
    let job = job
        .apply(
            Event::Observed {
                intent: job.expected_observation().unwrap(),
                observation: Observation::GrabAccepted { download: 5 },
            },
            2,
        )
        .unwrap()
        .job;
    job.apply(
        Event::Observed {
            intent: job.expected_observation().unwrap(),
            observation: Observation::DownloadCompleted {
                download: 5,
                file: NEW,
            },
        },
        3,
    )
    .unwrap()
    .job
}

#[test]
fn reproduction_ordinary_upgrade_imports_before_any_decode() {
    let mut arr = stack("arr-managed");
    assert_eq!(arr.settings.route(), ImportRoute::VerifyAfterImport);
    arr.completed_download_handling();
    assert_eq!(arr.library_file, NEW);
    assert_eq!(arr.calls, [ImportCall::AutoImport(NEW)]);
}

#[test]
fn held_category_preserves_old_copy_until_reducer_authorizes_exact_import() {
    let mut arr = stack("pitcrew-held");
    assert_eq!(arr.settings.route(), ImportRoute::Held);
    arr.completed_download_handling();
    assert_eq!(arr.library_file, OLD);
    let job = downloaded_job();
    assert!(
        job.apply(Event::RequestImport { access: access() }, 4)
            .is_err()
    );
    let job = job
        .apply(
            Event::Observed {
                intent: job.expected_observation().unwrap(),
                observation: Observation::DownloadDecoded {
                    file: NEW,
                    good: true,
                },
            },
            4,
        )
        .unwrap()
        .job;
    let transition = job
        .apply(Event::RequestImport { access: access() }, 5)
        .unwrap();
    let [intent] = transition.intents.as_slice() else {
        panic!("one import intent required")
    };
    let Effect::ImportVerifiedDownload {
        download,
        file,
        original,
    } = intent.effect
    else {
        panic!("expected exact verified import")
    };
    assert_eq!(original, OLD);
    arr.manual_import(download, file).unwrap();
    assert_eq!(arr.library_file, NEW);
    assert_eq!(arr.calls, [ImportCall::ManualImport(NEW)]);
}

#[test]
fn rejected_or_changed_held_bytes_never_replace_library() {
    let mut arr = stack("pitcrew-held");
    let job = downloaded_job();
    let job = job
        .apply(
            Event::Observed {
                intent: job.expected_observation().unwrap(),
                observation: Observation::DownloadDecoded {
                    file: NEW,
                    good: false,
                },
            },
            4,
        )
        .unwrap()
        .job;
    assert!(
        job.apply(Event::RequestImport { access: access() }, 5)
            .is_err()
    );
    arr.completed_download_handling();
    assert_eq!(arr.library_file, OLD);
    arr.download.as_mut().unwrap().file.revision += 1;
    assert!(arr.manual_import(5, NEW).is_err());
    assert_eq!(arr.library_file, OLD);
    assert!(arr.calls.is_empty());
}

#[test]
fn late_category_change_cannot_undo_an_import() {
    let mut arr = stack("arr-managed");
    arr.completed_download_handling();
    arr.settings.submission_category = "pitcrew-held".into();
    assert_eq!(arr.library_file, NEW);
    assert!(arr.download.is_none());
}

#[test]
fn unavailable_isolation_requires_verify_after_import() {
    let valid = stack("pitcrew-held").settings;
    let mut settings = valid.clone();
    settings.can_choose_category_before_submission = false;
    assert_eq!(settings.route(), ImportRoute::VerifyAfterImport);
    settings = valid.clone();
    settings.can_manual_import_exact_file = false;
    assert_eq!(settings.route(), ImportRoute::VerifyAfterImport);
    settings = valid;
    settings.watched_categories.push("pitcrew-held".into());
    assert_eq!(settings.route(), ImportRoute::VerifyAfterImport);
}

#[test]
fn unsupported_hold_auto_imports_then_rechecks_exact_destination_before_ready() {
    use pitcrew_domain::{StageTimeouts, State};
    let mut arr = stack("arr-managed");
    let job = FixJob::new_with_timeouts(
        1,
        2,
        OLD,
        Constraints::default(),
        Policy {
            max_grabs: 2,
            stage_timeout: 10,
            total_timeout: 100,
        },
        StageTimeouts {
            download: 10,
            import: 10,
            decode: 10,
            verify_retry: 3,
        },
        0,
    )
    .unwrap();
    let transition = job
        .apply(
            Event::SelectRelease {
                release: 3,
                access: access(),
            },
            1,
        )
        .unwrap();
    let grab = transition.job.expected_observation().unwrap();
    // The fake arr can finish and import before the accepted response is observed.
    arr.completed_download_handling();
    let transition = transition
        .job
        .apply(
            Event::AutoImported {
                grab,
                download: 5,
                file: arr.library_file,
            },
            2,
        )
        .unwrap();
    assert_eq!(transition.job.state(), State::Imported);
    assert_eq!(
        transition.intents[0].effect,
        Effect::DecodeImported { file: NEW }
    );
    let job = transition.job;
    let transition = job
        .apply(
            Event::Observed {
                intent: job.expected_observation().unwrap(),
                observation: Observation::ImportedDecoded {
                    file: NEW,
                    good: false,
                },
            },
            3,
        )
        .unwrap();
    assert_eq!(transition.job.state(), State::VerifyRetry);
    assert!(transition.intents.is_empty());
    let transition = transition.job.apply(Event::Tick, 5).unwrap();
    assert!(transition.intents.is_empty());
    let transition = transition.job.apply(Event::Tick, 6).unwrap();
    assert_eq!(
        transition.intents[0].effect,
        Effect::DecodeImported { file: NEW }
    );
    let job = transition.job;
    let job = job
        .apply(
            Event::Observed {
                intent: job.expected_observation().unwrap(),
                observation: Observation::ImportedDecoded {
                    file: NEW,
                    good: true,
                },
            },
            7,
        )
        .unwrap()
        .job;
    let job = job
        .apply(
            Event::Observed {
                intent: job.expected_observation().unwrap(),
                observation: Observation::TargetChecked {
                    file: NEW,
                    plex_file: NEW,
                    constraints_satisfied: true,
                },
            },
            8,
        )
        .unwrap()
        .job;
    let transition = job.apply(Event::QueueReady, 9).unwrap();
    assert_eq!(transition.intents.len(), 1);
    assert_eq!(
        transition.intents[0].effect,
        Effect::NotifyReady { file: NEW }
    );
    assert!(transition.job.apply(Event::QueueReady, 10).is_err());
    assert_eq!(arr.calls, [ImportCall::AutoImport(NEW)]);
}
