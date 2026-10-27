//! Server-side validation of the reference wizard's answer paths. No action
//! supplied by a client is accepted. These terminals describe routing, not effects.
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriageAction {
    Autofix,
    Langcheck,
    Playcheck,
    Notify,
    NotifyUrgent,
    Closed,
    Wherefrom,
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum ValidationError {
    #[error("unknown issue type")]
    UnknownIssue,
    #[error("unknown media type or incompatible scope")]
    InvalidScope,
    #[error("issue is TV-only")]
    TelevisionOnly,
    #[error("answers do not match a complete path")]
    InvalidAnswers,
    #[error("free text is only accepted for other issues")]
    UnexpectedFreeText,
}

pub fn validate_path(
    issue_type: &str,
    media_type: &str,
    scope: &str,
    answers: &[&str],
    has_free_text: bool,
) -> Result<TriageAction, ValidationError> {
    use TriageAction as A;
    use ValidationError as E;
    if !matches!(
        (media_type, scope),
        ("movie", "movie") | ("show", "episode" | "show")
    ) {
        return Err(E::InvalidScope);
    }
    if has_free_text && issue_type != "other" {
        return Err(E::UnexpectedFreeText);
    }
    let split = if scope == "show" {
        A::Notify
    } else {
        A::Autofix
    };
    let action = match issue_type {
        "video-broken" => match answers {
            [] => split,
            _ => return Err(E::InvalidAnswers),
        },
        "audio" => match answers {
            ["no-sound"] => split,
            ["wrong-language", "english" | "spanish"] => A::Langcheck,
            ["wrong-language", "other-language"] => A::Notify,
            _ => return Err(E::InvalidAnswers),
        },
        "wrong-episode" => {
            if media_type != "show" {
                return Err(E::TelevisionOnly);
            }
            match answers {
                ["different-episode" | "something-else" | "not-sure"] => split,
                _ => return Err(E::InvalidAnswers),
            }
        }
        "wont-play" => match answers {
            ["everything-broken"] => A::NotifyUrgent,
            ["just-this-one"] => split,
            _ => return Err(E::InvalidAnswers),
        },
        "poor-quality" => match answers {
            ["comes-and-goes" | "whole-time"] => A::Playcheck,
            _ => return Err(E::InvalidAnswers),
        },
        "subtitles" => match answers {
            ["english" | "spanish" | "other-language"] => A::Notify,
            _ => return Err(E::InvalidAnswers),
        },
        "buffering" => match answers {
            ["helped"] => A::Closed,
            ["still-bad"] => A::Playcheck,
            _ => return Err(E::InvalidAnswers),
        },
        "ads" => match answers {
            [] => A::Wherefrom,
            _ => return Err(E::InvalidAnswers),
        },
        "other" => match answers {
            [] => A::Notify,
            _ => return Err(E::InvalidAnswers),
        },
        _ => return Err(E::UnknownIssue),
    };
    Ok(action)
}
