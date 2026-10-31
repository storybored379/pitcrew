//! Live delivery construction. Only committed, prepared outbox rows can send.
//! Constructors resolve secrets and validate protocols without contacting a host.
use pitcrew_bot::{
    channels::{Platform, PlatformChannel, Transport},
    runtime::HttpTransport,
};
use pitcrew_config::{ChannelAdapter, Config, NotifierAdapter};
use pitcrew_domain::channels::Channel;
use pitcrew_engine::delivery::Notifications;
use pitcrew_notify::{
    Notifier,
    http::{HttpNotifier, Protocol},
};
use pitcrew_store::Store;
use pitcrew_upstream::Limits;
use std::{collections::BTreeMap, sync::Arc};

pub struct DeliveryAdapters {
    pub notifications: Notifications,
    /// Supply this ID to Bridge::prepare_notifications before worker dispatch.
    pub admin_notifier: String,
}

/// Injectable constructors let integration tests exercise the real composition
/// against owned loopback servers without substituting a media executor.
pub trait TransportFactory {
    fn channel(
        &self,
        platform: Platform,
        base: &str,
        credential: &str,
    ) -> Result<Arc<dyn Transport>, &'static str>;
    fn notifier(
        &self,
        protocol: Protocol,
        endpoint: &str,
        bearer: Option<&str>,
    ) -> Result<Arc<dyn Notifier>, &'static str>;
}

pub struct HttpFactory;
impl TransportFactory for HttpFactory {
    fn channel(
        &self,
        platform: Platform,
        base: &str,
        credential: &str,
    ) -> Result<Arc<dyn Transport>, &'static str> {
        Ok(Arc::new(HttpTransport::new(platform, base, credential)?))
    }
    fn notifier(
        &self,
        protocol: Protocol,
        endpoint: &str,
        bearer: Option<&str>,
    ) -> Result<Arc<dyn Notifier>, &'static str> {
        Ok(Arc::new(HttpNotifier::new(
            protocol,
            endpoint,
            bearer,
            Limits::default(),
        )?))
    }
}

pub fn compose(
    config: &Config,
    store: Store,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
) -> Result<DeliveryAdapters, &'static str> {
    compose_with(config, store, clock, &HttpFactory)
}

pub fn compose_with(
    config: &Config,
    store: Store,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    factory: &dyn TransportFactory,
) -> Result<DeliveryAdapters, &'static str> {
    if config.notifiers.len() != 1 {
        return Err("live delivery requires exactly one configured admin notifier");
    }
    let configured = &config.notifiers[0];
    let (protocol, endpoint, bearer) = match &configured.adapter {
        NotifierAdapter::Ntfy { url, bearer_token } => (Protocol::Ntfy, url.clone(), bearer_token),
        NotifierAdapter::Webhook { url, bearer_token } => (
            Protocol::Webhook,
            url.resolve("notifiers.adapter.url")
                .map_err(|_| "notifier URL secret is unavailable")?
                .expose()
                .to_owned(),
            bearer_token,
        ),
        NotifierAdapter::Pushover { .. } => {
            return Err("live Pushover delivery requires a certified relay receipt protocol");
        }
        NotifierAdapter::Email { .. } => {
            return Err("live email notifier transport is not implemented");
        }
    };
    let bearer = bearer
        .as_ref()
        .map(|secret| secret.resolve("notifiers.adapter.bearer_token"))
        .transpose()
        .map_err(|_| "notifier bearer secret is unavailable")?;
    let notifier = factory.notifier(
        protocol,
        &endpoint,
        bearer.as_ref().map(|secret| secret.expose()),
    )?;
    let mut channels: BTreeMap<String, Arc<dyn Channel>> = BTreeMap::new();
    for channel in &config.channels {
        let (platform, base, secret) = match &channel.adapter {
            ChannelAdapter::Discord { token, .. } => (
                Platform::Discord,
                "https://discord.com/api/v10".to_owned(),
                token,
            ),
            ChannelAdapter::Telegram { token, .. } => (Platform::Telegram, String::new(), token),
            ChannelAdapter::Seerr { url, api_key } => (Platform::Seerr, url.clone(), api_key),
            ChannelAdapter::Web { .. } => continue,
            ChannelAdapter::Email { .. } => {
                return Err("live email channel transport is not implemented");
            }
            ChannelAdapter::Matrix { .. } => {
                return Err("live Matrix channel transport is not implemented");
            }
        };
        let credential = secret
            .resolve("channels.adapter.credential")
            .map_err(|_| "channel credential is unavailable")?;
        // Telegram's token is embedded in its path; forbid URL delimiters before
        // interpolation. Credential values are never included in diagnostics.
        let base = if platform == Platform::Telegram {
            if !credential
                .expose()
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_:-".contains(&c))
            {
                return Err("Telegram credential contains invalid URL characters");
            }
            format!("https://api.telegram.org/bot{}", credential.expose())
        } else {
            base
        };
        let transport = factory.channel(platform, &base, credential.expose())?;
        if channels
            .insert(
                channel.id.clone(),
                Arc::new(PlatformChannel::new(
                    platform,
                    channel.id.clone(),
                    transport,
                )),
            )
            .is_some()
        {
            return Err("live channel IDs must be unique");
        }
    }
    if channels.is_empty() {
        return Err("live delivery requires a supported authenticated viewer channel");
    }
    Ok(DeliveryAdapters {
        notifications: Notifications {
            store,
            channels,
            notifiers: BTreeMap::from([(configured.id.clone(), notifier)]),
            clock,
        },
        admin_notifier: configured.id.clone(),
    })
}
