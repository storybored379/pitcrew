//! Explicit operator diagnostics. Offline inspection never opens a database,
//! binds a listener, or sends a request. Connectivity uses bounded GETs only.
use pitcrew_config::{ChannelAdapter, Config, NotifierAdapter, SecretRef};
use pitcrew_upstream::{Auth, Endpoint, JsonClient, Limits, Request, ResponseFuture, Transport};
use serde::Serialize;
use std::time::Duration;

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    Failed,
    Skipped,
}
#[derive(Debug, Serialize)]
pub struct Check {
    pub field: String,
    pub status: Status,
    pub message: &'static str,
}
#[derive(Debug, Serialize)]
pub struct Report {
    pub checks: Vec<Check>,
}
impl Report {
    pub fn success(&self) -> bool {
        self.checks
            .iter()
            .all(|check| check.status != Status::Failed)
    }
    fn add(&mut self, field: impl Into<String>, status: Status, message: &'static str) {
        self.checks.push(Check {
            field: field.into(),
            status,
            message,
        });
    }
}

/// Stable configuration keys only: never expose a URL, secret value, file path,
/// environment-variable name, server identifier or upstream response body.
pub fn secret_references(config: &Config) -> Vec<(String, &SecretRef)> {
    let mut refs = Vec::new();
    if let Some(secret) = &config.engine.control_secret {
        refs.push(("engine.control_secret".into(), secret));
    }
    if let Some(secret) = &config.engine.admin_secret {
        refs.push(("engine.admin_secret".into(), secret));
    }
    if let Some(plex) = &config.plex {
        refs.push(("plex.owner_token".into(), &plex.owner_token));
    }
    if let Some(agent) = &config.agent {
        refs.push(("agent.shared_secret".into(), &agent.shared_secret));
    }
    for (index, arr) in config.arr.iter().enumerate() {
        refs.push((format!("arr[{index}].api_key"), &arr.api_key));
        if let Some(secret) = &arr.webhook_secret {
            refs.push((format!("arr[{index}].webhook_secret"), secret));
        }
    }
    if let Some(discord) = &config.discord {
        refs.push(("discord.token".into(), &discord.token));
        refs.push((
            "discord.control_plane_secret".into(),
            &discord.control_plane_secret,
        ));
    }
    if let Some(web) = &config.web {
        refs.push(("web.control_plane_secret".into(), &web.control_plane_secret));
        refs.push(("web.session_secret".into(), &web.session_secret));
    }
    for (field, integration) in [
        ("seerr", &config.seerr),
        ("tracearr", &config.tracearr),
        ("tautulli", &config.tautulli),
    ] {
        if let Some(integration) = integration {
            refs.push((format!("{field}.api_key"), &integration.api_key));
        }
    }
    if let Some(notifications) = &config.notifications
        && let Some(secret) = &notifications.bearer_token
    {
        refs.push(("notifications.bearer_token".into(), secret));
    }
    for (field, provider) in [
        ("jev", &config.providers.jev),
        ("anthropic", &config.providers.anthropic),
    ] {
        if let Some(provider) = provider {
            refs.push((format!("providers.{field}.api_key"), &provider.api_key));
        }
    }
    for (index, provider) in config.providers.openai_compatible.iter().enumerate() {
        if let Some(secret) = &provider.api_key {
            refs.push((
                format!("providers.openai_compatible[{index}].api_key"),
                secret,
            ));
        }
    }
    for (index, source) in config.sources.iter().enumerate() {
        if let Some(secret) = &source.credential {
            refs.push((format!("sources[{index}].credential"), secret));
        }
    }
    for (index, channel) in config.channels.iter().enumerate() {
        let fields = match &channel.adapter {
            ChannelAdapter::Discord {
                token,
                control_plane_secret,
                ..
            }
            | ChannelAdapter::Telegram {
                token,
                control_plane_secret,
                ..
            } => vec![
                ("token", token),
                ("control_plane_secret", control_plane_secret),
            ],
            ChannelAdapter::Web {
                control_plane_secret,
                session_secret,
                ..
            } => vec![
                ("control_plane_secret", control_plane_secret),
                ("session_secret", session_secret),
            ],
            ChannelAdapter::Seerr { api_key, .. } => vec![("api_key", api_key)],
            ChannelAdapter::Email { mail, .. } => vec![("mail.password", &mail.password)],
            ChannelAdapter::Matrix { access_token, .. } => vec![("access_token", access_token)],
        };
        for (field, secret) in fields {
            refs.push((format!("channels[{index}].adapter.{field}"), secret));
        }
    }
    for (index, notifier) in config.notifiers.iter().enumerate() {
        let fields = match &notifier.adapter {
            NotifierAdapter::Ntfy { bearer_token, .. }
            | NotifierAdapter::Pushover { bearer_token, .. } => bearer_token
                .as_ref()
                .map(|secret| ("bearer_token", secret))
                .into_iter()
                .collect(),
            NotifierAdapter::Webhook { url, bearer_token } => std::iter::once(("url", url))
                .chain(bearer_token.as_ref().map(|secret| ("bearer_token", secret)))
                .collect(),
            NotifierAdapter::Email { mail, .. } => vec![("mail.password", &mail.password)],
        };
        for (field, secret) in fields {
            refs.push((format!("notifiers[{index}].adapter.{field}"), secret));
        }
    }
    refs
}

/// Passing no transport is the default offline operation. Network clients are
/// constructed by the CLI only for the explicit `--connectivity` flag.
pub async fn inspect(config: &Config, transport: Option<&dyn Transport>) -> Report {
    let mut report = Report { checks: Vec::new() };
    report.add("config", Status::Ok, "configuration valid");
    if config.engine.mode == pitcrew_config::EngineMode::Live {
        report.add(
            "engine.mode",
            Status::Failed,
            "this control-plane build requires shadow mode",
        );
    }
    for (field, secret) in secret_references(config) {
        match secret.resolve(&field) {
            Ok(value) => {
                let strong = field.starts_with("engine.")
                    || field.ends_with("webhook_secret")
                    || field == "agent.shared_secret";
                if strong && pitcrew_engine::webhook::Secret::new(value.expose()).is_err() {
                    report.add(
                        field,
                        Status::Failed,
                        "credential must contain 32..4096 visible ASCII bytes",
                    );
                } else {
                    report.add(field, Status::Ok, "secret source readable and nonempty");
                }
            }
            Err(_) => report.add(
                field,
                Status::Failed,
                "secret source absent, unreadable, or empty",
            ),
        }
    }
    let Some(transport) = transport else {
        report.add(
            "connectivity",
            Status::Skipped,
            "not requested; use --connectivity for bounded read-only checks",
        );
        return report;
    };
    if !report.success() {
        report.add(
            "connectivity",
            Status::Skipped,
            "correct local secret errors before connecting",
        );
        return report;
    }
    if let Some(plex) = &config.plex {
        probe(
            &mut report,
            transport,
            "plex",
            &plex.url,
            &plex.owner_token,
            Auth::plex,
            "library/sections",
        )
        .await;
    }
    for (index, arr) in config.arr.iter().enumerate() {
        probe(
            &mut report,
            transport,
            &format!("arr[{index}]"),
            &arr.url,
            &arr.api_key,
            Auth::arr,
            "api/v3/system/status",
        )
        .await;
    }
    if let Some(agent) = &config.agent {
        probe(
            &mut report,
            transport,
            "agent",
            &agent.url,
            &agent.shared_secret,
            Auth::bearer,
            "v1/health",
        )
        .await;
    }
    report.add(
        "other_integrations",
        Status::Skipped,
        "channel, notifier, source and cloud delivery are not exercised by doctor",
    );
    report
}
struct Borrowed<'a>(&'a dyn Transport);
impl Transport for Borrowed<'_> {
    fn send(&self, request: Request) -> ResponseFuture<'_> {
        self.0.send(request)
    }
}
async fn probe(
    report: &mut Report,
    transport: &dyn Transport,
    field: &str,
    url: &str,
    secret: &SecretRef,
    auth: fn(&str) -> Result<Auth, pitcrew_upstream::Error>,
    path: &str,
) {
    let result = async {
        let secret = secret.resolve(field).map_err(|_| ())?;
        let endpoint = Endpoint::new(
            url,
            auth(secret.expose()).map_err(|_| ())?,
            Limits {
                timeout: Duration::from_secs(5),
                max_response_bytes: 64 * 1024,
            },
        )
        .map_err(|_| ())?;
        JsonClient::new(endpoint, Borrowed(transport))
            .get::<serde_json::Value>(path, Vec::new())
            .await
            .map_err(|_| ())
    }
    .await;
    match result {
        Ok(_) => report.add(
            field,
            Status::Ok,
            "authenticated read responded with valid JSON",
        ),
        Err(_) => report.add(
            field,
            Status::Failed,
            "connectivity or authentication check failed",
        ),
    }
}
