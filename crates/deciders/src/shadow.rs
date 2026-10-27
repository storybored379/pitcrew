//! Same-evidence comparisons with explicit primary selection and rules fallback.
use crate::{Decider, DeciderKind, DecisionReport, LocalEvidence, RulesDecider, redact};
use serde::Serialize;
use std::io::Write;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ComparisonEntry {
    pub provider: DeciderKind,
    pub result: Option<DecisionReport>,
    /// Static classification only; provider errors and upstream bodies stay out.
    pub failed: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Comparison {
    pub primary: DeciderKind,
    pub used_rules_fallback: bool,
    pub selected: DecisionReport,
    pub entries: Vec<ComparisonEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RunError {
    #[error("each provider may appear only once")]
    DuplicateProvider,
    #[error("comparison could not be recorded")]
    ComparisonLog,
    #[error("rules decision unavailable")]
    RulesUnavailable,
}

/// Sink implementations must persist only this sanitized comparison. A log
/// failure returns no selected recommendation to the caller.
pub trait ComparisonSink {
    fn record(&mut self, comparison: &Comparison) -> Result<(), RunError>;
}

pub struct JsonlSink<W>(pub W);
impl<W: Write> ComparisonSink for JsonlSink<W> {
    fn record(&mut self, comparison: &Comparison) -> Result<(), RunError> {
        let mut line = serde_json::to_vec(comparison).map_err(|_| RunError::ComparisonLog)?;
        line.push(b'\n');
        self.0
            .write_all(&line)
            .map_err(|_| RunError::ComparisonLog)?;
        self.0.flush().map_err(|_| RunError::ComparisonLog)
    }
}

pub struct DecisionRunner {
    primary: Box<dyn Decider>,
    shadows: Vec<Box<dyn Decider>>,
}
impl Default for DecisionRunner {
    fn default() -> Self {
        Self {
            primary: Box::new(RulesDecider),
            shadows: Vec::new(),
        }
    }
}
impl DecisionRunner {
    pub fn new(
        primary: Box<dyn Decider>,
        shadows: Vec<Box<dyn Decider>>,
    ) -> Result<Self, RunError> {
        let mut seen = vec![primary.kind()];
        for provider in &shadows {
            if seen.contains(&provider.kind()) {
                return Err(RunError::DuplicateProvider);
            }
            seen.push(provider.kind());
        }
        Ok(Self { primary, shadows })
    }

    pub async fn run<S: ComparisonSink + ?Sized>(
        &self,
        local: &LocalEvidence,
        log: &mut S,
    ) -> Result<Comparison, RunError> {
        let evidence = redact(local);
        let primary = self.primary.decide(&evidence).await;
        let mut entries = vec![ComparisonEntry {
            provider: self.primary.kind(),
            result: primary.as_ref().ok().cloned(),
            failed: primary.is_err(),
        }];
        let used_rules_fallback = primary.is_err();
        let selected = match primary {
            Ok(report) => report,
            Err(_) => RulesDecider
                .decide(&evidence)
                .await
                .map_err(|_| RunError::RulesUnavailable)?,
        };
        for provider in &self.shadows {
            let result = provider.decide(&evidence).await;
            entries.push(ComparisonEntry {
                provider: provider.kind(),
                failed: result.is_err(),
                result: result.ok(),
            });
        }
        let comparison = Comparison {
            primary: self.primary.kind(),
            used_rules_fallback,
            selected,
            entries,
        };
        log.record(&comparison)?;
        Ok(comparison)
    }
}
