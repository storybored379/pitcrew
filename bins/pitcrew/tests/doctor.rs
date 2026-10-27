use pitcrew::{
    doctor::{Status, inspect, secret_references},
    stack::web_config,
};
use pitcrew_config::Config;
use pitcrew_upstream::{Error, Method, Request, Response, ResponseFuture, Transport};
use std::{process::Command, sync::Mutex, time::Duration};

struct Fake {
    requests: Mutex<Vec<Request>>,
    fail: bool,
}
impl Transport for Fake {
    fn send(&self, request: Request) -> ResponseFuture<'_> {
        self.requests.lock().unwrap().push(request);
        Box::pin(async {
            if self.fail {
                Err(Error::Transport)
            } else {
                Ok(Response {
                    status: 200,
                    body: b"{}".to_vec(),
                })
            }
        })
    }
}
fn upstreams(secret: &std::path::Path) -> Config {
    Config::parse(&format!(
        r#"
version=1
[engine]
control_secret={{file={secret:?}}}
admin_secret={{file={secret:?}}}
[plex]
url='https://plex.invalid/prefix/'
owner_token={{file={secret:?}}}
server_id='synthetic'
[[arr]]
id='tv'
kind='sonarr'
url='https://arr.invalid/prefix/'
api_key={{file={secret:?}}}
webhook_secret={{file={secret:?}}}
libraries=['library']
[agent]
url='https://agent.invalid/prefix/'
shared_secret={{file={secret:?}}}
media_roots=['/media']
storage_roots=['/storage']
plex_log_timezone='UTC'
"#,
        secret = secret.to_str().unwrap()
    ))
    .unwrap()
}

#[tokio::test]
async fn doctor_is_offline_by_default_and_reports_secret_errors_without_values() {
    let temp = tempfile::tempdir().unwrap();
    let secret = temp.path().join("PRIVATE_SECRET_PATH");
    let config = upstreams(&secret);
    let report = inspect(&config, None).await;
    assert!(!report.success());
    assert_eq!(report.checks[0].field, "config");
    assert_eq!(report.checks[0].status, Status::Ok);
    assert_eq!(report.checks.last().unwrap().status, Status::Skipped);
    assert_eq!(
        report
            .checks
            .iter()
            .filter(|c| c.status == Status::Failed)
            .count(),
        6
    );
    assert!(!serde_json::to_string(&report).unwrap().contains("PRIVATE"));
    std::fs::write(&secret, "private-token-that-must-never-appear-123456789\n").unwrap();
    let report = inspect(&config, None).await;
    assert!(report.success());
    let output = serde_json::to_string(&report).unwrap();
    assert!(!output.contains("private-token"));
    assert!(!output.contains("invalid"));
    std::fs::write(&secret, "short").unwrap();
    let report = inspect(&config, None).await;
    assert_eq!(
        report
            .checks
            .iter()
            .filter(|c| c.status == Status::Failed)
            .map(|c| c.field.as_str())
            .collect::<Vec<_>>(),
        [
            "engine.control_secret",
            "engine.admin_secret",
            "agent.shared_secret",
            "arr[0].webhook_secret"
        ]
    );
}

#[tokio::test]
async fn explicit_connectivity_uses_only_bounded_authenticated_gets() {
    let temp = tempfile::tempdir().unwrap();
    let secret = temp.path().join("key");
    std::fs::write(&secret, "synthetic-secret-0123456789abcdef0123456789").unwrap();
    let config = upstreams(&secret);
    let fake = Fake {
        requests: Mutex::new(vec![]),
        fail: false,
    };
    let report = inspect(&config, Some(&fake)).await;
    assert!(report.success());
    {
        let requests = fake.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests.iter().map(|r| r.url.path()).collect::<Vec<_>>(),
            [
                "/prefix/library/sections",
                "/prefix/api/v3/system/status",
                "/prefix/v1/health"
            ]
        );
        for request in requests.iter() {
            assert_eq!(request.method, Method::Get);
            assert!(request.body.is_none());
            assert_eq!(request.limits.timeout, Duration::from_secs(5));
            assert_eq!(request.limits.max_response_bytes, 65536);
        }
        assert_eq!(requests[0].headers[0].0, "X-Plex-Token");
        assert_eq!(requests[1].headers[0].0, "X-Api-Key");
        assert_eq!(requests[2].headers[0].0, "Authorization");
    }
    let failing = Fake {
        requests: Mutex::new(vec![]),
        fail: true,
    };
    assert!(!inspect(&config, Some(&failing)).await.success());
    std::fs::remove_file(secret).unwrap();
    let blocked = Fake {
        requests: Mutex::new(vec![]),
        fail: false,
    };
    let report = inspect(&config, Some(&blocked)).await;
    assert!(!report.success());
    assert!(blocked.requests.lock().unwrap().is_empty());
    assert_eq!(
        report.checks.last().unwrap().message,
        "correct local secret errors before connecting"
    );
}

#[tokio::test]
async fn doctor_rejects_runtime_modes_and_policies_that_cannot_start() {
    let config =
        Config::parse("version=1\n[engine]\nmode='live'\nacknowledge_live_mutations=true").unwrap();
    let fake = Fake {
        requests: Mutex::new(vec![]),
        fail: false,
    };
    let report = inspect(&config, Some(&fake)).await;
    assert!(!report.success());
    assert!(
        report
            .checks
            .iter()
            .any(|check| check.field == "engine.mode" && check.status == Status::Failed)
    );
    assert!(fake.requests.lock().unwrap().is_empty());
    assert!(Config::parse("version=1\n[budgets]\nitem_cooldown_seconds=0").is_err());
    assert!(
        inspect(&Config::parse("version=1").unwrap(), None)
            .await
            .success()
    );
}

#[test]
fn every_optional_secret_is_in_the_inventory() {
    let config = Config::parse(r#"
version=1
[discord]
token={env='TOKEN'}
control_plane_url='http://127.0.0.1:7400'
control_plane_secret={env='TOKEN'}
[web]
public_url='https://web.invalid'
control_plane_url='http://127.0.0.1:7400'
control_plane_secret={env='TOKEN'}
session_secret={env='TOKEN'}
[seerr]
url='https://seerr.invalid'
api_key={env='TOKEN'}
[tracearr]
url='https://tracearr.invalid'
api_key={env='TOKEN'}
[tautulli]
url='https://tautulli.invalid'
api_key={env='TOKEN'}
[notifications]
relay_url='http://127.0.0.1:7402'
bearer_token={env='TOKEN'}
[providers]
cloud_opt_in=true
[providers.jev]
api_key={env='TOKEN'}
model='synthetic'
endpoint='https://jev.invalid'
[providers.anthropic]
api_key={env='TOKEN'}
model='synthetic'
endpoint='https://anthropic.invalid'
[[providers.openai_compatible]]
name='local'
preset='ollama'
model='synthetic'
[[providers.openai_compatible]]
name='gpt'
preset='openai'
model='synthetic'
api_key={env='TOKEN'}
[[sources]]
id='source'
kind='sabnzbd'
url='http://127.0.0.1:7403'
credential={env='TOKEN'}
[[channels]]
id='discord'
adapter={kind='discord',token={env='TOKEN'},control_plane_url='http://127.0.0.1:7400',control_plane_secret={env='TOKEN'}}
[[channels]]
id='telegram'
adapter={kind='telegram',token={env='TOKEN'},control_plane_url='http://127.0.0.1:7400',control_plane_secret={env='TOKEN'}}
[[channels]]
id='web'
adapter={kind='web',public_url='https://web.invalid',control_plane_url='http://127.0.0.1:7400',control_plane_secret={env='TOKEN'},session_secret={env='TOKEN'}}
[[channels]]
id='seerr'
adapter={kind='seerr',url='https://seerr.invalid',api_key={env='TOKEN'}}
[[channels]]
id='email'
adapter={kind='email',imap_host='mail.invalid',imap_port=993,mail={smtp_host='mail.invalid',smtp_port=587,username='synthetic',password={env='TOKEN'},from_address='noreply@sender.example.invalid'}}
[[channels]]
id='matrix'
adapter={kind='matrix',homeserver_url='https://matrix.invalid',access_token={env='TOKEN'}}
[[notifiers]]
id='ntfy'
adapter={kind='ntfy',url='https://notify.invalid',bearer_token={env='TOKEN'}}
[[notifiers]]
id='pushover'
adapter={kind='pushover',relay_url='http://127.0.0.1:7402',bearer_token={env='TOKEN'}}
[[notifiers]]
id='webhook'
adapter={kind='webhook',url={env='TOKEN'},bearer_token={env='TOKEN'}}
[[notifiers]]
id='email'
adapter={kind='email',to_address='noreply@recipient.example.invalid',mail={smtp_host='mail.invalid',smtp_port=587,username='synthetic',password={env='TOKEN'},from_address='noreply@sender.example.invalid'}}
"#).unwrap();
    assert_eq!(
        secret_references(&config)
            .into_iter()
            .map(|(field, _)| field)
            .collect::<Vec<_>>(),
        [
            "discord.token",
            "discord.control_plane_secret",
            "web.control_plane_secret",
            "web.session_secret",
            "seerr.api_key",
            "tracearr.api_key",
            "tautulli.api_key",
            "notifications.bearer_token",
            "providers.jev.api_key",
            "providers.anthropic.api_key",
            "providers.openai_compatible[1].api_key",
            "sources[0].credential",
            "channels[0].adapter.token",
            "channels[0].adapter.control_plane_secret",
            "channels[1].adapter.token",
            "channels[1].adapter.control_plane_secret",
            "channels[2].adapter.control_plane_secret",
            "channels[2].adapter.session_secret",
            "channels[3].adapter.api_key",
            "channels[4].adapter.mail.password",
            "channels[5].adapter.access_token",
            "notifiers[0].adapter.bearer_token",
            "notifiers[1].adapter.bearer_token",
            "notifiers[2].adapter.url",
            "notifiers[2].adapter.bearer_token",
            "notifiers[3].adapter.mail.password"
        ]
    );
}

#[test]
fn doctor_cli_and_viewer_check_create_no_state_or_network() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("config.toml");
    std::fs::write(&file, "version=1\ndatabase='must-not-exist.sqlite'").unwrap();
    for arguments in [vec!["doctor", "--config"], vec!["web", "--config"]] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pitcrew"));
        command
            .current_dir(temp.path())
            .args(arguments.clone())
            .arg(&file);
        if arguments[0] == "web" {
            command.arg("--check");
        }
        let output = command.output().unwrap();
        assert_eq!(output.status.success(), arguments[0] == "doctor");
        if arguments[0] == "doctor" {
            let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            let checks = report["checks"].as_array().unwrap();
            assert_eq!(checks.last().unwrap()["field"], "connectivity");
            assert_eq!(checks.last().unwrap()["status"], "skipped");
        }
    }
    assert!(!temp.path().join("must-not-exist.sqlite").exists());
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
    let config = upstreams(&temp.path().join("not-resolved-by-web"));
    let web = web_config(&config).unwrap();
    assert_eq!(web.database, "intake.sqlite");
    assert_eq!(web.bind, config.stack.web_listen);
    assert_eq!(web.public_origin, config.stack.web_public_origin);
    assert_eq!(web.plex_auth_url, config.stack.plex_auth_url);
    assert_eq!(web.plex_server_url, "https://plex.invalid/prefix/");
    assert_eq!(web.plex_server_id, "synthetic");
    assert_eq!(web.client_id, "pitcrew");
    assert!(web.catalog.is_empty());
    assert!(web.catalog_snapshot_file.is_none());
    let output = Command::new(env!("CARGO_BIN_EXE_pitcrew"))
        .args(["doctor", "--config"])
        .arg(&file)
        .arg("--connectivity")
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report["checks"].as_array().unwrap().last().unwrap()["field"],
        "other_integrations"
    );
}
