use pitcrew::live::{self, validate};
use pitcrew_arr::Kind;
use pitcrew_config::Config;
use pitcrew_engine::live::{Certification, OwnerAcknowledgement};
use pitcrew_store::{
    Store,
    policy::{AdminAction, ClientCapabilities, PolicyRole},
};
use serde_json::{Value, json};
use std::path::Path;

const NOW: u64 = 100;
struct Fixture {
    _dir: tempfile::TempDir,
    config: Config,
    store: Store,
    certificate: Certification,
}
fn write(path: &Path, value: &impl serde::Serialize) {
    std::fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}
impl Fixture {
    async fn new(kind: Kind) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("secret");
        std::fs::write(&secret, "synthetic-secret-0123456789abcdef").unwrap();
        let version = match kind {
            Kind::Sonarr => "4.0.19.2979",
            Kind::Radarr => "6.3.0.10514",
        };
        let certificate = Certification {
            id: "synthetic-owner-review".into(),
            client: "arr-one".into(),
            kind,
            version: version.into(),
            fixture_digest: live::fixture_digest(kind),
            certified_at: 50,
            expires_at: 200,
        };
        let certificate_file = dir.path().join("certificate.json");
        let acknowledgement_file = dir.path().join("acknowledgement.json");
        let capabilities_file = dir.path().join("capabilities.json");
        write(&certificate_file, &certificate);
        write(
            &acknowledgement_file,
            &OwnerAcknowledgement {
                certification: certificate.clone(),
                acknowledge_live_mutations: true,
            },
        );
        let capabilities = ClientCapabilities {
            external_queue_actor: false,
            held_category: None,
        };
        write(
            &capabilities_file,
            &json!({"certification":certificate,"capabilities":capabilities,"evidence":{"review":"synthetic"}}),
        );
        let mut config: Config = serde_json::from_value(json!({
            "version":1, "database":dir.path().join("engine.sqlite"),
            "engine":{"mode":"live","acknowledge_live_mutations":true},
            "live":{"enabled":true,"certifications":[{
                "id":certificate.id, "client":certificate.client, "version":version,
                "fixture_digest":certificate.fixture_digest,"certified_at":50,"expires_at":200,
                "certificate_file":certificate_file,"acknowledgement_file":acknowledgement_file,"capabilities_file":capabilities_file
            }]},
            "stack":{"enabled":true}, "runtime":{"search_enabled":true},
            "plex":{"url":"http://127.0.0.1:9","owner_token":{"file":secret},"server_id":"synthetic-server"},
            "arr":[{"id":"arr-one","kind":match kind {Kind::Sonarr=>"sonarr",Kind::Radarr=>"radarr"},"url":"http://127.0.0.1:9","api_key":{"file":secret},"libraries":["1"]}],
            "agent":{"url":"http://127.0.0.1:9","shared_secret":{"file":secret},"media_roots":["/library"],"storage_roots":["/library"],"plex_log_timezone":"UTC"},
            "notifiers":[{"id":"admin-one","adapter":{"kind":"ntfy","url":"http://127.0.0.1:9/alerts"}}],
            "channels":[{"id":"viewer-one","adapter":{"kind":"seerr","url":"http://127.0.0.1:9","api_key":{"file":secret}}}]
        })).unwrap();
        config.stack.intake_database = dir.path().join("intake.sqlite");
        config.validate().unwrap();
        let store = Store::open(&config.database).await.unwrap();
        store
            .observe_client_version("arr-one", version, 50)
            .await
            .unwrap();
        store
            .record_capabilities(
                "arr-one",
                version,
                &capabilities,
                &json!({"review":"synthetic"}),
                50,
            )
            .await
            .unwrap();
        Self {
            _dir: dir,
            config,
            store,
            certificate,
        }
    }
    async fn check(&self) -> Result<live::Validated<'_>, &'static str> {
        validate(&self.config, Some(self.store.clone()), NOW).await
    }
    async fn error(&self, expected: &str) {
        match self.check().await {
            Err(reason) => assert_eq!(reason, expected),
            Ok(_) => panic!("missing refusal: {expected}"),
        }
    }
    fn modify_record(&self, name: &str, edit: impl FnOnce(&mut Value)) {
        let input = &self.config.live.certifications[0];
        let path = match name {
            "certificate" => &input.certificate_file,
            "ack" => &input.acknowledgement_file,
            _ => &input.capabilities_file,
        };
        let mut value: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        edit(&mut value);
        write(path, &value);
    }
}

#[test]
fn default_configuration_never_enables_live() {
    let config = Config::parse("version=1").unwrap();
    assert_eq!(config.engine.mode, pitcrew_config::EngineMode::Shadow);
    assert!(!config.live.enabled);
    assert!(config.live.certifications.is_empty());
}

#[tokio::test]
async fn every_local_prerequisite_has_a_specific_refusal() {
    for switch in 0..3 {
        let mut f = Fixture::new(Kind::Radarr).await;
        match switch {
            0 => f.config.engine.mode = pitcrew_config::EngineMode::Shadow,
            1 => f.config.engine.acknowledge_live_mutations = false,
            _ => f.config.live.enabled = false,
        }
        f.error("live requires engine.mode=live, engine.acknowledge_live_mutations=true and live.enabled=true").await;
    }
    let f = Fixture::new(Kind::Radarr).await;
    assert!(matches!(
        validate(&f.config, None, NOW).await,
        Err("live requires a durable engine store")
    ));
    type ConfigCase = (fn(&mut Config), &'static str);
    let edits: [ConfigCase; 6] = [
        (
            |c| c.stack.enabled = false,
            "live requires stack.enabled=true for authenticated intake and diagnosis",
        ),
        (
            |c| c.plex = None,
            "live requires Plex target and current access verification",
        ),
        (
            |c| c.agent = None,
            "live requires the exact-file media agent",
        ),
        (
            |c| c.arr.clear(),
            "live requires at least one configured Sonarr or Radarr client",
        ),
        (
            |c| c.runtime.search_enabled = false,
            "live requires runtime.search_enabled=true for budgeted candidate selection",
        ),
        (
            |c| c.live.certifications.clear(),
            "live requires exactly one certification per configured arr client",
        ),
    ];
    for (edit, expected) in edits {
        let mut f = Fixture::new(Kind::Radarr).await;
        edit(&mut f.config);
        f.error(expected).await;
    }
    let mut f = Fixture::new(Kind::Radarr).await;
    f.config.live.certifications[0].client = "other".into();
    f.error("live certification client IDs must match configured arr clients exactly once")
        .await;
}

#[tokio::test]
async fn bundled_contract_and_all_certification_records_must_agree() {
    for kind in [Kind::Sonarr, Kind::Radarr] {
        let mut f = Fixture::new(kind).await;
        assert!(f.check().await.is_ok());
        assert_eq!(live::fixture_digest(kind).len(), 64);
        f.config.live.certifications[0].fixture_digest = "different".into();
        f.error("live certification fixture_digest does not match the bundled contract")
            .await;
    }
    for (name, edit) in [
        (
            "certificate",
            (|v: &mut Value| v["client"] = json!("other")) as fn(&mut Value),
        ),
        ("ack", |v| v["acknowledge_live_mutations"] = json!(false)),
        ("ack", |v| v["certification"]["id"] = json!("other")),
    ] {
        let f = Fixture::new(Kind::Radarr).await;
        f.modify_record(name, edit);
        f.error(
            "live certificate and owner acknowledgement must match configuration and be current",
        )
        .await;
    }
    let f = Fixture::new(Kind::Radarr).await;
    for now in [49, 200] {
        assert!(matches!(
            validate(&f.config, Some(f.store.clone()), now).await,
            Err(
                "live certificate and owner acknowledgement must match configuration and be current"
            )
        ));
    }
}

#[tokio::test]
async fn capability_evidence_cannot_restore_reset_or_changed_ledgers() {
    for edit in [
        (|v: &mut Value| v["certification"]["id"] = json!("different")) as fn(&mut Value),
        |v| v["evidence"] = Value::Null,
    ] {
        let f = Fixture::new(Kind::Radarr).await;
        f.modify_record("capabilities", edit);
        f.error("live capability evidence must name the exact certification and include evidence")
            .await;
    }
    let f = Fixture::new(Kind::Radarr).await;
    f.modify_record("capabilities", |v| {
        v["capabilities"]["held_category"] = json!("held")
    });
    f.error("live held-category preservation transport is not implemented or certified")
        .await;
    let f = Fixture::new(Kind::Radarr).await;
    f.store
        .reset_capabilities(
            "arr-one",
            &AdminAction {
                actor: "admin-one".into(),
                role: PolicyRole::Admin,
                note: "review required".into(),
            },
            100,
        )
        .await
        .unwrap();
    f.error("live capability ledger is reset or differs from reviewed evidence; a new audited review is required").await;
    assert!(
        f.store
            .capabilities("arr-one", &f.certificate.version)
            .await
            .unwrap()
            .unwrap()
            .capabilities
            .is_none()
    );
    let f = Fixture::new(Kind::Radarr).await;
    f.store
        .observe_client_version("arr-one", "different", 100)
        .await
        .unwrap();
    f.error("live requires a reviewed version-bound capability ledger")
        .await;
    let f = Fixture::new(Kind::Radarr).await;
    f.store
        .record_capabilities(
            "arr-one",
            &f.certificate.version,
            &ClientCapabilities {
                external_queue_actor: true,
                held_category: None,
            },
            &json!({"review":"outside actor"}),
            100,
        )
        .await
        .unwrap();
    f.error("live capability ledger is reset or differs from reviewed evidence; a new audited review is required").await;
}

#[tokio::test]
async fn unreadable_malformed_large_and_nonregular_records_refuse_without_leaking_values() {
    for name in ["certificate", "ack", "capabilities"] {
        for mode in 0..5 {
            let f = Fixture::new(Kind::Radarr).await;
            let input = &f.config.live.certifications[0];
            let (path, expected) = match name {
                "certificate" => (
                    &input.certificate_file,
                    "live certificate_file must contain a bounded regular certification record",
                ),
                "ack" => (
                    &input.acknowledgement_file,
                    "live acknowledgement_file must contain a separate bounded owner acknowledgement",
                ),
                _ => (
                    &input.capabilities_file,
                    "live capabilities_file must contain bounded reviewed capability evidence",
                ),
            };
            match mode {
                0 => std::fs::remove_file(path).unwrap(),
                1 => std::fs::write(path, b"synthetic-private-invalid-json").unwrap(),
                2 => {
                    let mut bytes = std::fs::read(path).unwrap();
                    bytes.resize(65_537, b' ');
                    std::fs::write(path, bytes).unwrap();
                }
                3 => {
                    std::fs::remove_file(path).unwrap();
                    std::fs::create_dir(path).unwrap();
                }
                _ => {
                    let other = path.with_extension("other");
                    std::fs::rename(path, &other).unwrap();
                    std::os::unix::fs::symlink(other, path).unwrap();
                }
            }
            f.error(expected).await;
        }
    }
    // Exact limit still parses: the size guard must not reject 64 KiB.
    let f = Fixture::new(Kind::Radarr).await;
    let path = &f.config.live.certifications[0].certificate_file;
    let mut bytes = std::fs::read(path).unwrap();
    bytes.resize(65_536, b' ');
    std::fs::write(path, bytes).unwrap();
    assert!(f.check().await.is_ok());
}

#[derive(Default)]
struct NoNetwork;
impl pitcrew_upstream::Transport for NoNetwork {
    fn send(&self, _: pitcrew_upstream::Request) -> pitcrew_upstream::ResponseFuture<'_> {
        panic!("offline composition contacted upstream")
    }
}

#[tokio::test]
async fn constructs_real_graph_without_dispatch_and_still_names_runtime_blockers() {
    for kind in [Kind::Sonarr, Kind::Radarr] {
        let f = Fixture::new(kind).await;
        let composition = f
            .check()
            .await
            .unwrap()
            .compose(
                NoNetwork,
                std::sync::Arc::new(|| NOW),
                &pitcrew::live_delivery::HttpFactory,
            )
            .unwrap();
        assert_eq!(composition.routers.len(), 1);
        assert!(composition.routers.contains_key("arr-one"));
        assert_eq!(composition.admin_notifier, "admin-one");
    }
    for missing in [
        "trusted grab ownership receipt",
        "per-transition current access",
        "Plex target binding",
        "interference/dead-release",
    ] {
        assert!(live::UNAVAILABLE.contains(missing));
    }
}

#[tokio::test]
async fn reviewed_capability_evidence_must_match_the_current_ledger_and_clock() {
    let current = Fixture::new(Kind::Radarr).await;
    current
        .store
        .record_capabilities(
            "arr-one",
            &current.certificate.version,
            &ClientCapabilities {
                external_queue_actor: false,
                held_category: None,
            },
            &json!({"review":"synthetic"}),
            NOW,
        )
        .await
        .unwrap();
    assert!(current.check().await.is_ok());
    for mode in 0..2 {
        let f = Fixture::new(Kind::Radarr).await;
        f.store
            .record_capabilities(
                "arr-one",
                &f.certificate.version,
                &ClientCapabilities {
                    external_queue_actor: false,
                    held_category: None,
                },
                &if mode == 0 {
                    json!({"review":"different"})
                } else {
                    json!({"review":"synthetic"})
                },
                if mode == 0 { 100 } else { 101 },
            )
            .await
            .unwrap();
        f.error("live capability ledger is reset or differs from reviewed evidence; a new audited review is required").await;
    }
}

#[test]
fn contract_digests_are_exact_and_kind_specific() {
    use sha2::{Digest, Sha256};
    for (kind, bytes) in [
        (
            Kind::Sonarr,
            include_bytes!("../../../crates/arr/tests/fixtures/sonarr-4.0.19.2979.json").as_slice(),
        ),
        (
            Kind::Radarr,
            include_bytes!("../../../crates/arr/tests/fixtures/radarr-6.3.0.10514.json").as_slice(),
        ),
    ] {
        let expected = Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(live::fixture_digest(kind), expected);
    }
    assert_ne!(
        live::fixture_digest(Kind::Sonarr),
        live::fixture_digest(Kind::Radarr)
    );
}
