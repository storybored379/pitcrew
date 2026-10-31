//! Local parity reports. Unknown facts cannot count toward the safe-parity gate.
use pitcrew_domain::Effect;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Grab,
    Import,
    DeleteLibrary,
    RemoveDownload,
    Decode,
    CheckTarget,
    UpgradeGuard,
    Ready,
    AdminFailure,
    ViewerFailure,
    Reconcile,
    Hold,
}
impl Action {
    pub fn from_effect(effect: &Effect) -> Self {
        match effect {
            Effect::Grab { .. } => Self::Grab,
            Effect::ImportVerifiedDownload { .. } => Self::Import,
            Effect::RemoveRejectedDownload { .. } => Self::RemoveDownload,
            Effect::DecodeDownload { .. } | Effect::DecodeImported { .. } => Self::Decode,
            Effect::CheckTarget { .. } => Self::CheckTarget,
            Effect::InstallUpgradeGuard { .. } => Self::UpgradeGuard,
            Effect::NotifyReady { .. } => Self::Ready,
            Effect::NotifyAdmin { .. } => Self::AdminFailure,
            Effect::NotifyViewerFailure { .. } => Self::ViewerFailure,
            _ => Self::Reconcile,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    ExternalTool,
    ExternalImporter,
    /// Retained wire identity for already persisted version-1 observations.
    #[serde(rename = "python")]
    Legacy,
    Engine,
    #[serde(other)]
    Unknown,
}
impl Actor {
    /// Source compatibility for existing version-1 snapshot integrations.
    #[allow(non_upper_case_globals)]
    pub const Python: Self = Self::Legacy;
}

/// Exact installation/download join, hashed so comparison projections need not
/// retain raw upstream identifiers. Neither titles nor media IDs participate.
pub fn download_correlation(instance: &str, download: &str) -> Option<String> {
    use sha2::{Digest, Sha256};
    if [instance, download].iter().any(|value| {
        value.is_empty() || value.len() > 256 || !value.bytes().all(|b| b.is_ascii_graphic())
    }) {
        return None;
    }
    Some(hex::encode(Sha256::digest(format!(
        "{}:{instance}{download}",
        instance.len()
    ))))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchReceipt {
    pub correlation: String,
    pub intent: pitcrew_domain::Intent,
}

/// A trusted dispatcher calls this only with the exact download identifier from
/// its accepted request/receipt, never a guessed queue match. A bound route or
/// pending/shadow intent alone is not dispatch evidence. This record establishes
/// actor attribution only, not remote completion or safety certification.
pub async fn record_dispatch(
    store: &pitcrew_store::Store,
    intent: &pitcrew_domain::Intent,
    instance: &str,
    download: &str,
    now: u64,
) -> Result<bool, pitcrew_store::StoreError> {
    use pitcrew_store::{OutboxStatus, StoreError};
    let correlation = download_correlation(instance, download).ok_or(StoreError::InvalidInput)?;
    if !matches!(
        intent.effect,
        Effect::Grab { .. }
            | Effect::ImportVerifiedDownload { .. }
            | Effect::RemoveRejectedDownload { .. }
    ) || !store.outbox(intent.job).await?.iter().any(|row| {
        row.intent == *intent
            && matches!(
                row.status,
                OutboxStatus::Started | OutboxStatus::Completed | OutboxStatus::Uncertain
            )
    }) {
        return Err(StoreError::InvalidInput);
    }
    store
        .ingest_observation(
            "engine_dispatch",
            &format!("{}:{}", intent.job, intent.sequence),
            &serde_json::to_value(DispatchReceipt {
                correlation,
                intent: intent.clone(),
            })?,
            now,
        )
        .await
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activity {
    pub action: Action,
    pub actor: Actor,
    /// Only true when a trusted job/download correlation was supplied. Timing,
    /// title similarity and arr media IDs alone never attribute an actor.
    pub correlated: bool,
    pub safety: Safety,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Safety {
    pub access: Option<bool>,
    pub budget: Option<bool>,
    pub replacement_verified: Option<bool>,
    pub exact_import_decoded: Option<bool>,
    pub notification_unique: Option<bool>,
    pub no_auto_redownload: Option<bool>,
    pub reconciled: Option<bool>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unsafe {
    Access,
    Budget,
    UnverifiedReplacement,
    UnverifiedReady,
    DuplicateNotification,
    AutoRedownload,
    UnreconciledMutation,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Agreement {
    Match,
    Different,
    InsufficientEvidence,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diff {
    pub version: u8,
    pub fix: String,
    pub observed: Activity,
    pub proposed: Activity,
    pub agreement: Agreement,
    pub observed_unsafe: Vec<Unsafe>,
    pub proposed_unsafe: Vec<Unsafe>,
    pub missing_evidence: bool,
}
fn evaluate(activity: &Activity) -> (Vec<Unsafe>, bool) {
    let s = &activity.safety;
    let mut checks = vec![];
    match activity.action {
        Action::Grab => checks.extend([
            (s.access, Unsafe::Access),
            (s.budget, Unsafe::Budget),
            (s.reconciled, Unsafe::UnreconciledMutation),
        ]),
        Action::Import | Action::DeleteLibrary => checks.extend([
            (s.access, Unsafe::Access),
            (s.replacement_verified, Unsafe::UnverifiedReplacement),
            (s.reconciled, Unsafe::UnreconciledMutation),
        ]),
        Action::RemoveDownload => checks.extend([
            (s.access, Unsafe::Access),
            (s.no_auto_redownload, Unsafe::AutoRedownload),
            (s.reconciled, Unsafe::UnreconciledMutation),
        ]),
        Action::UpgradeGuard => checks.extend([
            (s.access, Unsafe::Access),
            (s.reconciled, Unsafe::UnreconciledMutation),
        ]),
        Action::Ready => checks.extend([
            (s.exact_import_decoded, Unsafe::UnverifiedReady),
            (s.notification_unique, Unsafe::DuplicateNotification),
        ]),
        Action::AdminFailure | Action::ViewerFailure => {
            checks.push((s.notification_unique, Unsafe::DuplicateNotification))
        }
        _ => {}
    }
    let missing = !activity.correlated
        || activity.actor == Actor::Unknown
        || checks.iter().any(|(fact, _)| fact.is_none());
    (
        checks
            .into_iter()
            .filter_map(|(fact, class)| (fact == Some(false)).then_some(class))
            .collect(),
        missing,
    )
}
pub fn compare(fix: String, observed: Activity, proposed: Activity) -> Diff {
    let (observed_unsafe, observed_missing) = evaluate(&observed);
    let (proposed_unsafe, proposed_missing) = evaluate(&proposed);
    let missing_evidence = observed_missing || proposed_missing;
    let agreement = if missing_evidence {
        Agreement::InsufficientEvidence
    } else if observed.action == proposed.action {
        Agreement::Match
    } else {
        Agreement::Different
    };
    Diff {
        version: 1,
        fix,
        observed,
        proposed,
        agreement,
        observed_unsafe,
        proposed_unsafe,
        missing_evidence,
    }
}
pub fn summary(reports: &[Diff]) -> String {
    let matches = reports
        .iter()
        .filter(|r| r.agreement == Agreement::Match)
        .count();
    let unknown = reports.iter().filter(|r| r.missing_evidence).count();
    let unsafe_observed = reports
        .iter()
        .filter(|r| !r.observed_unsafe.is_empty())
        .count();
    let unsafe_proposed = reports
        .iter()
        .filter(|r| !r.proposed_unsafe.is_empty())
        .count();
    let mut text = format!(
        "{} comparisons: {matches} matching actions, {unknown} incomplete; {unsafe_observed} unsafe observed, {unsafe_proposed} unsafe proposed. Unknown evidence does not establish safe parity.",
        reports.len()
    );
    let mut counts = [0usize; 5];
    let mut pairs = std::collections::BTreeMap::new();
    for report in reports {
        let actor = if report.observed.correlated {
            report.observed.actor
        } else {
            Actor::Unknown
        };
        let index = match actor {
            Actor::Engine => 0,
            Actor::ExternalTool => 1,
            Actor::ExternalImporter => 2,
            Actor::Legacy => 3,
            Actor::Unknown => 4,
        };
        counts[index] += 1;
        if actor != Actor::Unknown {
            *pairs
                .entry((actor, report.observed.action, report.proposed.action))
                .or_insert(0usize) += 1;
        }
    }
    let [engine, tool, importer, legacy, unknown] = counts;
    text.push_str(&format!("\n{} attributed actions; {unknown} unattributed routine/unknown activity. Per actor: engine={engine}, external_tool={tool}, external_importer={importer}, python={legacy}, unknown={unknown}.", reports.len() - unknown));
    for ((actor, observed, proposed), count) in pairs {
        text.push_str(&format!(
            "\n{actor:?}: observed {observed:?} -> proposed {proposed:?}: {count}"
        ));
    }
    text
}
