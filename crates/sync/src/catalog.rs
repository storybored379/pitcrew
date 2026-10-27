use crate::{AccessSnapshot, Error, reader::*};
use pitcrew_intake::{
    CatalogItem, Resolution,
    pipeline::{CatalogResolver, ResolveError, ResolvedTarget},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PickerKind {
    RecentPlay,
    Watchlist,
}
/// Trusted, account-scoped upstream observation. Never accepted from viewer text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickerEntry {
    pub account_id: String,
    pub item_id: String,
    pub kind: PickerKind,
    pub at: u64,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogRecord {
    pub item: CatalogItem,
    pub plex_rating_key: String,
    pub series_id: Option<String>,
    pub target: Option<ResolvedTarget>,
    pub observed_at: u64,
    pub expires_at: u64,
}
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogSnapshot {
    records: BTreeMap<String, CatalogRecord>,
    #[serde(default)]
    watermarks: BTreeMap<String, u64>,
    #[serde(default)]
    activity: Vec<PickerEntry>,
}
/// Explicit operator mapping; never infer library ownership from a title.
pub struct LibraryBinding {
    pub server_scope: String,
    pub section_id: u64,
    pub library_id: String,
    pub library_name: String,
    pub arr_instance_id: String,
    pub plex_root: String,
    pub arr_root: String,
    pub agent_root: String,
}
#[derive(Clone, Default)]
pub struct ArrInventory {
    pub series: Vec<Series>,
    pub episodes: Vec<Episode>,
    pub movies: Vec<Movie>,
    pub files: Vec<ArrFile>,
}

impl CatalogSnapshot {
    /// Upsert one bounded page atomically. Missing/ambiguous arr files remove an
    /// old target on that page; partial pages never evict unrelated catalog rows.
    /// Full sweeps must call retain_seen after their final successful page.
    pub fn merge_page(
        &mut self,
        binding: &LibraryBinding,
        page: &PlexPage,
        arr: &ArrInventory,
        observed_at: u64,
        expires_at: u64,
    ) -> Result<(), Error> {
        if page.items.len() > 1000
            || arr.series.len() > 100_000
            || arr.episodes.len() > 100_000
            || arr.movies.len() > 100_000
            || arr.files.len() > 100_000
        {
            return Err(Error::Bound);
        }
        if observed_at >= expires_at
            || binding.section_id == 0
            || binding.library_id.is_empty()
            || !scope(&binding.server_scope)
            || !scope(&binding.arr_instance_id)
        {
            return Err(Error::Malformed);
        }
        if self
            .watermarks
            .get(&binding.library_id)
            .is_some_and(|at| observed_at < *at)
        {
            return Err(Error::Unavailable);
        }
        let duplicates = Duplicates {
            movies: duplicate_ids(arr.movies.iter().map(|m| m.id)),
            movie_files: duplicate_ids(
                arr.movies
                    .iter()
                    .filter_map(|m| m.movie_file.as_ref().map(|f| f.id)),
            ),
            episodes: duplicate_ids(arr.episodes.iter().map(|e| e.id)),
            shared_episode_files: duplicate_ids(
                arr.episodes
                    .iter()
                    .map(|e| (e.series_id, e.episode_file_id)),
            ),
            files: duplicate_ids(arr.files.iter().map(|f| f.id)),
        };
        let mut updates = BTreeMap::new();
        for plex in &page.items {
            let record = build_record(binding, plex, arr, &duplicates, observed_at, expires_at)?;
            if updates.insert(record.item.id.clone(), record).is_some() {
                return Err(Error::Ambiguous);
            }
        }
        if self
            .records
            .keys()
            .chain(updates.keys())
            .collect::<BTreeSet<_>>()
            .len()
            > 100_000
        {
            return Err(Error::Bound);
        }
        self.records.extend(updates);
        self.watermarks
            .insert(binding.library_id.clone(), observed_at);
        Ok(())
    }

    pub fn items(&self, access: &AccessSnapshot, now: u64) -> Vec<CatalogItem> {
        let Some(viewer) = access.authorize(now) else {
            return Vec::new();
        };
        self.records
            .values()
            .filter(|r| fresh(r, now) && viewer.library_ids.contains(&r.item.library_id))
            .map(|r| r.item.clone())
            .collect()
    }

    pub fn series_id(&self, item_id: &str) -> Option<&str> {
        self.records.get(item_id)?.series_id.as_deref()
    }

    /// Replaces a complete bounded activity snapshot. This is not authorization.
    pub fn set_activity(&mut self, activity: Vec<PickerEntry>) -> Result<(), Error> {
        if activity.len() > 10_000 {
            return Err(Error::Bound);
        }
        self.activity = activity;
        Ok(())
    }

    pub fn picker(
        &self,
        account_id: &str,
        kind: PickerKind,
        access: &AccessSnapshot,
        now: u64,
        limit: usize,
    ) -> Vec<CatalogItem> {
        let Some(viewer) = access.authorize(now).filter(|v| v.account_id == account_id) else {
            return Vec::new();
        };
        let mut activity: Vec<_> = self
            .activity
            .iter()
            .filter(|a| a.account_id == account_id && a.kind == kind && a.at <= now)
            .collect();
        activity.sort_by_key(|a| (std::cmp::Reverse(a.at), &a.item_id));
        let mut seen = BTreeSet::new();
        activity
            .into_iter()
            .filter_map(|a| {
                let r = self.records.get(&a.item_id)?;
                (fresh(r, now)
                    && viewer.library_ids.contains(&r.item.library_id)
                    && seen.insert(&r.item.id))
                .then(|| r.item.clone())
            })
            .take(limit.min(100))
            .collect()
    }

    /// Call only after a complete successful full sweep of this library; a
    /// timeout, page cap or incremental timestamp sweep must never reach here.
    pub fn retain_seen(&mut self, library_id: &str, seen: &BTreeSet<String>) {
        self.records
            .retain(|id, r| r.item.library_id != library_id || seen.contains(id));
    }
}
impl CatalogResolver for CatalogSnapshot {
    fn resolve(&self, item_id: &str, now: u64) -> Result<ResolvedTarget, ResolveError> {
        let record = self
            .records
            .get(item_id)
            .filter(|r| fresh(r, now))
            .ok_or(ResolveError::Unavailable)?;
        record.target.clone().ok_or(ResolveError::Unavailable)
    }
}
fn fresh(record: &CatalogRecord, now: u64) -> bool {
    record.observed_at <= now && now < record.expires_at
}
fn scope(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}
fn numeric(s: &str) -> bool {
    s.parse::<u64>()
        .is_ok_and(|id| id > 0 && id.to_string() == s)
}

struct Duplicates {
    movies: BTreeSet<u64>,
    movie_files: BTreeSet<u64>,
    episodes: BTreeSet<u64>,
    shared_episode_files: BTreeSet<(u64, u64)>,
    files: BTreeSet<u64>,
}
fn duplicate_ids<T: Ord + Copy>(ids: impl Iterator<Item = T>) -> BTreeSet<T> {
    let mut seen = BTreeSet::new();
    ids.filter(|id| !seen.insert(*id)).collect()
}

fn build_record(
    b: &LibraryBinding,
    p: &PlexItem,
    arr: &ArrInventory,
    duplicates: &Duplicates,
    observed_at: u64,
    expires_at: u64,
) -> Result<CatalogRecord, Error> {
    if !numeric(&p.rating_key) || !matches!(p.kind.as_str(), "movie" | "episode") {
        return Err(Error::Malformed);
    }
    let series_id = if p.kind == "episode" {
        let key = p
            .series_key
            .as_deref()
            .filter(|v| numeric(v))
            .ok_or(Error::Malformed)?;
        if p.season.is_none() || p.episode.is_none() {
            return Err(Error::Malformed);
        }
        Some(format!("plex:{}:{}:{key}", b.server_scope, b.section_id))
    } else {
        None
    };
    let item = CatalogItem {
        id: format!("plex:{}:{}:{}", b.server_scope, b.section_id, p.rating_key),
        title: if p.kind == "episode" {
            p.series_title.clone().ok_or(Error::Malformed)?
        } else {
            p.title.clone()
        },
        aliases: Vec::new(),
        library_id: b.library_id.clone(),
        library_name: b.library_name.clone(),
        resolution: if p
            .media
            .iter()
            .any(|m| m.resolution.as_deref() == Some("4k"))
        {
            Resolution::Uhd
        } else {
            Resolution::Hd
        },
        season: if p.kind == "episode" { p.season } else { None },
        episode: if p.kind == "episode" { p.episode } else { None },
    };
    let mut record = CatalogRecord {
        item,
        plex_rating_key: p.rating_key.clone(),
        series_id,
        target: None,
        observed_at,
        expires_at,
    };
    // Multiple versions/parts cannot be reduced to a single mutable target.
    let [media] = p.media.as_slice() else {
        return Ok(record);
    };
    let [part] = media.parts.as_slice() else {
        return Ok(record);
    };
    let Some(path) = &part.file else {
        return Ok(record);
    };
    let relative = relative_path(path, &b.plex_root)?;
    let arr_path = join_root(&b.arr_root, relative)?;
    let agent_path = join_root(&b.agent_root, relative)?;
    let candidates: Vec<_> = if p.kind == "movie" {
        arr.movies
            .iter()
            .filter_map(|m| {
                let f = m.movie_file.as_ref()?;
                (m.id > 0
                    && !duplicates.movies.contains(&m.id)
                    && f.id > 0
                    && !duplicates.movie_files.contains(&f.id)
                    && f.movie_id.is_none_or(|id| id == m.id)
                    && f.path == arr_path)
                    .then_some((m.id, f.id))
            })
            .collect()
    } else {
        let matching_files: Vec<_> = arr
            .files
            .iter()
            .filter(|f| f.path == arr_path && !duplicates.files.contains(&f.id))
            .collect();
        arr.episodes
            .iter()
            .filter(|e| Some(e.season_number) == p.season && Some(e.episode_number) == p.episode)
            .filter_map(|e| {
                let matching: Vec<_> = matching_files
                    .iter()
                    .filter(|f| f.id == e.episode_file_id && f.series_id == Some(e.series_id))
                    .collect();
                (e.id > 0
                    && !duplicates.episodes.contains(&e.id)
                    && !duplicates
                        .shared_episode_files
                        .contains(&(e.series_id, e.episode_file_id))
                    && e.episode_file_id > 0
                    && e.series_id > 0
                    && matching.len() == 1
                    && arr.series.iter().filter(|s| s.id == e.series_id).count() == 1)
                    .then_some((e.id, e.episode_file_id))
            })
            .collect()
    };
    if let [(item_id, file_id)] = candidates.as_slice() {
        record.target = Some(ResolvedTarget {
            item: record.item.clone(),
            resource: format!("arr:{}:{}:{item_id}", b.arr_instance_id, p.kind),
            arr_instance_id: b.arr_instance_id.clone(),
            arr_item_id: *item_id,
            arr_file_id: *file_id,
            agent_path,
            agent_file: None,
            plex_rating_key: p.rating_key.clone(),
            observed_at,
            expires_at,
        });
    }
    Ok(record)
}
fn relative_path<'a>(path: &'a str, root: &str) -> Result<&'a Path, Error> {
    checked_root(root)?;
    let path = Path::new(path);
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(Error::Malformed);
    }
    path.strip_prefix(root)
        .ok()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or(Error::Malformed)
}
fn join_root(root: &str, relative: &Path) -> Result<String, Error> {
    checked_root(root)?;
    Ok(Path::new(root)
        .join(relative)
        .to_string_lossy()
        .into_owned())
}
fn checked_root(root: &str) -> Result<(), Error> {
    let root = Path::new(root);
    if !root.is_absolute() || root.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(Error::Malformed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn total_cap_accepts_updates_but_rejects_growth_atomically() {
        let record = CatalogRecord {
            item: CatalogItem {
                id: String::new(),
                title: String::new(),
                aliases: vec![],
                library_id: "2".into(),
                library_name: String::new(),
                resolution: Resolution::Hd,
                season: None,
                episode: None,
            },
            plex_rating_key: String::new(),
            series_id: None,
            target: None,
            observed_at: 100,
            expires_at: 200,
        };
        let records = (1..=100_000)
            .map(|i| {
                let mut r = record.clone();
                r.item.id = format!("plex:primary:2:{i}");
                r.plex_rating_key = i.to_string();
                (r.item.id.clone(), r)
            })
            .collect();
        let mut snapshot = CatalogSnapshot {
            records,
            ..CatalogSnapshot::default()
        };
        let binding = LibraryBinding {
            server_scope: "primary".into(),
            section_id: 2,
            library_id: "2".into(),
            library_name: String::new(),
            arr_instance_id: "arr".into(),
            plex_root: "/plex".into(),
            arr_root: "/arr".into(),
            agent_root: "/agent".into(),
        };
        let mut page = PlexPage {
            items: vec![PlexItem {
                rating_key: "1".into(),
                kind: "movie".into(),
                title: String::new(),
                series_key: None,
                series_title: None,
                season: None,
                episode: None,
                media: vec![],
            }],
            next: None,
        };
        assert!(
            snapshot
                .merge_page(&binding, &page, &ArrInventory::default(), 100, 200)
                .is_ok()
        );
        page.items[0].rating_key = "100001".into();
        assert_eq!(
            snapshot.merge_page(&binding, &page, &ArrInventory::default(), 150, 250),
            Err(Error::Bound)
        );
        assert_eq!(snapshot.records.len(), 100_000);
        assert_eq!(snapshot.watermarks.get("2"), Some(&100));
    }
}
