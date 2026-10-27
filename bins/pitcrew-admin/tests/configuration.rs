use pitcrew_admin::config::{self, Config};
use pitcrew_config::{SecretRef, admin::AdminEndpoint};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn configuration(dir: &Path) -> Config {
    Config {
        database: dir.join("admin.sqlite"),
        engine_database: dir.join("engine.sqlite"),
        intake_database: dir.join("intake.sqlite"),
        bind: "127.0.0.1:0".parse().unwrap(),
        public_origin: "https://admin.example.invalid".into(),
        plex_auth_url: "https://plex.example.invalid/".into(),
        client_id: "synthetic-installation".into(),
        admin_account_ids: vec!["1001".into()],
        api: None,
        rates: Default::default(),
    }
}

fn quoted(path: &Path) -> String {
    toml::Value::String(path.to_str().unwrap().into()).to_string()
}

fn document(config: &Config) -> String {
    format!(
        r#"version = 1
database = "shared-engine.sqlite"
[engine]
mode = "shadow"
[admin]
database = {}
engine_database = {}
intake_database = {}
bind = "127.0.0.1:0"
public_origin = "https://admin.example.invalid"
plex_auth_url = "https://plex.example.invalid/"
client_id = "synthetic-installation"
admin_account_ids = ["1001", "1002"]
[admin.api]
url = "http://127.0.0.1:7400/"
secret = {{ env = "PITCREW_SYNTHETIC_ADMIN_SECRET" }}
[admin.rates.anthropic]
input_microusd_per_million = 10
output_microusd_per_million = 20
cache_creation_microusd_per_million = 30
cache_read_microusd_per_million = 40
"#,
        quoted(&config.database),
        quoted(&config.engine_database),
        quoted(&config.intake_database),
    )
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut path = path.as_os_str().to_owned();
    path.push(suffix);
    path.into()
}

fn source_files(config: &Config) {
    fs::write(&config.engine_database, b"synthetic engine source").unwrap();
    fs::write(&config.intake_database, b"synthetic intake source").unwrap();
}

fn cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pitcrew-admin"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn shared_toml_document_keeps_admin_configuration_and_local_rates() {
    let dir = tempfile::tempdir().unwrap();
    let expected = configuration(dir.path());
    let parsed = config::parse(&document(&expected)).unwrap();
    assert_eq!(parsed.database, expected.database);
    assert_eq!(parsed.engine_database, expected.engine_database);
    assert_eq!(parsed.intake_database, expected.intake_database);
    assert_eq!(parsed.admin_account_ids, ["1001", "1002"]);
    assert_eq!(
        parsed.api.as_ref().unwrap().secret.env.as_deref(),
        Some("PITCREW_SYNTHETIC_ADMIN_SECRET")
    );
    assert_eq!(
        parsed.rates["anthropic"].cache_read_microusd_per_million,
        40
    );
    assert!(!expected.database.exists());
    assert!(!expected.engine_database.exists());
    assert!(!expected.intake_database.exists());
}

#[test]
fn origins_and_endpoints_require_https_or_explicit_loopback_http() {
    for endpoint in [
        "https://admin.example.invalid/",
        "http://127.0.0.1:7400/",
        "http://localhost:7400/",
        "http://[::1]:7400/",
    ] {
        assert!(config::endpoint(endpoint).is_ok(), "{endpoint}");
    }
    for endpoint in [
        "http://admin.example.invalid/",
        "ftp://admin.example.invalid/",
        "https://owner:noreply@admin.example.invalid/",
        "https://noreply@admin.example.invalid/",
        "https://:noreply@admin.example.invalid/",
        "https://admin.example.invalid/path",
        "https://admin.example.invalid/?token=synthetic",
        "https://admin.example.invalid/#fragment",
        "not-a-url",
        "",
    ] {
        assert!(config::endpoint(endpoint).is_err(), "{endpoint}");
    }
    let dir = tempfile::tempdir().unwrap();
    for origin in [
        "https://admin.example.invalid",
        "http://127.0.0.1:7400",
        "http://localhost:7400",
        "http://[::1]:7400",
    ] {
        let mut config = configuration(dir.path());
        config.public_origin = origin.into();
        assert!(config::validate(&config).is_ok(), "{origin}");
    }
    for origin in [
        "https://admin.example.invalid/",
        "https://admin.example.invalid/path",
        "http://admin.example.invalid",
        "https://admin.example.invalid:443",
        "https://owner:noreply@admin.example.invalid",
        "https://admin.example.invalid?query=1",
    ] {
        let mut config = configuration(dir.path());
        config.public_origin = origin.into();
        assert!(config::validate(&config).is_err(), "{origin}");
    }
}

#[test]
fn invalid_allowlists_client_ids_and_database_names_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    for ids in [
        vec![],
        vec!["0"],
        vec![""],
        vec!["owner"],
        vec!["-1"],
        vec![" 1001"],
        vec!["18446744073709551616"],
        vec!["1001", "1001"],
    ] {
        let mut config = configuration(dir.path());
        config.admin_account_ids = ids.iter().map(|id| (*id).into()).collect();
        assert!(config::validate(&config).is_err(), "{ids:?}");
    }
    let mut config = configuration(dir.path());
    config.client_id = " \n ".into();
    assert!(config::validate(&config).is_err());
    for field in 0..3 {
        let mut config = configuration(dir.path());
        match field {
            0 => config.database = PathBuf::new(),
            1 => config.engine_database = PathBuf::new(),
            _ => config.intake_database = PathBuf::new(),
        }
        assert!(config::validate(&config).is_err());
    }
    for endpoint in [
        "https://plex.example.invalid/private",
        "http://plex.example.invalid/",
    ] {
        let mut config = configuration(dir.path());
        config.plex_auth_url = endpoint.into();
        assert!(config::validate(&config).is_err());
    }
}

#[test]
fn secret_references_validate_shape_without_resolving_credentials() {
    let dir = tempfile::tempdir().unwrap();
    for secret in [
        SecretRef {
            env: None,
            file: None,
        },
        SecretRef {
            env: Some("".into()),
            file: None,
        },
        SecretRef {
            env: Some("1INVALID".into()),
            file: None,
        },
        SecretRef {
            env: Some("INVALID-NAME".into()),
            file: None,
        },
        SecretRef {
            env: None,
            file: Some("relative.secret".into()),
        },
        SecretRef {
            env: Some("PITCREW_SYNTHETIC_ADMIN_SECRET".into()),
            file: Some(dir.path().join("missing.secret")),
        },
    ] {
        let mut config = configuration(dir.path());
        config.api = Some(AdminEndpoint {
            url: "https://control.example.invalid/".into(),
            secret,
        });
        assert!(config::validate(&config).is_err());
    }
    for secret in [
        SecretRef {
            env: Some("PITCREW_SYNTHETIC_ADMIN_SECRET".into()),
            file: None,
        },
        SecretRef {
            env: None,
            file: Some(dir.path().join("missing.secret")),
        },
    ] {
        let mut config = configuration(dir.path());
        config.api = Some(AdminEndpoint {
            url: "https://control.example.invalid/".into(),
            secret,
        });
        assert!(config::validate(&config).is_ok());
        config.api.as_mut().unwrap().url = "http://control.example.invalid/".into();
        assert!(config::validate(&config).is_err());
    }
    assert!(!dir.path().join("missing.secret").exists());
}

#[test]
fn malformed_documents_and_unknown_admin_fields_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let source = document(&configuration(dir.path()));
    for input in [
        "private-synthetic-invalid-input".into(),
        "version = 1".into(),
        source.replace("[admin]\n", "[admin]\nunrecognized = true\n"),
        source.replace(
            "admin_account_ids = [\"1001\", \"1002\"]",
            "admin_account_ids = [1001]",
        ),
        source.replace(
            "env = \"PITCREW_SYNTHETIC_ADMIN_SECRET\"",
            "literal = \"synthetic-never-echo\"",
        ),
    ] {
        assert!(config::parse(&input).is_err());
    }
}

#[test]
fn source_guard_allows_separate_admin_database_without_creating_it() {
    let dir = tempfile::tempdir().unwrap();
    let config = configuration(dir.path());
    source_files(&config);
    config::protect_sources(&config).unwrap();
    assert!(!config.database.exists());
    fs::write(&config.database, b"independent admin source").unwrap();
    config::protect_sources(&config).unwrap();
    assert_eq!(
        fs::read(&config.database).unwrap(),
        b"independent admin source"
    );
}

#[test]
fn source_guard_rejects_a_regular_file_as_the_destination_parent() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = configuration(dir.path());
    source_files(&config);
    let parent = dir.path().join("not-a-directory");
    fs::write(&parent, b"synthetic regular file").unwrap();
    config.database = parent.join("admin.sqlite");
    assert!(config::protect_sources(&config).is_err());
    assert_eq!(fs::read(parent).unwrap(), b"synthetic regular file");
}

#[test]
fn source_guard_rejects_source_files_and_their_future_sidecars() {
    let dir = tempfile::tempdir().unwrap();
    let original = configuration(dir.path());
    source_files(&original);
    for source in [&original.engine_database, &original.intake_database] {
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let mut config = configuration(dir.path());
            config.database = sidecar(source, suffix);
            assert!(config::protect_sources(&config).is_err(), "{suffix}");
        }
    }
    let mut config = configuration(dir.path());
    config.database = dir.path().join(".").join("engine.sqlite");
    assert!(config::protect_sources(&config).is_err());
}

#[cfg(unix)]
#[test]
fn source_guard_rejects_symlinks_hardlinks_and_dangling_destinations() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let config = configuration(dir.path());
    source_files(&config);
    for source in [&config.engine_database, &config.intake_database] {
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let target = sidecar(source, suffix);
            if !suffix.is_empty() {
                fs::write(&target, b"synthetic source sidecar").unwrap();
            }
            symlink(&target, &config.database).unwrap();
            assert!(config::protect_sources(&config).is_err());
            fs::remove_file(&config.database).unwrap();
            fs::hard_link(&target, &config.database).unwrap();
            assert!(config::protect_sources(&config).is_err());
            fs::remove_file(&config.database).unwrap();
        }
    }
    symlink(dir.path().join("missing-target.sqlite"), &config.database).unwrap();
    assert!(config::protect_sources(&config).is_err());
}

#[test]
fn source_guard_rejects_admin_sidecar_overlapping_a_source_database() {
    for suffix in ["-journal", "-wal", "-shm"] {
        let dir = tempfile::tempdir().unwrap();
        let mut config = configuration(dir.path());
        config.engine_database = sidecar(&config.database, suffix);
        source_files(&config);
        assert!(
            config::protect_sources(&config).is_err(),
            "the writable admin {suffix} would overlap the engine source"
        );
        assert!(!config.database.exists());
    }
}

#[cfg(unix)]
#[test]
fn source_guard_rejects_admin_sidecar_hardlinks_to_source_databases() {
    for suffix in ["-journal", "-wal", "-shm"] {
        let dir = tempfile::tempdir().unwrap();
        let config = configuration(dir.path());
        source_files(&config);
        fs::hard_link(&config.engine_database, sidecar(&config.database, suffix)).unwrap();
        assert!(
            config::protect_sources(&config).is_err(),
            "the writable admin {suffix} aliases the engine inode"
        );
        assert!(!config.database.exists());
    }
}

#[test]
fn cli_help_and_check_do_not_open_databases_or_resolve_secret_files() {
    let help = cli(&["--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8(help.stdout).unwrap().contains("--check"));
    let dir = tempfile::tempdir().unwrap();
    let config = configuration(dir.path());
    let secret = dir.path().join("absent-synthetic-secret");
    let source = document(&config).replace(
        "env = \"PITCREW_SYNTHETIC_ADMIN_SECRET\"",
        &format!("file = {}", quoted(&secret)),
    );
    let path = dir.path().join("config.toml");
    fs::write(&path, source).unwrap();
    let result = cli(&["--config", path.to_str().unwrap(), "--check"]);
    assert!(result.status.success(), "{:?}", result.stderr);
    assert!(
        String::from_utf8(result.stdout)
            .unwrap()
            .contains("no secrets resolved, databases opened or upstreams contacted")
    );
    assert!(!config.database.exists());
    assert!(!config.engine_database.exists());
    assert!(!config.intake_database.exists());
    assert!(!secret.exists());
}

#[test]
fn cli_rejects_invalid_arguments_and_configuration_without_echoing_input() {
    for args in [
        vec![],
        vec!["--wrong"],
        vec!["--config"],
        vec!["--wrong", "missing"],
        vec!["--wrong", "missing", "--check"],
        vec!["--config", "missing", "--wrong"],
    ] {
        let result = cli(&args);
        assert!(!result.status.success());
        assert!(String::from_utf8(result.stderr).unwrap().contains("usage:"));
    }
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("not-created.toml");
    let result = cli(&["--config", missing.to_str().unwrap(), "--check"]);
    assert!(!result.status.success());
    assert_eq!(
        String::from_utf8(result.stderr).unwrap().trim(),
        "configuration unreadable"
    );
    let config = configuration(dir.path());
    let path = dir.path().join("invalid.toml");
    for source in [
        "private-synthetic-invalid-input".into(),
        document(&config).replace(
            "https://admin.example.invalid",
            "https://owner:noreply@admin.example.invalid",
        ),
    ] {
        fs::write(&path, source).unwrap();
        let result = cli(&["--config", path.to_str().unwrap(), "--check"]);
        assert!(!result.status.success());
        assert_eq!(
            String::from_utf8(result.stderr).unwrap().trim(),
            "invalid admin configuration"
        );
    }
    assert!(!config.database.exists());
}

#[test]
fn cli_refuses_protected_destination_before_resolving_secret_or_opening_source() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = configuration(dir.path());
    source_files(&config);
    config.database = config.engine_database.clone();
    let path = dir.path().join("protected.toml");
    let source = document(&config).replace(
        "env = \"PITCREW_SYNTHETIC_ADMIN_SECRET\"",
        &format!("file = {}", quoted(&dir.path().join("missing-secret"))),
    );
    fs::write(&path, source).unwrap();
    let result = cli(&["--config", path.to_str().unwrap()]);
    assert!(!result.status.success());
    assert!(
        String::from_utf8(result.stderr)
            .unwrap()
            .contains("admin database must be separate")
    );
    assert_eq!(
        fs::read(&config.engine_database).unwrap(),
        b"synthetic engine source"
    );
    assert_eq!(
        fs::read(&config.intake_database).unwrap(),
        b"synthetic intake source"
    );
}
