use pitcrew_config::Config;
const BASE: &str = r#"
version = 1
[agent]
url = "http://127.0.0.1:8790"
shared_secret = { env = "TEST_AGENT_SECRET" }
media_roots = ["/media"]
storage_roots = ["/storage"]
plex_log_timezone = "UTC"
"#;
#[test]
fn agent_defaults_are_local_and_optional() {
    let c = Config::parse(BASE).unwrap();
    let a = c.agent.unwrap();
    assert!(a.server.listen.ip().is_loopback());
    assert!(!a.server.compatibility);
    assert_eq!(a.server.max_connections, 32);
    assert_eq!(a.server.requests_per_minute, 120);
    assert_eq!(a.server.request_timeout_seconds, 600);
    assert_eq!(a.server.decode_seconds, 10);
    assert_eq!(a.server.decode_offsets_percent, vec![0, 40, 80]);
    assert_eq!(a.server.filesystem_workers, 4);
    assert!(a.server.plex_log_directory.is_none());
    assert!(!a.checks.read_timeout_policy.enabled);
    assert_eq!(a.checks.read_timeout_policy.attempts, 3);
    assert_eq!(a.checks.read_timeout_policy.control_timeout_ms, 1000);
}
#[test]
fn agent_rejects_unsafe_network_and_resource_settings() {
    for bad in [
        "listen = '0.0.0.0:8790'",
        "max_connections = 0",
        "max_connections = 257",
        "requests_per_minute = 0",
        "requests_per_minute = 6001",
        "request_timeout_seconds = 0",
        "request_timeout_seconds = 3601",
        "decode_seconds = 0",
        "decode_seconds = 301",
        "filesystem_workers = 0",
        "filesystem_workers = 65",
        "ffmpeg = ''",
        "ffprobe = ''",
        "plex_log_directory = 'relative'",
    ] {
        assert!(
            Config::parse(&format!("{BASE}\n[agent.server]\n{bad}")).is_err(),
            "{bad}"
        );
    }
    assert!(Config::parse(&format!("{BASE}\n[agent.server]\nlisten='[::1]:8790'\ncompatibility=true\nplex_log_directory='/logs'")).is_ok());
}

#[test]
fn decode_sampling_configuration_is_bounded_and_errors_name_the_key() {
    for offsets in [
        "[]",
        "[100]",
        "[80, 40]",
        "[0, 0]",
        "[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]",
    ] {
        let source = format!("{BASE}\n[agent.server]\ndecode_offsets_percent = {offsets}");
        assert_eq!(
            Config::parse(&source).unwrap_err().field,
            "agent.server.decode_offsets_percent"
        );
    }
    let source = format!("{BASE}\n[agent.server]\ndecode_offsets_percent = [0, 50, 99]");
    assert_eq!(
        Config::parse(&source)
            .unwrap()
            .agent
            .unwrap()
            .server
            .decode_offsets_percent,
        vec![0, 50, 99]
    );
    for (setting, key) in [
        ("decode_seconds = 0", "decode_seconds"),
        ("decode_timeout_seconds = 601", "decode_timeout_seconds"),
        ("listen = '0.0.0.0:8790'", "listen"),
        ("ffprobe = ''", "ffprobe"),
    ] {
        let source = format!("{BASE}\n[agent.server]\n{setting}");
        assert_eq!(
            Config::parse(&source).unwrap_err().field,
            format!("agent.server.{key}")
        );
    }
}

#[test]
fn media_constructor_limits_are_rejected_at_the_named_config_key() {
    for setting in [
        "symlink_hop_limit = 41",
        "read_bytes = 1048577",
        "read_offsets_bytes = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]",
    ] {
        let source = format!("{BASE}\n[agent.checks]\n{setting}");
        let key = setting.split('=').next().unwrap().trim();
        assert_eq!(
            Config::parse(&source).unwrap_err().field,
            format!("agent.checks.{key}")
        );
    }
    let boundary = format!(
        "{BASE}\n[agent.checks]\nsymlink_hop_limit=40\nread_bytes=1048576\nread_offsets_bytes=[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]"
    );
    Config::parse(&boundary).unwrap();
}

#[test]
fn repeated_read_timeout_policy_can_be_enabled_explicitly() {
    let source = format!(
        "{BASE}\n[agent.checks.read_timeout_policy]\nenabled = true\nattempts = 3\ncontrol_timeout_ms = 1000"
    );
    let policy = Config::parse(&source)
        .unwrap()
        .agent
        .unwrap()
        .checks
        .read_timeout_policy;
    assert!(policy.enabled);
    assert_eq!(policy.attempts, 3);
    assert_eq!(policy.control_timeout_ms, 1000);
}

#[test]
fn repeated_read_timeout_policy_rejects_unsafe_bounds_with_named_keys() {
    for (setting, field) in [
        ("attempts = 0", "attempts"),
        ("attempts = 1", "attempts"),
        ("attempts = 6", "attempts"),
        ("control_timeout_ms = 0", "control_timeout_ms"),
        ("control_timeout_ms = 5001", "control_timeout_ms"),
    ] {
        let source = format!("{BASE}\n[agent.checks.read_timeout_policy]\n{setting}");
        assert_eq!(
            Config::parse(&source).unwrap_err().field,
            format!("agent.checks.read_timeout_policy.{field}")
        );
    }
    for read_timeout in [999, 1000] {
        let source = format!(
            "{BASE}\n[agent.checks]\nread_timeout_ms = {read_timeout}\n[agent.checks.read_timeout_policy]\nenabled = true"
        );
        assert_eq!(
            Config::parse(&source).unwrap_err().field,
            "agent.checks.read_timeout_policy.control_timeout_ms"
        );
    }
    for (attempts, control_timeout) in [(2, 1), (5, 5000)] {
        let source = format!(
            "{BASE}\n[agent.checks.read_timeout_policy]\nenabled = true\nattempts = {attempts}\ncontrol_timeout_ms = {control_timeout}"
        );
        Config::parse(&source).unwrap();
    }
    let source = format!("{BASE}\n[agent.checks]\nread_timeout_ms = 1");
    Config::parse(&source).unwrap(); // Disabled policy preserves old timeout settings.
}
