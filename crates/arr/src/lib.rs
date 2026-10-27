//! Sonarr/Radarr v3 reads and explicitly live-gated writes. No mark-failed API.
pub mod shadow;
use pitcrew_upstream::{Endpoint, Error, JsonClient, LivePermit, Method, Transport};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Sonarr,
    Radarr,
}
impl Kind {
    fn media_key(self) -> &'static str {
        match self {
            Self::Sonarr => "episodeId",
            Self::Radarr => "movieId",
        }
    }
    fn file_path(self) -> &'static str {
        match self {
            Self::Sonarr => "episodefile",
            Self::Radarr => "moviefile",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Page {
    pub number: u32,
    pub size: u16,
}
impl Default for Page {
    fn default() -> Self {
        Self {
            number: 1,
            size: 100,
        }
    }
}
impl Page {
    fn query(self) -> Result<Vec<(String, String)>, Error> {
        if self.number == 0 || !(1..=1000).contains(&self.size) {
            return Err(Error::InvalidInput);
        }
        Ok(vec![
            ("page".into(), self.number.to_string()),
            ("pageSize".into(), self.size.to_string()),
        ])
    }
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PageResult<T> {
    pub page: u32,
    pub page_size: u16,
    pub total_records: u64,
    pub records: Vec<T>,
}

// Optional facts stay unknown when absent. Never deserialize missing evidence as
// a successful import, permitted access or proof that a queue entry is ours.
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueRecord {
    pub id: u64,
    pub download_id: Option<String>,
    pub series_id: Option<u64>,
    pub episode_id: Option<u64>,
    pub movie_id: Option<u64>,
    pub status: Option<String>,
    pub tracked_download_status: Option<String>,
    pub tracked_download_state: Option<String>,
    pub output_path: Option<String>,
    pub title: Option<String>,
    pub size: Option<f64>,
    pub sizeleft: Option<f64>,
    pub status_messages: Option<Vec<StatusMessage>>,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct StatusMessage {
    pub title: Option<String>,
    pub messages: Option<Vec<String>>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryRecord {
    pub id: u64,
    pub event_type: String,
    pub date: String,
    pub download_id: Option<String>,
    pub episode_id: Option<u64>,
    pub movie_id: Option<u64>,
    pub series_id: Option<u64>,
    pub source_title: Option<String>,
    pub data: Option<BTreeMap<String, Value>>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaFile {
    pub id: u64,
    pub path: Option<String>,
    pub size: Option<u64>,
    pub movie_id: Option<u64>,
    pub series_id: Option<u64>,
    pub quality: Option<Value>,
    pub media_info: Option<Value>,
    pub date_added: Option<String>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Release {
    pub guid: String,
    pub indexer_id: u64,
    pub title: Option<String>,
    pub size: Option<u64>,
    pub approved: Option<bool>,
    pub rejected: Option<bool>,
    pub download_allowed: Option<bool>,
    pub custom_format_score: Option<i64>,
    pub rejections: Option<Vec<String>>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportCandidate {
    pub id: u64,
    pub path: String,
    pub download_id: Option<String>,
    pub size: Option<u64>,
    pub series: Option<Value>,
    pub movie: Option<Value>,
    pub episodes: Option<Vec<Value>>,
    pub quality: Option<Value>,
    pub languages: Option<Vec<Value>>,
    pub release_group: Option<String>,
    pub rejections: Option<Vec<Value>>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlocklistRecord {
    pub id: u64,
    pub series_id: Option<u64>,
    pub movie_id: Option<u64>,
    pub source_title: Option<String>,
    pub date: Option<String>,
    pub message: Option<String>,
}

/// Exact fields selected from a re-read candidate by a durable import intent.
/// `quality`/`languages` retain upstream shape; they are not authorization.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportFile {
    pub path: String,
    pub download_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub movie_id: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub episode_ids: Vec<u64>,
    pub quality: Value,
    pub languages: Vec<Value>,
    pub release_group: String,
}

pub struct Client<T> {
    http: JsonClient<T>,
    kind: Kind,
}
impl<T: Transport> Client<T> {
    pub fn new(endpoint: Endpoint, transport: T, kind: Kind) -> Self {
        Self {
            http: JsonClient::new(endpoint, transport),
            kind,
        }
    }
    pub fn kind(&self) -> Kind {
        self.kind
    }
    pub async fn version(&self) -> Result<String, Error> {
        #[derive(Deserialize)]
        struct Status {
            version: String,
        }
        Ok(self
            .http
            .get::<Status>("api/v3/system/status", vec![])
            .await?
            .version)
    }
    /// Resolve the media item's current file; an old file record alone does not
    /// prove that the file is still installed for this item.
    pub async fn installed_file(&self, media_id: u64) -> Result<u64, Error> {
        positive(media_id)?;
        let (path, field) = match self.kind {
            Kind::Sonarr => ("episode", "episodeFileId"),
            Kind::Radarr => ("movie", "movieFileId"),
        };
        let value: Value = self
            .http
            .get(&format!("api/v3/{path}/{media_id}"), vec![])
            .await?;
        value
            .get(field)
            .and_then(Value::as_u64)
            .filter(|id| *id > 0)
            .ok_or(Error::InvalidResponse)
    }
    pub async fn queue(&self, page: Page) -> Result<PageResult<QueueRecord>, Error> {
        self.http.get("api/v3/queue", page.query()?).await
    }
    pub async fn history(&self, page: Page) -> Result<PageResult<HistoryRecord>, Error> {
        let mut query = page.query()?;
        query.extend([
            ("sortKey".into(), "date".into()),
            ("sortDirection".into(), "descending".into()),
        ]);
        self.http.get("api/v3/history", query).await
    }
    pub async fn file(&self, id: u64) -> Result<MediaFile, Error> {
        positive(id)?;
        self.http
            .get(&format!("api/v3/{}/{id}", self.kind.file_path()), vec![])
            .await
    }
    /// Interactive search consumes indexer quota even though its HTTP method is GET.
    /// Shadow polling must opt into searches separately; queue/history never search.
    pub async fn releases(&self, media_id: u64) -> Result<Vec<Release>, Error> {
        positive(media_id)?;
        self.http
            .get(
                "api/v3/release",
                vec![(self.kind.media_key().into(), media_id.to_string())],
            )
            .await
    }
    pub async fn manual_import_candidates(
        &self,
        download_id: &str,
    ) -> Result<Vec<ImportCandidate>, Error> {
        identifier(download_id)?;
        self.http
            .get(
                "api/v3/manualimport",
                vec![
                    ("downloadId".into(), download_id.into()),
                    ("filterExistingFiles".into(), "false".into()),
                ],
            )
            .await
    }
    pub async fn blocklist(&self, page: Page) -> Result<PageResult<BlocklistRecord>, Error> {
        self.http.get("api/v3/blocklist", page.query()?).await
    }

    pub async fn grab(
        &self,
        permit: &LivePermit,
        media_id: u64,
        guid: &str,
        indexer_id: u64,
    ) -> Result<Value, Error> {
        positive(media_id)?;
        positive(indexer_id)?;
        identifier(guid)?;
        let mut body = json!({"guid": guid, "indexerId": indexer_id});
        body[self.kind.media_key()] = json!(media_id);
        self.http
            .write(permit, Method::Post, "api/v3/release", vec![], Some(body))
            .await
    }
    pub async fn manual_import(
        &self,
        permit: &LivePermit,
        files: &[ImportFile],
    ) -> Result<Value, Error> {
        if files.is_empty() || files.len() > 100 {
            return Err(Error::InvalidInput);
        }
        for file in files {
            identifier(&file.download_id)?;
            if file.path.is_empty()
                || file.path.contains('\0')
                || file.path.len() > 4096
                || !file.quality.is_object()
            {
                return Err(Error::InvalidInput);
            }
            match self.kind {
                Kind::Sonarr
                    if file.series_id.is_some_and(|id| id > 0)
                        && file.movie_id.is_none()
                        && !file.episode_ids.is_empty()
                        && file.episode_ids.iter().all(|id| *id > 0) => {}
                Kind::Radarr
                    if file.movie_id.is_some_and(|id| id > 0)
                        && file.series_id.is_none()
                        && file.episode_ids.is_empty() => {}
                _ => return Err(Error::InvalidInput),
            }
        }
        self.http
            .write(
                permit,
                Method::Post,
                "api/v3/command",
                vec![],
                Some(json!({"name": "ManualImport", "importMode": "auto", "files": files})),
            )
            .await
    }
    /// Engine must prove a verified replacement exists before dispatching this.
    pub async fn delete_file(&self, permit: &LivePermit, file_id: u64) -> Result<Value, Error> {
        positive(file_id)?;
        self.http
            .write(
                permit,
                Method::Delete,
                &format!("api/v3/{}/{file_id}", self.kind.file_path()),
                vec![],
                None,
            )
            .await
    }
    /// Queue-only exclusion. Never mark history failed or ask arr to re-download.
    /// `removeFromClient=false` preserves downloaded source data for reconciliation.
    pub async fn blocklist_queue(
        &self,
        permit: &LivePermit,
        queue_id: u64,
    ) -> Result<Value, Error> {
        positive(queue_id)?;
        self.http
            .write(
                permit,
                Method::Delete,
                &format!("api/v3/queue/{queue_id}"),
                vec![
                    ("removeFromClient".into(), "false".into()),
                    ("blocklist".into(), "true".into()),
                    ("skipRedownload".into(), "true".into()),
                    ("changeCategory".into(), "false".into()),
                ],
                None,
            )
            .await
    }
}
fn positive(value: u64) -> Result<(), Error> {
    if value == 0 {
        Err(Error::InvalidInput)
    } else {
        Ok(())
    }
}
fn identifier(value: &str) -> Result<(), Error> {
    if value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control) {
        Err(Error::InvalidInput)
    } else {
        Ok(())
    }
}
