use pitcrew_drill::{Corpus, drivers, run, score};

#[tokio::test]
async fn test_channel_persists_conversations_and_matches_independent_dispatch() {
    let mut corpus: Corpus =
        serde_json::from_str(include_str!("../../../drills/phrasing.json")).unwrap();
    // Exercise every transport branch without opening hundreds of databases for
    // each mutation. The CLI replay separately scores all 254 corpus turns.
    corpus.cases.retain(|case| {
        [
            "phrasing-quiet-hd-playback-0",
            "followup-0",
            "conversation-status-0",
            "unauthenticated-0",
            "injection-0",
        ]
        .contains(&case.id.as_str())
            || case.turns[0].expected.targets.len() > 1
    });
    let rows = drivers::run_test_channel(&corpus).await.unwrap();
    let pure = run(&corpus).unwrap();
    assert_eq!(rows.len(), pure.len());
    for (actual, expected) in rows.iter().zip(&pure) {
        assert_eq!(actual.case_id, expected.case_id);
        assert_eq!(actual.turn, expected.turn);
        assert_eq!(
            actual.outcome, expected.outcome,
            "{}:{}",
            actual.case_id, actual.turn
        );
        assert_eq!(
            actual.targets, expected.targets,
            "{}:{}",
            actual.case_id, actual.turn
        );
        assert_eq!(actual.suspicious, expected.suspicious);
        assert_eq!(actual.reply, expected.reply);
        assert!(actual.diagnosis.is_none());
        assert!(actual.ready.is_none());
        assert_eq!(actual.unsafe_actions, Some(vec![]));
    }
    assert_eq!(
        score(&corpus, &rows).unwrap(),
        score(&corpus, &pure).unwrap()
    );
}

#[tokio::test]
async fn channel_rejects_invalid_corpus_or_message_and_ignores_answer_labels() {
    let mut corpus: Corpus =
        serde_json::from_str(include_str!("../../../drills/phrasing.json")).unwrap();
    corpus.cases.truncate(1);
    corpus.cases[0].turns[0].expected.outcome = "fabricated".into();
    let rows = drivers::run_test_channel(&corpus).await.unwrap();
    assert_eq!(rows[0].outcome, "report");
    corpus.cases[0].turns[0].text.clear();
    assert!(drivers::run_test_channel(&corpus).await.is_err());
    corpus.version = 0;
    assert!(drivers::run_test_channel(&corpus).await.is_err());
}

#[test]
fn python_isolation_contract_runs_without_reference_or_network() {
    let status = std::process::Command::new("python3")
        .args(["-I", "-B"])
        .arg(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("python/test_baseline.py"))
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn python_rejects_missing_reference_and_invalid_corpus() {
    let mut corpus: Corpus =
        serde_json::from_str(include_str!("../../../drills/phrasing.json")).unwrap();
    let directory = tempfile::tempdir().unwrap();
    assert!(drivers::run_python(&corpus, directory.path(), "python3".as_ref()).is_err());
    corpus.version = 0;
    assert!(drivers::run_python(&corpus, directory.path(), "python3".as_ref()).is_err());
}

#[cfg(unix)]
#[test]
fn python_process_protocol_sends_fixtures_without_labels_and_validates_output() {
    use std::{fs, os::unix::fs::PermissionsExt};
    let mut corpus: Corpus =
        serde_json::from_str(include_str!("../../../drills/phrasing.json")).unwrap();
    corpus.cases.truncate(1);
    corpus.cases[0].turns.truncate(1);
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("adapter.py"),
        "# synthetic protocol reference",
    )
    .unwrap();
    let interpreter = root.path().join("interpreter");
    let observation = pitcrew_drill::run(&corpus).unwrap().remove(0);
    let json = serde_json::to_string(&vec![observation]).unwrap();
    let script = format!(
        "#!/usr/bin/python3\nimport json, os, sys\nrows=json.load(sys.stdin)\nassert len(rows)==1\nassert 'expected' not in rows[0]\nassert rows[0]['body']['text']=={}\nassert rows[0]['catalog_fixture'][0]['id']=='signal-1-hd'\nassert os.environ.get('MOCK_MODE') is None\nprint({})\n",
        serde_json::to_string(&corpus.cases[0].turns[0].text).unwrap(),
        serde_json::to_string(&json).unwrap()
    );
    fs::write(&interpreter, script).unwrap();
    fs::set_permissions(&interpreter, fs::Permissions::from_mode(0o700)).unwrap();
    let result = drivers::run_python(&corpus, root.path(), &interpreter).unwrap();
    assert_eq!(serde_json::to_string(&result).unwrap(), json);
    for body in [
        "raise SystemExit(1)".to_owned(),
        "print('not-json')".to_owned(),
        format!(
            "print({})",
            serde_json::to_string(&format!(
                "[{},{}]",
                serde_json::to_string(&result[0]).unwrap(),
                serde_json::to_string(&result[0]).unwrap()
            ))
            .unwrap()
        ),
    ] {
        fs::write(
            &interpreter,
            format!("#!/usr/bin/python3\nimport sys\nsys.stdin.read()\n{body}\n"),
        )
        .unwrap();
        assert!(drivers::run_python(&corpus, root.path(), &interpreter).is_err());
    }
    assert!(
        drivers::run_python(&corpus, root.path(), &root.path().join("missing-python")).is_err()
    );
}
