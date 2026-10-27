//! Reads only the configured local sync export. A missing or invalid export never
//! falls back to a stale static catalog.
use pitcrew_intake::{AuthorizedViewer, CatalogItem};
use pitcrew_sync::{AccessSnapshot, CatalogSnapshot, PickerKind};
use tokio::io::AsyncReadExt;

use crate::WebError;

const MAX_SNAPSHOT_BYTES: usize = 16_777_216;
// Read one overflow byte so a syntactically complete but oversized export fails.
const MAX_SNAPSHOT_READ: u64 = 16_777_217;

pub(super) struct ViewerCatalog {
    pub(super) items: Vec<CatalogItem>,
    pub(super) recent: Vec<CatalogItem>,
    pub(super) watchlist: Vec<CatalogItem>,
    snapshot: Option<CatalogSnapshot>,
}
impl ViewerCatalog {
    pub(super) async fn load(
        path: Option<&str>,
        fallback: &[CatalogItem],
        viewer: &AuthorizedViewer,
        now: u64,
    ) -> Result<Self, WebError> {
        let Some(path) = path else {
            return Ok(Self {
                items: fallback
                    .iter()
                    .filter(|item| viewer.library_ids.contains(&item.library_id))
                    .cloned()
                    .collect(),
                recent: Vec::new(),
                watchlist: Vec::new(),
                snapshot: None,
            });
        };
        // The operator controls the path. Bound reads even if its writer fails
        // to honor the sync contract; errors never echo paths or private rows.
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|_| WebError::Catalog)?;
        let mut data = Vec::new();
        file.take(MAX_SNAPSHOT_READ)
            .read_to_end(&mut data)
            .await
            .map_err(|_| WebError::Catalog)?;
        if data.len() > MAX_SNAPSHOT_BYTES {
            return Err(WebError::Catalog);
        }
        let snapshot: CatalogSnapshot =
            serde_json::from_slice(&data).map_err(|_| WebError::Catalog)?;
        let access = AccessSnapshot::from_grants(
            viewer.account_id.clone(),
            viewer.library_ids.clone(),
            now,
            now.saturating_add(1),
        );
        Ok(Self {
            items: snapshot.items(&access, now),
            recent: snapshot.picker(&viewer.account_id, PickerKind::RecentPlay, &access, now, 20),
            watchlist: snapshot.picker(&viewer.account_id, PickerKind::Watchlist, &access, now, 20),
            snapshot: Some(snapshot),
        })
    }
    pub(super) fn series<'a>(&'a self, item: &'a CatalogItem) -> &'a str {
        self.snapshot
            .as_ref()
            .and_then(|s| s.series_id(&item.id))
            .unwrap_or(&item.title)
    }
}
