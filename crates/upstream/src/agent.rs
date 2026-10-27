//! Read-only client of the local P1 agent's authenticated versioned API.
use crate::{Endpoint, Error, JsonClient, Transport};
use pitcrew_domain::media::MediaEvidence;
use serde_json::Value;

pub struct Client<T>(JsonClient<T>);
impl<T: Transport> Client<T> {
    pub fn new(endpoint: Endpoint, transport: T) -> Self {
        Self(JsonClient::new(endpoint, transport))
    }
    pub async fn health(&self) -> Result<Value, Error> {
        self.0.get("v1/health", vec![]).await
    }
    pub async fn evidence(&self, path: &str, decode: bool) -> Result<MediaEvidence, Error> {
        self.0
            .get(
                "v1/evidence",
                vec![
                    ("path".into(), checked_path(path)?),
                    ("decode".into(), decode.to_string()),
                ],
            )
            .await
    }
    pub async fn probe(&self, path: &str) -> Result<Value, Error> {
        self.0
            .get("v1/probe", vec![("path".into(), checked_path(path)?)])
            .await
    }
    pub async fn decode(&self, path: &str) -> Result<Value, Error> {
        self.0
            .get("v1/decodecheck", vec![("path".into(), checked_path(path)?)])
            .await
    }
    pub async fn file(&self, path: &str) -> Result<Value, Error> {
        self.0
            .get("v1/file", vec![("path".into(), checked_path(path)?)])
            .await
    }
    pub async fn plex_failures(&self, minutes: u16) -> Result<Value, Error> {
        if !(1..=1440).contains(&minutes) {
            return Err(Error::InvalidInput);
        }
        self.0
            .get(
                "v1/plexfails",
                vec![("minutes".into(), minutes.to_string())],
            )
            .await
    }
}
fn checked_path(path: &str) -> Result<String, Error> {
    if !path.starts_with('/') || path.contains('\0') || path.len() > 3072 {
        return Err(Error::InvalidInput);
    }
    Ok(path.into())
}
