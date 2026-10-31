//! Rules own the complete option set. A model supplies a preference, never new
//! evidence or weaker replacement constraints.
use crate::rules::{Decision, DiagnosisKind, Reason, RecommendedAction};

/// Preserve the deterministic diagnosis when a model agrees with its action and
/// exact constraint. A model may also ask for review or more evidence, except
/// when the rules require an urgent alert, explicit closure, or injection review.
/// All other proposals are rejected and the caller must use the rules result.
pub fn select(rules: Decision, proposed: Decision) -> Option<Decision> {
    if proposed.action == rules.action && proposed.constraint == rules.constraint {
        return Some(rules);
    }
    if matches!(
        rules.action,
        RecommendedAction::NotifyUrgent | RecommendedAction::Close
    ) || rules.reason == Reason::UntrustedInstructions
        || proposed.constraint.is_some()
    {
        return None;
    }
    match proposed.action {
        RecommendedAction::NotifyAdmin | RecommendedAction::CollectEvidence => Some(Decision {
            kind: DiagnosisKind::Unknown,
            action: proposed.action,
            constraint: None,
            reason: Reason::ProviderAssessment,
        }),
        _ => None,
    }
}
