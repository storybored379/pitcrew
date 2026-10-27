use pitcrew_config::{Config, Decider, OpenAiPreset, ReplyWriter, StructuredOutput};

fn parse(providers: &str) -> Result<Config, pitcrew_config::ConfigError> {
    Config::parse(&format!("version = 1\n[providers]\n{providers}"))
}

const LOCAL: &str = r#"
[[providers.openai_compatible]]
name = "local"
preset = "ollama"
model = "llama3.1:8b"
"#;

#[test]
fn local_models_need_no_key_or_cloud_consent() {
    let config = parse(LOCAL).unwrap();
    let local = &config.providers.openai_compatible[0];
    assert!(local.is_local());
    assert!(local.shadow);
    assert!(local.api_key.is_none());
    assert_eq!(local.preset, OpenAiPreset::Ollama);
    // A plain-HTTP LAN endpoint is fine for a local model.
    parse(&format!(
        "{LOCAL}endpoint = \"http://gpu-box.lan:11434/v1/chat/completions\"\n"
    ))
    .unwrap();
}

#[test]
fn cloud_profiles_need_consent_key_and_https() {
    let gpt = r#"
[[providers.openai_compatible]]
name = "gpt"
preset = "openai"
model = "example-model"
api_key = { env = "OPENAI_API_KEY" }
"#;
    assert_eq!(parse(gpt).unwrap_err().field, "providers.cloud_opt_in");
    let ok = parse(&format!("cloud_opt_in = true\n{gpt}")).unwrap();
    assert!(!ok.providers.openai_compatible[0].is_local());
    let no_key = gpt.replace("api_key = { env = \"OPENAI_API_KEY\" }\n", "");
    assert_eq!(
        parse(&format!("cloud_opt_in = true\n{no_key}"))
            .unwrap_err()
            .field,
        "providers.openai_compatible[0].api_key"
    );
    let http = format!("cloud_opt_in = true\n{gpt}endpoint = \"http://api.example.com/v1\"\n");
    assert_eq!(
        parse(&http).unwrap_err().field,
        "providers.openai_compatible[0].endpoint"
    );
}

#[test]
fn custom_servers_must_state_endpoint_and_locality() {
    let custom = r#"
[[providers.openai_compatible]]
name = "box"
preset = "custom"
model = "m"
"#;
    assert_eq!(
        parse(custom).unwrap_err().field,
        "providers.openai_compatible[0]"
    );
    let host = std::net::Ipv4Addr::from([10, 0, 0, 5]);
    let full = format!(
        "{custom}endpoint = \"http://{host}:8000/v1/chat/completions\"\nlocal = true\nstructured = \"prompt_only\"\ntoken_field = \"max_tokens\"\n"
    );
    let config = parse(&full).unwrap();
    assert_eq!(
        config.providers.openai_compatible[0].structured,
        Some(StructuredOutput::PromptOnly)
    );
}

#[test]
fn names_are_unique_and_profiles_must_exist_and_be_live() {
    let twice = format!("{LOCAL}{LOCAL}");
    assert_eq!(
        parse(&twice).unwrap_err().field,
        "providers.openai_compatible[1].name"
    );
    let bad_name = LOCAL.replace("name = \"local\"", "name = \"has space\"");
    assert_eq!(
        parse(&bad_name).unwrap_err().field,
        "providers.openai_compatible[0].name"
    );
    // Selecting a profile that is shadow-only, missing, or unnamed is rejected.
    for selection in [
        "decider = \"openai_compatible\"\ndecider_profile = \"local\"\n",
        "decider = \"openai_compatible\"\ndecider_profile = \"nope\"\n",
        "decider = \"openai_compatible\"\n",
    ] {
        assert_eq!(
            parse(&format!("{selection}{LOCAL}")).unwrap_err().field,
            "providers.decider_profile"
        );
    }
    let live = format!(
        "decider = \"openai_compatible\"\ndecider_profile = \"local\"\nreply_writer = \"openai_compatible\"\nreply_writer_profile = \"local\"\n{LOCAL}shadow = false\n"
    );
    let config = parse(&live).unwrap();
    assert_eq!(config.providers.decider, Decider::OpenaiCompatible);
    assert_eq!(config.providers.reply_writer, ReplyWriter::OpenaiCompatible);
    let writer_missing = format!("reply_writer = \"openai_compatible\"\n{LOCAL}shadow = false\n");
    assert_eq!(
        parse(&writer_missing).unwrap_err().field,
        "providers.reply_writer_profile"
    );
}

#[test]
fn several_models_can_run_side_by_side() {
    let source = format!(
        "cloud_opt_in = true\n{LOCAL}{}",
        r#"
[[providers.openai_compatible]]
name = "deepseek"
preset = "deepseek"
model = "deepseek-chat"
api_key = { file = "/run/secrets/deepseek" }

[[providers.openai_compatible]]
name = "gemini"
preset = "gemini"
model = "example"
api_key = { env = "GEMINI_API_KEY" }
"#
    );
    assert_eq!(parse(&source).unwrap().providers.openai_compatible.len(), 3);
}

#[test]
fn catalog_snapshot_path_must_be_absolute() {
    let config = Config::parse(
        "version = 1\n[stack]\ncatalog_snapshot_file = \"/var/lib/pitcrew/catalog.json\"\n",
    )
    .unwrap();
    assert!(config.stack.catalog_snapshot_file.is_some());
    assert_eq!(
        Config::parse("version = 1\n[stack]\ncatalog_snapshot_file = \"catalog.json\"\n")
            .unwrap_err()
            .field,
        "stack.catalog_snapshot_file"
    );
}
