//! Conservative adapter from actual queue/candidate DTOs into arbiter facts.
//! Caller enriches access, ownership, exact-file evidence and authenticated
//! external-actor history. Queue data alone cannot establish those facts.
use crate::{ImportCandidate, Kind, QueueRecord};
use pitcrew_arbiter::shadow::{ImportFacts, ImportObservation, LegacyAction, MediaKind};
use pitcrew_domain::Access;

pub struct Correlation {
    pub job: u64,
    pub library: u64,
    pub download: u64,
    pub observed_at: u64,
}

pub fn observe_queue(
    kind: Kind,
    correlation: Correlation,
    queue: &QueueRecord,
    candidates: Option<&[ImportCandidate]>,
) -> ImportObservation {
    let no_rejections = candidates.and_then(|files| {
        if files.is_empty() {
            return None;
        }
        let rejections: Option<Vec<_>> =
            files.iter().map(|file| file.rejections.as_ref()).collect();
        rejections.map(|values| values.iter().all(|value| value.is_empty()))
    });
    ImportObservation {
        job: correlation.job,
        library: correlation.library,
        download: correlation.download,
        owned_download: None,
        observed_at: correlation.observed_at,
        kind: match kind {
            Kind::Sonarr => MediaKind::Episode,
            Kind::Radarr => MediaKind::Movie,
        },
        import_blocked: queue
            .tracked_download_state
            .as_ref()
            .map(|state| state == "importBlocked"),
        access: Access::Unknown,
        source: None,
        decoded_source: None,
        facts: ImportFacts {
            no_rejections,
            ..ImportFacts::default()
        },
        external_failure: None,
        legacy_action: LegacyAction::Unknown,
    }
}
