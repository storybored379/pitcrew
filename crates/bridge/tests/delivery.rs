//! Terminal routing uses real bridge consumption and the engine's verified
//! event transcript, with no upstream or platform access.
use pitcrew_bridge::{Authority, Bridge, Diagnosis, Options, ProgressState, ViewerRoute};
use pitcrew_diagnosis::{facts::*, rules::classify};
use pitcrew_domain::{channels::ChannelIdentity, *};
use pitcrew_domain::{channels::*, delivery::*};
use pitcrew_store::{BudgetLimits, Lease, Store, StoreError, delivery::NotificationPayload};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

struct ChannelFake {
    sends: AtomicUsize,
    outcome: DeliveryOutcome,
    allowed: bool,
}
struct LimitedChannel {
    base: ChannelFake,
    plain_text: bool,
    max_text_chars: u32,
}
impl Channel for LimitedChannel {
    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            plain_text: self.plain_text,
            max_text_chars: self.max_text_chars,
            ..self.base.capabilities()
        }
    }
    fn receive(&mut self) -> ChannelFuture<'_, Option<IncomingMessage>> {
        self.base.receive()
    }
    fn reply<'a>(
        &'a self,
        incoming: &'a IncomingMessage,
        outgoing: &'a OutgoingMessage,
    ) -> ChannelFuture<'a, DeliveryOutcome> {
        self.base.reply(incoming, outgoing)
    }
    fn follow_up<'a>(
        &'a self,
        outgoing: &'a OutgoingMessage,
        last_inbound: Option<u64>,
        now: u64,
    ) -> ChannelFuture<'a, DeliveryOutcome> {
        self.base.follow_up(outgoing, last_inbound, now)
    }
}
impl Channel for ChannelFake {
    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            buttons: false,
            rich_text: false,
            plain_text: true,
            max_text_chars: 2000,
            proactive: if self.allowed {
                ProactiveRule::Allowed
            } else {
                ProactiveRule::Unsupported
            },
            idempotent_delivery: false,
        }
    }
    fn receive(&mut self) -> ChannelFuture<'_, Option<IncomingMessage>> {
        Box::pin(async { Ok(None) })
    }
    fn reply<'a>(
        &'a self,
        _: &'a IncomingMessage,
        _: &'a OutgoingMessage,
    ) -> ChannelFuture<'a, DeliveryOutcome> {
        panic!("lifecycle must use proactive channel rules")
    }
    fn follow_up<'a>(
        &'a self,
        outgoing: &'a OutgoingMessage,
        last_inbound: Option<u64>,
        now: u64,
    ) -> ChannelFuture<'a, DeliveryOutcome> {
        Box::pin(async move {
            assert!(!outgoing.key.recipient.is_empty());
            assert_eq!(outgoing.recipient.platform_user_id, "user-a");
            assert_eq!(last_inbound, Some(10));
            assert!(now >= 10);
            self.sends.fetch_add(1, Ordering::SeqCst);
            Ok(self.outcome)
        })
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    bridge: Bridge,
    intake: pitcrew_intake_store::Store,
    engine: Store,
}
impl Fixture {
    async fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let intake_path = temp.path().join("intake.sqlite");
        let intake = pitcrew_intake_store::Store::open(&intake_path)
            .await
            .unwrap();
        let engine = Store::open(temp.path().join("engine.sqlite"))
            .await
            .unwrap();
        let bridge = Bridge::open(
            &intake_path,
            engine.clone(),
            Options {
                policy: Policy {
                    max_grabs: 2,
                    stage_timeout: 100,
                    total_timeout: 1000,
                },
                timeouts: StageTimeouts {
                    download: 100,
                    import: 100,
                    decode: 100,
                    verify_retry: 180,
                },
                budgets: BudgetLimits {
                    global_hour: 8,
                    global_day: 8,
                    viewer_hour: 4,
                    viewer_day: 4,
                },
                cooldown_seconds: 3600,
                evidence_max_age: 300,
            },
        )
        .await
        .unwrap();
        Self {
            _temp: temp,
            bridge,
            intake,
            engine,
        }
    }
    async fn report(&self, account: &str, message: &str, route: Option<ViewerRoute>) -> (i64, u64) {
        self.intake
            .persist_intake(
                "test",
                message,
                account,
                "inbound-thread",
                &serde_json::json!({"outcome":"report"}),
                10,
            )
            .await
            .unwrap();
        let report = self
            .intake
            .stored_intake("test", message, account)
            .await
            .unwrap()
            .unwrap()
            .id;
        let mut facts = EvidenceFacts::default();
        facts.playback.decode = DecodeOutcome::Bad;
        let diagnosis = Diagnosis {
            report_id: report,
            decided_at: 10,
            decision: classify(&facts),
            evidence_summary: facts,
            resource: "library-a:episode-a".into(),
            library_id: 1,
            arr_instance_id: "series-a".into(),
            arr_item_id: 2,
            arr_file_id: 3,
            agent_file: FileIdentity { id: 4, revision: 5 },
        };
        self.bridge
            .record_authority(
                report,
                &Authority {
                    account: account.into(),
                    resource: diagnosis.resource.clone(),
                    library: 1,
                    arr_instance_id: diagnosis.arr_instance_id.clone(),
                    arr_item_id: 2,
                    arr_file_id: 3,
                    file: diagnosis.agent_file,
                    observed_at: 10,
                    access: Access::Granted {
                        library: 1,
                        expires_at: 1000,
                    },
                    probe: ProbeOutcome::Unknown,
                    decode: DecodeOutcome::Bad,
                    route,
                },
            )
            .await
            .unwrap();
        self.engine
            .register_file(
                &diagnosis.arr_instance_id,
                &diagnosis.resource,
                &pitcrew_store::policy::IdentityFingerprint {
                    arr_file_id: diagnosis.arr_file_id,
                    size: 1024,
                    date_added: "2026-01-01T00:00:00Z".into(),
                    quality: "synthetic".into(),
                    agent: diagnosis.agent_file,
                },
                10,
            )
            .await
            .unwrap();
        let progress = self.bridge.consume(&diagnosis, 10).await.unwrap();
        (report, progress.job_id.unwrap())
    }
    async fn event(&self, lease: &Lease, event: Event) {
        let stored = self.engine.load_job(lease.job).await.unwrap();
        self.engine
            .apply_event(
                lease,
                stored.revision,
                &format!("event-{}", stored.revision),
                event,
                11,
            )
            .await
            .unwrap();
    }
    async fn verified(&self, job: u64) -> Lease {
        let lease = self
            .engine
            .acquire_lease(job, "test-worker", 11, 100)
            .await
            .unwrap();
        self.event(
            &lease,
            Event::SelectRelease {
                release: 7,
                access: Access::Granted {
                    library: 1,
                    expires_at: 1000,
                },
            },
        )
        .await;
        let imported = FileIdentity { id: 8, revision: 9 };
        self.event(
            &lease,
            Event::AutoImported {
                grab: IntentKey { job, sequence: 1 },
                download: 10,
                file: imported,
            },
        )
        .await;
        self.event(
            &lease,
            Event::Observed {
                intent: IntentKey { job, sequence: 2 },
                observation: Observation::ImportedDecoded {
                    file: imported,
                    good: true,
                },
            },
        )
        .await;
        self.event(
            &lease,
            Event::Observed {
                intent: IntentKey { job, sequence: 3 },
                observation: Observation::TargetChecked {
                    file: imported,
                    plex_file: imported,
                    constraints_satisfied: true,
                },
            },
        )
        .await;
        assert_eq!(
            self.engine.load_job(job).await.unwrap().job.state(),
            State::Verified
        );
        lease
    }
    async fn key(&self, job: u64, ready: bool, admin: bool) -> IntentKey {
        let row = self
            .engine
            .outbox(job)
            .await
            .unwrap()
            .into_iter()
            .find(|row| match row.intent.effect {
                Effect::NotifyReady { .. } => ready,
                Effect::NotifyAdmin { .. } => !ready && admin,
                Effect::NotifyViewerFailure { .. } => !ready && !admin,
                _ => false,
            })
            .unwrap();
        IntentKey {
            job,
            sequence: row.intent.sequence,
        }
    }
}
fn route(user: &str, conversation: &str) -> ViewerRoute {
    ViewerRoute {
        recipient: ChannelIdentity {
            channel_id: "channel-a".into(),
            platform_user_id: user.into(),
        },
        conversation: conversation.into(),
        last_inbound: Some(10),
    }
}

#[tokio::test]
async fn shared_verified_job_freezes_distinct_recipients_with_first_authenticated_conversation() {
    let f = Fixture::new().await;
    let (first, job) = f
        .report(
            "account-a",
            "message-a",
            Some(route("user-a", "first-thread")),
        )
        .await;
    assert_eq!(
        f.report(
            "account-a",
            "message-b",
            Some(route("user-a", "later-thread"))
        )
        .await
        .1,
        job
    );
    assert_eq!(
        f.report(
            "account-b",
            "message-c",
            Some(route("user-b", "other-thread"))
        )
        .await
        .1,
        job
    );
    assert_eq!(
        f.bridge
            .prepare_notifications(job, Some("admin-a"))
            .await
            .unwrap(),
        0
    );
    assert!(f.engine.outbox(job).await.unwrap().is_empty());
    let lease = f.verified(job).await;
    // Exact decode and target verification allow ready copy, but no delivery
    // exists until the reducer commits QueueReady.
    assert_eq!(
        f.bridge
            .prepare_notifications(job, Some("admin-a"))
            .await
            .unwrap(),
        0
    );
    f.event(&lease, Event::QueueReady).await;
    assert_eq!(
        f.bridge
            .prepare_notifications(job, Some("admin-a"))
            .await
            .unwrap(),
        1
    );
    let key = f.key(job, true, false).await;
    let frozen = f.engine.notification_payload(key).await.unwrap();
    let NotificationPayload::Viewers { recipients } = &frozen else {
        panic!("expected viewer fanout")
    };
    assert_eq!(recipients.len(), 2);
    assert_eq!(recipients[0].recipient, route("user-a", "").recipient);
    assert_eq!(recipients[0].conversation, "first-thread");
    assert_eq!(recipients[1].recipient, route("user-b", "").recipient);
    assert_eq!(recipients[1].conversation, "other-thread");
    for recipient in recipients {
        assert_eq!(recipient.last_inbound, Some(10));
        assert_eq!(
            recipient.text,
            "The new copy passed our playback check. Ready to watch!"
        );
    }
    // A later reporter sees the same job's verified progress but cannot rewrite
    // the immutable payload or cause another logical ready delivery.
    let (late, late_job) = f
        .report(
            "account-c",
            "message-d",
            Some(route("user-c", "late-thread")),
        )
        .await;
    assert_eq!(late_job, job);
    assert_eq!(
        f.bridge
            .prepare_notifications(job, Some("changed-admin"))
            .await
            .unwrap(),
        0
    );
    assert_eq!(f.engine.notification_payload(key).await.unwrap(), frozen);
    f.bridge.project(12).await.unwrap();
    for (report, account) in [(first, "account-a"), (late, "account-c")] {
        assert_eq!(
            f.bridge
                .progress(report, account)
                .await
                .unwrap()
                .unwrap()
                .state,
            ProgressState::Fixed
        );
    }
}

#[tokio::test]
async fn failed_job_prepares_admin_and_known_viewers_without_claiming_ready() {
    let f = Fixture::new().await;
    let (_, job) = f
        .report("account-a", "message-a", Some(route("user-a", "thread-a")))
        .await;
    assert_eq!(f.report("account-b", "message-b", None).await.1, job);
    let lease = f
        .engine
        .acquire_lease(job, "test-worker", 11, 100)
        .await
        .unwrap();
    f.event(&lease, Event::Fail(Failure::Cancelled)).await;
    assert_eq!(f.bridge.prepare_notifications(job, None).await.unwrap(), 1);
    let admin_key = f.key(job, false, true).await;
    assert!(matches!(
        f.engine.notification_payload(admin_key).await,
        Err(StoreError::Missing)
    ));
    assert_eq!(
        f.bridge
            .prepare_notifications(job, Some("admin-a"))
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        f.engine.notification_payload(admin_key).await.unwrap(),
        NotificationPayload::Admin {
            notifier: "admin-a".into(),
            title: "Repair needs review".into(),
            body: "A repair could not complete. Review the job and its evidence in Pit Crew."
                .into(),
        }
    );
    let NotificationPayload::Viewers { recipients } = f
        .engine
        .notification_payload(f.key(job, false, false).await)
        .await
        .unwrap()
    else {
        panic!("expected viewer fanout")
    };
    assert_eq!(recipients.len(), 1);
    assert_eq!(
        recipients[0].text,
        "This one needs the server owner to take a look. Your report has the details."
    );
    assert_eq!(recipients[0].recipient, route("user-a", "").recipient);
    assert_eq!(
        f.bridge
            .prepare_notifications(job, Some("different-admin"))
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn missing_or_invalid_routes_never_guess_a_viewer_destination() {
    for invalid in [false, true] {
        let f = Fixture::new().await;
        let (_, job) = f
            .report(
                "account-a",
                "message-a",
                invalid.then(|| route(" ", "thread-a")),
            )
            .await;
        let lease = f.verified(job).await;
        f.event(&lease, Event::QueueReady).await;
        let prepared = f.bridge.prepare_notifications(job, None).await;
        if invalid {
            assert!(prepared.is_err());
        } else {
            assert_eq!(prepared.unwrap(), 0);
        }
        assert!(matches!(
            f.engine
                .notification_payload(f.key(job, true, false).await)
                .await,
            Err(StoreError::Missing)
        ));
    }
}

#[tokio::test]
async fn missing_job_never_prepares_a_notification() {
    let f = Fixture::new().await;
    assert!(matches!(
        f.bridge.prepare_notifications(99, Some("admin-a")).await,
        Err(pitcrew_bridge::Error::Engine(StoreError::Missing))
    ));
}

#[tokio::test]
async fn working_commands_are_unique_immutable_and_uncertain_sends_never_retry() {
    for outcome in [
        DeliveryOutcome::Confirmed,
        DeliveryOutcome::Uncertain,
        DeliveryOutcome::NotAccepted,
        DeliveryOutcome::PermanentFailure,
    ] {
        let f = Fixture::new().await;
        let (_, job) = f
            .report("account-a", "message-a", Some(route("user-a", "thread-a")))
            .await;
        f.report(
            "account-a",
            "message-b",
            Some(route("user-a", "another-thread")),
        )
        .await;
        assert_eq!(
            f.bridge.prepare_lifecycle_notifications(11).await.unwrap(),
            1
        );
        for _ in 0..2 {
            assert_eq!(
                f.bridge.prepare_lifecycle_notifications(11).await.unwrap(),
                0
            );
        }
        let frozen = f.bridge.lifecycle_notifications().await.unwrap();
        assert_eq!(frozen.len(), 1);
        assert_eq!(frozen[0].job_id, Some(job));
        assert_eq!(frozen[0].purpose, DeliveryPurpose::Working);
        assert_eq!(frozen[0].payload.conversation, "thread-a");
        assert_eq!(
            frozen[0].payload.text,
            "We're checking your report. We'll update you when we have a verified result."
        );
        assert_eq!(
            f.bridge
                .dispatch_lifecycle(&BTreeMap::new(), 11, false)
                .await
                .unwrap(),
            0
        );
        let adapter = Arc::new(ChannelFake {
            sends: AtomicUsize::new(0),
            outcome,
            allowed: true,
        });
        let channels: BTreeMap<String, Arc<dyn Channel>> =
            BTreeMap::from([("channel-a".into(), adapter.clone() as Arc<dyn Channel>)]);
        let (first, second) = tokio::join!(
            f.bridge.dispatch_lifecycle(&channels, 11, false),
            f.bridge.dispatch_lifecycle(&channels, 11, false),
        );
        assert_eq!(first.unwrap() + second.unwrap(), 1);
        assert_eq!(
            f.bridge
                .dispatch_lifecycle(&channels, 12, false)
                .await
                .unwrap(),
            0
        );
        assert_eq!(adapter.sends.load(Ordering::SeqCst), 1);
        let saved = f.bridge.lifecycle_notifications().await.unwrap();
        assert_eq!(
            saved[0].state,
            if outcome == DeliveryOutcome::Confirmed {
                "confirmed"
            } else {
                "uncertain"
            }
        );
        assert_eq!(saved[0].payload, frozen[0].payload);
        assert_eq!(saved[0].key, frozen[0].key);
    }
}

#[tokio::test]
async fn lifecycle_shadow_and_channel_policy_never_call_a_write() {
    let f = Fixture::new().await;
    f.report("account-a", "message-a", Some(route("user-a", "thread-a")))
        .await;
    f.bridge.prepare_lifecycle_notifications(11).await.unwrap();
    let adapter = Arc::new(ChannelFake {
        sends: AtomicUsize::new(0),
        outcome: DeliveryOutcome::Confirmed,
        allowed: false,
    });
    let channels: BTreeMap<String, Arc<dyn Channel>> =
        BTreeMap::from([("channel-a".into(), adapter.clone() as Arc<dyn Channel>)]);
    assert_eq!(
        f.bridge
            .dispatch_lifecycle(&channels, 11, false)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        f.bridge.lifecycle_notifications().await.unwrap()[0].state,
        "pending"
    );
    assert_eq!(
        f.bridge
            .dispatch_lifecycle(&channels, 12, true)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        f.bridge.lifecycle_notifications().await.unwrap()[0].state,
        "shadowed"
    );
    assert_eq!(
        f.bridge
            .dispatch_lifecycle(&channels, 13, false)
            .await
            .unwrap(),
        0
    );
    assert_eq!(adapter.sends.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn late_terminal_join_gets_a_separate_immutable_followup_without_rewriting_fanout() {
    let f = Fixture::new().await;
    let (_, job) = f
        .report("account-a", "first", Some(route("user-a", "first-thread")))
        .await;
    f.bridge.prepare_lifecycle_notifications(11).await.unwrap();
    let lease = f.verified(job).await;
    for _ in 0..2 {
        f.bridge.prepare_lifecycle_notifications(11).await.unwrap();
    }
    // Verified is insufficient until a terminal engine command and immutable
    // recipient set exist. Pending working copy is already superseded.
    assert_eq!(f.bridge.lifecycle_notifications().await.unwrap().len(), 1);
    assert_eq!(
        f.bridge.lifecycle_notifications().await.unwrap()[0].state,
        "superseded"
    );
    f.event(&lease, Event::QueueReady).await;
    f.bridge.prepare_notifications(job, None).await.unwrap();
    let key = f.key(job, true, false).await;
    let terminal = f.engine.notification_payload(key).await.unwrap();
    f.report("account-b", "late", Some(route("user-b", "late-thread")))
        .await;
    f.report("account-b", "repeat", Some(route("user-b", "other-thread")))
        .await;
    f.report(
        "account-a",
        "same-first",
        Some(route("user-a", "another-first-thread")),
    )
    .await;
    let mut queued = 0;
    for _ in 0..3 {
        queued += f.bridge.prepare_lifecycle_notifications(12).await.unwrap();
    }
    assert_eq!(queued, 1);
    let items = f.bridge.lifecycle_notifications().await.unwrap();
    assert_eq!(items.len(), 2);
    let followup = items
        .iter()
        .find(|n| n.purpose == DeliveryPurpose::Ready)
        .unwrap();
    assert_eq!(followup.payload.recipient.platform_user_id, "user-b");
    assert_eq!(followup.payload.conversation, "late-thread");
    assert_eq!(
        followup.payload.text,
        "The new copy passed our playback check. Ready to watch!"
    );
    assert_eq!(followup.state, "pending");
    assert_eq!(f.engine.notification_payload(key).await.unwrap(), terminal);
}

#[tokio::test]
async fn authority_revocation_blocks_pending_lifecycle_dispatch() {
    let f = Fixture::new().await;
    let (report, _) = f
        .report("account-a", "a", Some(route("user-a", "thread")))
        .await;
    f.bridge.prepare_lifecycle_notifications(11).await.unwrap();
    f.bridge.clear_authority(report).await.unwrap();
    assert_eq!(
        f.bridge
            .dispatch_lifecycle(&BTreeMap::new(), 12, true)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        f.bridge.lifecycle_notifications().await.unwrap()[0].state,
        "pending"
    );
}

#[tokio::test]
async fn unavailable_lifecycle_commands_do_not_starve_later_deliveries() {
    let f = Fixture::new().await;
    let (report, job) = f
        .report("account-a", "a", Some(route("user-a", "thread-a")))
        .await;
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(f._temp.path().join("intake.sqlite")),
        )
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    for index in 0..129 {
        let payload = pitcrew_store::delivery::ViewerNotification {
            recipient: route("user-a", "thread-a").recipient,
            conversation: "thread-a".into(),
            last_inbound: Some(10),
            text: if index < 128 {
                "x".repeat(2001)
            } else {
                "Valid follow-up".into()
            },
        };
        sqlx::query("INSERT INTO bridge_lifecycle(key,report_id,job_id,purpose,payload,created_at,updated_at) VALUES (?,?,?,?,?,10,10)")
            .bind(format!("message-{index:03}")).bind(report).bind(job as i64).bind(serde_json::to_string(&DeliveryPurpose::Working).unwrap())
            .bind(serde_json::to_string(&payload).unwrap()).execute(&mut *tx).await.unwrap();
    }
    tx.commit().await.unwrap();
    assert!(
        sqlx::query("UPDATE bridge_lifecycle SET payload='{}' WHERE key='message-000'")
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM bridge_lifecycle WHERE key='message-001'")
            .execute(&pool)
            .await
            .is_err()
    );
    let adapter = Arc::new(ChannelFake {
        sends: AtomicUsize::new(0),
        outcome: DeliveryOutcome::Confirmed,
        allowed: true,
    });
    let channels: BTreeMap<String, Arc<dyn Channel>> =
        BTreeMap::from([("channel-a".into(), adapter.clone() as Arc<dyn Channel>)]);
    assert_eq!(
        f.bridge
            .dispatch_lifecycle(&channels, 11, false)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        f.bridge
            .dispatch_lifecycle(&channels, 12, false)
            .await
            .unwrap(),
        1
    );
    assert_eq!(adapter.sends.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn lifecycle_routes_reject_future_revoked_expired_or_redirected_authority() {
    for case in 0..8 {
        let f = Fixture::new().await;
        let (report, _) = f
            .report("account-a", "a", Some(route("user-a", "thread-a")))
            .await;
        f.bridge.prepare_lifecycle_notifications(10).await.unwrap();
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(f._temp.path().join("intake.sqlite")),
            )
            .await
            .unwrap();
        let json: String =
            sqlx::query_scalar("SELECT authority FROM bridge_authority WHERE report_id=?")
                .bind(report)
                .fetch_one(&pool)
                .await
                .unwrap();
        let mut authority: Authority = serde_json::from_str(&json).unwrap();
        match case {
            0 => authority.observed_at = 12,
            1 => authority.access = Access::Unknown,
            2 => {
                authority.access = Access::Granted {
                    library: 2,
                    expires_at: 1000,
                }
            }
            3 => {
                authority.access = Access::Granted {
                    library: 1,
                    expires_at: 11,
                }
            }
            4 => authority.route.as_mut().unwrap().recipient.channel_id = " ".into(),
            5 => authority.route.as_mut().unwrap().recipient.platform_user_id = " ".into(),
            6 => authority.route.as_mut().unwrap().conversation = " ".into(),
            7 => {
                authority.route.as_mut().unwrap().recipient.platform_user_id =
                    "different-user".into()
            }
            _ => unreachable!(),
        }
        f.bridge.record_authority(report, &authority).await.unwrap();
        assert_eq!(
            f.bridge
                .dispatch_lifecycle(&BTreeMap::new(), 11, true)
                .await
                .unwrap(),
            0,
            "case {case}"
        );
        assert_eq!(
            f.bridge.lifecycle_notifications().await.unwrap()[0].state,
            "pending"
        );
    }
}

#[tokio::test]
async fn lifecycle_preparation_refuses_blank_channel_sender_or_conversation() {
    for field in ["channel", "sender", "conversation"] {
        let f = Fixture::new().await;
        let mut destination = route("user-a", "thread-a");
        match field {
            "channel" => destination.recipient.channel_id = " \t".into(),
            "sender" => destination.recipient.platform_user_id = " \t".into(),
            "conversation" => destination.conversation = " \t".into(),
            _ => unreachable!(),
        }
        f.report("account-a", "invalid-route", Some(destination))
            .await;
        assert_eq!(
            f.bridge.prepare_lifecycle_notifications(10).await.unwrap(),
            0
        );
        assert!(f.bridge.lifecycle_notifications().await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn lifecycle_delivery_accepts_exact_channel_text_limit_and_requires_plain_text() {
    for (plain_text, shorten, expected) in [(true, false, 1), (true, true, 0), (false, false, 0)] {
        let f = Fixture::new().await;
        f.report("account-a", "working", Some(route("user-a", "thread-a")))
            .await;
        f.bridge.prepare_lifecycle_notifications(10).await.unwrap();
        let text_length = f.bridge.lifecycle_notifications().await.unwrap()[0]
            .payload
            .text
            .chars()
            .count() as u32;
        let adapter = Arc::new(LimitedChannel {
            base: ChannelFake {
                sends: AtomicUsize::new(0),
                outcome: DeliveryOutcome::Confirmed,
                allowed: true,
            },
            plain_text,
            max_text_chars: if shorten {
                text_length - 1
            } else {
                text_length
            },
        });
        let channels: BTreeMap<String, Arc<dyn Channel>> =
            BTreeMap::from([("channel-a".into(), adapter.clone() as Arc<dyn Channel>)]);
        assert_eq!(
            f.bridge
                .dispatch_lifecycle(&channels, 11, false)
                .await
                .unwrap(),
            expected
        );
        assert_eq!(adapter.base.sends.load(Ordering::SeqCst), expected);
        assert_eq!(
            f.bridge.lifecycle_notifications().await.unwrap()[0].state,
            if expected == 1 {
                "confirmed"
            } else {
                "pending"
            }
        );
    }
}

#[tokio::test]
async fn dispatch_supersedes_stale_working_copy_without_a_preparation_pass() {
    let f = Fixture::new().await;
    let (_, job) = f
        .report("account-a", "working", Some(route("user-a", "thread-a")))
        .await;
    f.bridge.prepare_lifecycle_notifications(10).await.unwrap();
    f.verified(job).await;
    assert_eq!(
        f.bridge
            .dispatch_lifecycle(&BTreeMap::new(), 11, true)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        f.bridge.lifecycle_notifications().await.unwrap()[0].state,
        "superseded"
    );
}

#[tokio::test]
async fn single_recipient_terminal_payloads_require_preparation_and_exclude_only_that_recipient() {
    let f = Fixture::new().await;
    let (_, job) = f
        .report("account-a", "first", Some(route("user-a", "first-thread")))
        .await;
    let lease = f.verified(job).await;
    f.event(&lease, Event::QueueReady).await;
    let key = f.key(job, true, false).await;
    // QueueReady alone cannot identify which viewers are already covered.
    assert_eq!(
        f.bridge.prepare_lifecycle_notifications(11).await.unwrap(),
        0
    );
    assert!(f.bridge.lifecycle_notifications().await.unwrap().is_empty());
    let frozen = NotificationPayload::Viewer {
        recipient: route("user-a", "first-thread").recipient,
        conversation: "first-thread".into(),
        text: f.bridge.reply_for_job(job).await.unwrap(),
        last_inbound: Some(10),
    };
    f.engine.prepare_notification(key, &frozen).await.unwrap();
    f.report("account-b", "late", Some(route("user-b", "late-thread")))
        .await;
    f.report("account-a", "repeat", Some(route("user-a", "other-thread")))
        .await;
    let mut queued = 0;
    for _ in 0..3 {
        queued += f.bridge.prepare_lifecycle_notifications(12).await.unwrap();
    }
    assert_eq!(queued, 1);
    let commands = f.bridge.lifecycle_notifications().await.unwrap();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].purpose, DeliveryPurpose::Ready);
    assert_eq!(commands[0].payload.recipient.platform_user_id, "user-b");
    assert_eq!(commands[0].payload.conversation, "late-thread");
    assert_eq!(f.engine.notification_payload(key).await.unwrap(), frozen);
}

#[tokio::test]
async fn a_late_report_on_a_failed_job_queues_only_an_honest_failure_followup() {
    let f = Fixture::new().await;
    let (_, job) = f
        .report("account-a", "first", Some(route("user-a", "first-thread")))
        .await;
    let lease = f
        .engine
        .acquire_lease(job, "test-worker", 11, 100)
        .await
        .unwrap();
    f.event(&lease, Event::Fail(Failure::Cancelled)).await;
    f.bridge.prepare_notifications(job, None).await.unwrap();
    let key = f.key(job, false, false).await;
    let frozen = f.engine.notification_payload(key).await.unwrap();
    f.report("account-b", "late", Some(route("user-b", "late-thread")))
        .await;
    assert_eq!(
        f.bridge.prepare_lifecycle_notifications(12).await.unwrap(),
        1
    );
    let commands = f.bridge.lifecycle_notifications().await.unwrap();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].purpose, DeliveryPurpose::ViewerFailure);
    assert_eq!(commands[0].payload.recipient.platform_user_id, "user-b");
    assert_eq!(
        commands[0].payload.text,
        "This one needs the server owner to take a look. Your report has the details."
    );
    assert_eq!(f.engine.notification_payload(key).await.unwrap(), frozen);
}

#[tokio::test]
async fn forged_terminal_outbox_rows_cannot_override_replayed_job_state() {
    let f = Fixture::new().await;
    let (_, job) = f
        .report("account-a", "message-a", Some(route("user-a", "thread-a")))
        .await;
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(f._temp.path().join("engine.sqlite")),
        )
        .await
        .unwrap();
    // Model stale/corrupt projected rows while the authoritative event journal
    // still has no imported bytes or terminal outcome.
    for (index, effect) in [
        Effect::NotifyReady {
            file: FileIdentity { id: 8, revision: 9 },
        },
        Effect::NotifyAdmin {
            failure: Failure::Cancelled,
        },
        Effect::NotifyViewerFailure {
            failure: Failure::Cancelled,
        },
    ]
    .into_iter()
    .enumerate()
    {
        let intent = Intent {
            job,
            sequence: index as u64 + 1,
            effect,
        };
        sqlx::query("INSERT INTO outbox(job,sequence,intent) VALUES (?,?,?)")
            .bind(job as i64)
            .bind(intent.sequence as i64)
            .bind(serde_json::to_string(&intent).unwrap())
            .execute(&pool)
            .await
            .unwrap();
    }
    assert_eq!(
        f.bridge
            .prepare_notifications(job, Some("admin-a"))
            .await
            .unwrap(),
        0
    );
    for sequence in 1..=3 {
        assert!(matches!(
            f.engine
                .notification_payload(IntentKey { job, sequence })
                .await,
            Err(StoreError::Missing)
        ));
    }
}
