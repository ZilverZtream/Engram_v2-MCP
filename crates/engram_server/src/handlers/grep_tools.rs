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
pub(crate) fn indexed_file_stats(
    graph: &engram_graph::GraphStore,
    project_id: &str,
) -> anyhow::Result<Vec<engram_index::grep::IndexedFileStat>> {
    indexed_file_stats_for_generation(graph, project_id, u64::MAX)
}

fn indexed_file_stats_for_generation(
    graph: &engram_graph::GraphStore,
    project_id: &str,
    active_generation: u64,
) -> anyhow::Result<Vec<engram_index::grep::IndexedFileStat>> {
    Ok(graph
        .list_file_node_metadata_with_generation(project_id)?
        .into_iter()
        .map(|(rel_path, meta, generation)| {
            let get = |key: &str| {
                meta.as_ref()
                    .and_then(|m| m.get(key))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0)
            };
            let file_hash = meta
                .as_ref()
                .filter(|m| {
                    generation <= active_generation
                        && m.get("source_index_version").and_then(|v| v.as_u64())
                            == Some(engram_index::SOURCE_INDEX_VERSION)
                })
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

/// Overlay current source files, suppressing obsolete indexed matches even when
/// a changed file no longer matches. Strict mode verifies hashes, including
/// edits preserving size and timestamp. All omitted coverage is reported.
fn overlay_working_tree(
    root: &std::path::Path,
    exts: &[&str],
    stats: &[engram_index::grep::IndexedFileStat],
    q: &engram_index::grep::GrepQuery,
    result: &mut engram_index::grep::GrepResult,
) -> anyhow::Result<()> {
    use engram_index::grep::FreshnessMode;
    use std::collections::{HashMap, HashSet};
    const MAX_FILE_BYTES: u64 = 8_000_000;
    const MAX_TOTAL_BYTES: u64 = 128_000_000;
    let indexed: HashMap<_, _> = stats
        .iter()
        .map(|s| (s.rel_path.replace('\\', "/"), s))
        .collect();
    let eligible = |rel: &str| {
        q.path_prefix.as_ref().is_none_or(|p| {
            rel.to_ascii_lowercase()
                .starts_with(&p.replace('\\', "/").to_ascii_lowercase())
        }) && q.language.as_ref().is_none_or(|l| {
            engram_core::types::guess_language(std::path::Path::new(rel)).eq_ignore_ascii_case(l)
        })
    };
    let mut paths: Vec<_> = engram_index::ingest::iter_files(root, exts);
    paths.sort();
    let mut replaced = HashSet::new();
    let mut current = Vec::new();
    let mut disk_query = q.clone();
    disk_query.max_results = q.max_results.saturating_add(1);
    let mut bytes = 0u64;
    let mut scanned = 0usize;
    let mut skipped = 0usize;
    for path in paths {
        let rel = path
            .strip_prefix(root)?
            .to_string_lossy()
            .replace('\\', "/");
        if !eligible(&rel) {
            continue;
        }
        let old = indexed.get(&rel);
        let metadata = std::fs::metadata(&path);
        let must_read = match (&metadata, old) {
            (Ok(meta), Some(old)) if !matches!(q.freshness, FreshnessMode::Strict) => {
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|t| t.as_secs());
                old.file_hash.is_none() || meta.len() != old.size || mtime != Some(old.mtime_secs)
            }
            _ => true,
        };
        if !must_read {
            continue;
        }
        let content = match metadata {
            Ok(meta)
                if meta.len() <= MAX_FILE_BYTES
                    && bytes.saturating_add(meta.len()) <= MAX_TOTAL_BYTES =>
            {
                bytes += meta.len();
                std::fs::read(&path).ok()
            }
            _ => None,
        };
        let Some(content) = content else {
            replaced.insert(rel);
            skipped += 1;
            continue;
        };
        if old.is_some_and(|old| {
            old.file_hash.as_deref() == Some(blake3::hash(&content).to_hex().as_str())
        }) {
            continue;
        }
        let Ok(content) = String::from_utf8(content) else {
            replaced.insert(rel);
            skipped += 1;
            continue;
        };
        replaced.insert(rel.clone());
        scanned += 1;
        if current.len() <= q.max_results {
            current.extend(engram_index::grep::scan_working_tree_content(
                &content,
                &rel,
                &disk_query,
            )?);
        }
    }
    for rel in indexed.keys().filter(|rel| eligible(rel)) {
        if !root.join(rel).is_file() {
            replaced.insert(rel.clone());
        }
    }
    result
        .matches
        .retain(|m| !replaced.contains(&m.file_path.replace('\\', "/")));
    current.append(&mut result.matches);
    let capped = current.len() > q.max_results;
    current.truncate(q.max_results);
    result.matches = current;
    result.files_scanned += scanned;
    result.stale_paths.extend(replaced);
    result.stale_paths.sort();
    result.stale_paths.dedup();
    if scanned > 0 || skipped > 0 || capped {
        let note = format!(
            "Working-tree overlay: {scanned} changed/new file(s) scanned; current contents NOT in the index replace cached hits. {skipped} file(s) could not be verified (read/size/budget limits); cached hits suppressed. Results capped: {capped}. Disk hits have no doc_id; read their file_path directly."
        );
        result.index_stale_warning = Some(match result.index_stale_warning.take() {
            Some(existing) => format!("{existing} | {note}"),
            None => note,
        });
    }
    Ok(())
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
        let context_before = req.context_before.min(100);
        let context_after = req.context_after.min(100);
        let max_results = req.max_results.clamp(1, 1000);
        let exts = crate::utils::files::exts_for_project_type(&rec.project_type);
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
            let started = std::time::Instant::now();
            let stats = indexed_file_stats_for_generation(&graph, &fingerprint_pid, generation)?;
            let mut result =
                engram_index::grep::grep(&engine, &project_dir, &gq, || Ok(stats.clone()))?;
            if gq.namespace == "memory"
                && (!matches!(gq.freshness, engram_index::grep::FreshnessMode::Off)
                    || stats.iter().any(|stat| stat.file_hash.is_none()))
            {
                overlay_working_tree(&project_dir, &exts, &stats, &gq, &mut result)?;
            }
            result.elapsed_ms = started.elapsed().as_millis() as u64;
            Ok(result)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let mut body = if req.output_json {
            let mut value = serde_json::to_value(&result)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            value["project_id"] = serde_json::json!(req.project_id);
            value["directory"] = serde_json::json!(rec.directory);
            value["active_generation"] = serde_json::json!(generation);
            value["result_limit"] = serde_json::json!(max_results);
            value["result_limit_reached"] = serde_json::json!(result.matches.len() >= max_results);
            value["coverage"] = serde_json::json!(
                "bounded_search_not_exhaustive; candidate scans, result limits and verification warnings apply; a result below the limit does not prove complete corpus coverage"
            );
            if let Some(matches) = value["matches"].as_array_mut() {
                for hit in matches {
                    if let Some(doc_id) = hit
                        .get("doc_id")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned)
                    {
                        hit["source"] = serde_json::json!("index");
                        hit["citation_recovery"] = serde_json::json!({
                            "tool": "get_chunk",
                            "arguments": {"project_id": req.project_id, "namespace": req.namespace, "doc_id": doc_id, "citation": {}}
                        });
                        hit["recovery"] = serde_json::json!({
                            "tool": "get_chunk",
                            "arguments": {"project_id": req.project_id, "namespace": req.namespace, "doc_id": doc_id}
                        });
                    } else {
                        hit["source"] = serde_json::json!("working_tree");
                        hit["recovery"] = serde_json::json!({
                            "action": "read_file",
                            "directory": rec.directory,
                            "file_path": hit["file_path"],
                            "line": hit["line"],
                            "note": "No indexed document exists for this hit. Read this file at the returned line; do not substitute a different search result or pass the legacy chunk_id sentinel to get_chunk."
                        });
                    }
                }
            }
            serde_json::to_string_pretty(&value)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
        } else {
            render_markdown(
                &result,
                &req.pattern,
                req.regex,
                &req.project_id,
                &req.namespace,
                max_results,
            )
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
fn render_markdown(
    r: &engram_index::grep::GrepResult,
    pattern: &str,
    regex: bool,
    project_id: &str,
    namespace: &str,
    result_limit: usize,
) -> String {
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
    let _ = writeln!(
        out,
        "Result limit: {result_limit}. Coverage: bounded search; candidate scans and verification warnings apply. Fewer results do not prove exhaustive corpus coverage.\n"
    );
    if r.matches.len() >= result_limit {
        out.push_str("> Result limit reached; additional matches may exist. Narrow the query or path scope. Raising max_results (maximum 1000) may recover more matches but does not remove scan or verification limits.\n\n");
    }
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
            "_No matches in the searched scope._ Source searches with freshness strict/warn also \
             inspect eligible working-tree files; freshness off and knowledge namespaces use the index. \
             Check the scope, exclusions and coverage warnings before concluding the string is absent.\n",
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
        if let Some(doc_id) = &m.doc_id {
            let _ = writeln!(out, "doc_id: `{doc_id}` (get_chunk)");
            let _ = writeln!(
                out,
                "get_chunk arguments: `{}`",
                serde_json::json!({"project_id": project_id, "namespace": namespace, "doc_id": doc_id})
            );
            let _ = writeln!(
                out,
                "citation get_chunk arguments: `{}`",
                serde_json::json!({"project_id": project_id, "namespace": namespace, "doc_id": doc_id, "citation": {}})
            );
        } else {
            let _ = writeln!(
                out,
                "_Working-tree match: open the file directly; no indexed doc_id._"
            );
        }
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
            "\n_… {} more match(es) from this result not shown (Markdown output budget reached). \
             Repeat the same request with `output_json: true` to recover these matches and their \
             retrieval arguments. Search/result caps and coverage warnings still apply; JSON does \
             not establish exhaustive corpus coverage. Narrow the pattern or pass `path_prefix` \
             when the search itself is capped._",
            r.matches.len() - shown
        );
    }
    out
}
