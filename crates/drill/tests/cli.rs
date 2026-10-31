use std::{fs, process::Command};

fn cmd(args: &[&std::ffi::OsStr]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_pitcrew-drill"))
        .args(args)
        .output()
        .unwrap()
}
#[test]
fn commands_write_no_overwrite_results_and_export_without_network() {
    let temp = tempfile::tempdir().unwrap();
    let corpus =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../drills/phrasing.json");
    let rows = temp.path().join("observations.jsonl");
    let result = cmd(&["run".as_ref(), corpus.as_os_str(), rows.as_os_str()]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let score: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(score["intake"]["expected"], 254);
    assert_eq!(fs::read_to_string(&rows).unwrap().lines().count(), 254);
    let saved = fs::read(&rows).unwrap();
    assert!(
        !cmd(&["run".as_ref(), corpus.as_os_str(), rows.as_os_str()])
            .status
            .success()
    );
    assert_eq!(fs::read(&rows).unwrap(), saved);
    let rescored = cmd(&["score".as_ref(), corpus.as_os_str(), rows.as_os_str()]);
    assert!(rescored.status.success());
    assert_eq!(rescored.stdout, result.stdout);
    let export = temp.path().join("requests.jsonl");
    let result = cmd(&[
        "export-external".as_ref(),
        corpus.as_os_str(),
        export.as_os_str(),
    ]);
    assert!(result.status.success());
    assert!(result.stdout.is_empty());
    let lines = fs::read_to_string(&export).unwrap();
    assert_eq!(lines.lines().count(), 254);
    let first: serde_json::Value = serde_json::from_str(lines.lines().next().unwrap()).unwrap();
    assert!(first.get("path").is_none());
    assert!(first["body"]["text"].is_string());
    assert!(first["body"].get("discord_id").is_none());
    assert!(first.get("expected").is_none());
    assert!(
        !cmd(&[
            "export-python".as_ref(),
            corpus.as_os_str(),
            export.as_os_str()
        ])
        .status
        .success()
    );
}
#[test]
fn invalid_cli_and_broken_files_fail_closed() {
    let temp = tempfile::tempdir().unwrap();
    let corpus = temp.path().join("corpus.json");
    assert!(!cmd(&[]).status.success());
    assert!(
        !cmd(&["run".as_ref(), corpus.as_os_str(), corpus.as_os_str()])
            .status
            .success()
    );
    fs::write(&corpus, "{bad json}").unwrap();
    assert!(
        !cmd(&["run".as_ref(), corpus.as_os_str(), corpus.as_os_str()])
            .status
            .success()
    );
    fs::write(&corpus, include_str!("../../../drills/phrasing.json")).unwrap();
    assert!(
        !cmd(&["bad".as_ref(), corpus.as_os_str(), corpus.as_os_str()])
            .status
            .success()
    );
    let observations = temp.path().join("observations.jsonl");
    fs::write(&observations, "bad json\n").unwrap();
    assert!(
        !cmd(&[
            "score".as_ref(),
            corpus.as_os_str(),
            observations.as_os_str()
        ])
        .status
        .success()
    );
}

#[test]
fn channel_driver_writes_observed_turns_and_scores_the_same_saved_rows() {
    let temp = tempfile::tempdir().unwrap();
    let corpus = temp.path().join("corpus.json");
    let rows = temp.path().join("rows.jsonl");
    let mut value: serde_json::Value =
        serde_json::from_str(include_str!("../../../drills/phrasing.json")).unwrap();
    value["cases"].as_array_mut().unwrap().truncate(1);
    fs::write(&corpus, serde_json::to_vec(&value).unwrap()).unwrap();
    let output = cmd(&["run-channel".as_ref(), corpus.as_os_str(), rows.as_os_str()]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let score: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(score["intake"]["passed"], 1);
    assert_eq!(
        cmd(&["score".as_ref(), corpus.as_os_str(), rows.as_os_str()]).stdout,
        output.stdout
    );
    assert!(
        !cmd(&["run-channel".as_ref(), corpus.as_os_str(), rows.as_os_str()])
            .status
            .success()
    );
}

#[cfg(unix)]
#[test]
fn python_cli_clears_parent_environment_and_keeps_observations_immutable() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let corpus = temp.path().join("corpus.json");
    let rows = temp.path().join("rows.jsonl");
    let mut value: serde_json::Value =
        serde_json::from_str(include_str!("../../../drills/phrasing.json")).unwrap();
    value["cases"].as_array_mut().unwrap().truncate(1);
    fs::write(&corpus, serde_json::to_vec(&value).unwrap()).unwrap();
    fs::write(temp.path().join("adapter.py"), "# synthetic reference").unwrap();
    let interpreter = temp.path().join("python-stub");
    fs::write(&interpreter, "#!/usr/bin/python3\nimport json,os,sys\nassert 'PITCREW_DRILL_PARENT_SECRET' not in os.environ\nrequest=json.load(sys.stdin)[0]\nassert 'expected' not in request\nprint(json.dumps([{'case_id':request['case_id'],'turn':request['turn'],'outcome':'report','targets':[],'suspicious':False,'reply':'Observed child reply','diagnosis':None,'ready':None,'unsafe_actions':None}]))\n").unwrap();
    fs::set_permissions(&interpreter, fs::Permissions::from_mode(0o700)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_pitcrew-drill"))
        .args([
            "external".as_ref(),
            corpus.as_os_str(),
            rows.as_os_str(),
            temp.path().as_os_str(),
            interpreter.as_os_str(),
        ])
        .env("PITCREW_DRILL_PARENT_SECRET", "synthetic-do-not-inherit")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let saved = fs::read(&rows).unwrap();
    assert!(String::from_utf8_lossy(&saved).contains("Observed child reply"));
    assert!(
        !cmd(&[
            "python".as_ref(),
            corpus.as_os_str(),
            rows.as_os_str(),
            temp.path().as_os_str(),
            interpreter.as_os_str()
        ])
        .status
        .success()
    );
    assert_eq!(fs::read(&rows).unwrap(), saved);
    assert!(
        !cmd(&[
            "python".as_ref(),
            corpus.as_os_str(),
            rows.as_os_str(),
            temp.path().as_os_str(),
            interpreter.as_os_str(),
            "extra".as_ref()
        ])
        .status
        .success()
    );
    // "python" is an accepted alias of "external" for a fresh output file.
    let alias_rows = temp.path().join("alias-rows.jsonl");
    let output = cmd(&[
        "python".as_ref(),
        corpus.as_os_str(),
        alias_rows.as_os_str(),
        temp.path().as_os_str(),
        interpreter.as_os_str(),
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fs::read_to_string(&alias_rows)
            .unwrap()
            .contains("Observed child reply")
    );
}
