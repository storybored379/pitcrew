//! Immutable working, late-join and owner-template commands. A send is marked
//! uncertain before calling a transport; restart never retries that command.
use crate::{Authority, Bridge, Error, Result, ViewerRoute, integer, unsigned};
use pitcrew_domain::{
    Access, Effect, IntentKey, State,
    channels::{Channel, OutgoingMessage},
    delivery::{DeliveryKey, DeliveryOutcome, DeliveryPurpose},
};
use pitcrew_store::{
    StoreError,
    delivery::{NotificationPayload, ViewerNotification},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    pub key: String,
    pub report_id: i64,
    pub job_id: Option<u64>,
    pub purpose: DeliveryPurpose,
    pub payload: ViewerNotification,
    pub state: String,
}

impl Bridge {
    pub(crate) async fn route_for_report(
        &self,
        report: i64,
        now: u64,
    ) -> Result<Option<ViewerRoute>> {
        let row = sqlx::query("SELECT i.account,a.authority FROM bridge_authority a JOIN web_intakes i ON i.id=a.report_id WHERE a.report_id=?")
            .bind(report).fetch_optional(&self.pool).await?;
        let Some(row) = row else { return Ok(None) };
        let authority: Authority = serde_json::from_str(row.try_get("authority")?)?;
        if authority.account != row.get::<String, _>("account")
            || authority.observed_at > now
            || !matches!(authority.access, Access::Granted { library, expires_at } if library == authority.library && expires_at > now)
        {
            return Ok(None);
        }
        Ok(authority.route.filter(|route| {
            !route.recipient.channel_id.trim().is_empty()
                && !route.recipient.platform_user_id.trim().is_empty()
                && !route.conversation.trim().is_empty()
        }))
    }

    /// Bounded, persistent round-robin scan. Terminal follow-ups only cover
    /// recipients absent from the already frozen engine terminal fanout.
    pub async fn prepare_lifecycle_notifications(&self, now: u64) -> Result<usize> {
        let cursor: i64 =
            sqlx::query_scalar("SELECT report_id FROM bridge_lifecycle_cursor WHERE id=1")
                .fetch_one(&self.pool)
                .await?;
        let rows = sqlx::query("SELECT report_id,job_id FROM report_progress WHERE job_id IS NOT NULL AND report_id>? ORDER BY report_id LIMIT 128")
            .bind(cursor).fetch_all(&self.pool).await?;
        let mut prepared = 0;
        for row in &rows {
            let report = row.get("report_id");
            let job = unsigned(row.get("job_id"))?;
            let stored = self.engine.load_job(job).await?;
            let terminal = matches!(
                stored.job.state(),
                State::Verified | State::Done | State::Failed
            );
            if terminal {
                // Avoid a stale working message after a terminal result, even
                // when no usable viewer route remains.
                sqlx::query("UPDATE bridge_lifecycle SET state='superseded',updated_at=? WHERE job_id=? AND purpose=? AND state='pending'")
                    .bind(integer(now)?).bind(integer(job)?).bind(serde_json::to_string(&DeliveryPurpose::Working)?)
                    .execute(&self.pool).await?;
            }
            let Some(route) = self.route_for_report(report, now).await? else {
                continue;
            };
            let purpose = if stored.job.state() == State::Failed {
                DeliveryPurpose::ViewerFailure
            } else if terminal {
                DeliveryPurpose::Ready
            } else {
                DeliveryPurpose::Working
            };
            if terminal && !self.needs_followup(job, purpose, &route).await? {
                continue;
            }
            let text = if terminal {
                self.reply_for_job(job).await?
            } else {
                "We're checking your report. We'll update you when we have a verified result."
                    .into()
            };
            prepared += usize::from(
                self.queue_lifecycle(
                    report,
                    Some(job),
                    purpose,
                    &format!("job:{job}"),
                    route,
                    text,
                    now,
                )
                .await?,
            );
        }
        let next = rows.last().map_or(0, |row| row.get::<i64, _>("report_id"));
        sqlx::query("UPDATE bridge_lifecycle_cursor SET report_id=? WHERE id=1")
            .bind(next)
            .execute(&self.pool)
            .await?;
        Ok(prepared)
    }

    async fn needs_followup(
        &self,
        job: u64,
        purpose: DeliveryPurpose,
        route: &ViewerRoute,
    ) -> Result<bool> {
        for row in self.engine.outbox(job).await? {
            if !matches!(
                (&row.intent.effect, purpose),
                (Effect::NotifyReady { .. }, DeliveryPurpose::Ready)
                    | (
                        Effect::NotifyViewerFailure { .. },
                        DeliveryPurpose::ViewerFailure
                    )
            ) {
                continue;
            }
            let payload = match self
                .engine
                .notification_payload(IntentKey {
                    job,
                    sequence: row.intent.sequence,
                })
                .await
            {
                Ok(payload) => payload,
                Err(StoreError::Missing) => return Ok(false),
                Err(error) => return Err(error.into()),
            };
            return Ok(match payload {
                NotificationPayload::Viewers { recipients } => !recipients
                    .iter()
                    .any(|viewer| viewer.recipient == route.recipient),
                NotificationPayload::Viewer { recipient, .. } => recipient != route.recipient,
                NotificationPayload::Admin { .. } => false,
            });
        }
        Ok(false)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn queue_lifecycle(
        &self,
        report: i64,
        job: Option<u64>,
        purpose: DeliveryPurpose,
        command: &str,
        route: ViewerRoute,
        text: String,
        now: u64,
    ) -> Result<bool> {
        let key = format!(
            "lifecycle:{:x}",
            Sha256::digest(serde_json::to_vec(&(command, purpose, &route.recipient))?)
        );
        let payload = ViewerNotification {
            recipient: route.recipient,
            conversation: route.conversation,
            last_inbound: route.last_inbound,
            text,
        };
        Ok(sqlx::query("INSERT INTO bridge_lifecycle(key,report_id,job_id,purpose,payload,created_at,updated_at) VALUES (?,?,?,?,?,?,?) ON CONFLICT(key) DO NOTHING")
            .bind(key).bind(report).bind(job.map(integer).transpose()?).bind(serde_json::to_string(&purpose)?)
            .bind(serde_json::to_string(&payload)?).bind(integer(now)?).bind(integer(now)?)
            .execute(&self.pool).await?.rows_affected() == 1)
    }

    pub async fn lifecycle_notifications(&self) -> Result<Vec<Notification>> {
        sqlx::query("SELECT * FROM bridge_lifecycle ORDER BY created_at,key LIMIT 128")
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(notification)
            .collect()
    }

    /// Shadow mode records `shadowed` commands without invoking a channel.
    /// Live mode requires a configured compatible channel and still-current
    /// authenticated recipient. No uncertain command is automatically retried.
    pub async fn dispatch_lifecycle(
        &self,
        channels: &BTreeMap<String, Arc<dyn Channel>>,
        now: u64,
        shadow: bool,
    ) -> Result<usize> {
        let cursor: String =
            sqlx::query_scalar("SELECT last_key FROM bridge_dispatch_cursor WHERE id=1")
                .fetch_one(&self.pool)
                .await?;
        let mut rows = sqlx::query(
            "SELECT * FROM bridge_lifecycle WHERE state='pending' AND key>? ORDER BY key LIMIT 128",
        )
        .bind(&cursor)
        .fetch_all(&self.pool)
        .await?;
        if rows.is_empty() {
            rows = sqlx::query(
                "SELECT * FROM bridge_lifecycle WHERE state='pending' ORDER BY key LIMIT 128",
            )
            .fetch_all(&self.pool)
            .await?;
        }
        // A permanently unavailable adapter/route must not monopolize the next
        // bounded pass. Cursor updates cannot make an uncertain send retryable.
        let next = rows
            .last()
            .map_or(String::new(), |row| row.get::<String, _>("key"));
        let mut dispatched = 0;
        for row in rows {
            let item = notification(row)?;
            if item.purpose == DeliveryPurpose::Working {
                let Some(job) = item.job_id else {
                    return Err(Error::Invalid);
                };
                if matches!(
                    self.engine.load_job(job).await?.job.state(),
                    State::Verified | State::Done | State::Failed
                ) {
                    sqlx::query("UPDATE bridge_lifecycle SET state='superseded',updated_at=? WHERE key=? AND state='pending'")
                        .bind(integer(now)?).bind(&item.key).execute(&self.pool).await?;
                    continue;
                }
            }
            let Some(route) = self.route_for_report(item.report_id, now).await? else {
                continue;
            };
            if route.recipient != item.payload.recipient {
                continue;
            }
            let adapter = channels.get(&item.payload.recipient.channel_id);
            if !shadow {
                let Some(adapter) = adapter else { continue };
                let capabilities = adapter.capabilities();
                if !capabilities.plain_text
                    || item.payload.text.chars().count() > capabilities.max_text_chars as usize
                    || !capabilities
                        .proactive
                        .permits(item.payload.last_inbound, now)
                {
                    continue;
                }
            }
            let claimed = sqlx::query(
                "UPDATE bridge_lifecycle SET state=?,updated_at=? WHERE key=? AND state='pending'",
            )
            .bind(if shadow { "shadowed" } else { "uncertain" })
            .bind(integer(now)?)
            .bind(&item.key)
            .execute(&self.pool)
            .await?
            .rows_affected();
            if claimed == 0 {
                continue;
            }
            dispatched += 1;
            if shadow {
                continue;
            }
            let outgoing = OutgoingMessage {
                key: DeliveryKey {
                    job: item.job_id.unwrap_or(unsigned(item.report_id)?),
                    purpose: item.purpose,
                    recipient: item.key.clone(),
                },
                recipient: item.payload.recipient,
                conversation_id: item.payload.conversation,
                text: item.payload.text,
            };
            if adapter
                .ok_or(Error::Invalid)?
                .follow_up(&outgoing, item.payload.last_inbound, now)
                .await
                == Ok(DeliveryOutcome::Confirmed)
            {
                sqlx::query("UPDATE bridge_lifecycle SET state='confirmed',updated_at=? WHERE key=? AND state='uncertain'")
                    .bind(integer(now)?).bind(&item.key).execute(&self.pool).await?;
            }
        }
        sqlx::query("UPDATE bridge_dispatch_cursor SET last_key=? WHERE id=1")
            .bind(next)
            .execute(&self.pool)
            .await?;
        Ok(dispatched)
    }
}

fn notification(row: sqlx::sqlite::SqliteRow) -> Result<Notification> {
    Ok(Notification {
        key: row.try_get("key")?,
        report_id: row.try_get("report_id")?,
        job_id: row
            .try_get::<Option<i64>, _>("job_id")?
            .map(unsigned)
            .transpose()?,
        purpose: serde_json::from_str(row.try_get("purpose")?)?,
        payload: serde_json::from_str(row.try_get("payload")?)?,
        state: row.try_get("state")?,
    })
}
