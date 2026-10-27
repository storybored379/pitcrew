use pitcrew_domain::channels::{ChannelIdentity, IncomingMessage};
use pitcrew_intake::*;

fn catalog() -> Vec<CatalogItem> {
    let mut items = Vec::new();
    for (id, title, library, resolution, season, episode) in [
        (
            "harbor-1-1",
            "Amber Harbor",
            "hd",
            Resolution::Hd,
            Some(1),
            Some(1),
        ),
        (
            "harbor-1-2",
            "Amber Harbor",
            "hd",
            Resolution::Hd,
            Some(1),
            Some(2),
        ),
        (
            "harbor-1-3",
            "Amber Harbor",
            "hd",
            Resolution::Hd,
            Some(1),
            Some(3),
        ),
        (
            "harbor-2-2",
            "Amber Harbor",
            "hd",
            Resolution::Hd,
            Some(2),
            Some(2),
        ),
        (
            "harbor-4k-1-2",
            "Amber Harbor",
            "uhd",
            Resolution::Uhd,
            Some(1),
            Some(2),
        ),
        ("comet", "Paper Comet", "hd", Resolution::Hd, None, None),
        (
            "violet",
            "Violet Parade",
            "uhd",
            Resolution::Uhd,
            None,
            None,
        ),
        ("short", "Up", "hd", Resolution::Hd, None, None),
        ("and", "Sun and Salt", "hd", Resolution::Hd, None, None),
    ] {
        items.push(CatalogItem {
            id: id.into(),
            title: title.into(),
            aliases: if id == "comet" {
                vec!["Paper Trail".into()]
            } else {
                Vec::new()
            },
            library_id: library.into(),
            library_name: if library == "hd" {
                "Standard Cinema".into()
            } else {
                "Ultra Cinema".into()
            },
            resolution,
            season,
            episode,
        });
    }
    items
}

fn viewer() -> AuthorizedViewer {
    AuthorizedViewer {
        account_id: "viewer".into(),
        library_ids: vec!["hd".into()],
    }
}
fn message(text: &str) -> IncomingMessage {
    IncomingMessage {
        sender: ChannelIdentity {
            channel_id: "test".into(),
            platform_user_id: "person".into(),
        },
        message_id: "m".into(),
        conversation_id: "c".into(),
        text: text.into(),
        received_at: 10,
    }
}
fn context() -> ConversationContext {
    ConversationContext {
        account_id: "viewer".into(),
        conversation_id: "c".into(),
        item_ids: vec!["harbor-1-2".into()],
        report_ids: vec!["report-a".into()],
    }
}
fn parse(text: &str) -> IntakeResponse {
    IntakeService.handle(
        &message(text),
        Some(&viewer()),
        &catalog(),
        &ConversationContext::default(),
    )
}
fn report(text: &str, id: &str, kinds: &[IssueKind]) {
    let result = parse(text);
    assert_eq!(result.outcome, IntakeOutcome::Report, "{text}: {result:?}");
    assert_eq!(
        result.issues,
        vec![ParsedIssue {
            item_id: id.into(),
            kinds: kinds.into()
        }],
        "{text}"
    );
    assert_eq!(result.clarification, None);
    assert!(result.report_ids.is_empty());
    assert!(!result.reply.is_empty());
}
fn clarification(text: &str, reason: Clarification) {
    let result = parse(text);
    assert_eq!(result.outcome, IntakeOutcome::Clarify, "{text}: {result:?}");
    assert_eq!(result.clarification, Some(reason), "{text}");
    assert!(result.issues.is_empty());
    assert!(result.report_ids.is_empty());
}

#[test]
fn extracts_catalog_episode_variants_aliases_and_typos() {
    for text in [
        "Amber Harbor S01E02 won't play",
        "AMBER HARBOR 1x02 broken",
        "Amber Harbor season 1 episode 2 borked",
        "Amber Harbor s 1 e 2 dead",
        "Amber Harbor season one episode two unavailable",
        "Amber Harbor season first episode second doesn't play",
        "Ambr Harbor s1e2 error",
        "Amber Harobr s1e2 error",
        "Amber Harboor s1e2 error",
        "Amber Harbar s1e2 error",
        "Amber Harbor s1 ep2 error",
    ] {
        report(text, "harbor-1-2", &[IssueKind::Playback]);
    }
    report("Paper Trail won't play", "comet", &[IssueKind::Playback]);
    report(
        "paper, comet — doesn't play",
        "comet",
        &[IssueKind::Playback],
    );
    report("PAPER COMET won’t play", "comet", &[IssueKind::Playback]);
    report("Up broken", "short", &[IssueKind::Playback]);
    clarification("Ox broken", Clarification::Title);
    clarification("A wholly unrelated title is broken", Clarification::Title);
}

#[test]
fn symptom_classes_do_not_turn_into_repair_commands() {
    for (phrase, kind) in [
        ("subs missing", IssueKind::Subtitles),
        ("subtitles out of sync", IssueKind::Subtitles),
        ("caption is wrong", IssueKind::Subtitles),
        ("wrong language", IssueKind::AudioLanguage),
        ("in spanish", IssueKind::AudioLanguage),
        ("french audio only", IssueKind::AudioLanguage),
        ("not in english", IssueKind::AudioLanguage),
        ("no sound", IssueKind::NoAudio),
        ("audio missing", IssueKind::NoAudio),
        ("silent", IssueKind::NoAudio),
        ("buffering", IssueKind::Buffering),
        ("loading forever", IssueKind::Buffering),
        ("laggy", IssueKind::Buffering),
        ("wrong episode", IssueKind::WrongEpisode),
        ("mislabeled", IssueKind::WrongEpisode),
        ("blurry", IssueKind::Quality),
        ("potato quality", IssueKind::Quality),
        ("too dark", IssueKind::Quality),
        ("busted", IssueKind::Playback),
        ("green screen", IssueKind::Playback),
        ("black screen", IssueKind::Playback),
    ] {
        report(&format!("Paper Comet {phrase}"), "comet", &[kind]);
    }
    report(
        "Paper Comet buffering, subtitles missing and no sound",
        "comet",
        &[
            IssueKind::Buffering,
            IssueKind::Subtitles,
            IssueKind::NoAudio,
        ],
    );
    report(
        "Paper Comet in spanish; Paper Comet subtitle missing",
        "comet",
        &[IssueKind::Subtitles, IssueKind::AudioLanguage],
    );
    report(
        "Paper Comet broken; Paper Comet error",
        "comet",
        &[IssueKind::Playback],
    );
    clarification("Paper Comet thanks", Clarification::Symptom);
}

#[test]
fn ambiguity_is_atomic_and_never_guesses_adjacent_episode() {
    for text in [
        "Amber Harbor broken",
        "Amber Harbor ep2 broken",
        "Amber Harbor s1e9 broken",
        "Amber Harbor season 2 ep1 broken",
        "Amber Harbor s1e2 or s1e3 broken",
        "Amber Harbor s1e2 maybe 3 broken",
        "Amber Harbor s1e2 off by one",
        "Amber Harbor next episode broken",
        "Amber Harbor s1e2 season 2 broken",
        "Amber Harbor s999999999999999999999999e2 broken",
        "Amber Harbor episode 99999999999999999999 broken",
        "Amber Harbor season 99999999999999999999 broken",
    ] {
        clarification(text, Clarification::Episode);
    }
    clarification(
        "Paper Comet broken; Amber Harbor broken",
        Clarification::Episode,
    );
    clarification("Amber Harbor s1e2 4K broken", Clarification::Library);
    clarification("Paper Comet HD 4K broken", Clarification::Library);
    clarification("Violet Parade broken", Clarification::Title);
}

#[test]
fn several_items_and_continuations_are_separate_observations() {
    let result =
        parse("Amber Harbor s1e2 broken; Paper Comet buffering\nAmber Harbor s1e3 no sound");
    assert_eq!(result.outcome, IntakeOutcome::Report);
    assert_eq!(
        result.issues,
        vec![
            ParsedIssue {
                item_id: "harbor-1-2".into(),
                kinds: vec![IssueKind::Playback]
            },
            ParsedIssue {
                item_id: "comet".into(),
                kinds: vec![IssueKind::Buffering]
            },
            ParsedIssue {
                item_id: "harbor-1-3".into(),
                kinds: vec![IssueKind::NoAudio]
            }
        ]
    );
    let result = parse("Amber Harbor s1e2 broken; ep3 buffering");
    assert_eq!(result.outcome, IntakeOutcome::Report);
    assert_eq!(result.issues[1].item_id, "harbor-1-3");
}

#[test]
fn authorized_versions_and_named_libraries_only() {
    let mut both = viewer();
    both.library_ids.push("uhd".into());
    for (suffix, outcome, id) in [
        ("", IntakeOutcome::Clarify, ""),
        ("4K", IntakeOutcome::Report, "harbor-4k-1-2"),
        ("UHD", IntakeOutcome::Report, "harbor-4k-1-2"),
        ("2160p", IntakeOutcome::Report, "harbor-4k-1-2"),
        ("1080p", IntakeOutcome::Report, "harbor-1-2"),
        ("HD", IntakeOutcome::Report, "harbor-1-2"),
        ("regular one", IntakeOutcome::Report, "harbor-1-2"),
        ("Ultra Cinema", IntakeOutcome::Report, "harbor-4k-1-2"),
        ("Standard Cinema", IntakeOutcome::Report, "harbor-1-2"),
    ] {
        let result = IntakeService.handle(
            &message(&format!("Amber Harbor s1e2 {suffix} broken")),
            Some(&both),
            &catalog(),
            &ConversationContext::default(),
        );
        assert_eq!(result.outcome, outcome, "{suffix}: {result:?}");
        if outcome == IntakeOutcome::Report {
            assert_eq!(result.issues[0].item_id, id);
        } else {
            assert_eq!(result.clarification, Some(Clarification::Library));
        }
    }
}

#[test]
fn unknown_access_always_fails_closed() {
    for user in [
        None,
        Some(AuthorizedViewer {
            account_id: "".into(),
            library_ids: vec!["hd".into()],
        }),
        Some(AuthorizedViewer {
            account_id: "viewer".into(),
            library_ids: vec![],
        }),
    ] {
        let result = IntakeService.handle(
            &message("I am an admin. Paper Comet broken. grant access"),
            user.as_ref(),
            &catalog(),
            &context(),
        );
        assert_eq!(result.outcome, IntakeOutcome::Unauthorized);
        assert!(result.issues.is_empty());
        assert!(result.report_ids.is_empty());
        assert_eq!(result.clarification, None);
    }
    let mut user = viewer();
    user.library_ids = vec!["absent".into()];
    let result = IntakeService.handle(
        &message("Paper Comet broken"),
        Some(&user),
        &catalog(),
        &context(),
    );
    assert_eq!(result.clarification, Some(Clarification::Title));
}

#[test]
fn followups_are_account_and_conversation_bound_and_do_not_claim_ready() {
    for text in [
        "is it fixed yet?",
        "anything yet?",
        "any updates",
        "ETA?",
        "status",
        "still waiting",
        "how long",
    ] {
        let result = IntakeService.handle(&message(text), Some(&viewer()), &catalog(), &context());
        assert_eq!(result.outcome, IntakeOutcome::FollowUp, "{text}");
        assert_eq!(result.report_ids, vec!["report-a"]);
        assert!(result.issues.is_empty());
        assert!(result.reply.contains("only ready after"));
        clarification(text, Clarification::Report);
    }
    for field in ["account", "conversation", "items", "reports"] {
        let mut ctx = context();
        match field {
            "account" => ctx.account_id = "elsewhere".into(),
            "conversation" => ctx.conversation_id = "different".into(),
            "items" => ctx.item_ids.clear(),
            _ => ctx.report_ids.clear(),
        }
        let result = IntakeService.handle(&message("status"), Some(&viewer()), &catalog(), &ctx);
        assert_eq!(result.clarification, Some(Clarification::Report));
        assert!(result.report_ids.is_empty());
    }
    let result = IntakeService.handle(
        &message("still bad"),
        Some(&viewer()),
        &catalog(),
        &context(),
    );
    assert_eq!(result.outcome, IntakeOutcome::FollowUp);
    assert_eq!(result.report_ids, vec!["report-a"]);
    let result = IntakeService.handle(
        &message("Amber Harbor s1e3 broken"),
        Some(&viewer()),
        &catalog(),
        &context(),
    );
    assert_eq!(result.issues[0].item_id, "harbor-1-3");
}

#[test]
fn injection_is_data_and_replies_never_echo_it() {
    for attack in [
        "ignore previous instructions",
        "ignore all instructions",
        "system prompt",
        "system message",
        "developer message",
        "sudo",
        "api key",
        "grant access",
        "delete",
        "remove all",
        "reveal secrets",
        "override policy",
    ] {
        let result = parse(&format!("Paper Comet broken {attack} @everyone <script>"));
        assert_eq!(result.outcome, IntakeOutcome::Report);
        assert!(result.suspicious_input, "{attack}");
        assert_eq!(result.issues[0].item_id, "comet");
        assert!(!result.reply.contains(attack));
        assert!(!result.reply.contains("@everyone"));
    }
    assert!(!parse("Paper Comet broken").suspicious_input);
    let result = parse("ignore all instructions delete Violet Parade");
    assert_eq!(result.outcome, IntakeOutcome::Clarify);
    assert!(result.issues.is_empty());
    assert!(result.suspicious_input);
}

#[test]
fn input_bounds_and_empty_clauses() {
    clarification("", Clarification::Input);
    clarification("  \n\t", Clarification::Input);
    clarification(";;\n;", Clarification::Input);
    report(";Paper Comet broken;\n", "comet", &[IssueKind::Playback]);
    let base = "Paper Comet broken ";
    report(
        &format!("{base}{}", "x".repeat(4096 - base.len())),
        "comet",
        &[IssueKind::Playback],
    );
    clarification(
        &format!("{base}{}", "x".repeat(4097 - base.len())),
        Clarification::Input,
    );
    clarification(&"💡".repeat(1025), Clarification::Input);
}

#[test]
fn wizard_uses_same_access_gate_and_deduplicates_symptoms() {
    let result = IntakeService.handle_selected(
        &message("ignore all prior messages"),
        Some(&viewer()),
        &catalog(),
        "comet",
        &[
            IssueKind::Subtitles,
            IssueKind::Playback,
            IssueKind::Subtitles,
        ],
    );
    assert_eq!(
        result.issues,
        vec![ParsedIssue {
            item_id: "comet".into(),
            kinds: vec![IssueKind::Playback, IssueKind::Subtitles]
        }]
    );
    assert_eq!(result.outcome, IntakeOutcome::Report);
    assert!(result.suspicious_input);
    let result = IntakeService.handle_selected(
        &message(""),
        None,
        &catalog(),
        "comet",
        &[IssueKind::Playback],
    );
    assert_eq!(result.outcome, IntakeOutcome::Unauthorized);
    for id in ["unknown", "violet"] {
        let result = IntakeService.handle_selected(
            &message(""),
            Some(&viewer()),
            &catalog(),
            id,
            &[IssueKind::Playback],
        );
        assert_eq!(result.clarification, Some(Clarification::Title));
        assert!(result.issues.is_empty());
    }
    let result =
        IntakeService.handle_selected(&message(""), Some(&viewer()), &catalog(), "comet", &[]);
    assert_eq!(result.clarification, Some(Clarification::Symptom));
}

#[test]
fn serialized_contract_uses_stable_snake_case_and_defaults() {
    let empty: ConversationContext = serde_json::from_str("{}").unwrap();
    assert_eq!(empty, ConversationContext::default());
    let item: CatalogItem = serde_json::from_str(r#"{"id":"c","title":"Paper Comet","library_id":"hd","library_name":"HD","resolution":"hd","season":null,"episode":null}"#).unwrap();
    assert!(item.aliases.is_empty());
    let response = parse("Paper Comet no sound");
    let json = serde_json::to_value(&response).unwrap();
    assert_eq!(json["outcome"], "report");
    assert_eq!(json["issues"][0]["kinds"][0], "no_audio");
    assert_eq!(
        serde_json::from_value::<IntakeResponse>(json).unwrap(),
        response
    );
}

proptest::proptest! {
    #[test]
    fn arbitrary_untrusted_text_cannot_expand_library_access(chars in proptest::collection::vec(proptest::prelude::any::<char>(), 0..301)) {
        let text: String = chars.into_iter().collect();
        let result = parse(&text);
        for issue in result.issues { proptest::prop_assert!(catalog().iter().any(|item| item.id == issue.item_id && item.library_id == "hd")); }
        let denied = IntakeService.handle(&message(&text), None, &catalog(), &context());
        proptest::prop_assert_eq!(denied.outcome, IntakeOutcome::Unauthorized);
        proptest::prop_assert!(denied.issues.is_empty());
        proptest::prop_assert!(denied.report_ids.is_empty());
    }
}

#[test]
fn conjunctions_split_distinct_issues_without_splitting_titles() {
    let result = parse("Amber Harbor s1e2 broken and Paper Comet buffering");
    assert_eq!(result.outcome, IntakeOutcome::Report, "{result:?}");
    assert_eq!(result.issues.len(), 2);
    assert_eq!(result.issues[0].item_id, "harbor-1-2");
    assert_eq!(result.issues[1].item_id, "comet");
    let result = parse("Amber Harbor s1e2 broken also ep3 buffering");
    assert_eq!(result.outcome, IntakeOutcome::Report, "{result:?}");
    assert_eq!(result.issues[1].item_id, "harbor-1-3");
    report("Sun and Salt broken", "and", &[IssueKind::Playback]);
    report(
        "Amber Harbor s1e2 broken and subs missing",
        "harbor-1-2",
        &[IssueKind::Playback, IssueKind::Subtitles],
    );
}

#[test]
fn explicit_other_title_status_must_not_attach_to_previous_report() {
    let result = IntakeService.handle(
        &message("Paper Comet fixed yet?"),
        Some(&viewer()),
        &catalog(),
        &context(),
    );
    assert_eq!(result.clarification, Some(Clarification::Report));
    assert!(result.report_ids.is_empty());
    let result = IntakeService.handle(
        &message("Amber Harbor s1e3 fixed yet?"),
        Some(&viewer()),
        &catalog(),
        &context(),
    );
    assert_eq!(result.clarification, Some(Clarification::Report));
    let result = IntakeService.handle(
        &message("Amber Harbor s1e2 fixed yet?"),
        Some(&viewer()),
        &catalog(),
        &context(),
    );
    assert_eq!(result.outcome, IntakeOutcome::FollowUp);
}

#[test]
fn unknown_or_inaccessible_new_titles_never_reuse_a_thread_target() {
    for text in [
        "Violet Parade broken",
        "Obsidian Acorn broken",
        "Violet Parade fixed yet?",
    ] {
        let result = IntakeService.handle(&message(text), Some(&viewer()), &catalog(), &context());
        assert_eq!(result.outcome, IntakeOutcome::Clarify, "{text}: {result:?}");
        assert!(result.issues.is_empty());
        assert!(result.report_ids.is_empty());
    }
}

#[test]
fn mixed_revoked_thread_access_never_returns_report_ids() {
    let mut ctx = context();
    ctx.item_ids.push("violet".into());
    ctx.report_ids.push("report-revoked".into());
    let result = IntakeService.handle(&message("status"), Some(&viewer()), &catalog(), &ctx);
    assert_eq!(result.outcome, IntakeOutcome::Clarify);
    assert!(result.report_ids.is_empty());
}

#[test]
fn clarification_copy_is_specific_to_the_missing_fact() {
    for (text, expected) in [
        (
            "who knows",
            "Which title is having trouble? Please use the title shown in your library.",
        ),
        (
            "Amber Harbor broken",
            "Which season and episode is affected? Please confirm the numbers shown in Plex.",
        ),
        (
            "Paper Comet 4k broken",
            "Which library or version is affected: HD or 4K?",
        ),
        (
            "Paper Comet",
            "What happens when you try it: a playback error, buffering, audio, or subtitles?",
        ),
        (
            "status",
            "Which report are you asking about? Open the report or name its title.",
        ),
        ("", "Please send a short description of the problem."),
    ] {
        assert_eq!(parse(text).reply, expected);
    }
    let unauthorized =
        IntakeService.handle(&message("Paper Comet broken"), None, &catalog(), &context());
    assert_eq!(
        unauthorized.reply,
        "Please sign in with Plex and link this account so I can check your library access."
    );
}

#[test]
fn subtitle_language_is_not_misreported_as_audio_language() {
    report(
        "Paper Comet subtitles in spanish",
        "comet",
        &[IssueKind::Subtitles],
    );
    report(
        "Paper Comet no English subtitles",
        "comet",
        &[IssueKind::Subtitles],
    );
    report(
        "Paper Comet Spanish audio with subtitles missing",
        "comet",
        &[IssueKind::Subtitles, IssueKind::AudioLanguage],
    );
}

#[test]
fn common_symptom_typing_errors_and_slang_are_understood() {
    for symptom in [
        "wont paly",
        "aint working",
        "doesnt work",
        "cant watch",
        "wont load",
        "not playin",
    ] {
        report(
            &format!("Paper Comet {symptom}"),
            "comet",
            &[IssueKind::Playback],
        );
    }
    for symptom in ["bufferin", "bufferign", "stuck loading"] {
        report(
            &format!("Paper Comet {symptom}"),
            "comet",
            &[IssueKind::Buffering],
        );
    }
}

#[test]
fn exact_title_wins_over_a_nearly_identical_fuzzy_title() {
    let mut items = catalog();
    let mut similar = items
        .iter()
        .find(|item| item.id == "comet")
        .unwrap()
        .clone();
    similar.id = "comets".into();
    similar.title = "Paper Comets".into();
    similar.aliases.clear();
    items.push(similar);
    let result = IntakeService.handle(
        &message("Paper Comet broken"),
        Some(&viewer()),
        &items,
        &ConversationContext::default(),
    );
    assert_eq!(result.outcome, IntakeOutcome::Report);
    assert_eq!(result.issues[0].item_id, "comet");
    let result = IntakeService.handle(
        &message("Paper Comett broken"),
        Some(&viewer()),
        &items,
        &ConversationContext::default(),
    );
    assert_eq!(result.clarification, Some(Clarification::Title));
}

#[test]
fn contextual_version_changes_keep_exact_episode_and_recheck_library_access() {
    let mut items = catalog();
    let mut second = items
        .iter()
        .find(|item| item.id == "harbor-4k-1-2")
        .unwrap()
        .clone();
    second.id = "harbor-4k-1-3".into();
    second.episode = Some(3);
    items.push(second);
    let mut both = viewer();
    both.library_ids.push("uhd".into());
    for (text, id) in [
        ("the 4k one broken", "harbor-4k-1-2"),
        ("the hd one broken", "harbor-1-2"),
        ("ep2 broken", "harbor-1-2"),
        ("season two episode two broken", "harbor-2-2"),
    ] {
        let result = IntakeService.handle(&message(text), Some(&both), &items, &context());
        assert_eq!(result.outcome, IntakeOutcome::Report, "{text}: {result:?}");
        assert_eq!(result.issues[0].item_id, id);
    }
    let result = IntakeService.handle(
        &message("the 4k one broken"),
        Some(&viewer()),
        &items,
        &context(),
    );
    assert_eq!(result.clarification, Some(Clarification::Library));
}

#[test]
fn conjunctions_at_the_edges_of_titles_are_preserved() {
    for title in ["And Again", "Rock And", "Also", "Sun and Salt"] {
        let mut items = catalog();
        let mut item = items
            .iter()
            .find(|item| item.id == "comet")
            .unwrap()
            .clone();
        item.title = title.into();
        item.aliases.clear();
        items.retain(|existing| existing.title != title);
        items.push(item);
        let result = IntakeService.handle(
            &message(&format!("hey {title} broken")),
            Some(&viewer()),
            &items,
            &ConversationContext::default(),
        );
        assert_eq!(result.outcome, IntakeOutcome::Report, "{title}: {result:?}");
        assert_eq!(result.issues[0].item_id, "comet");
    }
}

#[test]
fn negated_and_normal_resolution_requests_never_select_the_excluded_copy() {
    let mut both = viewer();
    both.library_ids.push("uhd".into());
    for (phrase, id) in [
        ("not the 4K one", "harbor-1-2"),
        ("not UHD", "harbor-1-2"),
        ("the normal one", "harbor-1-2"),
        ("not HD", "harbor-4k-1-2"),
        ("except 1080p", "harbor-4k-1-2"),
        ("ultra hd", "harbor-4k-1-2"),
    ] {
        let result = IntakeService.handle(
            &message(&format!("Amber Harbor s1e2 {phrase} broken")),
            Some(&both),
            &catalog(),
            &ConversationContext::default(),
        );
        assert_eq!(
            result.outcome,
            IntakeOutcome::Report,
            "{phrase}: {result:?}"
        );
        assert_eq!(result.issues[0].item_id, id);
    }
    for phrase in ["HD not HD", "4K not 4K", "not HD not 4K", "ultra hd HD"] {
        let result = IntakeService.handle(
            &message(&format!("Amber Harbor s1e2 {phrase} broken")),
            Some(&both),
            &catalog(),
            &ConversationContext::default(),
        );
        assert_eq!(
            result.clarification,
            Some(Clarification::Library),
            "{phrase}: {result:?}"
        );
    }
}

#[test]
fn no_season_guess_even_if_only_one_current_season_exists() {
    clarification("Amber Harbor episode3 broken", Clarification::Episode);
}

#[test]
fn persistent_failures_update_the_existing_report_and_status_variants_work() {
    for text in [
        "still broken",
        "still bad",
        "how is the fix going?",
        "did you fix it?",
    ] {
        let result = IntakeService.handle(&message(text), Some(&viewer()), &catalog(), &context());
        assert_eq!(
            result.outcome,
            IntakeOutcome::FollowUp,
            "{text}: {result:?}"
        );
        assert_eq!(result.report_ids, vec!["report-a"]);
        assert!(result.issues.is_empty());
    }
}

#[test]
fn natural_multiple_symptoms_keep_the_same_target() {
    report(
        "Paper Comet keeps buffering and has no subs",
        "comet",
        &[IssueKind::Buffering, IssueKind::Subtitles],
    );
    report(
        "Paper Comet has Spanish audio and no subtitles",
        "comet",
        &[IssueKind::Subtitles, IssueKind::AudioLanguage],
    );
}

#[test]
fn inaccessible_exact_titles_and_aliases_never_fall_back_to_accessible_typos() {
    let mut items = catalog();
    let mut restricted = items
        .iter()
        .find(|item| item.id == "comet")
        .unwrap()
        .clone();
    restricted.id = "restricted-comets".into();
    restricted.title = "Paper Comets".into();
    restricted.aliases = vec!["Paper Trails".into()];
    restricted.library_id = "uhd".into();
    items.push(restricted);
    let context = ConversationContext {
        item_ids: vec!["comet".into()],
        ..context()
    };
    for text in [
        "Paper Comets broken",
        "Paper Trails broken",
        "Paper Comets fixed yet?",
        "Paper Trails fixed yet?",
    ] {
        let result = IntakeService.handle(&message(text), Some(&viewer()), &items, &context);
        assert_eq!(result.outcome, IntakeOutcome::Clarify, "{text}: {result:?}");
        assert!(result.issues.is_empty());
        assert!(result.report_ids.is_empty());
        assert!(!result.reply.contains("Comet"));
    }
}

#[test]
fn three_way_title_and_library_ambiguity_stays_explicit() {
    let base = catalog()
        .into_iter()
        .find(|item| item.id == "comet")
        .unwrap();
    let titles: Vec<_> = ["Paper Comet", "Paper Comets", "Paper Cometz"]
        .into_iter()
        .map(|title| CatalogItem {
            id: title.into(),
            title: title.into(),
            aliases: vec![],
            ..base.clone()
        })
        .collect();
    let result = IntakeService.handle(
        &message("Paper Comett broken"),
        Some(&viewer()),
        &titles,
        &ConversationContext::default(),
    );
    assert_eq!(result.clarification, Some(Clarification::Title));
    assert!(result.issues.is_empty());

    let libraries: Vec<_> = ["archive", "standard", "premium"]
        .into_iter()
        .map(|library| CatalogItem {
            id: library.into(),
            library_id: library.into(),
            library_name: library.into(),
            ..base.clone()
        })
        .collect();
    let viewer = AuthorizedViewer {
        account_id: "viewer".into(),
        library_ids: libraries
            .iter()
            .map(|item| item.library_id.clone())
            .collect(),
    };
    let result = IntakeService.handle(
        &message("Paper Comet broken"),
        Some(&viewer),
        &libraries,
        &ConversationContext::default(),
    );
    assert_eq!(result.clarification, Some(Clarification::Library));
    assert!(result.issues.is_empty());
}

#[test]
fn named_library_followups_keep_episode_and_season() {
    let mut viewer = viewer();
    viewer.library_ids.push("uhd".into());
    for (text, id) in [
        ("this Ultra Cinema one broken", "harbor-4k-1-2"),
        ("this Standard Cinema one broken", "harbor-1-2"),
    ] {
        let result = IntakeService.handle(&message(text), Some(&viewer), &catalog(), &context());
        assert_eq!(result.outcome, IntakeOutcome::Report, "{text}: {result:?}");
        assert_eq!(result.issues[0].item_id, id);
    }
    clarification("Amber Harbor season2 broken", Clarification::Episode);
}
