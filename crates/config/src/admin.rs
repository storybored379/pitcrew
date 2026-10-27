//! Optional owner interface. Validation is performed by `pitcrew-admin`.
use crate::SecretRef;
use serde::Deserialize;
use std::{collections::BTreeMap, net::SocketAddr, path::PathBuf};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminUi {
    pub database: PathBuf,
    pub engine_database: PathBuf,
    pub intake_database: PathBuf,
    pub bind: SocketAddr,
    pub public_origin: String,
    pub plex_auth_url: String,
    pub client_id: String,
    pub admin_account_ids: Vec<String>,
    pub api: Option<AdminEndpoint>,
    #[serde(default)]
    pub rates: BTreeMap<String, AdminRates>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminEndpoint {
    pub url: String,
    pub secret: SecretRef,
}

/// Local estimates only, never a provider account or billing setting.
#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(deny_unknown_fields)]
pub struct AdminRates {
    pub input_microusd_per_million: u64,
    pub output_microusd_per_million: u64,
    pub cache_creation_microusd_per_million: u64,
    pub cache_read_microusd_per_million: u64,
}
