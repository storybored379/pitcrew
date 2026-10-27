use super::{Config, ConfigError, SecretRef, endpoint, invalid, nonempty};
use serde::Deserialize;
use std::collections::HashSet;

/// Presence enables a channel; omitted integrations have no requirements.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelConfig {
    pub id: String,
    pub adapter: ChannelAdapter,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChannelAdapter {
    Discord {
        token: SecretRef,
        control_plane_url: String,
        control_plane_secret: SecretRef,
    },
    Telegram {
        token: SecretRef,
        control_plane_url: String,
        control_plane_secret: SecretRef,
    },
    Web {
        public_url: String,
        control_plane_url: String,
        control_plane_secret: SecretRef,
        session_secret: SecretRef,
    },
    Seerr {
        url: String,
        api_key: SecretRef,
    },
    /// v1.x adapter contract; no implementation is shipped in this foundation.
    Email {
        mail: MailConfig,
        imap_host: String,
        imap_port: u16,
    },
    /// v1.x adapter contract.
    Matrix {
        homeserver_url: String,
        access_token: SecretRef,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MailConfig {
    pub smtp_host: String,
    pub smtp_port: u16,
    pub username: String,
    pub password: SecretRef,
    pub from_address: String,
    // TLS is required by contract; cleartext mail transports are not supported.
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotifierConfig {
    pub id: String,
    pub adapter: NotifierAdapter,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum NotifierAdapter {
    Ntfy {
        url: String,
        bearer_token: Option<SecretRef>,
    },
    /// Pushover credentials stay in the notification relay, not this process.
    Pushover {
        relay_url: String,
        bearer_token: Option<SecretRef>,
    },
    Email {
        mail: MailConfig,
        to_address: String,
    },
    Webhook {
        /// Complete webhook URLs commonly contain credentials in their paths.
        /// Resolve only in the notifier; validate the resolved URL without echo.
        url: SecretRef,
        bearer_token: Option<SecretRef>,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceBackend {
    NzbdavInfinidysk,
    RdtClient,
    Decypharr,
    Zurg,
    Qbittorrent,
    Sabnzbd,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceConfig {
    pub id: String,
    pub kind: SourceBackend,
    pub url: String,
    pub credential: Option<SecretRef>,
    /// Disabled by default, and still requires a committed authorized intent.
    #[serde(default)]
    pub repair_enabled: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DownloadProfile {
    /// nzbdav and already-cached debrid.
    #[default]
    Cached,
    /// Uncached torrents need a longer download window.
    Uncached,
    /// Local clients also support explicit per-client deadline overrides.
    Local,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadClient {
    pub id: String,
    #[serde(default)]
    pub profile: DownloadProfile,
    /// Optional enrichment adapter; generic probes do not require one.
    pub source_id: Option<String>,
    #[serde(default)]
    pub deadlines: DeadlineOverrides,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeadlineOverrides {
    pub download_seconds: Option<u64>,
    pub import_seconds: Option<u64>,
    pub decode_seconds: Option<u64>,
    pub verify_retry_seconds: Option<u64>,
}

impl DownloadClient {
    pub fn effective_deadlines(&self) -> pitcrew_domain::StageTimeouts {
        pitcrew_domain::StageTimeouts {
            download: self
                .deadlines
                .download_seconds
                .unwrap_or(match self.profile {
                    DownloadProfile::Cached | DownloadProfile::Local => 180,
                    DownloadProfile::Uncached => 3600,
                }),
            import: self.deadlines.import_seconds.unwrap_or(120),
            decode: self.deadlines.decode_seconds.unwrap_or(180),
            verify_retry: self.deadlines.verify_retry_seconds.unwrap_or(180),
        }
    }
}

/// Generic checks apply equally to local files, symlink trees and remote mounts.
/// Observations retain raw timings and thresholds; slowness is evidence.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MediaChecks {
    pub symlink_hop_limit: u16,
    pub symlink_timeout_ms: u64,
    pub mount_timeout_ms: u64,
    pub read_timeout_ms: u64,
    pub read_offsets_bytes: Vec<u64>,
    pub read_bytes: u64,
    pub slow_first_byte_ms: u64,
    pub slow_bytes_per_second: u64,
    pub header_timeout_seconds: u64,
}

impl Default for MediaChecks {
    fn default() -> Self {
        Self {
            symlink_hop_limit: 32,
            symlink_timeout_ms: 5000,
            mount_timeout_ms: 5000,
            read_timeout_ms: 15_000,
            read_offsets_bytes: vec![0, 1_048_576, 16_777_216],
            read_bytes: 1_048_576,
            slow_first_byte_ms: 3000,
            slow_bytes_per_second: 1_048_576,
            header_timeout_seconds: 180,
        }
    }
}

fn identifier<'a>(
    value: &'a str,
    seen: &mut HashSet<&'a str>,
    field: &str,
) -> Result<(), ConfigError> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
        || !seen.insert(value)
    {
        return Err(invalid(
            field,
            "must be a unique nonempty ASCII identifier (letters, digits, _, -)",
        ));
    }
    Ok(())
}

fn mail(config: &MailConfig, field: &str) -> Result<(), ConfigError> {
    nonempty(&config.smtp_host, &format!("{field}.smtp_host"))?;
    nonempty(&config.username, &format!("{field}.username"))?;
    nonempty(&config.from_address, &format!("{field}.from_address"))?;
    config.password.validate(&format!("{field}.password"))?;
    if config.smtp_port == 0 {
        return Err(invalid(format!("{field}.smtp_port"), "must be positive"));
    }
    Ok(())
}

pub(super) fn validate(config: &Config) -> Result<(), ConfigError> {
    let mut channels = HashSet::new();
    for (index, channel) in config.channels.iter().enumerate() {
        let field = format!("channels[{index}]");
        identifier(&channel.id, &mut channels, &format!("{field}.id"))?;
        let field = format!("{field}.adapter");
        match &channel.adapter {
            ChannelAdapter::Discord {
                token,
                control_plane_url,
                control_plane_secret,
            }
            | ChannelAdapter::Telegram {
                token,
                control_plane_url,
                control_plane_secret,
            } => {
                token.validate(&format!("{field}.token"))?;
                endpoint(
                    control_plane_url,
                    &format!("{field}.control_plane_url"),
                    false,
                )?;
                control_plane_secret.validate(&format!("{field}.control_plane_secret"))?;
            }
            ChannelAdapter::Web {
                public_url,
                control_plane_url,
                control_plane_secret,
                session_secret,
            } => {
                endpoint(public_url, &format!("{field}.public_url"), true)?;
                endpoint(
                    control_plane_url,
                    &format!("{field}.control_plane_url"),
                    false,
                )?;
                control_plane_secret.validate(&format!("{field}.control_plane_secret"))?;
                session_secret.validate(&format!("{field}.session_secret"))?;
            }
            ChannelAdapter::Seerr { url, api_key } => {
                endpoint(url, &format!("{field}.url"), false)?;
                api_key.validate(&format!("{field}.api_key"))?;
            }
            ChannelAdapter::Email {
                mail: connection,
                imap_host,
                imap_port,
            } => {
                mail(connection, &format!("{field}.mail"))?;
                nonempty(imap_host, &format!("{field}.imap_host"))?;
                if *imap_port == 0 {
                    return Err(invalid(format!("{field}.imap_port"), "must be positive"));
                }
            }
            ChannelAdapter::Matrix {
                homeserver_url,
                access_token,
            } => {
                endpoint(homeserver_url, &format!("{field}.homeserver_url"), true)?;
                access_token.validate(&format!("{field}.access_token"))?;
            }
        }
    }
    let mut notifiers = HashSet::new();
    for (index, notifier) in config.notifiers.iter().enumerate() {
        let field = format!("notifiers[{index}]");
        identifier(&notifier.id, &mut notifiers, &format!("{field}.id"))?;
        let field = format!("{field}.adapter");
        match &notifier.adapter {
            NotifierAdapter::Ntfy { url, bearer_token }
            | NotifierAdapter::Pushover {
                relay_url: url,
                bearer_token,
            } => {
                endpoint(url, &format!("{field}.url"), false)?;
                if let Some(secret) = bearer_token {
                    secret.validate(&format!("{field}.bearer_token"))?;
                }
            }
            NotifierAdapter::Webhook { url, bearer_token } => {
                url.validate(&format!("{field}.url"))?;
                if let Some(secret) = bearer_token {
                    secret.validate(&format!("{field}.bearer_token"))?;
                }
            }
            NotifierAdapter::Email {
                mail: connection,
                to_address,
            } => {
                mail(connection, &format!("{field}.mail"))?;
                nonempty(to_address, &format!("{field}.to_address"))?;
            }
        }
    }
    let mut sources = HashSet::new();
    for (index, source) in config.sources.iter().enumerate() {
        let field = format!("sources[{index}]");
        identifier(&source.id, &mut sources, &format!("{field}.id"))?;
        endpoint(&source.url, &format!("{field}.url"), false)?;
        if let Some(secret) = &source.credential {
            secret.validate(&format!("{field}.credential"))?;
        }
    }
    let mut clients = HashSet::new();
    for (index, client) in config.download_clients.iter().enumerate() {
        let field = format!("download_clients[{index}]");
        identifier(&client.id, &mut clients, &format!("{field}.id"))?;
        if client
            .source_id
            .as_deref()
            .is_some_and(|id| !sources.contains(id))
        {
            return Err(invalid(
                format!("{field}.source_id"),
                "must name a configured source adapter",
            ));
        }
        let timeouts = client.effective_deadlines();
        if [
            timeouts.download,
            timeouts.import,
            timeouts.decode,
            timeouts.verify_retry,
        ]
        .contains(&0)
        {
            return Err(invalid(
                format!("{field}.deadlines"),
                "all deadlines must be positive seconds",
            ));
        }
    }
    if let Some(agent) = &config.agent {
        let checks = &agent.checks;
        if !(1..=40).contains(&checks.symlink_hop_limit) {
            return Err(invalid(
                "agent.checks.symlink_hop_limit",
                "must be between 1 and 40",
            ));
        }
        if !(1..=1_048_576).contains(&checks.read_bytes) {
            return Err(invalid(
                "agent.checks.read_bytes",
                "must be between 1 and 1048576",
            ));
        }
        if !(1..=16).contains(&checks.read_offsets_bytes.len()) {
            return Err(invalid(
                "agent.checks.read_offsets_bytes",
                "must contain between 1 and 16 offsets",
            ));
        }
        for (key, value) in [
            ("symlink_timeout_ms", checks.symlink_timeout_ms),
            ("mount_timeout_ms", checks.mount_timeout_ms),
            ("read_timeout_ms", checks.read_timeout_ms),
            ("slow_first_byte_ms", checks.slow_first_byte_ms),
            ("slow_bytes_per_second", checks.slow_bytes_per_second),
            ("header_timeout_seconds", checks.header_timeout_seconds),
        ] {
            if value == 0 {
                return Err(invalid(format!("agent.checks.{key}"), "must be positive"));
            }
        }
    }
    Ok(())
}
