use pitcrew_domain::{Constraints, Effect, Failure, FileIdentity};
use pitcrew_engine::observer::{
    Action, Activity, Actor, Agreement, Safety, Unsafe, compare, summary,
};

fn safe(action: Action, actor: Actor) -> Activity {
    Activity {
        action,
        actor,
        correlated: true,
        safety: Safety {
            access: Some(true),
            budget: Some(true),
            replacement_verified: Some(true),
            exact_import_decoded: Some(true),
            notification_unique: Some(true),
            no_auto_redownload: Some(true),
            reconciled: Some(true),
        },
    }
}

#[test]
fn effects_map_to_every_observable_action() {
    let file = FileIdentity {
        id: 10,
        revision: 1,
    };
    for (effect, expected) in [
        (
            Effect::Grab {
                release: 7,
                reservation: 1,
            },
            Action::Grab,
        ),
        (
            Effect::ImportVerifiedDownload {
                download: 3,
                file,
                original: file,
            },
            Action::Import,
        ),
        (
            Effect::RemoveRejectedDownload {
                download: 3,
                blocklist: true,
                skip_redownload: true,
            },
            Action::RemoveDownload,
        ),
        (Effect::DecodeDownload { download: 3, file }, Action::Decode),
        (Effect::DecodeImported { file }, Action::Decode),
        (
            Effect::CheckTarget {
                file,
                constraints: Constraints::default(),
            },
            Action::CheckTarget,
        ),
        (
            Effect::InstallUpgradeGuard { excluded: vec![] },
            Action::UpgradeGuard,
        ),
        (Effect::NotifyReady { file }, Action::Ready),
        (
            Effect::NotifyAdmin {
                failure: Failure::Upstream,
            },
            Action::AdminFailure,
        ),
        (
            Effect::NotifyViewerFailure {
                failure: Failure::Deadline,
            },
            Action::ViewerFailure,
        ),
        (Effect::ReconcileGrab { release: 7 }, Action::Reconcile),
        (Effect::ReconcileDelete { download: 3 }, Action::Reconcile),
        (Effect::ReconcileImport { download: 3 }, Action::Reconcile),
        (
            Effect::ReconcileUpgradeGuard { excluded: vec![] },
            Action::Reconcile,
        ),
    ] {
        assert_eq!(Action::from_effect(&effect), expected);
    }
}

#[test]
fn each_action_enforces_only_its_relevant_safety_facts() {
    for (action, expected) in [
        (
            Action::Grab,
            vec![Unsafe::Access, Unsafe::Budget, Unsafe::UnreconciledMutation],
        ),
        (
            Action::Import,
            vec![
                Unsafe::Access,
                Unsafe::UnverifiedReplacement,
                Unsafe::UnreconciledMutation,
            ],
        ),
        (
            Action::DeleteLibrary,
            vec![
                Unsafe::Access,
                Unsafe::UnverifiedReplacement,
                Unsafe::UnreconciledMutation,
            ],
        ),
        (
            Action::RemoveDownload,
            vec![
                Unsafe::Access,
                Unsafe::AutoRedownload,
                Unsafe::UnreconciledMutation,
            ],
        ),
        (
            Action::UpgradeGuard,
            vec![Unsafe::Access, Unsafe::UnreconciledMutation],
        ),
        (
            Action::Ready,
            vec![Unsafe::UnverifiedReady, Unsafe::DuplicateNotification],
        ),
        (Action::AdminFailure, vec![Unsafe::DuplicateNotification]),
        (Action::ViewerFailure, vec![Unsafe::DuplicateNotification]),
        (Action::Decode, vec![]),
        (Action::CheckTarget, vec![]),
        (Action::Reconcile, vec![]),
        (Action::Hold, vec![]),
    ] {
        let mut observed = safe(action, Actor::ExternalTool);
        observed.safety = Safety {
            access: Some(false),
            budget: Some(false),
            replacement_verified: Some(false),
            exact_import_decoded: Some(false),
            notification_unique: Some(false),
            no_auto_redownload: Some(false),
            reconciled: Some(false),
        };
        let report = compare(
            "fix-opaque".into(),
            observed.clone(),
            safe(action, Actor::Engine),
        );
        assert_eq!(report.version, 1);
        assert_eq!(report.fix, "fix-opaque");
        assert_eq!(report.observed, observed);
        assert_eq!(report.proposed, safe(action, Actor::Engine));
        assert_eq!(report.observed_unsafe, expected);
        assert!(report.proposed_unsafe.is_empty());
        assert!(!report.missing_evidence);
        assert_eq!(report.agreement, Agreement::Match);
        let reverse = compare(
            "fix-opaque".into(),
            safe(action, Actor::ExternalImporter),
            observed,
        );
        assert!(reverse.observed_unsafe.is_empty());
        assert_eq!(reverse.proposed_unsafe, expected);
    }
}

#[test]
fn missing_each_relevant_fact_is_insufficient_even_when_actions_match() {
    type Setter = fn(&mut Safety);
    let cases: &[(Action, Setter)] = &[
        (Action::Grab, |s| s.access = None),
        (Action::Grab, |s| s.budget = None),
        (Action::Grab, |s| s.reconciled = None),
        (Action::Import, |s| s.access = None),
        (Action::Import, |s| s.replacement_verified = None),
        (Action::Import, |s| s.reconciled = None),
        (Action::DeleteLibrary, |s| s.access = None),
        (Action::DeleteLibrary, |s| s.replacement_verified = None),
        (Action::DeleteLibrary, |s| s.reconciled = None),
        (Action::RemoveDownload, |s| s.access = None),
        (Action::RemoveDownload, |s| s.no_auto_redownload = None),
        (Action::RemoveDownload, |s| s.reconciled = None),
        (Action::UpgradeGuard, |s| s.access = None),
        (Action::UpgradeGuard, |s| s.reconciled = None),
        (Action::Ready, |s| s.exact_import_decoded = None),
        (Action::Ready, |s| s.notification_unique = None),
        (Action::AdminFailure, |s| s.notification_unique = None),
        (Action::ViewerFailure, |s| s.notification_unique = None),
    ];
    for (action, erase) in cases {
        let good = safe(*action, Actor::Engine);
        let mut unknown = safe(*action, Actor::ExternalTool);
        erase(&mut unknown.safety);
        for (observed, proposed) in [(unknown.clone(), good.clone()), (good, unknown)] {
            let report = compare("fix".into(), observed, proposed);
            assert!(report.missing_evidence, "{action:?}");
            assert_eq!(report.agreement, Agreement::InsufficientEvidence);
            assert!(report.observed_unsafe.is_empty());
            assert!(report.proposed_unsafe.is_empty());
        }
    }
}

#[test]
fn attribution_and_correlation_are_required_on_both_sides() {
    let known = safe(Action::Hold, Actor::Engine);
    let unknown_actor = safe(Action::Hold, Actor::Unknown);
    let mut uncorrelated = safe(Action::Hold, Actor::ExternalTool);
    uncorrelated.correlated = false;
    for unknown in [unknown_actor, uncorrelated] {
        for (observed, proposed) in [(unknown.clone(), known.clone()), (known.clone(), unknown)] {
            let report = compare("fix".into(), observed, proposed);
            assert!(report.missing_evidence);
            assert_eq!(report.agreement, Agreement::InsufficientEvidence);
        }
    }
    let different = compare(
        "fix".into(),
        safe(Action::Import, Actor::ExternalTool),
        safe(Action::Hold, Actor::Engine),
    );
    assert_eq!(different.agreement, Agreement::Different);
    assert!(!different.missing_evidence);
    let readonly = Activity {
        action: Action::Decode,
        actor: Actor::ExternalTool,
        correlated: true,
        safety: Safety::default(),
    };
    let report = compare(
        "fix".into(),
        readonly,
        safe(Action::CheckTarget, Actor::Engine),
    );
    assert_eq!(report.agreement, Agreement::Different);
    assert!(!report.missing_evidence);
}

#[test]
fn jsonl_report_roundtrips_and_summary_counts_each_category() {
    let matched = compare(
        "fix-1".into(),
        safe(Action::Grab, Actor::ExternalTool),
        safe(Action::Grab, Actor::Engine),
    );
    let mut unsafe_observed = safe(Action::Ready, Actor::ExternalTool);
    unsafe_observed.safety.exact_import_decoded = Some(false);
    let mut unsafe_proposed = safe(Action::Ready, Actor::Engine);
    unsafe_proposed.safety.notification_unique = Some(false);
    let different = compare(
        "fix-2".into(),
        unsafe_observed,
        safe(Action::Hold, Actor::Engine),
    );
    let unsafe_match = compare(
        "fix-3".into(),
        safe(Action::Ready, Actor::ExternalTool),
        unsafe_proposed,
    );
    let unknown = compare(
        "fix-4".into(),
        safe(Action::Hold, Actor::Unknown),
        safe(Action::Hold, Actor::Engine),
    );
    let another_difference = compare(
        "fix-5".into(),
        safe(Action::Import, Actor::ExternalTool),
        safe(Action::Hold, Actor::Engine),
    );
    let reports = vec![
        matched,
        different,
        unsafe_match,
        unknown,
        another_difference,
    ];
    assert_eq!(
        summary(&reports).lines().next().unwrap(),
        "5 comparisons: 2 matching actions, 1 incomplete; 1 unsafe observed, 1 unsafe proposed. Unknown evidence does not establish safe parity."
    );
    assert_eq!(
        summary(&[]).lines().next().unwrap(),
        "0 comparisons: 0 matching actions, 0 incomplete; 0 unsafe observed, 0 unsafe proposed. Unknown evidence does not establish safe parity."
    );
    let jsonl = reports
        .iter()
        .map(|r| serde_json::to_string(r).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    let parsed: Vec<pitcrew_engine::observer::Diff> = jsonl
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(parsed, reports);
    assert!(jsonl.contains("\"agreement\":\"insufficient_evidence\""));
    assert!(jsonl.contains("\"observed_unsafe\":[\"unverified_ready\"]"));
}

proptest::proptest! {
    #[test]
    fn absent_correlation_never_counts_as_parity(observed_correlated in proptest::bool::ANY, proposed_correlated in proptest::bool::ANY,
        observed_known in proptest::bool::ANY, proposed_known in proptest::bool::ANY) {
        let mut observed = safe(Action::Grab, if observed_known { Actor::ExternalTool } else { Actor::Unknown });
        let mut proposed = safe(Action::Grab, if proposed_known { Actor::Engine } else { Actor::Unknown });
        observed.correlated = observed_correlated;
        proposed.correlated = proposed_correlated;
        let diff = compare("fix".into(), observed, proposed);
        let complete = observed_correlated && proposed_correlated && observed_known && proposed_known;
        proptest::prop_assert_eq!(diff.missing_evidence, !complete);
        proptest::prop_assert_eq!(diff.agreement, if complete { Agreement::Match } else { Agreement::InsufficientEvidence });
    }
}

#[test]
fn library_deletion_is_a_distinct_action_requiring_a_verified_replacement() {
    let observed: Activity = serde_json::from_value(serde_json::json!({
        "action": "delete_library", "actor": "python", "correlated": true,
        "safety": { "access":true, "replacement_verified":false, "reconciled":true }
    }))
    .unwrap();
    let report = compare(
        "fix-delete".into(),
        observed,
        safe(Action::Hold, Actor::Engine),
    );
    assert_eq!(report.observed_unsafe, vec![Unsafe::UnverifiedReplacement]);
    assert_eq!(report.agreement, Agreement::Different);
    assert!(!report.missing_evidence);
}

#[test]
fn external_actors_roundtrip_without_assuming_a_specific_tool() {
    for name in ["external_tool", "external_importer"] {
        let actor: Actor = serde_json::from_str(&format!("\"{name}\"")).unwrap();
        assert_eq!(serde_json::to_value(actor).unwrap(), name);
        assert_ne!(actor, Actor::Unknown);
    }
    let previous: Actor = serde_json::from_str("\"python\"").unwrap();
    assert_eq!(previous, Actor::Legacy);
    assert_eq!(serde_json::to_value(previous).unwrap(), "python");
    let unknown: Actor = serde_json::from_str("\"unrecognized_external_tool\"").unwrap();
    assert_eq!(unknown, Actor::Unknown);
    let diff = compare(
        "unknown".into(),
        safe(Action::Grab, unknown),
        safe(Action::Grab, Actor::Engine),
    );
    assert_eq!(diff.agreement, Agreement::InsufficientEvidence);
    assert!(diff.missing_evidence);
}

#[test]
fn summary_separates_attributed_actors_from_unattributed_traffic() {
    let mut reports = vec![];
    for actor in [
        Actor::Engine,
        Actor::ExternalTool,
        Actor::ExternalImporter,
        Actor::Legacy,
        Actor::Unknown,
    ] {
        reports.push(compare(
            "private-correlation".into(),
            safe(Action::Grab, actor),
            safe(Action::Hold, Actor::Engine),
        ));
    }
    let mut spoofed = safe(Action::Grab, Actor::Engine);
    spoofed.correlated = false;
    reports.push(compare(
        "uncorrelated".into(),
        spoofed,
        safe(Action::Hold, Actor::Engine),
    ));
    let text = summary(&reports);
    assert!(
        text.contains("4 attributed actions; 2 unattributed"),
        "{text}"
    );
    assert!(
        text.contains("engine=1, external_tool=1, external_importer=1, python=1, unknown=2"),
        "{text}"
    );
    assert!(text.contains("observed Grab -> proposed Hold: 1"), "{text}");
    assert!(!text.contains("private-correlation"));
}

#[test]
fn download_join_is_exact_scoped_and_bounded() {
    use pitcrew_engine::observer::download_correlation as key;
    assert_eq!(key("movies", "download"), key("movies", "download"));
    assert_ne!(key("movies", "download"), key("series", "download"));
    assert_ne!(key("movies", "download"), key("movies", "Download"));
    assert_ne!(key("ab", "c"), key("a", "bc"));
    assert_eq!(key("movies", "download").unwrap().len(), 64);
    for bad in [
        "".into(),
        " ".into(),
        "a\nb".into(),
        "é".into(),
        "a".repeat(257),
    ] {
        assert!(key(&bad, "download").is_none());
        assert!(key("movies", &bad).is_none());
    }
    assert!(key(&"a".repeat(256), &"b".repeat(256)).is_some());
}

#[tokio::test]
async fn dispatch_receipt_requires_the_exact_dispatched_intent_and_is_immutable() {
    use pitcrew_domain::{Access, Event, IntentKey, Policy, StageTimeouts};
    use pitcrew_engine::observer::{DispatchReceipt, record_dispatch};
    use pitcrew_store::{BudgetLimits, NewJob, OutboxStatus, Store};
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("db")).await.unwrap();
    store
        .create_job(
            &NewJob {
                id: 1,
                viewer: "viewer".into(),
                resource: "media".into(),
                library: 2,
                original: FileIdentity { id: 3, revision: 1 },
                constraints: Default::default(),
                policy: Policy {
                    max_grabs: 2,
                    stage_timeout: 30,
                    total_timeout: 300,
                },
                timeouts: StageTimeouts {
                    download: 30,
                    import: 30,
                    decode: 30,
                    verify_retry: 30,
                },
                created_at: 0,
            },
            "job",
            BudgetLimits {
                global_hour: 10,
                global_day: 20,
                viewer_hour: 10,
                viewer_day: 20,
            },
        )
        .await
        .unwrap();
    let lease = store.acquire_lease(1, "test", 0, 100).await.unwrap();
    let applied = store
        .apply_event(
            &lease,
            0,
            "select",
            Event::SelectRelease {
                release: 4,
                access: Access::Granted {
                    library: 2,
                    expires_at: 100,
                },
            },
            1,
        )
        .await
        .unwrap();
    let intent = applied.intents[0].clone();
    assert!(
        record_dispatch(&store, &intent, "movies", "download", 2)
            .await
            .is_err()
    );
    let key = IntentKey {
        job: 1,
        sequence: intent.sequence,
    };
    store.start_intent(&lease, key, 1, 2).await.unwrap();
    let mut forged = intent.clone();
    forged.job = 2;
    assert!(
        record_dispatch(&store, &forged, "movies", "download", 2)
            .await
            .is_err()
    );
    forged = intent.clone();
    forged.sequence += 1;
    assert!(
        record_dispatch(&store, &forged, "movies", "download", 2)
            .await
            .is_err()
    );
    forged = intent.clone();
    forged.effect = Effect::CheckTarget {
        file: FileIdentity { id: 3, revision: 1 },
        constraints: Default::default(),
    };
    assert!(
        record_dispatch(&store, &forged, "movies", "download", 2)
            .await
            .is_err()
    );
    assert!(
        record_dispatch(&store, &intent, "", "download", 2)
            .await
            .is_err()
    );
    assert!(
        record_dispatch(&store, &intent, "movies", "download", 2)
            .await
            .unwrap()
    );
    assert!(
        !record_dispatch(&store, &intent, "movies", "download", 3)
            .await
            .unwrap()
    );
    assert!(
        record_dispatch(&store, &intent, "series", "download", 3)
            .await
            .is_err()
    );
    store
        .finish_intent(&lease, key, OutboxStatus::Uncertain, 3)
        .await
        .unwrap();
    assert!(
        !record_dispatch(&store, &intent, "movies", "download", 3)
            .await
            .unwrap()
    );
    store
        .finish_intent(&lease, key, OutboxStatus::Completed, 4)
        .await
        .unwrap();
    assert!(
        !record_dispatch(&store, &intent, "movies", "download", 4)
            .await
            .unwrap()
    );
    let records = store.observations().await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].source, "engine_dispatch");
    assert_eq!(records[0].key, format!("1:{}", intent.sequence));
    assert_eq!(records[0].at, 2);
    let receipt: DispatchReceipt = serde_json::from_value(records[0].payload.clone()).unwrap();
    assert_eq!(receipt.intent, intent);
    assert_eq!(
        receipt.correlation,
        pitcrew_engine::observer::download_correlation("movies", "download").unwrap()
    );
}
