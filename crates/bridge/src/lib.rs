//! Diagnosis-to-engine seam. Recommendations alone are never authority to fix.
//! The host records fresh local access and exact-byte provenance separately;
//! SQLite freezes each intended job before the engine transaction is attempted.
pub mod admin;
pub mod lifecycle;
use pitcrew_deciders::{
    DecisionReport,
    reply::{RedactedReplyFacts, ReplyWriter, TemplateReplyWriter},
};
use pitcrew_diagnosis::{
    facts::{DecodeOutcome, EvidenceFacts, ProbeOutcome, Scope},
    rules::{Decision, RecommendedAction, ReplacementConstraint},
};
use pitcrew_domain::{
    Access, Constraints, Event, FileIdentity, Policy, StageTimeouts, State,
    channels::ChannelIdentity,
};
use pitcrew_store::{BudgetLimits, NewJob, Store, StoreError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};
use std::{path::Path, time::Duration};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid bridge input")]
    Invalid,
    #[error("bridge database unavailable")]
    Database(#[from] sqlx::Error),
    #[error("bridge data invalid")]
    Json(#[from] serde_json::Error),
    #[error("intake unavailable")]
    Intake(#[from] pitcrew_intake_store::Error),
    #[error("engine store unavailable")]
    Engine(#[from] StoreError),
}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug)]
pub struct Options {
    pub policy: Policy,
    pub timeouts: StageTimeouts,
    pub budgets: BudgetLimits,
    pub cooldown_seconds: u64,
    pub evidence_max_age: u64,
}

/// Locally collected, exact, current evidence. Never accept this structure from
/// viewer JSON. The trusted host supplies it after authenticating the account,
/// refreshing access and reading the currently installed arr/agent identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Authority {
    pub account: String,
    pub resource: String,
    pub library: u64,
    pub arr_instance_id: String,
    pub arr_item_id: u64,
    pub arr_file_id: u64,
    pub file: FileIdentity,
    pub observed_at: u64,
    pub access: Access,
    pub probe: ProbeOutcome,
    pub decode: DecodeOutcome,
    /// Authenticated transport routing; the Plex account is not a chat user ID.
    pub route: Option<ViewerRoute>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerRoute {
    pub recipient: ChannelIdentity,
    pub conversation: String,
    pub last_inbound: Option<u64>,
}

/// Tested SQL representation: agent_file is FileIdentity JSON; decision accepts
/// either P4 Decision or DecisionReport JSON. W1 is the sole writer of diagnoses.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnosis {
    pub report_id: i64,
    pub decided_at: u64,
    pub decision: Decision,
    pub evidence_summary: EvidenceFacts,
    pub resource: String,
    pub library_id: u64,
    pub arr_instance_id: String,
    pub arr_item_id: u64,
    pub arr_file_id: u64,
    pub agent_file: FileIdentity,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressState {
    Received,
    NeedsReview,
    Advice,
    Working,
    Fixed,
    CouldNotFix,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub report_id: i64,
    pub job_id: Option<u64>,
    pub state: ProgressState,
    pub updated_at: u64,
    pub viewer_copy_key: String,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Tick {
    pub examined: usize,
    pub linked: usize,
    pub held: usize,
    pub projected: usize,
}
#[derive(Clone)]
pub struct Bridge {
    pool: SqlitePool,
    engine: Store,
    options: Options,
}

impl Bridge {
    pub async fn open(path: impl AsRef<Path>, engine: Store, options: Options) -> Result<Self> {
        if options.cooldown_seconds == 0 || options.evidence_max_age == 0 {
            return Err(Error::Invalid);
        }
        // Preserve the intake journal's private file/key/migration checks.
        pitcrew_intake_store::Store::open(path.as_ref()).await?;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(path)
                    .foreign_keys(true)
                    .journal_mode(SqliteJournalMode::Wal)
                    .synchronous(SqliteSynchronous::Full)
                    .busy_timeout(Duration::from_secs(5)),
            )
            .await?;
        sqlx::raw_sql(include_str!("schema.sql"))
            .execute(&pool)
            .await?;
        // Upgrade the first bridge schema without touching W1's tables.
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        let columns = sqlx::query("PRAGMA table_info(report_progress)")
            .fetch_all(&mut *tx)
            .await?;
        if !columns
            .iter()
            .any(|row| row.get::<String, _>("name") == "job_revision")
        {
            sqlx::query(
                "ALTER TABLE report_progress ADD COLUMN job_revision INTEGER NOT NULL DEFAULT 0",
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(Self {
            pool,
            engine,
            options,
        })
    }

    /// Update trusted provenance. Existing report/account binding is checked here
    /// and again at consumption; it never installs an access grant in the engine.
    pub async fn record_authority(&self, report: i64, authority: &Authority) -> Result<()> {
        let account: String = sqlx::query_scalar("SELECT account FROM web_intakes WHERE id=?")
            .bind(report)
            .fetch_one(&self.pool)
            .await?;
        if account != authority.account
            || authority.resource.is_empty()
            || authority.arr_instance_id.is_empty()
        {
            return Err(Error::Invalid);
        }
        sqlx::query("INSERT INTO bridge_authority(report_id,authority) VALUES (?,?) ON CONFLICT(report_id) DO UPDATE SET authority=excluded.authority")
            .bind(report).bind(serde_json::to_string(authority)?).execute(&self.pool).await?;
        Ok(())
    }

    /// Revoke previously saved provenance before a trusted refresh. A failed
    /// refresh must not leave an older grant available to pending commands.
    pub async fn clear_authority(&self, report: i64) -> Result<()> {
        sqlx::query("DELETE FROM bridge_authority WHERE report_id=?")
            .bind(report)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Project an unavailable pre-diagnosis prerequisite without erasing an
    /// existing job association or a newer projection.
    pub async fn hold_report(&self, report: i64, now: u64) -> Result<()> {
        self.save_progress(report, None, ProgressState::NeedsReview, now)
            .await?;
        Ok(())
    }

    pub async fn progress(&self, report: i64, account: &str) -> Result<Option<Progress>> {
        let row = sqlx::query("SELECT p.* FROM report_progress p JOIN web_intakes i ON i.id=p.report_id WHERE p.report_id=? AND i.account=?")
            .bind(report).bind(account).fetch_optional(&self.pool).await?;
        row.map(progress_row).transpose()
    }

    /// Bounded pass; malformed/unauthorized rows do not prevent other reports
    /// from progressing. Missing W1 schema is idle, never a fabricated diagnosis.
    pub async fn tick(&self, now: u64) -> Result<Tick> {
        let mut result = Tick::default();
        sqlx::query("INSERT INTO report_progress(report_id,state,updated_at,viewer_copy_key) SELECT id,'received',?,'report.received' FROM web_intakes WHERE json_valid(response) AND json_extract(response,'$.outcome')='report' AND NOT EXISTS(SELECT 1 FROM report_progress p WHERE p.report_id=web_intakes.id) ORDER BY id LIMIT 128 ON CONFLICT(report_id) DO NOTHING")
            .bind(integer(now)?).execute(&self.pool).await?;
        let pending: Vec<i64> = sqlx::query_scalar("SELECT b.report_id FROM bridge_reports b LEFT JOIN report_progress p ON p.report_id=b.report_id WHERE p.job_id IS NULL ORDER BY COALESCE(p.updated_at,0),b.report_id LIMIT 128")
            .fetch_all(&self.pool).await?;
        for report in pending {
            if self.recover(report, now).await?.is_none() {
                // Rotate failed/uncommitted commands too, so older holds cannot
                // starve recovery of a later committed job beyond this batch.
                sqlx::query("INSERT INTO report_progress(report_id,state,updated_at,viewer_copy_key) VALUES (?,'needs_review',?,'report.needs_review') ON CONFLICT(report_id) DO UPDATE SET updated_at=excluded.updated_at WHERE report_progress.job_id IS NULL AND report_progress.updated_at<=excluded.updated_at")
                    .bind(report).bind(integer(now)?).execute(&self.pool).await?;
            }
        }
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='diagnoses')",
        )
        .fetch_one(&self.pool)
        .await?;
        if exists {
            let rows = sqlx::query("SELECT d.* FROM diagnoses d LEFT JOIN report_progress p ON p.report_id=d.report_id WHERE p.job_id IS NULL AND (p.state IS NULL OR p.state!='advice') ORDER BY COALESCE(p.updated_at,0), d.report_id LIMIT 128")
                .fetch_all(&self.pool).await?;
            for row in rows {
                result.examined += 1;
                let report: i64 = row.try_get("report_id")?;
                match diagnosis_row(row) {
                    Ok(diagnosis) => {
                        let progress = self.consume(&diagnosis, now).await?;
                        if progress.job_id.is_some() {
                            result.linked += 1;
                        } else {
                            result.held += 1;
                        }
                    }
                    Err(_) => {
                        self.save_progress(report, None, ProgressState::NeedsReview, now)
                            .await?;
                        result.held += 1;
                    }
                }
            }
        }
        result.projected = self.project(now).await?;
        Ok(result)
    }

    /// Also useful to hosts with a typed W1 reader. This still requires the
    /// independently recorded authority and a matching durable intake account.
    pub async fn consume(&self, d: &Diagnosis, now: u64) -> Result<Progress> {
        let account: String = sqlx::query_scalar("SELECT account FROM web_intakes WHERE id=?")
            .bind(d.report_id)
            .fetch_one(&self.pool)
            .await?;
        if let Some(p) = self.progress(d.report_id, &account).await?
            && p.job_id.is_some()
        {
            return self
                .save_progress(d.report_id, p.job_id, p.state, now)
                .await;
        }
        if let Some(progress) = self.recover(d.report_id, now).await? {
            return Ok(progress);
        }
        if self.report_stopped(d.report_id).await? {
            return self
                .save_progress(d.report_id, None, ProgressState::NeedsReview, now)
                .await;
        }
        if d.decision.action != RecommendedAction::Replace {
            return self
                .save_progress(d.report_id, None, ProgressState::Advice, now)
                .await;
        }
        let auth: Option<String> =
            sqlx::query_scalar("SELECT authority FROM bridge_authority WHERE report_id=?")
                .bind(d.report_id)
                .fetch_optional(&self.pool)
                .await?;
        let auth = auth
            .map(|v| serde_json::from_str::<Authority>(&v))
            .transpose()?;
        let Some(a) = auth.filter(|a| valid(d, a, &account, now, self.options.evidence_max_age))
        else {
            return self
                .save_progress(d.report_id, None, ProgressState::NeedsReview, now)
                .await;
        };
        let Some(constraints) = constraints(d.decision.constraint) else {
            return self
                .save_progress(d.report_id, None, ProgressState::NeedsReview, now)
                .await;
        };
        // P1 agent identities and P3 arr/registry identities are distinct.
        // Only the trusted collector registers full current arr metadata; reads
        // here cannot refresh or reinstall an old original after replacement.
        let Some(registered) = self
            .engine
            .registered_file(&d.arr_instance_id, &d.resource)
            .await?
        else {
            return self
                .save_progress(d.report_id, None, ProgressState::NeedsReview, now)
                .await;
        };
        if registered.fingerprint.arr_file_id != d.arr_file_id
            || registered.fingerprint.agent != d.agent_file
        {
            return self
                .save_progress(d.report_id, None, ProgressState::NeedsReview, now)
                .await;
        }
        match self
            .engine
            .validate_file(
                &d.arr_instance_id,
                &d.resource,
                registered.identity,
                &registered.fingerprint,
                now,
                self.options.evidence_max_age,
            )
            .await
        {
            Ok(()) => {}
            Err(StoreError::IdentityMismatch) => {
                return self
                    .save_progress(d.report_id, None, ProgressState::NeedsReview, now)
                    .await;
            }
            Err(error) => return Err(error.into()),
        }
        let key = request_key(d, &constraints, registered.identity)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let stopped: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM bridge_report_controls WHERE report_id=?)",
        )
        .bind(d.report_id)
        .fetch_one(&mut *tx)
        .await?;
        if stopped {
            tx.rollback().await?;
            return self
                .save_progress(d.report_id, None, ProgressState::NeedsReview, now)
                .await;
        }
        let proposed = NewJob {
            id: job_id(&key),
            viewer: a.account.clone(),
            resource: d.resource.clone(),
            library: d.library_id,
            original: registered.identity,
            constraints,
            policy: self.options.policy,
            timeouts: self.options.timeouts,
            created_at: now,
        };
        let limits = [
            self.options.budgets.global_hour,
            self.options.budgets.global_day,
            self.options.budgets.viewer_hour,
            self.options.budgets.viewer_day,
        ];
        sqlx::query("INSERT INTO bridge_requests(key,spec,limits,cooldown) VALUES (?,?,?,?) ON CONFLICT(key) DO NOTHING")
            .bind(&key).bind(serde_json::to_string(&proposed)?).bind(serde_json::to_string(&limits)?)
            .bind(integer(self.options.cooldown_seconds)?).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO bridge_reports(report_id,request_key) VALUES (?,?) ON CONFLICT(report_id) DO NOTHING")
            .bind(d.report_id).bind(&key).execute(&mut *tx).await?;
        let row = sqlx::query("SELECT r.spec,r.limits,r.cooldown FROM bridge_requests r JOIN bridge_reports b ON b.request_key=r.key WHERE b.report_id=? AND b.request_key=?")
            .bind(d.report_id).bind(&key).fetch_optional(&mut *tx).await?;
        let Some(row) = row else {
            tx.rollback().await?;
            return self
                .save_progress(d.report_id, None, ProgressState::NeedsReview, now)
                .await;
        };
        let spec: NewJob = serde_json::from_str(row.get("spec"))?;
        let _reserved_limits: [u32; 4] = serde_json::from_str(row.get("limits"))?;
        let cooldown = unsigned(row.get("cooldown"))?;
        tx.commit().await?;
        // The command may already have committed. A never-committed old command
        // must not reserve budgets using a historical creation timestamp.
        match self.engine.load_job(spec.id).await {
            Ok(_) => {}
            Err(StoreError::Missing) if spec.created_at != now => {
                return self
                    .save_progress(d.report_id, None, ProgressState::NeedsReview, now)
                    .await;
            }
            Err(StoreError::Missing) => {
                if spec.policy.max_grabs > self.options.policy.max_grabs {
                    return self
                        .save_progress(d.report_id, None, ProgressState::NeedsReview, now)
                        .await;
                }
            }
            Err(e) => return Err(e.into()),
        }
        match self
            .engine
            .create_job_with_cooldown(
                &spec,
                &key,
                self.options.budgets,
                cooldown.max(self.options.cooldown_seconds),
            )
            .await
        {
            Ok(_) => {}
            Err(
                StoreError::BudgetExhausted
                | StoreError::ItemCooldown { .. }
                | StoreError::ResourceBusy,
            ) => {
                return self
                    .save_progress(d.report_id, None, ProgressState::NeedsReview, now)
                    .await;
            }
            Err(e) => return Err(e.into()),
        }
        // This evidence-only event emits no upstream side effect. A later
        // candidate selection still needs fresh access, exclusions and budget.
        let stored = self.engine.load_job(spec.id).await?;
        if stored.job.state() == State::Picked {
            match self.engine.acquire_lease(spec.id, "bridge", now, 30).await {
                Ok(lease) => {
                    let result = self
                        .engine
                        .apply_event(
                            &lease,
                            stored.revision,
                            "bridge:broken-current",
                            Event::BrokenFileConfirmed {
                                file: spec.original,
                            },
                            now,
                        )
                        .await;
                    self.engine.release_lease(&lease, now).await?;
                    match result {
                        Ok(_) | Err(StoreError::RevisionConflict) => {}
                        Err(e) => return Err(e.into()),
                    }
                }
                Err(StoreError::LeaseBusy) => {}
                Err(e) => return Err(e.into()),
            }
        }
        self.save_progress(d.report_id, Some(spec.id), ProgressState::Working, now)
            .await
    }

    async fn recover(&self, report: i64, now: u64) -> Result<Option<Progress>> {
        let spec: Option<String> = sqlx::query_scalar("SELECT r.spec FROM bridge_requests r JOIN bridge_reports b ON b.request_key=r.key WHERE b.report_id=?")
            .bind(report).fetch_optional(&self.pool).await?;
        let Some(spec) = spec else {
            return Ok(None);
        };
        let spec: NewJob = serde_json::from_str(&spec)?;
        let stored = match self.engine.load_job(spec.id).await {
            Ok(stored) => stored,
            Err(StoreError::Missing) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if stored.spec != spec {
            return Err(Error::Invalid);
        }
        // Recovery only observes a previously committed job. Expired authority
        // must not hide it, and cannot authorize a new reservation or side effect.
        Ok(Some(
            self.save_progress(report, Some(spec.id), state(stored.job.state()), now)
                .await?,
        ))
    }

    pub async fn project(&self, now: u64) -> Result<usize> {
        let rows = sqlx::query("SELECT report_id,job_id FROM report_progress WHERE job_id IS NOT NULL ORDER BY updated_at,report_id LIMIT 128")
            .fetch_all(&self.pool).await?;
        for row in &rows {
            let job = self.engine.load_job(unsigned(row.get("job_id"))?).await?;
            self.save_progress(
                row.get("report_id"),
                Some(job.spec.id),
                state(job.job.state()),
                now,
            )
            .await?;
        }
        Ok(rows.len())
    }

    async fn save_progress(
        &self,
        report: i64,
        job: Option<u64>,
        state: ProgressState,
        now: u64,
    ) -> Result<Progress> {
        let (state, revision) = match job {
            Some(id) => {
                let stored = self.engine.load_job(id).await?;
                (crate::state(stored.job.state()), stored.revision)
            }
            None => (state, 0),
        };
        let row = sqlx::query("INSERT INTO report_progress(report_id,job_id,state,updated_at,viewer_copy_key,job_revision) VALUES (?,?,?,?,?,?) ON CONFLICT(report_id) DO UPDATE SET job_id=excluded.job_id,state=excluded.state,updated_at=excluded.updated_at,viewer_copy_key=excluded.viewer_copy_key,job_revision=excluded.job_revision WHERE report_progress.updated_at<=excluded.updated_at AND report_progress.job_revision<=excluded.job_revision AND (report_progress.job_id IS NULL OR report_progress.job_id=excluded.job_id) RETURNING *")
            .bind(report).bind(job.map(integer).transpose()?).bind(serde_json::to_value(state)?.as_str().ok_or(Error::Invalid)?)
            .bind(integer(now)?).bind(copy_key(state)).bind(integer(revision)?).fetch_optional(&self.pool).await?;
        // A delayed observation cannot undo newer progress or erase an existing
        // job association. Return the committed projection, not the stale input.
        progress_row(match row {
            Some(row) => row,
            None => {
                sqlx::query("SELECT * FROM report_progress WHERE report_id=?")
                    .bind(report)
                    .fetch_one(&self.pool)
                    .await?
            }
        })
    }

    /// Freeze all currently linked, authenticated routes into the P3 terminal
    /// outbox before worker dispatch. Later reports still see progress, but do
    /// not rewrite a message already prepared or sent. Unknown routes stay
    /// unprepared for administrative review; never guess a platform identity.
    pub async fn prepare_notifications(&self, job: u64, notifier: Option<&str>) -> Result<usize> {
        use pitcrew_domain::{Effect, IntentKey};
        use pitcrew_store::delivery::{NotificationPayload, ViewerNotification};
        let stored = self.engine.load_job(job).await?;
        let text = self.reply_for_job(job).await?;
        let rows = sqlx::query("SELECT a.authority FROM bridge_authority a JOIN report_progress p ON p.report_id=a.report_id WHERE p.job_id=? ORDER BY p.report_id")
            .bind(integer(job)?).fetch_all(&self.pool).await?;
        let mut seen = std::collections::BTreeSet::new();
        let mut recipients = Vec::new();
        for row in rows {
            let a: Authority = serde_json::from_str(row.try_get("authority")?)?;
            if let Some(route) = a.route
                && seen.insert((
                    route.recipient.channel_id.clone(),
                    route.recipient.platform_user_id.clone(),
                ))
            {
                recipients.push(ViewerNotification {
                    recipient: route.recipient,
                    conversation: route.conversation,
                    last_inbound: route.last_inbound,
                    text: text.clone(),
                });
            }
        }
        let mut prepared = 0;
        for row in self.engine.outbox(job).await? {
            let key = IntentKey {
                job,
                sequence: row.intent.sequence,
            };
            let payload = match row.intent.effect {
                Effect::NotifyReady { .. }
                    if matches!(stored.job.state(), State::Verified | State::Done)
                        && !recipients.is_empty() =>
                {
                    NotificationPayload::Viewers {
                        recipients: recipients.clone(),
                    }
                }
                Effect::NotifyViewerFailure { .. }
                    if stored.job.state() == State::Failed && !recipients.is_empty() =>
                {
                    NotificationPayload::Viewers {
                        recipients: recipients.clone(),
                    }
                }
                Effect::NotifyAdmin { .. } if stored.job.state() == State::Failed => {
                    let Some(notifier) = notifier else {
                        continue;
                    };
                    NotificationPayload::Admin { notifier:notifier.into(), title:"Repair needs review".into(), body:"A repair could not complete. Review the job and its evidence in Pit Crew.".into() }
                }
                _ => continue,
            };
            match self.engine.notification_payload(key).await {
                Ok(_) => continue,
                Err(StoreError::Missing) => {}
                Err(e) => return Err(e.into()),
            }
            self.engine.prepare_notification(key, &payload).await?;
            prepared += 1;
        }
        Ok(prepared)
    }

    /// Render status from a trusted engine reconstruction. Intake JSON and
    /// selected decisions cannot create a ready statement.
    pub async fn reply_for_job(&self, job: u64) -> Result<String> {
        let stored = self.engine.load_job(job).await?;
        Ok(TemplateReplyWriter::default()
            .write(&RedactedReplyFacts::for_job(&stored.job))
            .await
            .delivery_text)
    }
}
fn valid(d: &Diagnosis, a: &Authority, account: &str, now: u64, age: u64) -> bool {
    let broken = d.evidence_summary.playback.decode == DecodeOutcome::Bad
        || matches!(
            d.evidence_summary.playback.probe,
            ProbeOutcome::OpenFailed | ProbeOutcome::NoVideo
        );
    broken
        && d.evidence_summary.scope != Scope::Show
        && !d.evidence_summary.injection_suspected
        && a.account == account
        && a.resource == d.resource
        && a.library == d.library_id
        && a.arr_instance_id == d.arr_instance_id
        && a.arr_item_id == d.arr_item_id
        && a.arr_file_id == d.arr_file_id
        && a.file == d.agent_file
        && a.probe == d.evidence_summary.playback.probe
        && a.decode == d.evidence_summary.playback.decode
        && d.agent_file.id > 0
        && d.agent_file.revision > 0
        && d.arr_item_id > 0
        && d.arr_file_id > 0
        && !d.resource.is_empty()
        && !d.arr_instance_id.is_empty()
        && a.observed_at <= d.decided_at
        && d.decided_at <= now
        && now - a.observed_at <= age
        && matches!(a.access, Access::Granted { library, expires_at } if library == d.library_id && expires_at > now)
}
fn constraints(c: Option<ReplacementConstraint>) -> Option<Constraints> {
    Some(match c? {
        ReplacementConstraint::AnyWorking => Constraints::default(),
        ReplacementConstraint::Sdr => Constraints {
            require_sdr: true,
            ..Constraints::default()
        },
        ReplacementConstraint::EnglishAudio => Constraints {
            language: Some("eng".into()),
            ..Constraints::default()
        },
        ReplacementConstraint::SpanishAudio => Constraints {
            language: Some("spa".into()),
            ..Constraints::default()
        },
        ReplacementConstraint::HdrOkNoProfile5 => Constraints {
            forbid_dolby_vision_profile5: true,
            ..Constraints::default()
        },
    })
}
fn request_key(d: &Diagnosis, c: &Constraints, registered: FileIdentity) -> Result<String> {
    let bytes = serde_json::to_vec(&(
        &d.resource,
        d.library_id,
        &d.arr_instance_id,
        d.arr_item_id,
        d.arr_file_id,
        d.agent_file,
        registered,
        c,
    ))?;
    Ok(format!("bridge:{}", hex::encode(Sha256::digest(bytes))))
}
fn job_id(key: &str) -> u64 {
    let digest = Sha256::digest(key.as_bytes());
    const BASE: u64 = 0x4000_0000_0000_0000;
    u64::from_be_bytes(digest[..8].try_into().expect("eight bytes")) % BASE + BASE
}
fn state(s: State) -> ProgressState {
    match s {
        State::Verified | State::Done => ProgressState::Fixed,
        State::Failed => ProgressState::CouldNotFix,
        _ => ProgressState::Working,
    }
}
fn copy_key(s: ProgressState) -> &'static str {
    match s {
        ProgressState::Received => "report.received",
        ProgressState::NeedsReview => "report.needs_review",
        ProgressState::Advice => "report.advice",
        ProgressState::Working => "report.working",
        ProgressState::Fixed => "report.fixed",
        ProgressState::CouldNotFix => "report.could_not_fix",
    }
}
fn progress_row(r: sqlx::sqlite::SqliteRow) -> Result<Progress> {
    Ok(Progress {
        report_id: r.try_get("report_id")?,
        job_id: r
            .try_get::<Option<i64>, _>("job_id")?
            .map(unsigned)
            .transpose()?,
        state: serde_json::from_value(serde_json::Value::String(r.try_get("state")?))?,
        updated_at: unsigned(r.try_get("updated_at")?)?,
        viewer_copy_key: r.try_get("viewer_copy_key")?,
    })
}
fn diagnosis_row(r: sqlx::sqlite::SqliteRow) -> Result<Diagnosis> {
    let raw: &str = r.try_get("decision")?;
    let decision = serde_json::from_str::<Decision>(raw)
        .or_else(|_| serde_json::from_str::<DecisionReport>(raw).map(|v| v.decision))?;
    let library: String = r
        .try_get::<String, _>("library_id")
        .or_else(|_| r.try_get::<i64, _>("library_id").map(|v| v.to_string()))?;
    Ok(Diagnosis {
        report_id: r.try_get("report_id")?,
        decided_at: unsigned(r.try_get("decided_at")?)?,
        decision,
        evidence_summary: serde_json::from_str(r.try_get("evidence_summary")?)?,
        resource: r.try_get("resource")?,
        library_id: library.parse().map_err(|_| Error::Invalid)?,
        arr_instance_id: r.try_get("arr_instance_id")?,
        arr_item_id: unsigned(r.try_get("arr_item_id")?)?,
        arr_file_id: unsigned(r.try_get("arr_file_id")?)?,
        agent_file: serde_json::from_str(r.try_get("agent_file")?)?,
    })
}
fn integer(v: u64) -> Result<i64> {
    i64::try_from(v).map_err(|_| Error::Invalid)
}
fn unsigned(v: i64) -> Result<u64> {
    u64::try_from(v).map_err(|_| Error::Invalid)
}

#[cfg(test)]
mod tests;
