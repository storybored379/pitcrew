use super::{Accounting, CostRates, DeciderError, ProviderConfig, Usage};
use pitcrew_upstream::{Limits, Method, Request, Transport};
use serde::Deserialize;
use serde_json::Value;
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::Mutex,
    time::Duration,
};
use tokio::sync::Semaphore;
use url::Url;

/// Wire dialect of a provider. Authentication headers, version validation and
/// the usage envelope differ; the request body is built by each decider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Flavor {
    Anthropic,
    /// Jev: bearer credential, Anthropic-style usage fields.
    Bearer,
    /// Any OpenAI-style chat completions server (cloud or local).
    OpenAi {
        local: bool,
    },
}

/// No Debug implementation: this object holds a credential and configured URL.
pub(super) struct CloudClient<T> {
    pub config: ProviderConfig,
    transport: T,
    url: Url,
    headers: Vec<(String, String)>,
    flavor: Flavor,
    slots: Semaphore,
    accounting: Mutex<Accounting>,
}

impl<T: Transport> CloudClient<T> {
    pub fn new(
        config: ProviderConfig,
        api_key: &str,
        transport: T,
        anthropic: bool,
    ) -> Result<Self, DeciderError> {
        let flavor = if anthropic {
            Flavor::Anthropic
        } else {
            Flavor::Bearer
        };
        Self::with_flavor(config, Some(api_key), transport, flavor)
    }

    pub fn with_flavor(
        config: ProviderConfig,
        api_key: Option<&str>,
        transport: T,
        flavor: Flavor,
    ) -> Result<Self, DeciderError> {
        let local = flavor == Flavor::OpenAi { local: true };
        // A local model never leaves the administrator's network, so it needs no
        // cloud consent; the endpoint check below enforces that it is local.
        if !local && !config.cloud_opt_in {
            return Err(DeciderError::ConsentRequired);
        }
        let url = Url::parse(&config.endpoint)
            .map_err(|_| DeciderError::Configuration("endpoint must be an HTTPS URL"))?;
        let scheme_ok = url.scheme() == "https" || (local && url.scheme() == "http");
        if !scheme_ok
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(DeciderError::Configuration(
                "endpoint requires HTTPS without credentials, query or fragment",
            ));
        }
        if local && !url.host().is_some_and(|host| local_host(&host)) {
            return Err(DeciderError::Configuration(
                "a local provider endpoint must be a loopback, private or single-label host",
            ));
        }
        match api_key {
            Some(key)
                if key.is_empty()
                    || key.len() > 4096
                    || !key.bytes().all(|b| b.is_ascii_graphic()) =>
            {
                return Err(DeciderError::Configuration("invalid provider credential"));
            }
            None if !local => {
                return Err(DeciderError::Configuration("invalid provider credential"));
            }
            _ => {}
        }
        // OpenAI-style model ids also use ':' (local tags) and '/' (routers).
        let model_byte_ok = |b: u8| {
            b.is_ascii_alphanumeric()
                || b"-_.".contains(&b)
                || (matches!(flavor, Flavor::OpenAi { .. }) && b":/@".contains(&b))
        };
        if config.model.is_empty()
            || config.model.len() > 128
            || !config.model.bytes().all(model_byte_ok)
        {
            return Err(DeciderError::Configuration("invalid model identifier"));
        }
        // OpenAI-style servers have no dated version header.
        if !matches!(flavor, Flavor::OpenAi { .. })
            && (config.api_version.len() != 10
                || !config
                    .api_version
                    .bytes()
                    .enumerate()
                    .all(|(i, b)| match i {
                        4 | 7 => b == b'-',
                        _ => b.is_ascii_digit(),
                    }))
        {
            return Err(DeciderError::Configuration("invalid API version"));
        }
        if config.timeout.is_zero()
            || config.timeout > Duration::from_secs(120)
            || !(1..=1024 * 1024).contains(&config.max_request_bytes)
            || !(1..=1024 * 1024).contains(&config.max_response_bytes)
            || !(1..=8192).contains(&config.max_output_tokens)
            || !(1..=16).contains(&config.max_concurrent_requests)
        {
            return Err(DeciderError::Configuration("invalid cloud resource limits"));
        }
        let mut headers = vec![("Accept".into(), "application/json".into())];
        match (flavor, api_key) {
            (Flavor::Anthropic, Some(key)) => {
                headers.push(("x-api-key".into(), key.into()));
                headers.push(("anthropic-version".into(), config.api_version.clone()));
            }
            (_, Some(key)) => headers.push(("Authorization".into(), format!("Bearer {key}"))),
            (_, None) => {}
        }
        let accounting = Accounting {
            usage: Usage {
                estimated_cost_microusd: config.cost_rates.map(|_| 0),
                ..Usage::default()
            },
            ..Accounting::default()
        };
        Ok(Self {
            slots: Semaphore::new(config.max_concurrent_requests),
            config,
            transport,
            url,
            headers,
            flavor,
            accounting: Mutex::new(accounting),
        })
    }

    pub fn accounting(&self) -> Accounting {
        *self.accounting.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub async fn request(&self, body: Value) -> Result<(Value, Usage), DeciderError> {
        if body.to_string().len() > self.config.max_request_bytes {
            return Err(DeciderError::TooLarge);
        }
        // No unbounded queue of waiting tasks. The caller may fall back to rules.
        let _slot = self.slots.try_acquire().map_err(|_| DeciderError::Busy)?;
        let request = Request {
            method: Method::Post,
            url: self.url.clone(),
            headers: self.headers.clone(),
            body: Some(body),
            limits: Limits {
                timeout: self.config.timeout,
                max_response_bytes: self.config.max_response_bytes,
            },
        };
        {
            let mut accounting = self.accounting.lock().unwrap_or_else(|e| e.into_inner());
            accounting.requests_started = accounting.requests_started.saturating_add(1);
        }
        let response = tokio::time::timeout(self.config.timeout, self.transport.send(request))
            .await
            .map_err(|_| DeciderError::Timeout)?
            .map_err(map_transport_error)?;
        if !(200..300).contains(&response.status) {
            return Err(DeciderError::Status(response.status));
        }
        if response.body.len() > self.config.max_response_bytes {
            return Err(DeciderError::TooLarge);
        }
        let response: Value =
            serde_json::from_slice(&response.body).map_err(|_| DeciderError::InvalidResponse)?;
        let usage = match self.flavor {
            Flavor::OpenAi { local } => {
                match serde_json::from_value::<OpenAiUsage>(response["usage"].clone()) {
                    Ok(wire) => wire.normalize()?.account(self.config.cost_rates),
                    // Some local servers omit usage. Nothing is billed locally, so
                    // the call stays counted as started but has no usage record.
                    Err(_) if local && response["usage"].is_null() => {
                        return Ok((response, Usage::default()));
                    }
                    Err(_) => return Err(DeciderError::InvalidResponse),
                }
            }
            _ => serde_json::from_value::<WireUsage>(response["usage"].clone())
                .map_err(|_| DeciderError::InvalidResponse)?
                .account(self.config.cost_rates),
        };
        {
            let mut accounting = self.accounting.lock().unwrap_or_else(|e| e.into_inner());
            accounting.responses_with_usage = accounting.responses_with_usage.saturating_add(1);
            accounting.usage.add(usage);
        }
        Ok((response, usage))
    }
}

fn map_transport_error(error: pitcrew_upstream::Error) -> DeciderError {
    use pitcrew_upstream::Error;
    match error {
        Error::Timeout => DeciderError::Timeout,
        Error::Status(status) => DeciderError::Status(status),
        Error::TooLarge => DeciderError::TooLarge,
        _ => DeciderError::Transport,
    }
}

#[derive(Deserialize)]
struct WireUsage {
    input_tokens: u64,
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

impl WireUsage {
    fn account(self, rates: Option<CostRates>) -> Usage {
        let mut usage = Usage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_creation_input_tokens: self.cache_creation_input_tokens,
            cache_read_input_tokens: self.cache_read_input_tokens,
            estimated_cost_microusd: None,
        };
        usage.estimated_cost_microusd = rates.and_then(|r| r.estimate(usage));
        usage
    }
}

/// OpenAI reports cached prompt tokens inside `prompt_tokens`; DeepSeek reports
/// them as `prompt_cache_hit_tokens`. Cached tokens are split out so they are
/// never priced twice or at the uncached rate.
#[derive(Deserialize)]
struct OpenAiUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<PromptDetails>,
    #[serde(default)]
    prompt_cache_hit_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct PromptDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

impl OpenAiUsage {
    fn normalize(self) -> Result<WireUsage, DeciderError> {
        let cached = self
            .prompt_cache_hit_tokens
            .or(self.prompt_tokens_details.and_then(|d| d.cached_tokens))
            .unwrap_or(0);
        Ok(WireUsage {
            input_tokens: self
                .prompt_tokens
                .checked_sub(cached)
                .ok_or(DeciderError::InvalidResponse)?,
            output_tokens: self.completion_tokens,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: cached,
        })
    }
}

/// Loopback, private (RFC 1918, CGNAT/tailnet, ULA, link-local) addresses and
/// names that cannot be public DNS names: `localhost`, single-label container
/// names and the reserved local suffixes.
fn local_host(host: &url::Host<&str>) -> bool {
    match host {
        url::Host::Ipv4(ip) => local_ip(IpAddr::V4(*ip)),
        url::Host::Ipv6(ip) => local_ip(IpAddr::V6(*ip)),
        url::Host::Domain(name) => {
            let name = name.trim_end_matches('.').to_ascii_lowercase();
            name == "localhost"
                || !name.contains('.')
                || [".localhost", ".local", ".lan", ".internal", ".home.arpa"]
                    .iter()
                    .any(|suffix| name.ends_with(suffix))
        }
    }
}

pub(super) fn local_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || (v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]))
        }
        IpAddr::V6(v6) => {
            v6 == Ipv6Addr::LOCALHOST
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4: Ipv4Addr| local_ip(IpAddr::V4(v4)))
        }
    }
}
