use pitcrew_deciders::{
    DeciderKind, DecisionReport, Usage,
    shadow::{Comparison, ComparisonEntry},
};
use pitcrew_diagnosis::{
    facts::{DecodeOutcome, EvidenceFacts},
    rules::classify,
};
use pitcrew_domain::FileIdentity;
use pitcrew_intake_store::{
    Error, Store,
    diagnoses::{Diagnosis, MAX_DIAGNOSIS_PAGE},
};
use serde_json::json;

async fn setup() -> (tempfile::TempDir, Store) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("journal.sqlite"))
        .await
        .unwrap();
    (directory, store)
}

async fn report(store: &Store, sequence: u64) -> i64 {
    store
        .persist_intake(
            "test",
            &format!("message-{sequence}"),
            "viewer-a",
            "conversation-a",
            &json!({"report":sequence}),
            10,
        )
        .await
        .unwrap();
    store
        .stored_intake("test", &format!("message-{sequence}"), "viewer-a")
        .await
        .unwrap()
        .unwrap()
        .id
}

fn diagnosis(report_id: i64) -> Diagnosis {
    let mut evidence = EvidenceFacts::default();
    evidence.playback.decode = DecodeOutcome::Bad;
    Diagnosis {
        report_id,
        decided_at: 20,
        decider_kind: DeciderKind::Rules,
        decision: DecisionReport {
            decision: classify(&evidence),
            usage: Usage::default(),
        },
        evidence_summary: evidence,
        resource: "arr:series-hd:episode:41".into(),
        library_id: "library-tv".into(),
        arr_instance_id: "series-hd".into(),
        arr_item_id: 41,
        arr_file_id: 51,
        agent_file: Some(FileIdentity {
            id: 61,
            revision: 71,
        }),
        agent_observed_at: Some(19),
        access_observed_at: 15,
        access_expires_at: 25,
    }
}

fn comparison(diagnosis: &Diagnosis) -> Comparison {
    Comparison {
        primary: diagnosis.decider_kind,
        used_rules_fallback: false,
        selected: diagnosis.decision.clone(),
        entries: vec![ComparisonEntry {
            provider: diagnosis.decider_kind,
            result: Some(diagnosis.decision.clone()),
            failed: false,
            accounting: None,
        }],
    }
}

#[tokio::test]
async fn immutable_handoff_survives_reopen_and_distinguishes_exact_retries() {
    let (dir, store) = setup().await;
    let first = report(&store, 1).await;
    let original = diagnosis(first);
    assert_eq!(store.diagnosis(first).await.unwrap(), None);
    assert!(store.persist_diagnosis(&original).await.unwrap());
    assert!(!store.persist_diagnosis(&original).await.unwrap());
    let reopened = Store::open(dir.path().join("journal.sqlite"))
        .await
        .unwrap();
    assert_eq!(
        reopened.diagnosis(first).await.unwrap(),
        Some(original.clone())
    );
    assert!(reopened.pending_diagnoses(0, 100).await.unwrap().is_empty());
    let pending = reopened.report_for_diagnosis(first).await.unwrap().unwrap();
    assert_eq!(pending.account_id, "viewer-a");
    assert_eq!(pending.intake.response, json!({"report":1}));
    assert_eq!(pending.intake.message_id, "message-1");
    assert_eq!(pending.intake.created_at, 10);
    assert_eq!(
        reopened.report_for_diagnosis(first + 1).await.unwrap(),
        None
    );
    let mut changed = original.clone();
    changed.decided_at += 1;
    assert!(matches!(
        reopened.persist_diagnosis(&changed).await,
        Err(Error::Invalid)
    ));
    assert_eq!(store.diagnosis(first).await.unwrap(), Some(original));
    assert_eq!(store.diagnosis(first + 1).await.unwrap(), None);
    assert!(matches!(
        store.persist_diagnosis(&diagnosis(first + 1)).await,
        Err(Error::Invalid)
    ));
}

#[tokio::test]
async fn concurrent_connections_choose_one_complete_record() {
    let (dir, store) = setup().await;
    let original = diagnosis(report(&store, 1).await);
    let second = Store::open(dir.path().join("journal.sqlite"))
        .await
        .unwrap();
    let (a, b) = tokio::join!(
        store.persist_diagnosis(&original),
        second.persist_diagnosis(&original)
    );
    assert_ne!(a.unwrap(), b.unwrap());
    let mut changed = original.clone();
    changed.arr_file_id += 1;
    let (a, b) = tokio::join!(
        store.persist_diagnosis(&original),
        second.persist_diagnosis(&changed)
    );
    assert!(!a.unwrap());
    assert!(matches!(b, Err(Error::Invalid)));
    assert_eq!(
        store.diagnosis(original.report_id).await.unwrap(),
        Some(original)
    );
}

#[tokio::test]
async fn failed_comparison_write_rolls_back_the_selected_diagnosis() {
    let (dir, store) = setup().await;
    let original = diagnosis(report(&store, 1).await);
    let options =
        sqlx::sqlite::SqliteConnectOptions::new().filename(dir.path().join("journal.sqlite"));
    let direct = sqlx::SqlitePool::connect_with(options).await.unwrap();
    sqlx::raw_sql("CREATE TRIGGER unavailable_comparison BEFORE INSERT ON diagnosis_comparisons BEGIN SELECT RAISE(ABORT, 'injected local failure'); END;")
        .execute(&direct).await.unwrap();
    assert!(matches!(
        store
            .persist_diagnosis_with_comparison(&original, &comparison(&original))
            .await,
        Err(Error::Database(_))
    ));
    assert_eq!(store.diagnosis(original.report_id).await.unwrap(), None);
    assert_eq!(
        store
            .diagnosis_comparison(original.report_id)
            .await
            .unwrap(),
        None
    );
    assert_eq!(store.pending_diagnoses(0, 1).await.unwrap().len(), 1);
    sqlx::query("DROP TRIGGER unavailable_comparison")
        .execute(&direct)
        .await
        .unwrap();
    assert!(
        store
            .persist_diagnosis_with_comparison(&original, &comparison(&original))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn comparison_is_atomic_immutable_and_bound_to_selected_decision() {
    let (dir, store) = setup().await;
    let original = diagnosis(report(&store, 1).await);
    let compare = comparison(&original);
    assert_eq!(
        store
            .diagnosis_comparison(original.report_id)
            .await
            .unwrap(),
        None
    );
    assert!(
        store
            .persist_diagnosis_with_comparison(&original, &compare)
            .await
            .unwrap()
    );
    assert!(
        !store
            .persist_diagnosis_with_comparison(&original, &compare)
            .await
            .unwrap()
    );
    let reopened = Store::open(dir.path().join("journal.sqlite"))
        .await
        .unwrap();
    assert_eq!(
        reopened
            .diagnosis_comparison(original.report_id)
            .await
            .unwrap(),
        Some(serde_json::to_value(&compare).unwrap())
    );
    let mut changed = compare.clone();
    changed.entries.clear();
    assert!(matches!(
        store
            .persist_diagnosis_with_comparison(&original, &changed)
            .await,
        Err(Error::Invalid)
    ));
    assert_eq!(
        store
            .diagnosis_comparison(original.report_id)
            .await
            .unwrap(),
        Some(serde_json::to_value(&compare).unwrap())
    );

    let next = diagnosis(report(&store, 2).await);
    let mut different = comparison(&next);
    different.selected.usage.input_tokens = 1;
    assert!(matches!(
        store
            .persist_diagnosis_with_comparison(&next, &different)
            .await,
        Err(Error::Invalid)
    ));
    assert_eq!(store.diagnosis(next.report_id).await.unwrap(), None);
    assert_eq!(
        store.diagnosis_comparison(next.report_id).await.unwrap(),
        None
    );
    different = comparison(&next);
    different.primary = DeciderKind::Jev;
    assert!(matches!(
        store
            .persist_diagnosis_with_comparison(&next, &different)
            .await,
        Err(Error::Invalid)
    ));
    different.used_rules_fallback = true;
    assert!(
        store
            .persist_diagnosis_with_comparison(&next, &different)
            .await
            .unwrap()
    );
    assert_eq!(
        store.diagnosis_comparison(next.report_id).await.unwrap(),
        Some(serde_json::to_value(&different).unwrap())
    );

    let mut last = diagnosis(report(&store, 3).await);
    last.decider_kind = DeciderKind::Anthropic;
    assert!(store.persist_diagnosis(&last).await.unwrap());
    assert!(
        !store
            .persist_diagnosis_with_comparison(&last, &comparison(&last))
            .await
            .unwrap()
    );
    assert_eq!(
        store.diagnosis(last.report_id).await.unwrap(),
        Some(last.clone())
    );
    assert!(
        store
            .diagnosis_comparison(last.report_id)
            .await
            .unwrap()
            .is_some()
    );
    let mut jev = diagnosis(report(&store, 4).await);
    jev.decider_kind = DeciderKind::Jev;
    assert!(
        store
            .persist_diagnosis_with_comparison(&jev, &comparison(&jev))
            .await
            .unwrap()
    );
    assert_eq!(store.diagnosis(jev.report_id).await.unwrap(), Some(jev));
}

#[tokio::test]
async fn cursors_are_ordered_bounded_and_skip_completed_reports() {
    let (_dir, store) = setup().await;
    for sequence in 1..=u64::from(MAX_DIAGNOSIS_PAGE) + 2 {
        report(&store, sequence).await;
    }
    assert!(store.pending_diagnoses(0, 0).await.unwrap().is_empty());
    assert_eq!(
        store.pending_diagnoses(0, u32::MAX).await.unwrap().len(),
        MAX_DIAGNOSIS_PAGE as usize
    );
    let first_two = store.pending_diagnoses(0, 2).await.unwrap();
    assert_eq!(
        first_two.iter().map(|p| p.intake.id).collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert!(first_two.iter().all(|p| p.account_id == "viewer-a"));
    assert_eq!(
        store
            .pending_diagnoses(100, 2)
            .await
            .unwrap()
            .iter()
            .map(|p| p.intake.id)
            .collect::<Vec<_>>(),
        vec![101, 102]
    );
    for id in 1..=102 {
        assert!(store.persist_diagnosis(&diagnosis(id)).await.unwrap());
    }
    assert!(
        store
            .pending_diagnoses(0, u32::MAX)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(store.diagnoses(0, 0).await.unwrap().is_empty());
    assert_eq!(
        store.diagnoses(0, u32::MAX).await.unwrap().len(),
        MAX_DIAGNOSIS_PAGE as usize
    );
    assert_eq!(
        store.diagnoses(100, 2).await.unwrap(),
        vec![diagnosis(101), diagnosis(102)]
    );
    assert_eq!(
        store.diagnoses(0, 2).await.unwrap(),
        vec![diagnosis(1), diagnosis(2)]
    );
    assert!(store.diagnoses(102, 2).await.unwrap().is_empty());
    assert!(store.runtime_reports(0, 0).await.unwrap().is_empty());
    assert_eq!(
        store.runtime_reports(0, u32::MAX).await.unwrap().len(),
        MAX_DIAGNOSIS_PAGE as usize
    );
    let recovered = store.runtime_reports(100, 2).await.unwrap();
    assert_eq!(
        recovered.iter().map(|p| p.intake.id).collect::<Vec<_>>(),
        vec![101, 102]
    );
    assert!(recovered.iter().all(|p| p.account_id == "viewer-a"));
    assert!(store.runtime_reports(102, 2).await.unwrap().is_empty());
}

#[tokio::test]
async fn invalid_identity_access_and_time_never_create_a_diagnosis() {
    let (_dir, store) = setup().await;
    let original = diagnosis(report(&store, 1).await);
    let changes: &[fn(&mut Diagnosis)] = &[
        |d| d.report_id = 0,
        |d| d.report_id = -1,
        |d| d.arr_item_id = 0,
        |d| d.arr_file_id = 0,
        |d| d.resource.clear(),
        |d| d.resource = "   ".into(),
        |d| d.library_id.clear(),
        |d| d.library_id = "   ".into(),
        |d| d.arr_instance_id.clear(),
        |d| d.arr_instance_id = "   ".into(),
        |d| d.access_observed_at = d.decided_at + 1,
        |d| d.access_expires_at = d.decided_at,
        |d| d.access_expires_at = d.decided_at - 1,
        |d| d.agent_file = None,
        |d| d.agent_observed_at = None,
        |d| d.agent_file.as_mut().unwrap().id = 0,
        |d| d.agent_observed_at = Some(d.decided_at + 1),
        |d| d.arr_item_id = u64::MAX,
        |d| d.arr_file_id = u64::MAX,
        |d| d.access_expires_at = u64::MAX,
        |d| {
            d.decided_at = u64::MAX - 1;
            d.access_expires_at = u64::MAX
        },
        |d| {
            d.decided_at = 9;
            d.access_observed_at = 8;
            d.agent_observed_at = Some(8)
        },
    ];
    for change in changes {
        let mut invalid = original.clone();
        change(&mut invalid);
        assert!(
            matches!(store.persist_diagnosis(&invalid).await, Err(Error::Invalid)),
            "{invalid:?}"
        );
        assert_eq!(store.diagnosis(original.report_id).await.unwrap(), None);
    }
    let mut boundary = original.clone();
    boundary.decided_at = 10;
    boundary.access_observed_at = 10;
    boundary.access_expires_at = 11;
    boundary.agent_observed_at = Some(10);
    boundary.agent_file = Some(FileIdentity { id: 1, revision: 0 });
    boundary.arr_item_id = 1;
    boundary.arr_file_id = 1;
    assert!(store.persist_diagnosis(&boundary).await.unwrap());
    assert_eq!(
        store.diagnosis(boundary.report_id).await.unwrap(),
        Some(boundary)
    );
    let mut unavailable = diagnosis(report(&store, 2).await);
    unavailable.agent_file = None;
    unavailable.agent_observed_at = None;
    assert!(store.persist_diagnosis(&unavailable).await.unwrap());
    assert_eq!(
        store.diagnosis(unavailable.report_id).await.unwrap(),
        Some(unavailable)
    );
}

#[tokio::test]
async fn opening_existing_intake_database_adds_schema_without_changing_reports() {
    let (dir, store) = setup().await;
    let id = report(&store, 1).await;
    let before = store.reports("viewer-a").await.unwrap();
    // Recreate the pre-seam schema, retaining the journal and companion key.
    let options =
        sqlx::sqlite::SqliteConnectOptions::new().filename(dir.path().join("journal.sqlite"));
    let direct = sqlx::SqlitePool::connect_with(options).await.unwrap();
    sqlx::raw_sql("DROP TABLE diagnosis_comparisons; DROP TABLE diagnoses;")
        .execute(&direct)
        .await
        .unwrap();
    let reopened = Store::open(dir.path().join("journal.sqlite"))
        .await
        .unwrap();
    assert_eq!(reopened.reports("viewer-a").await.unwrap(), before);
    assert!(reopened.persist_diagnosis(&diagnosis(id)).await.unwrap());
    assert_eq!(store.diagnosis(id).await.unwrap(), Some(diagnosis(id)));
}

#[tokio::test]
async fn malformed_stored_facts_are_rejected_without_coercing_identity_or_time() {
    let (dir, store) = setup().await;
    let original = diagnosis(report(&store, 1).await);
    assert!(store.persist_diagnosis(&original).await.unwrap());
    let options =
        sqlx::sqlite::SqliteConnectOptions::new().filename(dir.path().join("journal.sqlite"));
    let direct = sqlx::SqlitePool::connect_with(options).await.unwrap();
    for (column, invalid, restore) in [
        ("decided_at", "-1".to_string(), "20".to_string()),
        ("arr_item_id", "-1".into(), "41".into()),
        (
            "agent_file",
            "malformed".into(),
            serde_json::to_string(&original.agent_file.unwrap()).unwrap(),
        ),
        ("agent_observed_at", "-1".into(), "19".into()),
        ("decider_kind", "invalid".into(), "rules".into()),
        (
            "decision",
            "{\"raw_text\":\"forbidden\"}".into(),
            serde_json::to_string(&original.decision).unwrap(),
        ),
        (
            "evidence_summary",
            "{\"raw_text\":\"forbidden\"}".into(),
            serde_json::to_string(&original.evidence_summary).unwrap(),
        ),
    ] {
        let statement = format!("UPDATE diagnoses SET {column}=? WHERE report_id=?");
        sqlx::query(&statement)
            .bind(&invalid)
            .bind(original.report_id)
            .execute(&direct)
            .await
            .unwrap();
        assert!(matches!(
            store.diagnosis(original.report_id).await,
            Err(Error::Invalid)
        ));
        assert!(matches!(store.diagnoses(0, 1).await, Err(Error::Invalid)));
        sqlx::query(&statement)
            .bind(&restore)
            .bind(original.report_id)
            .execute(&direct)
            .await
            .unwrap();
    }
    assert_eq!(
        store.diagnosis(original.report_id).await.unwrap(),
        Some(original)
    );
}

#[tokio::test]
async fn model_attempts_survive_reopen_and_never_replay_or_change_accounting() {
    let (dir, store) = setup().await;
    let id = report(&store, 1).await;
    assert_eq!(store.model_attempt(id).await.unwrap(), None);
    assert!(!store.begin_model_attempt(id, 9).await.unwrap());
    assert!(!store.begin_model_attempt(id + 1, 10).await.unwrap());
    assert!(store.begin_model_attempt(id, 10).await.unwrap());
    assert!(!store.begin_model_attempt(id, 11).await.unwrap());
    let reopened = Store::open(dir.path().join("journal.sqlite"))
        .await
        .unwrap();
    let pending = reopened.model_attempt(id).await.unwrap().unwrap();
    assert_eq!(pending.started_at, 10);
    assert_eq!(pending.comparison, None);
    assert!(!reopened.begin_model_attempt(id, 12).await.unwrap());
    let expected = comparison(&diagnosis(id));
    reopened.finish_model_attempt(id, &expected).await.unwrap();
    reopened.finish_model_attempt(id, &expected).await.unwrap();
    assert_eq!(
        store.model_attempt(id).await.unwrap().unwrap().comparison,
        Some(serde_json::to_value(&expected).unwrap())
    );
    let mut changed = expected.clone();
    changed.used_rules_fallback = true;
    assert!(matches!(
        store.finish_model_attempt(id, &changed).await,
        Err(Error::Invalid)
    ));
    assert!(matches!(
        store.finish_model_attempt(id + 1, &expected).await,
        Err(Error::Invalid)
    ));
    assert!(store.begin_model_attempt(id, u64::MAX).await.is_err());
    assert!(store.diagnosis(id).await.unwrap().is_none());
}
