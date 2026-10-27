use pitcrew_arbiter::shadow::*;
use pitcrew_domain::{Access, FileIdentity, queue::*};
fn valid() -> ImportObservation {
    ImportObservation {
        job: 4,
        library: 2,
        download: 8,
        owned_download: Some(8),
        observed_at: 100,
        kind: MediaKind::Episode,
        import_blocked: Some(true),
        access: Access::Granted {
            library: 2,
            expires_at: 101,
        },
        source: Some(FileIdentity {
            id: 11,
            revision: 3,
        }),
        decoded_source: Some(FileIdentity {
            id: 11,
            revision: 3,
        }),
        facts: ImportFacts {
            title: TitleMatch::Exact,
            year_matches: Some(true),
            language_matches: Some(true),
            episodes_match: Some(true),
            no_rejections: Some(true),
            no_samples: Some(true),
            existing_file: Some(false),
            video_files: Some(1),
        },
        external_failure: None,
        legacy_action: LegacyAction::Import,
    }
}
fn check(o: &ImportObservation, action: Action, reason: Reason) {
    assert_eq!(evaluate(o), Decision { action, reason });
}
#[test]
fn clear_evidence_allows_exact_source_proposal_for_both_media_kinds() {
    for kind in [MediaKind::Episode, MediaKind::Movie] {
        for title in [TitleMatch::Exact, TitleMatch::Prefix] {
            let mut o = valid();
            o.kind = kind;
            o.facts.title = title;
            if kind == MediaKind::Movie {
                o.facts.episodes_match = None;
            }
            check(&o, Action::Allow, Reason::VerifiedProposal);
        }
    }
    let mut o = valid();
    o.facts.video_files = Some(2);
    check(&o, Action::Hold, Reason::MissingFacts);
}
#[test]
fn access_and_ownership_are_current_and_exact() {
    for (access, action, reason) in [
        (Access::Unknown, Action::Hold, Reason::AccessUnknown),
        (Access::Denied, Action::Deny, Reason::AccessDenied),
        (
            Access::Granted {
                library: 3,
                expires_at: 101,
            },
            Action::Deny,
            Reason::AccessDenied,
        ),
        (
            Access::Granted {
                library: 2,
                expires_at: 100,
            },
            Action::Deny,
            Reason::AccessDenied,
        ),
        (
            Access::Granted {
                library: 2,
                expires_at: 99,
            },
            Action::Deny,
            Reason::AccessDenied,
        ),
    ] {
        let mut o = valid();
        o.access = access;
        check(&o, action, reason);
    }
    let mut o = valid();
    o.owned_download = None;
    check(&o, Action::Hold, Reason::OwnershipUnknown);
    o.owned_download = Some(9);
    check(&o, Action::Deny, Reason::OwnershipMismatch);
    for state in [None, Some(false)] {
        let mut o = valid();
        o.import_blocked = state;
        check(&o, Action::Hold, Reason::QueueNotBlocked);
    }
}
#[test]
fn missing_facts_hold_contradictions_deny() {
    let mut o = valid();
    o.facts.title = TitleMatch::Unknown;
    check(&o, Action::Hold, Reason::MissingFacts);
    o.facts.title = TitleMatch::Conflict;
    check(&o, Action::Deny, Reason::WrongTitle);
    for index in 0..5 {
        for value in [None, Some(false)] {
            let mut o = valid();
            let (field, reason) = match index {
                0 => (&mut o.facts.year_matches, Reason::WrongYear),
                1 => (&mut o.facts.language_matches, Reason::WrongLanguage),
                2 => (&mut o.facts.no_rejections, Reason::ArrRejections),
                3 => (&mut o.facts.no_samples, Reason::Sample),
                _ => (&mut o.facts.episodes_match, Reason::WrongEpisodes),
            };
            *field = value;
            if value.is_none() {
                check(&o, Action::Hold, Reason::MissingFacts);
            } else {
                check(&o, Action::Deny, reason);
            }
        }
    }
    for count in [None, Some(0)] {
        for kind in [MediaKind::Episode, MediaKind::Movie] {
            let mut o = valid();
            o.kind = kind;
            o.facts.video_files = count;
            check(&o, Action::Hold, Reason::MissingFacts);
        }
    }
    let mut o = valid();
    o.facts.existing_file = None;
    check(&o, Action::Hold, Reason::MissingFacts);
    o.facts.existing_file = Some(true);
    check(&o, Action::Hold, Reason::ExistingFile);
    o = valid();
    o.kind = MediaKind::Movie;
    o.facts.video_files = Some(2);
    check(&o, Action::Hold, Reason::MultipleMovies);
}
#[test]
fn every_source_identity_component_is_checked_and_missing_is_unknown() {
    for (source, decoded, reason, action) in [
        (None, None, Reason::UnverifiedSource, Action::Hold),
        (valid().source, None, Reason::UnverifiedSource, Action::Hold),
        (
            None,
            valid().decoded_source,
            Reason::UnverifiedSource,
            Action::Hold,
        ),
        (
            Some(FileIdentity {
                id: 12,
                revision: 3,
            }),
            valid().decoded_source,
            Reason::SourceChanged,
            Action::Deny,
        ),
        (
            Some(FileIdentity {
                id: 11,
                revision: 4,
            }),
            valid().decoded_source,
            Reason::SourceChanged,
            Action::Deny,
        ),
    ] {
        let mut o = valid();
        o.source = source;
        o.decoded_source = decoded;
        check(&o, action, reason);
    }
}
#[test]
fn diff_identifies_unsafe_actions_and_keeps_unknown_attribution() {
    let o = valid();
    let r = compare(&o);
    assert_eq!(r.schema_version, 1);
    assert_eq!(r.job, 4);
    assert_eq!(r.download, 8);
    assert_eq!(r.observed_at, 100);
    assert_eq!(r.legacy_action, LegacyAction::Import);
    assert_eq!(r.decision, evaluate(&o));
    assert_eq!(r.divergence, None);
    assert_eq!(r.interference, QueueInterference::default());
    let encoded = serde_json::to_value(&r).unwrap();
    assert!(encoded.get("facts").is_none());
    assert!(encoded.get("source").is_none());
    for (legacy, divergence) in [
        (LegacyAction::Unknown, Divergence::UnknownLegacyAction),
        (
            LegacyAction::BlocklistAndSearch,
            Divergence::UnbudgetedReplacement,
        ),
        (LegacyAction::Remove, Divergence::DifferentDecision),
        (LegacyAction::Hold, Divergence::DifferentDecision),
    ] {
        let mut o = valid();
        o.legacy_action = legacy;
        assert_eq!(compare(&o).divergence, Some(divergence));
    }
    let mut o = valid();
    o.access = Access::Unknown;
    assert_eq!(
        compare(&o).divergence,
        Some(Divergence::ImportWithoutSafeProposal)
    );
    o.legacy_action = LegacyAction::Hold;
    assert_eq!(compare(&o).divergence, None);
    o.access = Access::Denied;
    assert_eq!(compare(&o).divergence, Some(Divergence::DifferentDecision));
}
#[test]
fn external_queue_race_is_attributed_only_from_authoritative_evidence() {
    let evidence = QueueFailureEvidence {
        download: 8,
        completed_at: 100,
        failed_at: 110,
        removed: true,
        blocklisted: true,
        marked_failed_history: true,
        local_audit: LocalMutationAudit::NoMatchingMutation,
        actor: Some(QueueActor::NzbdavQueueRules),
    };
    let mut o = valid();
    o.external_failure = Some(evidence);
    let expected = QueueInterference {
        route: QueueImportRoute::ImportImmediatelyThenVerify,
        attribution: Some(QueueActor::NzbdavQueueRules),
    };
    assert_eq!(compare(&o).interference, expected);
    o.external_failure = Some(QueueFailureEvidence {
        actor: None,
        ..evidence
    });
    assert_eq!(
        compare(&o).interference.attribution,
        Some(QueueActor::UnknownExternal)
    );
    o.external_failure = Some(QueueFailureEvidence {
        download: 9,
        ..evidence
    });
    assert_eq!(compare(&o).interference, QueueInterference::default());
    o.external_failure = Some(QueueFailureEvidence {
        local_audit: LocalMutationAudit::Incomplete,
        ..evidence
    });
    assert_eq!(compare(&o).interference, QueueInterference::default());
    // Route degradation is facts only: a missing exact-source test still holds.
    o.external_failure = Some(evidence);
    o.decoded_source = None;
    check(&o, Action::Hold, Reason::UnverifiedSource);
    o.owned_download = None;
    assert_eq!(compare(&o).interference, QueueInterference::default());
    o.owned_download = Some(9);
    assert_eq!(compare(&o).interference, QueueInterference::default());
}
