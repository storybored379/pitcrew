//! Durable intake → decision seam. Evidence is a closed, redacted fact type;
//! neither raw viewer text nor upstream bodies can enter this table. Recording
//! a recommendation grants no permission to execute a repair.
use crate::{Error, Result, Store, StoredIntake, intake_row, integer};
use pitcrew_deciders::{DeciderKind, DecisionReport, shadow::Comparison};
use pitcrew_diagnosis::facts::EvidenceFacts;
use pitcrew_domain::FileIdentity;
use serde::{Deserialize, Serialize};
use sqlx::{Row, sqlite::SqliteRow};

pub const MAX_DIAGNOSIS_PAGE: u32 = 100;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Diagnosis {
    pub report_id: i64,
    /// Unix seconds, as are all observation and expiry fields below.
    pub decided_at: u64,
    pub decider_kind: DeciderKind,
    pub decision: DecisionReport,
    pub evidence_summary: EvidenceFacts,
    pub resource: String,
    pub library_id: String,
    pub arr_instance_id: String,
    pub arr_item_id: u64,
    pub arr_file_id: u64,
    /// Exact observed bytes; absent means unavailable, never proof of failure.
    pub agent_file: Option<FileIdentity>,
    pub agent_observed_at: Option<u64>,
    pub access_observed_at: u64,
    pub access_expires_at: u64,
}

/// The account comes from the trusted intake journal, not from caller input.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingReport {
    pub intake: StoredIntake,
    pub account_id: String,
}

impl Store {
    /// First writer wins. An exact replay returns false; a changed decision for
    /// the same report fails. Decisions cannot be silently replaced after a
    /// bridge has acted on them.
    pub async fn persist_diagnosis(&self, diagnosis: &Diagnosis) -> Result<bool> {
        self.write_diagnosis(diagnosis, None).await
    }

    /// Save the sanitized comparison and selected diagnosis atomically. An exact
    /// replay may attach a previously absent comparison, but never change one.
    pub async fn persist_diagnosis_with_comparison(
        &self,
        diagnosis: &Diagnosis,
        comparison: &Comparison,
    ) -> Result<bool> {
        let selected_kind = if comparison.used_rules_fallback {
            DeciderKind::Rules
        } else {
            comparison.primary
        };
        if comparison.selected != diagnosis.decision || selected_kind != diagnosis.decider_kind {
            return Err(Error::Invalid);
        }
        self.write_diagnosis(diagnosis, Some(comparison)).await
    }

    async fn write_diagnosis(
        &self,
        diagnosis: &Diagnosis,
        comparison: Option<&Comparison>,
    ) -> Result<bool> {
        validate(diagnosis)?;
        let mut tx = self.pool.begin().await?;
        // A write before the replay lookup serializes competing SQLite writers.
        // SELECT also rejects a missing report or a decision before its intake.
        let changed = sqlx::query("INSERT INTO diagnoses (report_id,decided_at,decider_kind,decision,evidence_summary,resource,library_id,arr_instance_id,arr_item_id,arr_file_id,agent_file,agent_observed_at,access_observed_at,access_expires_at) SELECT ?,?,?,?,?,?,?,?,?,?,?,?,?,? FROM web_intakes WHERE id=? AND created<=? ON CONFLICT(report_id) DO NOTHING")
            .bind(diagnosis.report_id)
            .bind(integer(diagnosis.decided_at)?)
            .bind(kind_name(diagnosis.decider_kind))
            .bind(json(&diagnosis.decision)?)
            .bind(json(&diagnosis.evidence_summary)?)
            .bind(&diagnosis.resource)
            .bind(&diagnosis.library_id)
            .bind(&diagnosis.arr_instance_id)
            .bind(integer(diagnosis.arr_item_id)?)
            .bind(integer(diagnosis.arr_file_id)?)
            .bind(diagnosis.agent_file.as_ref().map(json).transpose()?)
            .bind(diagnosis.agent_observed_at.map(integer).transpose()?)
            .bind(integer(diagnosis.access_observed_at)?)
            .bind(integer(diagnosis.access_expires_at)?)
            .bind(diagnosis.report_id)
            .bind(integer(diagnosis.decided_at)?)
            .execute(&mut *tx).await?.rows_affected();
        let stored = sqlx::query("SELECT * FROM diagnoses WHERE report_id=?")
            .bind(diagnosis.report_id)
            .fetch_optional(&mut *tx)
            .await?
            .map(diagnosis_row)
            .transpose()?;
        if stored.as_ref() != Some(diagnosis) {
            return Err(Error::Invalid);
        }
        if let Some(comparison) = comparison {
            let serialized = json(comparison)?;
            sqlx::query("INSERT INTO diagnosis_comparisons (report_id,comparison) VALUES (?,?) ON CONFLICT(report_id) DO NOTHING")
                .bind(diagnosis.report_id).bind(&serialized).execute(&mut *tx).await?;
            let stored: String = sqlx::query_scalar(
                "SELECT comparison FROM diagnosis_comparisons WHERE report_id=?",
            )
            .bind(diagnosis.report_id)
            .fetch_one(&mut *tx)
            .await?;
            if stored != serialized {
                return Err(Error::Invalid);
            }
        }
        tx.commit().await?;
        Ok(changed == 1)
    }

    pub async fn diagnosis(&self, report_id: i64) -> Result<Option<Diagnosis>> {
        sqlx::query("SELECT * FROM diagnoses WHERE report_id=?")
            .bind(report_id)
            .fetch_optional(&self.pool)
            .await?
            .map(diagnosis_row)
            .transpose()
    }

    /// Ascending report-id cursor, bounded even for an overlarge requested page.
    pub async fn diagnoses(&self, after_id: i64, limit: u32) -> Result<Vec<Diagnosis>> {
        sqlx::query("SELECT * FROM diagnoses WHERE report_id>? ORDER BY report_id LIMIT ?")
            .bind(after_id)
            .bind(limit.min(MAX_DIAGNOSIS_PAGE))
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(diagnosis_row)
            .collect()
    }

    pub async fn diagnosis_comparison(&self, report_id: i64) -> Result<Option<serde_json::Value>> {
        let stored: Option<String> =
            sqlx::query_scalar("SELECT comparison FROM diagnosis_comparisons WHERE report_id=?")
                .bind(report_id)
                .fetch_optional(&self.pool)
                .await?;
        stored
            .map(|value| serde_json::from_str(&value).map_err(|_| Error::Invalid))
            .transpose()
    }

    /// The immutable report remains readable after it has been diagnosed.
    pub async fn report_for_diagnosis(&self, report_id: i64) -> Result<Option<PendingReport>> {
        sqlx::query("SELECT id,channel,message,conversation,response,created,account FROM web_intakes WHERE id=?")
            .bind(report_id).fetch_optional(&self.pool).await?.map(pending_row).transpose()
    }

    /// Bounded fair resweep for the trusted runtime. Includes already-decided
    /// reports so missing authority and saved telemetry can recover after restart.
    pub async fn runtime_reports(&self, after_id: i64, limit: u32) -> Result<Vec<PendingReport>> {
        sqlx::query("SELECT id,channel,message,conversation,response,created,account FROM web_intakes WHERE id>? ORDER BY id LIMIT ?")
            .bind(after_id).bind(limit.min(MAX_DIAGNOSIS_PAGE))
            .fetch_all(&self.pool).await?.into_iter().map(pending_row).collect()
    }

    /// All undiagnosed intake outcomes, in bounded report-id order. The caller
    /// parses the typed response and skips advisory or clarification outcomes.
    pub async fn pending_diagnoses(&self, after_id: i64, limit: u32) -> Result<Vec<PendingReport>> {
        sqlx::query("SELECT id,channel,message,conversation,response,created,account FROM web_intakes WHERE id>? AND NOT EXISTS (SELECT 1 FROM diagnoses WHERE report_id=web_intakes.id) ORDER BY id LIMIT ?")
            .bind(after_id).bind(limit.min(MAX_DIAGNOSIS_PAGE))
            .fetch_all(&self.pool).await?.into_iter().map(pending_row).collect()
    }
}

fn json(value: &impl Serialize) -> Result<String> {
    serde_json::to_string(value).map_err(|_| Error::Invalid)
}

fn kind_name(kind: DeciderKind) -> &'static str {
    match kind {
        DeciderKind::Rules => "rules",
        DeciderKind::Jev => "jev",
        DeciderKind::Anthropic => "anthropic",
        DeciderKind::OpenAiCompatible => "openai_compatible",
    }
}

fn pending_row(row: SqliteRow) -> Result<PendingReport> {
    let account_id = row.try_get("account")?;
    Ok(PendingReport {
        intake: intake_row(row)?,
        account_id,
    })
}

fn diagnosis_row(row: SqliteRow) -> Result<Diagnosis> {
    let kind: String = row.try_get("decider_kind")?;
    let diagnosis = Diagnosis {
        report_id: row.try_get("report_id")?,
        decided_at: nonnegative(row.try_get("decided_at")?)?,
        decider_kind: serde_json::from_value(serde_json::Value::String(kind))
            .map_err(|_| Error::Invalid)?,
        decision: serde_json::from_str(row.try_get("decision")?).map_err(|_| Error::Invalid)?,
        evidence_summary: serde_json::from_str(row.try_get("evidence_summary")?)
            .map_err(|_| Error::Invalid)?,
        resource: row.try_get("resource")?,
        library_id: row.try_get("library_id")?,
        arr_instance_id: row.try_get("arr_instance_id")?,
        arr_item_id: nonnegative(row.try_get("arr_item_id")?)?,
        arr_file_id: nonnegative(row.try_get("arr_file_id")?)?,
        agent_file: row
            .try_get::<Option<&str>, _>("agent_file")?
            .map(serde_json::from_str)
            .transpose()
            .map_err(|_| Error::Invalid)?,
        agent_observed_at: row
            .try_get::<Option<i64>, _>("agent_observed_at")?
            .map(nonnegative)
            .transpose()?,
        access_observed_at: nonnegative(row.try_get("access_observed_at")?)?,
        access_expires_at: nonnegative(row.try_get("access_expires_at")?)?,
    };
    validate(&diagnosis)?;
    Ok(diagnosis)
}

fn nonnegative(value: i64) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::Invalid)
}

fn validate(diagnosis: &Diagnosis) -> Result<()> {
    if diagnosis.report_id <= 0
        || diagnosis.arr_item_id == 0
        || diagnosis.arr_file_id == 0
        || diagnosis.resource.trim().is_empty()
        || diagnosis.library_id.trim().is_empty()
        || diagnosis.arr_instance_id.trim().is_empty()
        || diagnosis.access_observed_at > diagnosis.decided_at
        || diagnosis.access_expires_at <= diagnosis.decided_at
    {
        return Err(Error::Invalid);
    }
    match (diagnosis.agent_file, diagnosis.agent_observed_at) {
        (None, None) => {}
        (Some(file), Some(observed)) if file.id != 0 && observed <= diagnosis.decided_at => {}
        _ => return Err(Error::Invalid),
    }
    Ok(())
}
