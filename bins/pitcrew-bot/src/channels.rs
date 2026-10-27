//! Conservative platform adapters. Authenticated ingress feeds normalized events;
//! the host supplies a transport and a durable delivery outbox. These adapters do
//! not automatically retry an uncertain send or claim physical idempotency.
use pitcrew_domain::{
    channels::{
        Channel, ChannelCapabilities, ChannelError, ChannelFuture, ChannelIdentity,
        IncomingMessage, OutgoingMessage, ProactiveRule,
    },
    delivery::DeliveryOutcome,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::VecDeque, sync::Arc};
use twilight_model::gateway::payload::incoming::MessageCreate;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Discord,
    Telegram,
    Seerr,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendRequest {
    /// Relative to the configured platform API base; credentials stay in the transport.
    pub path: String,
    pub body: Value,
}

pub trait Transport: Send + Sync {
    /// A network timeout or any lost response must return Uncertain. The durable
    /// outbox, not this adapter, owns resend/reconciliation policy.
    fn send(&self, request: SendRequest) -> ChannelFuture<'_, DeliveryOutcome>;
}

pub struct PlatformChannel {
    pub platform: Platform,
    channel_id: String,
    transport: Arc<dyn Transport>,
    pending: VecDeque<IncomingMessage>,
}

impl PlatformChannel {
    pub fn new(platform: Platform, channel_id: String, transport: Arc<dyn Transport>) -> Self {
        Self {
            platform,
            channel_id,
            transport,
            pending: VecDeque::new(),
        }
    }

    /// Called only by an authenticated gateway, webhook, or issue-polling host.
    /// Queue saturation leaves the event unacknowledged so its source can retry.
    pub fn enqueue(&mut self, incoming: IncomingMessage) -> Result<(), ChannelError> {
        crate::validate_message(&incoming).map_err(|_| ChannelError::Unsupported)?;
        if incoming.sender.channel_id != self.channel_id {
            return Err(ChannelError::Unauthorized);
        }
        if self.pending.len() >= 128 {
            return Err(ChannelError::RateLimited);
        }
        self.pending.push_back(incoming);
        Ok(())
    }

    fn request(&self, outgoing: &OutgoingMessage) -> Result<SendRequest, ChannelError> {
        if outgoing.recipient.channel_id != self.channel_id {
            return Err(ChannelError::Unauthorized);
        }
        if outgoing.text.is_empty()
            || outgoing.text.chars().count() > self.capabilities().max_text_chars as usize
        {
            return Err(ChannelError::Unsupported);
        }
        // Platform identifiers are decimal numbers, never URL fragments.
        let id = outgoing
            .conversation_id
            .parse::<u64>()
            .map_err(|_| ChannelError::Unsupported)?;
        if id == 0 {
            return Err(ChannelError::Unsupported);
        }
        Ok(match self.platform {
            Platform::Discord => SendRequest {
                path: format!("/channels/{id}/messages"),
                body: json!({"content":outgoing.text, "allowed_mentions":{"parse":[], "users":[], "roles":[], "replied_user":false}}),
            },
            Platform::Telegram => SendRequest {
                path: "/sendMessage".into(),
                // No parse_mode: text is literal. Private chats only on ingress.
                body: json!({"chat_id":id, "text":outgoing.text, "link_preview_options":{"is_disabled":true}}),
            },
            Platform::Seerr => SendRequest {
                path: format!("/api/v1/issue/{id}/comment"),
                body: json!({"message":outgoing.text}),
            },
        })
    }
}

impl Channel for PlatformChannel {
    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            buttons: false,
            rich_text: false,
            plain_text: true,
            max_text_chars: match self.platform {
                Platform::Discord => 2000,
                Platform::Telegram => 4096,
                Platform::Seerr => 2000,
            },
            proactive: ProactiveRule::UserMustMessageFirst,
            idempotent_delivery: false,
        }
    }
    fn receive(&mut self) -> ChannelFuture<'_, Option<IncomingMessage>> {
        Box::pin(async { Ok(self.pending.pop_front()) })
    }
    fn reply<'a>(
        &'a self,
        incoming: &'a IncomingMessage,
        outgoing: &'a OutgoingMessage,
    ) -> ChannelFuture<'a, DeliveryOutcome> {
        Box::pin(async move {
            if incoming.sender != outgoing.recipient
                || incoming.conversation_id != outgoing.conversation_id
            {
                return Err(ChannelError::Unauthorized);
            }
            self.transport.send(self.request(outgoing)?).await
        })
    }
    fn follow_up<'a>(
        &'a self,
        outgoing: &'a OutgoingMessage,
        last_inbound: Option<u64>,
        now: u64,
    ) -> ChannelFuture<'a, DeliveryOutcome> {
        Box::pin(async move {
            if !self.capabilities().proactive.permits(last_inbound, now) {
                return Err(ChannelError::Unsupported);
            }
            self.transport.send(self.request(outgoing)?).await
        })
    }
}

/// Twilight's typed Discord gateway event. DM-only and bot authors ignored;
/// receiving gateway events must use a bot application token, never a user token.
pub fn discord_message(
    channel_id: &str,
    event: &MessageCreate,
    now: u64,
) -> Option<IncomingMessage> {
    if event.author.bot || event.guild_id.is_some() || event.webhook_id.is_some() {
        return None;
    }
    Some(IncomingMessage {
        sender: ChannelIdentity {
            channel_id: channel_id.into(),
            platform_user_id: event.author.id.to_string(),
        },
        message_id: event.id.to_string(),
        conversation_id: event.channel_id.to_string(),
        text: event.content.clone(),
        received_at: now,
    })
}

#[derive(Debug, Deserialize)]
pub struct TelegramUpdate {
    pub update_id: u64,
    pub message: Option<TelegramMessage>,
}
#[derive(Debug, Deserialize)]
pub struct TelegramMessage {
    pub message_id: u64,
    pub from: Option<TelegramUser>,
    pub chat: TelegramChat,
    pub text: Option<String>,
}
#[derive(Debug, Deserialize)]
pub struct TelegramUser {
    pub id: u64,
    pub is_bot: bool,
}
#[derive(Debug, Deserialize)]
pub struct TelegramChat {
    pub id: i64,
    #[serde(rename = "type")]
    pub kind: String,
}

pub fn telegram_message(
    channel_id: &str,
    update: TelegramUpdate,
    now: u64,
) -> Option<IncomingMessage> {
    let message = update.message?;
    let sender = message.from?;
    if sender.is_bot || message.chat.kind != "private" || message.chat.id <= 0 {
        return None;
    }
    Some(IncomingMessage {
        sender: ChannelIdentity {
            channel_id: channel_id.into(),
            platform_user_id: sender.id.to_string(),
        },
        message_id: update.update_id.to_string(),
        conversation_id: message.chat.id.to_string(),
        text: message.text?,
        received_at: now,
    })
}

#[derive(Debug, Deserialize)]
pub struct SeerrIssue {
    pub id: u64,
    pub status: u8,
    pub comments: Vec<SeerrComment>,
}
#[derive(Debug, Deserialize)]
pub struct SeerrComment {
    pub id: u64,
    pub message: String,
    pub user: SeerrUser,
}
#[derive(Debug, Deserialize)]
pub struct SeerrUser {
    pub id: u64,
}

/// Each issue comment is its own replayable inbound event. Account linking is
/// still required: Seerr user ids never implicitly confer Plex membership.
pub fn seerr_messages(
    channel_id: &str,
    issue: SeerrIssue,
    bot_user: u64,
    now: u64,
) -> Vec<IncomingMessage> {
    if issue.status != 1 {
        return Vec::new();
    }
    issue
        .comments
        .into_iter()
        .filter(|comment| comment.user.id != bot_user)
        .map(|comment| IncomingMessage {
            sender: ChannelIdentity {
                channel_id: channel_id.into(),
                platform_user_id: comment.user.id.to_string(),
            },
            message_id: format!("{}:{}", issue.id, comment.id),
            conversation_id: issue.id.to_string(),
            text: comment.message,
            received_at: now,
        })
        .collect()
}
