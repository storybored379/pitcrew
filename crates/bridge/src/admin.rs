//! The owner UI writes intents; this consumer claims and audits each execution.
//! A crash after claim is uncertain, never an invitation to repeat a mutation.
use crate::{Bridge, Diagnosis, Error, ProgressState, Result, diagnosis_row, integer};
use pitcrew_diagnosis::rules::RecommendedAction;
use pitcrew_domain::delivery::DeliveryPurpose;
use pitcrew_store::policy::{AdminAction, PolicyRole};
use serde::Deserialize;
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{collections::BTreeSet, path::Path, time::Duration};

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Command {
    Approve { report: i64, decided_at: u64 },
    Deny { report: i64, decided_at: u64 },
    LowerQuality { job: u64, revision: u64 },
    Close { report: i64 },
    Message { report: i64, template: Template },
    Pin { id: u64 },
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Template {
    Received,
    NeedDetails,
    UnderReview,
}
impl Template {
    fn text(&self) -> &'static str {
        match self {
            Self::Received => "We received your report and will review it.",
            Self::NeedDetails => {
                "Please reply with the episode or movie, your device, and what happened when you tried to play it."
            }
            Self::UnderReview => {
                "Your report is under review. We will update you when we have a verified result."
            }
        }
    }
}

#[derive(Clone)]
pub struct Consumer {
    pool: SqlitePool,
    bridge: Bridge,
    admins: BTreeSet<String>,
    lower_quality_shadow: bool,
    lower_quality_minimum: Option<u32>,
}
struct Request {
    key: String,
    actor: String,
    command: String,
    note: String,
}
struct Outcome {
    state: &'static str,
    reason: &'static str,
}
impl Outcome {
    fn applied(reason: &'static str) -> Self {
        Self {
            state: "controller_applied",
            reason,
        }
    }
    fn rejected(reason: &'static str) -> Self {
        Self {
            state: "controller_rejected",
            reason,
        }
    }
    fn deferred(reason: &'static str) -> Self {
        Self {
            state: "controller_deferred",
            reason,
        }
    }
}

impl Consumer {
    /// Open an existing private owner journal. The allowlist is supplied from
    /// trusted configuration, never copied from the request's actor field.
    pub async fn open(path: impl AsRef<Path>, bridge: Bridge, admins: Vec<String>) -> Result<Self> {
        if admins.is_empty() || admins.iter().any(|actor| actor.trim().is_empty()) {
            return Err(Error::Invalid);
        }
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(path)
                    .foreign_keys(true)
                    .busy_timeout(Duration::from_secs(5)),
            )
            .await?;
        // Refuse a wrong database rather than creating an unauthenticated queue.
        sqlx::query("SELECT request_key,actor,command,note,state,created_at,updated_at FROM admin_requests LIMIT 0").execute(&pool).await?;
        sqlx::query("SELECT request_key,actor,event,at FROM admin_audit LIMIT 0")
            .execute(&pool)
            .await?;
        Ok(Self {
            pool,
            bridge,
            admins: admins.into_iter().collect(),
            lower_quality_shadow: false,
            lower_quality_minimum: None,
        })
    }

    /// Explicit shadow composition only. This permits a durable guard proposal;
    /// the host must still use a shadow worker that never dispatches effects.
    /// The current configured floor is required; None disables shadow approvals.
    pub fn with_lower_quality_shadow(mut self, minimum_resolution: Option<u32>) -> Self {
        self.lower_quality_shadow = true;
        self.lower_quality_minimum = minimum_resolution.filter(|minimum| *minimum > 0);
        self
    }

    pub async fn tick(&self, now: u64) -> Result<usize> {
        // Claims are never reclaimed. Expiration exposes an interrupted action
        // for review; the original claimant cannot overwrite that verdict.
        if let Some(expired) = now.checked_sub(300) {
            let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
            sqlx::query("INSERT INTO admin_audit(request_key,actor,event,at) SELECT request_key,actor,'controller_uncertain: Interrupted action; inspect saved state before submitting another request.',? FROM admin_requests WHERE state='controller_claimed' AND updated_at<=?")
                .bind(integer(now)?).bind(integer(expired)?).execute(&mut *tx).await?;
            sqlx::query("UPDATE admin_requests SET state='controller_uncertain',updated_at=? WHERE state='controller_claimed' AND updated_at<=?")
                .bind(integer(now)?).bind(integer(expired)?).execute(&mut *tx).await?;
            tx.commit().await?;
        }
        let mut consumed = 0;
        for _ in 0..32 {
            let Some(request) = self.claim(now).await? else {
                break;
            };
            let outcome = if !self.admins.contains(&request.actor) {
                Outcome::rejected(
                    "The requesting account is no longer an authorized administrator.",
                )
            } else if request.note.trim().is_empty()
                || request.note.len() > 1000
                || request.key.is_empty()
                || request.key.len() > 128
            {
                Outcome::rejected("The saved request is invalid.")
            } else {
                match serde_json::from_str::<Command>(&request.command) {
                    Ok(command) => match self.apply(&request, command, now).await {
                        Ok(outcome) => outcome,
                        Err(_) => Outcome {
                            state: "controller_uncertain",
                            reason: "The controller could not confirm the result. Inspect the saved journals before submitting another request.",
                        },
                    },
                    Err(_) => Outcome::rejected("The saved command is unsupported or malformed."),
                }
            };
            self.finish(&request, outcome, now).await?;
            consumed += 1;
        }
        Ok(consumed)
    }

    async fn claim(&self, now: u64) -> Result<Option<Request>> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query("UPDATE admin_requests SET state='controller_claimed',updated_at=? WHERE request_key=(SELECT request_key FROM admin_requests WHERE state='pending_controller' AND created_at<=? ORDER BY created_at,request_key LIMIT 1) AND state='pending_controller' RETURNING request_key,actor,command,note")
            .bind(integer(now)?).bind(integer(now)?).fetch_optional(&mut *tx).await?;
        let Some(row) = row else { return Ok(None) };
        let request = Request {
            key: row.get("request_key"),
            actor: row.get("actor"),
            command: row.get("command"),
            note: row.get("note"),
        };
        sqlx::query("INSERT INTO admin_audit(request_key,actor,event,at) VALUES (?,?,'controller_claimed',?)")
            .bind(&request.key).bind(&request.actor).bind(integer(now)?).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(Some(request))
    }

    async fn finish(&self, request: &Request, outcome: Outcome, now: u64) -> Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let changed = sqlx::query("UPDATE admin_requests SET state=?,updated_at=? WHERE request_key=? AND state='controller_claimed'")
            .bind(outcome.state).bind(integer(now)?).bind(&request.key).execute(&mut *tx).await?.rows_affected();
        if changed == 1 {
            sqlx::query("INSERT INTO admin_audit(request_key,actor,event,at) VALUES (?,?,?,?)")
                .bind(&request.key)
                .bind(&request.actor)
                .bind(format!("{}: {}", outcome.state, outcome.reason))
                .bind(integer(now)?)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn apply(&self, request: &Request, command: Command, now: u64) -> Result<Outcome> {
        match command {
            Command::Approve { report, decided_at } => {
                let Some(diagnosis) = self.diagnosis(report, decided_at).await? else {
                    return Ok(Outcome::rejected(
                        "The replacement diagnosis changed or a repair is already attached.",
                    ));
                };
                let progress = self.bridge.consume(&diagnosis, now).await?;
                Ok(if progress.job_id.is_some() {
                    Outcome::applied(
                        "The current evidence, access, identity and budget passed validation; a repair is attached.",
                    )
                } else {
                    Outcome::deferred(
                        "Current evidence, library access, exact file identity, supported constraints or repair budget did not pass validation. Multipart and multiversion files require review.",
                    )
                })
            }
            Command::Deny { report, decided_at } => {
                if self.diagnosis(report, decided_at).await?.is_none() {
                    return Ok(Outcome::rejected(
                        "The replacement diagnosis changed or a repair is already attached.",
                    ));
                }
                self.stop(request, report, "deny", now).await
            }
            Command::Close { report } => self.stop(request, report, "close", now).await,
            Command::Message { report, template } => {
                let Some(route) = self.bridge.route_for_report(report, now).await? else {
                    return Ok(Outcome::deferred(
                        "No currently authorized, authenticated delivery route is saved for this report.",
                    ));
                };
                self.bridge
                    .queue_lifecycle(
                        report,
                        None,
                        DeliveryPurpose::TemplateMessage,
                        &request.key,
                        route,
                        template.text().into(),
                        now,
                    )
                    .await?;
                Ok(Outcome::applied(
                    "The fixed template was queued durably. This does not confirm delivery.",
                ))
            }
            Command::Pin { id } => {
                let admin = AdminAction {
                    actor: request.actor.clone(),
                    role: PolicyRole::Admin,
                    note: request.note.clone(),
                };
                match self.bridge.engine.pin_exclusion(id, &admin, now).await {
                    Ok(_) => Ok(Outcome::applied(
                        "The active exclusion is pinned; its original evidence is preserved.",
                    )),
                    Err(
                        pitcrew_store::StoreError::Missing
                        | pitcrew_store::StoreError::InvalidInput,
                    ) => Ok(Outcome::rejected(
                        "The exclusion is missing, expired, released or future-dated.",
                    )),
                    Err(error) => Err(error.into()),
                }
            }
            Command::LowerQuality { job, revision } => {
                let lease = match self
                    .bridge
                    .engine
                    .acquire_lease(job, "admin-lower-quality", now, 30)
                    .await
                {
                    Ok(lease) => lease,
                    Err(pitcrew_store::StoreError::LeaseBusy) => {
                        return Ok(Outcome::deferred(
                            "The job is busy; submit a new request after it is refreshed.",
                        ));
                    }
                    Err(error) => return Err(error.into()),
                };
                let result = self
                    .approve_lower_quality(request, revision, &lease, now)
                    .await;
                self.bridge.engine.release_lease(&lease, now).await?;
                result
            }
        }
    }

    async fn approve_lower_quality(
        &self,
        request: &Request,
        revision: u64,
        lease: &pitcrew_store::Lease,
        now: u64,
    ) -> Result<Outcome> {
        use pitcrew_domain::{Event, State};
        let stored = self.bridge.engine.load_job(lease.job).await?;
        if stored.revision != revision || stored.job.state() != State::AwaitingQualityApproval {
            return Ok(Outcome::rejected(
                "The job changed or is no longer awaiting a lower-quality decision.",
            ));
        }
        if !self.lower_quality_shadow {
            return Ok(Outcome::deferred(
                "Live lower-quality replacement is refused: no certified upgrade-guard transport is configured.",
            ));
        }
        let Some(minimum) = self.lower_quality_minimum else {
            return Ok(Outcome::deferred(
                "Lower-quality shadow approval is disabled. Configure a positive resolution floor and enable release searches with the report runtime.",
            ));
        };
        let Some((approval, access)) = self
            .bridge
            .lower_quality_approval(&stored, minimum, now)
            .await?
        else {
            return Ok(Outcome::deferred(
                "Current bounded release evidence, broken bytes, viewer access, file identity, exclusions or budget did not pass validation.",
            ));
        };
        let applied = self
            .bridge
            .engine
            .apply_event(
                lease,
                revision,
                &format!("admin:lower-quality:{}", request.key),
                Event::ApproveLowerQuality { approval, access },
                now,
            )
            .await?;
        Ok(if applied.stored.job.state() == State::UpgradeGuardIntent {
            Outcome::applied(
                "The bounded lower-quality replacement was approved in shadow. An upgrade-guard intent was recorded; no upstream action was sent.",
            )
        } else {
            Outcome::rejected("The job expired before the approval could be applied.")
        })
    }

    async fn diagnosis(&self, report: i64, decided_at: u64) -> Result<Option<Diagnosis>> {
        let attached: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM report_progress WHERE report_id=? AND job_id IS NOT NULL)",
        )
        .bind(report)
        .fetch_one(&self.bridge.pool)
        .await?;
        if attached {
            return Ok(None);
        }
        let row = sqlx::query("SELECT * FROM diagnoses WHERE report_id=? AND decided_at=?")
            .bind(report)
            .bind(integer(decided_at)?)
            .fetch_optional(&self.bridge.pool)
            .await?;
        Ok(row
            .and_then(|row| diagnosis_row(row).ok())
            .filter(|diagnosis| diagnosis.decision.action == RecommendedAction::Replace))
    }

    async fn stop(
        &self,
        request: &Request,
        report: i64,
        action: &str,
        now: u64,
    ) -> Result<Outcome> {
        let mut tx = self.bridge.pool.begin_with("BEGIN IMMEDIATE").await?;
        let eligible: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM web_intakes WHERE id=? AND json_valid(response) AND json_extract(response,'$.outcome')='report' AND NOT EXISTS(SELECT 1 FROM bridge_reports WHERE report_id=web_intakes.id))")
            .bind(report).fetch_one(&mut *tx).await?;
        if !eligible {
            return Ok(Outcome::rejected(
                "The report is missing or already has a frozen repair command. Closing a report cannot cancel a repair.",
            ));
        }
        sqlx::query("INSERT INTO bridge_report_controls(report_id,request_key,action,actor,note,at) VALUES (?,?,?,?,?,?) ON CONFLICT(report_id) DO NOTHING")
            .bind(report).bind(&request.key).bind(action).bind(&request.actor).bind(&request.note).bind(integer(now)?).execute(&mut *tx).await?;
        tx.commit().await?;
        self.bridge
            .save_progress(report, None, ProgressState::NeedsReview, now)
            .await?;
        Ok(Outcome::applied(
            "Automatic repair is stopped for this report. No file was changed and no ready message was sent.",
        ))
    }
}

impl Bridge {
    pub(crate) async fn report_stopped(&self, report: i64) -> Result<bool> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM bridge_report_controls WHERE report_id=?)",
        )
        .bind(report)
        .fetch_one(&self.pool)
        .await?)
    }
}
