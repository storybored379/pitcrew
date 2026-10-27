//! Bounded HTTP boundary shared by the arr, Plex and P1-agent clients.
//! Credentials, URLs, response bodies and transport errors are never formatted.
pub mod agent;

use serde::de::DeserializeOwned;
use serde_json::Value;
use std::{future::Future, pin::Pin, time::Duration};
use url::Url;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("invalid upstream configuration: {0}")]
    Configuration(&'static str),
    #[error("invalid upstream request")]
    InvalidInput,
    #[error("writes require explicit live mode and a durable store")]
    ShadowDenied,
    #[error("upstream deadline exceeded; a submitted write may have succeeded")]
    Timeout,
    #[error("upstream transport failed; a submitted write may have succeeded")]
    Transport,
    #[error("upstream HTTP status {0}")]
    Status(u16),
    #[error("upstream response exceeded size limit")]
    TooLarge,
    #[error("upstream response has an unsupported shape")]
    InvalidResponse,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Shadow,
    Live,
}

/// A runtime gate, not a budget/ownership authorization. The durable engine must
/// still persist intent, reserve budget and apply its domain rails before a write.
pub struct LivePermit(());
impl LivePermit {
    pub fn authorize(mode: Mode, durable_store_available: bool) -> Result<Self, Error> {
        if mode != Mode::Live || !durable_store_available {
            return Err(Error::ShadowDenied);
        }
        Ok(Self(()))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub timeout: Duration,
    pub max_response_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            max_response_bytes: 4 * 1024 * 1024,
        }
    }
}

/// Deliberately has no Debug/Serialize implementation.
pub struct Auth {
    name: &'static str,
    value: String,
}
impl Auth {
    fn header(name: &'static str, secret: &str, prefix: &str) -> Result<Self, Error> {
        if secret.is_empty() || secret.len() > 4096 || !secret.bytes().all(|b| b.is_ascii_graphic())
        {
            return Err(Error::Configuration(
                "credential must be 1..4096 visible ASCII bytes",
            ));
        }
        Ok(Self {
            name,
            value: format!("{prefix}{secret}"),
        })
    }
    pub fn arr(secret: &str) -> Result<Self, Error> {
        Self::header("X-Api-Key", secret, "")
    }
    pub fn plex(secret: &str) -> Result<Self, Error> {
        Self::header("X-Plex-Token", secret, "")
    }
    pub fn bearer(secret: &str) -> Result<Self, Error> {
        Self::header("Authorization", secret, "Bearer ")
    }
}

pub struct Endpoint {
    base: Url,
    auth: Auth,
    limits: Limits,
}
impl Endpoint {
    pub fn new(base: &str, auth: Auth, limits: Limits) -> Result<Self, Error> {
        let mut base = Url::parse(base)
            .map_err(|_| Error::Configuration("base_url must be an absolute HTTP(S) URL"))?;
        if !matches!(base.scheme(), "http" | "https")
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(Error::Configuration(
                "base_url must be HTTP(S), without credentials, query or fragment",
            ));
        }
        if limits.timeout.is_zero()
            || limits.timeout > Duration::from_secs(3600)
            || limits.max_response_bytes == 0
            || limits.max_response_bytes > 64 * 1024 * 1024
        {
            return Err(Error::Configuration(
                "limits require timeout 1ns..1h and response size 1..64MiB",
            ));
        }
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        Ok(Self { base, auth, limits })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Delete,
}

/// Sensitive request data is only available to the transport; never log it.
pub struct Request {
    pub method: Method,
    pub url: Url,
    pub headers: Vec<(String, String)>,
    pub body: Option<Value>,
    pub limits: Limits,
}
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}
pub type ResponseFuture<'a> = Pin<Box<dyn Future<Output = Result<Response, Error>> + Send + 'a>>;
pub trait Transport: Send + Sync {
    fn send(&self, request: Request) -> ResponseFuture<'_>;
}

/// Does no network work until `send`. No redirects, ambient proxies, retries,
/// cookies or URL credentials. TLS uses normal certificate verification.
pub struct HttpTransport(reqwest::Client);
impl HttpTransport {
    pub fn new() -> Result<Self, Error> {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .build()
            .map(Self)
            .map_err(|_| Error::Configuration("HTTP client initialization failed"))
    }
}
impl Transport for HttpTransport {
    fn send(&self, request: Request) -> ResponseFuture<'_> {
        Box::pin(async move {
            let method = match request.method {
                Method::Get => reqwest::Method::GET,
                Method::Post => reqwest::Method::POST,
                Method::Delete => reqwest::Method::DELETE,
            };
            let mut builder = self
                .0
                .request(method, request.url)
                .timeout(request.limits.timeout);
            for (key, value) in request.headers {
                builder = builder.header(key, value);
            }
            if let Some(body) = request.body {
                builder = builder
                    .header("Content-Type", "application/json")
                    .body(body.to_string());
            }
            let mut response = builder.send().await.map_err(classify)?;
            let status = response.status().as_u16();
            if !(200..300).contains(&status) {
                return Err(Error::Status(status));
            }
            if response
                .content_length()
                .is_some_and(|n| n > request.limits.max_response_bytes as u64)
            {
                return Err(Error::TooLarge);
            }
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(classify)? {
                if chunk.len() > request.limits.max_response_bytes.saturating_sub(body.len()) {
                    return Err(Error::TooLarge);
                }
                body.extend_from_slice(&chunk);
            }
            Ok(Response { status, body })
        })
    }
}
fn classify(error: reqwest::Error) -> Error {
    if error.is_timeout() {
        Error::Timeout
    } else {
        Error::Transport
    }
}

pub struct JsonClient<T> {
    endpoint: Endpoint,
    transport: T,
}
impl<T: Transport> JsonClient<T> {
    pub fn new(endpoint: Endpoint, transport: T) -> Self {
        Self {
            endpoint,
            transport,
        }
    }

    pub async fn get<R: DeserializeOwned>(
        &self,
        path: &str,
        query: Vec<(String, String)>,
    ) -> Result<R, Error> {
        self.request(Method::Get, path, query, None).await
    }
    pub async fn write<R: DeserializeOwned>(
        &self,
        _permit: &LivePermit,
        method: Method,
        path: &str,
        query: Vec<(String, String)>,
        body: Option<Value>,
    ) -> Result<R, Error> {
        if method == Method::Get {
            return Err(Error::InvalidInput);
        }
        self.request(method, path, query, body).await
    }
    async fn request<R: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        query: Vec<(String, String)>,
        body: Option<Value>,
    ) -> Result<R, Error> {
        // Paths are client-owned static segments plus numeric identifiers. Never
        // allow an untrusted field to change origin or escape a proxy prefix.
        if path.is_empty()
            || path.starts_with('/')
            || path.split('/').any(|p| p == ".." || p == ".")
            || !path
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/_-".contains(&b))
        {
            return Err(Error::InvalidInput);
        }
        let mut url = self
            .endpoint
            .base
            .join(path)
            .map_err(|_| Error::InvalidInput)?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        let request = Request {
            method,
            url,
            headers: vec![
                (
                    self.endpoint.auth.name.into(),
                    self.endpoint.auth.value.clone(),
                ),
                ("Accept".into(), "application/json".into()),
            ],
            body,
            limits: self.endpoint.limits,
        };
        let response =
            tokio::time::timeout(self.endpoint.limits.timeout, self.transport.send(request))
                .await
                .map_err(|_| Error::Timeout)??;
        if !(200..300).contains(&response.status) {
            return Err(Error::Status(response.status));
        }
        if response.body.len() > self.endpoint.limits.max_response_bytes {
            return Err(Error::TooLarge);
        }
        // Empty 2xx bodies are valid for DELETE and some command versions.
        let bytes = if response.body.is_empty() {
            b"null"
        } else {
            response.body.as_slice()
        };
        serde_json::from_slice(bytes).map_err(|_| Error::InvalidResponse)
    }
}
