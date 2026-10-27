use pitcrew_domain::{channels::*, delivery::*, *};
use pitcrew_engine::{delivery::Notifications, *};
use pitcrew_notify::*;
use pitcrew_store::{
    delivery::NotificationPayload,
    policy::{AdminAction, PolicyRole},
    *,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone)]
struct Sender {
    viewer: Arc<Mutex<Vec<OutgoingMessage>>>,
    admin: Arc<Mutex<Vec<AdminAlert>>>,
    outcome: DeliveryOutcome,
    error: bool,
    capabilities: ChannelCapabilities,
}
impl Default for Sender {
    fn default() -> Self {
        Self {
            viewer: Default::default(),
            admin: Default::default(),
            outcome: DeliveryOutcome::Confirmed,
            error: false,
            capabilities: ChannelCapabilities {
                buttons: false,
                rich_text: false,
                plain_text: true,
                max_text_chars: 100,
                proactive: ProactiveRule::UserMustMessageFirst,
                idempotent_delivery: false,
            },
        }
    }
}
impl Notifier for Sender {
    fn idempotent_delivery(&self) -> bool {
        false
    }
    fn send<'a>(&'a self, alert: &'a AdminAlert) -> NotifyFuture<'a> {
        self.admin.lock().unwrap().push(alert.clone());
        Box::pin(async move {
            if self.error {
                Err(NotifyError::Unavailable)
            } else {
                Ok(self.outcome)
            }
        })
    }
}
impl Channel for Sender {
    fn capabilities(&self) -> ChannelCapabilities {
        self.capabilities
    }
    fn receive(&mut self) -> ChannelFuture<'_, Option<IncomingMessage>> {
        panic!("no receive")
    }
    fn reply<'a>(
        &'a self,
        _: &'a IncomingMessage,
        _: &'a OutgoingMessage,
    ) -> ChannelFuture<'a, DeliveryOutcome> {
        panic!("no reply")
    }
    fn follow_up<'a>(
        &'a self,
        message: &'a OutgoingMessage,
        last: Option<u64>,
        now: u64,
    ) -> ChannelFuture<'a, DeliveryOutcome> {
        assert_eq!(last, Some(1));
        assert_eq!(now, 10);
        self.viewer.lock().unwrap().push(message.clone());
        Box::pin(async move {
            if self.error {
                Err(ChannelError::Unavailable)
            } else {
                Ok(self.outcome)
            }
        })
    }
}
async fn setup(ready: bool) -> (tempfile::TempDir, Store, Lease, Intent) {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).await.unwrap();
    store
        .create_job(
            &NewJob {
                id: 1,
                viewer: "viewer-a".into(),
                resource: "item-a".into(),
                library: 1,
                original: FileIdentity { id: 2, revision: 3 },
                constraints: Constraints::default(),
                policy: Policy {
                    max_grabs: 1,
                    stage_timeout: 300,
                    total_timeout: 1000,
                },
                timeouts: StageTimeouts {
                    download: 300,
                    import: 300,
                    decode: 300,
                    verify_retry: 180,
                },
                created_at: 0,
            },
            "create",
            BudgetLimits {
                global_hour: 4,
                global_day: 4,
                viewer_hour: 4,
                viewer_day: 4,
            },
        )
        .await
        .unwrap();
    let lease = store.acquire_lease(1, "worker", 1, 900).await.unwrap();
    if ready {
        store
            .apply_event(
                &lease,
                0,
                "grab",
                Event::SelectRelease {
                    release: 3,
                    access: Access::Granted {
                        library: 1,
                        expires_at: 900,
                    },
                },
                1,
            )
            .await
            .unwrap();
        let imported = FileIdentity { id: 4, revision: 5 };
        store
            .apply_event(
                &lease,
                1,
                "import",
                Event::AutoImported {
                    grab: IntentKey {
                        job: 1,
                        sequence: 1,
                    },
                    download: 6,
                    file: imported,
                },
                1,
            )
            .await
            .unwrap();
        store
            .apply_event(
                &lease,
                2,
                "decode",
                Event::Observed {
                    intent: IntentKey {
                        job: 1,
                        sequence: 2,
                    },
                    observation: Observation::ImportedDecoded {
                        file: imported,
                        good: true,
                    },
                },
                1,
            )
            .await
            .unwrap();
        store
            .apply_event(
                &lease,
                3,
                "target",
                Event::Observed {
                    intent: IntentKey {
                        job: 1,
                        sequence: 3,
                    },
                    observation: Observation::TargetChecked {
                        file: imported,
                        plex_file: imported,
                        constraints_satisfied: true,
                    },
                },
                1,
            )
            .await
            .unwrap();
        store
            .apply_event(&lease, 4, "ready", Event::QueueReady, 1)
            .await
            .unwrap();
    } else {
        store
            .apply_event(&lease, 0, "fail", Event::Fail(Failure::Cancelled), 1)
            .await
            .unwrap();
    }
    let intent = store
        .outbox(1)
        .await
        .unwrap()
        .last()
        .unwrap()
        .intent
        .clone();
    (temp, store, lease, intent)
}
fn viewer() -> NotificationPayload {
    NotificationPayload::Viewer {
        recipient: ChannelIdentity {
            channel_id: "chat-a".into(),
            platform_user_id: "account-a".into(),
        },
        conversation: "thread-a".into(),
        text: "Ready to watch.".into(),
        last_inbound: Some(1),
    }
}
fn adapters(store: Store, sender: Sender) -> Notifications {
    Notifications {
        store,
        channels: BTreeMap::from([(
            "chat-a".into(),
            Arc::new(sender.clone()) as Arc<dyn Channel>,
        )]),
        notifiers: BTreeMap::from([("admin-a".into(), Arc::new(sender) as Arc<dyn Notifier>)]),
        clock: Arc::new(|| 10),
    }
}
fn options() -> Options {
    Options {
        mode: Mode::Live,
        acknowledge_live_mutations: true,
        call_timeout: Duration::from_secs(1),
    }
}
fn key(intent: &Intent) -> IntentKey {
    IntentKey {
        job: intent.job,
        sequence: intent.sequence,
    }
}

#[tokio::test]
async fn ready_dispatch_preserves_payload_and_key_and_requires_claim() {
    let (_temp, store, lease, intent) = setup(true).await;
    let sender = Sender::default();
    let adapter = adapters(store.clone(), sender.clone());
    assert_eq!(adapter.read(&intent).await, Err(CallError::Unavailable));
    assert_eq!(adapter.write(&intent).await, Err(CallError::Unavailable));
    store
        .prepare_notification(key(&intent), &viewer())
        .await
        .unwrap();
    assert_eq!(adapter.write(&intent).await, Err(CallError::Unavailable));
    let worker = Worker::new(Some(store.clone()), adapter, options()).unwrap();
    assert_eq!(worker.drive(&lease, 10).await.unwrap(), 1);
    assert_eq!(worker.drive(&lease, 10).await.unwrap(), 0);
    assert_eq!(
        store.outbox(1).await.unwrap().last().unwrap().status,
        OutboxStatus::Completed
    );
    let sent = sender.viewer.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].key,
        DeliveryKey {
            job: 1,
            purpose: DeliveryPurpose::Ready,
            recipient: r#"{"channel_id":"chat-a","platform_user_id":"account-a"}"#.into()
        }
    );
    assert_eq!(
        sent[0].recipient,
        ChannelIdentity {
            channel_id: "chat-a".into(),
            platform_user_id: "account-a".into()
        }
    );
    assert_eq!(sent[0].conversation_id, "thread-a");
    assert_eq!(sent[0].text, "Ready to watch.");
}

#[tokio::test]
async fn failure_routes_once_to_admin_and_viewer() {
    let (_temp, store, lease, intent) = setup(false).await;
    store
        .prepare_notification(key(&intent), &viewer())
        .await
        .unwrap();
    store
        .prepare_notification(
            IntentKey {
                job: 1,
                sequence: 1,
            },
            &NotificationPayload::Admin {
                notifier: "admin-a".into(),
                title: "Repair failed".into(),
                body: "A review is needed.".into(),
            },
        )
        .await
        .unwrap();
    let sender = Sender::default();
    let worker = Worker::new(
        Some(store.clone()),
        adapters(store.clone(), sender.clone()),
        options(),
    )
    .unwrap();
    assert_eq!(worker.drive(&lease, 10).await.unwrap(), 2);
    assert_eq!(worker.drive(&lease, 10).await.unwrap(), 0);
    assert_eq!(
        sender.viewer.lock().unwrap()[0].key.purpose,
        DeliveryPurpose::ViewerFailure
    );
    assert_eq!(
        *sender.admin.lock().unwrap(),
        vec![AdminAlert {
            key: DeliveryKey {
                job: 1,
                purpose: DeliveryPurpose::AdminFailure,
                recipient: "admin-a".into()
            },
            title: "Repair failed".into(),
            body: "A review is needed.".into()
        }]
    );
}

#[tokio::test]
async fn lost_ready_receipt_never_autoresends_admin_resend_uses_same_message() {
    let (temp, store, lease, intent) = setup(true).await;
    store
        .prepare_notification(key(&intent), &viewer())
        .await
        .unwrap();
    let sender = Sender::default();
    let worker = Worker::new(
        Some(store.clone()),
        adapters(store.clone(), sender.clone()),
        options(),
    )
    .unwrap();
    let cut = Arc::new(tokio::sync::Notify::new());
    let signal = cut.clone();
    let driving = worker.drive_with(&lease, 10, move |boundary| {
        let signal = signal.clone();
        async move {
            if boundary == Boundary::CallAfter {
                signal.notify_one();
                std::future::pending::<()>().await;
            }
        }
    });
    tokio::time::timeout(Duration::from_secs(30), async {
        tokio::select! {
            _ = cut.notified() => {},
            result = driving => panic!("worker completed before cut: {result:?}"),
        }
    })
    .await
    .unwrap();
    assert_eq!(sender.viewer.lock().unwrap().len(), 1);
    assert_eq!(
        store.outbox(1).await.unwrap().last().unwrap().status,
        OutboxStatus::Started
    );
    let reopened = Store::open(temp.path().join("db")).await.unwrap();
    let worker = Worker::new(
        Some(reopened.clone()),
        adapters(reopened.clone(), sender.clone()),
        options(),
    )
    .unwrap();
    assert_eq!(worker.drive(&lease, 10).await.unwrap(), 0);
    assert_eq!(
        reopened.outbox(1).await.unwrap().last().unwrap().status,
        OutboxStatus::Uncertain
    );
    assert_eq!(sender.viewer.lock().unwrap().len(), 1);
    assert!(
        reopened
            .resend_notification(
                &AdminAction {
                    role: PolicyRole::Admin,
                    actor: "admin-a".into(),
                    note: "Delivery uncertain; explicitly resend".into()
                },
                key(&intent),
                "request-a",
                11
            )
            .await
            .unwrap()
    );
    assert_eq!(worker.drive(&lease, 11).await.unwrap(), 1);
    let sent = sender.viewer.lock().unwrap();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0], sent[1]);
}

#[tokio::test]
async fn negative_receipts_and_transport_errors_stay_uncertain_for_both_routes() {
    for outcome in [
        DeliveryOutcome::NotAccepted,
        DeliveryOutcome::Uncertain,
        DeliveryOutcome::PermanentFailure,
    ] {
        let (_temp, store, lease, intent) = setup(false).await;
        store
            .prepare_notification(key(&intent), &viewer())
            .await
            .unwrap();
        store
            .prepare_notification(
                IntentKey {
                    job: 1,
                    sequence: 1,
                },
                &NotificationPayload::Admin {
                    notifier: "admin-a".into(),
                    title: "Failure".into(),
                    body: "Review".into(),
                },
            )
            .await
            .unwrap();
        let sender = Sender {
            outcome,
            ..Sender::default()
        };
        let worker = Worker::new(
            Some(store.clone()),
            adapters(store.clone(), sender.clone()),
            options(),
        )
        .unwrap();
        worker.drive(&lease, 10).await.unwrap();
        worker.drive(&lease, 10).await.unwrap();
        assert!(
            store
                .outbox(1)
                .await
                .unwrap()
                .iter()
                .all(|r| r.status == OutboxStatus::Uncertain)
        );
        assert_eq!(sender.viewer.lock().unwrap().len(), 1);
        assert_eq!(sender.admin.lock().unwrap().len(), 1);
    }
    let (_temp, store, lease, intent) = setup(false).await;
    store
        .prepare_notification(key(&intent), &viewer())
        .await
        .unwrap();
    store
        .prepare_notification(
            IntentKey {
                job: 1,
                sequence: 1,
            },
            &NotificationPayload::Admin {
                notifier: "admin-a".into(),
                title: "Failure".into(),
                body: "Review".into(),
            },
        )
        .await
        .unwrap();
    let sender = Sender {
        error: true,
        ..Sender::default()
    };
    let worker = Worker::new(
        Some(store.clone()),
        adapters(store.clone(), sender),
        options(),
    )
    .unwrap();
    worker.drive(&lease, 10).await.unwrap();
    assert!(
        store
            .outbox(1)
            .await
            .unwrap()
            .iter()
            .all(|r| r.status == OutboxStatus::Uncertain)
    );
}

#[tokio::test]
async fn missing_routes_payload_or_permission_and_invalid_intents_send_nothing() {
    for case in 0..8 {
        let (_temp, store, lease, intent) = setup(true).await;
        let mut payload = viewer();
        if case == 0
            && let NotificationPayload::Viewer { last_inbound, .. } = &mut payload
        {
            *last_inbound = None;
        }
        if case != 5 {
            store
                .prepare_notification(key(&intent), &payload)
                .await
                .unwrap();
        }
        let mut sender = Sender::default();
        if case == 1 {
            sender.capabilities.plain_text = false;
        }
        if case == 2 {
            sender.capabilities.max_text_chars = 14;
        }
        // Exact length is accepted; only longer text is denied.
        if case == 3 {
            sender.capabilities.max_text_chars = 15;
        }
        let mut adapter = adapters(store.clone(), sender.clone());
        if case == 4 {
            adapter.channels.clear();
        }
        store
            .start_intent(&lease, key(&intent), 5, 10)
            .await
            .unwrap();
        let mut call = intent.clone();
        if case == 6 {
            call.effect = Effect::NotifyViewerFailure {
                failure: Failure::Cancelled,
            };
        }
        if case == 7 {
            call.effect = Effect::Grab {
                release: 3,
                reservation: 1,
            };
        }
        let result = adapter.write(&call).await;
        if case == 3 {
            assert_eq!(result, Ok(Outcome::Delivered));
        } else {
            assert_eq!(result, Err(CallError::Unavailable));
            assert!(sender.viewer.lock().unwrap().is_empty());
        }
    }
    let (_temp, store, lease, _) = setup(false).await;
    let intent = store.outbox(1).await.unwrap()[0].intent.clone();
    store
        .prepare_notification(
            key(&intent),
            &NotificationPayload::Admin {
                notifier: "absent".into(),
                title: "Failure".into(),
                body: "Review".into(),
            },
        )
        .await
        .unwrap();
    store
        .start_intent(&lease, key(&intent), 1, 10)
        .await
        .unwrap();
    assert_eq!(
        adapters(store, Sender::default()).write(&intent).await,
        Err(CallError::Unavailable)
    );
}

// A fake channel that persists its externally visible acceptance independently
// of the worker database, so process death cannot erase the observed send.
struct DiskChannel(std::path::PathBuf);
impl Channel for DiskChannel {
    fn capabilities(&self) -> ChannelCapabilities {
        Sender::default().capabilities
    }
    fn receive(&mut self) -> ChannelFuture<'_, Option<IncomingMessage>> {
        panic!("not intake")
    }
    fn reply<'a>(
        &'a self,
        _: &'a IncomingMessage,
        _: &'a OutgoingMessage,
    ) -> ChannelFuture<'a, DeliveryOutcome> {
        panic!("not reply")
    }
    fn follow_up<'a>(
        &'a self,
        message: &'a OutgoingMessage,
        _: Option<u64>,
        _: u64,
    ) -> ChannelFuture<'a, DeliveryOutcome> {
        use std::io::Write;
        let mut disk = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.0)
            .unwrap();
        writeln!(
            disk,
            "{}:{:?}:{}",
            message.key.job, message.key.purpose, message.text
        )
        .unwrap();
        disk.sync_all().unwrap();
        Box::pin(async { Ok(DeliveryOutcome::Confirmed) })
    }
}

#[tokio::test]
async fn notification_process_child() {
    let Ok(root) = std::env::var("PITCREW_NOTIFICATION_CRASH_FIXTURE") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let store = Store::open(root.join("db")).await.unwrap();
    let lease = store
        .acquire_lease(1, "crash-worker", 10, 30)
        .await
        .unwrap();
    let notifications = Notifications {
        store: store.clone(),
        channels: BTreeMap::from([(
            "chat-a".into(),
            Arc::new(DiskChannel(root.join("accepted"))) as Arc<dyn Channel>,
        )]),
        notifiers: BTreeMap::new(),
        clock: Arc::new(|| 10),
    };
    let worker = Worker::new(Some(store), notifications, options()).unwrap();
    worker
        .drive_with(&lease, 10, |boundary| {
            let marker = root.join("kill-now");
            async move {
                if boundary == Boundary::CallAfter {
                    std::fs::write(marker, b"remote accepted; receipt not committed").unwrap();
                    std::future::pending::<()>().await;
                }
            }
        })
        .await
        .unwrap();
    panic!("child did not stop at the receipt boundary");
}

#[tokio::test]
async fn killed_notification_executor_recovers_without_an_automatic_duplicate() {
    use std::process::{Command, Stdio};
    let (temp, store, lease, intent) = setup(true).await;
    store
        .prepare_notification(key(&intent), &viewer())
        .await
        .unwrap();
    store.release_lease(&lease, 2).await.unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "notification_process_child", "--nocapture"])
        .env("PITCREW_NOTIFICATION_CRASH_FIXTURE", temp.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !temp.path().join("kill-now").exists() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("notification child exited before cut: {status}");
        }
        if tokio::time::Instant::now() > deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("notification child did not reach cut");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
    let reopened = Store::open(temp.path().join("db")).await.unwrap();
    let lease = reopened
        .acquire_lease(1, "restarted-worker", 50, 100)
        .await
        .unwrap();
    let sender = Sender::default();
    let worker = Worker::new(
        Some(reopened.clone()),
        adapters(reopened.clone(), sender.clone()),
        options(),
    )
    .unwrap();
    worker.drive(&lease, 50).await.unwrap();
    assert_eq!(
        reopened.outbox(1).await.unwrap().last().unwrap().status,
        OutboxStatus::Uncertain
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("accepted"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert!(sender.viewer.lock().unwrap().is_empty());
}

fn fanout_viewers() -> NotificationPayload {
    use pitcrew_store::delivery::ViewerNotification;
    NotificationPayload::Viewers {
        recipients: ["account-a", "account-b"]
            .into_iter()
            .map(|account| ViewerNotification {
                recipient: ChannelIdentity {
                    channel_id: "chat-a".into(),
                    platform_user_id: account.into(),
                },
                conversation: format!("thread-{account}"),
                text: "Ready to watch.".into(),
                last_inbound: Some(1),
            })
            .collect(),
    }
}

#[tokio::test]
async fn fanout_delivers_each_recipient_once_with_stable_keys_and_copy() {
    let (_temp, store, lease, intent) = setup(true).await;
    store
        .prepare_notification(key(&intent), &fanout_viewers())
        .await
        .unwrap();
    let sender = Sender::default();
    let worker = Worker::new(
        Some(store.clone()),
        adapters(store.clone(), sender.clone()),
        options(),
    )
    .unwrap();
    assert_eq!(worker.drive(&lease, 10).await.unwrap(), 1);
    assert_eq!(worker.drive(&lease, 10).await.unwrap(), 0);
    assert_eq!(
        store.outbox(1).await.unwrap().last().unwrap().status,
        OutboxStatus::Completed
    );
    let messages = sender.viewer.lock().unwrap();
    assert_eq!(messages.len(), 2);
    for (message, account) in messages.iter().zip(["account-a", "account-b"]) {
        assert_eq!(
            message.recipient,
            ChannelIdentity {
                channel_id: "chat-a".into(),
                platform_user_id: account.into()
            }
        );
        assert_eq!(message.conversation_id, format!("thread-{account}"));
        assert_eq!(message.text, "Ready to watch.");
        assert_eq!(
            message.key,
            DeliveryKey {
                job: 1,
                purpose: DeliveryPurpose::Ready,
                recipient: serde_json::to_string(&message.recipient).unwrap()
            }
        );
    }
}

#[tokio::test]
async fn fanout_partial_delivery_restart_and_audited_resend_preserve_confirmed_recipient() {
    let (temp, store, lease, intent) = setup(true).await;
    let NotificationPayload::Viewers { mut recipients } = fanout_viewers() else {
        unreachable!()
    };
    recipients[1].recipient.channel_id = "chat-b".into();
    store
        .prepare_notification(key(&intent), &NotificationPayload::Viewers { recipients })
        .await
        .unwrap();
    let confirmed = Sender::default();
    let uncertain = Sender {
        outcome: DeliveryOutcome::Uncertain,
        ..Sender::default()
    };
    let mut notifications = adapters(store.clone(), confirmed.clone());
    notifications
        .channels
        .insert("chat-b".into(), Arc::new(uncertain.clone()));
    let worker = Worker::new(Some(store.clone()), notifications, options()).unwrap();
    assert_eq!(worker.drive(&lease, 10).await.unwrap(), 1);
    assert_eq!(confirmed.viewer.lock().unwrap().len(), 1);
    assert_eq!(uncertain.viewer.lock().unwrap().len(), 1);
    assert_eq!(
        store.outbox(1).await.unwrap().last().unwrap().status,
        OutboxStatus::Uncertain
    );
    let reopened = Store::open(temp.path().join("db")).await.unwrap();
    let recovered = Sender::default();
    let mut notifications = adapters(reopened.clone(), confirmed.clone());
    notifications
        .channels
        .insert("chat-b".into(), Arc::new(recovered.clone()));
    let worker = Worker::new(Some(reopened.clone()), notifications, options()).unwrap();
    assert_eq!(worker.drive(&lease, 10).await.unwrap(), 0);
    assert_eq!(recovered.viewer.lock().unwrap().len(), 0);
    assert!(
        reopened
            .resend_notification(
                &AdminAction {
                    actor: "admin-a".into(),
                    role: PolicyRole::Admin,
                    note: "Reviewed delivery history.".into()
                },
                key(&intent),
                "fanout-resend",
                10
            )
            .await
            .unwrap()
    );
    assert_eq!(worker.drive(&lease, 10).await.unwrap(), 1);
    assert_eq!(confirmed.viewer.lock().unwrap().len(), 1);
    assert_eq!(
        reopened.outbox(1).await.unwrap().last().unwrap().status,
        OutboxStatus::Completed
    );
    let retry = recovered.viewer.lock().unwrap();
    let first = uncertain.viewer.lock().unwrap();
    assert_eq!(retry.as_slice(), first.as_slice());
}

#[tokio::test]
async fn fanout_refuses_to_repeat_a_recipient_claim_after_interrupted_send() {
    let (_temp, store, lease, intent) = setup(true).await;
    store
        .prepare_notification(key(&intent), &fanout_viewers())
        .await
        .unwrap();
    assert!(
        store
            .start_intent(&lease, key(&intent), 5, 10)
            .await
            .unwrap()
    );
    store
        .claim_notification_recipient(key(&intent), 0)
        .await
        .unwrap();
    let sender = Sender::default();
    let notifications = adapters(store.clone(), sender.clone());
    assert_eq!(
        notifications.write(&intent).await.unwrap(),
        Outcome::Uncertain
    );
    assert_eq!(sender.viewer.lock().unwrap().len(), 1);
    assert_eq!(
        sender.viewer.lock().unwrap()[0].recipient.platform_user_id,
        "account-b"
    );
    assert_eq!(
        notifications.write(&intent).await.unwrap(),
        Outcome::Uncertain
    );
    assert_eq!(sender.viewer.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn fanout_channel_failures_remain_uncertain_and_do_not_block_other_recipients() {
    for variant in 0..8 {
        let (_temp, store, lease, intent) = setup(false).await;
        let NotificationPayload::Viewers { mut recipients } = fanout_viewers() else {
            unreachable!()
        };
        recipients[1].recipient.channel_id = "chat-b".into();
        store
            .prepare_notification(key(&intent), &NotificationPayload::Viewers { recipients })
            .await
            .unwrap();
        let mut failed = Sender::default();
        match variant {
            0 => failed.error = true,
            1 => failed.outcome = DeliveryOutcome::NotAccepted,
            2 => failed.outcome = DeliveryOutcome::PermanentFailure,
            3 => failed.capabilities.plain_text = false,
            4 => failed.capabilities.max_text_chars = 14,
            5 => failed.capabilities.proactive = ProactiveRule::Unsupported,
            6 => failed.outcome = DeliveryOutcome::Uncertain,
            _ => {}
        }
        let confirmed = Sender::default();
        let mut notifications = adapters(store.clone(), failed.clone());
        if variant == 7 {
            notifications.channels.remove("chat-a");
        }
        notifications
            .channels
            .insert("chat-b".into(), Arc::new(confirmed.clone()));
        assert!(
            store
                .start_intent(&lease, key(&intent), 1, 10)
                .await
                .unwrap()
        );
        assert_eq!(
            notifications.write(&intent).await.unwrap(),
            Outcome::Uncertain
        );
        assert_eq!(confirmed.viewer.lock().unwrap().len(), 1);
        assert_eq!(
            confirmed.viewer.lock().unwrap()[0].key.purpose,
            DeliveryPurpose::ViewerFailure
        );
        assert_eq!(
            failed.viewer.lock().unwrap().len(),
            usize::from(matches!(variant, 0..=2 | 6))
        );
        assert_eq!(
            notifications.write(&intent).await.unwrap(),
            Outcome::Uncertain
        );
        assert_eq!(confirmed.viewer.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn fanout_channel_text_limit_counts_unicode_and_accepts_exact_bound() {
    let (_temp, store, lease, intent) = setup(true).await;
    let NotificationPayload::Viewers { mut recipients } = fanout_viewers() else {
        unreachable!()
    };
    for recipient in &mut recipients {
        recipient.text = "✓✓".into();
    }
    store
        .prepare_notification(key(&intent), &NotificationPayload::Viewers { recipients })
        .await
        .unwrap();
    let mut sender = Sender::default();
    sender.capabilities.max_text_chars = 2;
    assert!(
        store
            .start_intent(&lease, key(&intent), 5, 10)
            .await
            .unwrap()
    );
    let notifications = adapters(store.clone(), sender.clone());
    assert_eq!(
        notifications.write(&intent).await.unwrap(),
        Outcome::Delivered
    );
    assert_eq!(sender.viewer.lock().unwrap().len(), 2);
}
