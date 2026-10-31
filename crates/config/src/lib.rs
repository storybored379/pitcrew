//! Optional integration configuration. Parsing and validation never contact upstreams.
//!
//! Secrets are references, resolved separately by the process that needs them.
//! Diagnostics deliberately exclude input values and secret contents.

use serde::Deserialize;
use std::{collections::HashSet, fmt, path::PathBuf};
use thiserror::Error;
use url::Url;

mod integrations;
pub use integrations::*;
mod stack;
pub use stack::{Stack, StackCatalogItem, StackResolution};
pub mod admin;
pub mod live;
pub mod lower_quality;
pub mod runtime;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub admin: Option<admin::AdminUi>,
    #[serde(default)]
    pub stack: Stack,
    #[serde(default)]
    pub runtime: runtime::Runtime,
    #[serde(default)]
    pub live: live::Live,
    #[serde(default)]
    pub lower_quality: lower_quality::LowerQuality,
    #[serde(default)]
    pub engine: Engine,
    #[serde(default = "default_database")]
    pub database: PathBuf,
    pub plex: Option<Plex>,
    #[serde(default)]
    pub arr: Vec<Arr>,
    pub discord: Option<Discord>,
    pub web: Option<Web>,
    pub agent: Option<Agent>,
    pub seerr: Option<Integration>,
    pub tracearr: Option<Integration>,
    pub tautulli: Option<Integration>,
    pub notifications: Option<Notifications>,
    #[serde(default)]
    pub channels: Vec<ChannelConfig>,
    #[serde(default)]
    pub notifiers: Vec<NotifierConfig>,
    #[serde(default)]
    pub sources: Vec<SourceConfig>,
    #[serde(default)]
    pub download_clients: Vec<DownloadClient>,
    #[serde(default)]
    pub providers: Providers,
    #[serde(default)]
    pub budgets: Budgets,
}

fn default_database() -> PathBuf {
    PathBuf::from("pitcrew.sqlite")
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plex {
    pub url: String,
    pub owner_token: SecretRef,
    pub server_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Arr {
    /// Stable installation-local identifier, unrelated to a hostname.
    pub id: String,
    pub kind: ArrKind,
    pub url: String,
    pub api_key: SecretRef,
    pub webhook_secret: Option<SecretRef>,
    /// Plex section keys mapped by the administrator; never an authorization grant.
    pub libraries: Vec<String>,
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EngineMode {
    #[default]
    Shadow,
    Live,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Engine {
    pub mode: EngineMode,
    pub acknowledge_live_mutations: bool,
    pub listen: std::net::SocketAddr,
    /// Required for local observation/replay and report endpoints.
    pub control_secret: Option<SecretRef>,
    /// Separate credential for audited policy administration; disabled when absent.
    pub admin_secret: Option<SecretRef>,
    /// Explicitly enable read-only queue/history fallback polling.
    pub poll_enabled: bool,
    pub poll_seconds: u64,
}
impl Default for Engine {
    fn default() -> Self {
        Self {
            mode: EngineMode::Shadow,
            acknowledge_live_mutations: false,
            listen: "127.0.0.1:8791".parse().expect("literal socket address"),
            control_secret: None,
            admin_secret: None,
            poll_enabled: false,
            poll_seconds: 30,
        }
    }
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArrKind {
    Sonarr,
    Radarr,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Discord {
    pub token: SecretRef,
    pub control_plane_url: String,
    pub control_plane_secret: SecretRef,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Web {
    pub public_url: String,
    pub control_plane_url: String,
    pub control_plane_secret: SecretRef,
    pub session_secret: SecretRef,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Agent {
    #[serde(default)]
    pub server: AgentServer,
    pub url: String,
    pub shared_secret: SecretRef,
    /// Logical media roots and resolved storage roots are separate allow-lists.
    pub media_roots: Vec<PathBuf>,
    pub storage_roots: Vec<PathBuf>,
    pub plex_log_timezone: String,
    #[serde(default = "default_decode_concurrency")]
    pub decode_concurrency: u16,
    #[serde(default)]
    pub checks: MediaChecks,
}

/// Local HTTP process settings. Remote transport requires a TLS ingress.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentServer {
    pub listen: std::net::SocketAddr,
    pub compatibility: bool,
    pub max_connections: usize,
    pub requests_per_minute: u32,
    pub request_timeout_seconds: u64,
    pub plex_log_directory: Option<PathBuf>,
    pub decode_seconds: u32,
    pub decode_offsets_percent: Vec<u8>,
    pub decode_timeout_seconds: u64,
    pub filesystem_workers: usize,
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}
impl Default for AgentServer {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8790".parse().expect("literal socket address"),
            compatibility: false,
            max_connections: 32,
            requests_per_minute: 120,
            request_timeout_seconds: 600,
            plex_log_directory: None,
            decode_seconds: 10,
            decode_offsets_percent: vec![0, 40, 80],
            decode_timeout_seconds: 180,
            filesystem_workers: 4,
            ffmpeg: "ffmpeg".into(),
            ffprobe: "ffprobe".into(),
        }
    }
}

fn default_decode_concurrency() -> u16 {
    2
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Integration {
    pub url: String,
    pub api_key: SecretRef,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notifications {
    pub relay_url: String,
    pub bearer_token: Option<SecretRef>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Providers {
    /// Explicit consent covers both live and shadow cloud requests.
    pub cloud_opt_in: bool,
    pub jev: Option<CloudProvider>,
    pub anthropic: Option<CloudProvider>,
    /// Any number of OpenAI-style chat completions servers (OpenAI, DeepSeek,
    /// Gemini, Groq, OpenRouter, or local Ollama/LM Studio/llama.cpp/vLLM).
    pub openai_compatible: Vec<OpenAiCompatibleProvider>,
    pub decider: Decider,
    /// Name of the `openai_compatible` profile used when `decider = "openai_compatible"`.
    pub decider_profile: Option<String>,
    pub reply_writer: ReplyWriter,
    /// Name of the `openai_compatible` profile used when `reply_writer = "openai_compatible"`.
    pub reply_writer_profile: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenAiCompatibleProvider {
    /// Unique profile name used in comparisons and by `decider_profile`.
    pub name: String,
    pub preset: OpenAiPreset,
    /// Defaults to the preset's endpoint; required for `custom`.
    pub endpoint: Option<String>,
    pub model: String,
    /// Required for cloud servers; optional for local ones.
    pub api_key: Option<SecretRef>,
    /// Defaults to the preset (local presets are local); required for `custom`.
    /// Local profiles need no cloud consent but must point at a local host.
    pub local: Option<bool>,
    pub structured: Option<StructuredOutput>,
    pub token_field: Option<TokenField>,
    #[serde(default = "default_shadow")]
    pub shadow: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiPreset {
    Openai,
    Deepseek,
    Gemini,
    Groq,
    Openrouter,
    Ollama,
    LmStudio,
    LlamaCpp,
    Vllm,
    Custom,
}

impl OpenAiPreset {
    /// None for `custom`, which must state it explicitly.
    pub fn default_local(self) -> Option<bool> {
        match self {
            Self::Ollama | Self::LmStudio | Self::LlamaCpp | Self::Vllm => Some(true),
            Self::Custom => None,
            _ => Some(false),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StructuredOutput {
    JsonSchema,
    JsonObject,
    PromptOnly,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TokenField {
    MaxTokens,
    MaxCompletionTokens,
}

impl OpenAiCompatibleProvider {
    pub fn is_local(&self) -> bool {
        self.local.or(self.preset.default_local()).unwrap_or(false)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudProvider {
    pub api_key: SecretRef,
    pub model: String,
    pub endpoint: String,
    #[serde(default = "default_shadow")]
    pub shadow: bool,
}

fn default_shadow() -> bool {
    true
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Decider {
    #[default]
    Rules,
    Jev,
    Anthropic,
    OpenaiCompatible,
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReplyWriter {
    #[default]
    Templates,
    Anthropic,
    OpenaiCompatible,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Budgets {
    pub global_hour: u32,
    pub global_day: u32,
    pub viewer_hour: u32,
    pub viewer_day: u32,
    pub per_job: u32,
    pub item_cooldown_seconds: u64,
}

impl Default for Budgets {
    fn default() -> Self {
        Self {
            global_hour: 12,
            global_day: 40,
            viewer_hour: 3,
            viewer_day: 10,
            per_job: 2,
            item_cooldown_seconds: 21_600,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretRef {
    pub env: Option<String>,
    pub file: Option<PathBuf>,
}

impl fmt::Debug for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretRef([redacted])")
    }
}

/// Intentionally neither serializable nor displayable.
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error("{field}: {problem}")]
pub struct ConfigError {
    pub field: String,
    pub problem: &'static str,
}

fn invalid(field: impl Into<String>, problem: &'static str) -> ConfigError {
    ConfigError {
        field: field.into(),
        problem,
    }
}

impl SecretRef {
    pub fn validate(&self, field: &str) -> Result<(), ConfigError> {
        match (&self.env, &self.file) {
            (Some(name), None)
                if !name.is_empty()
                    && name.bytes().enumerate().all(|(i, c)| {
                        c == b'_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit())
                    }) =>
            {
                Ok(())
            }
            (None, Some(path)) if path.is_absolute() => Ok(()),
            _ => Err(invalid(
                field,
                "set exactly one valid env variable name or absolute secret file path",
            )),
        }
    }

    /// Called only by the component needing the secret, after validation.
    pub fn resolve(&self, field: &str) -> Result<Secret, ConfigError> {
        self.resolve_with(
            field,
            |name| std::env::var(name).ok(),
            |path| std::fs::read_to_string(path).ok(),
        )
    }

    /// Injected sources keep tests independent of the process environment and filesystem.
    pub fn resolve_with(
        &self,
        field: &str,
        environment: impl FnOnce(&str) -> Option<String>,
        file: impl FnOnce(&std::path::Path) -> Option<String>,
    ) -> Result<Secret, ConfigError> {
        self.validate(field)?;
        let value = match (&self.env, &self.file) {
            (Some(name), None) => environment(name),
            (None, Some(path)) => file(path),
            _ => unreachable!("validated secret reference"),
        }
        .ok_or_else(|| invalid(field, "secret source is absent, unreadable, or not UTF-8"))?;
        // Secret files commonly end with a newline. Do not alter other whitespace.
        let value = value.trim_end_matches(['\r', '\n']);
        if value.trim().is_empty() {
            return Err(invalid(field, "secret must not be empty"));
        }
        Ok(Secret(value.to_owned()))
    }
}

impl Config {
    pub fn parse(source: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(source).map_err(safe_parse_error)?;
        config.validate()?;
        Ok(config)
    }

    /// Empty integrations are valid for initial setup. This is structural validation,
    /// not a statement that an installation is ready to repair media.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.stack.validate()?;
        self.runtime.validate()?;
        integrations::validate(self)?;
        if !self.engine.listen.ip().is_loopback() {
            return Err(invalid(
                "engine.listen",
                "must bind loopback; use a TLS ingress for remote arr webhooks",
            ));
        }
        if !(1..=3600).contains(&self.engine.poll_seconds) {
            return Err(invalid(
                "engine.poll_seconds",
                "must be between 1 and 3600 seconds",
            ));
        }
        if self.engine.mode == EngineMode::Live && !self.engine.acknowledge_live_mutations {
            return Err(invalid(
                "engine.acknowledge_live_mutations",
                "live mode requires explicit mutation acknowledgement",
            ));
        }
        if let Some(secret) = &self.engine.admin_secret {
            secret.validate("engine.admin_secret")?;
        }
        if let Some(secret) = &self.engine.control_secret {
            secret.validate("engine.control_secret")?;
        }
        for (index, arr) in self.arr.iter().enumerate() {
            if let Some(secret) = &arr.webhook_secret {
                secret.validate(&format!("arr[{index}].webhook_secret"))?;
            }
        }
        if self.version != 1 {
            return Err(invalid("version", "expected configuration version 1"));
        }
        if self.database.as_os_str().is_empty() {
            return Err(invalid("database", "must name a database file"));
        }
        if let Some(plex) = &self.plex {
            endpoint(&plex.url, "plex.url", false)?;
            nonempty(&plex.server_id, "plex.server_id")?;
            plex.owner_token.validate("plex.owner_token")?;
        }
        let mut ids = HashSet::new();
        for (index, arr) in self.arr.iter().enumerate() {
            let field = format!("arr[{index}]");
            if arr.id.is_empty()
                || !arr
                    .id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
                || !ids.insert(&arr.id)
            {
                return Err(invalid(
                    format!("{field}.id"),
                    "must be a unique nonempty ASCII identifier (letters, digits, _, -)",
                ));
            }
            endpoint(&arr.url, &format!("{field}.url"), false)?;
            arr.api_key.validate(&format!("{field}.api_key"))?;
            let mut libraries = HashSet::new();
            if arr.libraries.is_empty()
                || arr
                    .libraries
                    .iter()
                    .any(|key| key.trim().is_empty() || !libraries.insert(key))
            {
                return Err(invalid(
                    format!("{field}.libraries"),
                    "must contain unique nonempty Plex library keys",
                ));
            }
        }
        if let Some(discord) = &self.discord {
            discord.token.validate("discord.token")?;
            discord
                .control_plane_secret
                .validate("discord.control_plane_secret")?;
            endpoint(
                &discord.control_plane_url,
                "discord.control_plane_url",
                false,
            )?;
        }
        if let Some(web) = &self.web {
            endpoint(&web.public_url, "web.public_url", true)?;
            endpoint(&web.control_plane_url, "web.control_plane_url", false)?;
            web.control_plane_secret
                .validate("web.control_plane_secret")?;
            web.session_secret.validate("web.session_secret")?;
        }
        if let Some(agent) = &self.agent {
            endpoint(&agent.url, "agent.url", false)?;
            let server = &agent.server;
            if !server.listen.ip().is_loopback() {
                return Err(invalid(
                    "agent.server.listen",
                    "must be a loopback socket address",
                ));
            }
            for (key, value, max) in [
                ("max_connections", server.max_connections as u64, 256),
                (
                    "requests_per_minute",
                    u64::from(server.requests_per_minute),
                    6000,
                ),
                (
                    "request_timeout_seconds",
                    server.request_timeout_seconds,
                    3600,
                ),
                ("decode_seconds", u64::from(server.decode_seconds), 300),
                ("decode_timeout_seconds", server.decode_timeout_seconds, 600),
                ("filesystem_workers", server.filesystem_workers as u64, 64),
            ] {
                if !(1..=max).contains(&value) {
                    return Err(invalid(
                        format!("agent.server.{key}"),
                        "must be positive and within the documented limit",
                    ));
                }
            }
            if server.decode_offsets_percent.is_empty()
                || server.decode_offsets_percent.len() > 16
                || server.decode_offsets_percent.iter().any(|p| *p >= 100)
                || server
                    .decode_offsets_percent
                    .windows(2)
                    .any(|p| p[0] >= p[1])
            {
                return Err(invalid(
                    "agent.server.decode_offsets_percent",
                    "must contain 1–16 strictly increasing percentages from 0 to 99",
                ));
            }
            for (key, path) in [("ffmpeg", &server.ffmpeg), ("ffprobe", &server.ffprobe)] {
                if path.as_os_str().is_empty() {
                    return Err(invalid(
                        format!("agent.server.{key}"),
                        "must name an executable",
                    ));
                }
            }
            if server
                .plex_log_directory
                .as_ref()
                .is_some_and(|p| !p.is_absolute())
            {
                return Err(invalid(
                    "agent.server.plex_log_directory",
                    "must be an absolute directory",
                ));
            }

            agent.shared_secret.validate("agent.shared_secret")?;
            for (field, paths) in [
                ("agent.media_roots", &agent.media_roots),
                ("agent.storage_roots", &agent.storage_roots),
            ] {
                if paths.is_empty()
                    || paths.iter().any(|path| {
                        !path.is_absolute()
                            || path.parent().is_none()
                            || path
                                .components()
                                .any(|c| c == std::path::Component::ParentDir)
                    })
                {
                    return Err(invalid(
                        field,
                        "must contain absolute non-root paths without parent traversal",
                    ));
                }
            }
            if agent.decode_concurrency == 0 || agent.decode_concurrency > 64 {
                return Err(invalid(
                    "agent.decode_concurrency",
                    "must be between 1 and 64",
                ));
            }
            // Never inherit the process timezone for timezone-free Plex logs.
            if agent.plex_log_timezone.parse::<chrono_tz::Tz>().is_err() {
                return Err(invalid(
                    "agent.plex_log_timezone",
                    "must be UTC or an explicit IANA timezone (for example Etc/UTC)",
                ));
            }
        }
        for (field, integration) in [
            ("seerr", &self.seerr),
            ("tracearr", &self.tracearr),
            ("tautulli", &self.tautulli),
        ] {
            if let Some(integration) = integration {
                endpoint(&integration.url, &format!("{field}.url"), false)?;
                integration.api_key.validate(&format!("{field}.api_key"))?;
            }
        }
        if let Some(notifications) = &self.notifications {
            endpoint(&notifications.relay_url, "notifications.relay_url", false)?;
            if let Some(secret) = &notifications.bearer_token {
                secret.validate("notifications.bearer_token")?;
            }
        }
        for (field, provider) in [
            ("providers.jev", &self.providers.jev),
            ("providers.anthropic", &self.providers.anthropic),
        ] {
            if let Some(provider) = provider {
                if !self.providers.cloud_opt_in {
                    return Err(invalid(
                        "providers.cloud_opt_in",
                        "explicit consent is required for cloud providers, including shadow calls",
                    ));
                }
                endpoint(&provider.endpoint, &format!("{field}.endpoint"), true)?;
                nonempty(&provider.model, &format!("{field}.model"))?;
                provider.api_key.validate(&format!("{field}.api_key"))?;
            }
        }
        let mut names = HashSet::new();
        for (i, provider) in self.providers.openai_compatible.iter().enumerate() {
            let field = format!("providers.openai_compatible[{i}]");
            if provider.name.is_empty()
                || provider.name.len() > 64
                || !provider
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
                || !names.insert(provider.name.as_str())
            {
                return Err(invalid(
                    format!("{field}.name"),
                    "names must be unique, 1-64 letters, digits, '-' or '_'",
                ));
            }
            nonempty(&provider.model, &format!("{field}.model"))?;
            if provider.preset == OpenAiPreset::Custom
                && (provider.endpoint.is_none() || provider.local.is_none())
            {
                return Err(invalid(
                    field,
                    "a custom preset must set both endpoint and local",
                ));
            }
            let local = provider.is_local();
            if let Some(url) = &provider.endpoint {
                endpoint(url, &format!("{field}.endpoint"), !local)?;
            }
            match &provider.api_key {
                Some(key) => key.validate(&format!("{field}.api_key"))?,
                None if !local => {
                    return Err(invalid(
                        format!("{field}.api_key"),
                        "cloud providers require an api_key",
                    ));
                }
                None => {}
            }
            if !local && !self.providers.cloud_opt_in {
                return Err(invalid(
                    "providers.cloud_opt_in",
                    "explicit consent is required for cloud providers, including shadow calls",
                ));
            }
        }
        let profile = |name: &Option<String>, field: &'static str| {
            let found = name.as_deref().and_then(|name| {
                self.providers
                    .openai_compatible
                    .iter()
                    .find(|p| p.name == name)
            });
            match found {
                Some(provider) if !provider.shadow => Ok(()),
                _ => Err(invalid(
                    field,
                    "must name an openai_compatible profile configured with shadow = false",
                )),
            }
        };
        let live = match self.providers.decider {
            Decider::Rules => None,
            Decider::Jev => Some(&self.providers.jev),
            Decider::Anthropic => Some(&self.providers.anthropic),
            Decider::OpenaiCompatible => {
                profile(&self.providers.decider_profile, "providers.decider_profile")?;
                None
            }
        };
        if live.is_some_and(|provider| provider.as_ref().is_none_or(|p| p.shadow)) {
            return Err(invalid(
                "providers.decider",
                "selected live provider must be configured with shadow = false",
            ));
        }
        if self.providers.reply_writer == ReplyWriter::Anthropic
            && self.providers.anthropic.as_ref().is_none_or(|p| p.shadow)
        {
            return Err(invalid(
                "providers.reply_writer",
                "Anthropic must be configured with shadow = false; templates are the default",
            ));
        }
        if self.providers.reply_writer == ReplyWriter::OpenaiCompatible {
            profile(
                &self.providers.reply_writer_profile,
                "providers.reply_writer_profile",
            )?;
        }
        let b = &self.budgets;
        if [
            b.global_hour,
            b.global_day,
            b.viewer_hour,
            b.viewer_day,
            b.per_job,
        ]
        .contains(&0)
            || b.item_cooldown_seconds == 0
            || b.global_hour > b.global_day
            || b.viewer_hour > b.viewer_day
            || b.viewer_hour > b.global_hour
            || b.viewer_day > b.global_day
        {
            return Err(invalid(
                "budgets",
                "limits and cooldown must be positive; hourly <= daily and viewer <= global",
            ));
        }
        Ok(())
    }
}

fn nonempty(value: &str, field: &str) -> Result<(), ConfigError> {
    if value.trim().is_empty() {
        return Err(invalid(field, "must not be empty"));
    }
    Ok(())
}

fn endpoint(value: &str, field: &str, https_only: bool) -> Result<(), ConfigError> {
    let valid = Url::parse(value).is_ok_and(|url| {
        (url.scheme() == "https" || (!https_only && url.scheme() == "http"))
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
    });
    if !valid {
        return Err(invalid(
            field,
            "expected an HTTP(S) base URL without credentials, query, or fragment (HTTPS required for public/cloud endpoints)",
        ));
    }
    Ok(())
}

/// Drop the TOML source excerpt and all value-bearing message text. Only the
/// schema/key path and a fixed error category can escape into startup logs.
fn safe_parse_error(mut error: toml::de::Error) -> ConfigError {
    error.set_input(None);
    let rendered = error.to_string();
    let message = error.message();
    let path = rendered
        .strip_prefix(message)
        .and_then(|s| s.strip_prefix("\nin `"))
        .and_then(|s| s.strip_suffix("`\n"))
        .filter(|s| safe_key(s))
        .unwrap_or("");
    let (key, problem) = if let Some(rest) = message.strip_prefix("missing field `") {
        (rest.split('`').next(), "required field is missing")
    } else if let Some(rest) = message.strip_prefix("unknown field `") {
        (rest.split('`').next(), "unknown configuration key")
    } else if message.starts_with("unknown variant") {
        (None, "unrecognized option")
    } else if message.starts_with("invalid type") {
        (None, "incorrect value type")
    } else if message.starts_with("invalid value") {
        (None, "value is outside the allowed range or format")
    } else {
        (
            None,
            "invalid TOML syntax; check delimiters, quoting, and duplicate keys",
        )
    };
    let key = key.filter(|s| safe_key(s));
    let field = match (path.is_empty(), key) {
        (true, Some(key)) => key.to_owned(),
        (false, Some(key)) => format!("{path}.{key}"),
        (false, None) => path.to_owned(),
        (true, None) => error.span().map_or_else(
            || "config".to_owned(),
            |span| format!("config at byte {}", span.start),
        ),
    };
    invalid(field, problem)
}

fn safe_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 256
        && key
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.-[]".contains(&c))
}
