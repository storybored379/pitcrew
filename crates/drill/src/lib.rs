//! Synthetic conversation corpus and independently observed score accounting.
//!
//! Ground truth is never passed to the intake dispatcher. Missing evidence does
//! not become a success, and unknown/duplicate observations reject the run.
use pitcrew_intake::{AuthorizedViewer, CatalogItem, ConversationContext};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub mod drivers;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Corpus {
    pub version: u32,
    pub catalog: Vec<CatalogItem>,
    pub cases: Vec<Case>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Case {
    pub id: String,
    pub tags: Vec<String>,
    pub viewer: Option<AuthorizedViewer>,
    #[serde(default)]
    pub context: ConversationContext,
    pub turns: Vec<Turn>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Turn {
    pub text: String,
    pub expected: Expected,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Expected {
    pub outcome: String,
    #[serde(default)]
    pub targets: Vec<Target>,
    #[serde(default)]
    pub suspicious: bool,
    #[serde(default)]
    pub reply_contains: Vec<String>,
    #[serde(default)]
    pub reply_omits: Vec<String>,
    pub diagnosis: Option<String>,
    pub ready_within_ms: Option<u64>,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Target {
    pub title: String,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub library: String,
    pub kinds: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Observation {
    pub case_id: String,
    pub turn: usize,
    pub outcome: String,
    pub targets: Vec<Target>,
    pub suspicious: bool,
    pub reply: String,
    pub diagnosis: Option<String>,
    pub ready: Option<Ready>,
    /// None means effects were not instrumented, not that there were no effects.
    pub unsafe_actions: Option<Vec<String>>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Ready {
    pub elapsed_ms: u64,
    pub exact_file_verified: bool,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metric {
    pub passed: u64,
    pub observed: u64,
    pub expected: u64,
}
impl Metric {
    fn record(&mut self, pass: Option<bool>) {
        self.expected += 1;
        if let Some(pass) = pass {
            self.observed += 1;
            self.passed += u64::from(pass);
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Score {
    pub intake: Metric,
    pub title: Metric,
    pub episode: Metric,
    pub library: Metric,
    pub symptoms: Metric,
    pub suspicious: Metric,
    pub reply: Metric,
    pub diagnosis: Metric,
    pub ready: Metric,
    pub safe: Metric,
    /// Intake mismatches only; inspect every metric to assess a run's safety.
    pub failures: Vec<String>,
}
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DrillError {
    #[error("invalid corpus: version, identifiers and turns must be valid")]
    InvalidCorpus,
    #[error("observation does not identify a corpus turn")]
    UnknownObservation,
    #[error("duplicate observation for a corpus turn")]
    DuplicateObservation,
    #[error("intake transport rejected a corpus message")]
    Dispatch,
    #[error("intake selected an item absent from the authorized catalog")]
    UnknownItem,
}

pub fn validate(corpus: &Corpus) -> Result<(), DrillError> {
    let mut ids = BTreeSet::new();
    if corpus.version != 1 || corpus.cases.is_empty() {
        return Err(DrillError::InvalidCorpus);
    }
    for item in &corpus.catalog {
        if item.id.trim().is_empty() || item.library_id.trim().is_empty() || !ids.insert(&item.id) {
            return Err(DrillError::InvalidCorpus);
        }
    }
    ids.clear();
    for case in &corpus.cases {
        if case.id.is_empty() || case.turns.is_empty() || !ids.insert(&case.id) {
            return Err(DrillError::InvalidCorpus);
        }
    }
    Ok(())
}

fn project(targets: &[Target], dimension: u8) -> BTreeSet<String> {
    targets
        .iter()
        .map(|target| match dimension {
            0 => format!("{:?}", target.title),
            1 => format!(
                "{:?}/{:?}/{:?}",
                target.title, target.season, target.episode
            ),
            2 => format!(
                "{:?}/{:?}/{:?}/{:?}",
                target.title, target.season, target.episode, target.library
            ),
            _ => {
                let kinds: BTreeSet<_> = target.kinds.iter().collect();
                format!(
                    "{:?}/{:?}/{:?}/{:?}/{kinds:?}",
                    target.title, target.season, target.episode, target.library
                )
            }
        })
        .collect()
}

fn reply_ok(expected: &Expected, observation: &Observation) -> bool {
    let reply = observation.reply.to_lowercase();
    !reply.trim().is_empty()
        && expected
            .reply_contains
            .iter()
            .all(|text| reply.contains(&text.to_lowercase()))
        && expected
            .reply_omits
            .iter()
            .all(|text| !reply.contains(&text.to_lowercase()))
        && (!claims_ready(&reply)
            || observation
                .ready
                .as_ref()
                .is_some_and(|ready| ready.exact_file_verified))
}

fn claims_ready(reply: &str) -> bool {
    [
        "ready to watch",
        "fixed now",
        "all fixed",
        "it is fixed",
        "it's fixed",
        "it’s fixed",
        "now fixed",
        "has been fixed",
        "is now fixed",
        "you can play",
        "you can watch",
        "ready now",
        "now ready",
        "ready for playback",
        "problem is resolved",
        "issue is resolved",
    ]
    .iter()
    .any(|claim| reply.contains(claim))
}

// Check observed selections independently of the parser's authorization logic.
fn observed_item<'a>(
    catalog: &'a [CatalogItem],
    viewer: Option<&AuthorizedViewer>,
    item_id: &str,
) -> Result<&'a CatalogItem, DrillError> {
    catalog
        .iter()
        .find(|item| item.id == item_id)
        .filter(|item| {
            viewer.is_some_and(|viewer| {
                !viewer.account_id.trim().is_empty()
                    && viewer.library_ids.contains(&item.library_id)
            })
        })
        .ok_or(DrillError::UnknownItem)
}

/// Scores full or partial recorded runs. A partial run remains visibly incomplete.
pub fn score(corpus: &Corpus, observations: &[Observation]) -> Result<Score, DrillError> {
    validate(corpus)?;
    let expected: BTreeMap<_, _> = corpus
        .cases
        .iter()
        .flat_map(|case| {
            case.turns
                .iter()
                .enumerate()
                .map(move |(turn, row)| ((case.id.as_str(), turn), &row.expected))
        })
        .collect();
    let mut actual = BTreeMap::new();
    for observation in observations {
        let key = (observation.case_id.as_str(), observation.turn);
        if !expected.contains_key(&key) {
            return Err(DrillError::UnknownObservation);
        }
        if actual.insert(key, observation).is_some() {
            return Err(DrillError::DuplicateObservation);
        }
    }
    let mut result = Score::default();
    for ((case_id, turn), expected) in expected {
        let row = actual.get(&(case_id, turn));
        let matches = row.map(|row| {
            row.outcome == expected.outcome
                && project(&row.targets, 3) == project(&expected.targets, 3)
                && row.suspicious == expected.suspicious
        });
        result.intake.record(matches);
        result
            .title
            .record(row.map(|row| project(&row.targets, 0) == project(&expected.targets, 0)));
        result
            .episode
            .record(row.map(|row| project(&row.targets, 1) == project(&expected.targets, 1)));
        result
            .library
            .record(row.map(|row| project(&row.targets, 2) == project(&expected.targets, 2)));
        result
            .symptoms
            .record(row.map(|row| project(&row.targets, 3) == project(&expected.targets, 3)));
        result
            .suspicious
            .record(row.map(|row| row.suspicious == expected.suspicious));
        result.reply.record(row.map(|row| reply_ok(expected, row)));
        if let Some(label) = &expected.diagnosis {
            result.diagnosis.record(
                row.and_then(|row| row.diagnosis.as_ref())
                    .map(|got| got == label),
            );
        }
        if let Some(limit) = expected.ready_within_ms {
            result.ready.record(
                row.and_then(|row| row.ready.as_ref())
                    .map(|ready| ready.exact_file_verified && ready.elapsed_ms <= limit),
            );
        }
        result.safe.record(row.and_then(|row| {
            row.unsafe_actions.as_ref().map(|actions| {
                actions.is_empty()
                    && row
                        .ready
                        .as_ref()
                        .is_none_or(|ready| ready.exact_file_verified)
                    && (!claims_ready(&row.reply.to_lowercase())
                        || row
                            .ready
                            .as_ref()
                            .is_some_and(|ready| ready.exact_file_verified))
            })
        }));
        if matches != Some(true) {
            result.failures.push(format!("{case_id}:{turn}"));
        }
    }
    Ok(result)
}

/// Export neutral inputs for a reviewed external tool, excluding answer labels.
pub use python_requests as external_requests;

/// Export only request data; never execute HTTP or infer results from labels.
/// An external adapter must map the catalog to fresh synthetic fixtures.
pub fn python_requests(corpus: &Corpus) -> Result<Vec<serde_json::Value>, DrillError> {
    validate(corpus)?;
    Ok(corpus
        .cases
        .iter()
        .flat_map(|case| {
            case.turns.iter().enumerate().map(move |(turn, row)| {
                serde_json::json!({
                    "case_id": case.id, "turn": turn,
                    "body": {"text": row.text},
                    "identity_fixture": case.viewer, "context_fixture": case.context,
                    "catalog_fixture": corpus.catalog,
                    "requires": "fresh_isolated_external_adapter"
                })
            })
        })
        .collect())
}

/// Runs the exact channel dispatcher with synthetic, server-owned authorization.
/// Context advances only after a report proposal; this tool performs no repairs.
pub fn run(corpus: &Corpus) -> Result<Vec<Observation>, DrillError> {
    use pitcrew_domain::channels::{ChannelIdentity, IncomingMessage};
    use pitcrew_intake::IntakeOutcome;
    validate(corpus)?;
    let mut observations = Vec::new();
    for case in &corpus.cases {
        let mut context = case.context.clone();
        for (turn, row) in case.turns.iter().enumerate() {
            let message = IncomingMessage {
                sender: ChannelIdentity {
                    channel_id: "drill".into(),
                    platform_user_id: case.id.clone(),
                },
                message_id: format!("{}-{turn}", case.id),
                conversation_id: case.id.clone(),
                text: row.text.clone(),
                received_at: turn as u64,
            };
            let response =
                pitcrew_bot::dispatch(&message, case.viewer.as_ref(), &corpus.catalog, &context)
                    .map_err(|_| DrillError::Dispatch)?;
            let mut targets = Vec::new();
            for issue in &response.issues {
                let item = observed_item(&corpus.catalog, case.viewer.as_ref(), &issue.item_id)?;
                targets.push(Target {
                    title: item.title.clone(),
                    season: item.season,
                    episode: item.episode,
                    library: item.library_id.clone(),
                    kinds: issue.kinds.iter().map(label).collect(),
                });
            }
            if response.outcome == IntakeOutcome::Report {
                context = ConversationContext {
                    account_id: case
                        .viewer
                        .as_ref()
                        .ok_or(DrillError::UnknownItem)?
                        .account_id
                        .clone(),
                    conversation_id: case.id.clone(),
                    item_ids: response
                        .issues
                        .iter()
                        .map(|issue| issue.item_id.clone())
                        .collect(),
                    report_ids: vec![format!("drill-proposal-{}-{turn}", case.id)],
                };
            }
            observations.push(Observation {
                case_id: case.id.clone(),
                turn,
                outcome: label(&response.outcome),
                targets,
                suspicious: response.suspicious_input,
                reply: response.reply,
                diagnosis: None,
                ready: None,
                // This observation covers this intake-only dispatch. No effect
                // executor, upstream connection or mutation capability is present.
                unsafe_actions: Some(Vec::new()),
            });
        }
    }
    Ok(observations)
}

fn label(value: &impl Serialize) -> String {
    serde_json::to_value(value)
        .expect("enum serializes")
        .as_str()
        .expect("enum is a label")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observed_selection_requires_known_identity_and_current_library_access() {
        let item: CatalogItem = serde_json::from_value(serde_json::json!({
            "id":"film", "title":"Copper Moon", "library_id":"hd",
            "library_name":"Standard", "resolution":"hd"
        }))
        .unwrap();
        let catalog = vec![item];
        let mut viewer = AuthorizedViewer {
            account_id: "synthetic".into(),
            library_ids: vec!["hd".into()],
        };
        assert_eq!(
            observed_item(&catalog, Some(&viewer), "film"),
            Ok(&catalog[0])
        );
        assert_eq!(
            observed_item(&catalog, Some(&viewer), "absent"),
            Err(DrillError::UnknownItem)
        );
        assert_eq!(
            observed_item(&catalog, None, "film"),
            Err(DrillError::UnknownItem)
        );
        for blank in ["", "  "] {
            viewer.account_id = blank.into();
            assert_eq!(
                observed_item(&catalog, Some(&viewer), "film"),
                Err(DrillError::UnknownItem)
            );
        }
        viewer.account_id = "synthetic".into();
        viewer.library_ids = vec!["other".into()];
        assert_eq!(
            observed_item(&catalog, Some(&viewer), "film"),
            Err(DrillError::UnknownItem)
        );
    }
}
