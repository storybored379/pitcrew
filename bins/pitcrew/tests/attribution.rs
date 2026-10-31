use pitcrew::{App, python};
use pitcrew_engine::{
    observer::{Action, Actor},
    webhook::ArrObservation,
};
use pitcrew_store::{BudgetLimits, Store};
use serde_json::json;
use sqlx::{Connection, Executor, SqliteConnection, sqlite::SqliteConnectOptions};
use std::collections::BTreeMap;

async fn app(path: &std::path::Path) -> App {
    App::new(
        Store::open(path).await.unwrap(),
        vec![],
        None,
        BudgetLimits {
            global_hour: 10,
            global_day: 20,
            viewer_hour: 10,
            viewer_day: 20,
        },
        2,
        BTreeMap::new(),
    )
    .unwrap()
}

#[tokio::test]
async fn imported_dispatch_ids_attribute_history_without_report_or_timing_inference() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.sqlite");
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&source)
            .create_if_missing(true),
    )
    .await
    .unwrap();
    db.execute(include_str!("fixtures/python.sql"))
        .await
        .unwrap();
    db.execute("ALTER TABLE fix_attempts ADD COLUMN download_id TEXT; UPDATE fix_attempts SET instance='movies', download_id='synthetic-download-a' WHERE id=2;").await.unwrap();
    db.close().await.unwrap();
    let service = app(&dir.path().join("engine.sqlite")).await;
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../crates/arr/tests/fixtures/radarr-6.3.0.10514.json"
    ))
    .unwrap();
    let history: pitcrew_arr::HistoryRecord =
        serde_json::from_value(fixture["history"]["records"][0].clone()).unwrap();
    service
        .store
        .ingest_observation(
            "history:movies",
            "401",
            &serde_json::to_value(ArrObservation {
                kind: pitcrew_engine::webhook::Kind::Grab,
                download: history.download_id,
                media: vec![101],
                files: vec![],
            })
            .unwrap(),
            2000,
        )
        .await
        .unwrap();
    service.project(0).await.unwrap();
    assert_eq!(
        service.reports().await.unwrap()[0].observed.actor,
        Actor::Unknown
    );
    service
        .import_legacy(python::read(&source, "external").await.unwrap(), 3000)
        .await
        .unwrap();
    let reports = service.reports().await.unwrap();
    let report = reports
        .iter()
        .find(|r| r.observed.action == Action::Grab && r.fix.starts_with("observation:"))
        .unwrap_or(&reports[0]);
    assert_eq!(report.observed.actor, Actor::ExternalTool);
    assert!(report.observed.correlated);
    assert!(report.missing_evidence);
    assert_eq!(report.proposed.action, Action::Hold);
    assert_eq!(report.observed.safety.access, None);
    assert!(service.store.jobs().await.unwrap().is_empty());
    // Same media/title/time, different download: ordinary arr traffic stays unknown.
    service
        .store
        .ingest_observation(
            "history:movies",
            "402",
            &json!({"kind":"Grab", "download":"other-download", "media":[101], "files":[]}),
            2000,
        )
        .await
        .unwrap();
    service.project(0).await.unwrap();
    assert!(
        service
            .reports()
            .await
            .unwrap()
            .iter()
            .any(|r| r.observed.actor == Actor::Unknown)
    );
}

async fn action_log(path: &std::path::Path, extra: &str) {
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true),
    )
    .await
    .unwrap();
    db.execute(include_str!("fixtures/python.sql"))
        .await
        .unwrap();
    db.execute("CREATE TABLE arr_actions(id INTEGER PRIMARY KEY,instance TEXT,download_id TEXT,actor TEXT,action TEXT);
        INSERT INTO arr_actions VALUES(1,'movies','download-one','external_tool','grab'),(2,'movies','download-one','external_importer','import');").await.unwrap();
    db.execute(extra).await.unwrap();
    db.close().await.unwrap();
}

async fn history(app: &App, id: &str, instance: &str, kind: &str, download: &str) {
    app.store.ingest_observation(&format!("history:{instance}"), id,
        &json!({"kind":kind,"download":download,"media":[101],"files":[],"actor":"engine","correlated":true}), 20).await.unwrap();
}

#[tokio::test]
async fn external_action_logs_are_scoped_per_action_and_survive_restart_and_late_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    action_log(&source, "SELECT 1").await;
    let destination = dir.path().join("engine");
    let service = app(&destination).await;
    assert_eq!(
        service
            .import_legacy(python::read(&source, "tool").await.unwrap(), 10)
            .await
            .unwrap(),
        6
    );
    for (id, instance, kind, download) in [
        ("1", "movies", "Grab", "download-one"),
        ("2", "movies", "Download", "download-one"),
        ("3", "series", "Grab", "download-one"),
        ("4", "movies", "Grab", "Download-one"),
        ("5", "movies", "MovieFileDelete", "download-one"),
        ("6", "movies", "DownloadFailed", "download-one"),
    ] {
        history(&service, id, instance, kind, download).await;
    }
    service.project(0).await.unwrap();
    let records = service.reports().await.unwrap();
    let raw = records
        .iter()
        .filter(|r| r.fix.starts_with("observation:"))
        .collect::<Vec<_>>();
    assert_eq!(raw.len(), 6);
    assert_eq!(
        raw.iter()
            .filter(|r| r.observed.actor == Actor::ExternalTool)
            .count(),
        1
    );
    assert_eq!(
        raw.iter()
            .filter(|r| r.observed.actor == Actor::ExternalImporter)
            .count(),
        1
    );
    assert_eq!(
        raw.iter()
            .filter(|r| r.observed.actor == Actor::Unknown)
            .count(),
        4
    );
    assert!(
        raw.iter()
            .all(|r| r.missing_evidence && r.proposed.action == Action::Hold)
    );
    assert!(
        !serde_json::to_string(&service.store.observations().await.unwrap())
            .unwrap()
            .contains("PRIVATE_")
    );
    let restarted = app(&destination).await;
    assert_eq!(
        restarted
            .import_legacy(python::read(&source, "tool").await.unwrap(), 11)
            .await
            .unwrap(),
        0
    );
    restarted.project(0).await.unwrap();
    assert_eq!(restarted.reports().await.unwrap(), records);
    // Conflicting independent tool claim arrives after the initial projection.
    let second = dir.path().join("second");
    action_log(
        &second,
        "UPDATE arr_actions SET actor='external_importer' WHERE id=1;",
    )
    .await;
    restarted
        .import_legacy(python::read(&second, "different-tool").await.unwrap(), 30)
        .await
        .unwrap();
    restarted.project(0).await.unwrap();
    assert!(
        restarted
            .reports()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.fix.starts_with("observation:"))
            .all(|r| r.observed.actor == Actor::Unknown)
    );
    // Re-reading the first claim cannot erase a conflict.
    restarted
        .import_legacy(python::read(&source, "tool").await.unwrap(), 31)
        .await
        .unwrap();
    assert!(
        restarted
            .reports()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.fix.starts_with("observation:"))
            .all(|r| !r.observed.correlated)
    );
}

#[tokio::test]
async fn queue_download_owner_never_supplies_import_actor_or_safe_import_facts() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    action_log(&source, "DELETE FROM arr_actions WHERE id=2").await;
    let service = app(&dir.path().join("engine")).await;
    service
        .import_legacy(python::read(&source, "log").await.unwrap(), 10)
        .await
        .unwrap();
    let index = pitcrew::attribution::Index::load(&service.store)
        .await
        .unwrap();
    let mut queue = json!({"download_correlation":pitcrew_engine::observer::download_correlation("movies","download-one"),"decision":{"action":"Hold","reason":"AccessUnknown"},"legacy_action":"Unknown"});
    index.queue(&mut queue);
    assert_eq!(queue["download_actor"], "external_tool");
    assert_eq!(queue["import_actor"], "unknown");
    assert_eq!(queue["observed_import"], false);
    assert_eq!(queue["decision"]["reason"], "AccessUnknown");
    assert_eq!(queue["legacy_action"], "Unknown");
    let mut old = json!({"decision":"Hold"});
    index.queue(&mut old);
    assert_eq!(old["download_actor"], "unknown");
}

#[test]
fn queue_projection_preserves_nonobject_journal_payloads() {
    let index = pitcrew::attribution::Index::default();
    for original in [
        json!(null),
        json!([]),
        json!("legacy"),
        json!(7),
        json!(true),
    ] {
        let mut payload = original.clone();
        index.queue(&mut payload);
        assert_eq!(payload, original);
    }
}

#[tokio::test]
async fn arbiter_import_comparison_tracks_late_receipts_and_conflicts_without_changing_safety() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    action_log(&source, "SELECT 1").await;
    let service = app(&dir.path().join("engine")).await;
    let mut queue = json!({"download_correlation":pitcrew_engine::observer::download_correlation("movies","download-one"),"decision":{"action":"Hold","reason":"AccessUnknown"},"legacy_action":"Unknown"});
    let unknown = pitcrew::attribution::Index::load(&service.store)
        .await
        .unwrap();
    unknown.queue(&mut queue);
    assert_eq!(queue["divergence"], "UnknownLegacyAction");
    service
        .import_legacy(python::read(&source, "log").await.unwrap(), 10)
        .await
        .unwrap();
    let known = pitcrew::attribution::Index::load(&service.store)
        .await
        .unwrap();
    known.queue(&mut queue);
    assert_eq!(queue["download_actor"], "external_tool");
    assert_eq!(queue["import_actor"], "external_importer");
    assert_eq!(queue["observed_import"], true);
    assert_eq!(queue["legacy_action"], "Import");
    assert_eq!(queue["divergence"], "ImportWithoutSafeProposal");
    assert_eq!(queue["decision"]["reason"], "AccessUnknown");
    service
        .import_legacy(python::read(&source, "second-log").await.unwrap(), 11)
        .await
        .unwrap();
    let conflict = pitcrew::attribution::Index::load(&service.store)
        .await
        .unwrap();
    conflict.queue(&mut queue);
    assert_eq!(queue["download_actor"], "unknown");
    assert_eq!(queue["import_actor"], "unknown");
    assert_eq!(queue["legacy_action"], "Unknown");
    assert_eq!(queue["divergence"], "UnknownLegacyAction");
    let mut recorded =
        json!({"legacy_action":"Import","decision":{"action":"Hold","reason":"AccessUnknown"}});
    conflict.queue(&mut recorded);
    assert_eq!(recorded["legacy_action"], "Import");
}

#[tokio::test]
async fn export_lists_actor_counts_and_exact_queue_import_comparisons_offline() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    action_log(&source, "SELECT 1").await;
    let database = dir.path().join("engine");
    let service = app(&database).await;
    service
        .import_legacy(python::read(&source, "log").await.unwrap(), 10)
        .await
        .unwrap();
    history(&service, "1", "movies", "Grab", "download-one").await;
    history(&service, "2", "movies", "Download", "download-one").await;
    let correlation = pitcrew_engine::observer::download_correlation("movies", "download-one");
    service.store.ingest_observation("arbiter:movies","1",&json!({"download_correlation":correlation,"decision":{"action":"Hold","reason":"AccessUnknown"}}),20).await.unwrap();
    let malformed = [
        json!(null),
        json!([]),
        json!("legacy"),
        json!(7),
        json!(true),
    ];
    for (index, payload) in malformed.iter().enumerate() {
        service
            .store
            .ingest_observation("arbiter:legacy", &index.to_string(), payload, 20)
            .await
            .unwrap();
    }
    let config = dir.path().join("config.toml");
    std::fs::write(&config,format!("version=1\ndatabase={database:?}\n[engine]\nlisten=\"127.0.0.1:0\"\npoll_enabled=true\n[[arr]]\nid=\"movies\"\nkind=\"radarr\"\nurl=\"http://unused.example.invalid\"\napi_key={{env=\"INTENTIONALLY_UNRESOLVED\"}}\nlibraries=[\"movies\"]\n")).unwrap();
    let output = dir.path().join("review.jsonl");
    let result = tokio::process::Command::new(env!("CARGO_BIN_EXE_pitcrew"))
        .arg("--config")
        .arg(config)
        .arg("--export")
        .arg(&output)
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(result.status.success(), "{:?}", result.stderr);
    let summary = std::fs::read_to_string(output.with_extension("jsonl.summary.txt")).unwrap();
    assert!(summary.contains("Queue download ownership: engine=0, external_tool=1, external_importer=0, python=0, unknown=0."),"{summary}");
    assert!(summary.contains("Queue import actions: engine=0, external_tool=0, external_importer=1, python=0, unknown=0."),"{summary}");
    assert!(summary.contains("ExternalTool: observed Grab -> proposed Hold: 1"));
    assert!(summary.contains("ExternalImporter: observed Import -> proposed Hold: 1"));
    let exported = std::fs::read_to_string(output.with_extension("jsonl.arbiter.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(exported.len(), malformed.len() + 1);
    for (row, original) in exported[1..].iter().zip(malformed) {
        assert_eq!(row["payload"], original);
    }
    let exported = &exported[0];
    assert_eq!(exported["payload"]["legacy_action"], "Import");
    assert_eq!(
        exported["payload"]["divergence"],
        "ImportWithoutSafeProposal"
    );
    assert_eq!(exported["payload"]["decision"]["reason"], "AccessUnknown");
}

#[tokio::test]
async fn engine_dispatch_compares_the_exact_saved_proposal_but_pending_intents_do_not_attribute() {
    use pitcrew_domain::{Access, Event, FileIdentity, IntentKey, Policy, StageTimeouts};
    use pitcrew_engine::observer::{DispatchReceipt, download_correlation, record_dispatch};
    use pitcrew_store::NewJob;
    let dir = tempfile::tempdir().unwrap();
    let service = app(&dir.path().join("engine")).await;
    service
        .store
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
    let lease = service
        .store
        .acquire_lease(1, "test", 0, 100)
        .await
        .unwrap();
    let applied = service
        .store
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
    let receipt = DispatchReceipt {
        correlation: download_correlation("movies", "download").unwrap(),
        intent: intent.clone(),
    };
    // Model a corrupt/premature receipt: the reader also checks the outbox.
    service
        .store
        .ingest_observation(
            "engine_dispatch",
            "1:1",
            &serde_json::to_value(&receipt).unwrap(),
            2,
        )
        .await
        .unwrap();
    history(&service, "1", "movies", "Grab", "download").await;
    assert_eq!(
        service.reports().await.unwrap()[0].observed.actor,
        Actor::Unknown
    );
    let key = IntentKey {
        job: 1,
        sequence: intent.sequence,
    };
    service.store.start_intent(&lease, key, 1, 2).await.unwrap();
    record_dispatch(&service.store, &intent, "movies", "download", 2)
        .await
        .unwrap();
    let reports = service.reports().await.unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].observed.actor, Actor::Engine);
    assert_eq!(reports[0].proposed.action, Action::Grab);
    assert!(reports[0].proposed.correlated);
    assert!(reports[0].missing_evidence);
    assert_eq!(reports[0].observed.safety.budget, None);
    assert_eq!(reports[0].proposed.safety.budget, Some(true));
    service
        .store
        .finish_intent(&lease, key, pitcrew_store::OutboxStatus::Shadowed, 3)
        .await
        .unwrap();
    assert_eq!(
        service.reports().await.unwrap()[0].observed.actor,
        Actor::Unknown
    );
}
