use pitcrew::stack::{bridge, bridge_options, web_config};
use pitcrew_config::Config;
use pitcrew_intake::{CatalogItem, Resolution};
use pitcrew_store::Store;
use sqlx::{
    Row,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

fn catalog() -> &'static str {
    "\n[[stack.catalog]]\nid='item-hd'\ntitle='Synthetic Show'\naliases=['Synthetic']\nlibrary_id='library'\nlibrary_name='Series'\nresolution='hd'\nseason=1\nepisode=2\n[[stack.catalog]]\nid='item-uhd'\ntitle='Synthetic Film'\nlibrary_id='library-uhd'\nlibrary_name='Films'\nresolution='uhd'\n"
}

#[test]
fn web_maps_configured_values_and_both_resolutions() {
    let source = format!(
        "version=1\n[plex]\nurl='https://plex.invalid'\nowner_token={{env='UNRESOLVED'}}\nserver_id='server'\n[stack]\nintake_database='custom.sqlite'\nweb_listen='[::1]:7411'\nweb_public_origin='https://viewer.invalid'\nplex_auth_url='https://auth.invalid'\nplex_client_id='custom-client'{}",
        catalog()
    );
    let config = Config::parse(&source).unwrap();
    let web = web_config(&config).unwrap();
    assert_eq!(web.catalog_snapshot_file, None);
    assert_eq!(web.database, "custom.sqlite");
    assert_eq!(web.bind.to_string(), "[::1]:7411");
    assert_eq!(web.public_origin, "https://viewer.invalid");
    assert_eq!(web.plex_auth_url, "https://auth.invalid");
    assert_eq!(web.plex_server_url, "https://plex.invalid");
    assert_eq!(web.plex_server_id, "server");
    assert_eq!(web.client_id, "custom-client");
    assert_eq!(
        web.catalog,
        vec![
            CatalogItem {
                id: "item-hd".into(),
                title: "Synthetic Show".into(),
                aliases: vec!["Synthetic".into()],
                library_id: "library".into(),
                library_name: "Series".into(),
                resolution: Resolution::Hd,
                season: Some(1),
                episode: Some(2)
            },
            CatalogItem {
                id: "item-uhd".into(),
                title: "Synthetic Film".into(),
                aliases: vec![],
                library_id: "library-uhd".into(),
                library_name: "Films".into(),
                resolution: Resolution::Uhd,
                season: None,
                episode: None
            }
        ]
    );
}

#[tokio::test]
async fn bridge_disabled_does_not_open_intake_and_options_use_configured_policy() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("intake.sqlite");
    let source = format!(
        "version=1\n[stack]\nintake_database={:?}\nevidence_max_age_seconds=10\n[budgets]\nglobal_hour=7\nglobal_day=17\nviewer_hour=3\nviewer_day=9\nper_job=1\nitem_cooldown_seconds=101",
        path.to_str().unwrap()
    );
    let config = Config::parse(&source).unwrap();
    let store = Store::open(temp.path().join("engine")).await.unwrap();
    assert!(bridge(&config, store.clone()).await.unwrap().is_none());
    assert!(!path.exists());
    let options = bridge_options(&config);
    assert_eq!(
        options.policy,
        pitcrew_domain::Policy {
            max_grabs: 1,
            stage_timeout: 180,
            total_timeout: 3600
        }
    );
    assert_eq!(
        options.timeouts,
        pitcrew_domain::StageTimeouts {
            download: 180,
            import: 120,
            decode: 180,
            verify_retry: 180
        }
    );
    assert_eq!(
        (
            options.budgets.global_hour,
            options.budgets.global_day,
            options.budgets.viewer_hour,
            options.budgets.viewer_day
        ),
        (7, 17, 3, 9)
    );
    assert_eq!(options.cooldown_seconds, 101);
    assert_eq!(options.evidence_max_age, 10);
    let config = Config::parse(&source.replace("[stack]", "[stack]\nenabled=true")).unwrap();
    let enabled = bridge(&config, store.clone()).await.unwrap().unwrap();
    assert!(path.exists());
    assert_eq!(enabled.tick(1).await.unwrap().examined, 0);
    let invalid = Config::parse(
        &source
            .replace("[stack]", "[stack]\nenabled=true")
            .replace("intake.sqlite", "missing/intake.sqlite"),
    )
    .unwrap();
    assert!(bridge(&invalid, store).await.is_err());
}

#[tokio::test]
async fn actual_shadow_process_consumes_diagnoses_without_external_connections() {
    let temp = tempfile::tempdir().unwrap();
    let intake_path = temp.path().join("intake.sqlite");
    let intake = pitcrew_intake_store::Store::open(&intake_path)
        .await
        .unwrap();
    intake
        .persist_intake(
            "test",
            "message",
            "viewer",
            "conversation",
            &serde_json::json!({}),
            1,
        )
        .await
        .unwrap();
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(SqliteConnectOptions::new().filename(&intake_path))
        .await
        .unwrap();
    // A malformed synthetic diagnosis must still become visible as review.
    // Written straight into the intake store's real `diagnoses` table, bypassing
    // its typed writer so the row can be malformed on purpose.
    sqlx::raw_sql("INSERT INTO diagnoses(report_id,decided_at,decider_kind,decision,evidence_summary,resource,library_id,arr_instance_id,arr_item_id,arr_file_id,agent_file,agent_observed_at,access_observed_at,access_expires_at) VALUES(1,1,'rules','invalid','{}','synthetic','1','tv',1,1,'{}',NULL,1,2);").execute(&pool).await.unwrap();
    let config = temp.path().join("config.toml");
    std::fs::write(&config,format!("version=1\ndatabase={:?}\n[engine]\nlisten='127.0.0.1:0'\npoll_seconds=1\n[stack]\nenabled=true\nintake_database={:?}",temp.path().join("engine.sqlite").to_str().unwrap(),intake_path.to_str().unwrap())).unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_pitcrew"))
        .args(["--config"])
        .arg(config)
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let mut lines = BufReader::new(stderr).lines();
    let started = tokio::time::timeout(Duration::from_secs(20), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let started: serde_json::Value = serde_json::from_str(&started).unwrap();
    assert_eq!(started["bridge_enabled"], true);
    assert_eq!(started["poll_enabled"], false);
    assert_eq!(started["effects_enabled"], false);
    let tick = tokio::time::timeout(Duration::from_secs(20), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let tick: serde_json::Value = serde_json::from_str(&tick).unwrap();
    assert_eq!(tick["event"], "bridge_tick");
    assert_eq!(tick["summary"]["held"], 1);
    let row = sqlx::query("SELECT state,job_id FROM report_progress WHERE report_id=1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("state"), "needs_review");
    assert_eq!(row.get::<Option<i64>, _>("job_id"), None);
    sqlx::query("DELETE FROM diagnoses")
        .execute(&pool)
        .await
        .unwrap();
    let idle = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let line = lines.next_line().await.unwrap().unwrap();
            let event: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(event["event"], "bridge_tick");
            if event["summary"]["examined"] == 0 {
                break event;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        idle["summary"],
        serde_json::json!({"examined":0,"linked":0,"held":0,"projected":0})
    );
    child.kill().await.unwrap();
    child.wait().await.unwrap();
}

#[tokio::test]
async fn cli_usage_and_offline_web_validation_are_explicit() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("stack.toml");
    let source = "version=1\n[plex]\nurl='https://plex.invalid'\nowner_token={env='UNRESOLVED'}\nserver_id='server'";
    std::fs::write(&config, source).unwrap();
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_pitcrew"))
            .current_dir(temp.path())
            .args(["web", "--config"])
            .arg(&config)
            .arg("--check")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
    for args in [
        vec!["doctor"],
        vec!["doctor", "--config"],
        vec!["doctor", "--wrong", "missing"],
        vec!["doctor", "--wrong", "missing", "--connectivity"],
        vec!["doctor", "--config", "missing", "--bad"],
        vec!["web"],
        vec!["web", "--config"],
        vec!["web", "--wrong", "missing"],
        vec!["web", "--wrong", "missing", "--check"],
        vec!["web", "--config", "missing", "--bad"],
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_pitcrew"))
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("usage:"));
    }
}

#[tokio::test]
async fn actual_viewer_launcher_serves_the_web_app_on_its_owned_socket() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("stack.toml");
    let database = temp.path().join("intake.sqlite");
    std::fs::write(&config,format!("version=1\n[plex]\nurl='https://plex.invalid'\nowner_token={{env='UNRESOLVED'}}\nserver_id='synthetic'\n[stack]\nweb_listen='127.0.0.1:0'\nintake_database={:?}",database.to_str().unwrap())).unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_pitcrew"))
        .args(["web", "--config"])
        .arg(config)
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let line = tokio::time::timeout(Duration::from_secs(20), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let started: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(started["event"], "pitcrew_web_started");
    let address: std::net::SocketAddr = started["listen"].as_str().unwrap().parse().unwrap();
    assert!(address.ip().is_loopback());
    assert_ne!(address.port(), 0);
    let response = tokio::time::timeout(Duration::from_secs(10), async {
        let mut connection = tokio::net::TcpStream::connect(address).await.unwrap();
        connection
            .write_all(b"GET / HTTP/1.1\r\nHost: local.test\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        connection.read_to_string(&mut response).await.unwrap();
        response
    })
    .await
    .unwrap();
    assert!(response.starts_with("HTTP/1.1 200"));
    assert!(response.contains("Sign in"));
    assert!(database.exists());
    child.kill().await.unwrap();
    child.wait().await.unwrap();
}
