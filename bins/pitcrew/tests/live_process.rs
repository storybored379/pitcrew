//! Executable live refusal is tested with local records and owned loopback peers.
use pitcrew::{live, live::fixture_digest};
use pitcrew_arr::Kind;
use pitcrew_engine::live::{Certification, OwnerAcknowledgement};
use pitcrew_store::{Store, policy::ClientCapabilities};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::{Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{io::AsyncWriteExt, net::TcpListener, task::JoinHandle};

struct Peer {
    base: String,
    connections: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}
impl Peer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let connections = Arc::new(AtomicUsize::new(0));
        let count = connections.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                count.fetch_add(1, Ordering::SeqCst);
                // Accidental probes fail promptly and remain observable.
                let _ = socket
                    .write_all(b"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
            }
        });
        Self {
            base,
            connections,
            task,
        }
    }

    fn assert_untouched(&self) {
        assert_eq!(self.connections.load(Ordering::SeqCst), 0);
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    config: PathBuf,
    database: PathBuf,
    secret: PathBuf,
    source: String,
}

fn write_json(path: &Path, value: &impl serde::Serialize) {
    std::fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}

impl Fixture {
    async fn new(kind: Kind, base: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("engine.sqlite");
        let secret = dir.path().join("credential");
        std::fs::write(&secret, "synthetic-process-secret-0123456789").unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let (kind_name, version) = match kind {
            Kind::Sonarr => ("sonarr", "4.0.19.2979"),
            Kind::Radarr => ("radarr", "6.3.0.10514"),
        };
        let certificate = Certification {
            id: "synthetic-process-review".into(),
            client: "arr-one".into(),
            kind,
            version: version.into(),
            fixture_digest: fixture_digest(kind),
            certified_at: now - 60,
            expires_at: now + 3600,
        };
        let certificate_file = dir.path().join("certificate.json");
        let acknowledgement_file = dir.path().join("acknowledgement.json");
        let capabilities_file = dir.path().join("capabilities.json");
        write_json(&certificate_file, &certificate);
        write_json(
            &acknowledgement_file,
            &OwnerAcknowledgement {
                certification: certificate.clone(),
                acknowledge_live_mutations: true,
            },
        );
        let capabilities = ClientCapabilities {
            external_queue_actor: false,
            held_category: None,
        };
        let evidence = json!({"review":"synthetic-process-review"});
        write_json(
            &capabilities_file,
            &json!({
                "certification":certificate, "capabilities":capabilities, "evidence":evidence,
            }),
        );
        let store = Store::open(&database).await.unwrap();
        store
            .observe_client_version("arr-one", version, now - 60)
            .await
            .unwrap();
        store
            .record_capabilities("arr-one", version, &capabilities, &evidence, now - 60)
            .await
            .unwrap();
        drop(store);
        let intake = dir.path().join("intake.sqlite");
        let auth = base.replacen("http://", "https://", 1);
        let source = format!(
            r#"
version = 1
database = {database:?}
[engine]
mode = "live"
acknowledge_live_mutations = true
listen = "127.0.0.1:0"
[live]
enabled = true
[[live.certifications]]
client = "arr-one"
id = "synthetic-process-review"
version = "{version}"
fixture_digest = "{digest}"
certified_at = {certified_at}
expires_at = {expires_at}
certificate_file = {certificate_file:?}
acknowledgement_file = {acknowledgement_file:?}
capabilities_file = {capabilities_file:?}
[stack]
enabled = true
intake_database = {intake:?}
plex_auth_url = "{auth}/auth"
[runtime]
search_enabled = true
[plex]
url = "{base}/plex"
owner_token = {{file = {secret:?}}}
server_id = "synthetic-server"
[[arr]]
id = "arr-one"
kind = "{kind_name}"
url = "{base}/arr"
api_key = {{file = {secret:?}}}
libraries = ["1"]
[agent]
url = "{base}/agent"
shared_secret = {{file = {secret:?}}}
media_roots = ["/library"]
storage_roots = ["/library"]
plex_log_timezone = "UTC"
[[notifiers]]
id = "admin-one"
adapter = {{kind = "ntfy", url = "{base}/alerts"}}
[[channels]]
id = "viewer-one"
adapter = {{kind = "seerr", url = "{base}/seerr", api_key = {{file = {secret:?}}}}}
"#,
            digest = certificate.fixture_digest,
            certified_at = certificate.certified_at,
            expires_at = certificate.expires_at,
        );
        pitcrew_config::Config::parse(&source).unwrap();
        let config = dir.path().join("config.toml");
        Self {
            dir,
            config,
            database,
            secret,
            source,
        }
    }

    async fn run(&self) -> Output {
        std::fs::write(&self.config, &self.source).unwrap();
        tokio::time::timeout(
            Duration::from_secs(10),
            tokio::process::Command::new(env!("CARGO_BIN_EXE_pitcrew"))
                .current_dir(self.dir.path())
                .arg("--config")
                .arg(&self.config)
                .env("NO_PROXY", "*")
                .env("no_proxy", "*")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .expect("live startup did not refuse within its deadline")
        .unwrap()
    }
}

fn events(output: &Output) -> Vec<Value> {
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    assert!(!stderr.contains("synthetic-process-secret"));
    stderr
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
async fn fully_configured_live_process_refuses_before_any_upstream_request() {
    for kind in [Kind::Sonarr, Kind::Radarr] {
        let peer = Peer::start().await;
        let fixture = Fixture::new(kind, &peer.base).await;
        let output = fixture.run().await;
        let events = events(&output);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0],
            json!({
                "event":"pitcrew_live_refused", "mode":"live", "effects_enabled":false,
                "reason":live::UNAVAILABLE,
            })
        );
        assert_eq!(
            events[1],
            json!({"event":"pitcrew_failed", "reason":live::UNAVAILABLE})
        );
        for missing in [
            "trusted grab ownership receipt producer",
            "per-transition current access refresh",
            "per-intent download/import and Plex target binding",
            "authoritative interference/dead-release policy polling",
            "these prerequisites are not implemented",
        ] {
            assert!(events[0]["reason"].as_str().unwrap().contains(missing));
        }
        assert!(!fixture.dir.path().join("intake.sqlite").exists());
        peer.assert_untouched();
    }
}

#[tokio::test]
async fn incomplete_live_configuration_and_unavailable_store_refuse_before_credentials() {
    for (mode, expected) in [
        (
            0,
            "live requires engine.mode=live, engine.acknowledge_live_mutations=true and live.enabled=true",
        ),
        (1, "live requires an available durable engine store"),
        (
            2,
            "live certificate_file must contain a bounded regular certification record",
        ),
    ] {
        let peer = Peer::start().await;
        let mut fixture = Fixture::new(Kind::Radarr, &peer.base).await;
        std::fs::remove_file(&fixture.secret).unwrap();
        match mode {
            0 => {
                fixture.source =
                    fixture
                        .source
                        .replacen("[live]\nenabled = true", "[live]\nenabled = false", 1)
            }
            1 => {
                fixture.source = fixture.source.replace(
                    &format!("database = {:?}", fixture.database),
                    &format!(
                        "database = {:?}",
                        fixture.dir.path().join("missing/engine.sqlite")
                    ),
                )
            }
            _ => std::fs::remove_file(fixture.dir.path().join("certificate.json")).unwrap(),
        }
        let output = fixture.run().await;
        assert_eq!(
            events(&output),
            vec![json!({"event":"pitcrew_failed", "reason":expected})]
        );
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains(fixture.secret.to_str().unwrap())
        );
        assert!(!fixture.dir.path().join("intake.sqlite").exists());
        peer.assert_untouched();
    }
}
