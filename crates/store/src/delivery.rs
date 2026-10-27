//! Immutable notification payloads and explicitly audited retries of uncertain sends.
use crate::{Result, Store, StoreError, integer, policy::AdminAction};
use pitcrew_domain::{Effect, Intent, IntentKey, channels::ChannelIdentity};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::collections::HashSet;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotificationPayload {
    /// Frozen recipients for reports sharing a single verified repair.
    Viewers { recipients: Vec<ViewerNotification> },
    Admin {
        notifier: String,
        title: String,
        body: String,
    },
    Viewer {
        recipient: ChannelIdentity,
        conversation: String,
        text: String,
        last_inbound: Option<u64>,
    },
}
/// Trusted channel routing and rendered copy; never populated from viewer text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewerNotification {
    pub recipient: ChannelIdentity,
    pub conversation: String,
    pub text: String,
    pub last_inbound: Option<u64>,
}

/// A recipient is uncertain before the remote request starts. Only an audited
/// resend of the parent notification can make an uncertain recipient pending.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecipientDispatch {
    Send { attempt: u64 },
    Confirmed,
    Uncertain,
}

impl NotificationPayload {
    fn matches(&self, effect: &Effect) -> bool {
        match (self, effect) {
            (
                Self::Viewers { recipients },
                Effect::NotifyReady { .. } | Effect::NotifyViewerFailure { .. },
            ) => {
                let mut identities = HashSet::new();
                !recipients.is_empty()
                    && recipients.iter().all(|viewer| {
                        !viewer.recipient.channel_id.trim().is_empty()
                            && !viewer.recipient.platform_user_id.trim().is_empty()
                            && !viewer.conversation.trim().is_empty()
                            && !viewer.text.trim().is_empty()
                            && identities.insert((
                                &viewer.recipient.channel_id,
                                &viewer.recipient.platform_user_id,
                            ))
                    })
            }
            (
                Self::Admin {
                    notifier,
                    title,
                    body,
                },
                Effect::NotifyAdmin { .. },
            ) => !notifier.trim().is_empty() && !title.trim().is_empty() && !body.trim().is_empty(),
            (
                Self::Viewer {
                    recipient,
                    conversation,
                    text,
                    ..
                },
                Effect::NotifyReady { .. } | Effect::NotifyViewerFailure { .. },
            ) => {
                !recipient.channel_id.trim().is_empty()
                    && !recipient.platform_user_id.trim().is_empty()
                    && !conversation.trim().is_empty()
                    && !text.trim().is_empty()
            }
            _ => false,
        }
    }
}

impl Store {
    /// Prepare from trusted routing/copy configuration. A conflict cannot redirect
    /// an already queued message or change its contents during an admin resend.
    pub async fn prepare_notification(
        &self,
        key: IntentKey,
        payload: &NotificationPayload,
    ) -> Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let intent: String =
            sqlx::query_scalar("SELECT intent FROM outbox WHERE job=? AND sequence=?")
                .bind(integer(key.job)?)
                .bind(integer(key.sequence)?)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(StoreError::Missing)?;
        if !payload.matches(&serde_json::from_str::<Intent>(&intent)?.effect) {
            return Err(StoreError::InvalidInput);
        }
        let json = serde_json::to_string(payload)?;
        let existing: Option<String> = sqlx::query_scalar(
            "SELECT payload FROM notification_payloads WHERE job=? AND sequence=?",
        )
        .bind(integer(key.job)?)
        .bind(integer(key.sequence)?)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(existing) = existing {
            if existing != json {
                return Err(StoreError::IdempotencyConflict);
            }
        } else {
            sqlx::query("INSERT INTO notification_payloads(job,sequence,payload) VALUES(?,?,?)")
                .bind(integer(key.job)?)
                .bind(integer(key.sequence)?)
                .bind(json)
                .execute(&mut *tx)
                .await?;
            if let NotificationPayload::Viewers { recipients } = payload {
                for index in 0..recipients.len() {
                    sqlx::query("INSERT INTO notification_recipients(job,sequence,recipient,state,attempt) VALUES(?,?,?,'pending',0)")
                        .bind(integer(key.job)?)
                        .bind(integer(key.sequence)?)
                        .bind(integer(index as u64)?)
                        .execute(&mut *tx).await?;
                }
            }
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn notification_payload(&self, key: IntentKey) -> Result<NotificationPayload> {
        let json: String = sqlx::query_scalar(
            "SELECT payload FROM notification_payloads WHERE job=? AND sequence=?",
        )
        .bind(integer(key.job)?)
        .bind(integer(key.sequence)?)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(StoreError::Missing)?;
        Ok(serde_json::from_str(&json)?)
    }

    /// Claim exactly one pending member of an immutable fanout under a started
    /// parent intent. The attempt token fences acknowledgments after a resend.
    pub async fn claim_notification_recipient(
        &self,
        key: IntentKey,
        recipient: u64,
    ) -> Result<RecipientDispatch> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query("SELECT n.state,n.attempt FROM notification_recipients n JOIN outbox o USING(job,sequence) WHERE n.job=? AND n.sequence=? AND n.recipient=? AND o.status='started' AND o.fence IS NOT NULL")
            .bind(integer(key.job)?)
            .bind(integer(key.sequence)?)
            .bind(integer(recipient)?)
            .fetch_optional(&mut *tx).await?
            .ok_or(StoreError::InvalidInput)?;
        let state: String = row.get("state");
        let dispatch = match state.as_str() {
            "confirmed" => RecipientDispatch::Confirmed,
            "uncertain" => RecipientDispatch::Uncertain,
            "pending" => {
                let attempt = row
                    .get::<i64, _>("attempt")
                    .checked_add(1)
                    .ok_or(StoreError::InvalidInput)?;
                sqlx::query("UPDATE notification_recipients SET state='uncertain',attempt=? WHERE job=? AND sequence=? AND recipient=?")
                    .bind(attempt).bind(integer(key.job)?).bind(integer(key.sequence)?).bind(integer(recipient)?)
                    .execute(&mut *tx).await?;
                RecipientDispatch::Send {
                    attempt: crate::unsigned(attempt)?,
                }
            }
            _ => return Err(StoreError::Corrupt),
        };
        tx.commit().await?;
        Ok(dispatch)
    }

    /// Persist only an acknowledgment for the current attempt. A delayed result
    /// cannot complete a newer admin-authorized attempt or undo confirmation.
    pub async fn confirm_notification_recipient(
        &self,
        key: IntentKey,
        recipient: u64,
        attempt: u64,
    ) -> Result<()> {
        let changed = sqlx::query("UPDATE notification_recipients SET state='confirmed' WHERE job=? AND sequence=? AND recipient=? AND attempt=? AND attempt>0 AND state IN ('uncertain','confirmed')")
            .bind(integer(key.job)?).bind(integer(key.sequence)?).bind(integer(recipient)?).bind(integer(attempt)?)
            .execute(&self.pool).await?.rows_affected();
        if changed != 1 {
            return Err(StoreError::Fenced);
        }
        Ok(())
    }

    /// The admin boundary authenticates `admin`; viewer text cannot construct it.
    /// Only a visibly uncertain send can be retried. Reuse the logical outbox row,
    /// destination and payload; one idempotency key never schedules two retries.
    pub async fn resend_notification(
        &self,
        admin: &AdminAction,
        key: IntentKey,
        request_key: &str,
        now: u64,
    ) -> Result<bool> {
        admin.validate()?;
        if request_key.trim().is_empty() {
            return Err(StoreError::InvalidInput);
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(row) = sqlx::query(
            "SELECT job,sequence,actor,note FROM notification_resends WHERE request_key=?",
        )
        .bind(request_key)
        .fetch_optional(&mut *tx)
        .await?
        {
            if row.get::<i64, _>("job") != integer(key.job)?
                || row.get::<i64, _>("sequence") != integer(key.sequence)?
                || row.get::<String, _>("actor") != admin.actor
                || row.get::<String, _>("note") != admin.note
            {
                return Err(StoreError::IdempotencyConflict);
            }
            return Ok(false);
        }
        let changed = sqlx::query("UPDATE outbox SET status='pending',fence=NULL WHERE job=? AND sequence=? AND status='uncertain' AND EXISTS (SELECT 1 FROM notification_payloads n WHERE n.job=outbox.job AND n.sequence=outbox.sequence)")
            .bind(integer(key.job)?).bind(integer(key.sequence)?).execute(&mut *tx).await?.rows_affected();
        if changed != 1 {
            return Err(StoreError::InvalidInput);
        }
        sqlx::query("UPDATE notification_recipients SET state='pending' WHERE job=? AND sequence=? AND state='uncertain'")
            .bind(integer(key.job)?).bind(integer(key.sequence)?)
            .execute(&mut *tx).await?;
        sqlx::query("INSERT INTO notification_resends(request_key,job,sequence,actor,note,at) VALUES(?,?,?,?,?,?)")
            .bind(request_key).bind(integer(key.job)?).bind(integer(key.sequence)?).bind(&admin.actor).bind(&admin.note).bind(integer(now)?)
            .execute(&mut *tx).await?;
        crate::policy::audit(
            &mut tx,
            admin,
            "notification_resend",
            &format!("{}:{}", key.job, key.sequence),
            now,
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }
}
