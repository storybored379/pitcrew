//! Version-specific reads through the shared bounded transport. This adapter has
//! no write methods, refresh endpoints, retries, or autonomous scan loop.
use crate::Error;
use pitcrew_upstream::{Endpoint, JsonClient, Transport};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Section {
    pub key: String,
    pub title: String,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlexItem {
    #[serde(rename = "ratingKey", deserialize_with = "identifier")]
    pub rating_key: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub title: String,
    #[serde(rename = "grandparentRatingKey")]
    pub series_key: Option<String>,
    #[serde(rename = "grandparentTitle")]
    pub series_title: Option<String>,
    #[serde(rename = "parentIndex")]
    pub season: Option<u32>,
    #[serde(rename = "index")]
    pub episode: Option<u32>,
    #[serde(default, rename = "Media")]
    pub media: Vec<PlexMedia>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlexMedia {
    #[serde(rename = "videoResolution")]
    pub resolution: Option<String>,
    #[serde(default, rename = "Part")]
    pub parts: Vec<PlexPart>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlexPart {
    pub file: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Series {
    pub id: u64,
    pub title: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Episode {
    pub id: u64,
    pub series_id: u64,
    pub season_number: u32,
    pub episode_number: u32,
    pub episode_file_id: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Movie {
    pub id: u64,
    pub title: String,
    pub movie_file: Option<ArrFile>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArrFile {
    pub id: u64,
    pub path: String,
    pub series_id: Option<u64>,
    pub movie_id: Option<u64>,
}

/// Persist this cursor only after merging its page. A timestamp filter narrows
/// incremental reads; callers also run bounded full sweeps to observe deletions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub offset: u64,
    pub updated_since: Option<u64>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlexPage {
    pub items: Vec<PlexItem>,
    pub next: Option<Cursor>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct HistoryItem {
    #[serde(rename = "ratingKey")]
    pub rating_key: String,
    #[serde(rename = "librarySectionID", deserialize_with = "identifier")]
    pub section_id: String,
    #[serde(rename = "accountID", deserialize_with = "identifier")]
    pub account_id: String,
    #[serde(rename = "viewedAt")]
    pub at: u64,
}

#[derive(Deserialize)]
struct Envelope<T> {
    #[serde(rename = "MediaContainer")]
    container: T,
}
#[derive(Deserialize)]
struct RawPage {
    size: usize,
    offset: u64,
    #[serde(rename = "totalSize")]
    total: u64,
    #[serde(default, rename = "Metadata")]
    items: Vec<PlexItem>,
}

pub struct Reader<T> {
    http: JsonClient<T>,
    max_rows: usize,
}
impl<T: Transport> Reader<T> {
    pub fn new(endpoint: Endpoint, transport: T, max_rows: usize) -> Result<Self, Error> {
        if !(1..=100_000).contains(&max_rows) {
            return Err(Error::Bound);
        }
        Ok(Self {
            http: JsonClient::new(endpoint, transport),
            max_rows,
        })
    }

    pub async fn sections(&self) -> Result<Vec<Section>, Error> {
        #[derive(Deserialize)]
        struct Sections {
            size: usize,
            #[serde(default, rename = "Directory")]
            entries: Vec<Section>,
        }
        let result: Envelope<Sections> = self
            .http
            .get("library/sections", vec![])
            .await
            .map_err(|_| Error::Upstream)?;
        if result.container.size != result.container.entries.len() {
            return Err(Error::Malformed);
        }
        self.bound(result.container.entries)
    }

    pub async fn plex_page(
        &self,
        section: u64,
        episodes: bool,
        cursor: Cursor,
        size: u16,
    ) -> Result<PlexPage, Error> {
        if section == 0 || !(1..=1000).contains(&size) {
            return Err(Error::Malformed);
        }
        let mut query = vec![
            ("type".into(), if episodes { "4" } else { "1" }.into()),
            ("X-Plex-Container-Start".into(), cursor.offset.to_string()),
            ("X-Plex-Container-Size".into(), size.to_string()),
            // Plex answers 500 to `sort=ratingKey`; `id` is the same key and is accepted.
            ("sort".into(), "id:asc".into()),
        ];
        if let Some(since) = cursor.updated_since {
            query.push(("updatedAt>=".into(), since.to_string()));
        }
        let result: Envelope<RawPage> = self
            .http
            .get(&format!("library/sections/{section}/all"), query)
            .await
            .map_err(|_| Error::Upstream)?;
        let page = result.container;
        let end = cursor
            .offset
            .checked_add(page.size as u64)
            .ok_or(Error::Bound)?;
        if page.offset != cursor.offset
            || page.size != page.items.len()
            || page.size > usize::from(size)
            || end > page.total
            || (page.size == 0 && end < page.total)
        {
            return Err(Error::Malformed);
        }
        Ok(PlexPage {
            items: self.bound(page.items)?,
            next: (end < page.total).then_some(Cursor {
                offset: end,
                ..cursor
            }),
        })
    }

    pub async fn series(&self) -> Result<Vec<Series>, Error> {
        self.list("api/v3/series", vec![]).await
    }
    pub async fn episodes(&self, series: u64) -> Result<Vec<Episode>, Error> {
        self.list("api/v3/episode", id_query("seriesId", series)?)
            .await
    }
    pub async fn episode_files(&self, series: u64) -> Result<Vec<ArrFile>, Error> {
        self.list("api/v3/episodefile", id_query("seriesId", series)?)
            .await
    }
    pub async fn movies(&self) -> Result<Vec<Movie>, Error> {
        self.list("api/v3/movie", vec![]).await
    }

    /// Plex's server-local playback history. The viewer's authenticated account
    /// is supplied by the host; response filtering is also checked locally.
    pub async fn recent_plays(
        &self,
        account: u64,
        start: u64,
        size: u16,
    ) -> Result<Vec<HistoryItem>, Error> {
        if account == 0 || !(1..=1000).contains(&size) {
            return Err(Error::Malformed);
        }
        #[derive(Deserialize)]
        struct History {
            size: usize,
            #[serde(default, rename = "Metadata")]
            entries: Vec<HistoryItem>,
        }
        let response: Envelope<History> = self
            .http
            .get(
                "status/sessions/history/all",
                vec![
                    ("accountID".into(), account.to_string()),
                    ("X-Plex-Container-Start".into(), start.to_string()),
                    ("X-Plex-Container-Size".into(), size.to_string()),
                    ("sort".into(), "viewedAt:desc".into()),
                ],
            )
            .await
            .map_err(|_| Error::Upstream)?;
        let rows = response.container;
        if rows.size != rows.entries.len()
            || rows.size > usize::from(size)
            || rows
                .entries
                .iter()
                .any(|r| r.account_id != account.to_string())
        {
            return Err(Error::Malformed);
        }
        self.bound(rows.entries)
    }

    /// Tautulli's bounded history page. Tautulli account IDs must be supplied by
    /// a trusted Plex-to-Tautulli identity mapping, never viewer text.
    pub async fn tautulli_recent(
        &self,
        api_key: &str,
        user_id: u64,
        start: u64,
        size: u16,
    ) -> Result<Vec<HistoryItem>, Error> {
        pitcrew_upstream::Auth::arr(api_key).map_err(|_| Error::Malformed)?;
        if user_id == 0 || !(1..=1000).contains(&size) {
            return Err(Error::Malformed);
        }
        #[derive(Deserialize)]
        struct Outer {
            response: Body,
        }
        #[derive(Deserialize)]
        struct Body {
            result: String,
            data: Data,
        }
        #[derive(Deserialize)]
        struct Data {
            data: Vec<Row>,
        }
        #[derive(Deserialize)]
        struct Row {
            #[serde(deserialize_with = "identifier")]
            rating_key: String,
            #[serde(deserialize_with = "identifier")]
            section_id: String,
            #[serde(deserialize_with = "identifier")]
            user_id: String,
            date: u64,
        }
        let response: Outer = self
            .http
            .get(
                "api/v2",
                vec![
                    ("apikey".into(), api_key.into()),
                    ("cmd".into(), "get_history".into()),
                    ("user_id".into(), user_id.to_string()),
                    ("start".into(), start.to_string()),
                    ("length".into(), size.to_string()),
                ],
            )
            .await
            .map_err(|_| Error::Upstream)?;
        if response.response.result != "success"
            || response.response.data.data.len() > usize::from(size)
        {
            return Err(Error::Malformed);
        }
        let rows = self.bound(response.response.data.data)?;
        if rows.iter().any(|r| r.user_id != user_id.to_string()) {
            return Err(Error::Malformed);
        }
        Ok(rows
            .into_iter()
            .map(|r| HistoryItem {
                rating_key: r.rating_key,
                section_id: r.section_id,
                account_id: r.user_id,
                at: r.date,
            })
            .collect())
    }
    pub async fn movie_files(&self, movie: u64) -> Result<Vec<ArrFile>, Error> {
        self.list("api/v3/moviefile", id_query("movieId", movie)?)
            .await
    }
    async fn list<R: DeserializeOwned>(
        &self,
        path: &str,
        query: Vec<(String, String)>,
    ) -> Result<Vec<R>, Error> {
        let rows = self
            .http
            .get(path, query)
            .await
            .map_err(|_| Error::Upstream)?;
        self.bound(rows)
    }
    fn bound<R>(&self, rows: Vec<R>) -> Result<Vec<R>, Error> {
        if rows.len() > self.max_rows {
            return Err(Error::Bound);
        }
        Ok(rows)
    }
}
fn identifier<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Id {
        Text(String),
        Number(u64),
    }
    Ok(match Id::deserialize(deserializer)? {
        Id::Text(s) => s,
        Id::Number(n) => n.to_string(),
    })
}
fn id_query(key: &str, id: u64) -> Result<Vec<(String, String)>, Error> {
    if id == 0 {
        return Err(Error::Malformed);
    }
    Ok(vec![(key.into(), id.to_string())])
}
