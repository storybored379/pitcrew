mod common;

use common::*;
use pitcrew_admin::{Error, actions::Command, read::Sources};
use pitcrew_domain::{Event, Failure, IntentKey, channels::ChannelIdentity};
use pitcrew_store::{OutboxStatus, delivery::NotificationPayload};
use serde_json::json;

#[tokio::test]
async fn saved_deliveries_remain_visible_and_only_uncertain_prepared_messages_offer_resend() {
    let s = Setup::new(true).await;
    for job in 1..=2 {
        s.job(job).await;
        let lease = s.engine.acquire_lease(job, "worker", 0, 100).await.unwrap();
        s.engine
            .apply_event(&lease, 0, "cancelled", Event::Fail(Failure::Cancelled), 1)
            .await
            .unwrap();
        // Job 2 has an uncertain outbox intent but no immutable payload yet.
        s.engine
            .finish_intent(
                &lease,
                IntentKey { job, sequence: 2 },
                OutboxStatus::Uncertain,
                2,
            )
            .await
            .unwrap();
        if job == 1 {
            s.engine
                .finish_intent(
                    &lease,
                    IntentKey { job, sequence: 1 },
                    OutboxStatus::Completed,
                    2,
                )
                .await
                .unwrap();
        }
    }
    let admin_payload = NotificationPayload::Admin {
        notifier: "synthetic-owner-route".into(),
        title: "Repair needs review".into(),
        body: "The saved terminal failure is Cancelled.".into(),
    };
    let viewer_payload = NotificationPayload::Viewer {
        recipient: ChannelIdentity {
            channel_id: "synthetic-channel".into(),
            platform_user_id: "synthetic-viewer".into(),
        },
        conversation: "synthetic-conversation".into(),
        text: "Your report needs review. <script>untrusted()</script>".into(),
        last_inbound: Some(1),
    };
    for (sequence, payload) in [(1, &admin_payload), (2, &viewer_payload)] {
        s.engine
            .prepare_notification(IntentKey { job: 1, sequence }, payload)
            .await
            .unwrap();
    }
    let read = Sources::open(
        &s.dir.path().join("engine.sqlite"),
        &s.dir.path().join("intake.sqlite"),
    )
    .await
    .unwrap();
    let outbox = s.engine.outbox(1).await.unwrap();
    let expected = vec![
        json!({"job":1,"sequence":2,"status":"uncertain","intent":outbox[1].intent,"payload":viewer_payload}),
        json!({"job":1,"sequence":1,"status":"completed","intent":outbox[0].intent,"payload":admin_payload}),
    ];
    assert_eq!(read.deliveries().await.unwrap(), expected);
    read.validate_command(
        &Command::Resend {
            job: 1,
            sequence: 2,
        },
        100,
    )
    .await
    .unwrap();
    for (job, sequence) in [(1, 1), (2, 2)] {
        assert!(matches!(
            read.validate_command(&Command::Resend { job, sequence }, 100)
                .await,
            Err(Error::Conflict)
        ));
    }

    let (cookie, _) = s.session().await;
    for _ in 0..2 {
        let (status, _, body) = call(&s.app, "GET", "/deliveries", &cookie, "").await;
        assert_eq!(status, 200);
        assert!(body.contains("synthetic-conversation"));
        assert!(body.contains("Repair needs review"));
        assert!(body.contains("<td>uncertain</td>"));
        assert!(body.contains("<td>completed</td>"));
        assert!(body.contains("&lt;script&gt;untrusted()&lt;/script&gt;"));
        assert!(!body.contains("<script>untrusted()</script>"));
        assert!(body.contains("Resend job 1 message 2"));
        assert!(!body.contains("Resend job 1 message 1"));
        assert!(!body.contains("Resend job 2 message 2"));
        assert!(!body.contains("No notification deliveries recorded."));
    }
    assert!(s.api.calls.lock().unwrap().is_empty());
    assert!(s.app.journal.requests().await.unwrap().is_empty());
    assert_eq!(read.deliveries().await.unwrap(), expected);
}
