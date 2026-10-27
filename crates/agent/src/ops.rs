//! Disabled-by-default operational planners. Persist each returned job and intent
//! atomically before any dispatcher acts. There is deliberately no privileged
//! remount/restart executor: production dispatch requires the durable worker.
use pitcrew_arbiter::{
    ArbiterReceipt, ImportArbiter, ImportDenial, MatchedIdRequest, VerifiedImportProposal,
};
use pitcrew_domain::IntentKey;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpsKind {
    MountHeal,
    PublishWatchdog,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpsPolicy {
    pub enabled: bool,
    pub dry_run: bool,
    pub cooldown_seconds: u64,
    /// Calendar days are UTC; unknown/rolled-back clocks never reopen a budget.
    pub daily_limit: u32,
}

impl OpsPolicy {
    pub fn for_kind(kind: OpsKind) -> Self {
        Self {
            enabled: false,
            dry_run: true,
            cooldown_seconds: match kind {
                OpsKind::MountHeal => 900,
                OpsKind::PublishWatchdog => 1800,
            },
            daily_limit: 4,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpsHealth {
    Healthy,
    Unhealthy,
    Unknown,
}

/// An authenticated exact-server resource observation; unreachable/malformed
/// resources are Unknown, never evidence that restarting Plex would help.
pub fn publish_health(
    server_present: Option<bool>,
    provides_server: bool,
    remote_connection: bool,
) -> OpsHealth {
    match server_present {
        None => OpsHealth::Unknown,
        Some(true) if provides_server && remote_connection => OpsHealth::Healthy,
        Some(_) => OpsHealth::Unhealthy,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpsCheck {
    pub now: u64,
    pub health: OpsHealth,
    /// None means unreadable/unknown pause status, and fails closed.
    pub paused: Option<bool>,
    /// None means unknown session state; only positively idle can act.
    pub active_sessions: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct OpsIntent {
    pub key: IntentKey,
    pub kind: OpsKind,
    pub created_at: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum OpsState {
    Watching,
    Intent(OpsIntent),
    Reconciling(OpsIntent),
    NeedsAdmin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpsHold {
    Disabled,
    Paused,
    PauseUnknown,
    Healthy,
    HealthUnknown,
    FirstFailure,
    SessionsActive,
    SessionsUnknown,
    Cooldown,
    DailyCap,
    Pending,
    NeedsAdmin,
    DuplicateSample,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpsDecision {
    Hold(OpsHold),
    DryRun(OpsKind),
    PersistIntent(OpsIntent),
    Reconcile(OpsIntent),
    Recovered,
    NotifyAdmin(OpsIntent),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpsError {
    ClockRegression,
    InvalidPolicy,
    WrongIntent,
    SequenceExhausted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OpsJob {
    id: u64,
    kind: OpsKind,
    state: OpsState,
    last_now: u64,
    last_sample: Option<u64>,
    failures: u8,
    day: u64,
    reserved_today: u32,
    last_started: Option<u64>,
    sequence: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpsTransition {
    pub job: OpsJob,
    pub decision: OpsDecision,
}

impl OpsJob {
    pub fn new(id: u64, kind: OpsKind, now: u64) -> Self {
        Self {
            id,
            kind,
            state: OpsState::Watching,
            last_now: now,
            last_sample: None,
            failures: 0,
            day: now / 86400,
            reserved_today: 0,
            last_started: None,
            sequence: 0,
        }
    }

    pub fn state(&self) -> OpsState {
        self.state
    }
    pub fn reserved_today(&self) -> u32 {
        self.reserved_today
    }

    pub fn check(&self, policy: OpsPolicy, check: OpsCheck) -> Result<OpsTransition, OpsError> {
        if check.now < self.last_now {
            return Err(OpsError::ClockRegression);
        }
        if policy.cooldown_seconds == 0 || policy.daily_limit == 0 {
            return Err(OpsError::InvalidPolicy);
        }
        let mut next = self.clone();
        next.last_now = check.now;
        let hold = |job, reason| {
            Ok(OpsTransition {
                job,
                decision: OpsDecision::Hold(reason),
            })
        };
        match next.state {
            OpsState::NeedsAdmin => return hold(next, OpsHold::NeedsAdmin),
            OpsState::Intent(_) | OpsState::Reconciling(_) => return hold(next, OpsHold::Pending),
            OpsState::Watching => {}
        }
        if !policy.enabled {
            next.failures = 0;
            return hold(next, OpsHold::Disabled);
        }
        match check.paused {
            Some(false) => {}
            status => {
                next.failures = 0;
                return hold(
                    next,
                    if status.is_some() {
                        OpsHold::Paused
                    } else {
                        OpsHold::PauseUnknown
                    },
                );
            }
        }
        if next.last_sample == Some(check.now) {
            return hold(next, OpsHold::DuplicateSample);
        }
        next.last_sample = Some(check.now);
        match check.health {
            OpsHealth::Healthy => {
                next.failures = 0;
                return hold(next, OpsHold::Healthy);
            }
            OpsHealth::Unknown => {
                next.failures = 0;
                return hold(next, OpsHold::HealthUnknown);
            }
            OpsHealth::Unhealthy => {}
        }
        next.failures = next.failures.saturating_add(1).min(2);
        if next.failures < 2 {
            return hold(next, OpsHold::FirstFailure);
        }
        match check.active_sessions {
            Some(0) => {}
            Some(_) => return hold(next, OpsHold::SessionsActive),
            None => return hold(next, OpsHold::SessionsUnknown),
        }
        if let Some(last) = next.last_started
            && check.now - last < policy.cooldown_seconds
        {
            return hold(next, OpsHold::Cooldown);
        }
        let day = check.now / 86400;
        if day > next.day {
            next.day = day;
            next.reserved_today = 0;
        }
        if next.reserved_today >= policy.daily_limit {
            return hold(next, OpsHold::DailyCap);
        }
        if policy.dry_run {
            return Ok(OpsTransition {
                job: next,
                decision: OpsDecision::DryRun(self.kind),
            });
        }
        next.sequence = next
            .sequence
            .checked_add(1)
            .ok_or(OpsError::SequenceExhausted)?;
        let intent = OpsIntent {
            key: IntentKey {
                job: next.id,
                sequence: next.sequence,
            },
            kind: next.kind,
            created_at: check.now,
        };
        next.reserved_today += 1;
        next.last_started = Some(check.now);
        next.state = OpsState::Intent(intent);
        Ok(OpsTransition {
            job: next,
            decision: OpsDecision::PersistIntent(intent),
        })
    }

    /// Unknown includes a lost response or recovery after claiming an intent.
    /// Never replay a restart/remount blindly; poll fresh health and reconcile.
    pub fn resolve(&self, key: IntentKey, health: OpsHealth) -> Result<OpsTransition, OpsError> {
        let intent = match self.state {
            OpsState::Intent(intent) | OpsState::Reconciling(intent) if intent.key == key => intent,
            _ => return Err(OpsError::WrongIntent),
        };
        let mut next = self.clone();
        let decision = match health {
            OpsHealth::Unknown => {
                next.state = OpsState::Reconciling(intent);
                OpsDecision::Reconcile(intent)
            }
            OpsHealth::Healthy => {
                next.state = OpsState::Watching;
                next.failures = 0;
                OpsDecision::Recovered
            }
            OpsHealth::Unhealthy => {
                next.state = OpsState::NeedsAdmin;
                OpsDecision::NotifyAdmin(intent)
            }
        };
        Ok(OpsTransition {
            job: next,
            decision,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfirmerPolicy {
    pub enabled: bool,
    pub dry_run: bool,
}

impl Default for ConfirmerPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            dry_run: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfirmDecision {
    Disabled,
    Denied(ImportDenial),
    DryRun(VerifiedImportProposal),
    Arbiter(ArbiterReceipt),
}

pub fn confirm_matched_by_id(
    policy: ConfirmerPolicy,
    request: &MatchedIdRequest,
    arbiter: &mut dyn ImportArbiter,
) -> ConfirmDecision {
    if !policy.enabled {
        return ConfirmDecision::Disabled;
    }
    match pitcrew_arbiter::matched_by_id(request) {
        Err(reason) => ConfirmDecision::Denied(reason),
        Ok(proposal) if policy.dry_run => ConfirmDecision::DryRun(proposal),
        Ok(proposal) => ConfirmDecision::Arbiter(arbiter.propose(proposal)),
    }
}
