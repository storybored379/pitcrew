//! Channel normalization and the single intake boundary used by the drill runner.
//! No message body can issue a library-access grant or execute a repair.
pub mod channels;
pub mod runtime;
pub mod server;
pub mod stack;

use pitcrew_domain::channels::IncomingMessage;
use pitcrew_intake::{
    AuthorizedViewer, CatalogItem, ConversationContext, IntakeResponse, IntakeService,
};
use thiserror::Error;

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum DispatchError {
    #[error("message fields exceed their bounds or contain invalid identifiers")]
    InvalidMessage,
}

pub fn validate_message(message: &IncomingMessage) -> Result<(), DispatchError> {
    for field in [
        &message.sender.channel_id,
        &message.sender.platform_user_id,
        &message.message_id,
        &message.conversation_id,
    ] {
        if field.is_empty() || field.len() > 128 || field.chars().any(char::is_control) {
            return Err(DispatchError::InvalidMessage);
        }
    }
    if message.text.trim().is_empty() || message.text.len() > 8192 {
        return Err(DispatchError::InvalidMessage);
    }
    Ok(())
}

/// `viewer` must come from an authenticated adapter's fresh authorization lookup,
/// never from deserializing a viewer-supplied message. Catalog and context are
/// likewise server-owned. This pure path is also used by the HTTP test channel.
pub fn dispatch(
    message: &IncomingMessage,
    viewer: Option<&AuthorizedViewer>,
    catalog: &[CatalogItem],
    context: &ConversationContext,
) -> Result<IntakeResponse, DispatchError> {
    validate_message(message)?;
    Ok(IntakeService.handle(message, viewer, catalog, context))
}

/// Catalog item identities must remain unambiguous across authorization scopes.
pub(crate) fn validate_catalog(catalog: &[CatalogItem]) -> Result<(), &'static str> {
    let mut ids = std::collections::HashSet::new();
    for item in catalog {
        if item.id.is_empty() || item.library_id.is_empty() || !ids.insert(&item.id) {
            return Err("catalog requires unique item IDs and nonempty library IDs");
        }
    }
    Ok(())
}
