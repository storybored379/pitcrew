use pitcrew_config::{Config, EngineMode};
#[test]
fn shadow_default_live_ack_and_local_ingress() {
    let config = Config::parse("version=1").unwrap();
    assert_eq!(config.engine.mode, EngineMode::Shadow);
    assert_eq!(config.engine.listen.to_string(), "127.0.0.1:8791");
    assert!(!config.engine.poll_enabled);
    for (field, source) in [
        ("engine.acknowledge_live_mutations", "mode='live'"),
        ("engine.listen", "listen='0.0.0.0:8791'"),
        ("engine.poll_seconds", "poll_seconds=0"),
        ("engine.poll_seconds", "poll_seconds=3601"),
    ] {
        assert_eq!(
            Config::parse(&format!("version=1\n[engine]\n{source}"))
                .unwrap_err()
                .field,
            field
        );
    }
    Config::parse("version=1\n[engine]\nmode='live'\nacknowledge_live_mutations=true").unwrap();
}

#[test]
fn five_instance_shadow_example_validates_offline() {
    let config = Config::parse(include_str!("../../../config.shadow.example.toml")).unwrap();
    assert_eq!(config.arr.len(), 5);
    assert!(
        config
            .arr
            .iter()
            .all(|instance| instance.webhook_secret.is_some())
    );
    assert_eq!(config.engine.mode, EngineMode::Shadow);
}
