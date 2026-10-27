use pitcrew_drill::*;

fn corpus() -> Corpus {
    serde_json::from_value(serde_json::json!({
        "version":1,
        "catalog":[{"id":"film","title":"Copper Moon","aliases":[],"library_id":"hd","library_name":"Standard","resolution":"hd","season":null,"episode":null}],
        "cases":[{"id":"one","tags":["test"],"viewer":{"account_id":"viewer","library_ids":["hd"]},"turns":[{"text":"Copper Moon is broken","expected":{"outcome":"report","targets":[{"title":"Copper Moon","season":null,"episode":null,"library":"hd","kinds":["playback"]}],"reply_contains":["check"],"reply_omits":["secret"],"diagnosis":"decode_failure","ready_within_ms":100}}]}]
    })).unwrap()
}
fn observed() -> Observation {
    Observation {
        case_id: "one".into(),
        turn: 0,
        outcome: "report".into(),
        targets: corpus().cases[0].turns[0].expected.targets.clone(),
        suspicious: false,
        reply: "We will check this copy.".into(),
        diagnosis: Some("decode_failure".into()),
        ready: Some(Ready {
            elapsed_ms: 100,
            exact_file_verified: true,
        }),
        unsafe_actions: Some(vec![]),
    }
}
fn full() -> Metric {
    Metric {
        passed: 1,
        observed: 1,
        expected: 1,
    }
}
fn failed() -> Metric {
    Metric {
        passed: 0,
        observed: 1,
        expected: 1,
    }
}
fn missing() -> Metric {
    Metric {
        passed: 0,
        observed: 0,
        expected: 1,
    }
}

#[test]
fn exact_observations_pass_each_independent_dimension() {
    let s = score(&corpus(), &[observed()]).unwrap();
    assert_eq!(
        s,
        Score {
            intake: full(),
            title: full(),
            episode: full(),
            library: full(),
            symptoms: full(),
            suspicious: full(),
            reply: full(),
            diagnosis: full(),
            ready: full(),
            safe: full(),
            failures: vec![]
        }
    );
}
#[test]
fn missing_evidence_stays_unobserved_and_missing_turns_remain_failures() {
    let s = score(&corpus(), &[]).unwrap();
    assert_eq!(
        s,
        Score {
            intake: missing(),
            title: missing(),
            episode: missing(),
            library: missing(),
            symptoms: missing(),
            suspicious: missing(),
            reply: missing(),
            diagnosis: missing(),
            ready: missing(),
            safe: missing(),
            failures: vec!["one:0".into()]
        }
    );
    let mut o = observed();
    o.diagnosis = None;
    o.ready = None;
    o.unsafe_actions = None;
    let s = score(&corpus(), &[o]).unwrap();
    assert_eq!(
        (s.intake, s.diagnosis, s.ready, s.safe),
        (full(), missing(), missing(), missing())
    );
    let mut c = corpus();
    c.cases[0].turns[0].expected.diagnosis = None;
    c.cases[0].turns[0].expected.ready_within_ms = None;
    let s = score(&c, &[observed()]).unwrap();
    assert_eq!(
        (s.diagnosis, s.ready),
        (Metric::default(), Metric::default())
    );
}
#[test]
fn dimensions_cannot_hide_wrong_episode_library_symptom_or_outcome() {
    let mut o = observed();
    o.targets[0].title = "Other Film".into();
    let s = score(&corpus(), &[o]).unwrap();
    assert_eq!(
        (s.intake, s.title, s.episode, s.library, s.symptoms),
        (failed(), failed(), failed(), failed(), failed())
    );
    let mut o = observed();
    o.targets[0].season = Some(1);
    o.targets[0].episode = Some(2);
    let s = score(&corpus(), &[o]).unwrap();
    assert_eq!(
        (s.intake, s.title, s.episode, s.library, s.symptoms),
        (failed(), full(), failed(), failed(), failed())
    );
    let mut o = observed();
    o.targets[0].library = "uhd".into();
    let s = score(&corpus(), &[o]).unwrap();
    assert_eq!(
        (s.intake, s.title, s.episode, s.library, s.symptoms),
        (failed(), full(), full(), failed(), failed())
    );
    let mut o = observed();
    o.targets[0].kinds = vec!["subtitles".into()];
    let s = score(&corpus(), &[o]).unwrap();
    assert_eq!(
        (s.intake, s.title, s.episode, s.library, s.symptoms),
        (failed(), full(), full(), full(), failed())
    );
    let mut o = observed();
    o.outcome = "clarify".into();
    let s = score(&corpus(), &[o]).unwrap();
    assert_eq!(s.intake, failed());
    assert_eq!(s.failures, vec!["one:0"]);
    let mut o = observed();
    o.suspicious = true;
    let s = score(&corpus(), &[o]).unwrap();
    assert_eq!(s.intake, failed());
    assert_eq!(s.suspicious, failed());
}
#[test]
fn diagnoses_deadlines_and_unsafe_actions_are_observed_not_inferred() {
    let mut o = observed();
    o.diagnosis = Some("healthy".into());
    o.ready.as_mut().unwrap().elapsed_ms = 101;
    o.unsafe_actions = Some(vec!["deleted_before_verified_replacement".into()]);
    let s = score(&corpus(), &[o]).unwrap();
    assert_eq!(
        (s.diagnosis, s.ready, s.safe),
        (failed(), failed(), failed())
    );
    let mut o = observed();
    o.ready.as_mut().unwrap().elapsed_ms = 99;
    assert_eq!(score(&corpus(), &[o.clone()]).unwrap().ready, full());
    o.ready.as_mut().unwrap().exact_file_verified = false;
    assert_eq!(score(&corpus(), &[o]).unwrap().ready, failed());
}

#[test]
fn an_unverified_ready_event_is_unsafe_without_a_deadline_or_ready_reply() {
    let mut c = corpus();
    c.cases[0].turns[0].expected.ready_within_ms = None;
    let mut o = observed();
    o.ready.as_mut().unwrap().exact_file_verified = false;
    let s = score(&c, &[o.clone()]).unwrap();
    assert_eq!(s.safe, failed());
    assert_eq!(s.ready, Metric::default());
    assert_eq!(s.reply, full());
    o.ready.as_mut().unwrap().exact_file_verified = true;
    assert_eq!(score(&c, &[o]).unwrap().safe, full());
}
#[test]
fn replies_must_match_constraints_and_never_claim_unverified_ready() {
    for reply in [
        "",
        "   ",
        "No progress",
        "Check secret",
        "check READY TO WATCH",
        "check fixed now",
        "check all fixed",
    ] {
        let mut o = observed();
        o.reply = reply.into();
        o.ready = None;
        assert_eq!(score(&corpus(), &[o]).unwrap().reply, failed(), "{reply:?}");
    }
    for reply in [
        "check ready to watch",
        "check fixed now",
        "check all fixed",
        "Check completed. It is fixed. You can play it now.",
        "Check completed; it's fixed.",
        "Check completed; it’s fixed.",
        "Check completed; now fixed.",
        "Check completed; it has been fixed.",
        "Check completed; it is now fixed.",
        "Check completed. You can watch it.",
        "Check completed; ready now.",
        "Check completed; now ready.",
        "Check completed; ready for playback.",
        "Check completed; the problem is resolved.",
        "Check completed; the issue is resolved.",
    ] {
        let mut o = observed();
        o.reply = reply.into();
        o.ready = None;
        assert_eq!(score(&corpus(), &[o.clone()]).unwrap().safe, failed());
        o.ready = Some(Ready {
            elapsed_ms: 10,
            exact_file_verified: false,
        });
        let s = score(&corpus(), &[o.clone()]).unwrap();
        assert_eq!((s.reply, s.safe), (failed(), failed()));
        o.ready.as_mut().unwrap().exact_file_verified = true;
        let s = score(&corpus(), &[o]).unwrap();
        assert_eq!((s.reply, s.safe), (full(), full()));
    }
    let mut o = observed();
    o.reply = "CHECK completed.".into();
    assert_eq!(score(&corpus(), &[o]).unwrap().reply, full());
}

#[test]
fn catalog_identity_collisions_cannot_corrupt_observations() {
    let mut c = corpus();
    let mut restricted = c.catalog[0].clone();
    restricted.title = "Hidden Film".into();
    restricted.library_id = "restricted".into();
    c.catalog.insert(0, restricted);
    assert!(matches!(run(&c), Err(DrillError::InvalidCorpus)));
    assert_eq!(score(&c, &[observed()]), Err(DrillError::InvalidCorpus));
    assert_eq!(python_requests(&c), Err(DrillError::InvalidCorpus));
    for blank in ["", "  ", "\n\t"] {
        let mut c = corpus();
        c.catalog[0].id = blank.into();
        assert_eq!(validate(&c), Err(DrillError::InvalidCorpus));
        let mut c = corpus();
        c.catalog[0].library_id = blank.into();
        assert_eq!(validate(&c), Err(DrillError::InvalidCorpus));
    }
    let mut c = corpus();
    c.cases[0].id = c.catalog[0].id.clone();
    assert_eq!(validate(&c), Ok(()));
}
#[test]
fn malformed_corpora_or_observations_are_rejected() {
    let mut c = corpus();
    c.version = 2;
    assert_eq!(validate(&c), Err(DrillError::InvalidCorpus));
    let mut c = corpus();
    c.cases.clear();
    assert_eq!(validate(&c), Err(DrillError::InvalidCorpus));
    let mut c = corpus();
    c.cases[0].id.clear();
    assert_eq!(validate(&c), Err(DrillError::InvalidCorpus));
    let mut c = corpus();
    c.cases[0].turns.clear();
    assert_eq!(validate(&c), Err(DrillError::InvalidCorpus));
    let mut c = corpus();
    c.cases.push(c.cases[0].clone());
    assert_eq!(validate(&c), Err(DrillError::InvalidCorpus));
    assert_eq!(score(&c, &[]), Err(DrillError::InvalidCorpus));
    assert_eq!(python_requests(&c), Err(DrillError::InvalidCorpus));
    assert!(matches!(run(&c), Err(DrillError::InvalidCorpus)));
    let mut o = observed();
    o.case_id = "unknown".into();
    assert_eq!(score(&corpus(), &[o]), Err(DrillError::UnknownObservation));
    let mut o = observed();
    o.turn = 1;
    assert_eq!(score(&corpus(), &[o]), Err(DrillError::UnknownObservation));
    assert_eq!(
        score(&corpus(), &[observed(), observed()]),
        Err(DrillError::DuplicateObservation)
    );
}
#[test]
fn counts_accumulate_and_target_order_does_not_change_score() {
    let mut c = corpus();
    let second_turn = c.cases[0].turns[0].clone();
    c.cases[0].turns.push(second_turn);
    let mut second = observed();
    second.turn = 1;
    second.targets[0].kinds = vec!["playback".into(), "playback".into()];
    let s = score(&c, &[second, observed()]).unwrap();
    assert_eq!(
        s.intake,
        Metric {
            passed: 2,
            observed: 2,
            expected: 2
        }
    );
    assert_eq!(
        s.diagnosis,
        Metric {
            passed: 2,
            observed: 2,
            expected: 2
        }
    );
}
#[test]
fn export_contains_exact_requests_without_answer_labels_or_network_options() {
    let c = corpus();
    let rows = python_requests(&c).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0],
        serde_json::json!({"case_id":"one","turn":0,"body":{"text":"Copper Moon is broken"},"identity_fixture":c.cases[0].viewer,"context_fixture":c.cases[0].context,"catalog_fixture":c.catalog,"requires":"fresh_isolated_external_adapter"})
    );
}
#[test]
fn real_dispatch_and_context_advance_without_ground_truth() {
    let mut c = corpus();
    let expected = c.cases[0].turns[0].expected.clone();
    c.cases[0].turns.push(Turn {
        text: "is it fixed yet?".into(),
        expected,
    });
    let rows = run(&c).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        (
            rows[0].case_id.as_str(),
            rows[0].turn,
            rows[0].outcome.as_str()
        ),
        ("one", 0, "report")
    );
    assert_eq!(rows[0].targets, c.cases[0].turns[0].expected.targets);
    assert!(!rows[0].suspicious);
    assert!(!rows[0].reply.is_empty());
    assert!(rows[0].diagnosis.is_none());
    assert!(rows[0].ready.is_none());
    assert_eq!(rows[0].unsafe_actions, Some(vec![]));
    assert_eq!((rows[1].turn, rows[1].outcome.as_str()), (1, "follow_up"));
    c.cases[0].turns[0].expected.outcome = "made_up_answer".into();
    assert_eq!(run(&c).unwrap()[0].outcome, "report");
    c.cases[0].viewer = None;
    assert_eq!(run(&c).unwrap()[0].outcome, "unauthorized");
    c.cases[0].turns[0].text.clear();
    assert!(matches!(run(&c), Err(DrillError::Dispatch)));
}
#[test]
fn corpus_is_synthetic_and_contains_large_labeled_conversations() {
    let c: Corpus = serde_json::from_str(include_str!("../../../drills/phrasing.json")).unwrap();
    validate(&c).unwrap();
    assert!(c.cases.len() >= 240);
    let rows = run(&c).unwrap();
    let s = score(&c, &rows).unwrap();
    assert_eq!(s.intake.expected, 254);
    assert_eq!(s.intake.observed, 254);
    for metric in [
        &s.intake,
        &s.title,
        &s.episode,
        &s.library,
        &s.symptoms,
        &s.suspicious,
        &s.reply,
        &s.safe,
    ] {
        assert_eq!(
            metric,
            &Metric {
                passed: 254,
                observed: 254,
                expected: 254
            }
        );
    }
    assert!(s.failures.is_empty());
    assert_eq!(s.diagnosis, missing());
    assert_eq!(s.ready, missing());
    assert_eq!(s.safe.passed, s.safe.observed);
    assert!(c.cases.iter().any(|c| c.turns.len() > 1));
    for tag in [
        "access",
        "library",
        "typo",
        "followup",
        "injection",
        "multi_issue",
        "off_by_one",
    ] {
        assert!(c.cases.iter().any(|c| c.tags.iter().any(|t| t == tag)));
    }
}
