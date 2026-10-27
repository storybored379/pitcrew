//! Pure fix lifecycle. Persist a transition and its intents atomically before dispatch.
//!
//! This crate performs no I/O. See ADR 0002 for the store/worker commit protocol.
pub mod channels;
pub mod delivery;
pub mod media;

use media::ReleaseIdentity;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Identity of exact content, not a release group or a reusable path.
/// Adapters must change `revision` whenever the underlying bytes change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileIdentity {
    pub id: u64,
    pub revision: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Constraints {
    pub language: Option<String>,
    pub require_sdr: bool,
    /// HDR is allowed, but every replacement video stream must prove it is not DV profile 5.
    /// Omitting false preserves the serialization of existing durable commands.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub forbid_dolby_vision_profile5: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// A budget envelope already allocated to this job by the store.
    pub max_grabs: u32,
    pub stage_timeout: u64,
    pub total_timeout: u64,
}

/// Per-download-client durations in logical seconds. Configure cached and uncached
/// clients separately; the initial selection deadline still uses `stage_timeout`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageTimeouts {
    pub download: u64,
    pub import: u64,
    pub decode: u64,
    pub verify_retry: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum QualityRelation {
    SameOrBetter,
    Lower,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseCandidate {
    pub release: u64,
    #[serde(default)]
    pub identity: ReleaseIdentity,
    pub quality: QualityRelation,
}

/// A snapshot issued by the authenticated admin boundary, never accepted directly
/// from a viewer message. Approval applies only to these bytes and this release.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminApproval {
    pub job: u64,
    pub release: u64,
    pub broken_file: FileIdentity,
    pub expires_at: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpgradeGuardResolution {
    Installed,
    Absent,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Access {
    Unknown,
    Denied,
    Granted { library: u64, expires_at: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    Picked,
    AwaitingQualityApproval,
    UpgradeGuardIntent,
    UpgradeGuardReconciling,
    QualityApproved,
    GrabIntent,
    GrabReconciling,
    Downloading,
    Downloaded,
    DownloadRejected,
    DownloadVerified,
    RejectIntent,
    RejectReconciling,
    RejectRetryable,
    Rejected,
    ImportIntent,
    ImportReconciling,
    Importing,
    Imported,
    VerifyRetry,
    Verified,
    Done,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Failure {
    Deadline,
    BudgetExhausted,
    Upstream,
    ImportedDecodeFailed,
    ConstraintsUnsatisfied,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GrabResolution {
    Accepted {
        download: u64,
    },
    /// Authoritative queue AND history evidence, including blocked downloads.
    Absent,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeleteResolution {
    Absent,
    Present,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImportResolution {
    Imported {
        download: u64,
        file: FileIdentity,
    },
    /// Command definitely did not run; safe to request it again.
    Absent,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    SelectCandidate {
        candidate: ReleaseCandidate,
        access: Access,
    },
    /// Evidence from a decode probe of the exact currently installed copy.
    BrokenFileConfirmed {
        file: FileIdentity,
    },
    RequestLowerQuality {
        candidate: ReleaseCandidate,
        access: Access,
    },
    ApproveLowerQuality {
        approval: AdminApproval,
        access: Access,
    },
    GrabApproved {
        access: Access,
    },
    /// Authoritative client/history failure, correlated to the original grab.
    DownloadFailed {
        grab: IntentKey,
        download: u64,
    },
    /// Arr may replace the library file before our download decode finishes.
    /// This is an upstream observation, never permission to delete a library file.
    AutoImported {
        grab: IntentKey,
        download: u64,
        file: FileIdentity,
    },
    SelectRelease {
        release: u64,
        access: Access,
    },
    RejectDownload {
        access: Access,
    },
    RequestImport {
        access: Access,
    },
    QueueReady,
    Fail(Failure),
    Tick,
    /// Preserve the original committed intent sequence on async callbacks.
    /// Never relabel an old observation with the current job sequence.
    Observed {
        intent: IntentKey,
        observation: Observation,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Observation {
    UpgradeGuardInstalled,
    UpgradeGuardAmbiguous,
    UpgradeGuardReconciled(UpgradeGuardResolution),
    GrabAccepted {
        download: u64,
    },
    GrabAmbiguous,
    GrabReconciled(GrabResolution),
    DownloadCompleted {
        download: u64,
        file: FileIdentity,
    },
    DownloadDecoded {
        file: FileIdentity,
        good: bool,
    },
    DeleteAcknowledged,
    DeleteAmbiguous,
    DeleteReconciled(DeleteResolution),
    ImportAccepted,
    ImportAmbiguous,
    ImportReconciled(ImportResolution),
    ImportCompleted {
        download: u64,
        file: FileIdentity,
    },
    ImportedDecoded {
        file: FileIdentity,
        good: bool,
    },
    TargetChecked {
        file: FileIdentity,
        plex_file: FileIdentity,
        constraints_satisfied: bool,
    },
}

/// Effects are persisted as outbox intents; these are not permission to execute
/// before commit. Notification keys and operation keys are stable across retries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Effect {
    /// Fence unattended arr upgrades for this media item, retaining every known
    /// dead identifier. If the adapter cannot guarantee this, fail closed.
    InstallUpgradeGuard {
        excluded: Vec<ReleaseCandidate>,
    },
    ReconcileUpgradeGuard {
        excluded: Vec<ReleaseCandidate>,
    },
    Grab {
        release: u64,
        reservation: u32,
    },
    ReconcileGrab {
        release: u64,
    },
    DecodeDownload {
        download: u64,
        file: FileIdentity,
    },
    RemoveRejectedDownload {
        download: u64,
        blocklist: bool,
        skip_redownload: bool,
    },
    ReconcileDelete {
        download: u64,
    },
    /// May replace the old file only with these already-decoded exact bytes.
    ImportVerifiedDownload {
        download: u64,
        file: FileIdentity,
        original: FileIdentity,
    },
    ReconcileImport {
        download: u64,
    },
    DecodeImported {
        file: FileIdentity,
    },
    CheckTarget {
        file: FileIdentity,
        constraints: Constraints,
    },
    NotifyAdmin {
        failure: Failure,
    },
    NotifyViewerFailure {
        failure: Failure,
    },
    NotifyReady {
        file: FileIdentity,
    },
}

/// Globally identifies one committed outbox row within an installation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentKey {
    pub job: u64,
    pub sequence: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Intent {
    pub job: u64,
    pub sequence: u64,
    pub effect: Effect,
}

/// A possibly executed mutation survives failure. Retain the media item's ownership
/// fence while this or an active download remains unresolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mutation {
    UpgradeGuard,
    Grab,
    Delete,
    Import,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct Attempt {
    candidate: ReleaseCandidate,
    grab_intent: IntentKey,
    verify_retried: bool,
    download: Option<u64>,
    download_file: Option<FileIdentity>,
    imported_file: Option<FileIdentity>,
    imported_decoded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct LowerQualityRequest {
    candidate: ReleaseCandidate,
    approved_until: Option<u64>,
}

/// Construct through `new` and replay committed events to restore. Snapshots are
/// intentionally not deserializable into unchecked authoritative state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FixJob {
    id: u64,
    library: u64,
    original: FileIdentity,
    constraints: Constraints,
    policy: Policy,
    timeouts: StageTimeouts,
    excluded_releases: Vec<ReleaseCandidate>,
    broken_current: bool,
    lower_quality: Option<LowerQualityRequest>,
    state: State,
    deadline: u64,
    total_deadline: u64,
    last_at: u64,
    reserved: u32,
    active: bool,
    attempt: Option<Attempt>,
    unresolved: Option<Mutation>,
    failure: Option<Failure>,
    sequence: u64,
    expected_observation: Option<IntentKey>,
    /// Positive access snapshot retained for delayed worker dispatch. Replaying
    /// events reconstructs it; no upstream credential is stored in the job.
    authorized_until: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transition {
    pub job: FixJob,
    pub intents: Vec<Intent>,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum TransitionError {
    #[error("timeouts must be positive and fit the logical clock")]
    InvalidPolicy,
    #[error("event time precedes the last committed event")]
    ClockWentBackwards,
    #[error("event is not legal in {0:?}")]
    InvalidTransition(State),
    #[error("a current positive access snapshot for the exact library is required")]
    Unauthorized,
    #[error("evidence does not identify this attempt's exact file or download")]
    IdentityMismatch,
    #[error("observation does not belong to the currently expected durable intent")]
    StaleObservation,
    #[error("the release is already known dead")]
    ExcludedRelease,
    #[error("lower quality requires exact broken-file evidence and current admin approval")]
    ApprovalRequired,
}

impl FixJob {
    pub fn new(
        id: u64,
        library: u64,
        original: FileIdentity,
        constraints: Constraints,
        policy: Policy,
        now: u64,
    ) -> Result<Self, TransitionError> {
        Self::new_with_timeouts(
            id,
            library,
            original,
            constraints,
            policy,
            StageTimeouts {
                download: policy.stage_timeout,
                import: policy.stage_timeout,
                decode: policy.stage_timeout,
                verify_retry: policy.stage_timeout,
            },
            now,
        )
    }

    pub fn new_with_timeouts(
        id: u64,
        library: u64,
        original: FileIdentity,
        constraints: Constraints,
        policy: Policy,
        timeouts: StageTimeouts,
        now: u64,
    ) -> Result<Self, TransitionError> {
        for timeout in [
            timeouts.download,
            timeouts.import,
            timeouts.decode,
            timeouts.verify_retry,
        ] {
            if timeout == 0 || now.checked_add(timeout).is_none() {
                return Err(TransitionError::InvalidPolicy);
            }
        }
        if policy.stage_timeout == 0 || policy.total_timeout == 0 {
            return Err(TransitionError::InvalidPolicy);
        }
        let total_deadline = now
            .checked_add(policy.total_timeout)
            .ok_or(TransitionError::InvalidPolicy)?;
        let deadline = now
            .checked_add(policy.stage_timeout)
            .ok_or(TransitionError::InvalidPolicy)?
            .min(total_deadline);
        Ok(Self {
            id,
            library,
            original,
            constraints,
            policy,
            timeouts,
            excluded_releases: Vec::new(),
            broken_current: false,
            lower_quality: None,
            state: State::Picked,
            deadline,
            total_deadline,
            last_at: now,
            reserved: 0,
            active: false,
            attempt: None,
            unresolved: None,
            failure: None,
            sequence: 0,
            expected_observation: None,
            authorized_until: None,
        })
    }

    /// Correlation used by adapters to track newly dispatched work. A callback
    /// must retain its original sequence, rather than reading this on arrival.
    pub fn expected_observation(&self) -> Option<IntentKey> {
        self.expected_observation
    }

    pub fn mutation_authorized(&self, now: u64) -> bool {
        self.authorized_until.is_some_and(|expiry| now < expiry)
    }

    pub fn excluded_releases(&self) -> &[ReleaseCandidate] {
        &self.excluded_releases
    }

    pub fn state(&self) -> State {
        self.state
    }
    pub fn deadline(&self) -> Option<u64> {
        if self.is_terminal() {
            None
        } else {
            Some(self.deadline)
        }
    }
    pub fn reserved_budget(&self) -> u32 {
        self.reserved
    }
    pub fn active_grabs(&self) -> u8 {
        u8::from(self.active)
    }
    pub fn unresolved_mutation(&self) -> Option<Mutation> {
        self.unresolved
    }
    pub fn failure(&self) -> Option<Failure> {
        self.failure
    }
    /// The current library copy, updated only by a correlated import observation.
    pub fn current_original(&self) -> FileIdentity {
        self.original
    }

    pub fn constraints(&self) -> &Constraints {
        &self.constraints
    }
    pub fn is_terminal(&self) -> bool {
        matches!(self.state, State::Done | State::Failed)
    }

    /// Compute only. The caller must compare-and-swap the prior job revision and
    /// atomically commit the next job, accepted event and every intent.
    pub fn apply(&self, event: Event, now: u64) -> Result<Transition, TransitionError> {
        if self.is_terminal() {
            return Err(TransitionError::InvalidTransition(self.state));
        }
        if now < self.last_at {
            return Err(TransitionError::ClockWentBackwards);
        }
        let mut next = self.clone();
        let mut effects = Vec::new();
        if self.state == State::VerifyRetry
            && now >= self.deadline
            && now < self.total_deadline
            && matches!(event, Event::Tick)
        {
            next.state = State::Imported;
            next.attempt_mut().verify_retried = true;
            effects.push(Effect::DecodeImported {
                file: self.imported_file(),
            });
        } else if now >= self.total_deadline
            || (now >= self.deadline && self.state != State::VerifyRetry)
        {
            next.fail(Failure::Deadline, &mut effects);
        } else {
            next.reduce(event, now, &mut effects)?;
        }
        if next.state != self.state {
            next.deadline = now
                .saturating_add(next.stage_timeout())
                .min(self.total_deadline);
        }
        next.last_at = now;
        let intents = effects
            .into_iter()
            .map(|effect| {
                next.sequence += 1;
                if !matches!(
                    effect,
                    Effect::NotifyAdmin { .. }
                        | Effect::NotifyViewerFailure { .. }
                        | Effect::NotifyReady { .. }
                ) {
                    next.expected_observation = Some(IntentKey {
                        job: self.id,
                        sequence: next.sequence,
                    });
                }
                Intent {
                    job: self.id,
                    sequence: next.sequence,
                    effect,
                }
            })
            .collect();
        Ok(Transition { job: next, intents })
    }

    fn stage_timeout(&self) -> u64 {
        match self.state {
            State::Downloading => self.timeouts.download,
            State::ImportIntent | State::ImportReconciling | State::Importing => {
                self.timeouts.import
            }
            State::Downloaded | State::Imported => self.timeouts.decode,
            State::VerifyRetry => self.timeouts.verify_retry,
            _ => self.policy.stage_timeout,
        }
    }

    fn authorize(&mut self, access: Access, now: u64) -> Result<(), TransitionError> {
        match access {
            Access::Granted {
                library,
                expires_at,
            } if library == self.library && now < expires_at => {
                self.authorized_until = Some(expires_at);
                Ok(())
            }
            _ => Err(TransitionError::Unauthorized),
        }
    }

    fn fail(&mut self, reason: Failure, effects: &mut Vec<Effect>) {
        self.state = State::Failed;
        self.failure = Some(reason);
        effects.extend([
            Effect::NotifyAdmin { failure: reason },
            Effect::NotifyViewerFailure { failure: reason },
        ]);
    }

    fn attempt(&self) -> &Attempt {
        self.attempt.as_ref().expect("state requires an attempt")
    }
    fn attempt_mut(&mut self) -> &mut Attempt {
        self.attempt.as_mut().expect("state requires an attempt")
    }
    fn download(&self) -> u64 {
        self.attempt().download.expect("state requires a download")
    }
    fn download_file(&self) -> FileIdentity {
        self.attempt()
            .download_file
            .expect("state requires downloaded bytes")
    }
    fn imported_file(&self) -> FileIdentity {
        self.attempt()
            .imported_file
            .expect("state requires imported bytes")
    }

    fn imported(
        &mut self,
        download: u64,
        file: FileIdentity,
        effects: &mut Vec<Effect>,
    ) -> Result<(), TransitionError> {
        if download != self.download() || file == self.original {
            return Err(TransitionError::IdentityMismatch);
        }
        self.attempt_mut().imported_file = Some(file);
        self.original = file;
        self.broken_current = false;
        self.unresolved = None;
        self.active = false;
        self.state = State::Imported;
        effects.push(Effect::DecodeImported { file });
        Ok(())
    }

    fn check_grab(&self, grab: IntentKey, download: u64) -> Result<(), TransitionError> {
        if grab != self.attempt().grab_intent {
            return Err(TransitionError::StaleObservation);
        }
        if self
            .attempt()
            .download
            .is_some_and(|expected| expected != download)
        {
            return Err(TransitionError::IdentityMismatch);
        }
        Ok(())
    }

    fn exclude_attempt(&mut self) {
        let candidate = self.attempt().candidate.clone();
        if !self.excluded_releases.contains(&candidate) {
            self.excluded_releases.push(candidate);
        }
    }

    fn check_candidate(&self, candidate: &ReleaseCandidate) -> Result<(), TransitionError> {
        if self.excluded_releases.iter().any(|dead| {
            dead.release == candidate.release || dead.identity.overlaps(&candidate.identity)
        }) {
            return Err(TransitionError::ExcludedRelease);
        }
        Ok(())
    }

    fn grab(&mut self, candidate: ReleaseCandidate, effects: &mut Vec<Effect>) {
        if self.reserved >= self.policy.max_grabs {
            self.fail(Failure::BudgetExhausted, effects);
        } else {
            self.reserved += 1;
            self.active = true;
            self.attempt = Some(Attempt {
                grab_intent: IntentKey {
                    job: self.id,
                    sequence: self.sequence + 1,
                },
                verify_retried: false,
                download: None,
                download_file: None,
                imported_file: None,
                imported_decoded: false,
                candidate: candidate.clone(),
            });
            self.state = State::GrabIntent;
            self.unresolved = Some(Mutation::Grab);
            effects.push(Effect::Grab {
                release: candidate.release,
                reservation: self.reserved,
            });
        }
    }

    fn reduce(
        &mut self,
        event: Event,
        now: u64,
        effects: &mut Vec<Effect>,
    ) -> Result<(), TransitionError> {
        match (self.state, event) {
            (_, Event::Tick) => {}
            (_, Event::Fail(reason)) => self.fail(reason, effects),
            (
                State::GrabIntent
                | State::GrabReconciling
                | State::Downloading
                | State::Downloaded
                | State::DownloadRejected
                | State::DownloadVerified,
                Event::DownloadFailed { grab, download },
            ) => {
                self.check_grab(grab, download)?;
                self.exclude_attempt();
                self.unresolved = None;
                self.active = false;
                self.state = State::Rejected;
                self.expected_observation = None;
            }
            (
                State::GrabIntent
                | State::GrabReconciling
                | State::Downloading
                | State::Downloaded
                | State::DownloadRejected
                | State::DownloadVerified,
                Event::AutoImported {
                    grab,
                    download,
                    file,
                },
            ) => {
                self.check_grab(grab, download)?;
                self.attempt_mut().download = Some(download);
                self.imported(download, file, effects)?;
            }
            (State::Picked | State::Rejected, Event::SelectRelease { release, access }) => {
                self.authorize(access, now)?;
                let candidate = ReleaseCandidate {
                    release,
                    identity: ReleaseIdentity::default(),
                    quality: QualityRelation::SameOrBetter,
                };
                self.check_candidate(&candidate)?;
                self.grab(candidate, effects);
            }
            (State::Picked | State::Rejected, Event::SelectCandidate { candidate, access }) => {
                self.authorize(access, now)?;
                self.check_candidate(&candidate)?;
                if candidate.quality == QualityRelation::Lower {
                    return Err(TransitionError::ApprovalRequired);
                }
                self.grab(candidate, effects);
            }
            (State::Picked | State::Rejected, Event::BrokenFileConfirmed { file }) => {
                if file != self.original {
                    return Err(TransitionError::IdentityMismatch);
                }
                self.broken_current = true;
            }
            (State::Picked | State::Rejected, Event::RequestLowerQuality { candidate, access }) => {
                self.authorize(access, now)?;
                self.check_candidate(&candidate)?;
                if !self.broken_current || candidate.quality != QualityRelation::Lower {
                    return Err(TransitionError::ApprovalRequired);
                }
                self.lower_quality = Some(LowerQualityRequest {
                    candidate,
                    approved_until: None,
                });
                self.state = State::AwaitingQualityApproval;
            }
            (State::AwaitingQualityApproval, Event::ApproveLowerQuality { approval, access }) => {
                self.authorize(access, now)?;
                let request = self
                    .lower_quality
                    .as_mut()
                    .expect("state requires approval request");
                if approval.job != self.id
                    || approval.release != request.candidate.release
                    || approval.broken_file != self.original
                    || now >= approval.expires_at
                {
                    return Err(TransitionError::ApprovalRequired);
                }
                request.approved_until = Some(approval.expires_at);
                self.state = State::UpgradeGuardIntent;
                self.unresolved = Some(Mutation::UpgradeGuard);
                effects.push(Effect::InstallUpgradeGuard {
                    excluded: self.excluded_releases.clone(),
                });
            }
            (State::QualityApproved, Event::GrabApproved { access }) => {
                self.authorize(access, now)?;
                let request = self
                    .lower_quality
                    .take()
                    .expect("state requires approval request");
                if now >= request.approved_until.expect("state requires approval") {
                    return Err(TransitionError::ApprovalRequired);
                }
                self.grab(request.candidate, effects);
            }
            (
                State::DownloadRejected | State::RejectRetryable,
                Event::RejectDownload { access },
            ) => {
                self.authorize(access, now)?;
                self.state = State::RejectIntent;
                self.unresolved = Some(Mutation::Delete);
                effects.push(Effect::RemoveRejectedDownload {
                    download: self.download(),
                    blocklist: true,
                    skip_redownload: true,
                });
            }
            (State::DownloadVerified, Event::RequestImport { access }) => {
                self.authorize(access, now)?;
                self.state = State::ImportIntent;
                self.unresolved = Some(Mutation::Import);
                effects.push(Effect::ImportVerifiedDownload {
                    download: self.download(),
                    file: self.download_file(),
                    original: self.original,
                });
            }
            (State::Verified, Event::QueueReady) => {
                self.state = State::Done;
                effects.push(Effect::NotifyReady {
                    file: self.imported_file(),
                });
            }
            (
                _,
                Event::Observed {
                    intent,
                    observation,
                },
            ) => {
                if self.expected_observation != Some(intent) {
                    return Err(TransitionError::StaleObservation);
                }
                self.observe(observation, effects)?;
            }
            _ => return Err(TransitionError::InvalidTransition(self.state)),
        }
        Ok(())
    }

    fn observe(
        &mut self,
        observation: Observation,
        effects: &mut Vec<Effect>,
    ) -> Result<(), TransitionError> {
        match (self.state, observation) {
            (State::UpgradeGuardIntent, Observation::UpgradeGuardInstalled)
            | (
                State::UpgradeGuardReconciling,
                Observation::UpgradeGuardReconciled(UpgradeGuardResolution::Installed),
            ) => {
                self.unresolved = None;
                self.state = State::QualityApproved;
            }
            (State::UpgradeGuardIntent, Observation::UpgradeGuardAmbiguous) => {
                self.state = State::UpgradeGuardReconciling;
                effects.push(Effect::ReconcileUpgradeGuard {
                    excluded: self.excluded_releases.clone(),
                });
            }
            (
                State::UpgradeGuardReconciling,
                Observation::UpgradeGuardReconciled(UpgradeGuardResolution::Absent),
            ) => {
                self.unresolved = None;
                self.state = State::AwaitingQualityApproval;
                self.lower_quality
                    .as_mut()
                    .expect("state requires approval request")
                    .approved_until = None;
            }
            (
                State::UpgradeGuardReconciling,
                Observation::UpgradeGuardReconciled(UpgradeGuardResolution::Unknown),
            ) => {}

            (State::GrabIntent, Observation::GrabAccepted { download })
            | (
                State::GrabReconciling,
                Observation::GrabReconciled(GrabResolution::Accepted { download }),
            ) => {
                self.attempt_mut().download = Some(download);
                self.unresolved = None;
                self.state = State::Downloading;
            }
            (State::GrabIntent, Observation::GrabAmbiguous) => {
                self.state = State::GrabReconciling;
                effects.push(Effect::ReconcileGrab {
                    release: self.attempt().candidate.release,
                });
            }
            (State::GrabReconciling, Observation::GrabReconciled(GrabResolution::Absent)) => {
                self.unresolved = None;
                self.active = false;
                self.state = State::Rejected;
            }
            (State::GrabReconciling, Observation::GrabReconciled(GrabResolution::Unknown))
            | (
                State::RejectReconciling,
                Observation::DeleteReconciled(DeleteResolution::Unknown),
            )
            | (
                State::ImportReconciling,
                Observation::ImportReconciled(ImportResolution::Unknown),
            ) => {}
            (State::Downloading, Observation::DownloadCompleted { download, file }) => {
                if download != self.download() {
                    return Err(TransitionError::IdentityMismatch);
                }
                self.attempt_mut().download_file = Some(file);
                self.state = State::Downloaded;
                effects.push(Effect::DecodeDownload { download, file });
            }
            (State::Downloaded, Observation::DownloadDecoded { file, good }) => {
                if file != self.download_file() {
                    return Err(TransitionError::IdentityMismatch);
                }
                if good {
                    self.state = State::DownloadVerified;
                } else {
                    self.exclude_attempt();
                    self.state = State::DownloadRejected;
                }
            }
            (State::RejectIntent, Observation::DeleteAcknowledged)
            | (State::RejectReconciling, Observation::DeleteReconciled(DeleteResolution::Absent)) =>
            {
                self.unresolved = None;
                self.active = false;
                self.state = State::Rejected;
            }
            (State::RejectIntent, Observation::DeleteAmbiguous) => {
                self.state = State::RejectReconciling;
                effects.push(Effect::ReconcileDelete {
                    download: self.download(),
                });
            }
            (
                State::RejectReconciling,
                Observation::DeleteReconciled(DeleteResolution::Present),
            ) => {
                self.unresolved = None;
                self.state = State::RejectRetryable;
            }
            (State::ImportIntent, Observation::ImportAccepted) => {
                self.unresolved = None;
                self.state = State::Importing;
            }
            (State::ImportIntent, Observation::ImportAmbiguous) => {
                self.state = State::ImportReconciling;
                effects.push(Effect::ReconcileImport {
                    download: self.download(),
                });
            }
            (State::ImportReconciling, Observation::ImportReconciled(ImportResolution::Absent)) => {
                self.unresolved = None;
                self.state = State::DownloadVerified;
            }
            (
                State::ImportReconciling,
                Observation::ImportReconciled(ImportResolution::Imported { download, file }),
            )
            | (State::Importing, Observation::ImportCompleted { download, file }) => {
                self.imported(download, file, effects)?
            }
            (State::Imported, Observation::ImportedDecoded { file, good }) => {
                if file != self.imported_file() {
                    return Err(TransitionError::IdentityMismatch);
                }
                self.attempt_mut().imported_decoded = good;
                if good {
                    effects.push(Effect::CheckTarget {
                        file,
                        constraints: self.constraints.clone(),
                    });
                } else if self.attempt().verify_retried {
                    self.exclude_attempt();
                    self.broken_current = true;
                    self.state = State::Rejected;
                    self.expected_observation = None;
                } else {
                    self.state = State::VerifyRetry;
                }
            }
            (
                State::Imported,
                Observation::TargetChecked {
                    file,
                    plex_file,
                    constraints_satisfied,
                },
            ) => {
                if !self.attempt().imported_decoded
                    || file != self.imported_file()
                    || plex_file != file
                {
                    return Err(TransitionError::IdentityMismatch);
                }
                if constraints_satisfied {
                    self.state = State::Verified;
                } else {
                    self.fail(Failure::ConstraintsUnsatisfied, effects);
                }
            }
            _ => return Err(TransitionError::InvalidTransition(self.state)),
        }
        Ok(())
    }
}

pub mod queue;
