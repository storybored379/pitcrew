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
    source(&source_path, "SELECT 1").await;
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
    assert!(String::from_utf8_lossy(&output.stdout).contains("\"new_actions\":4"));
    assert_eq!(service.reports().await.unwrap().len(), 4);
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
