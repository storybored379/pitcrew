//! Authenticated loopback drill transport. Static, expiring grants in this config
//! are synthetic test fixtures, never a live Plex authorization provider.
use crate::dispatch;
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    routing::post,
};
use pitcrew_domain::channels::{ChannelIdentity, IncomingMessage};
use pitcrew_intake::{
    AuthorizedViewer, CatalogItem, ConversationContext, IntakeOutcome, IntakeResponse,
};
use pitcrew_intake_store::Store;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;
use tokio::sync::Mutex;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestConfig {
    pub bind: SocketAddr,
    pub database: PathBuf,
    pub catalog: Vec<CatalogItem>,
    pub principals: Vec<TestPrincipal>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestPrincipal {
    pub bearer_token: String,
    pub viewer: AuthorizedViewer,
    pub expires_at: u64,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TestMessage {
    pub message_id: String,
    pub conversation_id: String,
    pub text: String,
}

pub struct TestServer {
    config: TestConfig,
    store: Store,
    gate: Mutex<()>,
}

impl TestConfig {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !self.bind.ip().is_loopback() {
            return Err("test channel must bind loopback");
        }
        crate::validate_catalog(&self.catalog)?;
        if self.principals.is_empty() {
            return Err("test channel requires authenticated principals");
        }
        for (index, principal) in self.principals.iter().enumerate() {
            if principal.bearer_token.len() < 32
                || principal.bearer_token.len() > 256
                || principal
                    .bearer_token
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control())
            {
                return Err("test bearer token must contain 32..256 non-whitespace bytes");
            }
            if self.principals[..index]
                .iter()
                .any(|other| other.bearer_token == principal.bearer_token)
            {
                return Err("test bearer tokens must be unique");
            }
        }
        Ok(())
    }
}

impl TestServer {
    pub async fn open(config: TestConfig) -> Result<Self, &'static str> {
        config.validate()?;
        let store = Store::open(&config.database)
            .await
            .map_err(|_| "could not open test intake database")?;
        Ok(Self {
            config,
            store,
            gate: Mutex::new(()),
        })
    }

    /// Exact entry point used by the HTTP route and in-process authenticated
    /// drills. Expiry is checked before replay lookup, so revoked access cannot
    /// disclose old responses. JSON has no account/library/access fields.
    pub async fn message(
        &self,
        bearer: &str,
        message: TestMessage,
        now: u64,
    ) -> Result<IntakeResponse, StatusCode> {
        if bearer.len() > 256 {
            return Err(StatusCode::UNAUTHORIZED);
        }
        let supplied = Sha256::digest(bearer.as_bytes());
        let principal = self
            .config
            .principals
            .iter()
            .find(|principal| {
                Sha256::digest(principal.bearer_token.as_bytes())
                    .ct_eq(&supplied)
                    .into()
            })
            .filter(|principal| now < principal.expires_at)
            .ok_or(StatusCode::UNAUTHORIZED)?;
        let incoming = IncomingMessage {
            sender: ChannelIdentity {
                channel_id: "test".into(),
                platform_user_id: principal.viewer.account_id.clone(),
            },
            message_id: message.message_id,
            conversation_id: message.conversation_id,
            text: message.text,
            received_at: now,
        };
        let _guard = self.gate.lock().await;
        journal_message(
            &self.store,
            &incoming,
            &principal.viewer,
            &self.config.catalog,
        )
        .await
    }
}

/// Shared durable intake for platform runners and the authenticated test channel.
/// Caller supplies a fresh, trusted viewer grant. No outbound message is sent.
pub async fn journal_message(
    store: &Store,
    incoming: &IncomingMessage,
    viewer: &AuthorizedViewer,
    catalog: &[CatalogItem],
) -> Result<IntakeResponse, StatusCode> {
    let now = incoming.received_at;
    crate::validate_message(incoming).map_err(|_| StatusCode::BAD_REQUEST)?;
    // Authenticated account is part of the idempotency key. Another principal
    // cannot claim another user's client-generated message id.
    let key = message_key(incoming);
    if let Some(row) = store
        .stored_intake(&incoming.sender.channel_id, &key, &viewer.account_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    {
        if !visible(&row.response, viewer, catalog) {
            return dispatch(incoming, None, catalog, &ConversationContext::default())
                .map_err(|_| StatusCode::BAD_REQUEST);
        }
        return serde_json::from_value(row.response).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR);
    }
    let prior = store
        .latest_intake(
            &incoming.sender.channel_id,
            &incoming.conversation_id,
            &viewer.account_id,
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut context = ConversationContext::default();
    if let Some(row) = prior.filter(|row| visible(&row.response, viewer, catalog)) {
        context = serde_json::from_value(
            row.response
                .get("_context")
                .cloned()
                .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?,
        )
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }
    let mut response =
        dispatch(incoming, Some(viewer), catalog, &context).map_err(|_| StatusCode::BAD_REQUEST)?;
    if response.outcome == IntakeOutcome::Report {
        response.report_ids = vec![key.clone()];
    }
    context.account_id = viewer.account_id.clone();
    context.conversation_id = incoming.conversation_id.clone();
    if !response.issues.is_empty() {
        context.item_ids = response
            .issues
            .iter()
            .map(|issue| issue.item_id.clone())
            .collect();
    }
    if !response.report_ids.is_empty() {
        context.report_ids = response.report_ids.clone();
    }
    let mut stored =
        serde_json::to_value(&response).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    stored["_context"] =
        serde_json::to_value(context).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    store
        .persist_intake(
            &incoming.sender.channel_id,
            &key,
            &viewer.account_id,
            &incoming.conversation_id,
            &stored,
            now,
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    // Re-read the winner if another process accepted this id concurrently.
    let row = store
        .stored_intake(&incoming.sender.channel_id, &key, &viewer.account_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    if !visible(&row.response, viewer, catalog) {
        return dispatch(incoming, None, catalog, &ConversationContext::default())
            .map_err(|_| StatusCode::BAD_REQUEST);
    }
    serde_json::from_value(row.response).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// Transport ids may be scoped to a conversation (Telegram message ids), so all
/// authenticated routing identity participates. Message text never changes a key.
pub fn message_key(incoming: &IncomingMessage) -> String {
    format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(
                &incoming.sender.channel_id,
                &incoming.sender.platform_user_id,
                &incoming.conversation_id,
                &incoming.message_id
            ))
            .expect("string tuple serialization")
        )
    )
}
fn visible(
    response: &serde_json::Value,
    viewer: &AuthorizedViewer,
    catalog: &[CatalogItem],
) -> bool {
    let Ok(context) = serde_json::from_value::<ConversationContext>(response["_context"].clone())
    else {
        return false;
    };
    context.item_ids.iter().all(|id| {
        catalog
            .iter()
            .any(|item| item.id == *id && viewer.library_ids.contains(&item.library_id))
    })
}

pub fn router(server: Arc<TestServer>) -> Router {
    Router::new()
        .route("/test/messages", post(post_message))
        .layer(DefaultBodyLimit::max(12 * 1024))
        .with_state(server)
}
async fn post_message(
    State(server): State<Arc<TestServer>>,
    headers: HeaderMap,
    Json(message): Json<TestMessage>,
) -> Result<Json<IntakeResponse>, StatusCode> {
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .as_secs();
    server.message(bearer, message, now).await.map(Json)
}
