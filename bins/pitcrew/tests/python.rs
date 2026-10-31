use pitcrew::{
    App,
    python::{self, Error},
};
use pitcrew_engine::observer::{Action, Actor, Agreement};
use pitcrew_store::{BudgetLimits, Store};
use sqlx::{Connection, Executor, SqliteConnection, sqlite::SqliteConnectOptions};
use std::{collections::BTreeMap, path::Path};

async fn source(path: &Path, extra: &str) {
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
    db.execute(extra).await.unwrap();
    db.close().await.unwrap();
}
async fn app(path: &Path) -> App {
    App::new(
        Store::open(path).await.unwrap(),
        vec![],
        None,
        BudgetLimits {
            global_hour: 20,
            global_day: 40,
            viewer_hour: 10,
            viewer_day: 20,
        },
        2,
        BTreeMap::new(),
    )
    .unwrap()
}
async fn project(app: &App) {
    let mut cursor = 0;
    loop {
        let next = app.project(cursor).await.unwrap();
        if next == cursor {
            break;
        }
        cursor = next;
    }
}

#[tokio::test]
async fn imports_proven_ownership_preserves_unknowns_and_deduplicates_after_restart() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("source.sqlite");
    source(&path, "SELECT 1").await;
    let before = std::fs::read(&path).unwrap();
    let snapshot = python::read(&path, "legacy-one").await.unwrap();
    assert_eq!(snapshot.unowned_attempts, 1);
    assert_eq!(snapshot.budget.research_global_rows, 1);
    assert_eq!(snapshot.budget.research_viewer_rows, 1);
    assert_eq!(snapshot.budget.item_fix_rows, 1);
    assert_eq!(snapshot.budget.decode_retry_rows, 1);
    assert_eq!(snapshot.budget_records.len(), 4);
    assert_eq!(snapshot.budget_records[0].at, 999);
    assert_eq!(snapshot.budget_records[0].key, "rate:1");
    assert_eq!(
        snapshot.budget_records[0].kind,
        python::BudgetKind::ResearchGlobal
    );
    assert_eq!(snapshot.budget_records[1].key, "rate:2");
    assert_eq!(
        snapshot.budget_records[1].kind,
        python::BudgetKind::ResearchViewer
    );
    assert_eq!(snapshot.replays[0].at, 1001);
    assert_eq!(
        snapshot
            .replays
            .iter()
            .map(|r| r.activity.action)
            .collect::<Vec<_>>(),
        vec![Action::Grab, Action::Import, Action::Decode, Action::Ready]
    );
    for replay in &snapshot.replays {
        assert_eq!(replay.activity.actor, Actor::Python);
        assert!(replay.activity.correlated);
        assert_eq!(replay.activity.safety.access, None);
        assert_eq!(replay.activity.safety.budget, None);
        assert_eq!(replay.activity.safety.exact_import_decoded, None);
        assert_eq!(
            replay.comparison().agreement,
            Agreement::InsufficientEvidence
        );
        assert_eq!(replay.comparison().proposed.action, Action::Hold);
    }
    assert_eq!(snapshot.replays[2].decoded_arr_file, Some(202));
    let destination = temp.path().join("engine.sqlite");
    let service = app(&destination).await;
    assert_eq!(service.import_python(snapshot, 2000).await.unwrap(), 4);
    project(&service).await;
    let reopened = app(&destination).await;
    assert_eq!(
        reopened
            .import_python(python::read(&path, "legacy-one").await.unwrap(), 3000)
            .await
            .unwrap(),
        0
    );
    project(&reopened).await;
    assert_eq!(reopened.reports().await.unwrap().len(), 4);
    let records = reopened.store.observations().await.unwrap();
    let serialized = serde_json::to_string(&records).unwrap();
    assert!(!serialized.contains("PRIVATE_"));
    assert!(serialized.contains("budget_attributed_to_report"));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    for suffix in ["-wal", "-shm", "-journal"] {
        assert!(!temp.path().join(format!("source.sqlite{suffix}")).exists());
    }
    assert!(reopened.store.outbox(1).await.unwrap().is_empty());
    assert!(reopened.store.jobs().await.unwrap().is_empty());
}

#[tokio::test]
async fn rejects_sidecars_missing_files_schema_and_destination_aliases_without_writes() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("source.sqlite");
    assert_eq!(
        python::read(&path, "source").await.unwrap_err(),
        Error::SourceUnavailable
    );
    assert!(!path.exists());
    source(&path, "SELECT 1").await;
    let before = std::fs::read(&path).unwrap();
    for suffix in ["-wal", "-shm", "-journal"] {
        let sidecar = temp.path().join(format!("source.sqlite{suffix}"));
        std::fs::write(&sidecar, "SYNTHETIC_SIDECAR").unwrap();
        assert_eq!(
            python::read(&path, "source").await.unwrap_err(),
            Error::SidecarPresent
        );
        assert_eq!(
            std::fs::read_to_string(&sidecar).unwrap(),
            "SYNTHETIC_SIDECAR"
        );
        std::fs::remove_file(sidecar).unwrap();
    }
    let alias = temp.path().join("alias");
    std::fs::hard_link(&path, &alias).unwrap();
    assert_eq!(
        python::distinct_destination(&path, &alias),
        Err(Error::DestinationAliasesSource)
    );
    std::fs::remove_file(alias).unwrap();
    assert_eq!(
        python::read(&path, "../private/path").await.unwrap_err(),
        Error::InvalidSourceKey
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let malformed = temp.path().join("invalid.sqlite");
    std::fs::write(&malformed, "not sqlite").unwrap();
    assert_eq!(
        python::read(&malformed, "source").await.unwrap_err(),
        Error::InvalidSchema
    );
}

#[tokio::test]
async fn mismatched_links_and_untrusted_audit_text_do_not_manufacture_ownership() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("source.sqlite");
    source(&path, "UPDATE fix_attempts SET media_id=999 WHERE id=1;").await;
    let snapshot = python::read(&path, "source").await.unwrap();
    assert!(snapshot.replays.is_empty());
    assert_eq!(snapshot.unowned_attempts, 2);
    assert_eq!(snapshot.uncorrelated_audits, 3);
    let path2 = temp.path().join("other.sqlite");
    source(&path2, "UPDATE audit SET detail='report 10 resolved: borrowed title',target='movie:tmdb:90001' WHERE action='dm_sent'; UPDATE audit SET detail='report 1 file 0: unknown' WHERE action='replacement_decode_check';").await;
    let snapshot = python::read(&path2, "source").await.unwrap();
    assert_eq!(snapshot.replays.len(), 2);
    assert_eq!(snapshot.uncorrelated_audits, 2);
}

#[tokio::test]
async fn failed_attempts_and_synthetic_decode_rows_are_not_successful_grabs() {
    let temp = tempfile::tempdir().unwrap();
    for (name, edit) in [
        ("none", "UPDATE fix_attempts SET verdict='none' WHERE id=1"),
        (
            "bad",
            "UPDATE fix_attempts SET title=NULL, verdict='bad' WHERE id=1",
        ),
    ] {
        let path = temp.path().join(name);
        source(&path, edit).await;
        assert!(
            python::read(&path, "source")
                .await
                .unwrap()
                .replays
                .is_empty()
        );
    }
}

#[tokio::test]
async fn cli_import_does_not_resolve_secrets_start_listener_or_poll() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("source.sqlite");
    source(&source_path, NATIVE_RECEIPTS).await;
    let config_path = temp.path().join("config.toml");
    let mut config = include_str!("../../../config.shadow.example.toml").to_owned();
    let database = temp.path().join("engine.sqlite");
    let service = app(&database).await;
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&database))
        .await
        .unwrap();
    db.execute("WITH RECURSIVE rows(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM rows WHERE n<1000) INSERT INTO observations(source,observation_key,payload,at) SELECT 'unrelated',CAST(n AS TEXT),'{}',1 FROM rows")
        .await
        .unwrap();
    db.close().await.unwrap();
    config = config.replace(
        "/var/lib/pitcrew/pitcrew.sqlite",
        database.to_str().unwrap(),
    );
    config = config.replace("poll_enabled = false", "poll_enabled = true");
    // The example contains unresolved file credentials; no path may be read.
    std::fs::write(&config_path, config).unwrap();
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_pitcrew"))
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--import-legacy",
            source_path.to_str().unwrap(),
            "--source-key",
            "synthetic",
        ])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("\"new_actions\":6"));
    assert_eq!(service.reports().await.unwrap().len(), 4);
    let receipts = service.store.action_receipts().await.unwrap();
    assert_eq!(receipts.len(), 2);
    for receipt in receipts {
        assert_eq!(receipt.payload["action"], "import");
        assert_eq!(receipt.payload["actor"], "external_tool");
    }
    let imported = service.store.observations().await.unwrap();
    assert!(
        imported
            .iter()
            .find(|row| row.source == "python_import:synthetic")
            .unwrap()
            .at
            >= started
    );
}

#[tokio::test]
async fn ownership_requires_every_structural_link_and_supported_media_kind() {
    let temp = tempfile::tempdir().unwrap();
    for (index, update) in [
        "UPDATE fix_attempts SET item_key='movie:tmdb:90002' WHERE id=1",
        "UPDATE fix_attempts SET instance='other-instance' WHERE id=1",
        "UPDATE fix_attempts SET media_kind='episode' WHERE id=1",
        "UPDATE reports SET media_type='show' WHERE id=1",
        "UPDATE reports SET arr_instance=NULL WHERE id=1",
        "UPDATE reports SET arr_media_id=NULL WHERE id=1",
        "UPDATE fix_attempts SET report_id=99 WHERE id=1",
        "UPDATE fix_attempts SET report_id=-1 WHERE id=1",
        "UPDATE fix_attempts SET verdict='unsupported' WHERE id=1",
        "UPDATE fix_attempts SET title='' WHERE id=1",
    ]
    .into_iter()
    .enumerate()
    {
        let path = temp.path().join(format!("source-{index}"));
        source(&path, update).await;
        let snapshot = python::read(&path, "source").await.unwrap();
        assert!(snapshot.replays.is_empty(), "case {index}");
        assert_eq!(snapshot.unowned_attempts, 2);
    }
    let episode = temp.path().join("episode");
    source(
        &episode,
        "UPDATE reports SET media_type='show'; UPDATE fix_attempts SET media_kind='episode';",
    )
    .await;
    assert_eq!(
        python::read(&episode, "source")
            .await
            .unwrap()
            .replays
            .len(),
        4
    );
}

#[tokio::test]
async fn budget_categories_timestamps_and_invalid_numeric_rows_are_explicit() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("source");
    source(&path, "INSERT INTO item_fixes(cooldown_key,item_key,ts,kind) VALUES ('opaque','opaque',1003,'owner'),('opaque','opaque',1004,'landmine');").await;
    let snapshot = python::read(&path, "source").await.unwrap();
    assert_eq!(snapshot.budget.owner_fix_rows, 1);
    assert_eq!(snapshot.budget.landmine_rows, 1);
    assert_eq!(snapshot.budget_records[4].at, 1003);
    assert_eq!(snapshot.budget_records[5].at, 1004);
    assert_eq!(snapshot.budget_records[4].kind, python::BudgetKind::Owner);
    assert_eq!(
        snapshot.budget_records[5].kind,
        python::BudgetKind::Landmine
    );
    for (index, update) in [
        "UPDATE fix_attempts SET media_id=0 WHERE id=1",
        "UPDATE fix_attempts SET id=-1 WHERE id=1",
        "UPDATE reports SET id=0 WHERE id=1",
        "UPDATE fix_attempts SET ts=-1 WHERE id=1",
        "UPDATE fix_attempts SET ts=1e100 WHERE id=1",
        "UPDATE audit SET ts='invalid' WHERE id=1",
        "UPDATE item_fixes SET kind='unsupported' WHERE id=1",
        "UPDATE rate_events SET ts=-1 WHERE id=1",
    ]
    .into_iter()
    .enumerate()
    {
        let path = temp.path().join(format!("invalid-{index}"));
        source(&path, update).await;
        assert_eq!(
            python::read(&path, "source").await.unwrap_err(),
            Error::InvalidRow,
            "case {index}"
        );
    }
}

#[tokio::test]
async fn source_names_destinations_and_schema_fail_closed_at_boundaries() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("source");
    source(&path, "SELECT 1").await;
    for key in [
        "".to_owned(),
        "a".repeat(65),
        "unicode-é".into(),
        "space key".into(),
    ] {
        assert_eq!(
            python::read(&path, &key).await.unwrap_err(),
            Error::InvalidSourceKey
        );
    }
    assert!(python::read(&path, &"a".repeat(64)).await.is_ok());
    assert!(python::read(&path, "A0_b-Z").await.is_ok());
    for suffix in ["", "-wal", "-shm", "-journal"] {
        assert_eq!(
            python::distinct_destination(&path, &temp.path().join(format!("source{suffix}"))),
            Err(Error::DestinationAliasesSource)
        );
    }
    assert!(python::distinct_destination(&path, &temp.path().join("engine")).is_ok());
    std::fs::write(temp.path().join("existing-engine"), "unrelated").unwrap();
    assert!(python::distinct_destination(&path, &temp.path().join("existing-engine")).is_ok());
    assert!(
        python::distinct_destination(&path, std::path::Path::new("nonexistent-engine.sqlite"))
            .is_ok()
    );
    assert_eq!(
        python::distinct_destination(&path, &temp.path().join("x".repeat(256))),
        Err(Error::SourceUnavailable)
    );
    let view = temp.path().join("view");
    source(&view, "ALTER TABLE rate_events RENAME TO archived_rates; CREATE VIEW rate_events AS SELECT * FROM archived_rates;").await;
    assert_eq!(
        python::read(&view, "source").await.unwrap_err(),
        Error::InvalidSchema
    );
    let missing = temp.path().join("missing");
    source(&missing, "DROP TABLE fix_attempts").await;
    assert_eq!(
        python::read(&missing, "source").await.unwrap_err(),
        Error::InvalidSchema
    );
    assert_eq!(
        Error::SidecarPresent.to_string(),
        "legacy snapshot import: SidecarPresent"
    );
}

#[tokio::test]
async fn changed_historical_audit_is_a_conflict_and_budget_rows_are_deduplicated() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("source");
    source(&path, "SELECT 1").await;
    let service = app(&temp.path().join("engine")).await;
    service
        .import_python(python::read(&path, "source").await.unwrap(), 100)
        .await
        .unwrap();
    service
        .import_python(python::read(&path, "source").await.unwrap(), 200)
        .await
        .unwrap();
    let rows = service.store.observations().await.unwrap();
    assert_eq!(
        rows.iter()
            .filter(|row| row.source == "python_budget:source")
            .count(),
        4
    );
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    db.execute("UPDATE audit SET outcome='bad' WHERE action='replacement_decode_check'")
        .await
        .unwrap();
    db.close().await.unwrap();
    assert_eq!(
        service
            .import_python(python::read(&path, "source").await.unwrap(), 300)
            .await,
        Err(Error::StoreUnavailable)
    );
}

#[tokio::test]
async fn audit_actions_require_their_own_machine_prefix_scope_and_outcome() {
    let dir = tempfile::tempdir().unwrap();
    for (i,edit) in [
        "UPDATE audit SET detail='report 1 unrelated: text' WHERE action='replacement_manual_import'",
        "UPDATE audit SET target='other: movie:tmdb:90001' WHERE action='replacement_manual_import'",
        "UPDATE audit SET outcome='failed' WHERE action='replacement_manual_import'",
        "UPDATE audit SET target='movie:tmdb:90002' WHERE action='replacement_decode_check'",
        "UPDATE audit SET detail='report 1 pending: text' WHERE action='dm_sent'",
        "UPDATE audit SET target='movie:tmdb:90002' WHERE action='dm_sent'",
        "UPDATE audit SET outcome='failed' WHERE action='dm_sent'",
        "UPDATE audit SET detail='freeform report 1 resolved: text' WHERE action='dm_sent'",
        "UPDATE audit SET detail='report 0 resolved: text' WHERE action='dm_sent'",
        "UPDATE audit SET detail='report 18446744073709551616 resolved: text' WHERE action='dm_sent'",
    ].into_iter().enumerate() {
        let path=dir.path().join(format!("audit-{i}"));
        source(&path,edit).await;
        let snapshot=python::read(&path,"source").await.unwrap();
        assert_eq!(snapshot.replays.len(),3,"case {i}");
        assert_eq!(snapshot.uncorrelated_audits,1,"case {i}");
    }
    for (i, (outcome, evidence)) in [
        ("ok", python::DecodeEvidence::Ok),
        ("bad", python::DecodeEvidence::Bad),
        ("other", python::DecodeEvidence::Unknown),
    ]
    .into_iter()
    .enumerate()
    {
        let path = dir.path().join(format!("decode-{i}"));
        source(
            &path,
            &format!(
                "UPDATE audit SET outcome='{outcome}' WHERE action='replacement_decode_check'"
            ),
        )
        .await;
        let snapshot = python::read(&path, "source").await.unwrap();
        assert_eq!(snapshot.replays[2].decode, Some(evidence));
        assert_eq!(snapshot.replays[1].at, 1767225600);
    }
}

#[tokio::test]
async fn a_snapshot_exceeding_the_global_row_limit_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    for (extra, accepted) in [(49_990, true), (49_991, false)] {
        let path = dir.path().join(format!("rows-{extra}"));
        source(&path,&format!("WITH RECURSIVE rows(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM rows WHERE n < {extra}) INSERT INTO rate_events(bucket,key,ts) SELECT 'unrelated','opaque',1 FROM rows")).await;
        let result = python::read(&path, "source").await;
        if accepted {
            assert!(result.is_ok());
        } else {
            assert_eq!(result.unwrap_err(), Error::TooManyRows);
        }
    }
}

#[tokio::test]
async fn large_regular_snapshots_are_bounded_before_copy_and_directories_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snapshot");
    source(&path, "SELECT 1").await;
    let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_len(4 * 1024 * 1024).unwrap();
    assert_eq!(
        python::read(&path, "source").await.unwrap().replays.len(),
        4
    );
    file.set_len(256 * 1024 * 1024 + 1).unwrap();
    assert_eq!(
        python::read(&path, "source").await.unwrap_err(),
        Error::SourceTooLarge
    );
    assert_eq!(
        python::read(dir.path(), "source").await.unwrap_err(),
        Error::SourceUnavailable
    );
}

#[tokio::test]
async fn exact_snapshot_byte_limit_is_inclusive_and_not_truncated() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snapshot");
    source(&path, "SELECT 1").await;
    // SQLite's header keeps the valid database page count; sparse trailing
    // padding exercises the real copy boundary without allocating library data.
    let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_len(268_435_456).unwrap();
    assert_eq!(
        python::read(&path, "source").await.unwrap().replays.len(),
        4
    );
    assert_eq!(file.metadata().unwrap().len(), 268_435_456);
}

#[tokio::test]
async fn cli_rejects_dangling_destination_symlinks_without_creating_source_sidecars() {
    let dir = tempfile::tempdir().unwrap();
    for suffix in ["-wal", "-shm", "-journal"] {
        let source_path = dir.path().join(format!("source{suffix}.sqlite"));
        source(&source_path, "SELECT 1").await;
        let before = std::fs::read(&source_path).unwrap();
        let sidecar = dir.path().join(format!("source{suffix}.sqlite{suffix}"));
        let destination = dir.path().join(format!("destination{suffix}.sqlite"));
        std::os::unix::fs::symlink(&sidecar, &destination).unwrap();
        let config = include_str!("../../../config.shadow.example.toml").replace(
            "/var/lib/pitcrew/pitcrew.sqlite",
            destination.to_str().unwrap(),
        );
        let config_path = dir.path().join(format!("config{suffix}.toml"));
        std::fs::write(&config_path, config).unwrap();
        let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_pitcrew"))
            .args([
                "--config",
                config_path.to_str().unwrap(),
                "--import-legacy",
                source_path.to_str().unwrap(),
                "--source-key",
                "synthetic",
            ])
            .output()
            .await
            .unwrap();
        assert_eq!(std::fs::read(&source_path).unwrap(), before);
        assert!(
            !sidecar.exists(),
            "created protected source sidecar {suffix}"
        );
        assert!(
            !output.status.success(),
            "accepted aliased destination {suffix}"
        );
        assert!(
            std::fs::symlink_metadata(&destination)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}

const ACTION_LOG: &str = "CREATE TABLE arr_actions(id INTEGER PRIMARY KEY,instance TEXT,download_id TEXT,actor TEXT,action TEXT); INSERT INTO arr_actions VALUES(1,'movies','download-one','external_tool','grab'),(2,'movies','download-one','external_importer','import');";

#[tokio::test]
async fn explicit_attempt_downloads_require_a_successful_named_release_and_only_claim_grab() {
    let dir = tempfile::tempdir().unwrap();
    for (i, verdict) in ["grabbed", "imported", "ok", "bad", "none", "unsupported"]
        .into_iter()
        .enumerate()
    {
        for (j, title) in ["'release'", "''", "NULL"].into_iter().enumerate() {
            let path = dir.path().join(format!("attempt-{i}-{j}"));
            source(&path, &format!("ALTER TABLE fix_attempts ADD COLUMN download_id TEXT; UPDATE fix_attempts SET instance='movies',download_id='download-one',title={title},verdict='{verdict}' WHERE id=2;")).await;
            let snapshot = python::read(&path, "log").await.unwrap();
            assert_eq!(snapshot.arr_actions.len(), usize::from(i < 4 && j == 0));
            if let Some(receipt) = snapshot.arr_actions.first() {
                assert_eq!(receipt.key, "attempt:2:download");
                assert_eq!(receipt.actor, Actor::ExternalTool);
                assert_eq!(receipt.action, Action::Grab);
                assert_eq!(
                    receipt.correlation,
                    pitcrew_engine::observer::download_correlation("movies", "download-one")
                        .unwrap()
                );
            }
        }
    }
    for (i, edit) in [
        "SELECT 1",
        "ALTER TABLE fix_attempts ADD COLUMN download_id TEXT",
    ]
    .into_iter()
    .enumerate()
    {
        let path = dir.path().join(format!("no-download-{i}"));
        source(&path, edit).await;
        assert!(
            python::read(&path, "log")
                .await
                .unwrap()
                .arr_actions
                .is_empty()
        );
    }
}

#[tokio::test]
async fn action_logs_preserve_each_exact_action_and_the_shared_row_limit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("actions");
    source(&path, &format!("{ACTION_LOG} INSERT INTO arr_actions VALUES(3,'movies','download-one','external_tool','remove_download'),(4,'movies','download-one','external_importer','delete_library');")).await;
    let snapshot = python::read(&path, "log").await.unwrap();
    assert_eq!(
        snapshot
            .arr_actions
            .iter()
            .map(|a| (a.action, a.actor))
            .collect::<Vec<_>>(),
        vec![
            (Action::Grab, Actor::ExternalTool),
            (Action::Import, Actor::ExternalImporter),
            (Action::RemoveDownload, Actor::ExternalTool),
            (Action::DeleteLibrary, Actor::ExternalImporter)
        ]
    );
    for (i, receipt) in snapshot.arr_actions.iter().enumerate() {
        assert_eq!(receipt.key, format!("action:{}", i + 1));
        assert_eq!(
            receipt.correlation,
            pitcrew_engine::observer::download_correlation("movies", "download-one").unwrap()
        );
    }
    // Ten existing fixture rows plus 49,990 action rows exactly reaches the cap.
    let path = dir.path().join("at-limit");
    source(&path, &format!("{ACTION_LOG} WITH RECURSIVE n(x) AS (VALUES(3) UNION ALL SELECT x+1 FROM n WHERE x<49990) INSERT INTO arr_actions SELECT x,'movies','download','external_tool','grab' FROM n;")).await;
    assert_eq!(
        python::read(&path, "log").await.unwrap().arr_actions.len(),
        49_990
    );
}

#[tokio::test]
async fn legacy_log_schema_and_identifiers_fail_closed_and_no_download_is_inferred() {
    let dir = tempfile::tempdir().unwrap();
    for (i, edit) in [
        "UPDATE arr_actions SET actor='engine'",
        "UPDATE arr_actions SET actor='unknown'",
        "UPDATE arr_actions SET action='ready'",
        "UPDATE arr_actions SET download_id=''",
        "UPDATE arr_actions SET instance=''",
        "UPDATE arr_actions SET id=-1 WHERE id=1",
        "UPDATE arr_actions SET download_id=NULL",
    ]
    .into_iter()
    .enumerate()
    {
        let path = dir.path().join(format!("invalid-{i}"));
        source(&path, &format!("{ACTION_LOG} {edit}")).await;
        assert_eq!(
            python::read(&path, "log").await.unwrap_err(),
            python::Error::InvalidRow
        );
    }
    let view = dir.path().join("view");
    source(
        &view,
        &format!("{ACTION_LOG} ALTER TABLE arr_actions RENAME TO saved; CREATE VIEW arr_actions AS SELECT * FROM saved;"),
    )
    .await;
    assert_eq!(
        python::read(&view, "log").await.unwrap_err(),
        python::Error::InvalidSchema
    );
    let big = dir.path().join("large");
    source(&big, &format!("{ACTION_LOG} WITH RECURSIVE n(x) AS (VALUES(3) UNION ALL SELECT x+1 FROM n WHERE x<50000) INSERT INTO arr_actions SELECT x,'movies','download','external_tool','grab' FROM n;")).await;
    assert_eq!(
        python::read(&big, "log").await.unwrap_err(),
        python::Error::TooManyRows
    );
}

// Native completion markers are written only after accepted ManualImport. The
// synthetic schema intentionally has no optional fix_attempts.download_id.
const NATIVE_RECEIPTS: &str = "
CREATE TABLE kv(k TEXT PRIMARY KEY, v TEXT);
CREATE TABLE landmines(id INTEGER PRIMARY KEY, instance TEXT);
INSERT INTO landmines VALUES(9,'PRIVATE_LANDMINE_INSTANCE_SENTINEL');
INSERT INTO kv VALUES
('finish:1:PRIVATE_REPORT_DOWNLOAD_SENTINEL','1700000000.125'),
('lmfinish:9:PRIVATE_LANDMINE_DOWNLOAD_SENTINEL','2');";

#[tokio::test]
async fn native_completion_receipts_attribute_only_import_and_preserve_snapshot_privacy() {
    use pitcrew_engine::observer::download_correlation;
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("native");
    source(
        &path,
        &format!(
            "{NATIVE_RECEIPTS}
        DELETE FROM fix_attempts; DELETE FROM audit;
        INSERT INTO kv VALUES
          ('finish:1:PRIVATE_REPORT_DOWNLOAD_SENTINEL:n',X'FF'),
          ('finish:1:PRIVATE_REPORT_DOWNLOAD_SENTINEL:wait',X'FF'),
          ('predecode:PRIVATE_REPORT_DOWNLOAD_SENTINEL',X'FF'),
          ('PRIVATE_TOKEN_SENTINEL',X'FF');"
        ),
    )
    .await;
    let original = std::fs::read(&path).unwrap();
    let snapshot = python::read(&path, "native").await.unwrap();
    assert_eq!(snapshot.arr_actions.len(), 2);
    assert!(snapshot.replays.is_empty());
    let expected = [
        (
            "PRIVATE_INSTANCE_SENTINEL",
            "PRIVATE_REPORT_DOWNLOAD_SENTINEL",
        ),
        (
            "PRIVATE_LANDMINE_INSTANCE_SENTINEL",
            "PRIVATE_LANDMINE_DOWNLOAD_SENTINEL",
        ),
    ];
    for (instance, download) in expected {
        assert!(snapshot.arr_actions.iter().any(|receipt| {
            receipt.correlation == download_correlation(instance, download).unwrap()
                && receipt.actor == Actor::ExternalTool
                && receipt.action == Action::Import
        }));
    }
    let destination = dir.path().join("engine");
    let service = app(&destination).await;
    assert_eq!(service.import_legacy(snapshot, 2000).await.unwrap(), 2);
    let imported = serde_json::to_string(&service.store.observations().await.unwrap()).unwrap();
    for private in ["PRIVATE_", "1700000000.125", "finish:1:", "lmfinish:9:"] {
        assert!(!imported.contains(private), "{private}");
    }
    assert_eq!(std::fs::read(&path).unwrap(), original);
    for suffix in ["-wal", "-shm", "-journal"] {
        assert!(!dir.path().join(format!("native{suffix}")).exists());
    }
    let restarted = app(&destination).await;
    assert_eq!(
        restarted
            .import_legacy(python::read(&path, "native").await.unwrap(), 3000)
            .await
            .unwrap(),
        0
    );
    for (i, (instance, download)) in expected.into_iter().enumerate() {
        for kind in ["Download", "Grab", "MovieFileDelete"] {
            restarted
                .store
                .ingest_observation(
                    &format!("history:{instance}"),
                    &format!("{i}:{kind}"),
                    &json!({"kind":kind,"download":download,"media":[101],"files":[]}),
                    20,
                )
                .await
                .unwrap();
        }
        let index = pitcrew::attribution::Index::load(&restarted.store)
            .await
            .unwrap();
        let mut queue = json!({"download_correlation":download_correlation(instance,download),
            "decision":{"action":"Hold","reason":"AccessUnknown"}, "owned_download":null});
        index.queue(&mut queue);
        assert_eq!(queue["download_actor"], "unknown");
        assert_eq!(queue["import_actor"], "external_tool");
        assert_eq!(queue["legacy_action"], "Import");
        assert_eq!(queue["observed_import"], true);
        assert_eq!(queue["decision"]["reason"], "AccessUnknown");
        assert!(queue["owned_download"].is_null());
        for (other_instance, other_download) in [("other", download), (instance, "other")] {
            assert_eq!(
                index
                    .compare(
                        String::new(),
                        download_correlation(other_instance, other_download),
                        Action::Import
                    )
                    .observed
                    .actor,
                Actor::Unknown
            );
        }
    }
    project(&restarted).await;
    let reports = restarted.reports().await.unwrap();
    assert_eq!(reports.len(), 6);
    for report in reports {
        assert_eq!(
            report.observed.actor,
            if report.observed.action == Action::Import {
                Actor::ExternalTool
            } else {
                Actor::Unknown
            }
        );
        assert_eq!(report.proposed.action, Action::Hold);
        assert_eq!(report.observed.safety, Default::default());
        assert!(report.missing_evidence);
    }
    assert!(restarted.store.jobs().await.unwrap().is_empty());
}

#[tokio::test]
async fn native_completion_counters_waits_missing_links_and_absent_identifiers_do_not_attribute() {
    let dir = tempfile::tempdir().unwrap();
    for (i, edit) in [
        "DELETE FROM reports; DELETE FROM landmines;",
        "UPDATE reports SET arr_instance=NULL; UPDATE landmines SET instance=NULL;",
        "UPDATE reports SET arr_instance=''; UPDATE landmines SET instance='';",
        "UPDATE kv SET k=k || ':n',v=X'FF';",
        "UPDATE kv SET k=k || ':wait',v=X'FF';",
        "DELETE FROM kv; INSERT INTO kv VALUES('finish:1:None','1'),('lmfinish:9:None','1');",
        "DELETE FROM kv; INSERT INTO kv VALUES('finish:1:','1'),('lmfinish:9:','1');",
        "DELETE FROM kv; INSERT INTO kv VALUES('predecode:1:download',X'FF'),('FINISH:1:download',X'FF'),('unrelated',X'FF');",
    ].into_iter().enumerate() {
        let path = dir.path().join(format!("ignored-{i}"));
        source(&path,&format!("{NATIVE_RECEIPTS} {edit}")).await;
        assert!(python::read(&path,"native").await.unwrap().arr_actions.is_empty(),"{edit}");
    }
    for (table, expected) in [("kv", 0), ("landmines", 1)] {
        let path = dir.path().join(format!("absent-{table}"));
        source(&path, &format!("{NATIVE_RECEIPTS} DROP TABLE {table};")).await;
        assert_eq!(
            python::read(&path, "native")
                .await
                .unwrap()
                .arr_actions
                .len(),
            expected
        );
    }
}

#[tokio::test]
async fn native_completion_receipts_reject_malformed_keys_and_positive_value_violations() {
    let dir = tempfile::tempdir().unwrap();
    let mut cases = Vec::new();
    for prefix in ["finish", "lmfinish"] {
        let id = if prefix == "finish" { "1" } else { "9" };
        for invalid_id in ["0", "-1", "+1", "01", "", "18446744073709551616"] {
            cases.push((format!("{prefix}:{invalid_id}:download"), "'1'"));
        }
        for download in ["with space", "with\nnewline", "synthetic:extra"] {
            cases.push((format!("{prefix}:{id}:{download}"), "'1'"));
        }
        cases.push((format!("{prefix}:{id}:{}", "x".repeat(257)), "'1'"));
        cases.push((format!("{prefix}:{id}"), "'1'"));
        for value in [
            "'0'",
            "'-1'",
            "'NaN'",
            "'inf'",
            "'unknown'",
            "NULL",
            "X'FF'",
        ] {
            cases.push((format!("{prefix}:{id}:download"), value));
        }
    }
    cases.push(("finish:1:download".into(), "'9223372036854775808'"));
    cases.push(("lmfinish:9:download".into(), "'1.5'"));
    cases.push(("lmfinish:9:download".into(), "'18446744073709551616'"));
    for (i, (key, value)) in cases.into_iter().enumerate() {
        let path = dir.path().join(format!("invalid-native-{i}"));
        source(
            &path,
            &format!("{NATIVE_RECEIPTS} DELETE FROM kv; INSERT INTO kv VALUES('{key}',{value});"),
        )
        .await;
        assert_eq!(
            python::read(&path, "native").await.unwrap_err(),
            Error::InvalidRow,
            "{key}"
        );
    }
    for (i, edit) in [
        "ALTER TABLE kv RENAME TO saved; CREATE TABLE kv AS SELECT * FROM saved UNION ALL SELECT * FROM saved;",
        "ALTER TABLE landmines RENAME TO saved; CREATE TABLE landmines AS SELECT * FROM saved UNION ALL SELECT * FROM saved;",
        "UPDATE reports SET arr_instance='invalid instance';",
        "UPDATE landmines SET instance='invalid instance';",
    ].into_iter().enumerate() {
        let path = dir.path().join(format!("ambiguous-native-{i}"));
        source(&path,&format!("{NATIVE_RECEIPTS} {edit}")).await;
        assert_eq!(python::read(&path,"native").await.unwrap_err(),Error::InvalidRow);
    }
}

#[tokio::test]
async fn native_completion_tables_require_real_schema_and_share_the_global_row_cap() {
    let dir = tempfile::tempdir().unwrap();
    for (i, edit) in [
        "ALTER TABLE kv RENAME TO saved; CREATE VIEW kv AS SELECT * FROM saved;",
        "ALTER TABLE landmines RENAME TO saved; CREATE VIEW landmines AS SELECT * FROM saved;",
        "DROP TABLE kv; CREATE TABLE kv(other TEXT);",
        "DROP TABLE landmines; CREATE TABLE landmines(other TEXT);",
    ]
    .into_iter()
    .enumerate()
    {
        let path = dir.path().join(format!("native-schema-{i}"));
        source(&path, &format!("{NATIVE_RECEIPTS} {edit}")).await;
        assert_eq!(
            python::read(&path, "native").await.unwrap_err(),
            Error::InvalidSchema
        );
    }
    // Ten base rows + three native rows + two normalized actions + 49,985
    // unrelated KV rows. Even ignored values consume the common snapshot cap.
    for (i, added) in [49_985, 49_986].into_iter().enumerate() {
        let path = dir.path().join(format!("native-cap-{i}"));
        source(
            &path,
            &format!(
                "{NATIVE_RECEIPTS} {ACTION_LOG}
            WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<{added})
            INSERT INTO kv SELECT 'unrelated:' || x,X'FF' FROM n;"
            ),
        )
        .await;
        let result = python::read(&path, "native").await;
        if i == 0 {
            assert_eq!(result.unwrap().arr_actions.len(), 4);
        } else {
            assert_eq!(result.unwrap_err(), Error::TooManyRows);
        }
    }
    let path = dir.path().join("landmine-cap");
    source(
        &path,
        &format!(
            "{NATIVE_RECEIPTS}
        WITH RECURSIVE n(x) AS (VALUES(10) UNION ALL SELECT x+1 FROM n WHERE x<50010)
        INSERT INTO landmines SELECT x,'movies' FROM n;"
        ),
    )
    .await;
    assert_eq!(
        python::read(&path, "native").await.unwrap_err(),
        Error::TooManyRows
    );
}

#[tokio::test]
async fn native_receipt_keys_ignore_counter_changes_but_reject_changed_instance_claims() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("native");
    source(&path, NATIVE_RECEIPTS).await;
    let service = app(&dir.path().join("engine")).await;
    service
        .import_legacy(python::read(&path, "native").await.unwrap(), 2000)
        .await
        .unwrap();
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    db.execute("UPDATE kv SET v='3' WHERE k GLOB 'lmfinish:*'; UPDATE kv SET v='1700000001.5' WHERE k GLOB 'finish:*';").await.unwrap();
    db.close().await.unwrap();
    assert_eq!(
        service
            .import_legacy(python::read(&path, "native").await.unwrap(), 3000)
            .await
            .unwrap(),
        0
    );
    let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    db.execute("UPDATE landmines SET instance='other';")
        .await
        .unwrap();
    db.close().await.unwrap();
    assert_eq!(
        service
            .import_legacy(python::read(&path, "native").await.unwrap(), 3001)
            .await
            .unwrap_err(),
        Error::StoreUnavailable
    );
}

#[tokio::test]
async fn native_completion_refuses_duplicate_report_ids_instead_of_choosing_an_instance() {
    let dir = tempfile::tempdir().unwrap();
    for (i, instance) in ["PRIVATE_INSTANCE_SENTINEL", "other"]
        .into_iter()
        .enumerate()
    {
        let path = dir.path().join(format!("duplicate-report-{i}"));
        source(
            &path,
            &format!(
                "{NATIVE_RECEIPTS}
            ALTER TABLE reports RENAME TO saved;
            CREATE TABLE reports AS SELECT * FROM saved;
            UPDATE saved SET arr_instance='{instance}';
            INSERT INTO reports SELECT * FROM saved;"
            ),
        )
        .await;
        assert_eq!(
            python::read(&path, "native").await.unwrap_err(),
            Error::InvalidRow
        );
    }
}
