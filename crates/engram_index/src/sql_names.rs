//! Shared object-name syntax for the existing heuristic SQL analyzers.
//! Captures the final identifier, preserving their basename-based contracts.
//! This does not bind a name to a deployed database/schema or resolve aliases.

pub const QUALIFIED_OBJECT_NAME: &str = r"(?:\[?\w+\]?\s*\.\s*)*(\[?\w+\]?)";

pub fn object_basename(name: &str) -> &str {
    name.trim().rsplit('.').next().unwrap_or(name).trim().trim_start_matches('[').trim_end_matches(']')
}
