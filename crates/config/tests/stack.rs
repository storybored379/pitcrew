use pitcrew_config::Config;

#[test]
fn stack_is_optional_and_defaults_to_local_shadow() {
    let config = Config::parse("version = 1").unwrap();
    assert!(!config.stack.enabled);
    assert_eq!(config.stack.intake_database.to_str(), Some("intake.sqlite"));
    assert_eq!(config.stack.evidence_max_age_seconds, 300);
    assert_eq!(config.stack.web_listen.to_string(), "127.0.0.1:7401");
    assert_eq!(config.stack.web_public_origin, "http://127.0.0.1:7401");
    assert_eq!(config.stack.plex_auth_url, "https://plex.tv/");
    assert_eq!(config.stack.plex_client_id, "pitcrew");
    assert_eq!(config.stack.bot_seerr_user_id, None);
    assert!(config.stack.catalog.is_empty());
}

#[test]
fn stack_catalog_never_accepts_ambiguous_identity_and_example_parses() {
    let item = "\n[[stack.catalog]]\nid='episode'\ntitle='Synthetic Episode'\naliases=['Synthetic']\nlibrary_id='library'\nlibrary_name='Series'\nresolution='hd'\nseason=1\nepisode=2\n";
    let source = format!("version=1\n[stack]{item}");
    let config = Config::parse(&source).unwrap();
    assert_eq!(config.stack.catalog[0].title, "Synthetic Episode");
    for source in [
        source.replace("id='episode'", "id=''"),
        source.replace("library_id='library'", "library_id=''"),
        format!("{source}{item}"),
    ] {
        assert_eq!(Config::parse(&source).unwrap_err().field, "stack.catalog");
    }
    let config = Config::parse(include_str!("../../../packaging/stack.toml")).unwrap();
    assert!(config.stack.enabled);
    assert!(!config.engine.poll_enabled);
}

#[test]
fn stack_validates_paths_ttls_local_bind_origins_and_auth() {
    for (setting, field) in [
        ("intake_database = ''", "intake_database"),
        ("evidence_max_age_seconds = 0", "evidence_max_age_seconds"),
        (
            "evidence_max_age_seconds = 3601",
            "evidence_max_age_seconds",
        ),
        ("web_listen = '0.0.0.0:7401'", "web_listen"),
        ("web_public_origin = 'invalid'", "web_public_origin"),
        (
            "web_public_origin = 'http://viewer.invalid'",
            "web_public_origin",
        ),
        (
            "web_public_origin = 'https://viewer.invalid/path'",
            "web_public_origin",
        ),
        (
            "web_public_origin = 'https://noreply@viewer.invalid'",
            "web_public_origin",
        ),
        ("plex_auth_url = 'http://auth.invalid'", "plex_auth_url"),
        ("plex_client_id = ''", "plex_client_id"),
        ("bot_seerr_user_id = 0", "bot_seerr_user_id"),
    ] {
        let error = Config::parse(&format!("version=1\n[stack]\n{setting}")).unwrap_err();
        assert_eq!(error.field, format!("stack.{field}"));
    }
    for ttl in [1, 3600] {
        for origin in [
            "https://viewer.invalid",
            "http://localhost:7401",
            "http://[::1]:7401",
        ] {
            let config = Config::parse(&format!("version=1\n[stack]\nenabled=true\nintake_database='/state/intake.sqlite'\nevidence_max_age_seconds={ttl}\nweb_listen='[::1]:7401'\nweb_public_origin='{origin}'\nplex_client_id='custom'\nbot_seerr_user_id=1")).unwrap();
            assert!(config.stack.enabled);
        }
    }
}
