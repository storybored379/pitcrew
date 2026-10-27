use pitcrew::{App, Replay};
use pitcrew_engine::{
    observer::{Action, Actor},
    webhook::{MAX_BODY, Secret},
};
use pitcrew_store::{BudgetLimits, Store};
use serde_json::{Value, json};
use sqlx::{Connection, Executor, SqliteConnection, sqlite::SqliteConnectOptions};
use std::{collections::BTreeMap, sync::Arc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const SECRET: &str = "synthetic-runtime-secret-0123456789abcdef";
async fn app() -> (tempfile::TempDir, Arc<App>) {
    let dir = tempfile::tempdir().unwrap();
    let app = App::new(
        Store::open(dir.path().join("db")).await.unwrap(),
        vec![],
        Some(Secret::new(SECRET).unwrap()),
        BudgetLimits {
            global_hour: 100,
            global_day: 100,
            viewer_hour: 100,
            viewer_day: 100,
        },
        2,
        BTreeMap::new(),
    )
    .unwrap();
    (dir, Arc::new(app))
}
fn replay() -> Value {
    json!({"key":"boundary-replay","job":{"id":1,"viewer":"viewer","resource":"resource","library":2,"original":{"id":3,"revision":1},"constraints":{"language":null,"require_sdr":false},"policy":{"max_grabs":2,"stage_timeout":3000,"total_timeout":5000},"timeouts":{"download":3000,"import":3000,"decode":3000,"verify_retry":30},"created_at":0},"events":[{"key":"tick-0","at":1,"event":"Tick"}]})
}
fn import(key: &str) -> Value {
    json!({"key":key,"observation":{"job":1,"library":2,"download":3,"owned_download":null,"observed_at":1,"kind":"Movie","import_blocked":null,"access":"Unknown","source":null,"decoded_source":null,"facts":{"title":"Unknown"},"external_failure":null,"legacy_action":"Unknown"}})
}
async fn request(app: Arc<App>, raw: &[u8]) -> (u16, Value) {
    let (mut client, server) = tokio::io::duplex(MAX_BODY * 2);
    let worker = tokio::spawn(pitcrew::connection(app, server, 100));
    client.write_all(raw).await.unwrap();
    client.shutdown().await.unwrap();
    let mut response = String::new();
    client.read_to_string(&mut response).await.unwrap();
    worker.await.unwrap().unwrap();
    let (headers, body) = response.split_once("\r\n\r\n").unwrap();
    (
        headers.split_whitespace().nth(1).unwrap().parse().unwrap(),
        serde_json::from_str(body).unwrap(),
    )
}

#[tokio::test]
async fn routes_methods_and_json_payload_limits_are_exact() {
    let (_dir, app) = app().await;
    for (method, path, status) in [
        ("POST", "/healthz", 404),
        ("GET", "/other", 404),
        ("GET", "/v1/webhooks/movies", 405),
        ("GET", "/v1/shadow/imports", 404),
        ("GET", "/v1/shadow/replay", 404),
        ("POST", "/v1/shadow/unknown", 404),
    ] {
        assert_eq!(
            app.handle(method, path, SECRET, b"{}", 1).await.0,
            status,
            "{method} {path}"
        );
    }
    for path in ["/v1/shadow/imports", "/v1/shadow/replay"] {
        assert_eq!(app.handle("POST", path, SECRET, b"invalid", 1).await.0, 400);
        let mut input = if path.ends_with("imports") {
            serde_json::to_vec(&import("first")).unwrap()
        } else {
            serde_json::to_vec(&replay()).unwrap()
        };
        input.resize(MAX_BODY, b' ');
        assert_eq!(
            app.handle("POST", path, SECRET, &input, 1).await.0,
            200,
            "{path}"
        );
        input.push(b' ');
        assert_eq!(app.handle("POST", path, SECRET, &input, 1).await.0, 413);
    }
    for (len, status) in [(0, 400), (256, 200), (257, 400)] {
        assert_eq!(
            app.handle(
                "POST",
                "/v1/shadow/imports",
                SECRET,
                &serde_json::to_vec(&import(&"k".repeat(len))).unwrap(),
                1
            )
            .await
            .0,
            status
        );
    }
}

#[tokio::test]
async fn replay_validates_every_limit_before_publishing_or_leasing() {
    for case in 0..6 {
        let (_dir, app) = app().await;
        let mut input = replay();
        match case {
            0 => input["key"] = json!(""),
            1 => input["events"] = json!([]),
            2 => {
                input["events"] = json!(
                    (0..513)
                        .map(|i| json!({"key":format!("tick-{i}"),"at":i+1,"event":"Tick"}))
                        .collect::<Vec<_>>()
                )
            }
            3 => input["job"]["policy"]["max_grabs"] = json!(3),
            4 => input["events"][0]["key"] = json!(""),
            5 => {
                let duplicate = input["events"][0].clone();
                input["events"].as_array_mut().unwrap().push(duplicate);
            }
            _ => unreachable!(),
        }
        assert!(
            app.replay(serde_json::from_value(input).unwrap())
                .await
                .is_err(),
            "case {case}"
        );
        assert!(app.store.jobs().await.unwrap().is_empty(), "case {case}");
        assert!(
            app.store.observations().await.unwrap().is_empty(),
            "case {case}"
        );
    }
    let (_dir, app) = app().await;
    let mut input = replay();
    input["events"] = json!(
        (0..512)
            .map(|i| json!({"key":format!("tick-{i}"),"at":i+1,"event":"Tick"}))
            .collect::<Vec<_>>()
    );
    // A valid maximal transcript reaches store validation. Avoid journaling
    // hundreds of no-op events just to test the admission boundary.
    input["job"]["resource"] = json!("");
    assert_eq!(
        app.replay(serde_json::from_value::<Replay>(input).unwrap())
            .await
            .unwrap_err(),
        "job_conflict_or_budget"
    );
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!(
        app.replay(serde_json::from_value::<Replay>(replay()).unwrap())
            .await
            .is_ok()
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    for row in app.store.observations().await.unwrap() {
        assert!(row.at >= started && row.at <= now);
    }
}

#[tokio::test]
async fn observed_history_actions_and_unknown_activity_are_preserved() {
    let (_dir, app) = app().await;
    for (i, kind) in [
        "Grab",
        "Download",
        "EpisodeFileDelete",
        "MovieFileDelete",
        "DownloadFailed",
    ]
    .into_iter()
    .enumerate()
    {
        app.store
            .ingest_observation(
                "history:movies",
                &i.to_string(),
                &json!({"kind":kind,"download":null,"media":[],"files":[]}),
                10,
            )
            .await
            .unwrap();
    }
    app.project(0).await.unwrap();
    let reports = app.reports().await.unwrap();
    assert_eq!(
        reports
            .iter()
            .map(|r| r.observed.action)
            .collect::<Vec<_>>(),
        [
            Action::Grab,
            Action::Import,
            Action::DeleteLibrary,
            Action::DeleteLibrary,
            Action::Hold
        ]
    );
    for report in reports {
        assert_eq!(report.observed.actor, Actor::Unknown);
        assert!(!report.observed.correlated);
        assert_eq!(report.proposed.action, Action::Hold);
    }
}

#[tokio::test]
async fn http_rejects_duplicate_auth_and_handles_fragmented_valid_headers() {
    let (_dir, app) = app().await;
    let raw = format!(
        "POST /v1/shadow/imports HTTP/1.1\r\nAuthorization: Bearer {SECRET}\r\nAuthorization: Bearer {SECRET}\r\nContent-Length: 0\r\n\r\n"
    );
    assert_eq!(request(app.clone(), raw.as_bytes()).await.0, 401);
    for raw in [
        b"GET /healthz HTTP/1.1\r\nContent-Length: invalid\r\n\r\n".as_slice(),
        b"GET /healthz HTTP/1.1\r\nContent-Length: 2\r\n\r\nx",
        b"invalid\r\n\r\n",
    ] {
        assert!(matches!(request(app.clone(), raw).await.0, 400 | 413));
    }
    let (mut client, server) = tokio::io::duplex(MAX_BODY * 2);
    let task = tokio::spawn(pitcrew::connection(app, server, 100));
    client
        .write_all(b"GET /healthz HTTP/1.1\r\nX-Test: ")
        .await
        .unwrap();
    tokio::task::yield_now().await;
    client.write_all(&vec![b'a'; 5000]).await.unwrap();
    tokio::task::yield_now().await;
    client.write_all(b"\r\n\r\n").await.unwrap();
    client.shutdown().await.unwrap();
    let mut response = String::new();
    client.read_to_string(&mut response).await.unwrap();
    task.await.unwrap().unwrap();
    assert!(response.starts_with("HTTP/1.1 200"));
}

#[tokio::test]
async fn cli_accepts_help_and_rejects_each_malformed_switch_before_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            r#"version=1
database={:?}
[engine]
listen="127.0.0.1:0"
[[arr]]
id="movies"
kind="radarr"
url="http://unused.example.invalid"
api_key={{env="INTENTIONALLY_UNRESOLVED"}}
libraries=["movies"]
"#,
            dir.path().join("db")
        ),
    )
    .unwrap();
    let cfg = config.to_str().unwrap();
    for args in [
        vec!["--typo", cfg],
        vec!["--typo", cfg, "--export", "unused"],
        vec!["--config", cfg, "--typo", "unused"],
        vec![
            "--typo",
            cfg,
            "--import-python",
            "unused",
            "--source-key",
            "source",
        ],
        vec![
            "--config",
            cfg,
            "--typo",
            "unused",
            "--source-key",
            "source",
        ],
        vec![
            "--config",
            cfg,
            "--import-python",
            "unused",
            "--typo",
            "source",
        ],
        vec![],
    ] {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            tokio::process::Command::new(env!("CARGO_BIN_EXE_pitcrew"))
                .args(args)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("usage:"));
    }
    let help = tokio::process::Command::new(env!("CARGO_BIN_EXE_pitcrew"))
        .arg("--help")
        .output()
        .await
        .unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("--import-legacy"));
}

#[tokio::test]
async fn http_header_body_and_combined_limits_accept_exact_boundaries() {
    let (_dir, app) = app().await;
    fn header(length: usize, body_len: usize, complete: bool) -> Vec<u8> {
        let mut raw = format!("GET /healthz HTTP/1.1\r\nContent-Length: {body_len}\r\nX-Padding: ")
            .into_bytes();
        let end = if complete {
            b"\r\n\r\n".as_slice()
        } else {
            b""
        };
        raw.resize(length - end.len(), b'a');
        raw.extend_from_slice(end);
        raw
    }
    let mut exact = header(8192, MAX_BODY, true);
    exact.resize(8192 + MAX_BODY, b'x');
    assert_eq!(request(app.clone(), &exact).await.0, 200);
    // Start with a short read so the final read crosses the combined cap,
    // rather than completing at an aligned frame boundary first.
    let mut oversized = exact;
    oversized.push(b'x');
    let fragmented = oversized[..1].chain(&oversized[1..]);
    let mut output = Vec::new();
    let stream = tokio::io::join(fragmented, &mut output);
    pitcrew::connection(app.clone(), stream, 100).await.unwrap();
    let response = String::from_utf8(output).unwrap();
    assert!(response.starts_with("HTTP/1.1 413 "));
    assert_eq!(
        serde_json::from_str::<Value>(response.split_once("\r\n\r\n").unwrap().1).unwrap(),
        json!({"error":"payload_too_large"})
    );
    assert_eq!(
        request(app.clone(), &header(8192, MAX_BODY + 1, true)).await,
        (413, json!({"error":"invalid_length"}))
    );
    assert_eq!(
        request(app.clone(), &header(8193, 0, true)).await,
        (400, json!({"error":"invalid_headers"}))
    );
    assert_eq!(
        request(app.clone(), &header(8193, 0, false)).await,
        (400, json!({"error":"invalid_headers"}))
    );
    assert_eq!(
        request(app, &header(8192, 0, false)).await,
        (400, json!({"error":"incomplete_request"}))
    );
}

#[tokio::test]
async fn cli_export_projects_every_page_and_counts_arbiter_observations() {
    let (dir, app) = app().await;
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(dir.path().join("db")),
    )
    .await
    .unwrap();
    db.execute("WITH RECURSIVE rows(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM rows WHERE n<1000) INSERT INTO observations(source,observation_key,payload,at) SELECT 'unrelated',CAST(n AS TEXT),'{}',1 FROM rows")
        .await
        .unwrap();
    db.close().await.unwrap();
    app.store
        .ingest_observation(
            "history:movies",
            "late-history",
            &json!({"kind":"Grab","download":null,"media":[],"files":[]}),
            2,
        )
        .await
        .unwrap();
    app.store
        .ingest_observation(
            "arbiter:movies",
            "late-arbiter",
            &json!({"decision":"Hold"}),
            3,
        )
        .await
        .unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            r#"version=1
database={:?}
[engine]
listen="127.0.0.1:0"
poll_enabled=true
[[arr]]
id="movies"
kind="radarr"
url="http://unused.example.invalid"
api_key={{env="INTENTIONALLY_UNRESOLVED"}}
libraries=["movies"]
"#,
            dir.path().join("db")
        ),
    )
    .unwrap();
    let output = dir.path().join("reports.jsonl");
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
    let reports = std::fs::read_to_string(&output).unwrap();
    assert_eq!(reports.lines().count(), 1);
    let report: Value = serde_json::from_str(reports.lines().next().unwrap()).unwrap();
    assert_eq!(report["observed"]["action"], "grab");
    let arbiter = std::fs::read_to_string(output.with_extension("jsonl.arbiter.jsonl")).unwrap();
    assert_eq!(arbiter.lines().count(), 1);
    let observation: Value = serde_json::from_str(arbiter.lines().next().unwrap()).unwrap();
    assert_eq!(observation["payload"]["decision"], "Hold");
    let summary = std::fs::read_to_string(output.with_extension("jsonl.summary.txt")).unwrap();
    assert!(summary.contains("1 comparisons"));
    assert!(summary.ends_with(" 1 arbiter observations exported separately.\n"));
}

#[tokio::test]
async fn cli_failed_poll_emits_an_incomplete_diagnostic() {
    use tokio::io::AsyncBufReadExt;
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = upstream.local_addr().unwrap();
    let fake = tokio::spawn(async move {
        let (mut stream, _) = upstream.accept().await.unwrap();
        let mut raw = Vec::new();
        loop {
            let mut part = [0; 4096];
            let read = stream.read(&mut part).await.unwrap();
            assert!(read > 0);
            raw.extend_from_slice(&part[..read]);
            if raw.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                break;
            }
        }
        assert!(raw.starts_with(b"GET /api/v3/history?"));
        stream
            .write_all(b"HTTP/1.1 500 Error\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
            .await
            .unwrap();
        stream.shutdown().await.unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("secret");
    std::fs::write(&secret, SECRET).unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            r#"version=1
database={:?}
[engine]
listen="127.0.0.1:0"
poll_enabled=true
poll_seconds=3600
[[arr]]
id="movies"
kind="radarr"
url="http://{address}"
api_key={{file={secret:?}}}
libraries=["movies"]
"#,
            dir.path().join("db")
        ),
    )
    .unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_pitcrew"))
        .arg("--config")
        .arg(config)
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = tokio::io::BufReader::new(child.stderr.take().unwrap()).lines();
    let started = tokio::time::timeout(std::time::Duration::from_secs(15), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&started).unwrap()["event"],
        "pitcrew_started"
    );
    let diagnostic = tokio::time::timeout(std::time::Duration::from_secs(15), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&diagnostic).unwrap(),
        json!({"event":"poll_incomplete","instance":"movies"})
    );
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    fake.await.unwrap();
}
