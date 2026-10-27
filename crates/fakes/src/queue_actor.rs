//! A deterministic independent queue cleaner; never used by production crates.
use crate::holding::HoldingArr;
use pitcrew_domain::queue::{LocalMutationAudit, QueueActor, QueueFailureEvidence};

pub struct ExternalQueueActor {
    pub actor: QueueActor,
    pub completed_at: u64,
    pub delay: u64,
    pub blocklisted: Vec<u64>,
}

impl ExternalQueueActor {
    pub fn tick(&mut self, arr: &mut HoldingArr, now: u64) -> Option<QueueFailureEvidence> {
        if now < self.completed_at.saturating_add(self.delay) {
            return None;
        }
        let download = arr.download.as_ref()?;
        if !download.complete {
            return None;
        }
        let download = download.id;
        arr.download = None;
        self.blocklisted.push(download);
        Some(QueueFailureEvidence {
            download,
            completed_at: self.completed_at,
            failed_at: now,
            removed: true,
            blocklisted: true,
            marked_failed_history: true,
            local_audit: LocalMutationAudit::NoMatchingMutation,
            actor: Some(self.actor),
        })
    }
}
