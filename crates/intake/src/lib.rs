//! Conservative, channel-independent parsing of untrusted viewer messages.
//!
//! This crate only proposes reports. It has no engine, filesystem, transport or
//! download capability. Authorization and conversation history come from trusted
//! adapters; a parsed report never authorizes a repair.

use pitcrew_domain::channels::IncomingMessage;
use serde::{Deserialize, Serialize};

mod parser;
pub mod pipeline;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    Hd,
    Uhd,
}

/// One movie or one episode in a particular authorized library.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogItem {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub library_id: String,
    pub library_name: String,
    pub resolution: Resolution,
    pub season: Option<u32>,
    pub episode: Option<u32>,
}

/// Supplied by fresh membership authorization, never deserialized from a message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizedViewer {
    pub account_id: String,
    pub library_ids: Vec<String>,
}

/// Server-owned context, scoped to both the account and conversation. A channel
/// adapter must load it by its authenticated identity, not accept it from text.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConversationContext {
    pub account_id: String,
    pub conversation_id: String,
    pub item_ids: Vec<String>,
    pub report_ids: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueKind {
    Playback,
    Buffering,
    Subtitles,
    AudioLanguage,
    NoAudio,
    WrongEpisode,
    Quality,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedIssue {
    pub item_id: String,
    pub kinds: Vec<IssueKind>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntakeOutcome {
    Unauthorized,
    Clarify,
    Report,
    FollowUp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Clarification {
    Title,
    Episode,
    Library,
    Symptom,
    Report,
    Input,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntakeResponse {
    pub outcome: IntakeOutcome,
    /// Only complete, unambiguous selections appear here. A Clarify response is
    /// atomic: hosts must not open partial reports for the other clauses.
    pub issues: Vec<ParsedIssue>,
    pub clarification: Option<Clarification>,
    pub report_ids: Vec<String>,
    /// Fixed safe copy. Never echoes arbitrary viewer text or promises readiness.
    pub reply: String,
    pub suspicious_input: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct IntakeService;

impl IntakeService {
    pub fn handle(
        &self,
        message: &IncomingMessage,
        viewer: Option<&AuthorizedViewer>,
        catalog: &[CatalogItem],
        context: &ConversationContext,
    ) -> IntakeResponse {
        parser::handle(message, viewer, catalog, context)
    }

    /// Wizard selections use the same authorization and observation-only output.
    /// A selected ID does not bypass the viewer's current library permissions.
    pub fn handle_selected(
        &self,
        message: &IncomingMessage,
        viewer: Option<&AuthorizedViewer>,
        catalog: &[CatalogItem],
        selected_item_id: &str,
        kinds: &[IssueKind],
    ) -> IntakeResponse {
        parser::selected(message, viewer, catalog, selected_item_id, kinds)
    }
}
