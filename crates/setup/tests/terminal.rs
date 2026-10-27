use pitcrew_setup::{Prompt, Terminal};
use std::process::Command;

#[test]
fn terminal_child() {
    let Ok(mode) = std::env::var("PITCREW_SETUP_TEST_PROMPT") else {
        return;
    };
    let result = Terminal.read("Setup input", !matches!(mode.as_str(), "plain" | "eof"));
    match mode.as_str() {
        "plain" => assert_eq!(result.unwrap(), "plain value "),
        "secret" => assert_eq!(result.unwrap(), "synthetic-secret"),
        "cancel" => assert_eq!(result.unwrap_err(), "init: setup cancelled"),
        "eof" => assert_eq!(
            result.unwrap_err(),
            "init: input ended before setup completed"
        ),
        "pipe" => assert_eq!(
            result.unwrap_err(),
            "init: interactive input needs a terminal; supply flags and --non-interactive"
        ),
        _ => panic!("unexpected child mode"),
    }
}

#[test]
fn terminal_hides_secrets_and_restores_settings_after_success_or_cancel() {
    let status = Command::new("python3")
        .args([
            "-B",
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/terminal.py"),
        ])
        .arg(std::env::current_exe().unwrap())
        .status()
        .unwrap();
    assert!(status.success());
}
