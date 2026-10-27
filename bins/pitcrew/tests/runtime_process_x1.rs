//! Exercise the executable's enabled runtime against an owned loopback fake.
//! These tests never depend on a running media stack or contact an external host.
use pitcrew_store::Store;
use serde_json::{Value, json};
use std::{
    net::{Ipv4Addr, SocketAddr},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
async fn enabled_runtime_repeats_worker_ticks_without_resyncing_a_healthy_catalog() {
    run_runtime(true).await;
}

#[tokio::test]
async fn enabled_runtime_records_failed_catalog_sync_and_keeps_ticking() {
    run_runtime(false).await;
}

async fn run_runtime(healthy: bool) {
    let upstream = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = upstream.local_addr().unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let requests = calls.clone();
    let fake = tokio::spawn(async move {
        loop {
            let (mut stream, _) = upstream.accept().await.unwrap();
            let mut raw = vec![];
            loop {
                let mut part = [0; 4096];
                let n = stream.read(&mut part).await.unwrap();
                assert!(n > 0, "incomplete fake request");
                raw.extend_from_slice(&part[..n]);
                assert!(raw.len() <= 16 * 1024, "unbounded fake request");
                if raw.windows(4).any(|part| part == b"\r\n\r\n") {
                    break;
                }
            }
            let raw = String::from_utf8(raw).unwrap();
            assert!(raw.starts_with("GET "), "runtime attempted a write");
            let route = raw.split_whitespace().nth(1).unwrap().to_owned();
            let path = route.split('?').next().unwrap();
            let (status, body) = match (healthy, path) {
                (false, "/library/sections") => ("500 Internal Server Error", json!({})),
                (true, "/library/sections") => (
                    "200 OK",
                    json!({"MediaContainer":{"size":1,"Directory":[{"key":"1","title":"Synthetic Films","type":"movie"}]}}),
                ),
                (true, "/api/v3/movie") => ("200 OK", json!([])),
                (true, "/library/sections/1/all") => (
                    "200 OK",
                    json!({"MediaContainer":{"size":0,"offset":0,"totalSize":0,"Metadata":[]}}),
                ),
                _ => panic!("unexpected runtime request {route}"),
            };
            requests.lock().unwrap().push(path.to_owned());
            let mut bytes = serde_json::to_vec(&body).unwrap();
            if path == "/api/v3/movie" {
                // A real library's movie list is several MiB; the runtime must accept it.
                bytes = [b"[".as_slice(), &vec![b' '; 5 << 20], b"]"].concat();
            }
            let headers = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                bytes.len()
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            stream.write_all(&bytes).await.unwrap();
            stream.shutdown().await.unwrap();
        }
    });

    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config.toml");
    let database = temp.path().join("engine.sqlite");
    let intake = temp.path().join("intake.sqlite");
    let snapshot = intake.with_extension("catalog.json");
    let secret = temp.path().join("secret");
    let engine_listen = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
    std::fs::write(&secret, "synthetic-runtime-secret").unwrap();
    std::fs::write(
        &config,
        format!(
            r#"version=1
database={database:?}
[engine]
listen="{engine_listen}"
poll_enabled=false
poll_seconds=1
[stack]
enabled=true
intake_database={intake:?}
plex_auth_url="https://{address}/"
[runtime]
sync_seconds=3600
[plex]
url="http://{address}/"
owner_token={{file={secret:?}}}
server_id="synthetic"
[[arr]]
id="films"
kind="radarr"
url="http://{address}/"
api_key={{file={secret:?}}}
libraries=["1"]
"#
        ),
    )
    .unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_pitcrew"))
        .arg("--config")
        .arg(&config)
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let observed = tokio::time::timeout(Duration::from_secs(20), async {
        let mut events = Vec::new();
        let mut ticks = 0;
        while ticks < 3 {
            let Some(line) = lines.next_line().await.unwrap() else {
                break;
            };
            let event: Value = serde_json::from_str(&line)
                .unwrap_or_else(|error| panic!("invalid runtime log {line}: {error}"));
            if event["event"] == "runtime_tick" {
                ticks += 1;
            }
            let failed_tick = event["event"] == "runtime_tick_incomplete";
            events.push(event);
            if failed_tick {
                break;
            }
        }
        (ticks, events)
    })
    .await;
    // Always stop and reap the owned process before checking its observations.
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    fake.abort();
    let stopped = fake.await.unwrap_err();
    assert!(stopped.is_cancelled(), "fake server failed: {stopped}");
    let (ticks, events) = observed.expect("enabled runtime did not keep ticking");
    assert_eq!(ticks, 3, "runtime events: {events:?}");
    let startup = events.first().unwrap();
    assert_eq!(startup["event"], "pitcrew_started");
    assert_eq!(startup["runtime_enabled"], true);
    assert_eq!(startup["bridge_enabled"], true);
    assert_eq!(startup["effects_enabled"], false);
    assert_eq!(startup["mode"], "shadow");
    assert_eq!(startup["poll_enabled"], false);
    assert_eq!(
        events
            .iter()
            .filter(|event| event["event"] == "catalog_sync_incomplete")
            .count(),
        usize::from(!healthy),
        "runtime events: {events:?}"
    );
    // An operator must be told why, not just that, sync was incomplete.
    for event in events
        .iter()
        .filter(|event| event["event"] == "catalog_sync_incomplete")
    {
        assert!(
            event["reason"].as_str().is_some_and(|r| !r.is_empty()),
            "runtime events: {events:?}"
        );
    }
    for event in &events {
        assert_ne!(event["event"], "runtime_tick_incomplete");
        if event["event"] == "runtime_tick" {
            assert_eq!(
                event["summary"],
                json!({"reports_examined":0,"authority_recorded":0,"reports_held":0,"effects_shadowed":0})
            );
        }
    }
    let expected = if healthy {
        vec![
            "/library/sections",
            "/api/v3/movie",
            "/library/sections/1/all",
        ]
    } else {
        vec!["/library/sections"]
    };
    // Polling each second must not restart the hour-long catalog interval.
    assert_eq!(*calls.lock().unwrap(), expected);
    let store = Store::open(&database).await.unwrap();
    let observations = store.observations().await.unwrap();
    let health: Vec<_> = observations
        .iter()
        .filter(|row| row.source == "admin:health" && row.payload["component"] == "catalog")
        .collect();
    assert_eq!(health.len(), 1);
    assert_eq!(health[0].payload["available"], healthy);
    assert_eq!(health[0].payload["mode"], "shadow");
    assert_eq!(health[0].payload["observed_at"], health[0].at);
    assert!(health[0].at > 0);
    assert_eq!(snapshot.exists(), healthy);
    if healthy {
        let _: pitcrew_sync::CatalogSnapshot =
            serde_json::from_slice(&std::fs::read(snapshot).unwrap()).unwrap();
    }
}
