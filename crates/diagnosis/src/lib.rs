//! Evidence-based diagnosis. Decisions are recommendations, never authorization
//! to mutate a library; the engine must enforce access, budget and exact-file rails.
pub mod collectors;
pub mod facts;
pub mod rules;
pub mod selection;
pub mod triage;
