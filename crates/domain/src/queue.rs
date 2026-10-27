//! Evidence-only detection of competing queue mutators. No queue snapshot alone
//! proves who removed a download, and a timeout never proves a local DELETE absent.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueueActor {
    NzbdavQueueRules,
    Cleanuparr,
    Decluttarr,
    DebridTool,
    UnknownExternal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LocalMutationAudit {
    /// Authoritative durable outbox audit, including claimed/uncertain effects.
    NoMatchingMutation,
    MatchingMutation,
    Incomplete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueFailureEvidence {
    pub download: u64,
    pub completed_at: u64,
    pub failed_at: u64,
    pub removed: bool,
    pub blocklisted: bool,
    /// Authenticated upstream history says "marked as failed". A queue miss is insufficient.
    pub marked_failed_history: bool,
    pub local_audit: LocalMutationAudit,
    /// Set only from authenticated actor/audit evidence; timing never identifies an actor.
    pub actor: Option<QueueActor>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueueImportRoute {
    #[default]
    HoldThenVerify,
    /// Preserve the old copy until verification, or observe independent arr auto-import.
    /// This is not permission for a destructive unverified ManualImport.
    ImportImmediatelyThenVerify,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct QueueInterference {
    pub route: QueueImportRoute,
    pub attribution: Option<QueueActor>,
}

impl QueueInterference {
    /// Persist the returned evidence/capability downgrade with the job/client policy.
    /// Do not automatically restore holding after one clean download.
    pub fn observe(self, owned_download: u64, evidence: QueueFailureEvidence) -> Self {
        if evidence.download != owned_download
            || evidence.failed_at < evidence.completed_at
            || !evidence.removed
            || !evidence.blocklisted
            || !evidence.marked_failed_history
            || evidence.local_audit != LocalMutationAudit::NoMatchingMutation
        {
            return self;
        }
        Self {
            route: QueueImportRoute::ImportImmediatelyThenVerify,
            attribution: Some(evidence.actor.unwrap_or(QueueActor::UnknownExternal)),
        }
    }
}
