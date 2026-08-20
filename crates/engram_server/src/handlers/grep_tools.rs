//! `grep_project` — fast literal / regex search over the indexed
//! file set. Uses the existing Tantivy trigram index as a prefilter
//! so we scan bytes only for chunks that could contain the literal.
//!
//! Design goal: beat `rg` on warm queries across every literal / regex
//! class. The index is already built, loaded, and hot; not using it is
//! a failure of imagination.

use std::path::PathBuf;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};

use crate::handlers::validate_project_id;
use crate::models::requests::GrepProjectRequest;
use crate::services::project_service::ensure_project_record;
use crate::tools::Engram;

/// Read every indexed file's recorded (size, mtime) from the code graph.
///
/// ingest writes these onto file nodes as
/// `{"mtime": <unix secs>, "size": <bytes>, "file_hash": <blake3>}`; the
/// incremental change scan reads the same three keys to decide what to
/// re-index. Anchoring the freshness guard here means "stale" and "an
/// update would pick this up" are the same statement by construction.
///
/// A node with no fingerprint metadata yields (0, 0), which
/// `check_freshness` skips rather than reporting as drift.
fn indexed_file_stats(
    graph: &engram_graph::GraphStore,
    project_id: &str,
) -> anyhow::Result<Vec<engram_index::grep::IndexedFileStat>> {
    Ok(graph
        .list_file_node_metadata(project_id)?
        .into_iter()
        .map(|(rel_path, meta)| {
            let get = |key: &str| {
                meta.as_ref()
                    .and_then(|m| m.get(key))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0)
            };
            engram_index::grep::IndexedFileStat {
                rel_path: rel_path.as_str().to_string(),
                size: get("size"),
                mtime_secs: get("mtime"),
            }
        })
        .collect())
}

impl Engram {
    pub async fn handle_grep_project(
        &self,
        req: GrepProjectRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let rec = ensure_project_record(&self.state, &req.project_id)
            .await
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        let project_dir = PathBuf::from(rec.directory.clone());

        // Ensure the project runtime is open (keeps the HybridSearchEngine
        // warm across calls — the whole point of this tool).
        let ps = self
            .ensure_project_runtime(&req.project_id)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let generation = self
            .get_active_generation(&req.project_id)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // Translate the request's freshness string into the engine
        // enum — fail closed on unknown values so typos don't silently
        // disable the correctness guard.
        let freshness = match req.freshness.to_ascii_lowercase().as_str() {
            "strict" => engram_index::grep::FreshnessMode::Strict,
            "warn" => engram_index::grep::FreshnessMode::Warn,
            "off" => engram_index::grep::FreshnessMode::Off,
            other => {
                return Err(McpError::invalid_params(
                    format!(
                        "grep_project: invalid freshness mode '{other}'. Expected one of: strict, warn, off"
                    ),
                    None,
                ));
            }
        };

        // Fail closed on unknown namespaces too — a typo'd namespace
        // previously returned 0 matches SILENTLY (knowledge-pack pilot
        // 2026-07-06: "code"/"source"/"files"/"project" all no-op'd and
        // read as "no results"). Source code lives in "memory", the
        // default.
        if !engram_core::namespaces::KNOWN_NAMESPACES.contains(&req.namespace.as_str()) {
            return Err(McpError::invalid_params(
                format!(
                    "grep_project: unknown namespace '{}'. Valid: {}. Source code lives in 'memory' (the default — omit the parameter to search it).",
                    req.namespace,
                    engram_core::namespaces::KNOWN_NAMESPACES.join(", ")
                ),
                None,
            ));
        }

        // Indexed file stats for the freshness guard come from the code
        // graph's file nodes — the same (mtime, size, file_hash) the
        // incremental change scan trusts to decide what to re-index. The
        // guard used to read a separate document store that nothing has ever
        // written to, so it compared against an empty set and could never
        // report a stale file, while defaulting to "strict".
        //
        // grep_project no longer opens a redb database at all, which also
        // retires the file-lock contention that made concurrent greps fail.
        let graph = self.state.graph.clone();
        let project_id = req.project_id.clone();
        let namespace = req.namespace.clone();
        let pattern = req.pattern.clone();
        let path_prefix = req.path_prefix.clone();
        let language = req.language.clone();
        let regex = req.regex;
        let case_sensitive = req.case_sensitive;
        let multiline = req.multiline;
        let context_before = req.context_before;
        let context_after = req.context_after;
        let max_results = req.max_results;
        let engine = ps.search.clone();

        let fingerprint_pid = project_id.clone();
        let result = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let gq = engram_index::grep::GrepQuery {
                project_id,
                namespace,
                generation,
                pattern,
                regex,
                case_sensitive,
                multiline,
                path_prefix,
                language,
                context_before,
                context_after,
                max_results,
                freshness,
            };
            engram_index::grep::grep(&engine, &project_dir, &gq, || {
                indexed_file_stats(&graph, &fingerprint_pid)
            })
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let mut body = if req.output_json {
            serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".into())
        } else {
            render_markdown(&result, &req.pattern, req.regex)
        };
        if !req.output_json {
            body.push_str(&self.freshness_footer(&req.project_id, generation).await);
        }
        Ok(CallToolResult::success(vec![Content::text(body)]))
    }
}

/// Default Markdown rendering. We keep it dense — each match is one
/// line with file:line:col plus the line content; context lines are
/// indented so a scanning reader can still pick out the match.
fn render_markdown(r: &engram_index::grep::GrepResult, pattern: &str, regex: bool) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(1024 + r.matches.len() * 128);
    let tier_label = match r.tier_used {
        engram_index::grep::GrepTier::TermIndex => "term_index",
        engram_index::grep::GrepTier::TermNarrowed => "term_narrowed",
        engram_index::grep::GrepTier::FullScan => "full_scan",
    };
    let _ = writeln!(
        out,
        "# grep_project — `{pattern}` ({mode})\n",
        mode = if regex { "regex" } else { "literal" }
    );
    let _ = writeln!(
        out,
        "**Matches**: {} | **Chunks scanned**: {} | **Files**: {} | **Tier**: `{tier_label}` | **Time**: {} ms\n",
        r.matches.len(),
        r.chunks_scanned,
        r.files_scanned,
        r.elapsed_ms,
    );
    if let Some(ref w) = r.index_stale_warning {
        let _ = writeln!(out, "> ⚠️ {w}");
        // Name the drifted files. A count alone is not actionable — the
        // caller cannot tell whether the stale file is one their results
        // depend on. JSON callers already got `stale_paths`; markdown
        // callers were told only how many.
        const SHOWN: usize = 10;
        for p in r.stale_paths.iter().take(SHOWN) {
            let _ = writeln!(out, "> - `{p}`");
        }
        if r.stale_paths.len() > SHOWN {
            let _ = writeln!(out, "> - …and {} more", r.stale_paths.len() - SHOWN);
        }
        out.push('\n');
    }
    if r.matches.is_empty() {
        out.push_str("_No matches._\n");
        return out;
    }
    out.push_str("## Matches\n\n");
    // A hit inside a minified/generated line used to dump the ENTIRE line —
    // thousands of chars per match. Cap every rendered line; the file:line:col
    // anchor stays exact so the agent can fetch more via get_chunk.
    fn clip(s: &str) -> std::borrow::Cow<'_, str> {
        const MAX: usize = 300;
        if s.chars().count() <= MAX {
            return std::borrow::Cow::Borrowed(s);
        }
        let clipped: String = s.chars().take(MAX).collect();
        std::borrow::Cow::Owned(format!("{clipped}…[+{} chars]", s.chars().count() - MAX))
    }
    for m in &r.matches {
        for (i, before) in m.context_before.iter().enumerate() {
            let ln = (m.line as usize).saturating_sub(m.context_before.len() - i);
            let _ = writeln!(out, "    {}:{}: {}", m.file_path, ln, clip(before));
        }
        let _ = writeln!(
            out,
            "**{}:{}:{}**: {}",
            m.file_path,
            m.line,
            m.column,
            clip(&m.line_text)
        );
        for (i, after) in m.context_after.iter().enumerate() {
            let ln = m.line as usize + i + 1;
            let _ = writeln!(out, "    {}:{}: {}", m.file_path, ln, clip(after));
        }
        out.push('\n');
    }
    out
}
