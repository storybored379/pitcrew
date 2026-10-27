use pitcrew_bot::{channels::*, dispatch, validate_message};
use pitcrew_domain::{channels::*, delivery::*};
use pitcrew_intake::{
    AuthorizedViewer, CatalogItem, ConversationContext, IntakeOutcome, Resolution,
};
use serde_json::json;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Recording {
    calls: Mutex<Vec<SendRequest>>,
    result: Mutex<Option<DeliveryOutcome>>,
}
impl Transport for Recording {
    fn send(&self, request: SendRequest) -> ChannelFuture<'_, DeliveryOutcome> {
        self.calls.lock().unwrap().push(request);
        let outcome = self
            .result
            .lock()
            .unwrap()
            .unwrap_or(DeliveryOutcome::Confirmed);
        Box::pin(async move { Ok(outcome) })
    }
}
fn incoming() -> IncomingMessage {
    IncomingMessage {
        sender: ChannelIdentity {
            channel_id: "fixture".into(),
            platform_user_id: "10".into(),
        },
        message_id: "20".into(),
        conversation_id: "30".into(),
        text: "The Glass Harbor won't play".into(),
        received_at: 80,
    }
}
fn outgoing() -> OutgoingMessage {
    OutgoingMessage {
        key: DeliveryKey {
            job: 1,
            purpose: DeliveryPurpose::ViewerFailure,
            recipient: "10".into(),
        },
        recipient: incoming().sender,
        conversation_id: "30".into(),
        text: "@everyone <b>literal</b>".into(),
    }
}
pub fn catalog() -> Vec<CatalogItem> {
    vec![CatalogItem {
        id: "glass".into(),
        title: "The Glass Harbor".into(),
        aliases: vec![],
        library_id: "films".into(),
        library_name: "Films".into(),
        resolution: Resolution::Hd,
        season: None,
        episode: None,
    }]
}
#[test]
fn bounded_dispatch_uses_trusted_authorization() {
    let viewer = AuthorizedViewer {
        account_id: "viewer".into(),
        library_ids: vec!["films".into()],
    };
    assert_eq!(
        dispatch(
            &incoming(),
            Some(&viewer),
            &catalog(),
            &ConversationContext::default()
        )
        .unwrap()
        .outcome,
        IntakeOutcome::Report
    );
    assert_eq!(
        dispatch(
            &incoming(),
            None,
            &catalog(),
            &ConversationContext::default()
        )
        .unwrap()
        .outcome,
        IntakeOutcome::Unauthorized
    );
    for field in 0..4 {
        for value in [String::new(), "a".repeat(129), "a\nb".into()] {
            let mut message = incoming();
            match field {
                0 => message.sender.channel_id = value,
                1 => message.sender.platform_user_id = value,
                2 => message.message_id = value,
                _ => message.conversation_id = value,
            }
            assert!(validate_message(&message).is_err());
            assert!(
                dispatch(
                    &message,
                    Some(&viewer),
                    &catalog(),
                    &ConversationContext::default()
                )
                .is_err()
            );
        }
        let mut message = incoming();
        match field {
            0 => message.sender.channel_id = "a".repeat(128),
            1 => message.sender.platform_user_id = "a".repeat(128),
            2 => message.message_id = "a".repeat(128),
            _ => message.conversation_id = "a".repeat(128),
        }
        assert!(validate_message(&message).is_ok());
    }
    for text in [String::new(), " \n ".into(), "a".repeat(8193)] {
        let mut m = incoming();
        m.text = text;
        assert!(validate_message(&m).is_err());
    }
    let mut m = incoming();
    m.text = "a".repeat(8192);
    assert!(validate_message(&m).is_ok());
}
#[tokio::test]
async fn channels_follow_platform_rules_and_preserve_uncertainty() {
    for (platform, max, path, body) in [
        (
            Platform::Discord,
            2000,
            "/channels/30/messages",
            json!({"content":outgoing().text,"allowed_mentions":{"parse":[],"users":[],"roles":[],"replied_user":false}}),
        ),
        (
            Platform::Telegram,
            4096,
            "/sendMessage",
            json!({"chat_id":30,"text":outgoing().text,"link_preview_options":{"is_disabled":true}}),
        ),
        (
            Platform::Seerr,
            2000,
            "/api/v1/issue/30/comment",
            json!({"message":outgoing().text}),
        ),
    ] {
        let recording = Arc::new(Recording::default());
        let mut channel = PlatformChannel::new(platform, "fixture".into(), recording.clone());
        assert_eq!(
            channel.capabilities(),
            ChannelCapabilities {
                buttons: false,
                rich_text: false,
                plain_text: true,
                max_text_chars: max,
                proactive: ProactiveRule::UserMustMessageFirst,
                idempotent_delivery: false
            }
        );
        assert_eq!(channel.receive().await, Ok(None));
        channel.enqueue(incoming()).unwrap();
        assert_eq!(channel.receive().await, Ok(Some(incoming())));
        assert_eq!(channel.receive().await, Ok(None));
        assert_eq!(
            channel.reply(&incoming(), &outgoing()).await,
            Ok(DeliveryOutcome::Confirmed)
        );
        assert_eq!(
            recording.calls.lock().unwrap().as_slice(),
            [SendRequest {
                path: path.into(),
                body
            }]
        );
        for outcome in [
            DeliveryOutcome::Uncertain,
            DeliveryOutcome::NotAccepted,
            DeliveryOutcome::PermanentFailure,
        ] {
            *recording.result.lock().unwrap() = Some(outcome);
            assert_eq!(
                channel.follow_up(&outgoing(), Some(80), 80).await,
                Ok(outcome)
            );
        }
        let before = recording.calls.lock().unwrap().len();
        for last in [None, Some(81)] {
            assert_eq!(
                channel.follow_up(&outgoing(), last, 80).await,
                Err(ChannelError::Unsupported)
            );
        }
        let mut wrong = outgoing();
        wrong.recipient.platform_user_id = "other".into();
        assert_eq!(
            channel.reply(&incoming(), &wrong).await,
            Err(ChannelError::Unauthorized)
        );
        wrong = outgoing();
        wrong.conversation_id = "31".into();
        assert_eq!(
            channel.reply(&incoming(), &wrong).await,
            Err(ChannelError::Unauthorized)
        );
        wrong = outgoing();
        wrong.recipient.channel_id = "other".into();
        assert_eq!(
            channel.follow_up(&wrong, Some(80), 80).await,
            Err(ChannelError::Unauthorized)
        );
        for value in ["0", "../secret", "-1", ""] {
            wrong = outgoing();
            wrong.conversation_id = value.into();
            assert_eq!(
                channel.follow_up(&wrong, Some(80), 80).await,
                Err(ChannelError::Unsupported)
            );
        }
        for value in [String::new(), "a".repeat(max as usize + 1)] {
            wrong = outgoing();
            wrong.text = value;
            assert_eq!(
                channel.follow_up(&wrong, Some(80), 80).await,
                Err(ChannelError::Unsupported)
            );
        }
        assert_eq!(recording.calls.lock().unwrap().len(), before);
        wrong = outgoing();
        wrong.text = "é".repeat(max as usize);
        assert!(channel.follow_up(&wrong, Some(80), 80).await.is_ok());
        let mut bad = incoming();
        bad.sender.channel_id = "other".into();
        assert_eq!(channel.enqueue(bad), Err(ChannelError::Unauthorized));
        bad = incoming();
        bad.text = "".into();
        assert_eq!(channel.enqueue(bad), Err(ChannelError::Unsupported));
        for _ in 0..128 {
            channel.enqueue(incoming()).unwrap();
        }
        assert_eq!(channel.enqueue(incoming()), Err(ChannelError::RateLimited));
    }
}
fn telegram() -> serde_json::Value {
    json!({"update_id":9,"message":{"message_id":4,"from":{"id":10,"is_bot":false},"chat":{"id":30,"type":"private"},"text":"broken"}})
}
#[test]
fn telegram_requires_a_private_human_message() {
    let value =
        telegram_message("fixture", serde_json::from_value(telegram()).unwrap(), 80).unwrap();
    assert_eq!(
        value,
        IncomingMessage {
            text: "broken".into(),
            message_id: "9".into(),
            ..incoming()
        }
    );
    for (pointer, value) in [
        ("/message/from/is_bot", json!(true)),
        ("/message/chat/type", json!("group")),
        ("/message/chat/id", json!(-1)),
        ("/message/chat/id", json!(0)),
        ("/message/from", json!(null)),
        ("/message/text", json!(null)),
        ("/message", json!(null)),
    ] {
        let mut event = telegram();
        *event.pointer_mut(pointer).unwrap() = value;
        assert!(telegram_message("fixture", serde_json::from_value(event).unwrap(), 80).is_none());
    }
}
#[test]
fn discord_twilight_contract_ignores_bots_guilds_and_webhooks() {
    let event = json!({"id":"20","channel_id":"30","author":{"id":"10","username":"synthetic","discriminator":"0","avatar":null,"bot":false},"content":"hello","timestamp":"2026-01-01T00:00:00+00:00","edited_timestamp":null,"tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],"attachments":[],"embeds":[],"pinned":false,"type":0});
    let parsed = serde_json::from_value(event.clone()).unwrap();
    assert_eq!(
        discord_message("fixture", &parsed, 80),
        Some(IncomingMessage {
            text: "hello".into(),
            ..incoming()
        })
    );
    for (key, value) in [("guild_id", json!("40")), ("webhook_id", json!("50"))] {
        let mut v = event.clone();
        v[key] = value;
        assert!(discord_message("fixture", &serde_json::from_value(v).unwrap(), 80).is_none());
    }
    let mut v = event;
    v["author"]["bot"] = json!(true);
    assert!(discord_message("fixture", &serde_json::from_value(v).unwrap(), 80).is_none());
}
#[test]
fn seerr_comment_ids_replay_and_never_authorize() {
    let event = json!({"id":30,"status":1,"comments":[{"id":7,"message":"broken","user":{"id":10}},{"id":8,"message":"bot reply","user":{"id":99}}]});
    assert_eq!(
        seerr_messages(
            "fixture",
            serde_json::from_value(event.clone()).unwrap(),
            99,
            80
        ),
        vec![IncomingMessage {
            message_id: "30:7".into(),
            text: "broken".into(),
            ..incoming()
        }]
    );
    let mut closed = event;
    closed["status"] = json!(3);
    assert!(seerr_messages("fixture", serde_json::from_value(closed).unwrap(), 99, 80).is_empty());
}
