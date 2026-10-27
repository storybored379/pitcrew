//! SQLite journal, fenced workers and transactional effect intents.
//!
//! Jobs are restored exclusively by replaying accepted events through the domain
//! reducer. No deserialization can install an unchecked `FixJob` snapshot.
pub mod delivery;
pub mod policy;
pub mod registry;

use pitcrew_domain::{
    Constraints, Event, FileIdentity, FixJob, Intent, IntentKey, Policy, StageTimeouts,
};
use serde::{Deserialize, Serialize};
use sqlx::{
    Row, SqliteConnection, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};
use std::{path::Path, time::Duration};
use thiserror::Error;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewJob {
    pub id: u64,
    /// Opaque installation identity; never an upstream username.
    pub viewer: String,
    /// Scoped immutable media identity, shared across jobs for the same item.
    pub resource: String,
    pub library: u64,
    pub original: FileIdentity,
    pub constraints: Constraints,
    pub policy: Policy,
    pub timeouts: StageTimeouts,
    pub created_at: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BudgetLimits {
    pub global_hour: u32,
    pub global_day: u32,
    pub viewer_hour: u32,
    pub viewer_day: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredJob {
    pub job: FixJob,
    pub revision: u64,
    pub spec: NewJob,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    pub job: u64,
    pub owner: String,
    pub fence: u64,
    pub expires_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    pub stored: StoredJob,
    pub intents: Vec<Intent>,
    pub duplicate: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboxStatus {
    Pending,
    Started,
    Completed,
    Uncertain,
    Suppressed,
    Shadowed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutboxEntry {
    pub intent: Intent,
    pub status: OutboxStatus,
    pub fence: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationRecord {
    pub id: u64,
    pub source: String,
    pub key: String,
    pub payload: serde_json::Value,
    pub at: u64,
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database operation failed")]
    Database(#[from] sqlx::Error),
    #[error("database migration failed")]
    Migration(#[from] sqlx::migrate::MigrateError),
    #[error("journal data is invalid")]
    Json(#[from] serde_json::Error),
    #[error("domain rejected event: {0}")]
    Domain(#[from] pitcrew_domain::TransitionError),
    #[error("job does not exist")]
    Missing,
    #[error("idempotency key was reused for different input")]
    IdempotencyConflict,
    #[error("media item is already owned by another job")]
    ResourceBusy,
    #[error("budget envelope exceeds an installation or viewer limit")]
    BudgetExhausted,
    #[error("media item cooldown remains active until {until}")]
    ItemCooldown { until: u64 },
    #[error("authenticated administrator authority is required")]
    AdminRequired,
    #[error("file identity is stale, unknown, or mismatched")]
    IdentityMismatch,
    #[error("worker lease is active")]
    LeaseBusy,
    #[error("worker lease is expired or fenced")]
    Fenced,
    #[error("job revision changed")]
    RevisionConflict,
    #[error("invalid store input")]
    InvalidInput,
    #[error("journal integrity check failed")]
    Corrupt,
}

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl NewJob {
    fn build(&self) -> Result<FixJob> {
        Ok(FixJob::new_with_timeouts(
            self.id,
            self.library,
            self.original,
            self.constraints.clone(),
            self.policy,
            self.timeouts,
            self.created_at,
        )?)
    }
}

fn integer(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| StoreError::InvalidInput)
}
fn unsigned(value: i64) -> Result<u64> {
    u64::try_from(value).map_err(|_| StoreError::Corrupt)
}

impl Store {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        MIGRATOR.run(&pool).await?;
        Ok(Self { pool })
    }

    pub async fn health(&self) -> Result<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    pub async fn load_job(&self, id: u64) -> Result<StoredJob> {
        let mut tx = self.pool.begin().await?;
        let stored = load(&mut tx, id).await?;
        tx.commit().await?;
        Ok(stored)
    }

    /// The currently active owner of an exact resource, without scanning history.
    pub async fn owned_job(&self, resource: &str) -> Result<Option<u64>> {
        sqlx::query_scalar::<_, i64>("SELECT job FROM ownership WHERE resource=?")
            .bind(resource)
            .fetch_optional(&self.pool)
            .await?
            .map(unsigned)
            .transpose()
    }

    /// Bounded scheduler cursor. Resweep from zero after the end to revisit
    /// active jobs and late notification intents without starving newer jobs.
    pub async fn jobs_page(&self, after: u64, limit: u32) -> Result<Vec<u64>> {
        sqlx::query_scalar::<_, i64>("SELECT id FROM jobs WHERE id>? ORDER BY id LIMIT ?")
            .bind(integer(after)?)
            .bind(limit.min(100))
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(unsigned)
            .collect()
    }

    pub async fn jobs(&self) -> Result<Vec<u64>> {
        sqlx::query_scalar::<_, i64>("SELECT id FROM jobs ORDER BY id")
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(unsigned)
            .collect()
    }

    pub async fn create_job(
        &self,
        spec: &NewJob,
        key: &str,
        limits: BudgetLimits,
    ) -> Result<StoredJob> {
        self.create_job_with_cooldown(spec, key, limits, 0).await
    }

    /// Reserves cooldown, ownership, and budget in one transaction. Every
    /// creation path honors an existing cooldown, including `create_job`.
    pub async fn create_job_with_cooldown(
        &self,
        spec: &NewJob,
        key: &str,
        limits: BudgetLimits,
        cooldown_seconds: u64,
    ) -> Result<StoredJob> {
        let job = spec.build()?;
        if key.is_empty() || spec.viewer.is_empty() || spec.resource.is_empty() {
            return Err(StoreError::InvalidInput);
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(existing) =
            sqlx::query("SELECT id, spec FROM jobs WHERE create_key = ? OR id = ?")
                .bind(key)
                .bind(integer(spec.id)?)
                .fetch_optional(&mut *tx)
                .await?
        {
            let existing_spec: NewJob = serde_json::from_str(existing.get("spec"))?;
            if existing_spec != *spec {
                return Err(StoreError::IdempotencyConflict);
            }
            let existing_key: String =
                sqlx::query_scalar("SELECT create_key FROM jobs WHERE id = ?")
                    .bind(integer(spec.id)?)
                    .fetch_one(&mut *tx)
                    .await?;
            if existing_key != key {
                return Err(StoreError::IdempotencyConflict);
            }
            return load(&mut tx, unsigned(existing.get("id"))?).await;
        }
        if sqlx::query_scalar::<_, i64>("SELECT job FROM ownership WHERE resource = ?")
            .bind(&spec.resource)
            .fetch_optional(&mut *tx)
            .await?
            .is_some()
        {
            return Err(StoreError::ResourceBusy);
        }
        if let Some(until) =
            sqlx::query_scalar::<_, i64>("SELECT until_at FROM item_cooldowns WHERE resource=?")
                .bind(&spec.resource)
                .fetch_optional(&mut *tx)
                .await?
        {
            let until = unsigned(until)?;
            if spec.created_at < until {
                return Err(StoreError::ItemCooldown { until });
            }
        }
        reserve_budget(&mut tx, spec, limits).await?;
        if cooldown_seconds > 0 {
            let until = spec
                .created_at
                .checked_add(cooldown_seconds)
                .ok_or(StoreError::InvalidInput)?;
            sqlx::query("INSERT INTO item_cooldowns(resource,until_at) VALUES(?,?) ON CONFLICT(resource) DO UPDATE SET until_at=excluded.until_at")
                .bind(&spec.resource).bind(integer(until)?).execute(&mut *tx).await?;
        }
        sqlx::query("INSERT INTO jobs(id,create_key,spec) VALUES(?,?,?)")
            .bind(integer(spec.id)?)
            .bind(key)
            .bind(serde_json::to_string(spec)?)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO ownership(resource,job) VALUES(?,?)")
            .bind(&spec.resource)
            .bind(integer(spec.id)?)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO budget_reservations(job,viewer,at,units) VALUES(?,?,?,?)")
            .bind(integer(spec.id)?)
            .bind(&spec.viewer)
            .bind(integer(spec.created_at)?)
            .bind(spec.policy.max_grabs)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(StoredJob {
            job,
            revision: 0,
            spec: spec.clone(),
        })
    }

    pub async fn acquire_lease(&self, id: u64, owner: &str, now: u64, ttl: u64) -> Result<Lease> {
        if owner.is_empty() || ttl == 0 {
            return Err(StoreError::InvalidInput);
        }
        let until = now.checked_add(ttl).ok_or(StoreError::InvalidInput)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query("SELECT lease_until, lease_fence FROM jobs WHERE id = ?")
            .bind(integer(id)?)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::Missing)?;
        if unsigned(row.get("lease_until"))? > now {
            return Err(StoreError::LeaseBusy);
        }
        let fence = unsigned(row.get("lease_fence"))?
            .checked_add(1)
            .ok_or(StoreError::Corrupt)?;
        sqlx::query("UPDATE jobs SET lease_owner=?,lease_fence=?,lease_until=? WHERE id=?")
            .bind(owner)
            .bind(integer(fence)?)
            .bind(integer(until)?)
            .bind(integer(id)?)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Lease {
            job: id,
            owner: owner.into(),
            fence,
            expires_at: until,
        })
    }

    /// Release only this still-current lease; a stale holder cannot release a
    /// successor's token. The next acquisition always increments the fence.
    pub async fn release_lease(&self, lease: &Lease, now: u64) -> Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        check_lease(&mut tx, lease, now).await?;
        sqlx::query("UPDATE jobs SET lease_owner=NULL,lease_until=? WHERE id=?")
            .bind(integer(now)?)
            .bind(integer(lease.job)?)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Replay preserves the original observation clock while checking fencing
    /// against the current processing clock. This does not authorize dispatch.
    pub async fn apply_recorded_event(
        &self,
        lease: &Lease,
        revision: u64,
        key: &str,
        event: Event,
        event_at: u64,
        lease_now: u64,
    ) -> Result<Applied> {
        self.transition(lease, revision, key, event, (event_at, lease_now), None)
            .await
    }

    pub async fn apply_event(
        &self,
        lease: &Lease,
        revision: u64,
        key: &str,
        event: Event,
        now: u64,
    ) -> Result<Applied> {
        self.transition(lease, revision, key, event, (now, now), None)
            .await
    }

    pub async fn apply_result(
        &self,
        lease: &Lease,
        revision: u64,
        key: &str,
        event: Event,
        now: u64,
        completed: IntentKey,
    ) -> Result<Applied> {
        self.transition(lease, revision, key, event, (now, now), Some(completed))
            .await
    }

    async fn transition(
        &self,
        lease: &Lease,
        revision: u64,
        key: &str,
        event: Event,
        clocks: (u64, u64),
        completed: Option<IntentKey>,
    ) -> Result<Applied> {
        let (now, lease_now) = clocks;
        if key.is_empty() {
            return Err(StoreError::InvalidInput);
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        check_lease(&mut tx, lease, lease_now).await?;
        let mut stored = load(&mut tx, lease.job).await?;
        let json = serde_json::to_string(&event)?;
        if let Some(previous) = sqlx::query("SELECT event FROM events WHERE job=? AND event_key=?")
            .bind(integer(lease.job)?)
            .bind(key)
            .fetch_optional(&mut *tx)
            .await?
        {
            if previous.get::<&str, _>("event") != json {
                return Err(StoreError::IdempotencyConflict);
            }
            return Ok(Applied {
                stored,
                intents: vec![],
                duplicate: true,
            });
        }
        if stored.revision != revision {
            return Err(StoreError::RevisionConflict);
        }
        let transition = stored.job.apply(event, now)?;
        if let Some(intent) = completed {
            if intent.job != lease.job {
                return Err(StoreError::InvalidInput);
            }
            let changed = sqlx::query("UPDATE outbox SET status='completed' WHERE job=? AND sequence=? AND status IN ('started','uncertain')")
                .bind(integer(intent.job)?).bind(integer(intent.sequence)?).execute(&mut *tx).await?.rows_affected();
            if changed != 1 {
                return Err(StoreError::InvalidInput);
            }
        }
        if transition.job.reserved_budget() > stored.job.reserved_budget() {
            sqlx::query("UPDATE budget_reservations SET at=? WHERE job=?")
                .bind(integer(now)?)
                .bind(integer(lease.job)?)
                .execute(&mut *tx)
                .await?;
        }
        stored.job = transition.job;
        stored.revision = stored.revision.checked_add(1).ok_or(StoreError::Corrupt)?;
        sqlx::query("INSERT INTO events(job,revision,event_key,at,event) VALUES(?,?,?,?,?)")
            .bind(integer(lease.job)?)
            .bind(integer(stored.revision)?)
            .bind(key)
            .bind(integer(now)?)
            .bind(json)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE jobs SET revision=? WHERE id=?")
            .bind(integer(stored.revision)?)
            .bind(integer(lease.job)?)
            .execute(&mut *tx)
            .await?;
        if stored.job.is_terminal() {
            sqlx::query("UPDATE outbox SET status='suppressed' WHERE job=? AND status='pending'")
                .bind(integer(lease.job)?)
                .execute(&mut *tx)
                .await?;
            if stored.job.active_grabs() == 0 && stored.job.unresolved_mutation().is_none() {
                sqlx::query("DELETE FROM ownership WHERE job=?")
                    .bind(integer(lease.job)?)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        for intent in &transition.intents {
            sqlx::query("INSERT INTO outbox(job,sequence,intent) VALUES(?,?,?)")
                .bind(integer(intent.job)?)
                .bind(integer(intent.sequence)?)
                .bind(serde_json::to_string(intent)?)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(Applied {
            stored,
            intents: transition.intents,
            duplicate: false,
        })
    }

    pub async fn outbox(&self, job: u64) -> Result<Vec<OutboxEntry>> {
        let rows =
            sqlx::query("SELECT intent,status,fence FROM outbox WHERE job=? ORDER BY sequence")
                .bind(integer(job)?)
                .fetch_all(&self.pool)
                .await?;
        rows.into_iter()
            .map(|row| {
                Ok(OutboxEntry {
                    intent: serde_json::from_str(row.get("intent"))?,
                    status: serde_json::from_value(serde_json::Value::String(row.get("status")))?,
                    fence: row
                        .get::<Option<i64>, _>("fence")
                        .map(unsigned)
                        .transpose()?,
                })
            })
            .collect()
    }

    pub async fn start_intent(
        &self,
        lease: &Lease,
        intent: IntentKey,
        revision: u64,
        now: u64,
    ) -> Result<bool> {
        if intent.job != lease.job {
            return Err(StoreError::InvalidInput);
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if check_lease(&mut tx, lease, now).await? != revision {
            return Err(StoreError::RevisionConflict);
        }
        let changed = sqlx::query("UPDATE outbox SET status='started',fence=? WHERE job=? AND sequence=? AND status='pending'")
            .bind(integer(lease.fence)?).bind(integer(intent.job)?).bind(integer(intent.sequence)?).execute(&mut *tx).await?.rows_affected();
        tx.commit().await?;
        Ok(changed == 1)
    }

    pub async fn finish_intent(
        &self,
        lease: &Lease,
        intent: IntentKey,
        status: OutboxStatus,
        now: u64,
    ) -> Result<()> {
        if intent.job != lease.job
            || !matches!(
                status,
                OutboxStatus::Completed
                    | OutboxStatus::Uncertain
                    | OutboxStatus::Suppressed
                    | OutboxStatus::Shadowed
            )
        {
            return Err(StoreError::InvalidInput);
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        check_lease(&mut tx, lease, now).await?;
        let changed = sqlx::query("UPDATE outbox SET status=? WHERE job=? AND sequence=? AND status IN ('pending','started','uncertain')")
            .bind(serde_json::to_value(status)?.as_str().ok_or(StoreError::Corrupt)?)
            .bind(integer(intent.job)?).bind(integer(intent.sequence)?).execute(&mut *tx).await?.rows_affected();
        if changed != 1 {
            return Err(StoreError::InvalidInput);
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn ingest_observation(
        &self,
        source: &str,
        key: &str,
        payload: &serde_json::Value,
        now: u64,
    ) -> Result<bool> {
        if source.is_empty() || key.is_empty() {
            return Err(StoreError::InvalidInput);
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let json = serde_json::to_string(payload)?;
        if let Some(previous) = sqlx::query_scalar::<_, String>(
            "SELECT payload FROM observations WHERE source=? AND observation_key=?",
        )
        .bind(source)
        .bind(key)
        .fetch_optional(&mut *tx)
        .await?
        {
            if previous != json {
                return Err(StoreError::IdempotencyConflict);
            }
            return Ok(false);
        }
        sqlx::query("INSERT INTO observations(source,observation_key,payload,at) VALUES(?,?,?,?)")
            .bind(source)
            .bind(key)
            .bind(json)
            .bind(integer(now)?)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn observations_after(
        &self,
        after: u64,
        limit: u32,
    ) -> Result<Vec<ObservationRecord>> {
        if limit == 0 || limit > 1000 {
            return Err(StoreError::InvalidInput);
        }
        let rows = sqlx::query("SELECT * FROM observations WHERE id > ? ORDER BY id LIMIT ?")
            .bind(integer(after)?)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(observation_row).collect()
    }

    pub async fn observations(&self) -> Result<Vec<ObservationRecord>> {
        let rows = sqlx::query("SELECT * FROM observations ORDER BY id")
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(observation_row).collect()
    }
}

fn observation_row(row: sqlx::sqlite::SqliteRow) -> Result<ObservationRecord> {
    Ok(ObservationRecord {
        id: unsigned(row.get("id"))?,
        source: row.get("source"),
        key: row.get("observation_key"),
        payload: serde_json::from_str(row.get("payload"))?,
        at: unsigned(row.get("at"))?,
    })
}

async fn check_lease(connection: &mut SqliteConnection, lease: &Lease, now: u64) -> Result<u64> {
    let row =
        sqlx::query("SELECT lease_owner,lease_fence,lease_until,revision FROM jobs WHERE id=?")
            .bind(integer(lease.job)?)
            .fetch_optional(connection)
            .await?
            .ok_or(StoreError::Missing)?;
    if row.get::<Option<&str>, _>("lease_owner") != Some(lease.owner.as_str())
        || unsigned(row.get("lease_fence"))? != lease.fence
        || unsigned(row.get("lease_until"))? <= now
    {
        return Err(StoreError::Fenced);
    }
    unsigned(row.get("revision"))
}

async fn load(connection: &mut SqliteConnection, id: u64) -> Result<StoredJob> {
    let row = sqlx::query("SELECT spec,revision FROM jobs WHERE id=?")
        .bind(integer(id)?)
        .fetch_optional(&mut *connection)
        .await?
        .ok_or(StoreError::Missing)?;
    let spec: NewJob = serde_json::from_str(row.get("spec"))?;
    let mut job = spec.build()?;
    let rows = sqlx::query("SELECT revision,at,event FROM events WHERE job=? ORDER BY revision")
        .bind(integer(id)?)
        .fetch_all(connection)
        .await?;
    let mut revision = 0;
    for event in rows {
        revision += 1;
        if unsigned(event.get("revision"))? != revision {
            return Err(StoreError::Corrupt);
        }
        job = job
            .apply(
                serde_json::from_str(event.get("event"))?,
                unsigned(event.get("at"))?,
            )?
            .job;
    }
    if revision != unsigned(row.get("revision"))? || spec.id != id {
        return Err(StoreError::Corrupt);
    }
    Ok(StoredJob {
        job,
        revision,
        spec,
    })
}

async fn reserve_budget(
    connection: &mut SqliteConnection,
    spec: &NewJob,
    limits: BudgetLimits,
) -> Result<()> {
    for (duration, global, viewer) in [
        (3600, limits.global_hour, limits.viewer_hour),
        (86400, limits.global_day, limits.viewer_day),
    ] {
        let since = spec.created_at.saturating_sub(duration - 1);
        let rows = sqlx::query("SELECT viewer,units FROM budget_reservations WHERE at >= ? OR job IN (SELECT job FROM ownership)")
            .bind(integer(since)?)
            .fetch_all(&mut *connection)
            .await?;
        let mut global_used = u64::from(spec.policy.max_grabs);
        let mut viewer_used = global_used;
        for row in rows {
            let units = unsigned(row.get("units"))?;
            global_used = global_used.checked_add(units).ok_or(StoreError::Corrupt)?;
            if row.get::<&str, _>("viewer") == spec.viewer {
                viewer_used = viewer_used.checked_add(units).ok_or(StoreError::Corrupt)?;
            }
        }
        if global_used > u64::from(global) || viewer_used > u64::from(viewer) {
            return Err(StoreError::BudgetExhausted);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
