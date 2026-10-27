//! Narrow import-arbiter contract. This crate never contacts an arr instance.
//! A confirmer proposes an exact verified source; only a durable FixJob worker
//! may reserve ownership, persist ImportIntent, recheck access, then dispatch.
pub mod shadow;
use pitcrew_domain::{Access, FileIdentity};
use std::{collections::BTreeSet, sync::OnceLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportWarning {
    MatchedById,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Episode {
    pub season: u16,
    pub number: u16,
}

#[derive(Clone, Debug)]
pub struct MatchedIdRequest {
    pub job: u64,
    pub library: u64,
    pub access: Access,
    pub now: u64,
    pub download: u64,
    pub owned_download: u64,
    pub import_blocked: bool,
    pub warnings: Vec<ImportWarning>,
    pub queue_series: u64,
    pub proposed_series: u64,
    pub proposed_episodes: Vec<Episode>,
    pub rejection_count: usize,
    /// A basename only, from the exact proposed file. Never use the parent path.
    pub filename: String,
    pub bytes: u64,
    pub source: FileIdentity,
    pub decoded_source: Option<FileIdentity>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedImportProposal {
    pub job: u64,
    pub library: u64,
    pub download: u64,
    pub series: u64,
    pub episodes: BTreeSet<Episode>,
    pub source: FileIdentity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportDenial {
    Access,
    Ownership,
    Warnings,
    Series,
    Rejections,
    Sample,
    Episodes,
    UnverifiedSource,
}

pub const MIN_EPISODE_BYTES: u64 = 70 * 1024 * 1024;

/// Does not authorize a side effect. The receiving arbiter must use these exact
/// identities in a FixJob, including media-item ownership and destination verification.
pub fn matched_by_id(request: &MatchedIdRequest) -> Result<VerifiedImportProposal, ImportDenial> {
    if !matches!(request.access, Access::Granted { library, expires_at }
        if library == request.library && expires_at > request.now)
    {
        return Err(ImportDenial::Access);
    }
    if request.download != request.owned_download {
        return Err(ImportDenial::Ownership);
    }
    if !request.import_blocked
        || request.warnings.is_empty()
        || request
            .warnings
            .iter()
            .any(|warning| *warning != ImportWarning::MatchedById)
    {
        return Err(ImportDenial::Warnings);
    }
    if request.proposed_series == 0 || request.proposed_series != request.queue_series {
        return Err(ImportDenial::Series);
    }
    if request.rejection_count != 0 {
        return Err(ImportDenial::Rejections);
    }
    reject_sample(&request.filename, request.bytes)?;
    let episodes = parse_episodes(&request.filename).ok_or(ImportDenial::Episodes)?;
    let proposed: BTreeSet<_> = request.proposed_episodes.iter().copied().collect();
    if proposed != episodes || proposed.len() != request.proposed_episodes.len() {
        return Err(ImportDenial::Episodes);
    }
    if request.decoded_source != Some(request.source) {
        return Err(ImportDenial::UnverifiedSource);
    }
    Ok(VerifiedImportProposal {
        job: request.job,
        library: request.library,
        download: request.download,
        series: request.proposed_series,
        episodes,
        source: request.source,
    })
}

fn reject_sample(filename: &str, bytes: u64) -> Result<(), ImportDenial> {
    static SAMPLE: OnceLock<regex::Regex> = OnceLock::new();
    let sample = SAMPLE.get_or_init(|| {
        regex::Regex::new(r"(?i)(?:^|[^a-z0-9])(?:sample|NCOP|NCED)(?:$|[^a-z0-9])|!sample")
            .expect("static regex")
    });
    if bytes < MIN_EPISODE_BYTES || sample.is_match(filename) {
        return Err(ImportDenial::Sample);
    }
    Ok(())
}

fn parse_episodes(filename: &str) -> Option<BTreeSet<Episode>> {
    // Reject paths and multiple tokens: a parent's episode number or a second
    // ambiguous episode token must never authorize a different destination.
    if filename.contains(['/', '\\']) {
        return None;
    }
    static EPISODES: OnceLock<regex::Regex> = OnceLock::new();
    let pattern = EPISODES.get_or_init(|| {
        regex::Regex::new(r"(?i)\bS([0-9]+)E([0-9]+)(?:(?:-E?|E)([0-9]+))?\b")
            .expect("static regex")
    });
    let mut matches = pattern.captures_iter(filename);
    let parts = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    // A regex must not fall back to a shorter valid token when a malformed
    // chained range follows it (for example S01E02-03-04).
    let suffix = &filename[parts.get(0)?.end()..];
    static EXTRA_RANGE: OnceLock<regex::Regex> = OnceLock::new();
    if EXTRA_RANGE
        .get_or_init(|| regex::Regex::new(r"(?i)^-E?[0-9]").expect("static regex"))
        .is_match(suffix)
    {
        return None;
    }
    let season = parts[1].parse().ok()?;
    let start: u16 = parts[2].parse().ok()?;
    let end: u16 = parts
        .get(3)
        .map_or(Ok(start), |part| part.as_str().parse())
        .ok()?;
    if end < start || end - start > 100 {
        return None;
    }
    Some(
        (start..=end)
            .map(|number| Episode { season, number })
            .collect(),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArbiterReceipt {
    /// The arbiter persisted/deduplicated the request; not proof of an imported file.
    Queued,
    /// Lost response: reconcile the same job; never submit an untracked second command.
    Uncertain,
    Rejected,
}

pub trait ImportArbiter {
    fn propose(&mut self, proposal: VerifiedImportProposal) -> ArbiterReceipt;
}

/// Exact replacement proof assembled by the durable engine from fresh arr and
/// agent reads. It authorizes no I/O on its own. All candidate targets must equal
/// the owned item; ambiguity, rejections, and an unverified source remain held.
pub struct ReplacementProof {
    pub original: FileIdentity,
    pub current: FileIdentity,
    pub source: FileIdentity,
    pub decoded_source: Option<FileIdentity>,
    pub owned_download: u64,
    pub download: u64,
    pub exact_target: bool,
    pub candidate_count: usize,
    pub rejection_count: Option<usize>,
    /// Basename and measured size of the exact ManualImport candidate. A healthy
    /// decode alone does not distinguish an episode/movie from its sample.
    pub filename: String,
    pub bytes: Option<u64>,
}

pub fn replacement(proof: &ReplacementProof) -> Result<(), ImportDenial> {
    if proof.original != proof.current {
        return Err(ImportDenial::Ownership);
    }
    if proof.download != proof.owned_download {
        return Err(ImportDenial::Ownership);
    }
    if !proof.exact_target || proof.candidate_count != 1 {
        return Err(ImportDenial::Episodes);
    }
    if proof.rejection_count != Some(0) {
        return Err(ImportDenial::Rejections);
    }
    reject_sample(&proof.filename, proof.bytes.ok_or(ImportDenial::Sample)?)?;
    if proof.decoded_source != Some(proof.source) {
        return Err(ImportDenial::UnverifiedSource);
    }
    Ok(())
}
