pub mod files;
pub mod text;

pub use files::{exts_for_project_type, pattern_match};
pub use text::{code_to_query, stacktrace_to_query};

/// Current UTC timestamp in milliseconds since UNIX epoch.
pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
