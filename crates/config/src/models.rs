//! Optional per-diagnosis model routing. No route is scheduled by default.
use crate::{ConfigError, Providers, invalid};
use serde::Deserialize;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    Broken,
    StaleAnalysis,
    DolbyVisionTranscode,
    SlowSource,
    WeakLink4k,
    Client,
    AudioLanguage,
    ReportedProblem,
    Outage,
    Subtitles,
    PlayOrigin,
    SelfResolved,
    Unknown,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Models {
    pub enabled: bool,
    pub routes: Vec<Route>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    /// The deterministic diagnosis selects a route before a model is called.
    pub kind: DecisionKind,
    pub provider: Provider,
    /// Required only for local/cloud OpenAI-compatible profiles.
    pub profile: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    pub cost_rates: Option<CostRates>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Rules,
    Local,
    Cloud,
    Jev,
    Anthropic,
}

/// Administrator-supplied estimates, in millionths of USD per million tokens.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostRates {
    pub input_microusd_per_million: u64,
    pub output_microusd_per_million: u64,
    pub cache_creation_microusd_per_million: u64,
    pub cache_read_microusd_per_million: u64,
}

fn default_timeout() -> u64 {
    30
}

impl Models {
    pub(crate) fn validate(&self, providers: &Providers) -> Result<(), ConfigError> {
        let mut seen = Vec::new();
        for route in &self.routes {
            if seen.contains(&route.kind) {
                return Err(invalid(
                    "models.routes.kind",
                    "each decision kind may appear only once",
                ));
            }
            seen.push(route.kind);
            if !(1..=120).contains(&route.timeout_seconds) {
                return Err(invalid(
                    "models.routes.timeout_seconds",
                    "must be between 1 and 120 seconds",
                ));
            }
            match route.provider {
                Provider::Local | Provider::Cloud => {
                    let profile = route.profile.as_ref().and_then(|name| {
                        providers.openai_compatible.iter().find(|p| &p.name == name)
                    });
                    if profile.is_none_or(|p| {
                        p.shadow || p.is_local() != (route.provider == Provider::Local)
                    }) {
                        return Err(invalid(
                            "models.routes.profile",
                            "must name a non-shadow profile matching local or cloud selection",
                        ));
                    }
                }
                Provider::Rules | Provider::Jev | Provider::Anthropic => {
                    if route.profile.is_some() {
                        return Err(invalid(
                            "models.routes.profile",
                            "only local and cloud selections use a profile",
                        ));
                    }
                    let native = match route.provider {
                        Provider::Jev => Some(&providers.jev),
                        Provider::Anthropic => Some(&providers.anthropic),
                        _ => None,
                    };
                    if native.is_some_and(|p| p.as_ref().is_none_or(|p| p.shadow)) {
                        return Err(invalid(
                            "models.routes.provider",
                            "selected native provider must be configured with shadow = false",
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}
