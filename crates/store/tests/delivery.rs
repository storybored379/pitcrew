use pitcrew_domain::{channels::ChannelIdentity, *};
use pitcrew_store::{
    delivery::NotificationPayload,
    policy::{AdminAction, PolicyRole},
    *,
};

fn payload() -> NotificationPayload {
    NotificationPayload::Viewer {
        recipient: ChannelIdentity {
            channel_id: "channel-a".into(),
            platform_user_id: "person-a".into(),
        },
        conversation: "conversation-a".into(),
        text: "The repair needs an administrator.".into(),
        last_inbound: Some(1),
    }
}
fn admin() -> AdminAction {
    AdminAction {
        actor: "admin-a".into(),
        role: PolicyRole::Admin,
        note: "Checked delivery history; resend requested.".into(),
    }
}
async fn setup() -> (tempfile::TempDir, Store, Lease, IntentKey) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("journal")).await.unwrap();
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
                    total_timeout: 900,
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
                global_hour: 10,
                global_day: 10,
                viewer_hour: 10,
                viewer_day: 10,
            },
        )
        .await
        .unwrap();
    let lease = store.acquire_lease(1, "worker", 1, 900).await.unwrap();
    store
        .apply_event(&lease, 0, "fail", Event::Fail(Failure::Cancelled), 1)
        .await
        .unwrap();
    (
        dir,
        store,
        lease,
        IntentKey {
            job: 1,
            sequence: 2,
        },
    )
}

#[tokio::test]
async fn immutable_payloads_are_bound_to_notification_kind_and_survive_reopen() {
    let (dir, store, _, key) = setup().await;
    assert!(matches!(
        store.notification_payload(key).await,
        Err(StoreError::Missing)
    ));
    assert!(matches!(
        store
            .prepare_notification(IntentKey { job: 9, ..key }, &payload())
            .await,
        Err(StoreError::Missing)
    ));
    let mut invalid = Vec::new();
    if let NotificationPayload::Viewer {
        recipient,
        conversation,
        text,
        ..
    } = payload()
    {
        invalid.push(NotificationPayload::Viewer {
            recipient: ChannelIdentity {
                channel_id: " ".into(),
                ..recipient.clone()
            },
            conversation: conversation.clone(),
            text: text.clone(),
            last_inbound: None,
        });
        invalid.push(NotificationPayload::Viewer {
            recipient: ChannelIdentity {
                platform_user_id: " ".into(),
                ..recipient.clone()
            },
            conversation: conversation.clone(),
            text: text.clone(),
            last_inbound: None,
        });
        invalid.push(NotificationPayload::Viewer {
            recipient: recipient.clone(),
            conversation: " ".into(),
            text: text.clone(),
            last_inbound: None,
        });
        invalid.push(NotificationPayload::Viewer {
            recipient,
            conversation,
            text: " ".into(),
            last_inbound: None,
        });
    }
    for bad in invalid {
        assert!(matches!(
            store.prepare_notification(key, &bad).await,
            Err(StoreError::InvalidInput)
        ));
    }
    let admin_key = IntentKey { sequence: 1, ..key };
    assert!(matches!(
        store.prepare_notification(admin_key, &payload()).await,
        Err(StoreError::InvalidInput)
    ));
    for (notifier, title, body) in [
        (" ", "title", "body"),
        ("notify", " ", "body"),
        ("notify", "title", " "),
    ] {
        assert!(matches!(
            store
                .prepare_notification(
                    admin_key,
                    &NotificationPayload::Admin {
                        notifier: notifier.into(),
                        title: title.into(),
                        body: body.into()
                    }
                )
                .await,
            Err(StoreError::InvalidInput)
        ));
    }
    let notice = NotificationPayload::Admin {
        notifier: "notify".into(),
        title: "Repair failed".into(),
        body: "Review required".into(),
    };
    assert!(matches!(
        store.prepare_notification(key, &notice).await,
        Err(StoreError::InvalidInput)
    ));
    store
        .prepare_notification(admin_key, &notice)
        .await
        .unwrap();
    store.prepare_notification(key, &payload()).await.unwrap();
    store.prepare_notification(key, &payload()).await.unwrap();
    let reopened = Store::open(dir.path().join("journal")).await.unwrap();
    assert_eq!(reopened.notification_payload(key).await.unwrap(), payload());
    let mut changed = payload();
    if let NotificationPayload::Viewer { text, .. } = &mut changed {
        *text = "different".into();
    }
    assert!(matches!(
        reopened.prepare_notification(key, &changed).await,
        Err(StoreError::IdempotencyConflict)
    ));
}

#[tokio::test]
async fn admin_resend_is_atomic_audited_idempotent_and_retains_same_row() {
    let (dir, store, lease, key) = setup().await;
    store.prepare_notification(key, &payload()).await.unwrap();
    for role in [PolicyRole::Viewer, PolicyRole::Unknown] {
        assert!(matches!(
            store
                .resend_notification(&AdminAction { role, ..admin() }, key, "r", 2)
                .await,
            Err(StoreError::AdminRequired)
        ));
    }
    assert!(matches!(
        store.resend_notification(&admin(), key, " ", 2).await,
        Err(StoreError::InvalidInput)
    ));
    assert!(matches!(
        store.resend_notification(&admin(), key, "r", 2).await,
        Err(StoreError::InvalidInput)
    ));
    assert!(store.start_intent(&lease, key, 1, 2).await.unwrap());
    assert!(matches!(
        store.resend_notification(&admin(), key, "r", 2).await,
        Err(StoreError::InvalidInput)
    ));
    store
        .finish_intent(&lease, key, OutboxStatus::Uncertain, 2)
        .await
        .unwrap();
    let a = store.clone();
    let b = store.clone();
    let left = async { a.resend_notification(&admin(), key, "r", 3).await.unwrap() };
    let right = async { b.resend_notification(&admin(), key, "r", 3).await.unwrap() };
    let (one, two) = tokio::join!(left, right);
    assert_ne!(one, two);
    let rows = store.outbox(1).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].status, OutboxStatus::Pending);
    assert_eq!(rows[1].fence, None);
    assert!(store.start_intent(&lease, key, 1, 4).await.unwrap());
    store
        .finish_intent(&lease, key, OutboxStatus::Uncertain, 4)
        .await
        .unwrap();
    let reopened = Store::open(dir.path().join("journal")).await.unwrap();
    assert!(
        !reopened
            .resend_notification(&admin(), key, "r", 5)
            .await
            .unwrap()
    );
    assert_eq!(
        reopened.outbox(1).await.unwrap()[1].status,
        OutboxStatus::Uncertain
    );
    for (authority, changed_key) in [
        (
            AdminAction {
                actor: "admin-b".into(),
                ..admin()
            },
            key,
        ),
        (
            AdminAction {
                note: "different".into(),
                ..admin()
            },
            key,
        ),
        (admin(), IntentKey { job: 2, ..key }),
        (admin(), IntentKey { sequence: 1, ..key }),
    ] {
        assert!(matches!(
            reopened
                .resend_notification(&authority, changed_key, "r", 5)
                .await,
            Err(StoreError::IdempotencyConflict)
        ));
    }
    assert!(
        reopened
            .resend_notification(&admin(), key, "r2", 6)
            .await
            .unwrap()
    );
    assert_eq!(reopened.notification_payload(key).await.unwrap(), payload());
    store
        .finish_intent(&lease, key, OutboxStatus::Completed, 7)
        .await
        .unwrap();
    assert!(matches!(
        store.resend_notification(&admin(), key, "r3", 8).await,
        Err(StoreError::InvalidInput)
    ));
    // No payload: a non-notification or unprepared message cannot be retried.
    let missing_payload = IntentKey { sequence: 1, ..key };
    store
        .finish_intent(&lease, missing_payload, OutboxStatus::Uncertain, 8)
        .await
        .unwrap();
    assert!(matches!(
        store
            .resend_notification(&admin(), missing_payload, "r4", 9)
            .await,
        Err(StoreError::InvalidInput)
    ));
    let connection =
        sqlx::SqlitePool::connect(&format!("sqlite:{}", dir.path().join("journal").display()))
            .await
            .unwrap();
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM policy_audit WHERE action='notification_resend'")
            .fetch_one(&connection)
            .await
            .unwrap();
    assert_eq!(count, 2);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM notification_resends")
        .fetch_one(&connection)
        .await
        .unwrap();
    assert_eq!(count, 2);
    assert!(
        sqlx::query("UPDATE notification_payloads SET payload='{}'")
            .execute(&connection)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM notification_payloads")
            .execute(&connection)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM notification_resends")
            .execute(&connection)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE notification_resends SET note='other'")
            .execute(&connection)
            .await
            .is_err()
    );
}

fn fanout() -> NotificationPayload {
    use pitcrew_store::delivery::ViewerNotification;
    NotificationPayload::Viewers {
        recipients: vec![ViewerNotification {
            recipient: ChannelIdentity {
                channel_id: "channel-a".into(),
                platform_user_id: "person-a".into(),
            },
            conversation: "conversation-a".into(),
            text: "The repair needs an administrator.".into(),
            last_inbound: Some(1),
        }],
    }
}

#[tokio::test]
async fn fanout_is_immutable_nonempty_valid_and_deduplicated() {
    let (dir, store, _, key) = setup().await;
    let NotificationPayload::Viewers { recipients } = fanout() else {
        unreachable!()
    };
    let first = recipients[0].clone();
    let mut other_conversation = first.clone();
    other_conversation.conversation = "second-conversation".into();
    let mut invalid = vec![
        Vec::new(),
        vec![first.clone(), first.clone()],
        vec![first.clone(), other_conversation],
    ];
    for field in 0..4 {
        let mut recipient = first.clone();
        match field {
            0 => recipient.recipient.channel_id = " ".into(),
            1 => recipient.recipient.platform_user_id = " ".into(),
            2 => recipient.conversation = " ".into(),
            _ => recipient.text = " ".into(),
        }
        invalid.push(vec![recipient]);
    }
    for recipients in invalid {
        assert!(matches!(
            store
                .prepare_notification(key, &NotificationPayload::Viewers { recipients })
                .await,
            Err(StoreError::InvalidInput)
        ));
    }
    assert!(matches!(
        store
            .prepare_notification(IntentKey { sequence: 1, ..key }, &fanout())
            .await,
        Err(StoreError::InvalidInput)
    ));
    store.prepare_notification(key, &fanout()).await.unwrap();
    store.prepare_notification(key, &fanout()).await.unwrap();
    let reopened = Store::open(dir.path().join("journal")).await.unwrap();
    assert_eq!(reopened.notification_payload(key).await.unwrap(), fanout());
    let mut changed = first;
    changed.text = "Changed".into();
    assert!(matches!(
        reopened
            .prepare_notification(
                key,
                &NotificationPayload::Viewers {
                    recipients: vec![changed]
                }
            )
            .await,
        Err(StoreError::IdempotencyConflict)
    ));
}

#[tokio::test]
async fn recipient_claims_survive_restart_and_only_audited_resend_reopens_uncertain() {
    use pitcrew_store::delivery::RecipientDispatch;
    let (dir, store, lease, key) = setup().await;
    let NotificationPayload::Viewers { mut recipients } = fanout() else {
        unreachable!()
    };
    let mut second = recipients[0].clone();
    second.recipient.platform_user_id = "person-b".into();
    recipients.push(second);
    store
        .prepare_notification(key, &NotificationPayload::Viewers { recipients })
        .await
        .unwrap();
    assert!(matches!(
        store.claim_notification_recipient(key, 0).await,
        Err(StoreError::InvalidInput)
    ));
    assert!(store.start_intent(&lease, key, 1, 2).await.unwrap());
    assert!(matches!(
        store.claim_notification_recipient(key, 2).await,
        Err(StoreError::InvalidInput)
    ));
    assert!(matches!(
        store.claim_notification_recipient(key, u64::MAX).await,
        Err(StoreError::InvalidInput)
    ));
    assert!(matches!(
        store.confirm_notification_recipient(key, 0, 0).await,
        Err(StoreError::Fenced)
    ));
    assert_eq!(
        store.claim_notification_recipient(key, 0).await.unwrap(),
        RecipientDispatch::Send { attempt: 1 }
    );
    assert_eq!(
        store.claim_notification_recipient(key, 0).await.unwrap(),
        RecipientDispatch::Uncertain
    );
    assert!(matches!(
        store.confirm_notification_recipient(key, 0, 2).await,
        Err(StoreError::Fenced)
    ));
    store
        .confirm_notification_recipient(key, 0, 1)
        .await
        .unwrap();
    store
        .confirm_notification_recipient(key, 0, 1)
        .await
        .unwrap();
    assert_eq!(
        store.claim_notification_recipient(key, 0).await.unwrap(),
        RecipientDispatch::Confirmed
    );
    assert_eq!(
        store.claim_notification_recipient(key, 1).await.unwrap(),
        RecipientDispatch::Send { attempt: 1 }
    );
    store
        .finish_intent(&lease, key, OutboxStatus::Uncertain, 2)
        .await
        .unwrap();
    let reopened = Store::open(dir.path().join("journal")).await.unwrap();
    assert!(matches!(
        reopened.claim_notification_recipient(key, 1).await,
        Err(StoreError::InvalidInput)
    ));
    assert!(
        reopened
            .resend_notification(&admin(), key, "fanout-retry", 3)
            .await
            .unwrap()
    );
    assert!(matches!(
        reopened.confirm_notification_recipient(key, 1, 1).await,
        Err(StoreError::Fenced)
    ));
    assert!(reopened.start_intent(&lease, key, 1, 4).await.unwrap());
    assert_eq!(
        reopened.claim_notification_recipient(key, 0).await.unwrap(),
        RecipientDispatch::Confirmed
    );
    assert_eq!(
        reopened.claim_notification_recipient(key, 1).await.unwrap(),
        RecipientDispatch::Send { attempt: 2 }
    );
    assert!(matches!(
        reopened.confirm_notification_recipient(key, 1, 1).await,
        Err(StoreError::Fenced)
    ));
    reopened
        .confirm_notification_recipient(key, 1, 2)
        .await
        .unwrap();
    assert_eq!(
        reopened.claim_notification_recipient(key, 1).await.unwrap(),
        RecipientDispatch::Confirmed
    );
}

#[tokio::test]
async fn concurrent_fanout_dispatch_claims_only_one_attempt() {
    use pitcrew_store::delivery::RecipientDispatch;
    let (dir, store, lease, key) = setup().await;
    store.prepare_notification(key, &fanout()).await.unwrap();
    assert!(store.start_intent(&lease, key, 1, 2).await.unwrap());
    let reopened = Store::open(dir.path().join("journal")).await.unwrap();
    let (left, right) = tokio::join!(
        store.claim_notification_recipient(key, 0),
        reopened.claim_notification_recipient(key, 0)
    );
    let outcomes = [left.unwrap(), right.unwrap()];
    assert!(outcomes.contains(&RecipientDispatch::Send { attempt: 1 }));
    assert!(outcomes.contains(&RecipientDispatch::Uncertain));
}
