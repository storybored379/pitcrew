use pitcrew_config::{ChannelAdapter, Config, DownloadProfile};
use pitcrew_domain::StageTimeouts;

#[test]
fn every_new_integration_is_optional() {
    let config = Config::parse("version = 1").unwrap();
    assert!(config.channels.is_empty());
    assert!(config.notifiers.is_empty());
    assert!(config.sources.is_empty());
    assert!(config.download_clients.is_empty());
}

const CHANNELS: &str = r#"
version = 1
[[channels]]
id = "discord"
[channels.adapter]
kind = "discord"
token = { env = "DISCORD_TOKEN" }
control_plane_url = "https://control.example.invalid"
control_plane_secret = { env = "CONTROL_SECRET" }
[[channels]]
id = "telegram"
[channels.adapter]
kind = "telegram"
token = { file = "/run/secrets/telegram" }
control_plane_url = "https://control.example.invalid"
control_plane_secret = { env = "CONTROL_SECRET" }
[[channels]]
id = "web"
[channels.adapter]
kind = "web"
public_url = "https://help.example.invalid"
control_plane_url = "https://control.example.invalid"
control_plane_secret = { env = "CONTROL_SECRET" }
session_secret = { env = "SESSION_SECRET" }
[[channels]]
id = "issues"
[channels.adapter]
kind = "seerr"
url = "https://seerr.example.invalid"
api_key = { env = "SEERR_KEY" }
[[channels]]
id = "mail"
[channels.adapter]
kind = "email"
imap_host = "mail.example.invalid"
imap_port = 993
mail = { smtp_host = "mail.example.invalid", smtp_port = 465, username = "support", password = { env = "MAIL_PASSWORD" }, from_address = "noreply@help.example.invalid" }
[[channels]]
id = "matrix"
[channels.adapter]
kind = "matrix"
homeserver_url = "https://matrix.example.invalid"
access_token = { env = "MATRIX_TOKEN" }
"#;

#[test]
fn all_v1_and_v1x_channel_contracts_parse_without_discord_dependency() {
    let config = Config::parse(CHANNELS).unwrap();
    assert_eq!(config.channels.len(), 6);
    assert!(matches!(
        config.channels[0].adapter,
        ChannelAdapter::Discord { .. }
    ));
    assert!(matches!(
        config.channels[1].adapter,
        ChannelAdapter::Telegram { .. }
    ));
    assert!(matches!(
        config.channels[2].adapter,
        ChannelAdapter::Web { .. }
    ));
    assert!(matches!(
        config.channels[3].adapter,
        ChannelAdapter::Seerr { .. }
    ));
    assert!(matches!(
        config.channels[4].adapter,
        ChannelAdapter::Email { .. }
    ));
    assert!(matches!(
        config.channels[5].adapter,
        ChannelAdapter::Matrix { .. }
    ));
    assert!(config.discord.is_none());
    assert!(config.plex.is_none());
}

#[test]
fn channel_ids_credentials_and_endpoints_are_validated() {
    for (from, to, field) in [
        ("id = \"telegram\"", "id = \"discord\"", "channels[1].id"),
        ("id = \"telegram\"", "id = \"space name\"", "channels[1].id"),
        (
            "DISCORD_TOKEN",
            "bad-secret-name",
            "channels[0].adapter.token",
        ),
        (
            "https://help.example.invalid",
            "http://help.example.invalid",
            "channels[2].adapter.public_url",
        ),
        (
            "SEERR_KEY",
            "bad-secret-name",
            "channels[3].adapter.api_key",
        ),
        (
            "imap_port = 993",
            "imap_port = 0",
            "channels[4].adapter.imap_port",
        ),
        (
            "smtp_port = 465",
            "smtp_port = 0",
            "channels[4].adapter.mail.smtp_port",
        ),
        (
            "https://matrix.example.invalid",
            "http://matrix.example.invalid",
            "channels[5].adapter.homeserver_url",
        ),
    ] {
        assert_eq!(
            Config::parse(&CHANNELS.replace(from, to))
                .unwrap_err()
                .field,
            field
        );
    }
    for unsupported in [
        "slack",
        "sms",
        "whatsapp",
        "signal_bridge",
        "imessage_bridge",
    ] {
        assert!(
            Config::parse(
                &CHANNELS.replace("kind = \"discord\"", &format!("kind = \"{unsupported}\""))
            )
            .is_err()
        );
    }
}

const NOTIFIERS: &str = r#"
version = 1
[[notifiers]]
id = "ntfy"
[notifiers.adapter]
kind = "ntfy"
url = "https://notify.example.invalid/admin"
bearer_token = { env = "NOTIFY_TOKEN" }
[[notifiers]]
id = "push"
[notifiers.adapter]
kind = "pushover"
relay_url = "https://relay.example.invalid/notify"
[[notifiers]]
id = "mail"
[notifiers.adapter]
kind = "email"
mail = { smtp_host = "mail.example.invalid", smtp_port = 465, username = "support", password = { env = "MAIL_PASSWORD" }, from_address = "noreply@help.example.invalid" }
to_address = "noreply@admin.example.invalid"
[[notifiers]]
id = "webhook"
[notifiers.adapter]
kind = "webhook"
url = { env = "ADMIN_WEBHOOK_URL" }
bearer_token = { file = "/run/secrets/webhook" }
"#;

#[test]
fn admin_notifiers_are_independent_of_channels_and_keep_secrets_indirect() {
    let config = Config::parse(NOTIFIERS).unwrap();
    assert_eq!(config.notifiers.len(), 4);
    assert!(config.channels.is_empty());
    assert!(config.discord.is_none());
    assert!(!format!("{config:?}").contains("ADMIN_WEBHOOK_URL"));
    for (from, to, field) in [
        ("id = \"push\"", "id = \"ntfy\"", "notifiers[1].id"),
        (
            "NOTIFY_TOKEN",
            "bad-secret-name",
            "notifiers[0].adapter.bearer_token",
        ),
        (
            "https://relay.example.invalid/notify",
            "file:///nope",
            "notifiers[1].adapter.url",
        ),
        (
            "noreply@admin.example.invalid",
            "",
            "notifiers[2].adapter.to_address",
        ),
    ] {
        assert_eq!(
            Config::parse(&NOTIFIERS.replace(from, to))
                .unwrap_err()
                .field,
            field
        );
    }
}

#[test]
fn cached_uncached_and_local_deadlines_have_explicit_defaults() {
    let config = Config::parse(
        r#"
version = 1
[[download_clients]]
id = "cached"
[[download_clients]]
id = "uncached"
profile = "uncached"
[[download_clients]]
id = "local"
profile = "local"
"#,
    )
    .unwrap();
    assert_eq!(config.download_clients[0].profile, DownloadProfile::Cached);
    for (index, download) in [(0, 180), (1, 3600), (2, 180)] {
        assert_eq!(
            config.download_clients[index].effective_deadlines(),
            StageTimeouts {
                download,
                import: 120,
                decode: 180,
                verify_retry: 180
            }
        );
    }
}

#[test]
fn each_client_deadline_is_overridable_but_zero_is_rejected() {
    let base = "version = 1\n[[download_clients]]\nid = 'torrent'\nprofile = 'uncached'\n[download_clients.deadlines]\n";
    let config = Config::parse(&format!("{base}download_seconds=7200\nimport_seconds=240\ndecode_seconds=300\nverify_retry_seconds=600")).unwrap();
    assert_eq!(
        config.download_clients[0].effective_deadlines(),
        StageTimeouts {
            download: 7200,
            import: 240,
            decode: 300,
            verify_retry: 600
        }
    );
    for key in [
        "download_seconds",
        "import_seconds",
        "decode_seconds",
        "verify_retry_seconds",
    ] {
        assert_eq!(
            Config::parse(&format!("{base}{key}=0")).unwrap_err().field,
            "download_clients[0].deadlines"
        );
    }
}

#[test]
fn source_facts_are_optional_and_repair_is_opt_in_for_every_backend() {
    for kind in [
        "nzbdav_infinidysk",
        "rdt_client",
        "decypharr",
        "zurg",
        "qbittorrent",
        "sabnzbd",
    ] {
        let source = format!(
            r#"
version = 1
[[sources]]
id = "source"
kind = "{kind}"
url = "https://source.example.invalid"
credential = {{ env = "SOURCE_TOKEN" }}
[[download_clients]]
id = "client"
source_id = "source"
"#
        );
        assert!(!Config::parse(&source).unwrap().sources[0].repair_enabled);
        assert_eq!(
            Config::parse(&source.replace("source_id = \"source\"", "source_id = \"missing\""))
                .unwrap_err()
                .field,
            "download_clients[0].source_id"
        );
        assert_eq!(
            Config::parse(&source.replace("SOURCE_TOKEN", "bad-secret-name"))
                .unwrap_err()
                .field,
            "sources[0].credential"
        );
        assert_eq!(
            Config::parse(&(source + "\n[[download_clients]]\nid='client'\n"))
                .unwrap_err()
                .field,
            "download_clients[1].id"
        );
    }
}

#[test]
fn generic_checks_have_bounded_probes_and_reject_empty_read_plans() {
    let source = r#"
version = 1
[agent]
url = "https://agent.example.invalid"
shared_secret = { env = "AGENT_SECRET" }
media_roots = ["/media"]
storage_roots = ["/storage"]
plex_log_timezone = "Etc/UTC"
"#;
    let config = Config::parse(source).unwrap();
    let checks = config.agent.unwrap().checks;
    assert_eq!(checks.read_offsets_bytes, vec![0, 1_048_576, 16_777_216]);
    assert_eq!(checks.header_timeout_seconds, 180);
    for setting in [
        "symlink_hop_limit=0",
        "symlink_timeout_ms=0",
        "mount_timeout_ms=0",
        "read_timeout_ms=0",
        "read_bytes=0",
        "slow_first_byte_ms=0",
        "slow_bytes_per_second=0",
        "header_timeout_seconds=0",
        "read_offsets_bytes=[]",
    ] {
        assert_eq!(
            Config::parse(&format!("{source}\n[agent.checks]\n{setting}"))
                .unwrap_err()
                .field,
            format!("agent.checks.{}", setting.split('=').next().unwrap())
        );
    }
}

#[test]
fn credential_bearing_webhook_urls_cannot_be_inlined_or_echoed() {
    let source = r#"
version = 1
[[notifiers]]
id = "hook"
[notifiers.adapter]
kind = "webhook"
url = "https://hooks.example.invalid/token/VERY_SECRET"
"#;
    let error = Config::parse(source).unwrap_err();
    assert!(!error.to_string().contains("VERY_SECRET"));
    let with_reference = source.replace(
        "\"https://hooks.example.invalid/token/VERY_SECRET\"",
        "{ file = \"/run/secrets/admin_webhook_url\" }",
    );
    Config::parse(&with_reference).unwrap();
}
