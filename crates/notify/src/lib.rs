//! One-way admin alerts are independent of conversational channels.
//! Pre-alpha contract only; no production transport performs I/O yet.

use pitcrew_domain::delivery::{DeliveryKey, DeliveryOutcome};
use std::{future::Future, pin::Pin};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminAlert {
    pub key: DeliveryKey,
    pub title: String,
    pub body: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotifyError {
    Unavailable,
    Unauthorized,
    RateLimited,
}

pub type NotifyFuture<'a> =
    Pin<Box<dyn Future<Output = Result<DeliveryOutcome, NotifyError>> + Send + 'a>>;

/// ntfy, Pushover, email and webhooks implement this boundary. Adapters preserve
/// the outbox idempotency key; uncertain receipt remains visible to the admin.
pub trait Notifier: Send + Sync {
    fn idempotent_delivery(&self) -> bool;
    fn send<'a>(&'a self, alert: &'a AdminAlert) -> NotifyFuture<'a>;
}
