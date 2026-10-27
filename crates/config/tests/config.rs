use pitcrew_config::{Config, Decider, ReplyWriter, SecretRef};
use std::path::PathBuf;

const MINIMAL: &str = r#"
version = 1
[plex]
url = "http://plex.example.invalid:32400"
owner_token = { env = "PLEX_TOKEN" }
server_id = "example-server"
[[arr]]
id = "movies"
kind = "radarr"
url = "http://radarr.example.invalid:7878"
api_key = { file = "/run/secrets/radarr" }
libraries = ["movies"]
"#;

#[test]
fn setup_and_minimum_install_need_no_ai_or_discord() {
    let setup = Config::parse("version = 1").unwrap();
    assert!(setup.plex.is_none());
    assert!(setup.arr.is_empty());
    let minimum = Config::parse(MINIMAL).unwrap();
    assert_eq!(minimum.arr.len(), 1);
    assert!(minimum.discord.is_none());
    assert_eq!(minimum.providers.decider, Decider::Rules);
    assert_eq!(minimum.providers.reply_writer, ReplyWriter::Templates);
    assert!(!minimum.providers.cloud_opt_in);
}

#[test]
fn arbitrary_number_of_arr_instances() {
    let mut source = MINIMAL.to_owned();
    for i in 0..25 {
        source.push_str(&format!(
            r#"
[[arr]]
id = "series_{i}"
kind = "sonarr"
url = "https://arr.example.invalid/{i}"
api_key = {{ env = "SONARR_{i}_KEY" }}
libraries = ["tv_{i}"]
"#
        ));
    }
    assert_eq!(Config::parse(&source).unwrap().arr.len(), 26);
}

#[test]
fn duplicate_ids_and_invalid_scope_are_rejected() {
    let mut source = MINIMAL.to_owned();
    source.push_str(&MINIMAL[MINIMAL.find("[[arr]]").unwrap()..]);
    assert_eq!(Config::parse(&source).unwrap_err().field, "arr[1].id");
    for libraries in ["[]", "[\"\"]", "[\"movies\", \"movies\"]"] {
        let source = MINIMAL.replace("[\"movies\"]", libraries);
        assert_eq!(
            Config::parse(&source).unwrap_err().field,
            "arr[0].libraries"
        );
    }
}

#[test]
fn malformed_and_credential_bearing_urls_do_not_leak_values() {
    for url in [
        "ftp://example.invalid",
        "not-a-url",
        "https://user:noreply@example.invalid",
        "https://example.invalid/?api_key=VERY_SECRET",
        "https://example.invalid/#VERY_SECRET",
    ] {
        let source = MINIMAL.replace("http://plex.example.invalid:32400", url);
        let error = Config::parse(&source).unwrap_err();
        assert_eq!(error.field, "plex.url");
        assert!(!error.to_string().contains("VERY_SECRET"));
        assert!(!error.to_string().contains("noreply"));
    }
}

#[test]
fn unknown_fields_and_inline_secret_values_are_rejected_without_echo() {
    for source in [
        "version = 1\nunknown = 'VERY_SECRET'",
        "version = 'VERY_SECRET'",
        "version = 1\n[discord]\ntoken = 'VERY_SECRET'",
        "version = 1\n[providers]\nanthopic = 'VERY_SECRET'",
    ] {
        let error = Config::parse(source).unwrap_err();
        assert!(!error.to_string().contains("VERY_SECRET"));
        assert!(!error.field.is_empty());
    }
}

#[test]
fn secret_reference_requires_exactly_one_valid_source() {
    for (env, file) in [
        (None, None),
        (Some("VALID"), Some("/secret")),
        (Some(""), None),
        (Some("1INVALID"), None),
        (Some("INVALID=NAME"), None),
        (None, Some("relative-file")),
    ] {
        let secret = SecretRef {
            env: env.map(str::to_owned),
            file: file.map(PathBuf::from),
        };
        assert!(secret.validate("token").is_err());
    }
    let secret = SecretRef {
        env: Some("VALID_1".to_owned()),
        file: None,
    };
    let value = secret
        .resolve_with("token", |_| Some("VERY_SECRET".to_owned()), |_| panic!())
        .unwrap();
    assert_eq!(value.expose(), "VERY_SECRET");
    assert_eq!(format!("{value:?}"), "Secret([redacted])");
    assert!(!format!("{secret:?}").contains("VALID_1"));
    assert!(
        secret
            .resolve_with("token", |_| None, |_| panic!())
            .is_err()
    );
}

#[test]
fn secret_files_trim_only_trailing_newlines_and_reject_empty() {
    let secret = SecretRef {
        env: None,
        file: Some(PathBuf::from("/run/secrets/test")),
    };
    let value = secret
        .resolve_with("token", |_| panic!(), |_| Some(" a token \r\n".to_owned()))
        .unwrap();
    assert_eq!(value.expose(), " a token ");
    for input in ["", "\n", "  "] {
        assert!(
            secret
                .resolve_with("token", |_| panic!(), |_| Some(input.to_owned()))
                .is_err()
        );
    }
    assert!(
        secret
            .resolve_with("token", |_| panic!(), |_| None)
            .is_err()
    );
}

const CLOUD: &str = r#"
version = 1
[providers]
cloud_opt_in = true
decider = "rules"
[providers.anthropic]
api_key = { env = "ANTHROPIC_API_KEY" }
model = "admin-selected-model"
endpoint = "https://api.example.invalid"
"#;

#[test]
fn cloud_consent_applies_to_shadow_too() {
    assert!(
        Config::parse(CLOUD)
            .unwrap()
            .providers
            .anthropic
            .unwrap()
            .shadow
    );
    let source = CLOUD.replace("cloud_opt_in = true", "cloud_opt_in = false");
    assert_eq!(
        Config::parse(&source).unwrap_err().field,
        "providers.cloud_opt_in"
    );
}

#[test]
fn live_provider_must_exist_and_cannot_be_shadow_only() {
    for decider in ["jev", "anthropic"] {
        let source = format!("version = 1\n[providers]\ndecider = '{decider}'");
        assert_eq!(
            Config::parse(&source).unwrap_err().field,
            "providers.decider"
        );
    }
    let source = CLOUD.replace("decider = \"rules\"", "decider = \"anthropic\"");
    assert_eq!(
        Config::parse(&source).unwrap_err().field,
        "providers.decider"
    );
    Config::parse(&(source + "\nshadow = false\n")).unwrap();
    assert_eq!(
        Config::parse("version = 1\n[providers]\nreply_writer = 'anthropic'")
            .unwrap_err()
            .field,
        "providers.reply_writer"
    );
}

#[test]
fn public_and_cloud_endpoints_require_https() {
    let source = CLOUD.replace("https://api.example.invalid", "http://api.example.invalid");
    assert_eq!(
        Config::parse(&source).unwrap_err().field,
        "providers.anthropic.endpoint"
    );
}

#[test]
fn invalid_budget_combinations_fail_clearly() {
    for setting in [
        "global_hour = 0",
        "global_hour = 41",
        "viewer_hour = 11",
        "viewer_day = 41",
        "per_job = 0",
        "item_cooldown_seconds = 0",
    ] {
        let source = format!("version = 1\n[budgets]\n{setting}");
        assert_eq!(Config::parse(&source).unwrap_err().field, "budgets");
    }
}

#[test]
fn checked_in_example_validates_without_resolving_secrets() {
    let config = Config::parse(include_str!("../../../config.example.toml")).unwrap();
    assert_eq!(config.arr.len(), 2);
}

#[test]
fn agent_needs_scoped_roots_and_valid_explicit_timezone() {
    let source = r#"
version = 1
[agent]
url = "https://agent.example.invalid"
shared_secret = { env = "AGENT_SECRET" }
media_roots = ["/media"]
storage_roots = ["/storage"]
plex_log_timezone = "Etc/UTC"
"#;
    Config::parse(source).unwrap();
    for replacement in ["/", "/media/../elsewhere", "relative"] {
        let bad = source.replace("\"/media\"", &format!("\"{replacement}\""));
        assert_eq!(Config::parse(&bad).unwrap_err().field, "agent.media_roots");
    }
    let bad = source.replace("Etc/UTC", "Not/AZone");
    assert_eq!(
        Config::parse(&bad).unwrap_err().field,
        "agent.plex_log_timezone"
    );
}

#[test]
fn parse_errors_name_the_field_and_safe_reason() {
    for (source, field, reason) in [
        ("", "version", "required field is missing"),
        ("version = 'VERY_SECRET'", "version", "incorrect value type"),
        (
            "version = 1\n[plex]\nurl = 'https://example.invalid'",
            "plex.owner_token",
            "required field is missing",
        ),
        (
            "version = 1\n[providers]\nanthopic = 'VERY_SECRET'",
            "providers.anthopic",
            "unknown configuration key",
        ),
        (
            "version = 1\n[providers]\ndecider = 'VERY_SECRET'",
            "providers.decider",
            "unrecognized option",
        ),
    ] {
        let error = Config::parse(source).unwrap_err();
        assert_eq!(error.field, field);
        assert_eq!(error.problem, reason);
        assert!(!error.to_string().contains("VERY_SECRET"));
    }
}
