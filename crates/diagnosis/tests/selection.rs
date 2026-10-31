use pitcrew_diagnosis::{rules::*, selection::select};

fn baseline(action: RecommendedAction, constraint: Option<ReplacementConstraint>) -> Decision {
    Decision {
        kind: DiagnosisKind::Broken,
        action,
        constraint,
        reason: Reason::DecodeFailed,
    }
}
#[test]
fn only_rule_action_and_exact_constraints_or_conservative_review_are_allowed() {
    use RecommendedAction::*;
    let actions = [
        Replace,
        Analyze,
        AdviseHd,
        ClientAdvice,
        SelectAudioTrack,
        NotifyAdmin,
        NotifyUrgent,
        ExplainPlayOrigin,
        Close,
        CollectEvidence,
    ];
    let constraints = [
        None,
        Some(ReplacementConstraint::AnyWorking),
        Some(ReplacementConstraint::Sdr),
        Some(ReplacementConstraint::HdrOkNoProfile5),
        Some(ReplacementConstraint::EnglishAudio),
        Some(ReplacementConstraint::SpanishAudio),
    ];
    for rule_action in actions {
        for rule_constraint in constraints {
            let rule = baseline(rule_action, rule_constraint);
            for action in actions {
                for constraint in constraints {
                    let proposed = Decision {
                        kind: DiagnosisKind::Outage,
                        reason: Reason::ProviderAssessment,
                        ..baseline(action, constraint)
                    };
                    let result = select(rule, proposed);
                    if rule_action == action && rule_constraint == constraint {
                        assert_eq!(result, Some(rule));
                    } else if !matches!(rule_action, NotifyUrgent | Close)
                        && matches!(action, NotifyAdmin | CollectEvidence)
                        && constraint.is_none()
                    {
                        assert_eq!(
                            result,
                            Some(Decision {
                                kind: DiagnosisKind::Unknown,
                                action,
                                constraint: None,
                                reason: Reason::ProviderAssessment
                            })
                        );
                    } else {
                        assert_eq!(result, None);
                    }
                }
            }
        }
    }
}
#[test]
fn model_cannot_suppress_injection_review_or_invent_supporting_evidence() {
    let rules = Decision {
        kind: DiagnosisKind::Unknown,
        action: RecommendedAction::NotifyAdmin,
        constraint: None,
        reason: Reason::UntrustedInstructions,
    };
    assert_eq!(
        select(rules, baseline(RecommendedAction::CollectEvidence, None)),
        None
    );
    assert_eq!(
        select(rules, baseline(RecommendedAction::NotifyAdmin, None)),
        Some(rules)
    );
}
