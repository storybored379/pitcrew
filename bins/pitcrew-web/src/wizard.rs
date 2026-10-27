//! Validated wizard intent. Choices are observations, never repair authority.
use pitcrew_intake::IssueKind;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum WizardIssue {
    Playback,
    Buffering,
    Subtitles,
    AudioLanguage,
    NoAudio,
    WrongEpisode,
    Quality,
    VideoBroken,
    Ads,
    Other,
}

impl WizardIssue {
    pub(super) fn kind(self) -> Option<IssueKind> {
        match self {
            Self::Playback | Self::VideoBroken => Some(IssueKind::Playback),
            Self::Buffering => Some(IssueKind::Buffering),
            Self::Subtitles => Some(IssueKind::Subtitles),
            Self::AudioLanguage => Some(IssueKind::AudioLanguage),
            Self::NoAudio => Some(IssueKind::NoAudio),
            Self::WrongEpisode => Some(IssueKind::WrongEpisode),
            Self::Quality => Some(IssueKind::Quality),
            Self::Ads | Self::Other => None,
        }
    }
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::VideoBroken => "Video looks broken",
            Self::Ads => "Ads or unexpected version",
            Self::Other => "Something else",
            _ => super::issue_label(self.kind().expect("known diagnostic issue")),
        }
    }
    pub(super) fn disposition(self, answer: &str) -> &'static str {
        match (self, answer) {
            (Self::Buffering, "helped") => "self_resolved",
            (Self::Ads, _) | (Self::Buffering, "not_tried") => "advice",
            (Self::Other, _) => "review",
            _ => "diagnostic",
        }
    }
    pub(super) fn valid(self, answer: &str, details: &str) -> bool {
        question(self).2.iter().any(|(id, _)| *id == answer)
            && if self == Self::Other {
                !details.trim().is_empty() && details.chars().count() <= 140
            } else {
                details.chars().count() <= 2000
            }
    }
}

pub(super) fn receipt(issue: WizardIssue, answer: &str) -> &'static str {
    match issue.disposition(answer) {
        "self_resolved" => {
            "<h1>Glad that helped</h1><p>Logged as self-resolved. If the problem comes back, send another report.</p><a href=\"/my-reports\">View my reports</a>"
        }
        "advice" if issue == WizardIssue::Ads => {
            "<h1>Open your shared library</h1><p>In Plex, select your shared server from the sidebar or server menu, then open this title from its library. Check the version before pressing Play.</p><p>Ads can indicate that Plex opened its free catalog or another service. If the shared copy still has a problem, report it again.</p><a href=\"/my-reports\">View my reports</a>"
        }
        "advice" => {
            "<h1>Try these playback checks</h1><p>Pause for 30 seconds, reduce playback quality in Plex settings, or restart the app. If it still buffers, report it again and choose “It still buffers”.</p><a href=\"/my-reports\">View my reports</a>"
        }
        _ => {
            "<h1>Report received</h1><p>Your report is saved. Follow any checks or repairs in My reports.</p><a class=\"button\" href=\"/my-reports\">View my reports</a>"
        }
    }
}

pub(super) fn question(
    issue: WizardIssue,
) -> (
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str)],
) {
    match issue {
        WizardIssue::Ads => (
            "Where did you start playback?",
            "Plex search can open its free catalog or another service. Open your shared server in the sidebar, then select this title from its library.",
            &[("check_source", "I will check the playback source")],
        ),
        WizardIssue::Other => (
            "What is going wrong?",
            "Describe the problem in one line (up to 140 characters). Your report will be saved for review.",
            &[("describe", "I have described the problem below")],
        ),
        WizardIssue::VideoBroken => (
            "What does the picture look like?",
            "For a green, glitchy, or black picture while sound keeps playing, send a report so the exact file can be checked.",
            &[("confirmed", "The video is green, glitchy, or black")],
        ),
        WizardIssue::Playback => (
            "Does other stuff on Plex play fine right now?",
            "Check that you opened the title from your shared server. Plex also shows copies from other services.",
            &[
                ("everything_broken", "No, everything is broken"),
                ("just_this_one", "Yes, just this one"),
            ],
        ),
        WizardIssue::Buffering => (
            "Did pausing briefly or restarting the app help?",
            "Try pausing for 30 seconds, reducing playback quality in Plex settings, or restarting the app. These checks do not prove whether the file is damaged.",
            &[
                ("helped", "That helped"),
                ("still_bad", "It still buffers"),
                ("not_tried", "I have not tried yet"),
            ],
        ),
        WizardIssue::AudioLanguage | WizardIssue::Subtitles => (
            "Which language do you need?",
            "If you need another language, name it in the details below.",
            &[
                ("english", "English"),
                ("spanish", "Spanish"),
                ("other_language", "Another language"),
            ],
        ),
        WizardIssue::WrongEpisode => (
            "Which episode did you actually get?",
            "Confirm the episode number you selected before sending.",
            &[
                ("different_episode", "A different episode of this show"),
                ("something_else", "Something else entirely"),
                ("not_sure", "I am not sure"),
            ],
        ),
        WizardIssue::Quality => (
            "Is it bad the whole way through, or does it come and go?",
            "",
            &[
                ("comes_and_goes", "It comes and goes"),
                ("whole_time", "It is bad the whole time"),
            ],
        ),
        WizardIssue::NoAudio => (
            "Is the device muted?",
            "Check the player and device volume before sending your report.",
            &[
                ("checked", "I checked and there is still no sound"),
                ("not_sure", "I am not sure"),
            ],
        ),
    }
}
