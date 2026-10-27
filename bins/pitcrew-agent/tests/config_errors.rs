#[test]
fn missing_version_is_actionable_in_the_actual_binary() {
    let path = std::env::temp_dir().join(format!("pitcrew-config-repro-{}", std::process::id()));
    std::fs::write(&path, "# configuration intentionally missing version\n").unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_pitcrew-agent"))
        .arg("--config")
        .arg(&path)
        .output()
        .unwrap();
    std::fs::remove_file(path).unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("version"), "{stderr}");
    assert!(stderr.contains("required field is missing"), "{stderr}");
}
