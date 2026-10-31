//! Runtime model routing. Fact-derived rule kinds select a provider; arbitrary
//! viewer text, model names and profile names never enter comparison records.
use crate::{Decider, DeciderKind, RulesDecider, providers::*};
use pitcrew_config::{Config, models};
use pitcrew_diagnosis::rules::DiagnosisKind;
use pitcrew_upstream::Transport;
use std::{sync::Arc, time::Duration};
use tokio::sync::{Semaphore, oneshot};

type Factory = dyn Fn() -> Result<Box<dyn Decider>, DeciderError> + Send + Sync;

pub struct Route {
    pub(crate) kind: DiagnosisKind,
    pub(crate) provider: DeciderKind,
    pub(crate) timeout: Duration,
    factory: Arc<Factory>,
    constructors: Arc<Semaphore>,
}
impl Route {
    pub(crate) async fn create(&self) -> Result<Box<dyn Decider>, DeciderError> {
        let permit = self
            .constructors
            .clone()
            .try_acquire_owned()
            .map_err(|_| DeciderError::Busy)?;
        let (send, receive) = oneshot::channel();
        let factory = self.factory.clone();
        // Never block the async worker or use spawn_blocking: a hung secret
        // mount must neither hold shutdown open nor create unbounded threads.
        // The detached worker retains its slot until the constructor returns.
        std::thread::Builder::new()
            .name("model-setup".into())
            .spawn(move || {
                let _permit = permit;
                let _ = send.send(factory());
            })
            .map_err(|_| DeciderError::Busy)?;
        receive.await.map_err(|_| DeciderError::Transport)?
    }
}

#[derive(Default)]
pub struct Schedule {
    routes: Vec<Route>,
}
impl Schedule {
    /// Construct without network activity or secret resolution. Missing secrets
    /// and provider construction failures are handled as rules fallback at use.
    pub fn from_config<T: Transport + Clone + 'static>(config: &Config, transport: T) -> Self {
        if !config.models.enabled {
            return Self::default();
        }
        let constructors = Arc::new(Semaphore::new(2));
        let routes = config
            .models
            .routes
            .iter()
            .map(|route| {
                let kind = kind(route.kind);
                let timeout = Duration::from_secs(route.timeout_seconds);
                let rates = route.cost_rates.map(|r| CostRates {
                    input_microusd_per_million: r.input_microusd_per_million,
                    output_microusd_per_million: r.output_microusd_per_million,
                    cache_creation_microusd_per_million: r.cache_creation_microusd_per_million,
                    cache_read_microusd_per_million: r.cache_read_microusd_per_million,
                });
                let consent = config.providers.cloud_opt_in;
                let transport = transport.clone();
                let (provider, factory): (_, Arc<Factory>) = match route.provider {
                    models::Provider::Rules => {
                        (DeciderKind::Rules, Arc::new(|| Ok(Box::new(RulesDecider))))
                    }
                    models::Provider::Local | models::Provider::Cloud => {
                        let profile = config
                            .providers
                            .openai_compatible
                            .iter()
                            .find(|p| Some(&p.name) == route.profile.as_ref())
                            .cloned();
                        (
                            DeciderKind::OpenAiCompatible,
                            Arc::new(move || {
                                let profile = profile.as_ref().ok_or(
                                    DeciderError::Configuration("selected profile unavailable"),
                                )?;
                                let key = profile.api_key.as_ref().map(resolve_key).transpose()?;
                                Ok(Box::new(openai::from_profile_with_policy(
                                    profile,
                                    consent,
                                    key.as_deref(),
                                    transport.clone(),
                                    Some(timeout),
                                    rates,
                                )?))
                            }),
                        )
                    }
                    models::Provider::Jev | models::Provider::Anthropic => {
                        let anthropic = route.provider == models::Provider::Anthropic;
                        let profile = if anthropic {
                            &config.providers.anthropic
                        } else {
                            &config.providers.jev
                        }
                        .clone();
                        let provider = if anthropic {
                            DeciderKind::Anthropic
                        } else {
                            DeciderKind::Jev
                        };
                        (
                            provider,
                            Arc::new(move || {
                                let profile = profile.as_ref().ok_or(
                                    DeciderError::Configuration("selected provider unavailable"),
                                )?;
                                let key = resolve_key(&profile.api_key)?;
                                let config = ProviderConfig {
                                    cloud_opt_in: consent,
                                    endpoint: profile.endpoint.clone(),
                                    model: profile.model.clone(),
                                    timeout,
                                    cost_rates: rates,
                                    ..ProviderConfig::default()
                                };
                                if anthropic {
                                    Ok(Box::new(ClaudeDecider::new(
                                        config,
                                        &key,
                                        transport.clone(),
                                    )?))
                                } else {
                                    Ok(Box::new(JevDecider::new(config, &key, transport.clone())?))
                                }
                            }),
                        )
                    }
                };
                Route {
                    kind,
                    provider,
                    timeout,
                    factory,
                    constructors: constructors.clone(),
                }
            })
            .collect();
        Self { routes }
    }

    pub(crate) fn route(&self, kind: DiagnosisKind) -> Option<&Route> {
        self.routes.iter().find(|route| route.kind == kind)
    }
}

fn resolve_key(reference: &pitcrew_config::SecretRef) -> Result<String, DeciderError> {
    reference
        .resolve("models.api_key")
        .map(|key| key.expose().to_owned())
        .map_err(|_| DeciderError::Configuration("selected provider credential unavailable"))
}

fn kind(kind: models::DecisionKind) -> DiagnosisKind {
    use models::DecisionKind as C;
    match kind {
        C::Broken => DiagnosisKind::Broken,
        C::StaleAnalysis => DiagnosisKind::StaleAnalysis,
        C::DolbyVisionTranscode => DiagnosisKind::DolbyVisionTranscode,
        C::SlowSource => DiagnosisKind::SlowSource,
        C::WeakLink4k => DiagnosisKind::WeakLink4k,
        C::Client => DiagnosisKind::Client,
        C::AudioLanguage => DiagnosisKind::AudioLanguage,
        C::ReportedProblem => DiagnosisKind::ReportedProblem,
        C::Outage => DiagnosisKind::Outage,
        C::Subtitles => DiagnosisKind::Subtitles,
        C::PlayOrigin => DiagnosisKind::PlayOrigin,
        C::SelfResolved => DiagnosisKind::SelfResolved,
        C::Unknown => DiagnosisKind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        LocalEvidence,
        shadow::{DecisionRunner, JsonlSink},
    };
    use pitcrew_diagnosis::facts::{DecodeOutcome, EvidenceFacts};

    #[tokio::test]
    async fn blocked_provider_construction_obeys_the_route_deadline() {
        let schedule = Schedule {
            routes: vec![Route {
                kind: DiagnosisKind::Broken,
                provider: DeciderKind::OpenAiCompatible,
                timeout: Duration::from_millis(20),
                constructors: Arc::new(Semaphore::new(2)),
                factory: Arc::new(|| {
                    // A synthetic slow secret source; never touch real storage.
                    std::thread::sleep(Duration::from_millis(200));
                    Ok(Box::new(RulesDecider))
                }),
            }],
        };
        let mut facts = EvidenceFacts::default();
        facts.playback.decode = DecodeOutcome::Bad;
        let result = DecisionRunner::with_schedule(schedule)
            .run(
                &LocalEvidence {
                    facts,
                    viewer_text: String::new(),
                    private_context: serde_json::Value::Null,
                },
                &mut JsonlSink(Vec::new()),
            )
            .await
            .unwrap();
        assert!(result.used_rules_fallback);
        assert_eq!(result.entries[0].accounting.unwrap().requests_started, 0);
    }

    #[tokio::test]
    async fn stuck_constructors_retain_slots_and_bound_detached_workers() {
        use std::sync::{
            Condvar, Mutex,
            atomic::{AtomicUsize, Ordering},
        };
        struct Release(Arc<(Mutex<bool>, Condvar)>);
        impl Drop for Release {
            fn drop(&mut self) {
                *self.0.0.lock().unwrap() = true;
                self.0.1.notify_all();
            }
        }
        let release = Release(Arc::new((Mutex::new(false), Condvar::new())));
        let gate = release.0.clone();
        let started = Arc::new(AtomicUsize::new(0));
        let count = started.clone();
        let constructors = Arc::new(Semaphore::new(2));
        let schedule = Schedule {
            routes: vec![Route {
                kind: DiagnosisKind::Broken,
                provider: DeciderKind::OpenAiCompatible,
                timeout: Duration::from_millis(20),
                constructors: constructors.clone(),
                factory: Arc::new(move || {
                    count.fetch_add(1, Ordering::SeqCst);
                    let mut released = gate.0.lock().unwrap();
                    while !*released {
                        released = gate.1.wait(released).unwrap();
                    }
                    Ok(Box::new(RulesDecider))
                }),
            }],
        };
        let mut facts = EvidenceFacts::default();
        facts.playback.decode = DecodeOutcome::Bad;
        let local = LocalEvidence {
            facts,
            viewer_text: String::new(),
            private_context: serde_json::Value::Null,
        };
        let runner = DecisionRunner::with_schedule(schedule);
        for _ in 0..5 {
            let result = runner
                .run(&local, &mut JsonlSink(Vec::new()))
                .await
                .unwrap();
            assert!(result.used_rules_fallback);
            assert_eq!(result.entries[0].accounting.unwrap().requests_started, 0);
        }
        assert_eq!(started.load(Ordering::SeqCst), 2);
        assert_eq!(constructors.available_permits(), 0);
        drop(release);
        tokio::time::timeout(Duration::from_secs(1), async {
            while constructors.available_permits() != 2 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            !runner
                .run(&local, &mut JsonlSink(Vec::new()))
                .await
                .unwrap()
                .used_rules_fallback
        );
    }

    #[tokio::test]
    async fn request_uses_only_the_time_left_after_construction() {
        struct Slow;
        impl crate::Decider for Slow {
            fn kind(&self) -> DeciderKind {
                DeciderKind::OpenAiCompatible
            }
            fn decide<'a>(
                &'a self,
                evidence: &'a crate::RedactedEvidence,
            ) -> crate::DecisionFuture<'a> {
                Box::pin(async move {
                    tokio::time::sleep(Duration::from_millis(60)).await;
                    RulesDecider.decide(evidence).await
                })
            }
        }
        let schedule = Schedule {
            routes: vec![Route {
                kind: DiagnosisKind::Broken,
                provider: DeciderKind::OpenAiCompatible,
                timeout: Duration::from_millis(80),
                constructors: Arc::new(Semaphore::new(2)),
                factory: Arc::new(|| {
                    std::thread::sleep(Duration::from_millis(40));
                    Ok(Box::new(Slow))
                }),
            }],
        };
        let mut facts = EvidenceFacts::default();
        facts.playback.decode = DecodeOutcome::Bad;
        let local = LocalEvidence {
            facts,
            viewer_text: String::new(),
            private_context: serde_json::Value::Null,
        };
        let result = DecisionRunner::with_schedule(schedule)
            .run(&local, &mut JsonlSink(Vec::new()))
            .await
            .unwrap();
        assert!(result.used_rules_fallback);
    }

    #[tokio::test]
    async fn ready_constructor_cannot_dispatch_after_a_starved_runtime_misses_deadline() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Counted(Arc<AtomicUsize>);
        impl crate::Decider for Counted {
            fn kind(&self) -> DeciderKind {
                DeciderKind::OpenAiCompatible
            }
            fn decide<'a>(
                &'a self,
                evidence: &'a crate::RedactedEvidence,
            ) -> crate::DecisionFuture<'a> {
                self.0.fetch_add(1, Ordering::SeqCst);
                RulesDecider.decide(evidence)
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let (ready, constructed) = std::sync::mpsc::channel();
        let schedule = Schedule {
            routes: vec![Route {
                kind: DiagnosisKind::Broken,
                provider: DeciderKind::OpenAiCompatible,
                timeout: Duration::from_millis(20),
                constructors: Arc::new(Semaphore::new(2)),
                factory: Arc::new(move || {
                    ready.send(()).unwrap();
                    Ok(Box::new(Counted(count.clone())))
                }),
            }],
        };
        // The constructor completes off-thread while this task deliberately
        // starves the current-thread runtime beyond the absolute deadline.
        let blocker = tokio::spawn(async move {
            constructed.recv_timeout(Duration::from_secs(1)).unwrap();
            std::thread::sleep(Duration::from_millis(100));
        });
        let mut facts = EvidenceFacts::default();
        facts.playback.decode = DecodeOutcome::Bad;
        let local = LocalEvidence {
            facts,
            viewer_text: String::new(),
            private_context: serde_json::Value::Null,
        };
        let result = DecisionRunner::with_schedule(schedule)
            .run(&local, &mut JsonlSink(Vec::new()))
            .await
            .unwrap();
        blocker.await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(result.used_rules_fallback);
    }
}
