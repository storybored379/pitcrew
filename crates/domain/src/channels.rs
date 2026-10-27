//! Channel-independent intake, follow-up rules, and Plex account linking.

use crate::delivery::{DeliveryKey, DeliveryOutcome};
use serde::{Deserialize, Serialize};
use std::{future::Future, pin::Pin};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelKind {
    Discord,
    Telegram,
    Web,
    Seerr,
    Email,
    Matrix,
    Slack,
    Sms,
    WhatsappOfficial,
    SignalBridge,
    ImessageBridge,
}

/// Identifier belongs to a configured adapter instance, not a global username.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelIdentity {
    pub channel_id: String,
    pub platform_user_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncomingMessage {
    pub sender: ChannelIdentity,
    pub message_id: String,
    pub conversation_id: String,
    /// Untrusted text: never grants access or changes policy.
    pub text: String,
    pub received_at: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProactiveRule {
    Allowed,
    Unsupported,
    UserMustMessageFirst,
    /// Window measured from the last inbound user message, in seconds.
    WithinWindow {
        seconds: u64,
    },
}

impl ProactiveRule {
    pub fn permits(self, last_inbound: Option<u64>, now: u64) -> bool {
        match self {
            Self::Allowed => true,
            Self::Unsupported => false,
            Self::UserMustMessageFirst => last_inbound.is_some_and(|at| at <= now),
            Self::WithinWindow { seconds } => last_inbound
                .and_then(|at| now.checked_sub(at))
                .is_some_and(|elapsed| elapsed < seconds),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelCapabilities {
    pub buttons: bool,
    pub rich_text: bool,
    pub plain_text: bool,
    /// Adapter's conservative rendered-text bound; split before dispatch.
    pub max_text_chars: u32,
    pub proactive: ProactiveRule,
    /// Whether resending the same idempotency key is physically deduplicated.
    pub idempotent_delivery: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutgoingMessage {
    pub key: DeliveryKey,
    pub recipient: ChannelIdentity,
    pub conversation_id: String,
    /// Reply writer renders for the adapter's declared capabilities.
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelError {
    Unavailable,
    Unauthorized,
    Unsupported,
    RateLimited,
}

pub type ChannelFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ChannelError>> + Send + 'a>>;

/// The bot binary hosts every enabled channel. Receive returns None when idle.
/// Implementations apply platform rules to replies and proactive messages.
pub trait Channel: Send + Sync {
    fn capabilities(&self) -> ChannelCapabilities;
    fn receive(&mut self) -> ChannelFuture<'_, Option<IncomingMessage>>;
    fn reply<'a>(
        &'a self,
        incoming: &'a IncomingMessage,
        outgoing: &'a OutgoingMessage,
    ) -> ChannelFuture<'a, DeliveryOutcome>;
    fn follow_up<'a>(
        &'a self,
        outgoing: &'a OutgoingMessage,
        last_inbound: Option<u64>,
        now: u64,
    ) -> ChannelFuture<'a, DeliveryOutcome>;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlexIdentity {
    pub account_id: String,
}

/// Created only after web Plex sign-in and fresh membership authorization.
/// Store only a digest of the randomly generated short code. Apply attempt/rate
/// limits and atomic single-use consumption in the store, never in a transport.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkChallenge {
    pub code_digest: String,
    pub plex: PlexIdentity,
    pub expires_at: u64,
    pub consumed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkedIdentity {
    pub plex: PlexIdentity,
    pub channel: ChannelIdentity,
    pub linked_at: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkError {
    Expired,
    AlreadyUsed,
}

impl LinkChallenge {
    /// Call only after constant-time code-digest verification and fresh Plex
    /// membership check; persist challenge+link atomically. The link itself is
    /// identity proof, never ongoing library-access authorization.
    pub fn consume(
        &mut self,
        channel: ChannelIdentity,
        now: u64,
    ) -> Result<LinkedIdentity, LinkError> {
        if self.consumed {
            return Err(LinkError::AlreadyUsed);
        }
        if now >= self.expires_at {
            return Err(LinkError::Expired);
        }
        self.consumed = true;
        Ok(LinkedIdentity {
            plex: self.plex.clone(),
            channel,
            linked_at: now,
        })
    }
}
