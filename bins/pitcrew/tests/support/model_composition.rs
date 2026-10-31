use super::*;

async fn enabled() -> Setup {
    let mut setup = setup().await;
    setup.config.models = Config::parse(
        r#"version=1
[models]
enabled=true
[[models.routes]]
kind="broken"
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
model="fixture"
shadow=false
"#,
    )
    .unwrap()
    .models;
    setup.config.providers = Config::parse(
        r#"version=1
[[providers.openai_compatible]]
name="fixture"
preset="ollama"
model="fixture"
shadow=false
"#,
    )
    .unwrap()
    .providers;
    setup.config.validate().unwrap();
    let time = setup.clock.clone();
    setup.runtime = Runtime::open(
        &setup.config,
        setup.store.clone(),
        setup.fake.clone(),
        Arc::new(move || time.load(Ordering::SeqCst)),
    )
    .await
    .unwrap()
    .unwrap();
    setup.runtime.sync().await.unwrap();
    setup
}
fn reply(valid: bool) -> serde_json::Value {
    let content = if valid {
        json!({"root_cause":"bad_file","cause_confidence":0.9,"fix":"redownload","fix_confidence":0.9})
    } else {
        json!({"malformed":true})
    };
    json!({"choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":content.to_string()}}],"usage":{"prompt_tokens":10,"completion_tokens":20}})
}
async fn usage(setup: &Setup) -> Vec<serde_json::Value> {
    setup
        .store
        .observations()
        .await
        .unwrap()
        .into_iter()
        .filter(|o| o.source == "decider:usage")
        .map(|o| o.payload)
        .collect()
}

#[tokio::test]
async fn model_runtime_persists_provider_and_usage_once_across_restart() {
    let mut setup = enabled().await;
    *setup.fake.model_reply.lock().unwrap() = Some((reply(true), None));
    let (_, _, id) = report(&setup).await;
    setup.runtime.tick().await.unwrap();
    let diagnosis = setup.intake.diagnosis(id).await.unwrap().unwrap();
    assert_eq!(
        diagnosis.decider_kind,
        pitcrew_deciders::DeciderKind::OpenAiCompatible
    );
    assert_eq!(
        diagnosis.decision.decision.action,
        pitcrew_diagnosis::rules::RecommendedAction::Replace
    );
    let records = usage(&setup).await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["usage"]["input_tokens"], 10);
    assert_eq!(records[0]["usage"]["output_tokens"], 20);
    assert_eq!(records[0]["usage"]["estimated_cost_microusd"], 50);
    let clock = setup.clock.clone();
    setup.runtime = Runtime::open(
        &setup.config,
        setup.store.clone(),
        setup.fake.clone(),
        Arc::new(move || clock.load(Ordering::SeqCst)),
    )
    .await
    .unwrap()
    .unwrap();
    setup.runtime.sync().await.unwrap();
    setup.runtime.tick().await.unwrap();
    assert_eq!(usage(&setup).await, records);
    assert_eq!(
        setup
            .fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.as_str() == "/v1/chat/completions")
            .count(),
        1
    );
}

#[tokio::test]
async fn rejected_model_answer_keeps_reported_usage_with_rules_provenance() {
    let mut setup = enabled().await;
    *setup.fake.model_reply.lock().unwrap() = Some((reply(false), None));
    let (_, _, id) = report(&setup).await;
    setup.runtime.tick().await.unwrap();
    let diagnosis = setup.intake.diagnosis(id).await.unwrap().unwrap();
    assert_eq!(diagnosis.decider_kind, pitcrew_deciders::DeciderKind::Rules);
    let records = usage(&setup).await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["usage"]["input_tokens"], 10);
    assert_eq!(records[0]["failed"], true);
}

#[tokio::test]
async fn post_model_access_expiry_preserves_usage_without_a_diagnosis() {
    let mut setup = enabled().await;
    *setup.fake.model_reply.lock().unwrap() =
        Some((reply(true), Some((setup.clock.clone(), 401_000))));
    let (_, _, id) = report(&setup).await;
    setup.runtime.tick().await.unwrap();
    assert!(setup.intake.diagnosis(id).await.unwrap().is_none());
    let records = usage(&setup).await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["usage"]["input_tokens"], 10);
    assert_eq!(records[0]["accounting"]["responses_with_usage"], 1);
}

#[tokio::test]
async fn interrupted_attempt_is_unknown_usage_and_is_never_sent_again() {
    let mut setup = enabled().await;
    let (_, _, id) = report(&setup).await;
    assert!(setup.intake.begin_model_attempt(id, 100).await.unwrap());
    setup.runtime.tick().await.unwrap();
    let records = usage(&setup).await;
    assert!(records.is_empty());
    let pending: Vec<_> = setup
        .store
        .observations()
        .await
        .unwrap()
        .into_iter()
        .filter(|record| record.source == "decider:pending")
        .collect();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].payload["unresolved"], true);
    assert!(pending[0].payload["usage"].is_null());
    assert_eq!(
        setup
            .intake
            .diagnosis(id)
            .await
            .unwrap()
            .unwrap()
            .decider_kind,
        pitcrew_deciders::DeciderKind::Rules
    );
    setup.runtime.tick().await.unwrap();
    assert_eq!(usage(&setup).await, records);
    assert!(
        !setup
            .fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|p| p == "/v1/chat/completions")
    );
}

#[tokio::test]
async fn missing_provider_usage_is_unknown_even_when_rates_are_configured() {
    let mut setup = enabled().await;
    *setup.fake.model_reply.lock().unwrap() = Some((json!({}), None));
    let (_, _, id) = report(&setup).await;
    setup.runtime.tick().await.unwrap();
    assert_eq!(
        setup
            .intake
            .diagnosis(id)
            .await
            .unwrap()
            .unwrap()
            .decider_kind,
        pitcrew_deciders::DeciderKind::Rules
    );
    let records = usage(&setup).await;
    assert_eq!(records.len(), 1);
    assert!(records[0]["usage"].is_null());
    assert_eq!(records[0]["accounting"]["requests_started"], 1);
    assert_eq!(records[0]["accounting"]["responses_with_usage"], 0);
}

#[tokio::test]
async fn pending_projection_then_late_completion_reconciles_without_replay_or_double_count() {
    let mut setup = enabled().await;
    let (_, _, id) = report(&setup).await;
    // Controller A reserves; B sees the unfinished attempt and publishes rules.
    assert!(setup.intake.begin_model_attempt(id, 100).await.unwrap());
    setup.runtime.tick().await.unwrap();
    // A then completes its single provider call. B must recover known usage.
    *setup.fake.model_reply.lock().unwrap() = Some((reply(true), None));
    let mut facts = pitcrew_diagnosis::facts::EvidenceFacts::default();
    facts.playback.decode = pitcrew_diagnosis::facts::DecodeOutcome::Bad;
    let comparison = pitcrew_deciders::shadow::DecisionRunner::with_schedule(
        pitcrew_deciders::schedule::Schedule::from_config(&setup.config, setup.fake.clone()),
    )
    .run(
        &pitcrew_deciders::LocalEvidence {
            facts,
            viewer_text: String::new(),
            private_context: serde_json::Value::Null,
        },
        &mut pitcrew_deciders::shadow::JsonlSink(Vec::new()),
    )
    .await
    .unwrap();
    setup
        .intake
        .finish_model_attempt(id, &comparison)
        .await
        .unwrap();
    setup.runtime.tick().await.unwrap();
    setup.runtime.tick().await.unwrap();
    let known = usage(&setup).await;
    assert_eq!(known.len(), 1);
    assert_eq!(known[0]["usage"]["input_tokens"], 10);
    setup.runtime.tick().await.unwrap();
    assert_eq!(usage(&setup).await, known);
    assert_eq!(
        setup
            .fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.as_str() == "/v1/chat/completions")
            .count(),
        1
    );
}
