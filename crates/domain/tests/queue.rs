use pitcrew_domain::queue::*;

fn evidence() -> QueueFailureEvidence {
    QueueFailureEvidence {
        download: 17,
        completed_at: 30,
        failed_at: 40,
        removed: true,
        blocklisted: true,
        marked_failed_history: true,
        local_audit: LocalMutationAudit::NoMatchingMutation,
        actor: Some(QueueActor::NzbdavQueueRules),
    }
}

#[test]
fn authoritative_external_removal_degrades_holding_without_inventing_attribution() {
    let initial = QueueInterference::default();
    assert_eq!(initial.route, QueueImportRoute::HoldThenVerify);
    assert_eq!(initial.attribution, None);
    let degraded = initial.observe(17, evidence());
    assert_eq!(
        degraded.route,
        QueueImportRoute::ImportImmediatelyThenVerify
    );
    assert_eq!(degraded.attribution, Some(QueueActor::NzbdavQueueRules));
    let mut unknown = evidence();
    unknown.actor = None;
    assert_eq!(
        initial.observe(17, unknown).attribution,
        Some(QueueActor::UnknownExternal)
    );
    unknown.failed_at = unknown.completed_at;
    assert_eq!(initial.observe(17, unknown).route, degraded.route);
    // The actor can act much later, too: ten seconds is a reproduction, not a heuristic.
    unknown.failed_at = 500;
    assert_eq!(initial.observe(17, unknown).route, degraded.route);
    unknown.removed = false;
    assert_eq!(degraded.observe(17, unknown), degraded);
}

#[test]
fn queue_misses_own_deletes_uncertain_audits_and_other_downloads_do_not_attribute() {
    let initial = QueueInterference::default();
    let valid = evidence();
    let invalid = [
        QueueFailureEvidence {
            download: 18,
            ..valid
        },
        QueueFailureEvidence {
            failed_at: 29,
            ..valid
        },
        QueueFailureEvidence {
            removed: false,
            ..valid
        },
        QueueFailureEvidence {
            blocklisted: false,
            ..valid
        },
        QueueFailureEvidence {
            marked_failed_history: false,
            ..valid
        },
        QueueFailureEvidence {
            local_audit: LocalMutationAudit::MatchingMutation,
            ..valid
        },
        QueueFailureEvidence {
            local_audit: LocalMutationAudit::Incomplete,
            ..valid
        },
    ];
    for evidence in invalid {
        assert_eq!(initial.observe(17, evidence), initial);
    }
}
