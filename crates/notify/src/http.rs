//! Explicit one-way HTTP protocols. Construction performs no network I/O.
//! A receipt confirms remote acceptance, not a person's reading the notification.
use crate::{AdminAlert, Notifier, NotifyError, NotifyFuture};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use pitcrew_domain::delivery::DeliveryOutcome;
use pitcrew_upstream::Limits;
use reqwest::Client;
use serde_json::{Value, json};
use std::time::Duration;
use url::Url;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// POST JSON `{key,title,body}`; acknowledgement is `{accepted:true,key}`.
    Webhook,
    /// POST plain text to a configured topic URL with a Title header.
    Ntfy,
}

/// The key is transmitted for audit/correlation. Neither protocol promises
/// physical deduplication, so the engine must never automatically resend.
pub struct HttpNotifier {
    client: Client,
    url: Url,
    bearer: Option<String>,
    protocol: Protocol,
    topic: Option<String>,
    limits: Limits,
}

impl HttpNotifier {
    pub fn new(
        protocol: Protocol,
        endpoint: &str,
        bearer: Option<&str>,
        limits: Limits,
    ) -> Result<Self, &'static str> {
        let url = Url::parse(endpoint).map_err(|_| "notifier URL is invalid")?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err("notifier URL must be HTTP(S) without userinfo, query or fragment");
        }
        if bearer.is_some_and(|token| {
            token.is_empty() || token.len() > 4096 || !token.bytes().all(|c| c.is_ascii_graphic())
        }) {
            return Err("notifier bearer credential is invalid");
        }
        if limits.timeout.is_zero()
            || limits.timeout > Duration::from_secs(3600)
            || limits.max_response_bytes == 0
            || limits.max_response_bytes > 64 * 1024 * 1024
        {
            return Err("notifier timeout or response limit is invalid");
        }
        let topic = if protocol == Protocol::Ntfy {
            let topic = url.path().rsplit('/').next().unwrap_or_default();
            if topic.is_empty()
                || !topic
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
            {
                return Err("ntfy requires a topic URL ending in an ASCII topic name");
            }
            Some(topic.into())
        } else {
            None
        };
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(limits.timeout)
            .build()
            .map_err(|_| "notifier HTTP client initialization failed")?;
        Ok(Self {
            client,
            url,
            bearer: bearer.map(str::to_owned),
            protocol,
            topic,
            limits,
        })
    }

    async fn deliver(&self, alert: &AdminAlert) -> Result<DeliveryOutcome, NotifyError> {
        if alert.title.is_empty()
            || alert.title.len() > 1024
            || alert.body.is_empty()
            || alert.body.len() > 64 * 1024
        {
            return Err(NotifyError::Unavailable);
        }
        let key = serde_json::to_value(&alert.key).map_err(|_| NotifyError::Unavailable)?;
        let encoded = URL_SAFE_NO_PAD.encode(key.to_string());
        let mut request = self
            .client
            .post(self.url.clone())
            .header("X-PitCrew-Delivery-Key", encoded);
        if let Some(token) = &self.bearer {
            request = request.bearer_auth(token);
        }
        request = match self.protocol {
            Protocol::Webhook => request.json(&json!({
                "key": key, "title": alert.title, "body": alert.body,
            })),
            Protocol::Ntfy => {
                if !alert
                    .title
                    .bytes()
                    .all(|c| c == b' ' || c.is_ascii_graphic())
                {
                    return Err(NotifyError::Unavailable);
                }
                request
                    .header("Title", &alert.title)
                    .header("Content-Type", "text/plain; charset=utf-8")
                    .body(alert.body.clone())
            }
        };
        let Ok(mut response) = request.send().await else {
            return Ok(DeliveryOutcome::Uncertain);
        };
        let status = response.status().as_u16();
        if status == 429 {
            return Ok(DeliveryOutcome::NotAccepted);
        }
        if (400..500).contains(&status) && status != 408 {
            return Ok(DeliveryOutcome::PermanentFailure);
        }
        if !(200..300).contains(&status) {
            return Ok(DeliveryOutcome::Uncertain);
        }
        let mut body = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if chunk.len() > self.limits.max_response_bytes.saturating_sub(body.len()) {
                        return Ok(DeliveryOutcome::Uncertain);
                    }
                    body.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(_) => return Ok(DeliveryOutcome::Uncertain),
            }
        }
        let receipt: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let accepted = match self.protocol {
            Protocol::Webhook => {
                receipt.get("accepted") == Some(&Value::Bool(true))
                    && receipt.get("key") == Some(&key)
            }
            Protocol::Ntfy => {
                receipt.get("event").and_then(Value::as_str) == Some("message")
                    && receipt.get("topic").and_then(Value::as_str) == self.topic.as_deref()
                    && receipt
                        .get("id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| !id.is_empty())
            }
        };
        Ok(if accepted {
            DeliveryOutcome::Confirmed
        } else {
            DeliveryOutcome::Uncertain
        })
    }
}

impl Notifier for HttpNotifier {
    fn idempotent_delivery(&self) -> bool {
        false
    }
    fn send<'a>(&'a self, alert: &'a AdminAlert) -> NotifyFuture<'a> {
        Box::pin(self.deliver(alert))
    }
}
