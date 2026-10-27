//! Authenticated arr observation intake. No payload supplies mutation authority.
use pitcrew_store::{Store, StoreError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use subtle::ConstantTimeEq;
use thiserror::Error;

pub const MAX_BODY: usize = 65_536;

/// Hashes are kept private and never formatted into diagnostics.
pub struct Secret([u8; 32]);
impl Secret {
    pub fn new(value: &str) -> Result<Self, IntakeError> {
        if !(32..=4096).contains(&value.len()) || !value.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(IntakeError::Configuration);
        }
        Ok(Self(Sha256::digest(value.as_bytes()).into()))
    }
    pub fn accepts(&self, value: &str) -> bool {
        let candidate: [u8; 32] = Sha256::digest(value.as_bytes()).into();
        bool::from(self.0.ct_eq(&candidate))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Grab,
    Download,
    DownloadFailed,
    ManualInteractionRequired,
    Rename,
    EpisodeFileDelete,
    MovieFileDelete,
    Health,
    Test,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArrObservation {
    pub kind: Kind,
    pub download: Option<String>,
    pub media: Vec<u64>,
    pub files: Vec<u64>,
}

#[derive(Debug, Error)]
pub enum IntakeError {
    #[error(
        "webhook configuration requires unique instance IDs and 32..4096 visible ASCII secret bytes"
    )]
    Configuration,
    #[error("unauthorized")]
    Unauthorized,
    #[error("invalid webhook payload")]
    Payload,
    #[error("payload too large")]
    TooLarge,
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub struct Intake {
    store: Store,
    secrets: BTreeMap<String, Secret>,
}
impl Intake {
    pub fn new(
        store: Store,
        instances: impl IntoIterator<Item = (String, Secret)>,
    ) -> Result<Self, IntakeError> {
        let mut secrets = BTreeMap::new();
        for (id, secret) in instances {
            if id.is_empty()
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
                || secrets.insert(id, secret).is_some()
            {
                return Err(IntakeError::Configuration);
            }
        }
        Ok(Self { store, secrets })
    }
    pub fn authorized(&self, instance: &str, secret: &str) -> bool {
        self.secrets
            .get(instance)
            .is_some_and(|expected| expected.accepts(secret))
    }
    pub async fn ingest(
        &self,
        instance: &str,
        secret: &str,
        body: &[u8],
        now: u64,
    ) -> Result<bool, IntakeError> {
        if !self.authorized(instance, secret) {
            return Err(IntakeError::Unauthorized);
        }
        if body.len() > MAX_BODY {
            return Err(IntakeError::TooLarge);
        }
        let raw: Value = serde_json::from_slice(body).map_err(|_| IntakeError::Payload)?;
        let observation = parse(&raw)?;
        // serde_json's default sorted object keys make whitespace/key order
        // irrelevant; event bodies without stable IDs get lifetime replay dedup.
        let canonical = serde_json::to_vec(&raw).map_err(|_| IntakeError::Payload)?;
        let key = hex::encode(Sha256::digest(&canonical));
        Ok(self
            .store
            .ingest_observation(
                &format!("webhook:{instance}"),
                &key,
                &serde_json::to_value(observation).map_err(|_| IntakeError::Payload)?,
                now,
            )
            .await?)
    }
}

pub fn parse(value: &Value) -> Result<ArrObservation, IntakeError> {
    let kind = match value.get("eventType").and_then(Value::as_str) {
        Some("Grab") => Kind::Grab,
        Some("Download" | "Import") => Kind::Download,
        Some("DownloadFailed") => Kind::DownloadFailed,
        Some("ManualInteractionRequired") => Kind::ManualInteractionRequired,
        Some("Rename") => Kind::Rename,
        Some("EpisodeFileDelete") => Kind::EpisodeFileDelete,
        Some("MovieFileDelete") => Kind::MovieFileDelete,
        Some("Health" | "HealthRestored") => Kind::Health,
        Some("Test") => Kind::Test,
        _ => return Err(IntakeError::Payload),
    };
    let download = match value.get("downloadId") {
        None | Some(Value::Null) => None,
        Some(Value::String(id)) if !id.is_empty() && id.len() <= 256 => Some(id.clone()),
        _ => return Err(IntakeError::Payload),
    };
    let mut media = vec![];
    let mut files = vec![];
    for (key, output) in [("movie", &mut media), ("movieFile", &mut files)] {
        collect_id(value.get(key), output)?;
    }
    if let Some(episodes) = value.get("episodes") {
        for episode in episodes.as_array().ok_or(IntakeError::Payload)? {
            collect_id(Some(episode), &mut media)?;
        }
    }
    collect_id(value.get("episodeFile"), &mut files)?;
    // Sonarr's OnImportComplete carries an array, while OnDownload uses the
    // singular episodeFile. Both are Download observations in its webhook API.
    if let Some(episode_files) = value.get("episodeFiles") {
        for file in episode_files.as_array().ok_or(IntakeError::Payload)? {
            collect_id(Some(file), &mut files)?;
        }
    }
    media.sort_unstable();
    media.dedup();
    files.sort_unstable();
    files.dedup();
    Ok(ArrObservation {
        kind,
        download,
        media,
        files,
    })
}
fn collect_id(value: Option<&Value>, output: &mut Vec<u64>) -> Result<(), IntakeError> {
    if let Some(value) = value {
        let id = value
            .get("id")
            .and_then(Value::as_u64)
            .filter(|id| *id > 0)
            .ok_or(IntakeError::Payload)?;
        output.push(id);
    }
    Ok(())
}
