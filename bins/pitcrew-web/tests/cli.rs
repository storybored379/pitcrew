use serde_json::json;
use std::process::Command;
fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_pitcrew-web"))
        .args(args)
        .output()
        .unwrap()
}
#[test]
fn cli_help_and_configuration_validation_never_start_upstreams() {
    let output = run(&["--help"]);
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("No repair effects")
    );
    for args in [
        vec![],
        vec!["--wrong"],
        vec!["--config"],
        vec!["--config", "missing-file"],
        vec!["--config", "missing", "--wrong"],
        vec!["--bad", "missing", "--check"],
    ] {
        assert!(!run(&args).status.success());
    }
    for args in [
        vec!["--bad", "missing"],
        vec!["--config", "missing", "--wrong"],
    ] {
        assert!(
            String::from_utf8(run(&args).stderr)
                .unwrap()
                .contains("usage:")
        );
    }
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("web.json");
    let database = dir.path().join("journal.sqlite");
    std::fs::write(&config, "private-invalid-input").unwrap();
    let output = run(&["--config", config.to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(
        !String::from_utf8(output.stderr)
            .unwrap()
            .contains("private-invalid-input")
    );
    let mut value = json!({"database":database,"public_origin":"http://127.0.0.1:7401","plex_server_url":"https://media.example.invalid/","plex_server_id":"server","client_id":"client","catalog":[]});
    std::fs::write(&config, value.to_string()).unwrap();
    let output = run(&["--config", config.to_str().unwrap(), "--check"]);
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("upstreams not contacted")
    );
    assert!(!database.exists());
    value["database"] = json!(dir.path().join("absent/journal.sqlite"));
    std::fs::write(&config, value.to_string()).unwrap();
    let output = run(&["--config", config.to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("intake database unavailable")
    );
    value["plex_server_url"] = json!("https://user:noreply@media.example.invalid/");
    std::fs::write(&config, value.to_string()).unwrap();
    let output = run(&["--config", config.to_str().unwrap(), "--check"]);
    assert!(!output.status.success());
    assert!(
        !String::from_utf8(output.stderr)
            .unwrap()
            .contains("noreply")
    );
}
