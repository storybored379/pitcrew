use pitcrew_arbiter::shadow::{Action, Decision, Divergence, LegacyAction, Reason, divergence};

#[test]
fn trusted_action_comparison_keeps_every_existing_safety_decision() {
    for action in [Action::Allow, Action::Deny, Action::Hold] {
        let decision = Decision {
            action,
            reason: Reason::AccessUnknown,
        };
        for (observed, expected) in [
            (LegacyAction::Unknown, Some(Divergence::UnknownLegacyAction)),
            (
                LegacyAction::BlocklistAndSearch,
                Some(Divergence::UnbudgetedReplacement),
            ),
            (LegacyAction::Remove, Some(Divergence::DifferentDecision)),
            (
                LegacyAction::Import,
                if action == Action::Allow {
                    None
                } else {
                    Some(Divergence::ImportWithoutSafeProposal)
                },
            ),
            (
                LegacyAction::Hold,
                if action == Action::Hold {
                    None
                } else {
                    Some(Divergence::DifferentDecision)
                },
            ),
        ] {
            assert_eq!(
                divergence(observed, decision),
                expected,
                "{observed:?} / {action:?}"
            );
        }
        assert_eq!(decision.action, action);
        assert_eq!(decision.reason, Reason::AccessUnknown);
    }
}
