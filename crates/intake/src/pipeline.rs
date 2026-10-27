//! Read-only report-to-diagnosis orchestration. No effect or download capability.
//! Catalog and access inputs come from trusted sync adapters, never viewer text.
use crate::{CatalogItem, Clarification, IntakeOutcome, IntakeResponse, IssueKind, Resolution};
use pitcrew_deciders::{
    Decider, DeciderKind, LocalEvidence, RulesDecider,
    shadow::{DecisionRunner, JsonlSink, RunError},
};
use pitcrew_diagnosis::{
    collectors::{AgentEvidence, Observation, SessionEvidence, Unavailable},
    facts::*,
};
use pitcrew_domain::FileIdentity;
use pitcrew_intake_store::{Store, diagnoses::Diagnosis};
use serde::{Deserialize, Serialize};
use std::{future::Future, pin::Pin};

pub mod collectors;

/// A unique current arr mapping. Paths/identifiers remain local and are excluded
/// from the decision facts. A missing agent identity is learned from the probe.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedTarget {
    pub item: CatalogItem,
    pub resource: String,
    pub arr_instance_id: String,
    pub arr_item_id: u64,
    pub arr_file_id: u64,
    pub agent_path: String,
    pub agent_file: Option<FileIdentity>,
    pub plex_rating_key: String,
    pub observed_at: u64,
    pub expires_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessGrant {
    pub account_id: String,
    pub library_ids: Vec<String>,
    pub observed_at: u64,
    pub expires_at: u64,
}
impl AccessGrant {
    pub fn permits(&self, account: &str, library: &str, now: u64) -> bool {
        self.account_id == account
            && self.observed_at <= now
            && now < self.expires_at
            && self.library_ids.iter().any(|id| id == library)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolveError {
    Unavailable,
    Ambiguous,
    EpisodeRequired,
}
pub trait CatalogResolver: Send + Sync {
    fn resolve(&self, item_id: &str, now: u64) -> Result<ResolvedTarget, ResolveError>;
}
pub trait AccessProvider: Send + Sync {
    fn access(&self, account_id: &str, now: u64) -> Option<AccessGrant>;
}
pub type Clock = dyn Fn() -> u64 + Send + Sync;
pub type EvidenceFuture<'a> = Pin<Box<dyn Future<Output = CollectedEvidence> + Send + 'a>>;
#[derive(Default)]
pub struct CollectedEvidence {
    pub agent: AgentEvidence,
    pub sessions: Observation<SessionEvidence>,
}
pub trait EvidenceSource: Send + Sync {
    fn collect<'a>(
        &'a self,
        target: &'a ResolvedTarget,
        account: &'a str,
        clock: &'a Clock,
    ) -> EvidenceFuture<'a>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Persisted,
    AlreadyDecided,
    Ignored,
    Unauthorized,
    CatalogUnavailable,
    Clarify(Clarification),
    ChangedDuringCollection,
}
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("diagnosis storage unavailable")]
    Storage(#[from] pitcrew_intake_store::Error),
    #[error("invalid stored report")]
    InvalidReport,
    #[error("cloud shadow providers require explicit consent")]
    ConsentRequired,
    #[error("decision comparison failed")]
    Decision(#[from] RunError),
}

pub struct Pipeline<'a> {
    store: &'a Store,
    catalog: &'a dyn CatalogResolver,
    access: &'a dyn AccessProvider,
    evidence: &'a dyn EvidenceSource,
    clock: &'a Clock,
    runner: DecisionRunner,
}
impl<'a> Pipeline<'a> {
    /// Clock values are Unix milliseconds; catalog/access/store times are seconds.
    pub fn new(
        store: &'a Store,
        catalog: &'a dyn CatalogResolver,
        access: &'a dyn AccessProvider,
        evidence: &'a dyn EvidenceSource,
        clock: &'a Clock,
    ) -> Self {
        Self {
            store,
            catalog,
            access,
            evidence,
            clock,
            runner: DecisionRunner::default(),
        }
    }
    /// Rules remain primary. Provider constructors independently enforce consent.
    pub fn with_shadows(
        self,
        shadows: Vec<Box<dyn Decider>>,
        cloud_consent: bool,
    ) -> Result<Self, Error> {
        self.with_shadow_sets(shadows, Vec::new(), cloud_consent)
    }

    /// Local models (on the administrator's own network) need no cloud consent;
    /// any cloud model still does. Their constructors enforce the same split.
    pub fn with_shadow_sets(
        mut self,
        cloud: Vec<Box<dyn Decider>>,
        local: Vec<Box<dyn Decider>>,
        cloud_consent: bool,
    ) -> Result<Self, Error> {
        if !cloud.is_empty() && !cloud_consent {
            return Err(Error::ConsentRequired);
        }
        let mut shadows = cloud;
        shadows.extend(local);
        self.runner = DecisionRunner::new(Box::new(RulesDecider), shadows)?;
        Ok(self)
    }

    pub async fn diagnose(&self, report_id: i64) -> Result<Outcome, Error> {
        if self.store.diagnosis(report_id).await?.is_some() {
            return Ok(Outcome::AlreadyDecided);
        }
        let Some(report) = self.store.report_for_diagnosis(report_id).await? else {
            return Err(Error::InvalidReport);
        };
        let response: IntakeResponse = serde_json::from_value(report.intake.response.clone())
            .map_err(|_| Error::InvalidReport)?;
        if response.outcome != IntakeOutcome::Report {
            return Ok(Outcome::Ignored);
        }
        let [issue] = response.issues.as_slice() else {
            return Ok(Outcome::Clarify(Clarification::Title));
        };
        let started = (self.clock)() / 1000;
        let target = match self.catalog.resolve(&issue.item_id, started) {
            Ok(target) => target,
            Err(ResolveError::EpisodeRequired) => {
                return Ok(Outcome::Clarify(Clarification::Episode));
            }
            Err(ResolveError::Ambiguous) => return Ok(Outcome::Clarify(Clarification::Library)),
            Err(ResolveError::Unavailable) => return Ok(Outcome::CatalogUnavailable),
        };
        // A resolver is trusted to map identities, but stale/mismatched mappings
        // are still rejected here before any local file observation.
        if !valid_target(&target, &issue.item_id, started) {
            return Ok(Outcome::CatalogUnavailable);
        }
        if target.item.season.is_some() != target.item.episode.is_some() {
            return Ok(Outcome::Clarify(Clarification::Episode));
        }
        let Some(grant) = self
            .access
            .access(&report.account_id, started)
            .filter(|g| g.permits(&report.account_id, &target.item.library_id, started))
        else {
            return Ok(Outcome::Unauthorized);
        };
        let mut facts = report_facts(&response, &report.intake.response, &target)?;
        let mut collected = self
            .evidence
            .collect(&target, &report.account_id, self.clock)
            .await;
        let observed_now = (self.clock)();
        // Defense at the composition boundary also covers custom evidence sources.
        let media = match &collected.agent.media {
            Observation::Available(media)
                if media.file.id != 0
                    && target
                        .agent_file
                        .is_none_or(|expected| expected == media.file)
                    && media.observed_at_unix_ms <= observed_now
                    && observed_now - media.observed_at_unix_ms <= 300_000 =>
            {
                Some((media.file, media.observed_at_unix_ms))
            }
            _ => None,
        };
        if media.is_none() {
            collected.agent.media = Observation::Unavailable(Unavailable::Stale);
        }
        collected.agent.apply_to(&mut facts);
        if let Observation::Available(sessions) = &collected.sessions {
            sessions.apply_to(&mut facts);
        }
        apply_audio(&collected.agent, &mut facts);
        let local = LocalEvidence {
            facts,
            viewer_text: String::new(),
            private_context: serde_json::Value::Null,
        };
        // Buffer the sanitized comparison, then commit it with the diagnosis.
        let mut sink = JsonlSink(Vec::new());
        let comparison = self.runner.run(&local, &mut sink).await?;
        let finished = (self.clock)();
        let now = finished / 1000;
        // Slow collectors/cloud calls cannot extend the original access lease.
        if !grant.permits(&report.account_id, &target.item.library_id, now)
            || self
                .access
                .access(&report.account_id, now)
                .is_none_or(|g| !g.permits(&report.account_id, &target.item.library_id, now))
        {
            return Ok(Outcome::Unauthorized);
        }
        if !valid_target(&target, &issue.item_id, now)
            || self.catalog.resolve(&issue.item_id, now).ok().as_ref() != Some(&target)
            || media
                .is_some_and(|(_, observed)| finished < observed || finished - observed > 300_000)
        {
            return Ok(Outcome::ChangedDuringCollection);
        }
        let diagnosis = Diagnosis {
            report_id,
            decided_at: now,
            decider_kind: DeciderKind::Rules,
            decision: comparison.selected.clone(),
            evidence_summary: facts,
            resource: target.resource,
            library_id: target.item.library_id,
            arr_instance_id: target.arr_instance_id,
            arr_item_id: target.arr_item_id,
            arr_file_id: target.arr_file_id,
            agent_file: media.map(|(file, _)| file),
            agent_observed_at: media.map(|(_, at)| at / 1000),
            access_observed_at: grant.observed_at,
            access_expires_at: grant.expires_at,
        };
        let persisted = match self
            .store
            .persist_diagnosis_with_comparison(&diagnosis, &comparison)
            .await
        {
            Ok(persisted) => persisted,
            Err(pitcrew_intake_store::Error::Invalid)
                if self.store.diagnosis(report_id).await?.is_some() =>
            {
                false
            }
            Err(error) => return Err(error.into()),
        };
        Ok(if persisted {
            Outcome::Persisted
        } else {
            Outcome::AlreadyDecided
        })
    }
}

fn valid_target(target: &ResolvedTarget, item_id: &str, now: u64) -> bool {
    target.item.id == item_id
        && !target.resource.is_empty()
        && !target.item.library_id.is_empty()
        && !target.arr_instance_id.is_empty()
        && target.arr_item_id != 0
        && target.arr_file_id != 0
        && !target.agent_path.is_empty()
        && target.observed_at <= now
        && now < target.expires_at
}

fn report_facts(
    response: &IntakeResponse,
    value: &serde_json::Value,
    target: &ResolvedTarget,
) -> Result<EvidenceFacts, Error> {
    let mut facts = EvidenceFacts {
        scope: if target.item.episode.is_some() {
            Scope::Episode
        } else {
            Scope::Movie
        },
        injection_suspected: response.suspicious_input,
        ..EvidenceFacts::default()
    };
    facts.playback.is_4k = target.item.resolution == Resolution::Uhd;
    let issue = &response.issues[0];
    facts.complaint = match issue.kinds.as_slice() {
        [IssueKind::Playback | IssueKind::Buffering] => Complaint::Playback,
        [IssueKind::Quality] => Complaint::VideoBroken,
        [IssueKind::NoAudio] => Complaint::NoAudio,
        [IssueKind::WrongEpisode] => Complaint::WrongEpisode,
        [IssueKind::AudioLanguage] => Complaint::WrongAudioLanguage,
        [IssueKind::Subtitles] => Complaint::Subtitles,
        _ => Complaint::Other,
    };
    if let Some(wizard) = value.get("wizard") {
        facts.complaint = match wizard.get("issue").and_then(|v| v.as_str()) {
            Some("ads") => Complaint::AdsOrWrongVersion,
            Some("other") => Complaint::Other,
            Some("video_broken") => Complaint::VideoBroken,
            _ => facts.complaint,
        };
        let answer = wizard
            .get("answer")
            .and_then(|v| v.as_str())
            .ok_or(Error::InvalidReport)?;
        if answer == "everything_broken" {
            facts.complaint = Complaint::EverythingBroken;
        }
        if issue.kinds == [IssueKind::Buffering] {
            if answer == "helped" {
                facts.complaint = Complaint::SelfResolved;
            }
            if answer == "not_tried" {
                facts.complaint = Complaint::Other;
            }
        }
        facts.audio.requested = match answer {
            "english" => Language::English,
            "spanish" => Language::Spanish,
            _ => Language::Other,
        };
    }
    Ok(facts)
}

fn apply_audio(agent: &AgentEvidence, facts: &mut EvidenceFacts) {
    use pitcrew_domain::media::{ProbeOutcome, StreamFacts};
    let Observation::Available(media) = &agent.media else {
        return;
    };
    let Some(header) = &media.header else {
        return;
    };
    if header.outcome != ProbeOutcome::Passed || !header.faults.is_empty() {
        return;
    }
    let languages: Vec<_> = header
        .streams
        .iter()
        .filter_map(|s| match s {
            StreamFacts::Audio { language, .. } => Some(language.as_deref().and_then(language_tag)),
            _ => None,
        })
        .collect();
    if facts.audio.requested == Language::Other {
        return;
    }
    facts.audio.requested_track = if languages.contains(&Some(facts.audio.requested)) {
        TrackPresence::Present
    } else if !languages.is_empty() && languages.iter().all(|language| language.is_some()) {
        TrackPresence::Absent
    } else {
        TrackPresence::Unknown
    };
}

// A free-text stream tag is not proof of language absence. Recognize explicit
// language codes and treat unspecified/unsupported tags conservatively.
fn language_tag(tag: &str) -> Option<Language> {
    match tag.trim().to_ascii_lowercase().as_str() {
        "en" | "eng" => Some(Language::English),
        "es" | "spa" => Some(Language::Spanish),
        "fr" | "fra" | "fre" | "de" | "deu" | "ger" | "it" | "ita" | "ja" | "jpn" | "ko"
        | "kor" | "pt" | "por" | "ru" | "rus" | "zh" | "zho" | "chi" => Some(Language::Other),
        _ => None,
    }
}
