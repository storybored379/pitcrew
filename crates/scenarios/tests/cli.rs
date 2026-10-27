use std::process::Command;

#[test]
fn optional_replay_skips_only_when_no_adapter_options_were_requested() {
    for command in ["external", "python"] {
        let output = Command::new(env!("CARGO_BIN_EXE_pitcrew-scenarios"))
            .arg(command)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("skipped"));
        for options in [vec!["--adapter"], vec!["--reference", "missing-source"]] {
            let output = Command::new(env!("CARGO_BIN_EXE_pitcrew-scenarios"))
                .arg(command)
                .args(options)
                .output()
                .unwrap();
            assert!(!output.status.success());
            assert!(String::from_utf8_lossy(&output.stderr).contains("--adapter PATH"));
        }
    }
}

#[test]
fn adapter_path_cannot_be_a_python_option() {
    let mut accepted = Vec::new();
    for command in ["external", "python"] {
        for option in ["--help", "-V"] {
            let output = Command::new(env!("CARGO_BIN_EXE_pitcrew-scenarios"))
                .args([command, "--adapter", option])
                .output()
                .unwrap();
            if output.status.success() {
                accepted.push((command, option));
            }
        }
    }
    assert!(
        accepted.is_empty(),
        "accepted interpreter options: {accepted:?}"
    );
}

#[test]
fn adapter_path_must_name_an_existing_regular_file() {
    for adapter in [env!("CARGO_MANIFEST_DIR"), "missing-synthetic-adapter.py"] {
        let output = Command::new(env!("CARGO_BIN_EXE_pitcrew-scenarios"))
            .args(["external", "--adapter", adapter])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("existing regular file"));
    }
}
