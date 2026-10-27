//! Plex PIN identity and viewer-token library checks. Redirects are disabled and
//! responses bounded. Tokens are headers only and are never sent to a model.
use serde::Deserialize;
use std::{future::Future, pin::Pin, time::Duration};
use thiserror::Error;
use url::Url;

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    #[error("Plex could not verify access; please try again")]
    Unavailable,
    #[error("This Plex account does not have access to the configured server")]
    Denied,
    #[error("invalid Plex configuration")]
    Configuration,
}
pub type AuthFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, AuthError>> + Send + 'a>>;
#[derive(Clone, Deserialize)]
pub struct LoginPin {
    pub id: u64,
    pub code: String,
}
pub struct Account {
    pub id: String,
    pub token: String,
}
pub trait Auth: Send + Sync {
    fn create_pin(&self) -> AuthFuture<'_, LoginPin>;
    fn poll(&self, id: u64) -> AuthFuture<'_, Option<Account>>;
    fn access<'a>(&'a self, token: &'a str) -> AuthFuture<'a, Vec<String>>;
}
#[derive(Clone)]
pub struct PlexClient {
    client: reqwest::Client,
    auth_url: Url,
    server_url: Url,
    server_id: String,
    client_id: String,
}
impl PlexClient {
    pub fn new(
        auth_url: &str,
        server_url: &str,
        server_id: &str,
        client_id: &str,
    ) -> Result<Self, AuthError> {
        if server_id.is_empty() || client_id.is_empty() {
            return Err(AuthError::Configuration);
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| AuthError::Configuration)?;
        Ok(Self {
            client,
            auth_url: endpoint(auth_url)?,
            server_url: endpoint(server_url)?,
            server_id: server_id.into(),
            client_id: client_id.into(),
        })
    }
    async fn json<T: serde::de::DeserializeOwned>(
        &self,
        method: reqwest::Method,
        base: &Url,
        path: &str,
        token: Option<&str>,
    ) -> Result<T, AuthError> {
        let url = base.join(path).map_err(|_| AuthError::Configuration)?;
        let mut request = self
            .client
            .request(method, url)
            .header("Accept", "application/json")
            .header("X-Plex-Product", "Pit Crew")
            .header("X-Plex-Client-Identifier", &self.client_id);
        if let Some(token) = token {
            request = request.header("X-Plex-Token", token);
        }
        let mut response = request.send().await.map_err(|_| AuthError::Unavailable)?;
        if !response.status().is_success() {
            return Err(AuthError::Unavailable);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| AuthError::Unavailable)? {
            if bytes.len() + chunk.len() > 1_048_576 {
                return Err(AuthError::Unavailable);
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| AuthError::Unavailable)
    }
}
fn endpoint(value: &str) -> Result<Url, AuthError> {
    let url = Url::parse(value).map_err(|_| AuthError::Configuration)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err(AuthError::Configuration);
    }
    Ok(url)
}
impl Auth for PlexClient {
    fn create_pin(&self) -> AuthFuture<'_, LoginPin> {
        Box::pin(async move {
            let pin: LoginPin = self
                .json(
                    reqwest::Method::POST,
                    &self.auth_url,
                    "api/v2/pins?strong=true",
                    None,
                )
                .await?;
            if pin.id == 0 || pin.code.is_empty() {
                return Err(AuthError::Unavailable);
            }
            Ok(pin)
        })
    }
    fn poll(&self, id: u64) -> AuthFuture<'_, Option<Account>> {
        Box::pin(async move {
            #[derive(Deserialize)]
            struct PinResult {
                #[serde(rename = "authToken")]
                token: Option<String>,
            }
            #[derive(Deserialize)]
            struct User {
                id: u64,
            }
            let pin: PinResult = self
                .json(
                    reqwest::Method::GET,
                    &self.auth_url,
                    &format!("api/v2/pins/{id}"),
                    None,
                )
                .await?;
            let Some(token) = pin.token.filter(|v| !v.is_empty()) else {
                return Ok(None);
            };
            let user: User = self
                .json(
                    reqwest::Method::GET,
                    &self.auth_url,
                    "api/v2/user",
                    Some(&token),
                )
                .await?;
            if user.id == 0 {
                return Err(AuthError::Unavailable);
            }
            Ok(Some(Account {
                id: user.id.to_string(),
                token,
            }))
        })
    }
    fn access<'a>(&'a self, token: &'a str) -> AuthFuture<'a, Vec<String>> {
        Box::pin(async move {
            #[derive(Deserialize)]
            struct Resource {
                #[serde(rename = "clientIdentifier")]
                id: String,
                provides: String,
            }
            let resources: Vec<Resource> = self
                .json(
                    reqwest::Method::GET,
                    &self.auth_url,
                    "api/v2/resources?includeHttps=1",
                    Some(token),
                )
                .await?;
            if !resources
                .iter()
                .any(|r| r.id == self.server_id && r.provides.split(',').any(|p| p == "server"))
            {
                return Err(AuthError::Denied);
            }
            #[derive(Deserialize)]
            struct Envelope {
                #[serde(rename = "MediaContainer")]
                container: Sections,
            }
            #[derive(Deserialize)]
            struct Sections {
                size: usize,
                #[serde(default, rename = "Directory")]
                entries: Vec<Section>,
            }
            #[derive(Deserialize)]
            struct Section {
                key: String,
            }
            let sections: Envelope = self
                .json(
                    reqwest::Method::GET,
                    &self.server_url,
                    "library/sections",
                    Some(token),
                )
                .await?;
            if sections.container.entries.len() != sections.container.size
                || sections.container.entries.iter().any(|s| s.key.is_empty())
            {
                return Err(AuthError::Unavailable);
            }
            Ok(sections
                .container
                .entries
                .into_iter()
                .map(|s| s.key)
                .collect())
        })
    }
}
