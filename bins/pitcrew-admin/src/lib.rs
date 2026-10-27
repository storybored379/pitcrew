//! Owner-only administration. Engine and intake journals are read-only here;
//! mutations pass through the control plane or an audited pending command.
pub mod accounting;
pub mod actions;
pub mod auth;
pub mod config;
pub mod journal;
pub mod read;
pub mod render;
pub mod web;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Sign in with an allowed Plex administrator account.")]
    Unauthorized,
    #[error("This request could not be authorized. Refresh the page and try again.")]
    Forbidden,
    #[error("Check the form fields and try again.")]
    Input,
    #[error("The record changed or this request key was already used. Refresh the page.")]
    Conflict,
    #[error("The requested record was not found.")]
    Missing,
    #[error("Too many sign-in attempts. Please wait a minute.")]
    Rate,
    #[error("The saved data is unavailable. Please try again.")]
    Storage,
    #[error("Plex could not verify your identity. Please try again.")]
    Upstream,
}

impl From<sqlx::Error> for Error {
    fn from(_: sqlx::Error) -> Self {
        Self::Storage
    }
}
impl From<serde_json::Error> for Error {
    fn from(_: serde_json::Error) -> Self {
        Self::Storage
    }
}
pub type Result<T> = std::result::Result<T, Error>;
