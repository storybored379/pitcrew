//! Read-only, bounded catalog observations. Snapshots contain private local data;
//! they are never cloud evidence and must be stored behind the host's auth gate.
pub mod access;
pub mod catalog;
pub mod reader;
pub use access::AccessSnapshot;
pub use catalog::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("catalog bound exceeded")]
    Bound,
    #[error("invalid catalog data")]
    Malformed,
    #[error("ambiguous catalog mapping")]
    Ambiguous,
    #[error("catalog target unavailable")]
    Unavailable,
    #[error("catalog access denied")]
    Denied,
    #[error("catalog upstream unavailable")]
    Upstream,
}
