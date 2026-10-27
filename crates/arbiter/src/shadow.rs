//! Evidence-only import shadow. Every output is a proposal for a durable FixJob,
//! never an import authorization. No network or side-effect interface exists here.
use pitcrew_domain::{
    Access, FileIdentity,
    queue::{QueueFailureEvidence, QueueInterference},
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaKind {
    Episode,
    Movie,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TitleMatch {
    Exact,
    Prefix,
    Conflict,
    #[default]
    Unknown,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ImportFacts {
    pub title: TitleMatch,
    pub year_matches: Option<bool>,
    pub language_matches: Option<bool>,
    pub episodes_match: Option<bool>,
    pub no_rejections: Option<bool>,
    pub no_samples: Option<bool>,
    pub existing_file: Option<bool>,
    pub video_files: Option<u32>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LegacyAction {
    Import,
    BlocklistAndSearch,
    Remove,
    Hold,
    #[default]
    Unknown,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImportObservation {
    /// Local opaque correlation IDs, not upstream usernames, paths or titles.
    pub job: u64,
    pub library: u64,
    pub download: u64,
    pub owned_download: Option<u64>,
    pub observed_at: u64,
    pub kind: MediaKind,
    pub import_blocked: Option<bool>,
    pub access: Access,
    pub source: Option<FileIdentity>,
    pub decoded_source: Option<FileIdentity>,
    pub facts: ImportFacts,
    pub external_failure: Option<QueueFailureEvidence>,
    /// Only populated from authenticated legacy activity, never timing inference.
    pub legacy_action: LegacyAction,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    Allow,
    Deny,
    Hold,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reason {
    AccessUnknown,
    AccessDenied,
    OwnershipUnknown,
    OwnershipMismatch,
    QueueNotBlocked,
    MissingFacts,
    WrongTitle,
    WrongYear,
    WrongLanguage,
    WrongEpisodes,
    ArrRejections,
    Sample,
    ExistingFile,
    MultipleMovies,
    UnverifiedSource,
    SourceChanged,
    VerifiedProposal,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub action: Action,
    pub reason: Reason,
}
fn decision(action: Action, reason: Reason) -> Decision {
    Decision { action, reason }
}

/// Mirrors only the clear-cut legacy rule path, adding current access, ownership
/// and exact-source verification. Ambiguous/model-only cases remain held.
pub fn evaluate(o: &ImportObservation) -> Decision {
    match o.access {
        Access::Unknown => return decision(Action::Hold, Reason::AccessUnknown),
        Access::Granted {
            library,
            expires_at,
        } if library == o.library && expires_at > o.observed_at => {}
        _ => return decision(Action::Deny, Reason::AccessDenied),
    }
    match o.owned_download {
        None => return decision(Action::Hold, Reason::OwnershipUnknown),
        Some(download) if download == o.download => {}
        _ => return decision(Action::Deny, Reason::OwnershipMismatch),
    }
    if o.import_blocked != Some(true) {
        return decision(Action::Hold, Reason::QueueNotBlocked);
    }
    match o.facts.title {
        TitleMatch::Exact | TitleMatch::Prefix => {}
        TitleMatch::Conflict => return decision(Action::Deny, Reason::WrongTitle),
        TitleMatch::Unknown => return decision(Action::Hold, Reason::MissingFacts),
    }
    for (fact, reason) in [
        (o.facts.year_matches, Reason::WrongYear),
        (o.facts.language_matches, Reason::WrongLanguage),
        (o.facts.no_rejections, Reason::ArrRejections),
        (o.facts.no_samples, Reason::Sample),
    ] {
        match fact {
            Some(true) => {}
            Some(false) => return decision(Action::Deny, reason),
            None => return decision(Action::Hold, Reason::MissingFacts),
        }
    }
    match o.facts.existing_file {
        Some(false) => {}
        Some(true) => return decision(Action::Hold, Reason::ExistingFile),
        None => return decision(Action::Hold, Reason::MissingFacts),
    }
    let Some(count) = o.facts.video_files.filter(|n| *n > 0) else {
        return decision(Action::Hold, Reason::MissingFacts);
    };
    match o.kind {
        MediaKind::Movie if count != 1 => return decision(Action::Hold, Reason::MultipleMovies),
        // One source identity cannot certify several files in a season pack.
        MediaKind::Episode if count != 1 => return decision(Action::Hold, Reason::MissingFacts),
        MediaKind::Episode => match o.facts.episodes_match {
            Some(true) => {}
            Some(false) => return decision(Action::Deny, Reason::WrongEpisodes),
            None => return decision(Action::Hold, Reason::MissingFacts),
        },
        _ => {}
    }
    match (o.source, o.decoded_source) {
        (Some(source), Some(decoded)) if source == decoded => {
            decision(Action::Allow, Reason::VerifiedProposal)
        }
        (Some(_), Some(_)) => decision(Action::Deny, Reason::SourceChanged),
        _ => decision(Action::Hold, Reason::UnverifiedSource),
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Divergence {
    /// Legacy arr re-search has no Pit Crew budget reservation.
    UnbudgetedReplacement,
    /// The legacy import disagrees with an explicit safety veto or lacks evidence.
    ImportWithoutSafeProposal,
    DifferentDecision,
    UnknownLegacyAction,
}
#[derive(Clone, Debug, Serialize)]
pub struct ShadowReport {
    pub schema_version: u8,
    pub job: u64,
    pub download: u64,
    pub observed_at: u64,
    pub legacy_action: LegacyAction,
    pub decision: Decision,
    pub divergence: Option<Divergence>,
    pub interference: QueueInterference,
}
/// Contains no raw upstream text, paths, titles or secrets. Serialize one report
/// per JSONL line; absence of attribution is retained, not guessed.
pub fn compare(o: &ImportObservation) -> ShadowReport {
    let decision = evaluate(o);
    let divergence = match o.legacy_action {
        LegacyAction::Unknown => Some(Divergence::UnknownLegacyAction),
        LegacyAction::BlocklistAndSearch => Some(Divergence::UnbudgetedReplacement),
        LegacyAction::Import if decision.action != Action::Allow => {
            Some(Divergence::ImportWithoutSafeProposal)
        }
        LegacyAction::Import => None,
        LegacyAction::Hold if decision.action == Action::Hold => None,
        _ => Some(Divergence::DifferentDecision),
    };
    let interference = o
        .external_failure
        .filter(|_| o.owned_download == Some(o.download))
        .map_or_else(QueueInterference::default, |evidence| {
            QueueInterference::default().observe(o.download, evidence)
        });
    ShadowReport {
        schema_version: 1,
        job: o.job,
        download: o.download,
        observed_at: o.observed_at,
        legacy_action: o.legacy_action,
        decision,
        divergence,
        interference,
    }
}
