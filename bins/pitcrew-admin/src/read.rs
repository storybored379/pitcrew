//! Bounded read-only projections. Missing parallel-lane seams stay unavailable.
use crate::{Error, Result, actions::Command, journal::integer};
use pitcrew_domain::FixJob;
use pitcrew_store::NewJob;
use serde_json::{Value, json};
use sqlx::{
    Column, Row, SqlitePool, ValueRef,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteRow},
};
use std::{path::Path, time::Duration};

#[derive(Clone)]
pub struct Sources {
    pub(crate) engine: SqlitePool,
    pub(crate) intake: SqlitePool,
}
#[derive(Debug, PartialEq)]
pub struct Job {
    pub id: u64,
    pub revision: u64,
    pub state: String,
    pub summary: Value,
    pub events: Vec<Value>,
    pub intents: Vec<Value>,
}
#[derive(Debug, PartialEq)]
pub struct Queue {
    pub reports: Vec<Value>,
    pub diagnoses: Option<Vec<Value>>,
    pub progress: Option<Vec<Value>>,
}
pub fn replacement(value: &Value) -> bool {
    value.get("decision").unwrap_or(value)["action"] == "replace"
}
async fn open(path: &Path) -> Result<SqlitePool> {
    Ok(SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(path)
                .read_only(true)
                .busy_timeout(Duration::from_secs(5)),
        )
        .await?)
}
pub(crate) fn row(row: SqliteRow) -> Result<Value> {
    let mut values = serde_json::Map::new();
    for column in row.columns() {
        let name = column.name();
        let value = if row.try_get_raw(name)?.is_null() {
            Value::Null
        } else if let Ok(value) = row.try_get::<i64, _>(name) {
            json!(value)
        } else {
            let text: String = row.try_get(name)?;
            if matches!(
                name,
                "spec"
                    | "response"
                    | "decision"
                    | "evidence_summary"
                    | "payload"
                    | "ledger"
                    | "event"
                    | "intent"
                    | "command"
                    | "comparison"
            ) {
                serde_json::from_str(&text).map_err(|_| Error::Storage)?
            } else {
                Value::String(text)
            }
        };
        values.insert(name.into(), value);
    }
    Ok(Value::Object(values))
}
pub(crate) async fn rows(pool: &SqlitePool, query: &str) -> Result<Vec<Value>> {
    sqlx::query(query)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(row)
        .collect()
}
async fn table_exists(pool: &SqlitePool, table: &str) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?)",
    )
    .bind(table)
    .fetch_one(pool)
    .await?)
}
/// The pipeline stores decider comparisons in their own table; expose them as a
/// `comparison` column of each diagnosis when that table exists.
async fn diagnoses_source(pool: &SqlitePool) -> Result<&'static str> {
    Ok(if table_exists(pool, "diagnosis_comparisons").await? {
        "(SELECT d.*, c.comparison FROM diagnoses d LEFT JOIN diagnosis_comparisons c ON c.report_id = d.report_id)"
    } else {
        "diagnoses"
    })
}
async fn optional(pool: &SqlitePool, table: &str, query: &str) -> Result<Option<Vec<Value>>> {
    if table_exists(pool, table).await? {
        Ok(Some(rows(pool, query).await?))
    } else {
        Ok(None)
    }
}
impl Sources {
    pub async fn open(engine: &Path, intake: &Path) -> Result<Self> {
        Ok(Self {
            engine: open(engine).await?,
            intake: open(intake).await?,
        })
    }
    pub async fn queue(&self) -> Result<Queue> {
        Ok(Queue {
            reports: rows(&self.intake,"SELECT id,channel,response,created FROM web_intakes WHERE json_extract(response,'$.outcome')='report' ORDER BY id DESC LIMIT 100").await?,
            diagnoses: optional(&self.intake,"diagnoses",&format!("SELECT * FROM {} ORDER BY report_id DESC LIMIT 100", diagnoses_source(&self.intake).await?)).await?,
            progress: optional(&self.intake,"report_progress","SELECT report_id,job_id,state,updated_at,viewer_copy_key FROM report_progress ORDER BY report_id DESC LIMIT 100").await?,
        })
    }
    pub async fn report(&self, id: u64) -> Result<Queue> {
        let report = sqlx::query("SELECT id,channel,response,created FROM web_intakes WHERE id=? AND json_extract(response,'$.outcome')='report'")
            .bind(integer(id)?)
            .fetch_optional(&self.intake)
            .await?
            .ok_or(Error::Missing)?;
        // Bind exact report IDs; detail must still work beyond the queue window.
        let mut data = Queue {
            reports: vec![row(report)?],
            diagnoses: None,
            progress: None,
        };
        for (table, target) in [
            ("diagnoses", &mut data.diagnoses),
            ("report_progress", &mut data.progress),
        ] {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=? AND type='table')",
            )
            .bind(table)
            .fetch_one(&self.intake)
            .await?;
            if exists {
                let source = if table == "diagnoses" {
                    diagnoses_source(&self.intake).await?
                } else {
                    table
                };
                let query = format!("SELECT * FROM {source} WHERE report_id=?");
                *target = Some(
                    sqlx::query(&query)
                        .bind(integer(id)?)
                        .fetch_all(&self.intake)
                        .await?
                        .into_iter()
                        .map(row)
                        .collect::<Result<_>>()?,
                );
            }
        }
        Ok(data)
    }
    pub async fn job(&self, id: u64) -> Result<Job> {
        let mut tx = self.engine.begin().await?;
        let spec = sqlx::query("SELECT spec,revision FROM jobs WHERE id=?")
            .bind(integer(id)?)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Error::Missing)?;
        let revision: u64 = spec
            .try_get::<i64, _>("revision")?
            .try_into()
            .map_err(|_| Error::Storage)?;
        let spec: NewJob = serde_json::from_str(spec.try_get("spec")?)?;
        if spec.id != id {
            return Err(Error::Storage);
        }
        let mut restored = FixJob::new_with_timeouts(
            spec.id,
            spec.library,
            spec.original,
            spec.constraints.clone(),
            spec.policy,
            spec.timeouts,
            spec.created_at,
        )
        .map_err(|_| Error::Storage)?;
        let events =
            sqlx::query("SELECT revision,at,event FROM events WHERE job=? ORDER BY revision")
                .bind(integer(id)?)
                .fetch_all(&mut *tx)
                .await?
                .into_iter()
                .map(row)
                .collect::<Result<Vec<_>>>()?;
        if events.len() as u64 != revision {
            return Err(Error::Storage);
        }
        for (index, event) in events.iter().enumerate() {
            if event["revision"].as_u64() != Some(index as u64 + 1) {
                return Err(Error::Storage);
            }
            restored = restored
                .apply(
                    serde_json::from_value(event["event"].clone())?,
                    event["at"].as_u64().ok_or(Error::Storage)?,
                )
                .map_err(|_| Error::Storage)?
                .job;
        }
        let intents =
            sqlx::query("SELECT sequence,intent,status FROM outbox WHERE job=? ORDER BY sequence")
                .bind(integer(id)?)
                .fetch_all(&mut *tx)
                .await?
                .into_iter()
                .map(row)
                .collect::<Result<_>>()?;
        tx.commit().await?;
        Ok(Job {
            id,
            revision,
            state: format!("{:?}", restored.state()),
            summary: json!({"resource":spec.resource,"library":spec.library,"original":spec.original,"constraints":spec.constraints,"policy":spec.policy,"reserved_grabs":restored.reserved_budget(),"deadline":restored.deadline(),"failure":restored.failure()}),
            events,
            intents,
        })
    }
    pub async fn jobs(&self) -> Result<Vec<Job>> {
        let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM jobs ORDER BY id DESC LIMIT 100")
            .fetch_all(&self.engine)
            .await?;
        let mut jobs = Vec::new();
        for id in ids {
            jobs.push(self.job(id.try_into().map_err(|_| Error::Storage)?).await?);
        }
        Ok(jobs)
    }
    pub async fn exclusions(&self) -> Result<Vec<Value>> {
        rows(&self.engine,"SELECT id,client,spec,expires_at,pinned,released_at FROM release_exclusions ORDER BY id DESC LIMIT 100").await
    }
    pub async fn capabilities(&self) -> Result<Vec<Value>> {
        rows(
            &self.engine,
            "SELECT client,ledger FROM client_capabilities ORDER BY client LIMIT 100",
        )
        .await
    }
    pub async fn deliveries(&self) -> Result<Vec<Value>> {
        rows(&self.engine,"SELECT o.job,o.sequence,o.status,o.intent,p.payload FROM outbox o JOIN notification_payloads p ON p.job=o.job AND p.sequence=o.sequence ORDER BY o.job DESC,o.sequence DESC LIMIT 100").await
    }
    pub async fn lifecycle_deliveries(&self) -> Result<Option<Vec<Value>>> {
        let present: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='bridge_lifecycle')")
            .fetch_one(&self.intake).await?;
        if !present {
            return Ok(None);
        }
        Ok(Some(rows(&self.intake,"SELECT key,report_id,job_id,purpose,payload,state,created_at,updated_at FROM bridge_lifecycle ORDER BY created_at DESC,key LIMIT 100").await?))
    }
    pub async fn observations(&self) -> Result<Vec<Value>> {
        rows(&self.engine,"SELECT id,source,at,payload FROM (SELECT id,source,at,payload,ROW_NUMBER() OVER (PARTITION BY source ORDER BY id DESC) AS source_row FROM observations WHERE source IN ('diff','arbiter:recorded','agent:shadow','admin:health','decider:comparison','decider:usage')) WHERE source_row<=100 ORDER BY id DESC").await
    }
    pub async fn unresolved_model_attempts(&self) -> Result<Option<Vec<Value>>> {
        optional(
            &self.intake,
            "model_attempts",
            "SELECT report_id,started_at FROM model_attempts WHERE comparison IS NULL ORDER BY started_at DESC,report_id DESC LIMIT 100",
        )
        .await
    }
    pub async fn budgets(&self, now: u64) -> Result<Vec<Value>> {
        let rows = sqlx::query("SELECT b.job,b.viewer,b.at,b.units,EXISTS(SELECT 1 FROM ownership o WHERE o.job=b.job) AS active FROM budget_reservations b WHERE b.at>=? OR b.job IN (SELECT job FROM ownership) ORDER BY b.at DESC LIMIT 100")
            .bind(integer(now.saturating_sub(86_399))?).fetch_all(&self.engine).await?;
        rows.into_iter().map(row).collect()
    }
    pub async fn validate_command(&self, command: &Command, now: u64) -> Result<()> {
        match command {
            Command::Approve { report, decided_at } | Command::Deny { report, decided_at } => {
                let data = self.report(*report).await?;
                let diagnosis = data
                    .diagnoses
                    .and_then(|mut v| v.pop())
                    .ok_or(Error::Conflict)?;
                if !replacement(&diagnosis["decision"])
                    || diagnosis["decided_at"].as_u64() != Some(*decided_at)
                    || data
                        .progress
                        .is_some_and(|p| p.iter().any(|v| !v["job_id"].is_null()))
                {
                    return Err(Error::Conflict);
                }
            }
            Command::Close { report } | Command::Message { report, .. } => {
                self.report(*report).await?;
            }
            Command::LowerQuality { job, revision } => {
                let current = self.job(*job).await?;
                if current.revision != *revision || current.state != "AwaitingQualityApproval" {
                    return Err(Error::Conflict);
                }
            }
            Command::Release { id } | Command::Pin { id } => {
                let found: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM release_exclusions WHERE id=? AND released_at IS NULL AND (pinned=1 OR expires_at>?))")
                    .bind(integer(*id)?).bind(integer(now)?).fetch_one(&self.engine).await?;
                if !found {
                    return Err(Error::Conflict);
                }
            }
            Command::Reset { client } => {
                let found: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM client_capabilities WHERE client=?)",
                )
                .bind(client)
                .fetch_one(&self.engine)
                .await?;
                if !found {
                    return Err(Error::Missing);
                }
            }
            Command::Resend { job, sequence } => {
                let found: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM outbox o JOIN notification_payloads p ON p.job=o.job AND p.sequence=o.sequence WHERE o.job=? AND o.sequence=? AND o.status='uncertain')")
                    .bind(integer(*job)?).bind(integer(*sequence)?).fetch_one(&self.engine).await?;
                if !found {
                    return Err(Error::Conflict);
                }
            }
        }
        Ok(())
    }
}
