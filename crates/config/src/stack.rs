//! Settings shared by the separately supervised stack processes.
use super::{ConfigError, endpoint, invalid, nonempty};
use serde::Deserialize;
use std::{net::SocketAddr, path::PathBuf};

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Stack {
    /// Opt in to diagnosis consumption; never enables upstream mutations.
    pub enabled: bool,
    pub intake_database: PathBuf,
    pub evidence_max_age_seconds: u64,
    pub web_listen: SocketAddr,
    pub web_public_origin: String,
    pub plex_auth_url: String,
    pub plex_client_id: String,
    pub bot_seerr_user_id: Option<u64>,
    /// Optional initial catalog; a synced catalog adapter can replace this input.
    pub catalog: Vec<StackCatalogItem>,
    /// Catalog snapshot written by the sync job; when set, the viewer web reads
    /// it instead of the static `catalog`.
    pub catalog_snapshot_file: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StackCatalogItem {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub library_id: String,
    pub library_name: String,
    pub resolution: StackResolution,
    pub season: Option<u32>,
    pub episode: Option<u32>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StackResolution {
    Hd,
    Uhd,
}
impl Default for Stack {
    fn default() -> Self {
        Self {
            enabled: false,
            intake_database: "intake.sqlite".into(),
            evidence_max_age_seconds: 300,
            web_listen: ([127, 0, 0, 1], 7401).into(),
            web_public_origin: "http://127.0.0.1:7401".into(),
            plex_auth_url: "https://plex.tv/".into(),
            plex_client_id: "pitcrew".into(),
            bot_seerr_user_id: None,
            catalog: Vec::new(),
            catalog_snapshot_file: None,
        }
    }
}
impl Stack {
    pub(super) fn validate(&self) -> Result<(), ConfigError> {
        if self
            .catalog_snapshot_file
            .as_ref()
            .is_some_and(|path| !path.is_absolute())
        {
            return Err(invalid(
                "stack.catalog_snapshot_file",
                "must be an absolute path",
            ));
        }
        if self.intake_database.as_os_str().is_empty() {
            return Err(invalid("stack.intake_database", "must not be empty"));
        }
        if !(1..=3600).contains(&self.evidence_max_age_seconds) {
            return Err(invalid(
                "stack.evidence_max_age_seconds",
                "must be between 1 and 3600 seconds",
            ));
        }
        if !self.web_listen.ip().is_loopback() {
            return Err(invalid(
                "stack.web_listen",
                "must bind loopback; use a TLS ingress",
            ));
        }
        let origin = url::Url::parse(&self.web_public_origin)
            .map_err(|_| invalid("stack.web_public_origin", "must be an HTTP(S) origin"))?;
        if origin.origin().ascii_serialization() != self.web_public_origin
            || !(origin.scheme() == "https"
                || origin.scheme() == "http"
                    && matches!(origin.host_str(), Some("127.0.0.1" | "[::1]" | "localhost")))
        {
            return Err(invalid(
                "stack.web_public_origin",
                "must be an HTTPS origin (HTTP allowed only on loopback)",
            ));
        }
        endpoint(&self.plex_auth_url, "stack.plex_auth_url", true)?;
        nonempty(&self.plex_client_id, "stack.plex_client_id")?;
        if self.bot_seerr_user_id == Some(0) {
            return Err(invalid("stack.bot_seerr_user_id", "must be positive"));
        }
        let mut ids = std::collections::HashSet::new();
        if self
            .catalog
            .iter()
            .any(|item| item.id.is_empty() || item.library_id.is_empty() || !ids.insert(&item.id))
        {
            return Err(invalid(
                "stack.catalog",
                "requires unique item IDs and nonempty library IDs",
            ));
        }
        Ok(())
    }
}
