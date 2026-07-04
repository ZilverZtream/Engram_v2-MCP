//! P0-4/P0-5: standard one-line trailer for read-tool responses.
//!
//! Every hot read tool appends this footer so agents always know which
//! generation served the answer and how old the index is — staleness is
//! otherwise invisible until the answers are wrong. Keep it to a single
//! line: it is paid on every tool call.

/// Render a duration in compact form: `42s`, `17m`, `3h`, `5d`.
fn age_str(age_ms: u64) -> String {
    let secs = age_ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// One-line, token-cheap trailer: generation + index age + freshness hint.
///
/// `last_index_ms` is the `last_index_completed_ms` registry meta value;
/// `None` means the project predates freshness tracking (or never indexed).
pub fn footer(generation: u64, last_index_ms: Option<u64>) -> String {
    let indexed = match last_index_ms {
        Some(ms) => {
            let now = crate::utils::now_ms();
            format!("indexed {} ago", age_str(now.saturating_sub(ms)))
        }
        None => "index age unknown".to_string(),
    };
    format!("\n---\n[engram] gen={generation} | {indexed} | stale? call get_index_freshness\n")
}

/// Prominent per-file drift warning for tools that read source from disk by
/// GRAPH line numbers (the access layer). The wall-clock footer alone can't
/// catch this: "indexed 2m ago" looks fine even when the agent edited the
/// file 10 seconds ago and every line range in the graph is now shifted.
/// Returns `None` when the file has not changed since the last index (or
/// when either timestamp is unknown — no false alarms on missing data).
pub fn stale_file_banner(
    rel_path: &str,
    file_mtime_ms: Option<u64>,
    last_index_ms: Option<u64>,
) -> Option<String> {
    let (mtime, indexed) = (file_mtime_ms?, last_index_ms?);
    if mtime <= indexed {
        return None;
    }
    let now = crate::utils::now_ms();
    Some(format!(
        "⚠ STALE FILE: `{rel_path}` was modified {} ago — AFTER the last index ({} ago). \
         Line numbers and bodies below may be shifted. Run update_project (or wait for \
         the watcher) before trusting exact line ranges.\n\n",
        age_str(now.saturating_sub(mtime)),
        age_str(now.saturating_sub(indexed)),
    ))
}

/// Millisecond mtime of a file, when obtainable.
pub fn file_mtime_ms(path: &std::path::Path) -> Option<u64> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn age_rendering_boundaries() {
        assert_eq!(age_str(0), "0s");
        assert_eq!(age_str(59_000), "59s");
        assert_eq!(age_str(61_000), "1m");
        assert_eq!(age_str(3_599_000), "59m");
        assert_eq!(age_str(3_600_000), "1h");
        assert_eq!(age_str(82_800_000), "23h");
        assert_eq!(age_str(86_400_000), "1d");
        assert_eq!(age_str(200_000_000), "2d");
    }

    #[test]
    fn footer_with_known_age_names_freshness_tool() {
        let f = footer(42, Some(crate::utils::now_ms().saturating_sub(120_000)));
        assert!(f.contains("gen=42"), "footer must carry generation: {f}");
        assert!(f.contains("indexed 2m ago"), "footer must carry age: {f}");
        assert!(
            f.contains("get_index_freshness"),
            "footer must name the freshness tool: {f}"
        );
        assert_eq!(f.lines().count(), 3, "footer must stay compact: {f:?}");
    }

    #[test]
    fn footer_without_metadata_is_honest() {
        let f = footer(1, None);
        assert!(f.contains("index age unknown"), "{f}");
    }

    #[test]
    fn stale_banner_fires_only_when_file_newer_than_index() {
        let now = crate::utils::now_ms();
        // File edited after the index → banner.
        let b = stale_file_banner("a/b.vb", Some(now - 5_000), Some(now - 60_000));
        let b = b.expect("must warn when file is newer than index");
        assert!(b.contains("STALE FILE"), "{b}");
        assert!(b.contains("a/b.vb"), "{b}");
        // File older than the index → silent.
        assert!(stale_file_banner("a/b.vb", Some(now - 60_000), Some(now - 5_000)).is_none());
        // Unknown timestamps → silent (no false alarms).
        assert!(stale_file_banner("a/b.vb", None, Some(now)).is_none());
        assert!(stale_file_banner("a/b.vb", Some(now), None).is_none());
    }
}
