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
            let file_hash = meta
                .as_ref()
                .and_then(|m| m.get("file_hash"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            engram_index::grep::IndexedFileStat {
                rel_path: rel_path.as_str().to_string(),
                size: get("size"),
                mtime_secs: get("mtime"),
                file_hash,
            }
        })
        .collect())
}

/// Scan the WORKING TREE for literal matches the INDEX cannot see: files added
/// since the last index (no indexed fingerprint), or edited since (on-disk mtime
/// newer than the indexed one). This is the real fix for "a stale index is worse
/// than no index" — grep_project now actually searches the tree the way the
/// agent expects, instead of only telling it to. Literal + line-based only
/// (regex/multiline still fall back to the agent). Bounded: on a fresh index
/// only the handful of changed files are read; unchanged and oversized files are
/// skipped.
fn disk_fallback_matches(
    project_dir: &std::path::Path,
    exts: &[&str],
    indexed_mtimes: &std::collections::HashMap<String, u64>,
    pattern: &str,
    case_sensitive: Option<bool>,
    path_prefix: Option<&str>,
    cap: usize,
) -> (Vec<engram_index::grep::GrepMatch>, usize) {
    const MAX_FILE_BYTES: u64 = 2_000_000; // don't read a 26k-line designer file
    const MAX_CANDIDATE_FILES: usize = 800; // bound the work on a very stale index
    // Smart case, matching the engine: honor an explicit flag; otherwise a
    // pattern with any uppercase is case-sensitive, all-lowercase is insensitive.
    let cs = case_sensitive.unwrap_or_else(|| pattern.chars().any(|c| c.is_uppercase()));
    let ci = !cs;
    let needle = if ci {
        pattern.to_lowercase()
    } else {
        pattern.to_string()
    };
    let mut out: Vec<engram_index::grep::GrepMatch> = Vec::new();
    let mut files_scanned = 0usize;
    for path in engram_index::ingest::iter_files(project_dir, exts) {
        if out.len() >= cap || files_scanned >= MAX_CANDIDATE_FILES {
            break;
        }
        let Ok(rel_os) = path.strip_prefix(project_dir) else {
            continue;
        };
        let rel = rel_os.to_string_lossy().replace('\\', "/");
        if let Some(pfx) = path_prefix {
            if !rel.starts_with(pfx) {
                continue;
            }
        }
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.len() > MAX_FILE_BYTES {
            continue;
        }
        let disk_mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Candidate iff new (not indexed) or edited since indexed.
        match indexed_mtimes.get(&rel) {
            None => {}                         // new file
            Some(&im) if disk_mtime > im => {} // edited since indexed
            Some(_) => continue,               // indexed + unchanged
        }
        let Ok(buf) = std::fs::read_to_string(&path) else {
            continue; // binary / non-utf8
        };
        files_scanned += 1;
        for (i, line) in buf.lines().enumerate() {
            if out.len() >= cap {
                break;
            }
            let hay = if ci {
                line.to_lowercase()
            } else {
                line.to_string()
            };
            if let Some(col) = hay.find(&needle) {
                out.push(engram_index::grep::GrepMatch {
                    file_path: rel.clone(),
                    line: (i as u32) + 1,
                    column: (col as u32) + 1,
                    line_text: line.chars().take(300).collect(),
                    context_before: vec![],
                    context_after: vec![],
                    chunk_id: 0,
                });
            }
        }
    }
    (out, files_scanned)
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
        let mut result = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
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

        // Working-tree fallback — a TRUE fallback, only when the index found
        // NOTHING. The engine searches only the index, so a string in a file
        // added since the last index is invisible (the J5 failure). We scan the
        // tree only on an index miss: augmenting non-empty results would bloat
        // every grep (index + disk) and, across a review's dozens of calls, blow
        // the caller's context budget. Regex/multiline keep the "grep the tree"
        // hint instead.
        if !req.regex && !req.multiline && result.matches.is_empty() {
            let graph2 = self.state.graph.clone();
            let pid2 = req.project_id.clone();
            let project_dir2 = PathBuf::from(rec.directory.clone());
            let pattern2 = req.pattern.clone();
            let case_sensitive2 = req.case_sensitive;
            let path_prefix2 = req.path_prefix.clone();
            let ptype = rec.project_type.clone();
            let (disk_matches, disk_files) = tokio::task::spawn_blocking(move || {
                let indexed: std::collections::HashMap<String, u64> = graph2
                    .list_file_node_metadata(&pid2)
                    .map(|rows| {
                        rows.into_iter()
                            .map(|(rp, meta)| {
                                let m = meta
                                    .as_ref()
                                    .and_then(|m| m.get("mtime"))
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                (rp.as_str().replace('\\', "/"), m)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let exts = crate::utils::files::exts_for_project_type(&ptype);
                // Small cap: this is an index-miss fallback, so the agent just
                // needs to know the string exists and roughly where in the
                // new/changed files — not a full dump (context-budget safe).
                disk_fallback_matches(
                    &project_dir2,
                    &exts,
                    &indexed,
                    &pattern2,
                    case_sensitive2,
                    path_prefix2.as_deref(),
                    15,
                )
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            if !disk_matches.is_empty() {
                let n = disk_matches.len();
                result.matches.extend(disk_matches);
                result.files_scanned += disk_files;
                let disk_note = format!(
                    "{n} additional match(es) from {disk_files} file(s) NOT in the index \
                     (added or edited since the last index) — found by scanning the working \
                     tree directly. These are current; any same-file index matches may be stale."
                );
                result.index_stale_warning = Some(match result.index_stale_warning.take() {
                    Some(existing) => format!("{existing} | {disk_note}"),
                    None => disk_note,
                });
            }
        }

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
        out.push_str(
            "_No matches in the index._ grep_project searches Engram's index, not the disk — a \
             file added or edited since the last index is invisible here. The files are on disk \
             and a working-tree grep always works: grep the working tree before concluding this \
             string is absent. Reserve \"cannot determine\" for questions the source can't settle.\n",
        );
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
    // Byte budget: even under the match-count cap, a pattern hitting many long
    // lines can still emit a large block, and a review makes dozens of greps
    // whose output accumulates in the model's request until it overflows (the
    // HTTP 400 this guards against). Stop rendering matches past the budget and
    // tell the caller how to get the rest.
    const MATCHES_BUDGET: usize = 3_000;
    let matches_start = out.len();
    let mut shown = 0usize;
    for m in &r.matches {
        if shown > 0 && out.len() - matches_start > MATCHES_BUDGET {
            break;
        }
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
        shown += 1;
    }
    if shown < r.matches.len() {
        let _ = writeln!(
            out,
            "\n_… {} more match(es) not shown (output budget reached). Narrow the pattern, pass \
             `path_prefix`, or raise `max_results` for an exhaustive list._",
            r.matches.len() - shown
        );
    }
    out
}
