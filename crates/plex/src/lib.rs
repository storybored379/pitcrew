//! Plex server reads. The token travels in a header, never a URL. No refresh,
//! analyze or other mutating endpoints are exposed, including mutating GETs.
use pitcrew_upstream::{Endpoint, Error, JsonClient, Transport};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Deserialize, Serialize)]
pub struct Section {
    pub key: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub title: Option<String>,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct Metadata {
    #[serde(rename = "ratingKey")]
    pub rating_key: String,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub title: Option<String>,
    #[serde(rename = "Media")]
    pub media: Option<Vec<Value>>,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct Session {
    #[serde(rename = "ratingKey")]
    pub rating_key: Option<String>,
    #[serde(rename = "sessionKey")]
    pub session_key: Option<String>,
    #[serde(rename = "Session")]
    pub session: Option<Value>,
    #[serde(rename = "Player")]
    pub player: Option<Value>,
    #[serde(rename = "TranscodeSession")]
    pub transcode_session: Option<Value>,
}
#[derive(Deserialize)]
struct Envelope<T> {
    #[serde(rename = "MediaContainer")]
    container: T,
}
#[derive(Deserialize)]
struct Identity {
    #[serde(rename = "machineIdentifier")]
    machine_identifier: String,
}
#[derive(Deserialize)]
struct Sections {
    #[serde(rename = "Directory")]
    entries: Option<Vec<Section>>,
    size: u64,
}
#[derive(Deserialize)]
struct Items<T> {
    #[serde(rename = "Metadata")]
    entries: Option<Vec<T>>,
    size: u64,
}

pub struct Client<T>(JsonClient<T>);
impl<T: Transport> Client<T> {
    pub fn new(endpoint: Endpoint, transport: T) -> Self {
        Self(JsonClient::new(endpoint, transport))
    }
    /// The server's `machineIdentifier`, which viewer membership checks match.
    pub async fn identity(&self) -> Result<String, Error> {
        let response: Envelope<Identity> = self.0.get("identity", vec![]).await?;
        let id = response.container.machine_identifier;
        if id.is_empty()
            || id.len() > 128
            || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(Error::InvalidResponse);
        }
        Ok(id)
    }
    pub async fn sections(&self) -> Result<Vec<Section>, Error> {
        let response: Envelope<Sections> = self.0.get("library/sections", vec![]).await?;
        complete(response.container.entries, response.container.size)
    }
    pub async fn metadata(&self, rating_key: u64) -> Result<Vec<Metadata>, Error> {
        if rating_key == 0 {
            return Err(Error::InvalidInput);
        }
        let response: Envelope<Items<Metadata>> = self
            .0
            .get(&format!("library/metadata/{rating_key}"), vec![])
            .await?;
        complete(response.container.entries, response.container.size)
    }
    /// A failed/malformed response is an error, never zero active streams.
    pub async fn sessions(&self) -> Result<Vec<Session>, Error> {
        let response: Envelope<Items<Session>> = self.0.get("status/sessions", vec![]).await?;
        complete(response.container.entries, response.container.size)
    }
}
fn complete<T>(entries: Option<Vec<T>>, size: u64) -> Result<Vec<T>, Error> {
    let entries = entries.unwrap_or_default();
    if entries.len() as u64 != size {
        return Err(Error::InvalidResponse);
    }
    Ok(entries)
}
