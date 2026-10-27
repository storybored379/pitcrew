use crate::*;
use regex::Regex;
use std::collections::BTreeSet;
use std::sync::LazyLock;

static COORDINATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:s(?:eason)?\s*(\d+)\s*e(?:p(?:isode)?)?\s*(\d+)|(\d+)\s*x\s*(\d+))\b")
        .unwrap()
});
static REFERENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:it|this|that|same|still|is it|any update|any updates|anything yet|fixed yet|status|eta|how long|how is the fix going|did you fix it|(?:has )?(?:no|missing) (?:subs?|subtitles?|captions?)|the (?:4k|hd|uhd|regular|normal) (?:one|1)|subs?|subtitles?|captions?|no audio|no sound|wrong language|wrong episode|not playing|not working|broken|busted|borked|dead|buffering|laggy|silent)\b|^(?:s\d+e\d+|\d+x\d+|(?:episode|ep|e|season|s)\s*\d+)\b").unwrap()
});
static NEGATED_VERSION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:not|except)(?: the)? (4k|uhd|2160p|2160|hd|1080p|1080)\b").unwrap()
});
static CONJUNCTION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b(?:and|also)\b").unwrap());
static SEASON: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b(?:season|s)\s*(\d+)\b").unwrap());
static EPISODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:episode|ep|e)\s*(\d+)\b").unwrap());

fn normalize(text: &str) -> String {
    text.replace(['\'', '’'], "")
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn contains(text: &str, phrase: &str) -> bool {
    format!(" {text} ").contains(&format!(" {phrase} "))
}

fn any(text: &str, phrases: &[&str]) -> bool {
    phrases.iter().any(|phrase| contains(text, phrase))
}

fn suspicious(text: &str) -> bool {
    any(
        text,
        &[
            "ignore previous",
            "ignore your rules",
            "grant admin access",
            "forget all prior",
            "bypass library permissions",
            "run curl",
            "ignore all",
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
        ],
    )
}

fn response(
    outcome: IntakeOutcome,
    clarification: Option<Clarification>,
    suspicious_input: bool,
) -> IntakeResponse {
    let reply = match (outcome, clarification) {
        (IntakeOutcome::Unauthorized, _) => {
            "Please sign in with Plex and link this account so I can check your library access."
        }
        (_, Some(Clarification::Title)) => {
            "Which title is having trouble? Please use the title shown in your library."
        }
        (_, Some(Clarification::Episode)) => {
            "Which season and episode is affected? Please confirm the numbers shown in Plex."
        }
        (_, Some(Clarification::Library)) => "Which library or version is affected: HD or 4K?",
        (_, Some(Clarification::Symptom)) => {
            "What happens when you try it: a playback error, buffering, audio, or subtitles?"
        }
        (_, Some(Clarification::Report)) => {
            "Which report are you asking about? Open the report or name its title."
        }
        (_, Some(Clarification::Input)) => "Please send a short description of the problem.",
        (IntakeOutcome::FollowUp, _) => {
            "I can check the existing report. A fix is only ready after the imported file passes verification."
        }
        _ => {
            "I understand the reported issue. The exact item still needs diagnosis before a repair can be considered."
        }
    };
    IntakeResponse {
        outcome,
        issues: Vec::new(),
        clarification,
        report_ids: Vec::new(),
        reply: reply.into(),
        suspicious_input,
    }
}

fn clarify(reason: Clarification, suspicious_input: bool) -> IntakeResponse {
    response(IntakeOutcome::Clarify, Some(reason), suspicious_input)
}

fn authorized<'a, 'v>(
    viewer: Option<&'v AuthorizedViewer>,
    catalog: &'a [CatalogItem],
) -> Option<(&'v AuthorizedViewer, Vec<&'a CatalogItem>)> {
    let viewer =
        viewer.filter(|viewer| !viewer.account_id.is_empty() && !viewer.library_ids.is_empty())?;
    let items = catalog
        .iter()
        .filter(|item| viewer.library_ids.contains(&item.library_id))
        .collect();
    Some((viewer, items))
}

fn kinds(text: &str) -> Vec<IssueKind> {
    let rules: &[(IssueKind, &[&str])] = &[
        (
            IssueKind::Subtitles,
            &["subs", "subtitles", "subtitle", "captions", "caption"],
        ),
        (
            IssueKind::AudioLanguage,
            &[
                "wrong language",
                "not in english",
                "no english",
                "dubbed",
                "dub",
                "spanish audio",
                "french audio",
                "german audio",
                "japanese audio",
                "in spanish",
                "in french",
                "in german",
                "in japanese",
                "audio language",
            ],
        ),
        (
            IssueKind::NoAudio,
            &[
                "no audio",
                "no sound",
                "silent",
                "cant hear",
                "cannot hear",
                "audio missing",
                "sound missing",
            ],
        ),
        (
            IssueKind::Buffering,
            &[
                "buffering",
                "keeps freezing",
                "bufferin",
                "bufferign",
                "stuck loading",
                "buffers",
                "buffer",
                "laggy",
                "lagging",
                "stuttering",
                "stutters",
                "keeps pausing",
                "loading forever",
                "spinning",
                "spinner",
            ],
        ),
        (
            IssueKind::WrongEpisode,
            &[
                "wrong episode",
                "episodes out of order",
                "different episode",
                "off by one",
                "mislabeled",
                "mislabelled",
                "wrong ep",
            ],
        ),
        (
            IssueKind::Quality,
            &[
                "blurry",
                "pixelated",
                "low quality",
                "poor picture quality",
                "bad quality",
                "potato quality",
                "washed out",
                "too dark",
            ],
        ),
        (
            IssueKind::Playback,
            &[
                "wont play",
                "wont start",
                "wont paly",
                "aint working",
                "doesnt work",
                "cant watch",
                "wont load",
                "not playin",
                "will not play",
                "cant play",
                "cannot play",
                "doesnt play",
                "not playing",
                "not working",
                "broken",
                "busted",
                "borked",
                "dead",
                "playback error",
                "error",
                "crashes",
                "crashing",
                "black screen",
                "green screen",
                "unavailable",
                "not available",
                "still bad",
            ],
        ),
    ];
    let subtitles = any(
        text,
        &["subs", "subtitles", "subtitle", "captions", "caption"],
    );
    let audio = any(text, &["audio", "dub", "dubbed"]);
    rules
        .iter()
        .filter(|(kind, phrases)| {
            any(text, phrases) && (*kind != IssueKind::AudioLanguage || !subtitles || audio)
        })
        .map(|(kind, _)| *kind)
        .collect()
}

// One edit or one adjacent transposition; never fuzzy-match very short names.
fn close_word(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let a: Vec<_> = a.chars().collect();
    let b: Vec<_> = b.chars().collect();
    if a.len().min(b.len()) < 4 {
        return false;
    }
    let mut previous: Vec<_> = (0..=b.len()).collect();
    for (i, ac) in a.iter().enumerate() {
        let mut row = vec![i + 1];
        for (j, bc) in b.iter().enumerate() {
            row.push(
                (previous[j + 1] + 1)
                    .min(row[j] + 1)
                    .min(previous[j] + usize::from(ac != bc)),
            );
        }
        previous = row;
    }
    if previous[b.len()] <= 1 {
        return true;
    }
    let differences: Vec<_> = a
        .iter()
        .zip(&b)
        .enumerate()
        .filter(|(_, (x, y))| x != y)
        .map(|(i, _)| i)
        .collect();
    match differences.as_slice() {
        [i, j] => a.len() == b.len() && *j == i + 1 && a[*i] == b[*j] && a[*j] == b[*i],
        _ => false,
    }
}

fn title_match(text: &str, title: &str) -> bool {
    let title = normalize(title);
    let wanted: Vec<_> = title.split_whitespace().collect();
    if wanted.is_empty() {
        return false;
    }
    let words: Vec<_> = text.split_whitespace().collect();
    words
        .windows(wanted.len())
        .any(|window| window.iter().zip(&wanted).all(|(a, b)| close_word(a, b)))
}

fn matches(item: &CatalogItem, text: &str, exact: bool) -> bool {
    std::iter::once(&item.title)
        .chain(&item.aliases)
        .any(|title| {
            if exact {
                !title.is_empty() && contains(text, &normalize(title))
            } else {
                title_match(text, title)
            }
        })
}

fn number_words(text: &str) -> String {
    text.split_whitespace()
        .map(|word| match word {
            "zero" => "0",
            "one" | "first" => "1",
            "two" | "second" => "2",
            "three" | "third" => "3",
            "four" | "fourth" => "4",
            "five" | "fifth" => "5",
            "six" | "sixth" => "6",
            "seven" | "seventh" => "7",
            "eight" | "eighth" => "8",
            "nine" | "ninth" => "9",
            "ten" | "tenth" => "10",
            _ => word,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn coordinates(text: &str) -> Result<(Option<u32>, Option<u32>), Clarification> {
    let text = number_words(text);
    let mut seasons = BTreeSet::new();
    let mut episodes = BTreeSet::new();
    for capture in COORDINATE.captures_iter(&text) {
        let (s, e) = if capture.get(1).is_some() {
            (&capture[1], &capture[2])
        } else {
            (&capture[3], &capture[4])
        };
        seasons.insert(s.parse().map_err(|_| Clarification::Episode)?);
        episodes.insert(e.parse().map_err(|_| Clarification::Episode)?);
    }
    for capture in SEASON.captures_iter(&text) {
        seasons.insert(capture[1].parse().map_err(|_| Clarification::Episode)?);
    }
    for capture in EPISODE.captures_iter(&text) {
        episodes.insert(capture[1].parse().map_err(|_| Clarification::Episode)?);
    }
    if seasons.len() > 1
        || episodes.len() > 1
        || any(
            &text,
            &[
                "maybe",
                "off by 1",
                "next episode",
                "previous episode",
                "last episode",
                "or",
            ],
        )
    {
        return Err(Clarification::Episode);
    }
    Ok((seasons.first().copied(), episodes.first().copied()))
}

fn resolve<'a>(
    text: &str,
    items: &[&'a CatalogItem],
    inherited: &[&'a CatalogItem],
    catalog: &'a [CatalogItem],
) -> Result<&'a CatalogItem, Clarification> {
    let exact: Vec<_> = catalog
        .iter()
        .filter(|item| matches(item, text, true))
        .collect();
    let mut found: Vec<_> = if exact.is_empty() {
        items
            .iter()
            .copied()
            .filter(|item| matches(item, text, false))
            .collect()
    } else {
        let authorized: Vec<_> = exact
            .into_iter()
            .filter(|item| items.contains(item))
            .collect();
        if authorized.is_empty() {
            return Err(Clarification::Title);
        }
        authorized
    };
    if found.is_empty() {
        if !REFERENCE.is_match(&number_words(text)) {
            return Err(Clarification::Title);
        }
        let (season, episode) = coordinates(text)?;
        let version = any(
            text,
            &[
                "hd",
                "1080p",
                "4k",
                "uhd",
                "2160p",
                "normal one",
                "regular one",
            ],
        ) || items.iter().any(|item| {
            !item.library_name.is_empty() && contains(text, &normalize(&item.library_name))
        });
        let explicit = season.is_some() || episode.is_some() || version;
        found = if explicit {
            items
                .iter()
                .copied()
                .filter(|item| {
                    inherited.iter().any(|prior| {
                        prior.title == item.title
                            && (version || item.library_id == prior.library_id)
                            && (season.is_some() || item.season == prior.season)
                            && (season.is_some()
                                || episode.is_some()
                                || item.episode == prior.episode)
                    })
                })
                .collect()
        } else {
            inherited.to_vec()
        };
    }
    if found.is_empty() {
        return Err(Clarification::Title);
    }
    let titles: BTreeSet<_> = found.iter().map(|item| &item.title).collect();
    if titles.len() > 1 {
        return Err(Clarification::Title);
    }
    let (season, episode) = coordinates(text)?;
    if found.iter().any(|item| item.episode.is_some())
        && (season.is_none() || episode.is_none())
        && !inherited
            .iter()
            .any(|prior| found.iter().any(|item| prior.title == item.title))
    {
        return Err(Clarification::Episode);
    }
    found.retain(|item| {
        season.is_none_or(|s| item.season == Some(s))
            && episode.is_none_or(|e| item.episode == Some(e))
    });
    if found.is_empty() {
        return Err(Clarification::Episode);
    }
    let version_text = text.replace("ultra hd", "uhd");
    let excluded_hd = NEGATED_VERSION
        .captures_iter(&version_text)
        .any(|c| any(&c[1], &["hd", "1080p", "1080"]));
    let excluded_uhd = NEGATED_VERSION
        .captures_iter(&version_text)
        .any(|c| any(&c[1], &["4k", "uhd", "2160p", "2160"]));
    let positive = NEGATED_VERSION.replace_all(&version_text, "");
    let hd = any(
        &positive,
        &[
            "hd",
            "1080p",
            "1080",
            "regular version",
            "regular one",
            "normal one",
        ],
    );
    let uhd = any(&positive, &["4k", "uhd", "2160p", "2160"]);
    if hd && uhd {
        return Err(Clarification::Library);
    }
    let named: Vec<_> = found
        .iter()
        .copied()
        .filter(|item| {
            !item.library_name.is_empty() && contains(text, &normalize(&item.library_name))
        })
        .collect();
    if !named.is_empty() {
        found = named;
    }
    found.retain(|item| {
        (!hd || item.resolution == Resolution::Hd)
            && (!uhd || item.resolution == Resolution::Uhd)
            && (!excluded_hd || item.resolution == Resolution::Uhd)
            && (!excluded_uhd || item.resolution == Resolution::Hd)
    });
    if found.is_empty() {
        return Err(Clarification::Library);
    }
    let libraries: BTreeSet<_> = found.iter().map(|item| &item.library_id).collect();
    if libraries.len() > 1 {
        return Err(Clarification::Library);
    }
    if found.len() != 1 {
        return Err(Clarification::Episode);
    }
    Ok(found[0])
}

fn clauses<'a>(text: &'a str, items: &[&CatalogItem]) -> Vec<&'a str> {
    let mut clauses = Vec::new();
    let mut start = 0;
    for conjunction in CONJUNCTION.find_iter(text) {
        let inside_title = items.iter().any(|item| {
            std::iter::once(&item.title)
                .chain(&item.aliases)
                .any(|title| {
                    let title = normalize(title);
                    text.match_indices(&title).any(|(at, found)| {
                        at <= conjunction.start() && conjunction.end() <= at + found.len()
                    })
                })
        });
        if !inside_title {
            clauses.push(text[start..conjunction.start()].trim());
            start = conjunction.end();
        }
    }
    clauses.push(text[start..].trim());
    clauses
}

pub(super) fn handle(
    message: &IncomingMessage,
    viewer: Option<&AuthorizedViewer>,
    catalog: &[CatalogItem],
    context: &ConversationContext,
) -> IntakeResponse {
    let Some((viewer, items)) = authorized(viewer, catalog) else {
        return response(IntakeOutcome::Unauthorized, None, false);
    };
    if message.text.len() > 4096 || message.text.trim().is_empty() {
        return clarify(Clarification::Input, false);
    }
    let text = normalize(&message.text);
    let suspicious_input = suspicious(&text);
    let valid_context = context.account_id == viewer.account_id
        && context.conversation_id == message.conversation_id
        && context
            .item_ids
            .iter()
            .all(|id| items.iter().any(|item| item.id == *id));
    let inherited: Vec<_> = items
        .iter()
        .copied()
        .filter(|item| valid_context && context.item_ids.contains(&item.id))
        .collect();
    if (kinds(&text).is_empty()
        && any(
            &text,
            &[
                "is it fixed",
                "fixed yet",
                "is it ready",
                "any update",
                "any updates",
                "anything yet",
                "status",
                "eta",
                "how long",
                "still waiting",
                "how is the fix going",
                "did you fix it",
            ],
        ))
        || (!inherited.is_empty()
            && any(
                &text,
                &["still broken", "still bad", "same problem", "same issue"],
            ))
    {
        if inherited.is_empty()
            || context.report_ids.is_empty()
            || !resolve(&text, &items, &inherited, catalog)
                .is_ok_and(|item| context.item_ids.contains(&item.id))
        {
            return clarify(Clarification::Report, suspicious_input);
        }
        let mut out = response(IntakeOutcome::FollowUp, None, suspicious_input);
        out.report_ids = context.report_ids.clone();
        return out;
    }
    let mut out = response(IntakeOutcome::Report, None, suspicious_input);
    let mut previous = inherited;
    for raw in message.text.split([';', '\n']) {
        let text = normalize(raw);
        if text.is_empty() {
            continue;
        }
        for text in clauses(&text, &items) {
            if text.is_empty() {
                continue;
            }
            let item = match resolve(text, &items, &previous, catalog) {
                Ok(item) => item,
                Err(reason) => return clarify(reason, suspicious_input),
            };
            let mut kinds = kinds(text);
            if kinds.is_empty() {
                return clarify(Clarification::Symptom, suspicious_input);
            }
            kinds.sort();
            if let Some(issue) = out.issues.iter_mut().find(|issue| issue.item_id == item.id) {
                issue.kinds.extend(kinds);
                issue.kinds.sort();
                issue.kinds.dedup();
            } else {
                out.issues.push(ParsedIssue {
                    item_id: item.id.clone(),
                    kinds,
                });
            }
            previous = vec![item];
        }
    }
    if out.issues.is_empty() {
        return clarify(Clarification::Input, suspicious_input);
    }
    out
}

pub(super) fn selected(
    message: &IncomingMessage,
    viewer: Option<&AuthorizedViewer>,
    catalog: &[CatalogItem],
    selected_item_id: &str,
    kinds: &[IssueKind],
) -> IntakeResponse {
    let Some((_, items)) = authorized(viewer, catalog) else {
        return response(IntakeOutcome::Unauthorized, None, false);
    };
    let suspicious_input = suspicious(&normalize(&message.text));
    if !items.iter().any(|item| item.id == selected_item_id) {
        return clarify(Clarification::Title, suspicious_input);
    }
    if kinds.is_empty() {
        return clarify(Clarification::Symptom, suspicious_input);
    }
    let mut kinds = kinds.to_vec();
    kinds.sort();
    kinds.dedup();
    let mut out = response(IntakeOutcome::Report, None, suspicious_input);
    out.issues.push(ParsedIssue {
        item_id: selected_item_id.into(),
        kinds,
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_distance_accepts_only_one_edit_or_adjacent_swap_on_long_words() {
        for (a, b, expected) in [
            ("", "", true),
            ("a", "a", true),
            ("abc", "abd", false),
            ("abcd", "abce", true),
            ("abcd", "abd", false),
            ("abcd", "abcde", true),
            ("abcde", "abcd", true),
            ("abcd", "abxy", false),
            ("abcdef", "abcd", false),
            ("abcd", "abcdef", false),
            ("abcd", "badc", false),
            ("abcd", "abdc", true),
            ("abcd", "acbd", true),
            ("abcd", "bacd", true),
            ("abcd", "bcad", false),
            ("abcd", "cbad", false),
            ("abcd", "acxd", false),
            ("abcd", "abdd", true),
            ("aaaa", "bbbb", false),
            ("abcd", "abdcx", false),
            ("abcde", "abdcex", false),
            ("abcd", "adcbb", false),
            ("acbdx", "abcd", false),
            ("café", "cafe", true),
            ("雨月", "雨日", false),
            ("été", "étè", false),
        ] {
            assert_eq!(close_word(a, b), expected, "{a} / {b}");
        }
    }

    #[test]
    fn numbered_words_and_ordinals_preserve_episode_numbers() {
        assert_eq!(
            number_words(
                "zero one first two second three third four fourth five fifth six sixth seven seventh eight eighth nine ninth ten tenth outside"
            ),
            "0 1 1 2 2 3 3 4 4 5 5 6 6 7 7 8 8 9 9 10 10 outside"
        );
        assert_eq!(coordinates("s0e0"), Ok((Some(0), Some(0))));
        assert_eq!(coordinates("season 2"), Ok((Some(2), None)));
        assert_eq!(coordinates("episode 3"), Ok((None, Some(3))));
        assert_eq!(coordinates("s1e2 ep2"), Ok((Some(1), Some(2))));
        assert_eq!(coordinates("s1e2 s2e2"), Err(Clarification::Episode));
        assert_eq!(coordinates("s1e2 s1e3"), Err(Clarification::Episode));
    }

    #[test]
    fn empty_names_and_word_boundaries_never_match() {
        assert!(!title_match("something", ""));
        assert!(!title_match("something", "  "));
        assert!(!title_match("ab", "abc"));
        assert!(!contains("unbroken", "broken"));
        assert!(!contains("subsonic", "subs"));
        assert!(contains("a broken item", "broken"));
    }
}
