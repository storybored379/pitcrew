//! Durable execution of the pure reducer. Shadow records all effects and calls none.
pub mod delivery;
pub mod live;
pub mod observer;
pub mod target;
pub mod webhook;

use pitcrew_domain::{Effect, Event, Intent, IntentKey, Observation, State};
use pitcrew_store::{Applied, Lease, OutboxStatus, Store, StoreError};
use serde::{Deserialize, Serialize};
use std::{future::Future, pin::Pin, time::Duration};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Shadow,
    Live,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub mode: Mode,
    /// Deliberate second switch, independent of selecting the mode.
    pub acknowledge_live_mutations: bool,
    pub call_timeout: Duration,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            mode: Mode::Shadow,
            acknowledge_live_mutations: false,
            call_timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Observed(Observation),
    Delivered,
    Uncertain,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallError {
    Timeout,
    ServerError,
    Unavailable,
}
pub type CallFuture<'a> = Pin<Box<dyn Future<Output = Result<Outcome, CallError>> + Send + 'a>>;
/// Adapters must retain exact intent/file identities. A successful HTTP status is
/// not sufficient evidence for `ImportedDecoded` or a notification receipt.
pub trait Upstream: Send + Sync {
    fn read<'a>(&'a self, intent: &'a Intent) -> CallFuture<'a>;
    fn write<'a>(&'a self, intent: &'a Intent) -> CallFuture<'a>;
}

#[derive(Debug, Error)]
pub enum EngineError {
    #[error(
        "engine requires a durable store, a positive call timeout, and explicit live acknowledgement"
    )]
    Configuration,
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("worker lease has insufficient time remaining for dispatch")]
    LeaseDeadline,
    #[error("the positive access snapshot expired before mutation dispatch")]
    AccessExpired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Boundary {
    LoadBefore,
    LoadAfter,
    TickBefore,
    TickAfter,
    OutboxBefore,
    OutboxAfter,
    ShadowBefore,
    ShadowAfter,
    StartBefore,
    StartAfter,
    CallBefore,
    CallAfter,
    ResultBefore,
    ResultAfter,
    FinishBefore,
    FinishAfter,
}

pub struct Worker<U> {
    store: Store,
    upstream: U,
    options: Options,
}
impl<U: Upstream> Worker<U> {
    pub fn new(store: Option<Store>, upstream: U, options: Options) -> Result<Self, EngineError> {
        if options.call_timeout.is_zero()
            || (options.mode == Mode::Live && !options.acknowledge_live_mutations)
        {
            return Err(EngineError::Configuration);
        }
        Ok(Self {
            store: store.ok_or(EngineError::Configuration)?,
            upstream,
            options,
        })
    }

    /// Accept an already authenticated, exactly correlated event. Webhook payloads
    /// are never passed to this method as access grants.
    pub async fn apply(
        &self,
        lease: &Lease,
        key: &str,
        event: Event,
        now: u64,
    ) -> Result<Applied, EngineError> {
        let current = self.store.load_job(lease.job).await?;
        Ok(self
            .store
            .apply_event(lease, current.revision, key, event, now)
            .await?)
    }

    /// Replay trusted historical observations using their original domain time
    /// while lease validity uses the current processing clock. Shadow only.
    pub async fn apply_recorded(
        &self,
        lease: &Lease,
        key: &str,
        event: Event,
        event_at: u64,
        now: u64,
    ) -> Result<Applied, EngineError> {
        if self.options.mode != Mode::Shadow {
            return Err(EngineError::Configuration);
        }
        let current = self.store.load_job(lease.job).await?;
        Ok(self
            .store
            .apply_recorded_event(lease, current.revision, key, event, event_at, now)
            .await?)
    }

    /// Record pending historical proposals without advancing their domain clock
    /// to wall time. This never dispatches even a read effect.
    pub async fn record_decisions(&self, lease: &Lease, now: u64) -> Result<usize, EngineError> {
        if self.options.mode != Mode::Shadow {
            return Err(EngineError::Configuration);
        }
        let started = tokio::time::Instant::now();
        let clock = || now.saturating_add(started.elapsed().as_secs());
        let mut count = 0;
        for row in self.store.outbox(lease.job).await? {
            if row.status != OutboxStatus::Pending {
                continue;
            }
            let key = IntentKey {
                job: row.intent.job,
                sequence: row.intent.sequence,
            };
            self.store
                .ingest_observation(
                    "engine_decision",
                    &format!("{}:{}", key.job, key.sequence),
                    &serde_json::json!({"mode":"shadow","intent":row.intent}),
                    clock(),
                )
                .await?;
            self.store
                .finish_intent(lease, key, OutboxStatus::Shadowed, clock())
                .await?;
            count += 1;
        }
        Ok(count)
    }

    pub async fn drive(&self, lease: &Lease, now: u64) -> Result<usize, EngineError> {
        self.drive_with(lease, now, |_| std::future::ready(()))
            .await
    }

    /// Deterministic crash harness: hooks surround every worker I/O await. Tests
    /// suspend or kill the process at each boundary and resume from the same DB.
    pub async fn drive_with<F, Fut>(
        &self,
        lease: &Lease,
        now: u64,
        mut hook: F,
    ) -> Result<usize, EngineError>
    where
        F: FnMut(Boundary) -> Fut,
        Fut: Future<Output = ()>,
    {
        let started = tokio::time::Instant::now();
        let clock = || now.saturating_add(started.elapsed().as_secs());
        hook(Boundary::LoadBefore).await;
        let current = self.store.load_job(lease.job).await?;
        hook(Boundary::LoadAfter).await;
        if let Some(deadline) = current.job.deadline().filter(|d| *d <= clock()) {
            hook(Boundary::TickBefore).await;
            self.store
                .apply_event(
                    lease,
                    current.revision,
                    &format!("deadline:{deadline}"),
                    Event::Tick,
                    clock(),
                )
                .await?;
            hook(Boundary::TickAfter).await;
        }
        hook(Boundary::OutboxBefore).await;
        let rows = self.store.outbox(lease.job).await?;
        hook(Boundary::OutboxAfter).await;
        let mut count = 0;
        for row in rows {
            if !matches!(
                row.status,
                OutboxStatus::Pending | OutboxStatus::Started | OutboxStatus::Uncertain
            ) {
                continue;
            }
            let intent = row.intent;
            let key = IntentKey {
                job: intent.job,
                sequence: intent.sequence,
            };
            if self.options.mode == Mode::Shadow {
                // A prior live dispatch retains its uncertainty when mode changes.
                if row.status != OutboxStatus::Pending {
                    continue;
                }
                hook(Boundary::ShadowBefore).await;
                self.store
                    .ingest_observation(
                        "engine_decision",
                        &format!("{}:{}", key.job, key.sequence),
                        &serde_json::json!({"mode":"shadow","intent":intent}),
                        clock(),
                    )
                    .await?;
                hook(Boundary::ShadowAfter).await;
                hook(Boundary::FinishBefore).await;
                self.store
                    .finish_intent(lease, key, OutboxStatus::Shadowed, clock())
                    .await?;
                hook(Boundary::FinishAfter).await;
                count += 1;
                continue;
            }
            // Never automatically resend an uncertain message, including admin
            // failure notifications: one durable logical delivery, manual review.
            if notification(&intent.effect) && row.status != OutboxStatus::Pending {
                if row.status == OutboxStatus::Started {
                    hook(Boundary::FinishBefore).await;
                    self.store
                        .finish_intent(lease, key, OutboxStatus::Uncertain, clock())
                        .await?;
                    hook(Boundary::FinishAfter).await;
                }
                continue;
            }
            hook(Boundary::LoadBefore).await;
            let current = self.store.load_job(lease.job).await?;
            hook(Boundary::LoadAfter).await;
            if !notification(&intent.effect) && current.job.is_terminal() {
                if row.status == OutboxStatus::Started {
                    hook(Boundary::FinishBefore).await;
                    self.store
                        .finish_intent(lease, key, OutboxStatus::Uncertain, clock())
                        .await?;
                    hook(Boundary::FinishAfter).await;
                }
                continue;
            }
            if !notification(&intent.effect)
                && (current.job.expected_observation() != Some(key)
                    || !dispatchable(&intent.effect, current.job.state()))
            {
                hook(Boundary::FinishBefore).await;
                self.store
                    .finish_intent(lease, key, OutboxStatus::Suppressed, clock())
                    .await?;
                hook(Boundary::FinishAfter).await;
                continue;
            }
            let recovered = row.status != OutboxStatus::Pending;
            let result = if recovered && ambiguous(&intent.effect).is_some() {
                Outcome::Observed(ambiguous(&intent.effect).expect("checked mutation"))
            } else {
                // Reserve enough lease time for the whole bounded external call.
                if clock().saturating_add(self.options.call_timeout.as_secs().saturating_add(1))
                    >= lease.expires_at
                {
                    return Err(EngineError::LeaseDeadline);
                }
                if !recovered {
                    hook(Boundary::StartBefore).await;
                    let claimed = self
                        .store
                        .start_intent(lease, key, current.revision, clock())
                        .await?;
                    hook(Boundary::StartAfter).await;
                    if !claimed {
                        continue;
                    }
                }
                hook(Boundary::CallBefore).await;
                if is_write(&intent.effect)
                    && !notification(&intent.effect)
                    && !current.job.mutation_authorized(clock())
                {
                    return Err(EngineError::AccessExpired);
                }
                // Claiming the row and scheduling the task may consume the
                // lease. No await separates this final check from dispatch.
                if clock().saturating_add(self.options.call_timeout.as_secs().saturating_add(1))
                    >= lease.expires_at
                {
                    return Err(EngineError::LeaseDeadline);
                }
                let call = if is_write(&intent.effect) {
                    self.upstream.write(&intent)
                } else {
                    self.upstream.read(&intent)
                };
                let result = tokio::time::timeout(self.options.call_timeout, call).await;
                hook(Boundary::CallAfter).await;
                match result {
                    Ok(Ok(outcome)) => outcome,
                    _ => ambiguous(&intent.effect).map_or(Outcome::Uncertain, Outcome::Observed),
                }
            };
            match result {
                Outcome::Observed(observation) if !notification(&intent.effect) => {
                    hook(Boundary::ResultBefore).await;
                    self.store
                        .apply_result(
                            lease,
                            current.revision,
                            &format!("effect:{}", key.sequence),
                            Event::Observed {
                                intent: key,
                                observation,
                            },
                            clock(),
                            key,
                        )
                        .await?;
                    hook(Boundary::ResultAfter).await;
                }
                other => {
                    let status = if other == Outcome::Delivered && notification(&intent.effect) {
                        OutboxStatus::Completed
                    } else {
                        OutboxStatus::Uncertain
                    };
                    hook(Boundary::FinishBefore).await;
                    self.store
                        .finish_intent(lease, key, status, clock())
                        .await?;
                    hook(Boundary::FinishAfter).await;
                }
            }
            count += 1;
        }
        Ok(count)
    }
}

pub fn notification(effect: &Effect) -> bool {
    matches!(
        effect,
        Effect::NotifyAdmin { .. }
            | Effect::NotifyViewerFailure { .. }
            | Effect::NotifyReady { .. }
    )
}
pub fn is_write(effect: &Effect) -> bool {
    matches!(
        effect,
        Effect::InstallUpgradeGuard { .. }
            | Effect::Grab { .. }
            | Effect::RemoveRejectedDownload { .. }
            | Effect::ImportVerifiedDownload { .. }
    ) || notification(effect)
}
fn ambiguous(effect: &Effect) -> Option<Observation> {
    match effect {
        Effect::InstallUpgradeGuard { .. } => Some(Observation::UpgradeGuardAmbiguous),
        Effect::Grab { .. } => Some(Observation::GrabAmbiguous),
        Effect::RemoveRejectedDownload { .. } => Some(Observation::DeleteAmbiguous),
        Effect::ImportVerifiedDownload { .. } => Some(Observation::ImportAmbiguous),
        _ => None,
    }
}

fn dispatchable(effect: &Effect, state: State) -> bool {
    match effect {
        Effect::InstallUpgradeGuard { .. } => state == State::UpgradeGuardIntent,
        Effect::ReconcileUpgradeGuard { .. } => state == State::UpgradeGuardReconciling,
        Effect::Grab { .. } => state == State::GrabIntent,
        Effect::ReconcileGrab { .. } => state == State::GrabReconciling,
        Effect::DecodeDownload { .. } => state == State::Downloaded,
        Effect::RemoveRejectedDownload { .. } => state == State::RejectIntent,
        Effect::ReconcileDelete { .. } => state == State::RejectReconciling,
        Effect::ImportVerifiedDownload { .. } => state == State::ImportIntent,
        Effect::ReconcileImport { .. } => state == State::ImportReconciling,
        Effect::DecodeImported { .. } => state == State::Imported,
        Effect::CheckTarget { .. } => state == State::Imported,
        _ => true,
    }
}
