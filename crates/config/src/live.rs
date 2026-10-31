//! Local certification inputs. Presence alone never authorizes a live worker.
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Live {
    pub enabled: bool,
    pub certifications: Vec<ClientCertification>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientCertification {
    pub client: String,
    pub id: String,
    pub version: String,
    pub fixture_digest: String,
    pub certified_at: u64,
    pub expires_at: u64,
    pub certificate_file: PathBuf,
    pub acknowledgement_file: PathBuf,
    pub capabilities_file: PathBuf,
}
