use pitcrew_domain::queue::{QueueActor, QueueImportRoute, QueueInterference};
use pitcrew_domain::{
    Access, Constraints, Effect, Event, FileIdentity, FixJob, Observation, Policy, State,
};
use pitcrew_fakes::{
    holding::{FakeDownload, HoldingArr, HoldingSettings, ImportCall},
    queue_actor::ExternalQueueActor,
};

const OLD: FileIdentity = FileIdentity { id: 1, revision: 1 };
const FIRST: FileIdentity = FileIdentity { id: 2, revision: 1 };
const REPLACEMENT: FileIdentity = FileIdentity { id: 3, revision: 1 };
fn access() -> Access {
    Access::Granted {
        library: 2,
        expires_at: 1000,
    }
}
fn observed(job: FixJob, observation: Observation, now: u64) -> FixJob {
    job.apply(
        Event::Observed {
            intent: job.expected_observation().unwrap(),
            observation,
        },
        now,
    )
    .unwrap()
    .job
}

#[test]
fn external_actor_removes_ten_seconds_after_completion_then_next_import_verifies_immediately() {
    let mut arr = HoldingArr {
        settings: HoldingSettings {
            watched_categories: vec!["arr-managed".into()],
            submission_category: "held".into(),
            can_choose_category_before_submission: true,
            can_manual_import_exact_file: true,
        },
        library_file: OLD,
        download: Some(FakeDownload {
            id: 5,
            file: FIRST,
            category: "held".into(),
            complete: true,
        }),
        calls: vec![],
    };
    let job = FixJob::new(
        1,
        2,
        OLD,
        Constraints::default(),
        Policy {
            max_grabs: 2,
            stage_timeout: 100,
            total_timeout: 1000,
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
    let first_grab = job.expected_observation().unwrap();
    let job = observed(job, Observation::GrabAccepted { download: 5 }, 2);
    let job = observed(
        job,
        Observation::DownloadCompleted {
            download: 5,
            file: FIRST,
        },
        30,
    );
    assert_eq!(job.state(), State::Downloaded);
    let mut actor = ExternalQueueActor {
        actor: QueueActor::NzbdavQueueRules,
        completed_at: 30,
        delay: 10,
        blocklisted: vec![],
    };
    assert_eq!(actor.tick(&mut arr, 39), None);
    let history = actor.tick(&mut arr, 40).unwrap();
    assert_eq!(actor.blocklisted, [5]);
    assert_eq!(arr.library_file, OLD);
    assert!(arr.calls.is_empty());
    let interference = QueueInterference::default().observe(5, history);
    assert_eq!(
        interference.route,
        QueueImportRoute::ImportImmediatelyThenVerify
    );
    assert_eq!(interference.attribution, Some(QueueActor::NzbdavQueueRules));
    let job = job
        .apply(
            Event::DownloadFailed {
                grab: first_grab,
                download: 5,
            },
            40,
        )
        .unwrap()
        .job;
    assert_eq!(job.state(), State::Rejected);
    assert!(
        job.apply(
            Event::SelectRelease {
                release: 3,
                access: access()
            },
            41
        )
        .is_err()
    );
    let job = job
        .apply(
            Event::SelectRelease {
                release: 4,
                access: access(),
            },
            41,
        )
        .unwrap()
        .job;
    let grab = job.expected_observation().unwrap();
    let job = observed(job, Observation::GrabAccepted { download: 6 }, 42);
    // Fallback deliberately permits independently configured arr handling. It
    // does not grant Pit Crew an unverified destructive ManualImport effect.
    arr.download = Some(FakeDownload {
        id: 6,
        file: REPLACEMENT,
        category: "arr-managed".into(),
        complete: true,
    });
    arr.completed_download_handling();
    let transition = job
        .apply(
            Event::AutoImported {
                grab,
                download: 6,
                file: arr.library_file,
            },
            43,
        )
        .unwrap();
    assert_eq!(transition.job.state(), State::Imported);
    assert_eq!(
        transition.intents[0].effect,
        Effect::DecodeImported { file: REPLACEMENT }
    );
    assert!(transition.job.apply(Event::QueueReady, 44).is_err());
    let job = observed(
        transition.job,
        Observation::ImportedDecoded {
            file: REPLACEMENT,
            good: true,
        },
        44,
    );
    let job = observed(
        job,
        Observation::TargetChecked {
            file: REPLACEMENT,
            plex_file: REPLACEMENT,
            constraints_satisfied: true,
        },
        45,
    );
    let done = job.apply(Event::QueueReady, 46).unwrap();
    assert_eq!(
        done.intents[0].effect,
        Effect::NotifyReady { file: REPLACEMENT }
    );
    assert_eq!(arr.calls, [ImportCall::AutoImport(REPLACEMENT)]);
    assert_eq!(actor.tick(&mut arr, 53), None);
    assert_eq!(arr.library_file, REPLACEMENT);
}
