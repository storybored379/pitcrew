//! Plex proves account identity; only the configured allowlist grants admin.
use crate::{Error, Result, config::endpoint};
use serde::Deserialize;
use std::{future::Future, pin::Pin, time::Duration};
use url::Url;

pub type AuthFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;
#[derive(Deserialize)]
pub struct LoginPin {
    pub id: u64,
    pub code: String,
}
pub trait Auth: Send + Sync {
    fn start(&self) -> AuthFuture<'_, LoginPin>;
    fn poll(&self, pin: u64) -> AuthFuture<'_, Option<String>>;
}
pub struct PlexAuth {
    client: reqwest::Client,
    base: Url,
    client_id: String,
}
impl PlexAuth {
    pub fn new(base: &str, client_id: &str) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| Error::Input)?;
        Ok(Self {
            client,
            base: endpoint(base)?,
            client_id: client_id.into(),
        })
    }
    async fn json<T: serde::de::DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        token: Option<&str>,
    ) -> Result<T> {
        let mut request = self
            .client
            .request(method, self.base.join(path).map_err(|_| Error::Input)?)
            .header("Accept", "application/json")
            .header("X-Plex-Product", "Pit Crew Admin")
            .header("X-Plex-Client-Identifier", &self.client_id);
        if let Some(token) = token {
            request = request.header("X-Plex-Token", token);
        }
        let response = request.send().await.map_err(|_| Error::Upstream)?;
        if !response.status().is_success() {
            return Err(Error::Upstream);
        }
        let bytes = bounded(response).await?;
        serde_json::from_slice(&bytes).map_err(|_| Error::Upstream)
    }
}
pub(crate) async fn bounded(mut response: reqwest::Response) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Error::Upstream)? {
        if bytes.len() + chunk.len() > 65_536 {
            return Err(Error::Upstream);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
impl Auth for PlexAuth {
    fn start(&self) -> AuthFuture<'_, LoginPin> {
        Box::pin(async move {
            let pin: LoginPin = self
                .json(reqwest::Method::POST, "api/v2/pins?strong=true", None)
                .await?;
            if pin.id == 0 || pin.code.is_empty() {
                return Err(Error::Upstream);
            }
            Ok(pin)
        })
    }
    fn poll(&self, pin: u64) -> AuthFuture<'_, Option<String>> {
        Box::pin(async move {
            #[derive(Deserialize)]
            struct Poll {
                #[serde(rename = "authToken")]
                token: Option<String>,
            }
            #[derive(Deserialize)]
            struct User {
                id: u64,
            }
            let result: Poll = self
                .json(reqwest::Method::GET, &format!("api/v2/pins/{pin}"), None)
                .await?;
            let Some(token) = result.token.filter(|t| !t.is_empty()) else {
                return Ok(None);
            };
            let account: User = self
                .json(reqwest::Method::GET, "api/v2/user", Some(&token))
                .await?;
            if account.id == 0 {
                return Err(Error::Upstream);
            }
            // The Plex token is discarded; no owner/server token is needed.
            Ok(Some(account.id.to_string()))
        })
    }
}
