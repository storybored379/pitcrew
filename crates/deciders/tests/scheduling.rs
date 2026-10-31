use pitcrew_config::Config;
use pitcrew_deciders::{
    DeciderKind, LocalEvidence,
    schedule::Schedule,
    shadow::{DecisionRunner, JsonlSink},
};
use pitcrew_diagnosis::{facts::*, rules::*};
use pitcrew_upstream::{Request, Response, ResponseFuture, Transport};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Fake {
    requests: Arc<Mutex<Vec<Request>>>,
    response: Option<Value>,
    status: u16,
    transport_error: bool,
}
impl Transport for Fake {
    fn send(&self, request: Request) -> ResponseFuture<'_> {
        self.requests.lock().unwrap().push(request);
        Box::pin(async move {
            if self.transport_error {
                return Err(pitcrew_upstream::Error::Transport);
            }
            let Some(response) = &self.response else {
                return std::future::pending().await;
            };
            Ok(Response {
                status: self.status,
                body: serde_json::to_vec(response).unwrap(),
            })
        })
    }
}
fn evidence() -> LocalEvidence {
    let mut facts = EvidenceFacts::default();
    facts.playback.decode = DecodeOutcome::Bad;
    LocalEvidence {
        facts,
        viewer_text: "synthetic-private-text".into(),
        private_context: json!({"private":"synthetic-private-context"}),
    }
}
fn answer() -> Value {
    json!({"root_cause":"bad_file","cause_confidence":0.9,"fix":"redownload","fix_confidence":0.8})
}
fn envelope(answer: Value) -> Value {
    json!({"choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":answer.to_string()}}],"usage":{"prompt_tokens":123,"completion_tokens":45,"prompt_tokens_details":{"cached_tokens":23}}})
}
fn config(enabled: bool, kind: &str) -> Config {
    Config::parse(&format!(
        r#"version=1
[models]
enabled={enabled}
[[models.routes]]
kind="{kind}"
provider="local"
profile="fixture"
timeout_seconds=2
[models.routes.cost_rates]
input_microusd_per_million=1000000
output_microusd_per_million=2000000
cache_creation_microusd_per_million=3000000
cache_read_microusd_per_million=4000000
[[providers.openai_compatible]]
name="fixture"
preset="ollama"
model="fixture-model"
shadow=false
"#
    ))
    .unwrap()
}
async fn run(
    config: &Config,
    fake: &Fake,
    local: &LocalEvidence,
) -> pitcrew_deciders::shadow::Comparison {
    DecisionRunner::with_schedule(Schedule::from_config(config, fake.clone()))
        .run(local, &mut JsonlSink(Vec::new()))
        .await
        .unwrap()
}

#[tokio::test]
async fn disabled_unmatched_and_explicit_rules_routes_make_no_calls() {
    let fake = Fake::default();
    for config in [config(false, "broken"), config(true, "unknown"), Config::parse("version=1\n[models]\nenabled=true\n[[models.routes]]\nkind=\"broken\"\nprovider=\"rules\"\n").unwrap()] {
        let runner = DecisionRunner::with_schedule(Schedule::from_config(&config, fake.clone()));
        assert!(!runner.uses_model(&evidence().facts));
        let result = runner.run(&evidence(), &mut JsonlSink(Vec::new())).await.unwrap();
        assert_eq!(result.primary, DeciderKind::Rules);
        assert_eq!(result.selected.decision, classify(&evidence().facts));
        assert!(!result.used_rules_fallback);
        assert_eq!(result.entries[0].accounting, None);
    }
    assert!(fake.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn selected_local_route_is_redacted_bounded_and_accounts_each_attempt() {
    let fake = Fake {
        response: Some(envelope(answer())),
        status: 200,
        ..Fake::default()
    };
    let config = config(true, "broken");
    let runner = DecisionRunner::with_schedule(Schedule::from_config(&config, fake.clone()));
    assert!(runner.uses_model(&evidence().facts));
    for _ in 0..2 {
        let result = runner
            .run(&evidence(), &mut JsonlSink(Vec::new()))
            .await
            .unwrap();
        assert_eq!(result.primary, DeciderKind::OpenAiCompatible);
        assert!(!result.used_rules_fallback);
        assert_eq!(result.selected.decision, classify(&evidence().facts));
        let accounting = result.entries[0].accounting.unwrap();
        assert_eq!(accounting.requests_started, 1);
        assert_eq!(accounting.responses_with_usage, 1);
        assert_eq!(accounting.usage.input_tokens, 100);
        assert_eq!(accounting.usage.output_tokens, 45);
        assert_eq!(accounting.usage.cache_read_input_tokens, 23);
        assert_eq!(accounting.usage.estimated_cost_microusd, Some(282));
        assert_eq!(result.selected.usage, accounting.usage);
    }
    let requests = fake.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let request = &requests[0];
    assert_eq!(request.limits.timeout, std::time::Duration::from_secs(2));
    assert_eq!(request.url.path(), "/v1/chat/completions");
    assert_eq!(request.body.as_ref().unwrap()["model"], "fixture-model");
    let text = request.body.as_ref().unwrap().to_string();
    assert!(!text.contains("synthetic-private"));
}

#[tokio::test]
async fn timeout_and_provider_failures_fall_back_with_honest_usage() {
    for fake in [
        Fake::default(),
        Fake {
            response: Some(json!({})),
            status: 503,
            ..Fake::default()
        },
        Fake {
            transport_error: true,
            ..Fake::default()
        },
        Fake {
            response: Some(envelope(json!({"invalid":"private response"}))),
            status: 200,
            ..Fake::default()
        },
    ] {
        let before = tokio::time::Instant::now();
        let result = run(&config(true, "broken"), &fake, &evidence()).await;
        assert!(tokio::time::Instant::now() - before < std::time::Duration::from_secs(3));
        assert!(result.used_rules_fallback);
        assert_eq!(result.selected.decision, classify(&evidence().facts));
        assert_eq!(result.selected.usage, Default::default());
        assert!(result.entries[0].failed);
        let accounting = result.entries[0].accounting.unwrap();
        assert_eq!(accounting.requests_started, 1);
        assert_eq!(
            accounting.responses_with_usage,
            u64::from(fake.status == 200)
        );
        if fake.status == 200 {
            assert_eq!(accounting.usage.estimated_cost_microusd, Some(282));
        }
        assert!(
            !serde_json::to_string(&result)
                .unwrap()
                .contains("private response")
        );
    }
}

#[tokio::test]
async fn missing_credentials_and_invalid_endpoint_are_rules_fallback_without_requests() {
    let fake = Fake::default();
    let mut config = config(true, "broken");
    config.providers.openai_compatible[0].api_key = Some(pitcrew_config::SecretRef {
        env: None,
        file: Some("/nonexistent-fixture-secret".into()),
    });
    let result = run(&config, &fake, &evidence()).await;
    assert!(result.used_rules_fallback);
    assert_eq!(result.entries[0].accounting.unwrap().requests_started, 0);
    config.providers.openai_compatible[0].api_key = None;
    config.providers.openai_compatible[0].endpoint =
        Some("https://public.example.com/v1/chat/completions".into());
    let result = run(&config, &fake, &evidence()).await;
    assert!(result.used_rules_fallback);
    assert!(fake.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_model_cannot_add_replacement_or_suppress_urgent_or_injection_review() {
    let fake = Fake {
        response: Some(envelope(answer())),
        status: 200,
        ..Fake::default()
    };
    let mut local = evidence();
    local.facts = EvidenceFacts::default();
    let result = run(&config(true, "unknown"), &fake, &local).await;
    assert!(result.used_rules_fallback);
    assert_eq!(result.selected.decision, classify(&local.facts));
    let fake = Fake {
        response: Some(envelope(
            json!({"root_cause":"unknown","cause_confidence":0.9,"fix":"wait_and_watch","fix_confidence":0.8}),
        )),
        status: 200,
        ..Fake::default()
    };
    local.facts.complaint = Complaint::EverythingBroken;
    local.facts.server_health = ServerHealth::Unavailable;
    let result = run(&config(true, "outage"), &fake, &local).await;
    assert!(result.used_rules_fallback);
    assert_eq!(
        result.selected.decision.action,
        RecommendedAction::NotifyUrgent
    );
    local.facts.injection_suspected = true;
    let result = run(&config(true, "unknown"), &fake, &local).await;
    assert!(result.used_rules_fallback);
    assert_eq!(
        result.selected.decision.reason,
        Reason::UntrustedInstructions
    );
}

#[tokio::test]
async fn native_and_compatible_cloud_routes_preserve_model_secret_timeout_and_usage() {
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("model-key");
    std::fs::write(&secret, "synthetic-model-key").unwrap();
    for provider in ["jev", "anthropic", "cloud"] {
        let profile = if provider == "cloud" {
            format!(
                "[[providers.openai_compatible]]\nname=\"fixture\"\npreset=\"openai\"\nmodel=\"fixture-model\"\nendpoint=\"https://model.invalid/fixture\"\napi_key={{file={secret:?}}}\nshadow=false\n"
            )
        } else {
            format!(
                "[providers.{provider}]\nmodel=\"fixture-model\"\nendpoint=\"https://model.invalid/fixture\"\napi_key={{file={secret:?}}}\nshadow=false\n"
            )
        };
        let mut config = Config::parse(&format!("version=1\n[providers]\ncloud_opt_in=true\n{profile}\n[models]\nenabled=true\n[[models.routes]]\nkind=\"broken\"\nprovider=\"{provider}\"\n{}timeout_seconds=7\n", if provider=="cloud" { "profile=\"fixture\"\n" } else { "" })).unwrap();
        config.models.routes[0].cost_rates = Some(pitcrew_config::models::CostRates {
            input_microusd_per_million: 1000000,
            output_microusd_per_million: 2000000,
            cache_creation_microusd_per_million: 3000000,
            cache_read_microusd_per_million: 4000000,
        });
        let response = match provider {
            "jev" => {
                json!({"answers":{"root_cause":{"type":"choice","choice":"bad_file","confidence":0.9},"fix":{"type":"choice","choice":"redownload","confidence":0.9}},"usage":{"input_tokens":11,"output_tokens":22,"cache_creation_input_tokens":3,"cache_read_input_tokens":4}})
            }
            "anthropic" => {
                json!({"type":"message","role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":answer().to_string()}],"usage":{"input_tokens":11,"output_tokens":22,"cache_creation_input_tokens":3,"cache_read_input_tokens":4}})
            }
            _ => envelope(answer()),
        };
        let fake = Fake {
            response: Some(response),
            status: 200,
            ..Fake::default()
        };
        let result = run(&config, &fake, &evidence()).await;
        assert!(!result.used_rules_fallback, "{provider}");
        assert_eq!(
            result.primary,
            match provider {
                "jev" => DeciderKind::Jev,
                "anthropic" => DeciderKind::Anthropic,
                _ => DeciderKind::OpenAiCompatible,
            }
        );
        let accounting = result.entries[0].accounting.unwrap();
        assert_eq!(accounting.requests_started, 1);
        assert_eq!(accounting.responses_with_usage, 1);
        if provider != "cloud" {
            assert_eq!(accounting.usage.input_tokens, 11);
            assert_eq!(accounting.usage.output_tokens, 22);
            assert_eq!(accounting.usage.cache_creation_input_tokens, 3);
            assert_eq!(accounting.usage.cache_read_input_tokens, 4);
        }
        assert_eq!(
            accounting.usage.estimated_cost_microusd,
            Some(if provider == "cloud" { 282 } else { 80 })
        );
        let requests = fake.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].limits.timeout,
            std::time::Duration::from_secs(7)
        );
        assert_eq!(requests[0].url.path(), "/fixture");
        assert_eq!(requests[0].body.as_ref().unwrap()["model"], "fixture-model");
        assert!(requests[0].headers.iter().any(|(key, value)| {
            (key == "Authorization" && value == "Bearer synthetic-model-key")
                || (key == "x-api-key" && value == "synthetic-model-key")
        }));
    }
}
