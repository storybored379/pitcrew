//! Templates are deliverable; optional Claude drafts are private review material.
//! A ready statement can only be derived from a verified FixJob state.
use crate::{DeciderError, Usage, providers::ClaudeDecider};
use pitcrew_domain::{FixJob, State};
use pitcrew_upstream::Transport;
use serde::Serialize;
use std::{future::Future, pin::Pin};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplyStage {
    Received,
    Checking,
    Fixing,
    Ready,
    Escalated,
    Closed,
}

/// No deserialization or arbitrary stage setter. Even a selected model decision
/// cannot manufacture the proof required for a ready reply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RedactedReplyFacts {
    stage: ReplyStage,
}
impl RedactedReplyFacts {
    pub fn received() -> Self {
        Self {
            stage: ReplyStage::Received,
        }
    }
    pub fn checking() -> Self {
        Self {
            stage: ReplyStage::Checking,
        }
    }
    pub fn escalated() -> Self {
        Self {
            stage: ReplyStage::Escalated,
        }
    }
    /// Viewer-reported resolution, never a claim of verified repaired bytes.
    pub fn self_resolved() -> Self {
        Self {
            stage: ReplyStage::Closed,
        }
    }
    pub fn for_job(job: &FixJob) -> Self {
        let stage = match job.state() {
            State::Verified | State::Done => ReplyStage::Ready,
            State::Failed => ReplyStage::Escalated,
            _ => ReplyStage::Fixing,
        };
        Self { stage }
    }
    pub fn stage(&self) -> ReplyStage {
        self.stage
    }
}

/// Untrusted generated text has no Debug/Serialize implementation. It is never
/// returned through `delivery_text`; callers must deliberately open private review.
pub struct ReplyDraftReport {
    text: String,
    pub usage: Usage,
}
impl ReplyDraftReport {
    pub(crate) fn new(text: String, usage: Usage) -> Self {
        Self { text, usage }
    }
    pub fn text_for_review(&self) -> &str {
        &self.text
    }
}

pub struct ReplyReport {
    pub delivery_text: String,
    pub draft: Option<ReplyDraftReport>,
    pub draft_failed: bool,
}
pub type ReplyFuture<'a> = Pin<Box<dyn Future<Output = ReplyReport> + Send + 'a>>;
pub trait ReplyWriter: Send + Sync {
    fn write<'a>(&'a self, facts: &'a RedactedReplyFacts) -> ReplyFuture<'a>;
}

/// Owner display name is deployment configuration, used locally only. Templates
/// make no claim that an admin was notified until the outbox confirms delivery.
pub struct TemplateReplyWriter {
    owner_label: String,
}
impl Default for TemplateReplyWriter {
    fn default() -> Self {
        Self {
            owner_label: "the server owner".into(),
        }
    }
}
impl TemplateReplyWriter {
    pub fn new(owner_label: String) -> Result<Self, DeciderError> {
        if owner_label.trim().is_empty()
            || owner_label.chars().count() > 80
            || owner_label.chars().any(char::is_control)
        {
            return Err(DeciderError::Configuration("reply.owner_label"));
        }
        Ok(Self { owner_label })
    }
    fn render(&self, facts: &RedactedReplyFacts) -> String {
        match facts.stage {
            ReplyStage::Received => "Got your report. We'll check what's happening.".into(),
            ReplyStage::Checking => {
                "We're checking this one. We'll update you when we know more.".into()
            }
            ReplyStage::Fixing => {
                "The fix is still in progress. We'll check the new copy before saying it's ready."
                    .into()
            }
            ReplyStage::Ready => "The new copy passed our playback check. Ready to watch!".into(),
            ReplyStage::Escalated => format!(
                "This one needs {0} to take a look. Your report has the details.",
                self.owner_label
            ),
            ReplyStage::Closed => "Glad it's working again. Let us know if it comes back.".into(),
        }
    }
}
impl ReplyWriter for TemplateReplyWriter {
    fn write<'a>(&'a self, facts: &'a RedactedReplyFacts) -> ReplyFuture<'a> {
        Box::pin(async move {
            ReplyReport {
                delivery_text: self.render(facts),
                draft: None,
                draft_failed: false,
            }
        })
    }
}

/// Explicit opt-in flag controls requests in addition to provider cloud consent.
/// Until prose has its own validated-fact certification, drafts never deliver.
pub struct ClaudeReplyWriter<T> {
    pub provider: ClaudeDecider<T>,
    pub enabled: bool,
    pub templates: TemplateReplyWriter,
}
impl<T: Transport> ReplyWriter for ClaudeReplyWriter<T> {
    fn write<'a>(&'a self, facts: &'a RedactedReplyFacts) -> ReplyFuture<'a> {
        Box::pin(async move {
            let mut report = self.templates.write(facts).await;
            if self.enabled {
                match self.provider.draft_reply(facts).await {
                    Ok(draft) => report.draft = Some(draft),
                    Err(_) => report.draft_failed = true,
                }
            }
            report
        })
    }
}
