//! Full local repair through the actual intake, rules, bridge, worker and outbox.
//! Every upstream implementation in this file is an instrumented test fake.
use pitcrew_bot::server::{TestConfig, TestMessage, TestPrincipal, TestServer};
use pitcrew_bridge::{
    Authority, Bridge, Diagnosis, Options as BridgeOptions, ProgressState, ViewerRoute,
};
use pitcrew_deciders::{
    LocalEvidence,
    shadow::{DecisionRunner, JsonlSink},
};
use pitcrew_diagnosis::facts::{DecodeOutcome, EvidenceFacts, PlaybackFacts, ProbeOutcome};
use pitcrew_domain::{channels::*, delivery::DeliveryOutcome, *};
use pitcrew_drill::{Corpus, Metric, Ready, Target, score};
use pitcrew_engine::{CallFuture, Outcome, Upstream, Worker, delivery::Notifications};
use pitcrew_store::{BudgetLimits, Store, policy::IdentityFingerprint};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const ORIGINAL: FileIdentity = FileIdentity {
    id: 11,
    revision: 1,
};
const AGENT_ORIGINAL: FileIdentity = FileIdentity {
    id: 71,
    revision: 7001,
};
const DOWNLOAD: FileIdentity = FileIdentity {
    id: 21,
    revision: 2,
};
const IMPORTED: FileIdentity = FileIdentity {
    id: 31,
    revision: 3,
};

#[derive(Default)]
struct TestChannel(Mutex<Vec<OutgoingMessage>>);
impl Channel for TestChannel {
    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            buttons: false,
            rich_text: false,
            plain_text: true,
            max_text_chars: 4096,
            proactive: ProactiveRule::UserMustMessageFirst,
            idempotent_delivery: false,
        }
    }
    fn receive(&mut self) -> ChannelFuture<'_, Option<IncomingMessage>> {
        panic!("intake is exercised through TestServer")
    }
    fn reply<'a>(
        &'a self,
        _: &'a IncomingMessage,
        _: &'a OutgoingMessage,
    ) -> ChannelFuture<'a, DeliveryOutcome> {
        panic!("outbox follow-up required")
    }
    fn follow_up<'a>(
        &'a self,
        message: &'a OutgoingMessage,
        last: Option<u64>,
        _: u64,
    ) -> ChannelFuture<'a, DeliveryOutcome> {
        assert_eq!(last, Some(10));
        self.0.lock().unwrap().push(message.clone());
        Box::pin(async { Ok(DeliveryOutcome::Confirmed) })
    }
}
struct FakeMedia {
    delivery: Notifications,
    calls: Arc<Mutex<Vec<Effect>>>,
    source_decoded: Mutex<bool>,
    current: Arc<Mutex<FileIdentity>>,
}
impl Upstream for FakeMedia {
    fn read<'a>(&'a self, intent: &'a Intent) -> CallFuture<'a> {
        self.calls.lock().unwrap().push(intent.effect.clone());
        Box::pin(async move {
            let observation = match &intent.effect {
                Effect::DecodeDownload { file, .. } => {
                    assert_eq!(*self.current.lock().unwrap(), ORIGINAL);
                    assert_eq!(*file, DOWNLOAD);
                    *self.source_decoded.lock().unwrap() = true;
                    Observation::DownloadDecoded {
                        file: *file,
                        good: true,
                    }
                }
                Effect::DecodeImported { file } => {
                    assert_eq!(*file, IMPORTED);
                    assert_eq!(*self.current.lock().unwrap(), IMPORTED);
                    Observation::ImportedDecoded {
                        file: *file,
                        good: true,
                    }
                }
                Effect::CheckTarget { file, .. } => Observation::TargetChecked {
                    file: *file,
                    plex_file: *self.current.lock().unwrap(),
                    constraints_satisfied: true,
                },
                effect => panic!("unexpected fake read {effect:?}"),
            };
            Ok(Outcome::Observed(observation))
        })
    }
    fn write<'a>(&'a self, intent: &'a Intent) -> CallFuture<'a> {
        self.calls.lock().unwrap().push(intent.effect.clone());
        if matches!(intent.effect, Effect::NotifyReady { .. }) {
            return self.delivery.write(intent);
        }
        Box::pin(async move {
            let observation = match &intent.effect {
                Effect::Grab { reservation, .. } => {
                    assert_eq!(*reservation, 1);
                    assert_eq!(*self.current.lock().unwrap(), ORIGINAL);
                    Observation::GrabAccepted { download: 41 }
                }
                Effect::ImportVerifiedDownload {
                    download,
                    file,
                    original,
                } => {
                    assert!(
                        *self.source_decoded.lock().unwrap(),
                        "old copy must survive source verification"
                    );
                    assert_eq!((*download, *file, *original), (41, DOWNLOAD, ORIGINAL));
                    assert_eq!(*self.current.lock().unwrap(), ORIGINAL);
                    *self.current.lock().unwrap() = IMPORTED;
                    Observation::ImportAccepted
                }
                effect => panic!("unexpected fake mutation {effect:?}"),
            };
            Ok(Outcome::Observed(observation))
        })
    }
}

#[tokio::test]
async fn broken_episode_message_becomes_exact_verified_repair_and_one_fixed_reply() {
    let start = Instant::now();
    let corpus: Corpus =
        serde_json::from_str(include_str!("../../../drills/broken-episode.json")).unwrap();
    let case = &corpus.cases[0];
    let viewer = case.viewer.as_ref().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let intake_path = directory.path().join("intake.sqlite");
    let bearer = "synthetic-test-channel-bearer-no-live-access";
    let channel = TestServer::open(TestConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        database: intake_path.clone(),
        catalog: corpus.catalog.clone(),
        principals: vec![TestPrincipal {
            bearer_token: bearer.into(),
            viewer: viewer.clone(),
            expires_at: 1000,
        }],
    })
    .await
    .unwrap();
    let response = channel
        .message(
            bearer,
            TestMessage {
                message_id: "episode-message".into(),
                conversation_id: case.id.clone(),
                text: case.turns[0].text.clone(),
            },
            10,
        )
        .await
        .unwrap();
    assert_eq!(response.issues.len(), 1);
    assert_eq!(response.issues[0].item_id, "signal-episode");
    assert!(!response.reply.contains("fixed"));
    let journal = pitcrew_intake_store::Store::open(&intake_path)
        .await
        .unwrap();
    let reports = journal.reports(&viewer.account_id).await.unwrap();
    assert_eq!(reports.len(), 1);
    let report_id = reports[0].id;
    let evidence = EvidenceFacts {
        playback: PlaybackFacts {
            probe: ProbeOutcome::Video,
            decode: DecodeOutcome::Bad,
            ..Default::default()
        },
        ..Default::default()
    };
    let comparison = DecisionRunner::default()
        .run(
            &LocalEvidence {
                facts: evidence,
                viewer_text: case.turns[0].text.clone(),
                private_context: serde_json::Value::Null,
            },
            &mut JsonlSink(Vec::new()),
        )
        .await
        .unwrap();
    let store = Store::open(directory.path().join("engine.sqlite"))
        .await
        .unwrap();
    let registered = store
        .register_file(
            "synthetic-sonarr",
            "synthetic:episode:7:2",
            &IdentityFingerprint {
                arr_file_id: 11,
                size: 1_000_000,
                date_added: "2026-01-01T00:00:00Z".into(),
                quality: r#"{"quality":"WEBDL-1080p"}"#.into(),
                agent: AGENT_ORIGINAL,
            },
            10,
        )
        .await
        .unwrap();
    assert_eq!(registered, ORIGINAL);
    assert_ne!(registered, AGENT_ORIGINAL);
    let bridge = Bridge::open(
        &intake_path,
        store.clone(),
        BridgeOptions {
            policy: Policy {
                max_grabs: 1,
                stage_timeout: 100,
                total_timeout: 900,
            },
            timeouts: StageTimeouts {
                download: 100,
                import: 100,
                decode: 100,
                verify_retry: 180,
            },
            budgets: BudgetLimits {
                global_hour: 2,
                global_day: 2,
                viewer_hour: 2,
                viewer_day: 2,
            },
            cooldown_seconds: 60,
            evidence_max_age: 30,
        },
    )
    .await
    .unwrap();
    let access = Access::Granted {
        library: 7,
        expires_at: 1000,
    };
    let recipient = ChannelIdentity {
        channel_id: "test".into(),
        platform_user_id: viewer.account_id.clone(),
    };
    bridge
        .record_authority(
            report_id,
            &Authority {
                account: viewer.account_id.clone(),
                resource: "synthetic:episode:7:2".into(),
                library: 7,
                arr_instance_id: "synthetic-sonarr".into(),
                arr_item_id: 2,
                arr_file_id: 11,
                file: AGENT_ORIGINAL,
                observed_at: 10,
                access,
                probe: ProbeOutcome::Video,
                decode: DecodeOutcome::Bad,
                route: Some(ViewerRoute {
                    recipient: recipient.clone(),
                    conversation: case.id.clone(),
                    last_inbound: Some(10),
                }),
            },
        )
        .await
        .unwrap();
    let diagnosis = Diagnosis {
        report_id,
        decided_at: 10,
        decision: comparison.selected.decision,
        evidence_summary: evidence,
        resource: "synthetic:episode:7:2".into(),
        library_id: 7,
        arr_instance_id: "synthetic-sonarr".into(),
        arr_item_id: 2,
        arr_file_id: 11,
        agent_file: AGENT_ORIGINAL,
    };
    let progress = bridge.consume(&diagnosis, 10).await.unwrap();
    assert_eq!(progress.state, ProgressState::Working);
    let job = progress.job_id.unwrap();
    assert_eq!(
        store.load_job(job).await.unwrap().job.current_original(),
        registered
    );
    assert_eq!(
        bridge.consume(&diagnosis, 10).await.unwrap().job_id,
        Some(job)
    );
    let replies = Arc::new(TestChannel::default());
    let calls = Arc::new(Mutex::new(Vec::new()));
    let current = Arc::new(Mutex::new(ORIGINAL));
    let worker = Worker::new(
        Some(store.clone()),
        FakeMedia {
            delivery: Notifications {
                store: store.clone(),
                channels: BTreeMap::from([("test".into(), replies.clone() as Arc<dyn Channel>)]),
                notifiers: BTreeMap::new(),
                clock: Arc::new(|| 20),
            },
            calls: calls.clone(),
            source_decoded: Mutex::new(false),
            current: current.clone(),
        },
        pitcrew_engine::Options {
            mode: pitcrew_engine::Mode::Live,
            acknowledge_live_mutations: true,
            call_timeout: Duration::from_secs(1),
        },
    )
    .unwrap();
    let lease = store.acquire_lease(job, "drill", 10, 900).await.unwrap();
    assert!(
        worker
            .apply(&lease, "premature-ready", Event::QueueReady, 10)
            .await
            .is_err()
    );
    worker
        .apply(
            &lease,
            "select",
            Event::SelectRelease {
                release: 51,
                access,
            },
            11,
        )
        .await
        .unwrap();
    worker.drive(&lease, 11).await.unwrap();
    worker
        .apply(
            &lease,
            "download",
            Event::Observed {
                intent: IntentKey { job, sequence: 1 },
                observation: Observation::DownloadCompleted {
                    download: 41,
                    file: DOWNLOAD,
                },
            },
            12,
        )
        .await
        .unwrap();
    worker.drive(&lease, 12).await.unwrap();
    worker
        .apply(
            &lease,
            "request-import",
            Event::RequestImport { access },
            13,
        )
        .await
        .unwrap();
    worker.drive(&lease, 13).await.unwrap();
    worker
        .apply(
            &lease,
            "imported",
            Event::Observed {
                intent: IntentKey { job, sequence: 3 },
                observation: Observation::ImportCompleted {
                    download: 41,
                    file: IMPORTED,
                },
            },
            14,
        )
        .await
        .unwrap();
    worker.drive(&lease, 14).await.unwrap();
    assert!(replies.0.lock().unwrap().is_empty());
    assert_ne!(
        bridge
            .progress(report_id, &viewer.account_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        ProgressState::Fixed
    );
    worker.drive(&lease, 15).await.unwrap();
    assert_eq!(
        store.load_job(job).await.unwrap().job.state(),
        State::Verified
    );
    bridge.project(15).await.unwrap();
    assert_eq!(
        bridge
            .progress(report_id, &viewer.account_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        ProgressState::Fixed
    );
    worker
        .apply(&lease, "ready", Event::QueueReady, 16)
        .await
        .unwrap();
    assert_eq!(bridge.prepare_notifications(job, None).await.unwrap(), 1);
    assert_eq!(bridge.prepare_notifications(job, None).await.unwrap(), 0);
    worker.drive(&lease, 16).await.unwrap();
    worker.drive(&lease, 17).await.unwrap();
    assert_eq!(store.load_job(job).await.unwrap().job.state(), State::Done);
    let delivered = replies.0.lock().unwrap();
    assert_eq!(delivered.len(), 1);
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 6);
    let imported_decoded = calls
        .iter()
        .any(|effect| matches!(effect, Effect::DecodeImported { file } if *file == IMPORTED));
    assert!(imported_decoded);
    assert_eq!(
        calls
            .iter()
            .filter(|effect| matches!(effect, Effect::Grab { .. }))
            .count(),
        1
    );
    let item = &corpus.catalog[0];
    let observed = pitcrew_drill::Observation {
        case_id: case.id.clone(),
        turn: 0,
        outcome: serde_json::to_value(response.outcome)
            .unwrap()
            .as_str()
            .unwrap()
            .into(),
        targets: vec![Target {
            title: item.title.clone(),
            season: item.season,
            episode: item.episode,
            library: item.library_id.clone(),
            kinds: response.issues[0]
                .kinds
                .iter()
                .map(|kind| {
                    serde_json::to_value(kind)
                        .unwrap()
                        .as_str()
                        .unwrap()
                        .to_owned()
                })
                .collect(),
        }],
        suspicious: response.suspicious_input,
        reply: delivered[0].text.clone(),
        diagnosis: Some(
            serde_json::to_value(comparison.selected.decision.kind)
                .unwrap()
                .as_str()
                .unwrap()
                .into(),
        ),
        ready: Some(Ready {
            elapsed_ms: start.elapsed().as_millis().try_into().unwrap(),
            exact_file_verified: imported_decoded && *current.lock().unwrap() == IMPORTED,
        }),
        unsafe_actions: Some(vec![]),
    };
    let score = score(&corpus, &[observed]).unwrap();
    for metric in [
        &score.intake,
        &score.title,
        &score.episode,
        &score.library,
        &score.symptoms,
        &score.suspicious,
        &score.reply,
        &score.diagnosis,
        &score.ready,
        &score.safe,
    ] {
        assert_eq!(
            *metric,
            Metric {
                passed: 1,
                observed: 1,
                expected: 1
            }
        );
    }
}
