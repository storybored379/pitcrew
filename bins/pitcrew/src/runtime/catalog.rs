use super::*;
use pitcrew_sync::{ArrInventory, LibraryBinding, reader::Cursor};

impl<T: Transport> Runtime<T> {
    /// A failed or capped sweep leaves the prior expiring snapshot untouched.
    /// No partial sweep evicts old items or extends their observation time.
    pub async fn sync(&mut self) -> Result<(), &'static str> {
        let at = self.now_seconds();
        let expires = at.saturating_add(self.ttl);
        let sections = self
            .plex
            .sections()
            .await
            .map_err(|_| "catalog sections unavailable")?;
        let mut next = pitcrew_sync::CatalogSnapshot::default();
        for binding in &self.bindings {
            let section = sections
                .iter()
                .find(|s| s.key == binding.library_id)
                .ok_or("configured Plex library unavailable")?;
            let arr = self
                .arr
                .get(&binding.arr_instance_id)
                .ok_or("configured arr unavailable")?;
            let episodes = arr.client.kind() == pitcrew_arr::Kind::Sonarr;
            if section.kind != if episodes { "show" } else { "movie" } {
                return Err("Plex library type does not match configured arr");
            }
            let mut inventory = ArrInventory::default();
            if episodes {
                inventory.series = arr
                    .reader
                    .series()
                    .await
                    .map_err(|_| "series inventory unavailable")?;
                if inventory.series.len() > self.max_series {
                    return Err("series inventory exceeds runtime.max_series");
                }
                for series in &inventory.series {
                    inventory.episodes.extend(
                        arr.reader
                            .episodes(series.id)
                            .await
                            .map_err(|_| "episode inventory unavailable")?,
                    );
                    inventory.files.extend(
                        arr.reader
                            .episode_files(series.id)
                            .await
                            .map_err(|_| "file inventory unavailable")?,
                    );
                    if inventory.episodes.len() > 100_000 || inventory.files.len() > 100_000 {
                        return Err("series inventory exceeds row bound");
                    }
                }
            } else {
                inventory.movies = arr
                    .reader
                    .movies()
                    .await
                    .map_err(|_| "movie inventory unavailable")?;
            }
            let mut cursor = Cursor::default();
            let mut complete = false;
            for _ in 0..self.max_pages {
                let page = self
                    .plex
                    .plex_page(binding.section_id, episodes, cursor, 1000)
                    .await
                    .map_err(|_| "Plex catalog page unavailable")?;
                let named = LibraryBinding {
                    library_name: section.title.clone(),
                    ..clone_binding(binding)
                };
                next.merge_page(&named, &page, &inventory, at, expires)
                    .map_err(|_| "catalog mapping rejected")?;
                match page.next {
                    Some(next) => cursor = next,
                    None => {
                        complete = true;
                        break;
                    }
                }
            }
            if !complete {
                return Err("Plex catalog exceeds runtime.max_pages");
            }
        }
        if self.now_seconds() >= expires {
            return Err("catalog expired during sync");
        }
        export(&self.snapshot_path, &next)?;
        self.catalog = next;
        Ok(())
    }
}
fn clone_binding(b: &LibraryBinding) -> LibraryBinding {
    LibraryBinding {
        server_scope: b.server_scope.clone(),
        section_id: b.section_id,
        library_id: b.library_id.clone(),
        library_name: b.library_name.clone(),
        arr_instance_id: b.arr_instance_id.clone(),
        plex_root: b.plex_root.clone(),
        arr_root: b.arr_root.clone(),
        agent_root: b.agent_root.clone(),
    }
}

pub(super) fn export(
    path: &std::path::Path,
    catalog: &pitcrew_sync::CatalogSnapshot,
) -> Result<(), &'static str> {
    use std::io::Write;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent)
        .map_err(|_| "catalog export directory unavailable")?;
    serde_json::to_writer(file.as_file_mut(), catalog).map_err(|_| "catalog export failed")?;
    file.flush().map_err(|_| "catalog export failed")?;
    file.as_file()
        .sync_all()
        .map_err(|_| "catalog export failed")?;
    file.persist(path).map_err(|_| "catalog export failed")?;
    std::fs::File::open(parent)
        .and_then(|dir| dir.sync_all())
        .map_err(|_| "catalog directory sync failed")?;
    Ok(())
}
