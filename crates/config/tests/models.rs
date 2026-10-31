use pitcrew_config::{Config, models::Provider};

fn config(routes: &str, provider: &str) -> Result<Config, pitcrew_config::ConfigError> {
    Config::parse(&format!(
        "version=1\n{provider}\n[models]\nenabled=true\n{routes}"
    ))
}
const LOCAL: &str = r#"
[[providers.openai_compatible]]
name="fixture"
preset="ollama"
model="fixture-model"
shadow=false
"#;
const ROUTE: &str = r#"
[[models.routes]]
kind="broken"
provider="local"
profile="fixture"
"#;

#[test]
fn routing_is_disabled_by_default_and_unspecified_kinds_need_no_model() {
    let minimal = Config::parse("version=1").unwrap();
    assert!(!minimal.models.enabled);
    assert!(minimal.models.routes.is_empty());
    let selected = config(ROUTE, LOCAL).unwrap();
    assert!(selected.models.enabled);
    assert_eq!(selected.models.routes[0].provider, Provider::Local);
    assert_eq!(selected.models.routes[0].timeout_seconds, 30);
    assert!(selected.models.routes[0].cost_rates.is_none());
}

#[test]
fn routes_reject_mismatched_missing_and_shadow_profiles() {
    for (route, provider) in [
        (ROUTE.replace("local", "cloud"), LOCAL.to_string()),
        (ROUTE.replace("fixture", "missing"), LOCAL.to_string()),
        (ROUTE.replace("profile=\"fixture\"", ""), LOCAL.to_string()),
        (
            ROUTE.to_string(),
            LOCAL.replace("shadow=false", "shadow=true"),
        ),
    ] {
        assert_eq!(
            config(&route, &provider).unwrap_err().field,
            "models.routes.profile"
        );
    }
    for provider in ["rules", "jev", "anthropic"] {
        let route = ROUTE.replace("local", provider);
        assert_eq!(
            config(&route, LOCAL).unwrap_err().field,
            "models.routes.profile"
        );
    }
}

#[test]
fn native_cloud_routes_require_enabled_profiles_and_consent() {
    for native in ["jev", "anthropic"] {
        let route = format!("[[models.routes]]\nkind=\"unknown\"\nprovider=\"{native}\"\n");
        assert_eq!(
            config(&route, "").unwrap_err().field,
            "models.routes.provider"
        );
        let profile = format!(
            "[providers]\ncloud_opt_in=true\n[providers.{native}]\nmodel=\"fixture\"\nendpoint=\"https://model.invalid/messages\"\napi_key={{env=\"FIXTURE_MODEL_KEY\"}}\nshadow=false\n"
        );
        config(&route, &profile).unwrap();
        assert_eq!(
            config(&route, &profile.replace("shadow=false", "shadow=true"))
                .unwrap_err()
                .field,
            "models.routes.provider"
        );
        assert_eq!(
            config(
                &route,
                &profile.replace("cloud_opt_in=true", "cloud_opt_in=false")
            )
            .unwrap_err()
            .field,
            "providers.cloud_opt_in"
        );
    }
}

#[test]
fn routes_bound_time_and_reject_duplicates_or_unknown_kinds() {
    for seconds in [0, 121, u64::MAX] {
        assert_eq!(
            config(&format!("{ROUTE}timeout_seconds={seconds}\n"), LOCAL)
                .unwrap_err()
                .field,
            "models.routes.timeout_seconds"
        );
    }
    for seconds in [1, 120] {
        config(&format!("{ROUTE}timeout_seconds={seconds}\n"), LOCAL).unwrap();
    }
    assert_eq!(
        config(&format!("{ROUTE}{ROUTE}"), LOCAL).unwrap_err().field,
        "models.routes.kind"
    );
    assert!(config(&ROUTE.replace("broken", "made_up"), LOCAL).is_err());
    let rules = "[[models.routes]]\nkind=\"unknown\"\nprovider=\"rules\"\n";
    assert_eq!(
        config(rules, "").unwrap().models.routes[0].provider,
        Provider::Rules
    );
}
