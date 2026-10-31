//! Durable model attempts are separate from authorized diagnoses. A result that
//! arrives after access expiry still has accounting, but never grants authority.
use crate::{Error, Result, Store, integer};
use pitcrew_deciders::shadow::Comparison;
use serde_json::Value;
use sqlx::Row;

#[derive(Debug, PartialEq)]
pub struct ModelAttempt {
    pub started_at: u64,
    /// Absent means an interrupted/unfinished attempt with unknown usage.
    pub comparison: Option<Value>,
}

impl Store {
    /// Reserve once before calling a model. A restart or concurrent worker must
    /// use current rules instead of repeating a potentially billable request.
    pub async fn begin_model_attempt(&self, report: i64, now: u64) -> Result<bool> {
        let changed = sqlx::query("INSERT INTO model_attempts (report_id,started_at) SELECT id,? FROM web_intakes WHERE id=? AND created<=? ON CONFLICT(report_id) DO NOTHING")
            .bind(integer(now)?).bind(report).bind(integer(now)?)
            .execute(&self.pool).await?.rows_affected();
        Ok(changed == 1)
    }

    /// Save only the typed, redacted comparison; exact replays are idempotent.
    pub async fn finish_model_attempt(&self, report: i64, comparison: &Comparison) -> Result<()> {
        let serialized = serde_json::to_string(comparison).map_err(|_| Error::Invalid)?;
        sqlx::query(
            "UPDATE model_attempts SET comparison=? WHERE report_id=? AND comparison IS NULL",
        )
        .bind(&serialized)
        .bind(report)
        .execute(&self.pool)
        .await?;
        let stored: Option<String> =
            sqlx::query_scalar("SELECT comparison FROM model_attempts WHERE report_id=?")
                .bind(report)
                .fetch_optional(&self.pool)
                .await?
                .flatten();
        if stored.as_deref() != Some(&serialized) {
            return Err(Error::Invalid);
        }
        Ok(())
    }

    pub async fn model_attempt(&self, report: i64) -> Result<Option<ModelAttempt>> {
        let row = sqlx::query("SELECT started_at,comparison FROM model_attempts WHERE report_id=?")
            .bind(report)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| {
            let stored: Option<String> = row.get("comparison");
            Ok(ModelAttempt {
                started_at: row.get::<i64, _>("started_at") as u64,
                comparison: stored
                    .map(|s| serde_json::from_str(&s).map_err(|_| Error::Invalid))
                    .transpose()?,
            })
        })
        .transpose()
    }
}
