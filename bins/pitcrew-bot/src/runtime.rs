//! Explicit platform runtime. Credentials remain in transports and are never
//! included in errors. All channel inputs converge on the durable intake path.
use crate::{
    channels::*,
    dispatch,
    server::{journal_message, message_key},
};
use pitcrew_domain::{channels::*, delivery::*};
use pitcrew_intake::{AuthorizedViewer, CatalogItem, ConversationContext};
use pitcrew_intake_store::Store;
use pitcrew_web::auth::{Auth, PlexClient};
use reqwest::{Client, Url};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::HashSet,
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::task::JoinSet;
use twilight_gateway::{Event, EventTypeFlags, Intents, Shard, ShardId, StreamExt};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    pub database: PathBuf,
    pub catalog: Vec<CatalogItem>,
    pub plex_auth_url: String,
    pub plex_server_url: String,
    pub plex_server_id: String,
    pub client_id: String,
    pub discord: Option<DiscordConfig>,
    pub telegram: Option<TelegramConfig>,
    pub seerr: Option<SeerrConfig>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscordConfig {
    pub channel_id: String,
    pub bot_token: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelegramConfig {
    pub channel_id: String,
    pub bot_token: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeerrConfig {
    pub channel_id: String,
    pub api_url: String,
    pub api_key: String,
    pub bot_user_id: u64,
}

/// Bounded HTTP with redirects disabled so a platform response cannot forward a
/// credential to a different origin. Seerr may use HTTP on a private install.
pub struct HttpTransport {
    client: Client,
    base: String,
    authorization: Option<String>,
    api_key: Option<String>,
    platform: Platform,
}
impl HttpTransport {
    pub fn new(platform: Platform, base: &str, credential: &str) -> Result<Self, &'static str> {
        let url = Url::parse(base).map_err(|_| "invalid channel API URL")?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err("invalid channel API URL");
        }
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(35))
            .build()
            .map_err(|_| "HTTP initialization failed")?;
        Ok(Self {
            client,
            base: base.trim_end_matches('/').into(),
            authorization: (platform == Platform::Discord).then(|| format!("Bot {credential}")),
            api_key: (platform == Platform::Seerr).then(|| credential.into()),
            platform,
        })
    }
    fn request(&self, path: &str, post: bool) -> reqwest::RequestBuilder {
        let url = format!("{}{path}", self.base);
        let mut request = if post {
            self.client.post(url)
        } else {
            self.client.get(url)
        };
        if let Some(value) = &self.authorization {
            request = request.header("authorization", value);
        }
        if let Some(value) = &self.api_key {
            request = request.header("x-api-key", value);
        }
        request
    }
    pub async fn get(&self, path: &str) -> Result<Value, &'static str> {
        let response = self
            .request(path, false)
            .send()
            .await
            .map_err(|_| "channel poll failed")?;
        if !response.status().is_success() {
            return Err("channel poll rejected");
        }
        read_json(response).await
    }
}
async fn read_json(mut response: reqwest::Response) -> Result<Value, &'static str> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "channel response unavailable")?
    {
        if body.len() + chunk.len() > 1024 * 1024 {
            return Err("channel response too large");
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| "invalid channel response")
}

/// Only a parsed platform receipt confirms acceptance. A 5xx, malformed success,
/// redirect or network loss remains uncertain and is never automatically resent.
pub fn delivery_receipt(platform: Platform, status: u16, body: &Value) -> DeliveryOutcome {
    if status == 429 {
        return DeliveryOutcome::NotAccepted;
    }
    if (400..500).contains(&status) && status != 408 {
        return DeliveryOutcome::PermanentFailure;
    }
    if !(200..300).contains(&status) {
        return DeliveryOutcome::Uncertain;
    }
    let accepted = match platform {
        Platform::Discord => body
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| id.parse::<u64>().ok())
            .is_some_and(|id| id > 0),
        Platform::Telegram => {
            body.get("ok") == Some(&Value::Bool(true))
                && body
                    .pointer("/result/message_id")
                    .and_then(Value::as_u64)
                    .is_some_and(|id| id > 0)
        }
        Platform::Seerr => body
            .get("id")
            .and_then(Value::as_u64)
            .is_some_and(|id| id > 0),
    };
    if accepted {
        DeliveryOutcome::Confirmed
    } else {
        DeliveryOutcome::Uncertain
    }
}
impl Transport for HttpTransport {
    fn send(&self, request: SendRequest) -> ChannelFuture<'_, DeliveryOutcome> {
        Box::pin(async move {
            let Ok(response) = self
                .request(&request.path, true)
                .json(&request.body)
                .send()
                .await
            else {
                return Ok(DeliveryOutcome::Uncertain);
            };
            let status = response.status().as_u16();
            let body = read_json(response).await.unwrap_or(Value::Null);
            Ok(delivery_receipt(self.platform, status, &body))
        })
    }
}

pub type PollFuture<'a> = Pin<Box<dyn Future<Output = Result<Value, &'static str>> + Send + 'a>>;
pub trait PollSource: Send + Sync {
    fn get<'a>(&'a self, path: &'a str) -> PollFuture<'a>;
}
impl PollSource for HttpTransport {
    fn get<'a>(&'a self, path: &'a str) -> PollFuture<'a> {
        Box::pin(HttpTransport::get(self, path))
    }
}

pub struct BotHost {
    pub store: Store,
    pub catalog: Vec<CatalogItem>,
    pub auth: Arc<dyn Auth>,
}
impl BotHost {
    /// Every message performs a fresh access lookup; a saved account link proves
    /// identity only. All returned text is fixed copy or the shared intake reply.
    pub async fn accept(
        &self,
        incoming: &IncomingMessage,
    ) -> Result<(String, String), &'static str> {
        crate::validate_message(incoming).map_err(|_| "invalid inbound message")?;
        if let Some(code) = incoming.text.trim().strip_prefix("link ") {
            // A valid code identifies an account, never a current access grant.
            // Check membership before consuming the proof so outages/revocation
            // leave the code usable after the viewer regains access.
            let result = self.link(code, incoming).await;
            return Ok(match result {
                Ok(account) => (
                    account,
                    "Your account is linked. Tell me which title is having trouble.".into(),
                ),
                Err(_) => (
                    format!("unlinked:{}", incoming.sender.platform_user_id),
                    "That link code could not be used. Create a new code in the web app.".into(),
                ),
            });
        }
        let account = self
            .store
            .linked_account(
                &incoming.sender.channel_id,
                &incoming.sender.platform_user_id,
            )
            .await
            .map_err(|_| "identity storage unavailable")?;
        let Some(account) = account else {
            return Ok((
                format!("unlinked:{}", incoming.sender.platform_user_id),
                "Sign in with Plex in the web app, then send link followed by your code.".into(),
            ));
        };
        let token = self
            .store
            .account_token(&account)
            .await
            .map_err(|_| "identity storage unavailable")?;
        let viewer = if let Some(token) = token {
            self.auth
                .access(&token)
                .await
                .ok()
                .map(|library_ids| AuthorizedViewer {
                    account_id: account.clone(),
                    library_ids,
                })
        } else {
            None
        };
        let response = if let Some(viewer) = viewer {
            journal_message(&self.store, incoming, &viewer, &self.catalog)
                .await
                .map_err(|_| "intake storage unavailable")?
        } else {
            dispatch(
                incoming,
                None,
                &self.catalog,
                &ConversationContext::default(),
            )
            .map_err(|_| "invalid inbound message")?
        };
        Ok((account, response.reply))
    }
    async fn link(&self, code: &str, incoming: &IncomingMessage) -> Result<String, &'static str> {
        let account = self
            .store
            .link_account(code, incoming.received_at)
            .await
            .map_err(|_| "link unavailable")?
            .ok_or("invalid link")?;
        let token = self
            .store
            .account_token(&account)
            .await
            .map_err(|_| "identity storage unavailable")?
            .ok_or("identity unavailable")?;
        let libraries = self
            .auth
            .access(&token)
            .await
            .map_err(|_| "membership unavailable")?;
        if libraries.is_empty() {
            return Err("membership denied");
        }
        self.store
            .redeem_link(
                code,
                &incoming.sender.channel_id,
                &incoming.sender.platform_user_id,
                incoming.received_at,
            )
            .await
            .map_err(|_| "link unavailable")
    }
    pub async fn handle(
        &self,
        channel: &PlatformChannel,
        incoming: IncomingMessage,
    ) -> Result<(), &'static str> {
        if crate::validate_message(&incoming).is_err() {
            return Ok(());
        }
        let delivery_key = message_key(&incoming);
        // Replayed transport events are already settled for intake/delivery.
        // In particular, do not redeem an old link proof again or spend its
        // global lookup quota every time Seerr polls the same comment.
        if self
            .store
            .delivery_state(
                &incoming.sender.channel_id,
                &delivery_key,
                &incoming.sender.platform_user_id,
            )
            .await
            .map_err(|_| "delivery storage unavailable")?
            .is_some()
        {
            return Ok(());
        }
        let (account, text) = self.accept(&incoming).await?;
        if !self
            .store
            .claim_delivery(
                &incoming.sender.channel_id,
                &delivery_key,
                &incoming.sender.platform_user_id,
            )
            .await
            .map_err(|_| "delivery storage unavailable")?
        {
            return Ok(());
        }
        let outgoing = OutgoingMessage {
            // Intake replies never use the engine's Ready purpose or claim repair.
            key: DeliveryKey {
                job: 0,
                purpose: DeliveryPurpose::ViewerFailure,
                recipient: account.clone(),
            },
            recipient: incoming.sender.clone(),
            conversation_id: incoming.conversation_id.clone(),
            text,
        };
        let outcome = channel
            .reply(&incoming, &outgoing)
            .await
            .map_err(|_| "channel rejected reply")?;
        let state = match outcome {
            DeliveryOutcome::Confirmed => "confirmed",
            DeliveryOutcome::NotAccepted => "not_accepted",
            DeliveryOutcome::Uncertain => "uncertain",
            DeliveryOutcome::PermanentFailure => "permanent_failure",
        };
        self.store
            .observe_delivery(
                &incoming.sender.channel_id,
                &delivery_key,
                &incoming.sender.platform_user_id,
                state,
            )
            .await
            .map_err(|_| "delivery observation unavailable")?;
        Ok(())
    }
}

pub fn validate_channels(config: &RuntimeConfig) -> Result<(), &'static str> {
    crate::validate_catalog(&config.catalog)?;
    let mut names = Vec::new();
    if let Some(c) = &config.discord {
        names.push(&c.channel_id);
    }
    if let Some(c) = &config.telegram {
        names.push(&c.channel_id);
    }
    if let Some(c) = &config.seerr {
        names.push(&c.channel_id);
    }
    names.sort();
    if names.is_empty() || names.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err("configure at least one channel with unique channel ids");
    }
    Ok(())
}

pub async fn run(config: RuntimeConfig) -> Result<(), &'static str> {
    validate_channels(&config)?;
    let host = Arc::new(BotHost {
        store: Store::open(&config.database)
            .await
            .map_err(|_| "could not open intake store")?,
        catalog: config.catalog,
        auth: Arc::new(
            PlexClient::new(
                &config.plex_auth_url,
                &config.plex_server_url,
                &config.plex_server_id,
                &config.client_id,
            )
            .map_err(|_| "invalid Plex configuration")?,
        ),
    });
    let mut tasks = JoinSet::new();
    if let Some(c) = config.discord {
        if c.bot_token.trim().is_empty() {
            return Err("Discord requires a bot application token");
        }
        let transport = Arc::new(HttpTransport::new(
            Platform::Discord,
            "https://discord.com/api/v10",
            &c.bot_token,
        )?);
        let mut shard = Shard::new(ShardId::ONE, c.bot_token, Intents::DIRECT_MESSAGES);
        let host = host.clone();
        tasks.spawn(async move { run_discord(host, &c.channel_id, transport, &mut shard).await });
    }
    if let Some(c) = config.telegram {
        let transport = Arc::new(HttpTransport::new(
            Platform::Telegram,
            &format!("https://api.telegram.org/bot{}", c.bot_token),
            "",
        )?);
        tasks.spawn(run_telegram(host.clone(), c.channel_id, transport));
    }
    if let Some(c) = config.seerr {
        let transport = Arc::new(HttpTransport::new(Platform::Seerr, &c.api_url, &c.api_key)?);
        tasks.spawn(run_seerr(host, c.channel_id, c.bot_user_id, transport));
    }
    tasks
        .join_next()
        .await
        .ok_or("no channel tasks")?
        .map_err(|_| "channel task stopped")?
}
pub fn now() -> Result<u64, &'static str> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|time| time.as_secs())
        .map_err(|_| "system clock unavailable")
}
/// The gateway event stream is injectable so fake-only tests exercise the same
/// runtime without starting a platform connection.
pub type EventFuture<'a> = Pin<Box<dyn Future<Output = Option<Event>> + Send + 'a>>;
pub trait DiscordEvents: Send {
    fn next(&mut self) -> EventFuture<'_>;
}
impl DiscordEvents for Shard {
    fn next(&mut self) -> EventFuture<'_> {
        Box::pin(async {
            loop {
                match self.next_event(EventTypeFlags::MESSAGE_CREATE).await {
                    Some(Ok(event)) => return Some(event),
                    Some(Err(_)) => continue,
                    None => return None,
                }
            }
        })
    }
}
pub async fn run_discord(
    host: Arc<BotHost>,
    channel_id: &str,
    transport: Arc<HttpTransport>,
    events: &mut dyn DiscordEvents,
) -> Result<(), &'static str> {
    let channel = PlatformChannel::new(Platform::Discord, channel_id.into(), transport.clone());
    verify_discord_bot(transport.as_ref()).await?;
    while let Some(event) = events.next().await {
        if let Event::MessageCreate(event) = event
            && let Some(incoming) = discord_message(channel_id, &event, now()?)
        {
            host.handle(&channel, incoming).await?;
        }
    }
    Err("Discord gateway stopped")
}

pub async fn verify_discord_bot(source: &dyn PollSource) -> Result<(), &'static str> {
    let user = source.get("/users/@me").await?;
    if user.get("bot") != Some(&Value::Bool(true)) {
        return Err("Discord requires a bot application account");
    }
    Ok(())
}
pub async fn run_telegram(
    host: Arc<BotHost>,
    channel_id: String,
    transport: Arc<HttpTransport>,
) -> Result<(), &'static str> {
    let channel = PlatformChannel::new(Platform::Telegram, channel_id.clone(), transport.clone());
    let mut offset = 0_u64;
    loop {
        offset = telegram_page(
            &host,
            &channel,
            transport.as_ref(),
            &channel_id,
            offset,
            now()?,
        )
        .await?;
        // Keep cancellation responsive even if an upstream returns ready pages
        // without performing its requested long poll.
        tokio::task::yield_now().await;
    }
}
/// Process one bounded Bot API page. The next offset is returned only after each
/// accepted message and its delivery claim have committed.
pub async fn telegram_page(
    host: &BotHost,
    channel: &PlatformChannel,
    source: &dyn PollSource,
    channel_id: &str,
    offset: u64,
    now: u64,
) -> Result<u64, &'static str> {
    let page = source
        .get(&format!(
            "/getUpdates?offset={offset}&timeout=25&allowed_updates=%5B%22message%22%5D"
        ))
        .await?;
    if page.get("ok") != Some(&Value::Bool(true)) {
        return Err("Telegram polling rejected");
    }
    let updates: Vec<TelegramUpdate> = serde_json::from_value(
        page.get("result")
            .cloned()
            .ok_or("missing Telegram updates")?,
    )
    .map_err(|_| "invalid Telegram updates")?;
    if updates.len() > 100 {
        return Err("oversized Telegram update page");
    }
    let mut next_offset = offset;
    for update in updates {
        let next = update
            .update_id
            .checked_add(1)
            .ok_or("Telegram update id overflow")?;
        if let Some(incoming) = telegram_message(channel_id, update, now) {
            host.handle(channel, incoming).await?;
        }
        next_offset = next_offset.max(next);
    }
    Ok(next_offset)
}
pub async fn run_seerr(
    host: Arc<BotHost>,
    channel_id: String,
    bot_user_id: u64,
    transport: Arc<HttpTransport>,
) -> Result<(), &'static str> {
    let channel = PlatformChannel::new(Platform::Seerr, channel_id.clone(), transport.clone());
    loop {
        seerr_cycle(
            &host,
            &channel,
            transport.as_ref(),
            &channel_id,
            bot_user_id,
            now()?,
        )
        .await?;
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}
/// Walk at most ten pages per poll; malformed or repeated upstream pages fail
/// closed. Detail fetches include comment authors absent from list responses.
pub async fn seerr_cycle(
    host: &BotHost,
    channel: &PlatformChannel,
    source: &dyn PollSource,
    channel_id: &str,
    bot_user: u64,
    now: u64,
) -> Result<(), &'static str> {
    let mut seen = HashSet::new();
    for page_index in 0..10 {
        let skip = page_index * 100;
        let page = source
            .get(&format!("/api/v1/issue?take=100&skip={skip}&filter=open"))
            .await?;
        let issues = page
            .get("results")
            .and_then(Value::as_array)
            .ok_or("invalid Seerr issue page")?;
        if issues.len() > 100 {
            return Err("oversized Seerr issue page");
        }
        for summary in issues {
            let id = summary
                .get("id")
                .and_then(Value::as_u64)
                .ok_or("invalid Seerr issue id")?;
            if !seen.insert(id) {
                return Err("Seerr returned a duplicate issue");
            }
            let value = source.get(&format!("/api/v1/issue/{id}")).await?;
            let prefix = seerr_title(source, &value).await?;
            let issue: SeerrIssue =
                serde_json::from_value(value).map_err(|_| "invalid Seerr issue")?;
            for mut incoming in seerr_messages(channel_id, issue, bot_user, now) {
                // Link commands carry a one-time identity proof, not a title
                // complaint. Enrichment must leave their grammar intact.
                if !incoming.text.trim().starts_with("link ") {
                    incoming.text = format!("{prefix}{}", incoming.text);
                }
                host.handle(channel, incoming).await?;
            }
        }
        if issues.len() < 100 {
            return Ok(());
        }
    }
    Err("Seerr pagination limit reached")
}
/// Seerr issue comments often omit the title; authoritative media metadata and
/// episode selectors provide context, while the shared parser still resolves
/// authorized library/quality ambiguity.
pub async fn seerr_title(source: &dyn PollSource, issue: &Value) -> Result<String, &'static str> {
    let Some(media) = issue.get("media") else {
        return Ok(String::new());
    };
    let kind = media
        .get("mediaType")
        .and_then(Value::as_str)
        .ok_or("missing Seerr media type")?;
    if !matches!(kind, "movie" | "tv") {
        return Err("unsupported Seerr media type");
    }
    let id = media
        .get("tmdbId")
        .and_then(Value::as_u64)
        .ok_or("missing Seerr media identity")?;
    let details = source.get(&format!("/api/v1/{kind}/{id}")).await?;
    let title = details
        .get(if kind == "tv" { "name" } else { "title" })
        .and_then(Value::as_str)
        .ok_or("missing Seerr media title")?;
    let mut prefix = format!("{title} ");
    if let Some(season) = issue
        .get("problemSeason")
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
    {
        prefix.push_str(&format!("season {season} "));
    }
    if let Some(episode) = issue
        .get("problemEpisode")
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
    {
        prefix.push_str(&format!("episode {episode} "));
    }
    Ok(prefix)
}
