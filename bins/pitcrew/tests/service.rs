use pitcrew::App;
use pitcrew_engine::webhook::Secret;
use pitcrew_store::{BudgetLimits, Store};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
const SECRET: &str = "synthetic-test-secret-0123456789abcdef";
async fn app(path: &std::path::Path) -> Arc<App> {
    Arc::new(
        App::new(
            Store::open(path).await.unwrap(),
            vec![("movies".into(), Secret::new(SECRET).unwrap())],
            Some(Secret::new(SECRET).unwrap()),
            BudgetLimits {
                global_hour: 10,
                global_day: 20,
                viewer_hour: 4,
                viewer_day: 8,
            },
            2,
            BTreeMap::new(),
        )
        .unwrap(),
    )
}
async fn request(app: Arc<App>, raw: Vec<u8>) -> (u16, Value) {
    let (mut client, server) = tokio::io::duplex(80_000);
    let task = tokio::spawn(pitcrew::connection(app, server, 100));
    client.write_all(&raw).await.unwrap();
    client.shutdown().await.unwrap();
    let mut response = vec![];
    client.read_to_end(&mut response).await.unwrap();
    task.await.unwrap().unwrap();
    let response = String::from_utf8(response).unwrap();
    let (headers, body) = response.split_once("\r\n\r\n").unwrap();
    (
        headers.split_whitespace().nth(1).unwrap().parse().unwrap(),
        serde_json::from_str(body).unwrap(),
    )
}
fn post(path: &str, body: &Value, auth: &str) -> Vec<u8> {
    let body = serde_json::to_vec(body).unwrap();
    let mut raw = format!("POST {path} HTTP/1.1\r\nHost: local.test\r\nAuthorization: {auth}\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
    raw.extend(body);
    raw
}
#[tokio::test]
async fn webhook_basic_bearer_replay_and_restart_projection() {
    use base64::Engine as _;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let service = app(&path).await;
    assert_eq!(
        request(
            service.clone(),
            b"GET /healthz HTTP/1.1\r\nHost: local.test\r\n\r\n".to_vec()
        )
        .await
        .0,
        200
    );
    let body = json!({"eventType":"Download","downloadId":"synthetic-download","movie":{"id":8},"movieFile":{"id":9},"path":"PRIVATE_PATH_NOT_STORED"});
    assert_eq!(
        request(
            service.clone(),
            post("/v1/webhooks/movies", &body, "Bearer wrong")
        )
        .await
        .0,
        401
    );
    let basic = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("pitcrew:{SECRET}"))
    );
    assert_eq!(
        request(service.clone(), post("/v1/webhooks/movies", &body, &basic)).await,
        (202, json!({"accepted":true,"duplicate":false}))
    );
    assert_eq!(
        request(
            service.clone(),
            post("/v1/webhooks/movies", &body, &format!("Bearer {SECRET}"))
        )
        .await
        .1["duplicate"],
        true
    );
    service.project(0).await.unwrap();
    service.project(0).await.unwrap();
    assert_eq!(service.reports().await.unwrap().len(), 1);
    let reopened = app(&path).await;
    reopened.project(0).await.unwrap();
    let reports = reopened.reports().await.unwrap();
    assert_eq!(reports.len(), 1);
    assert!(reports[0].missing_evidence);
    let records = reopened.store.observations().await.unwrap();
    assert!(
        !serde_json::to_string(&records)
            .unwrap()
            .contains("PRIVATE_PATH")
    );
}
#[tokio::test]
async fn malformed_http_framing_and_credentials_fail_closed() {
    let temp = tempfile::tempdir().unwrap();
    let service = app(&temp.path().join("db")).await;
    for raw in [
        "POST /v1/webhooks/movies HTTP/1.1\r\nContent-Length: 0\r\nContent-Length: 0\r\n\r\n",
        "POST /v1/webhooks/movies HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n",
        "GET /healthz HTTP/1.1\r\n\r\nGET /healthz HTTP/1.1\r\n\r\n",
    ] {
        assert_eq!(
            request(service.clone(), raw.as_bytes().to_vec()).await.0,
            400
        );
    }
    for auth in [
        "Basic !!!!",
        "Basic d3Jvbmc6dXNlcg==",
        "Bearer wrong",
        "Unknown secret",
    ] {
        assert_eq!(
            request(
                service.clone(),
                post("/v1/webhooks/movies", &json!({"eventType":"Test"}), auth)
            )
            .await
            .0,
            401
        );
    }
    assert_eq!(
        request(
            service.clone(),
            b"GET /healthz HTTP/1.1\r\nContent-Length: 999999\r\n\r\n".to_vec()
        )
        .await
        .0,
        413
    );
}

#[tokio::test]
async fn authenticated_recorded_fix_produces_persisted_would_do_diffs() {
    let temp = tempfile::tempdir().unwrap();
    let service = app(&temp.path().join("db")).await;
    let initial = json!({"id":1,"viewer":"opaque-viewer","resource":"opaque-media","library":2,"original":{"id":3,"revision":1},"constraints":{"language":null,"require_sdr":false},"policy":{"max_grabs":2,"stage_timeout":30,"total_timeout":300},"timeouts":{"download":60,"import":60,"decode":60,"verify_retry":30},"created_at":0});
    let replay = json!({"key":"fix-example","job":initial,"events":[{"key":"select","at":1,"event":{"SelectRelease":{"release":4,"access":{"Granted":{"library":2,"expires_at":100}}}}}],"observed":{"1":{"action":"grab","actor":"python","correlated":true,"safety":{"access":true,"budget":true,"reconciled":true}}}});
    let mut invalid = replay.clone();
    invalid["job"]["resource"] = json!("");
    assert_eq!(
        service
            .handle(
                "POST",
                "/v1/shadow/replay",
                SECRET,
                &serde_json::to_vec(&invalid).unwrap(),
                100
            )
            .await
            .0,
        409
    );
    assert!(service.store.observations().await.unwrap().is_empty());
    let unauthorized = service
        .handle(
            "POST",
            "/v1/shadow/replay",
            "wrong",
            &serde_json::to_vec(&replay).unwrap(),
            100,
        )
        .await;
    assert_eq!(unauthorized.0, 401);
    let (status, result) = request(
        service.clone(),
        post("/v1/shadow/replay", &replay, &format!("Bearer {SECRET}")),
    )
    .await;
    assert_eq!(status, 200, "{result}");
    assert_eq!(result["reports"][0]["agreement"], "match");
    assert_eq!(
        service.store.outbox(1).await.unwrap()[0].status,
        pitcrew_store::OutboxStatus::Shadowed
    );
    assert_eq!(service.reports().await.unwrap().len(), 1);
}

#[tokio::test]
async fn actual_binary_local_fake_webhook_then_export() {
    use tokio::io::AsyncBufReadExt;
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config.toml");
    let secret = temp.path().join("secret");
    std::fs::write(&secret, SECRET).unwrap();
    std::fs::write(
        &config,
        format!(
            r#"version=1
database={db:?}
[engine]
listen="127.0.0.1:0"
control_secret={{file={secret:?}}}
[[arr]]
id="movies"
kind="radarr"
url="http://unused.example.invalid"
api_key={{env="INTENTIONALLY_UNRESOLVED"}}
webhook_secret={{file={secret:?}}}
libraries=["movies"]
"#,
            db = temp.path().join("db"),
            secret = secret
        ),
    )
    .unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_pitcrew"))
        .arg("--config")
        .arg(&config)
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = tokio::io::BufReader::new(child.stderr.take().unwrap()).lines();
    let line = tokio::time::timeout(std::time::Duration::from_secs(15), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let startup: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(startup["mode"], "shadow");
    assert_eq!(startup["effects_enabled"], false);
    let address = startup["listen"].as_str().unwrap();
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    stream
        .write_all(&post(
            "/v1/webhooks/movies",
            &json!({"eventType":"Grab","downloadId":"fake-dl","movie":{"id":3}}),
            &format!("Bearer {SECRET}"),
        ))
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    assert!(response.starts_with("HTTP/1.1 202"));
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    let export = temp.path().join("diff.jsonl");
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_pitcrew"))
        .arg("--config")
        .arg(&config)
        .arg("--export")
        .arg(&export)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reports = std::fs::read_to_string(&export).unwrap();
    let report: Value = serde_json::from_str(reports.lines().next().unwrap()).unwrap();
    assert_eq!(report["observed"]["action"], "grab");
    assert_eq!(report["proposed"]["action"], "hold");
    assert_eq!(report["agreement"], "insufficient_evidence");
    assert!(
        std::fs::read_to_string(format!("{}.summary.txt", export.display()))
            .unwrap()
            .contains("1 comparisons")
    );
}

#[tokio::test]
async fn actual_binary_poll_fallback_reads_only_owned_fake_and_exports_arbiter() {
    use tokio::io::AsyncBufReadExt;
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = upstream.local_addr().unwrap();
    let fake = tokio::spawn(async move {
        let mut routes = vec![];
        for _ in 0..3 {
            let (mut stream, _) = upstream.accept().await.unwrap();
            let mut raw = vec![];
            loop {
                let mut part = [0; 4096];
                let n = stream.read(&mut part).await.unwrap();
                assert!(n > 0);
                raw.extend_from_slice(&part[..n]);
                if raw.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let raw = String::from_utf8(raw).unwrap();
            assert!(raw.starts_with("GET "), "poll attempted a write");
            let route = raw.split_whitespace().nth(1).unwrap().to_owned();
            let body = if route.starts_with("/api/v3/history?") {
                json!({"page":1,"pageSize":100,"totalRecords":1,"records":[{"id":1,"eventType":"grabbed","date":"2026-01-01T00:00:00Z","downloadId":"synthetic-dl","movieId":4}]})
            } else if route.starts_with("/api/v3/queue?") {
                json!({"page":1,"pageSize":100,"totalRecords":1,"records":[{"id":5,"downloadId":"synthetic-dl","movieId":4,"trackedDownloadState":"importBlocked"}]})
            } else if route.starts_with("/api/v3/manualimport?") {
                json!([])
            } else {
                panic!("unexpected route {route}")
            };
            let bytes = serde_json::to_vec(&body).unwrap();
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                bytes.len()
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            stream.write_all(&bytes).await.unwrap();
            stream.shutdown().await.unwrap();
            routes.push(route);
        }
        routes
    });
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config.toml");
    let secret = temp.path().join("secret");
    std::fs::write(&secret, SECRET).unwrap();
    std::fs::write(
        &config,
        format!(
            r#"version=1
database={db:?}
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
            db = temp.path().join("db"),
            secret = secret
        ),
    )
    .unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_pitcrew"))
        .arg("--config")
        .arg(&config)
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = tokio::io::BufReader::new(child.stderr.take().unwrap()).lines();
    let line = tokio::time::timeout(std::time::Duration::from_secs(15), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(line.contains("pitcrew_started"), "{line}");
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(10), fake)
            .await
            .unwrap()
            .unwrap()
            .len(),
        3
    );
    let store = Store::open(temp.path().join("db")).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if store
                .observations()
                .await
                .unwrap()
                .iter()
                .any(|r| r.source == "arbiter:movies")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    let output = temp.path().join("reports.jsonl");
    let exported = tokio::process::Command::new(env!("CARGO_BIN_EXE_pitcrew"))
        .arg("--config")
        .arg(&config)
        .arg("--export")
        .arg(&output)
        .output()
        .await
        .unwrap();
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    let arbiter = std::fs::read_to_string(format!("{}.arbiter.jsonl", output.display())).unwrap();
    assert!(arbiter.contains("AccessUnknown"));
    assert!(arbiter.contains("Hold"));
    assert!(std::fs::read_to_string(&output).unwrap().contains("grab"));
}

#[tokio::test]
async fn authenticated_legacy_import_observation_is_diffed_without_execution() {
    let temp = tempfile::tempdir().unwrap();
    let service = app(&temp.path().join("db")).await;
    let input = json!({"key":"import-observation-1","observation":{
        "job":1,"library":2,"download":3,"owned_download":3,"observed_at":1,"kind":"Movie","import_blocked":true,
        "access":{"Granted":{"library":2,"expires_at":100}},"source":{"id":4,"revision":1},"decoded_source":{"id":4,"revision":1},
        "facts":{"title":"Exact","year_matches":true,"language_matches":true,"episodes_match":null,"no_rejections":true,"no_samples":true,"existing_file":false,"video_files":1},
        "external_failure":null,"legacy_action":"Import"
    }});
    let raw = serde_json::to_vec(&input).unwrap();
    assert_eq!(
        service
            .handle("POST", "/v1/shadow/imports", "wrong", &raw, 1)
            .await
            .0,
        401
    );
    let result = service
        .handle("POST", "/v1/shadow/imports", SECRET, &raw, 1)
        .await;
    assert_eq!(result.0, 200);
    assert_eq!(result.1["duplicate"], false);
    assert!(result.1.to_string().contains("VerifiedProposal"));
    assert_eq!(
        service
            .handle("POST", "/v1/shadow/imports", SECRET, &raw, 2)
            .await
            .1["duplicate"],
        true
    );
    assert!(service.store.jobs().await.unwrap().is_empty());
}
