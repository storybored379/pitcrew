//! Shadow-only downgrade proposals; an absent floor preserves existing behavior.
use serde::Deserialize;
use std::num::NonZeroU32;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LowerQuality {
    pub minimum_resolution: Option<NonZeroU32>,
}
