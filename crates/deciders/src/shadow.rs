//! Same-evidence comparisons with explicit primary selection and rules fallback.
use crate::{Decider, DeciderKind, DecisionReport, LocalEvidence, RulesDecider, redact};
use crate::{providers::Accounting, schedule::Schedule};
use pitcrew_diagnosis::{facts::EvidenceFacts, rules::classify, selection::select};
use serde::Serialize;
use std::{io::Write, time::Duration};

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ComparisonEntry {
    pub provider: DeciderKind,
    pub result: Option<DecisionReport>,
    /// Static classification only; provider errors and upstream bodies stay out.
    pub failed: bool,
    /// Per-attempt counters, including usage from rejected structured answers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accounting: Option<Accounting>,
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
    schedule: Schedule,
}
impl Default for DecisionRunner {
    fn default() -> Self {
        Self {
            primary: Box::new(RulesDecider),
            shadows: Vec::new(),
            schedule: Schedule::default(),
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
        Ok(Self {
            primary,
            shadows,
            schedule: Schedule::default(),
        })
    }

    pub fn with_schedule(schedule: Schedule) -> Self {
        Self {
            schedule,
            ..Self::default()
        }
    }

    pub fn uses_model(&self, facts: &EvidenceFacts) -> bool {
        self.schedule
            .route(classify(facts).kind)
            .map_or(self.primary.kind(), |r| r.provider)
            != DeciderKind::Rules
    }

    pub async fn run<S: ComparisonSink + ?Sized>(
        &self,
        local: &LocalEvidence,
        log: &mut S,
    ) -> Result<Comparison, RunError> {
        let evidence = redact(local);
        let rule = classify(evidence.facts());
        let route = self.schedule.route(rule.kind);
        // A fresh provider per scheduled attempt makes counters exact even when
        // several reports run concurrently. Construction failure sends nothing.
        let timeout = route.map_or(Duration::from_secs(30), |r| r.timeout);
        let deadline = tokio::time::Instant::now() + timeout;
        let created = match route {
            Some(route) => Some(
                tokio::time::timeout_at(deadline, route.create())
                    .await
                    .unwrap_or(Err(crate::DeciderError::Timeout)),
            ),
            None => None,
        };
        let provider = match &created {
            Some(Ok(provider)) => Some(provider.as_ref()),
            Some(Err(_)) => None,
            None => Some(self.primary.as_ref()),
        };
        let kind = route.map_or(self.primary.kind(), |r| r.provider);
        let primary = match provider {
            Some(provider) => decide_before_deadline(provider, &evidence, deadline).await,
            None => Err(crate::DeciderError::Configuration(
                "selected provider unavailable",
            )),
        };
        let accounting = created
            .as_ref()
            .filter(|_| kind != DeciderKind::Rules)
            .map(|_| {
                provider
                    .and_then(Decider::attempt_accounting)
                    .unwrap_or_default()
            });
        let chosen = primary
            .as_ref()
            .ok()
            .and_then(|report| select(rule, report.decision));
        let used_rules_fallback = chosen.is_none();
        let selected = match chosen {
            Some(decision) => DecisionReport {
                decision,
                usage: primary.as_ref().expect("validated report").usage,
            },
            None => DecisionReport {
                decision: rule,
                usage: crate::Usage::default(),
            },
        };
        let mut entries = vec![ComparisonEntry {
            provider: kind,
            result: primary.ok(),
            failed: used_rules_fallback,
            accounting,
        }];
        for provider in &self.shadows {
            let result = tokio::time::timeout(Duration::from_secs(30), provider.decide(&evidence))
                .await
                .unwrap_or(Err(crate::DeciderError::Timeout));
            entries.push(ComparisonEntry {
                provider: provider.kind(),
                failed: result.is_err(),
                result: result.ok(),
                accounting: None,
            });
        }
        let comparison = Comparison {
            primary: kind,
            used_rules_fallback,
            selected,
            entries,
        };
        log.record(&comparison)?;
        Ok(comparison)
    }
}

async fn decide_before_deadline(
    provider: &dyn Decider,
    evidence: &crate::RedactedEvidence,
    deadline: tokio::time::Instant,
) -> Result<DecisionReport, crate::DeciderError> {
    // Tokio polls a ready inner future before its timer. Check explicitly so a
    // starved runtime cannot dispatch a request after construction used its budget.
    if tokio::time::Instant::now() >= deadline {
        return Err(crate::DeciderError::Timeout);
    }
    tokio::time::timeout_at(deadline, provider.decide(evidence))
        .await
        .unwrap_or(Err(crate::DeciderError::Timeout))
}

#[cfg(test)]
mod deadline_tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn exact_deadline_refuses_even_an_immediately_ready_decider() {
        let local = LocalEvidence {
            facts: EvidenceFacts::default(),
            viewer_text: String::new(),
            private_context: serde_json::Value::Null,
        };
        let evidence = redact(&local);
        let now = tokio::time::Instant::now();
        assert_eq!(
            decide_before_deadline(&RulesDecider, &evidence, now).await,
            Err(crate::DeciderError::Timeout)
        );
        assert!(
            decide_before_deadline(&RulesDecider, &evidence, now + Duration::from_nanos(1))
                .await
                .is_ok()
        );
    }
}
