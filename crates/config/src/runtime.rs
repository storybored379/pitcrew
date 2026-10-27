//! Bounded control-plane scheduling. These settings never enable mutations.
use crate::{ConfigError, invalid};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Runtime {
    /// Explicit opt-in: interactive release searches spend indexer quota even in shadow.
    pub search_enabled: bool,
    pub sync_seconds: u64,
    pub snapshot_ttl_seconds: u64,
    pub batch_size: u32,
    pub max_pages: u32,
    pub max_series: usize,
    pub libraries: Vec<LibraryPaths>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryPaths {
    pub library_id: String,
    pub plex_root: String,
    pub arr_root: String,
    pub agent_root: String,
}
impl Default for Runtime {
    fn default() -> Self {
        Self {
            search_enabled: false,
            sync_seconds: 300,
            snapshot_ttl_seconds: 900,
            batch_size: 10,
            max_pages: 100,
            max_series: 1000,
            libraries: Vec::new(),
        }
    }
}
impl Runtime {
    pub(crate) fn validate(&self) -> Result<(), ConfigError> {
        if !(10..=3600).contains(&self.sync_seconds)
            || !(30..=7200).contains(&self.snapshot_ttl_seconds)
            || !(1..=100).contains(&self.batch_size)
            || !(1..=1000).contains(&self.max_pages)
            || !(1..=10_000).contains(&self.max_series)
        {
            return Err(invalid("runtime", "scheduler bounds are invalid"));
        }
        let mut seen = std::collections::HashSet::new();
        for paths in &self.libraries {
            if paths.library_id.is_empty() || !seen.insert(&paths.library_id) {
                return Err(invalid(
                    "runtime.libraries",
                    "library IDs must be unique and nonempty",
                ));
            }
            for root in [&paths.plex_root, &paths.arr_root, &paths.agent_root] {
                let path = std::path::Path::new(root);
                if !path.is_absolute()
                    || path
                        .components()
                        .any(|p| p == std::path::Component::ParentDir)
                {
                    return Err(invalid(
                        "runtime.libraries",
                        "roots must be absolute without parent traversal",
                    ));
                }
            }
        }
        Ok(())
    }
}
