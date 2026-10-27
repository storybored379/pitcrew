//! Durable logical delivery identity and honest handling of uncertain sends.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DeliveryPurpose {
    Ready,
    ViewerFailure,
    AdminFailure,
    Working,
    TemplateMessage,
}

/// Unique outbox constraint: one logical message per job/purpose/recipient.
/// Retries always preserve this key and the payload. Transport receipts can
/// acknowledge the same row repeatedly; a retry never creates a new ready row.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeliveryKey {
    pub job: u64,
    pub purpose: DeliveryPurpose,
    pub recipient: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryOutcome {
    Confirmed,
    /// Rejected before acceptance; safe to retry the same intent.
    NotAccepted,
    /// Accepted remotely or lost response: must not pretend this is failure.
    Uncertain,
    PermanentFailure,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryState {
    Pending,
    Uncertain,
    Confirmed,
    PermanentFailure,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryAction {
    RetrySameKey,
    ReconcileOrReview,
    Complete,
    ReviewFailure,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryRecord {
    pub key: DeliveryKey,
    pub state: DeliveryState,
    pub attempt: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryAttempt {
    pub key: DeliveryKey,
    pub sequence: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryError {
    StaleAttempt,
    NotRetryable,
    AttemptOverflow,
}

impl DeliveryRecord {
    /// Persist Uncertain before starting a send, so a process death cannot turn
    /// a possibly accepted request into a new, apparently unsent ready message.
    pub fn begin_attempt(
        &mut self,
        transport_deduplicates: bool,
    ) -> Result<DeliveryAttempt, DeliveryError> {
        if self.next_action(transport_deduplicates) != DeliveryAction::RetrySameKey {
            return Err(DeliveryError::NotRetryable);
        }
        self.attempt = self
            .attempt
            .checked_add(1)
            .ok_or(DeliveryError::AttemptOverflow)?;
        self.state = DeliveryState::Uncertain;
        Ok(DeliveryAttempt {
            key: self.key.clone(),
            sequence: self.attempt,
        })
    }

    /// Confirmed is monotonic despite delayed failure/uncertain callbacks.
    /// Preserve the dispatch attempt token; never relabel an old response with
    /// the current attempt. Persist the observation before scheduling a retry.
    pub fn observe(
        &mut self,
        attempt: &DeliveryAttempt,
        outcome: DeliveryOutcome,
    ) -> Result<(), DeliveryError> {
        if attempt.key != self.key || attempt.sequence != self.attempt || self.attempt == 0 {
            return Err(DeliveryError::StaleAttempt);
        }
        if self.state == DeliveryState::Confirmed {
            return Ok(());
        }
        self.state = match outcome {
            DeliveryOutcome::Confirmed => DeliveryState::Confirmed,
            DeliveryOutcome::NotAccepted => DeliveryState::Pending,
            DeliveryOutcome::Uncertain => DeliveryState::Uncertain,
            DeliveryOutcome::PermanentFailure => DeliveryState::PermanentFailure,
        };
        Ok(())
    }

    pub fn next_action(&self, transport_deduplicates: bool) -> DeliveryAction {
        match self.state {
            DeliveryState::Pending => DeliveryAction::RetrySameKey,
            DeliveryState::Confirmed => DeliveryAction::Complete,
            DeliveryState::PermanentFailure => DeliveryAction::ReviewFailure,
            DeliveryState::Uncertain => {
                if self.key.purpose == DeliveryPurpose::Ready && !transport_deduplicates {
                    DeliveryAction::ReconcileOrReview
                } else {
                    DeliveryAction::RetrySameKey
                }
            }
        }
    }
}
