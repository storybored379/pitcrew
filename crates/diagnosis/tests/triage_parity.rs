use pitcrew_diagnosis::triage::{TriageAction as A, ValidationError as E, validate_path};

#[test]
fn every_reference_wizard_path_has_the_same_terminal() {
    let cases: Vec<(&str, Vec<&str>, A)> = vec![
        ("video-broken", vec![], A::Autofix),
        ("audio", vec!["no-sound"], A::Autofix),
        ("audio", vec!["wrong-language", "english"], A::Langcheck),
        ("audio", vec!["wrong-language", "spanish"], A::Langcheck),
        ("audio", vec!["wrong-language", "other-language"], A::Notify),
        ("wrong-episode", vec!["different-episode"], A::Autofix),
        ("wrong-episode", vec!["something-else"], A::Autofix),
        ("wrong-episode", vec!["not-sure"], A::Autofix),
        ("wont-play", vec!["everything-broken"], A::NotifyUrgent),
        ("wont-play", vec!["just-this-one"], A::Autofix),
        ("poor-quality", vec!["comes-and-goes"], A::Playcheck),
        ("poor-quality", vec!["whole-time"], A::Playcheck),
        ("subtitles", vec!["english"], A::Notify),
        ("subtitles", vec!["spanish"], A::Notify),
        ("subtitles", vec!["other-language"], A::Notify),
        ("buffering", vec!["helped"], A::Closed),
        ("buffering", vec!["still-bad"], A::Playcheck),
        ("ads", vec![], A::Wherefrom),
        ("other", vec![], A::Notify),
    ];
    for (issue, answers, action) in cases {
        assert_eq!(
            validate_path(issue, "show", "episode", &answers, false),
            Ok(action),
            "{issue}/{answers:?}"
        );
        assert_eq!(
            validate_path(issue, "show", "show", &answers, false),
            Ok(if action == A::Autofix {
                A::Notify
            } else {
                action
            })
        );
        assert_eq!(
            validate_path(issue, "movie", "movie", &answers, false),
            if issue == "wrong-episode" {
                Err(E::TelevisionOnly)
            } else {
                Ok(action)
            }
        );
        let mut extra = answers.clone();
        extra.push("unauthorized-action");
        assert_eq!(
            validate_path(issue, "show", "episode", &extra, false),
            Err(E::InvalidAnswers)
        );
        if !answers.is_empty() {
            assert_eq!(
                validate_path(
                    issue,
                    "show",
                    "episode",
                    &answers[..answers.len() - 1],
                    false
                ),
                Err(E::InvalidAnswers)
            );
        }
        assert_eq!(
            validate_path(issue, "show", "episode", &["bogus"], false),
            Err(E::InvalidAnswers)
        );
        assert_eq!(
            validate_path(issue, "show", "episode", &answers, true),
            if issue == "other" {
                Ok(A::Notify)
            } else {
                Err(E::UnexpectedFreeText)
            }
        );
    }
}

#[test]
fn unrecognized_media_scope_and_issue_are_rejected() {
    for (media, scope) in [
        ("movie", "episode"),
        ("movie", "show"),
        ("show", "movie"),
        ("album", "movie"),
        ("movie", "unknown"),
        ("show", "unknown"),
    ] {
        assert_eq!(
            validate_path("video-broken", media, scope, &[], false),
            Err(E::InvalidScope)
        );
    }
    assert_eq!(
        validate_path("unknown", "movie", "movie", &[], false),
        Err(E::UnknownIssue)
    );
    assert_eq!(
        validate_path(
            "audio",
            "movie",
            "movie",
            &["wrong-language", "klingon"],
            false
        ),
        Err(E::InvalidAnswers)
    );
}
