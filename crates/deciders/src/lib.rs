//! Rules by default; optional cloud recommendations cannot execute effects.
//! Every provider receives the same closed, redacted fact projection.
pub mod providers;
pub mod redaction;
pub mod reply;
pub mod schedule;
pub mod shadow;

pub use pitcrew_diagnosis::rules::Decision;
pub use providers::{DeciderError, Usage};
pub use redaction::{LocalEvidence, RedactedEvidence, redact};
use serde::{Deserialize, Serialize};
use std::{future::Future, pin::Pin};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeciderKind {
    Rules,
    Jev,
    Anthropic,
    /// Any OpenAI-style chat completions server, cloud or local.
    #[serde(rename = "openai_compatible")]
    OpenAiCompatible,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionReport {
    pub decision: Decision,
    pub usage: Usage,
}

pub type DecisionFuture<'a> =
    Pin<Box<dyn Future<Output = Result<DecisionReport, DeciderError>> + Send + 'a>>;

/// Implementations only recommend; access, budgets, identity and FixJob state
/// remain mandatory at the engine boundary.
pub trait Decider: Send + Sync {
    fn kind(&self) -> DeciderKind;
    /// Optional counters; a scheduled request owns its provider instance.
    fn attempt_accounting(&self) -> Option<providers::Accounting> {
        None
    }
    fn decide<'a>(&'a self, evidence: &'a RedactedEvidence) -> DecisionFuture<'a>;
}

/// Retain a provider handle for accounting while the runner owns another clone.
impl<T: Decider + ?Sized> Decider for std::sync::Arc<T> {
    fn kind(&self) -> DeciderKind {
        self.as_ref().kind()
    }

    fn attempt_accounting(&self) -> Option<providers::Accounting> {
        self.as_ref().attempt_accounting()
    }

    fn decide<'a>(&'a self, evidence: &'a RedactedEvidence) -> DecisionFuture<'a> {
        self.as_ref().decide(evidence)
    }
}

#[derive(Default)]
pub struct RulesDecider;
impl Decider for RulesDecider {
    fn kind(&self) -> DeciderKind {
        DeciderKind::Rules
    }
    fn decide<'a>(&'a self, evidence: &'a RedactedEvidence) -> DecisionFuture<'a> {
        Box::pin(async move {
            Ok(DecisionReport {
                decision: pitcrew_diagnosis::rules::classify(evidence.facts()),
                usage: Usage::default(),
            })
        })
    }
}
