//! Planning tools: the pre-implementation context an agent needs to turn a
//! one-line user story ("as an admin I want to set the minimum number of
//! photos") into a complete change in a legacy codebase.
//!
//! - `get_concept_footprint`: every touchpoint of a domain concept, grouped
//!   by role — the defense against "edited 2 of the 17 places the concept
//!   appears".
//! - `find_similar_changes`: historical commits most similar to a planned
//!   file set, and the recurring companion artifacts MISSING from it — the
//!   defense against "added the feature, forgot the admin page / menu entry".
//! - `find_implementation_pattern`: concrete exemplars of how this codebase
//!   already implements a pattern, with their data/state edges — agents
//!   imitate better than they invent.

use crate::handlers::validate_project_id;
use crate::models::{
    FindImplementationPatternRequest, FindSimilarChangesRequest, GetConceptFootprintRequest,
};
use crate::tools::Engram;
use engram_git::history::{GitWalker, MergeCommitPolicy};
use engram_graph::EdgeKind;
use engram_index::HybridQuery;
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

// ── Pure helpers ─────────────────────────────────────────────────────────────

/// Lowercase concept stems used for substring matching: the term itself,
/// a naive singular (trailing 's' stripped), the clean `ies`->`y` / `es`
/// singular, and a compacted form without separators so "code category" also
/// matches "CodeCategory"/"code_category".
pub(crate) fn concept_stems(concept: &str) -> Vec<String> {
    let lower = concept.trim().to_lowercase();
    let mut stems = vec![lower.clone()];
    if let Some(singular) = lower.strip_suffix('s')
        && singular.len() >= 3
    {
        stems.push(singular.to_string());
    }
    // Clean English plural -> singular so a PLURAL concept matches SINGULAR code
    // identifiers: "roqentries" -> "roqentry" (matches RoqEntryService), which the
    // naive 's'-strip ("roqentrie") misses; "categories" -> "category". Guard the
    // base length so we never produce a tiny, over-matching stem.
    if let Some(base) = lower.strip_suffix("ies")
        && base.len() >= 3
    {
        let singular = format!("{base}y");
        if !stems.contains(&singular) {
            stems.push(singular);
        }
    } else if let Some(base) = lower.strip_suffix("es")
        && base.len() >= 4
        && !stems.iter().any(|s| s == base)
    {
        stems.push(base.to_string());
    }
    let compact: String = lower.chars().filter(|c| c.is_alphanumeric()).collect();
    if !stems.contains(&compact) && compact.len() >= 3 {
        stems.push(compact);
    }

    // Multi-word concepts: the whole phrase almost never appears verbatim in
    // identifiers ("user role and permission management" compacted to one
    // token matched NOTHING on a codebase full of role/permission code).
    // Add salient-word bigrams ("user role" → also "userrole") and long
    // single words. Connectives are dropped; short single words ("user",
    // "role") are NOT added alone — they over-match thousands of nodes.
    const CONNECTIVES: &[&str] = &[
        "and", "or", "of", "the", "a", "an", "to", "for", "in", "on", "with", "by", "from",
    ];
    let salient: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3 && !CONNECTIVES.contains(w))
        .collect();
    if salient.len() >= 2 {
        for pair in salient.windows(2) {
            let spaced = format!("{} {}", pair[0], pair[1]);
            let joined = format!("{}{}", pair[0], pair[1]);
            for s in [spaced, joined] {
                if !stems.contains(&s) {
                    stems.push(s);
                }
            }
        }
        for w in &salient {
            if w.len() >= 6 {
                let s = w.to_string();
                if !stems.contains(&s) {
                    stems.push(s);
                }
            }
        }
    }

    stems.retain(|s| !s.is_empty());
    stems
}

/// Split an identifier into lowercase word tokens on separators (`_`, `-`,
/// `.`, `/`, spaces) AND camelCase boundaries: `UserRoleProvider` →
/// ["user", "role", "provider"].
pub(crate) fn name_tokens(name: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                tokens.push(cur.to_lowercase());
                cur.clear();
            }
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower && !cur.is_empty() {
            tokens.push(cur.to_lowercase());
            cur.clear();
        }
        prev_lower = c.is_lowercase() || c.is_ascii_digit();
        cur.push(c);
    }
    if !cur.is_empty() {
        tokens.push(cur.to_lowercase());
    }
    tokens
}

/// Does `name` match any stem AT A TOKEN BOUNDARY?
///
/// The old raw-substring version made "order" match `reorder`,
/// `placeholder`, and `borderColor` — false touchpoints that propagated
/// into plan_user_story and get_change_set seeds. Now a stem matches when
/// (a) some identifier token equals or starts with it (`order` → `orders`,
/// NOT `reorder`), or (b) for multi-word stems, the concatenation of
/// CONSECUTIVE tokens starting at a token boundary begins with the stem's
/// compact form (`user role`/`userrole` → `UserRoleProvider`).
pub(crate) fn matches_concept(name: &str, stems: &[String]) -> bool {
    let tokens = name_tokens(name);
    if tokens.is_empty() {
        return false;
    }
    stems.iter().any(|s| {
        let s_compact: String = s.chars().filter(|c| c.is_alphanumeric()).collect();
        if s_compact.is_empty() {
            return false;
        }
        // Single-token check: equality or prefix at a token start.
        if tokens.iter().any(|t| t.starts_with(&s_compact)) {
            return true;
        }
        // Multi-token check: consecutive-token concatenation from each
        // boundary. Early-exits once the running concat outgrows the stem.
        for start in 0..tokens.len() {
            let mut concat = String::new();
            for t in &tokens[start..] {
                concat.push_str(t);
                if concat.len() >= s_compact.len() {
                    break;
                }
            }
            if concat.starts_with(&s_compact) {
                return true;
            }
        }
        false
    })
}

/// Token bag for change-shape similarity: full path, directory segments,
/// extension, and basename words (split on `_`, `-`, `.`, and case
/// boundaries). All lowercase.
pub(crate) fn path_token_bag(files: &[String]) -> HashSet<String> {
    let mut bag = HashSet::new();
    for f in files {
        let norm = f.replace('\\', "/").to_lowercase();
        bag.insert(norm.clone());
        let mut segments: Vec<&str> = norm.split('/').collect();
        let file_name = segments.pop().unwrap_or("");
        for seg in segments {
            if !seg.is_empty() {
                bag.insert(format!("dir:{seg}"));
            }
        }
        if let Some((stem, ext)) = file_name.rsplit_once('.') {
            bag.insert(format!("ext:{ext}"));
            // ASPX-family double extensions (aspx.cs / ascx.vb / designer.cs)
            if let Some((_, ext2)) = stem.rsplit_once('.') {
                bag.insert(format!("ext:{ext2}.{ext}"));
            }
            for word in stem.split(['_', '-', '.']) {
                if word.len() >= 3 {
                    bag.insert(format!("w:{word}"));
                }
            }
        }
    }
    bag
}

/// Jaccard similarity of two token bags.
pub(crate) fn bag_jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count();
    let union = a.len() + b.len() - inter;
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

/// Candidate transpile-partner paths for a (web-root-stripped, lowercased)
/// path: a TypeScript source's committed `.js` bundle, or a `.js` bundle's `.ts`
/// source. Generic transpile conventions only — a same-dir extension swap, or a
/// single output-dir swap (`ts/` <-> `~.js/` or `js/`) with an IDENTICAL
/// basename. The caller keeps only candidates that actually exist in the index,
/// so over-generation here is harmless. Empty for non-TS/JS paths.
pub(crate) fn transpile_pair_candidates(ps: &str) -> Vec<String> {
    let build = |src_exts: &[&str], out_exts: &[&str], dir_swaps: &[(&str, &str)]| -> Vec<String> {
        let stem = src_exts.iter().fold(ps.to_string(), |acc, e| {
            acc.strip_suffix(e).map(str::to_string).unwrap_or(acc)
        });
        if stem == ps {
            return Vec::new(); // ext didn't match (e.g. ".json" not ".js")
        }
        let mut bases = vec![stem.clone()];
        for (from, to) in dir_swaps {
            if stem.contains(from) {
                bases.push(stem.replacen(from, to, 1));
            }
        }
        bases
            .iter()
            .flat_map(|b| out_exts.iter().map(move |e| format!("{b}{e}")))
            .collect()
    };
    if ps.ends_with(".ts") || ps.ends_with(".tsx") {
        build(
            &[".tsx", ".ts"],
            &[".js", ".jsx"],
            &[("/ts/", "/~.js/"), ("/ts/", "/js/")],
        )
    } else if ps.ends_with(".js") || ps.ends_with(".jsx") {
        build(
            &[".jsx", ".js"],
            &[".ts", ".tsx"],
            &[("/~.js/", "/ts/"), ("/js/", "/ts/")],
        )
    } else {
        Vec::new()
    }
}

/// "dir/.../*.ext" shape of a path, used to report recurring companion
/// patterns ("Admin/*.aspx") instead of raw historical file names.
pub(crate) fn dir_ext_shape(path: &str) -> Option<String> {
    let norm = path.replace('\\', "/");
    let (dir, file) = norm.rsplit_once('/').unwrap_or(("", norm.as_str()));
    let ext = file.split_once('.').map(|(_, e)| e)?;
    Some(if dir.is_empty() {
        format!("*.{ext}")
    } else {
        format!("{dir}/*.{ext}")
    })
}

impl Engram {
    // ── get_concept_footprint ────────────────────────────────────────────────

    pub async fn handle_get_concept_footprint(
        &self,
        req: GetConceptFootprintRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        if req.concept.trim().len() < 2 {
            return Err(McpError::invalid_params(
                "concept must be at least 2 characters",
                None,
            ));
        }
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;
        let cap = req.max_per_group.clamp(1, 100);
        let stems = concept_stems(&req.concept);

        // One graph scan + bounded consumer expansion, all in one blocking hop.
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let stems_b = stems.clone();
        type Entry = (String, String, String, u32); // name, node_id, file, line
        let (groups, consumers, scan_truncated) = tokio::task::spawn_blocking(move || {
            let nodes = graph
                .query_nodes(&pid, None, None, None, crate::handlers::NODE_SCAN_LIMIT)
                .unwrap_or_default();
            let scan_truncated = nodes.len() >= crate::handlers::NODE_SCAN_LIMIT;

            let mut groups: BTreeMap<&'static str, Vec<Entry>> = BTreeMap::new();
            let mut anchors: Vec<(String, String)> = Vec::new(); // (node_id, name)
            for n in &nodes {
                if !matches_concept(&n.name, &stems_b) {
                    continue;
                }
                let group = match n.node_type.as_str() {
                    "db_table" | "db_column" => "data",
                    "stored_proc" | "stored_procedure" | "inline_sql" => "sql",
                    "global_state" => "state",
                    "page" | "control" | "ui_container" | "control_layout" => "ui",
                    "function" | "class" | "interface" => "logic",
                    "web_service" | "http_handler" | "wcf_service" | "route_handler" => "endpoints",
                    "file" => "files",
                    _ => continue,
                };
                if matches!(n.node_type.as_str(), "db_table" | "global_state") && anchors.len() < 5
                {
                    anchors.push((n.node_id.clone(), n.name.clone()));
                }
                groups.entry(group).or_default().push((
                    n.name.clone(),
                    n.node_id.clone(),
                    n.file_path.as_str().to_string(),
                    n.start_line,
                ));
            }
            for list in groups.values_mut() {
                list.sort();
                list.dedup();
            }

            // Consumers of the anchor tables / state keys: who reads/writes.
            let mut consumers: Vec<(String, String, String)> = Vec::new(); // anchor, kind, src
            for (anchor_id, anchor_name) in &anchors {
                if let Ok(incoming) =
                    graph.find_incoming_edges_with_kind(&pid, None, anchor_id, 200)
                {
                    for (src, kind, _w) in incoming {
                        if matches!(
                            kind,
                            EdgeKind::QueriesTable
                                | EdgeKind::ReadsColumn
                                | EdgeKind::ReadsState
                                | EdgeKind::WritesState
                                | EdgeKind::SqlCalls
                                | EdgeKind::DataBinding
                        ) {
                            consumers.push((anchor_name.clone(), kind.as_str().to_string(), src));
                        }
                    }
                }
            }
            consumers.sort();
            consumers.dedup();
            (groups, consumers, scan_truncated)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // Lexical layer: files mentioning the concept that no graph group has.
        let engine = ps.search.clone();
        let q = HybridQuery {
            project_id: req.project_id.clone(),
            namespace: "memory".into(),
            generation: gen_,
            text: req.concept.clone(),
            top_k: 50,
            fts_mode: "loose".into(),
            include_path_prefixes: None,
            exclude_path_prefixes: None,
            language_filters: None,
            author_filter: None,
            date_after: None,
            date_before: None,
            use_mmr: false,
        };
        let lexical_files: Vec<String> = tokio::task::spawn_blocking(move || {
            engine
                .lexical_search(&q)
                .map(|hits| {
                    let mut files: Vec<String> =
                        hits.iter().map(|h| h.path.as_str().to_string()).collect();
                    files.sort();
                    files.dedup();
                    files
                })
                .unwrap_or_default()
        })
        .await
        .unwrap_or_default();

        let grouped_files: HashSet<&str> = groups
            .values()
            .flatten()
            .map(|(_, _, file, _)| file.as_str())
            .collect();
        // ENG-2026-CFP-NOISE: drop vendored/minified/bundled artifacts (bower,
        // node_modules, *.min.js, versioned jquery, …). The OciusX eval found the
        // concept-footprint lexical layer was the dominant noise source handed to
        // the model (precision ~5%); these generated files are never the change
        // target and crowd out the real files.
        let lexical_only: Vec<&String> = lexical_files
            .iter()
            .filter(|f| !grouped_files.contains(f.as_str()))
            .filter(|f| !engram_core::is_vendor_path(f))
            .collect();

        let total: usize = groups.values().map(Vec::len).sum();
        if total == 0 && lexical_only.is_empty() {
            let mut out = format!(
                "No touchpoints found for concept '{}' (stems tried: {}).\n\
                 hints: try a shorter stem (e.g. \"photo\" not \"photographs\"); \
                 search_memory with fts_mode=\"loose\" to discover the codebase's \
                 actual vocabulary for this concept.",
                req.concept,
                stems.join(", ")
            );
            out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
            return Ok(CallToolResult::success(vec![Content::text(out)]));
        }

        let mut out = format!(
            "# Concept footprint: '{}'\n\nstems matched: {} | graph touchpoints: {}\n",
            req.concept,
            stems.join(", "),
            total
        );
        if scan_truncated {
            out.push_str(
                "⚠ node scan hit the node-scan cap — touchpoints may be incomplete on this \
                 graph; narrow the concept or rely on the lexical section below.\n",
            );
        }
        let titles = [
            ("data", "## Data (tables / columns)"),
            ("sql", "## SQL (stored procs / inline)"),
            (
                "state",
                "## Shared state (Session / ViewState / Cache keys)",
            ),
            ("ui", "## UI (pages / controls)"),
            ("logic", "## Logic (functions / classes)"),
            ("endpoints", "## Endpoints (services / routes)"),
            ("files", "## Files"),
        ];
        for (key, title) in titles {
            let Some(list) = groups.get(key) else {
                continue;
            };
            out.push_str(&format!("\n{title} — {}\n", list.len()));
            for (name, node_id, file, line) in list.iter().take(cap) {
                if file.is_empty() {
                    out.push_str(&format!("- {name} — node_id={node_id}\n"));
                } else {
                    out.push_str(&format!("- {name} — node_id={node_id} ({file}:{line})\n"));
                }
            }
            if list.len() > cap {
                out.push_str(&format!("  ... and {} more\n", list.len() - cap));
            }
        }

        if !consumers.is_empty() {
            out.push_str(&format!(
                "\n## Consumers of core anchors — {} (who reads/writes the tables & state keys)\n",
                consumers.len()
            ));
            for (anchor, kind, src) in consumers.iter().take(cap * 2) {
                out.push_str(&format!("- [{kind}] {src} -> {anchor}\n"));
            }
            if consumers.len() > cap * 2 {
                out.push_str(&format!("  ... and {} more\n", consumers.len() - cap * 2));
            }
        }

        if !lexical_only.is_empty() {
            out.push_str(&format!(
                "\n## Mentioned only in text — {} file(s) the graph has no concept edge for (verify manually)\n",
                lexical_only.len()
            ));
            for f in lexical_only.iter().take(cap) {
                out.push_str(&format!("- {f}\n"));
            }
            if lexical_only.len() > cap {
                out.push_str(&format!("  ... and {} more\n", lexical_only.len() - cap));
            }
        }

        out.push_str(
            "\nnext: trace_state_usage for each state key; get_table_schema for each table; \
             find_similar_changes once you know which files you'll touch.\n",
        );
        out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    // ── find_similar_changes ─────────────────────────────────────────────────

    pub async fn handle_find_similar_changes(
        &self,
        req: FindSimilarChangesRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        if req.files.is_empty() {
            return Err(McpError::invalid_params(
                "files must contain at least one planned/changed file path",
                None,
            ));
        }
        let rec = self.ensure_project_record(&req.project_id).await?;
        let gen_ = self
            .get_active_generation(&req.project_id)
            .await
            .unwrap_or(1);
        let max_commits = req.sanitized_max_commits();
        let top = req.sanitized_top();
        let input_files: Vec<String> = req.files.iter().map(|f| f.replace('\\', "/")).collect();
        let repo_dir = PathBuf::from(&rec.directory);

        let input_bag = path_token_bag(&input_files);
        let input_set: HashSet<String> = input_files.iter().map(|f| f.to_lowercase()).collect();

        let started = std::time::Instant::now();
        let cache = self.state.co_change_cache.clone();
        let cache_key = req.project_id.clone();
        // Disk-persisted snapshot: the in-memory cache dies with the daemon,
        // making every cold start pay the ~24 s walk again. History is
        // immutable, so a bincode dump keyed by HEAD oid is exact.
        let disk_path = self
            .state
            .cfg
            .data_dir
            .join("co_change")
            .join(format!("{}.bin", req.project_id));
        let result = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let repo = GitWalker::open_repo(&repo_dir)?;
            let head = repo
                .head()
                .ok()
                .and_then(|h| h.target())
                .map(|o| o.to_string())
                .unwrap_or_default();

            // Cache hit: same HEAD, at least as deep a walk. History is
            // immutable, so the cached (oid, summary, files) list is exact —
            // this turns a 24 s / 800-git-diff call into pure scoring.
            let disk_load = || -> Option<std::sync::Arc<crate::state::CoChangeSnapshot>> {
                let bytes = std::fs::read(&disk_path).ok()?;
                let snap: crate::state::CoChangeSnapshot = bincode::deserialize(&bytes).ok()?;
                (snap.head == head && !head.is_empty() && snap.walked >= max_commits)
                    .then(|| std::sync::Arc::new(snap))
            };
            let snapshot = match cache.get(&cache_key) {
                Some(s) if s.head == head && !head.is_empty() && s.walked >= max_commits => {
                    s.clone()
                }
                _ if disk_load().is_some() => {
                    let snap = disk_load().expect("checked");
                    cache.insert(cache_key, snap.clone());
                    snap
                }
                _ => {
                    let cancel = tokio_util::sync::CancellationToken::new();
                    let oids = GitWalker::walk_older_commits(
                        &repo,
                        None,
                        max_commits,
                        MergeCommitPolicy::FirstParentOnly,
                        &cancel,
                    )?;
                    let mut commits = Vec::with_capacity(oids.len());
                    for oid in oids {
                        let Ok(changes) = GitWalker::files_changed_in_commit(&repo, oid) else {
                            continue;
                        };
                        // Bulk commits (vendoring, formatting) are shape noise.
                        if changes.len() > 80 || changes.is_empty() {
                            continue;
                        }
                        let files: Vec<String> = changes
                            .iter()
                            .map(|c| c.path().as_str().replace('\\', "/"))
                            .collect();
                        let summary = repo
                            .find_commit(oid)
                            .ok()
                            .and_then(|c| c.summary().map(|s| s.to_string()))
                            .unwrap_or_default();
                        commits.push(crate::state::CoChangeCommit {
                            oid: oid.to_string(),
                            summary,
                            files,
                        });
                    }
                    let snap = std::sync::Arc::new(crate::state::CoChangeSnapshot {
                        head,
                        walked: max_commits,
                        commits,
                    });
                    cache.insert(cache_key, snap.clone());
                    // Best-effort disk persist for the next cold start.
                    if let Ok(bytes) = bincode::serialize(snap.as_ref()) {
                        let _ = std::fs::create_dir_all(disk_path.parent().unwrap());
                        let _ = std::fs::write(&disk_path, bytes);
                    }
                    snap
                }
            };

            let scanned = snapshot.walked;
            let mut scored: Vec<(f64, String, String, Vec<String>)> = Vec::new();
            for c in &snapshot.commits {
                let score = bag_jaccard(&input_bag, &path_token_bag(&c.files));
                if score <= 0.0 {
                    continue;
                }
                scored.push((score, c.oid.clone(), c.summary.clone(), c.files.clone()));
            }
            scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            scored.truncate(top);
            Ok((scanned, scored))
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| {
            McpError::internal_error(
                format!(
                    "find_similar_changes: cannot walk git history at '{}': {e}. \
                     hint: the project directory must be a git repository.",
                    rec.directory
                ),
                None,
            )
        })?;
        let (scanned, scored) = result;

        if scored.is_empty() {
            let mut out = format!(
                "No similar historical changes found ({scanned} commits scanned).\n\
                 hints: pass more representative file paths; raise max_commits; \
                 this also happens when the planned files share no naming/directory \
                 conventions with past work — worth a closer look in itself."
            );
            out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
            return Ok(CallToolResult::success(vec![Content::text(out)]));
        }

        // Companion analysis: what do the similar changes touch that the plan
        // doesn't? Exact recurring files first (menu/config/registration files
        // recur verbatim), then recurring dir/*.ext shapes.
        let threshold = scored.len().div_ceil(2);
        let mut exact_counts: HashMap<&str, usize> = HashMap::new();
        let mut shape_counts: HashMap<String, usize> = HashMap::new();
        for (_, _, _, files) in &scored {
            let mut seen_shapes = HashSet::new();
            for f in files {
                if !input_set.contains(&f.to_lowercase()) {
                    *exact_counts.entry(f.as_str()).or_default() += 1;
                    if let Some(shape) = dir_ext_shape(f)
                        && seen_shapes.insert(shape.clone())
                    {
                        *shape_counts.entry(shape).or_default() += 1;
                    }
                }
            }
        }
        let input_shapes: HashSet<String> = input_files
            .iter()
            .filter_map(|f| dir_ext_shape(f))
            .collect();
        let mut recurring_exact: Vec<(&str, usize)> = exact_counts
            .into_iter()
            .filter(|(_, c)| *c >= threshold)
            .collect();
        recurring_exact.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        let mut recurring_shapes: Vec<(String, usize)> = shape_counts
            .into_iter()
            .filter(|(s, c)| *c >= threshold && !input_shapes.contains(s))
            .collect();
        recurring_shapes.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

        let mut out = format!(
            "# Similar historical changes ({} of {scanned} commits scanned, {:.1}s)\n\nYour set: {}\n",
            scored.len(),
            started.elapsed().as_secs_f32(),
            input_files.join(", ")
        );
        for (i, (score, hash, summary, files)) in scored.iter().enumerate() {
            out.push_str(&format!(
                "\n## #{} {} (similarity {:.2}) — {}\n",
                i + 1,
                &hash[..10.min(hash.len())],
                score,
                summary
            ));
            for f in files.iter().take(25) {
                let marker = if input_set.contains(&f.to_lowercase()) {
                    " ← in your set"
                } else {
                    ""
                };
                out.push_str(&format!("- {f}{marker}\n"));
            }
            if files.len() > 25 {
                out.push_str(&format!("  ... and {} more\n", files.len() - 25));
            }
        }

        if recurring_exact.is_empty() && recurring_shapes.is_empty() {
            out.push_str("\n## Companion check\nNo recurring companion artifacts are missing from your set.\n");
        } else {
            out.push_str(&format!(
                "\n## Changes like this also touched — MISSING from your set (≥{threshold} of {} commits)\n",
                scored.len()
            ));
            for (f, c) in recurring_exact.iter().take(15) {
                out.push_str(&format!("- {f} ({c}x) ← exact file, strong signal\n"));
            }
            for (s, c) in recurring_shapes.iter().take(15) {
                out.push_str(&format!("- {s} ({c}x)\n"));
            }
            out.push_str(
                "\nReview each before committing: these are the artifacts a reviewer \
                 will notice are absent (admin pages, menu/sitemap entries, \
                 registrations, migrations).\n",
            );
        }
        out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    // ── find_implementation_pattern ──────────────────────────────────────────

    pub async fn handle_find_implementation_pattern(
        &self,
        req: FindImplementationPatternRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        if req.pattern_query.trim().is_empty() {
            return Err(McpError::invalid_params(
                "pattern_query must not be empty",
                None,
            ));
        }
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;
        let max_examples = req.max_examples.clamp(1, 10);

        let engine = ps.search.clone();
        let q = HybridQuery {
            project_id: req.project_id.clone(),
            namespace: "memory".into(),
            generation: gen_,
            text: req.pattern_query.clone(),
            top_k: 30,
            fts_mode: "loose".into(),
            include_path_prefixes: None,
            exclude_path_prefixes: None,
            language_filters: None,
            author_filter: None,
            date_after: None,
            date_before: None,
            use_mmr: false,
        };
        let hits = tokio::task::spawn_blocking(move || engine.lexical_search(&q))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if hits.is_empty() {
            let mut out = format!(
                "No code matching pattern '{}' found.\n\
                 hints: use the codebase's own vocabulary (try search_memory first to \
                 discover it); shorter queries match more.",
                req.pattern_query
            );
            out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
            return Ok(CallToolResult::success(vec![Content::text(out)]));
        }

        // Rank candidate files by hit count then best score; prefer directory
        // diversity so three exemplars aren't three siblings of one another.
        let mut per_file: BTreeMap<String, (usize, f32, String, u32)> = BTreeMap::new();
        for h in &hits {
            let entry = per_file.entry(h.path.as_str().to_string()).or_insert((
                0,
                f32::MIN,
                String::new(),
                0,
            ));
            entry.0 += 1;
            if h.score > entry.1 {
                entry.1 = h.score;
                entry.2 = h.snippet.clone().unwrap_or_default();
                entry.3 = h.start_line;
            }
        }
        let mut ranked: Vec<(String, (usize, f32, String, u32))> = per_file.into_iter().collect();
        ranked.sort_by(|a, b| {
            b.1.0.cmp(&a.1.0).then(
                b.1.1
                    .partial_cmp(&a.1.1)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
        });
        let mut exemplars: Vec<(String, usize, f32, String, u32)> = Vec::new();
        let mut seen_dirs: HashSet<String> = HashSet::new();
        for (path, (hits_n, score, snippet, line)) in &ranked {
            let dir = path
                .rsplit_once('/')
                .map(|(d, _)| d.to_string())
                .unwrap_or_default();
            if exemplars.len() >= max_examples {
                break;
            }
            if seen_dirs.contains(&dir) && ranked.len() > max_examples {
                continue;
            }
            seen_dirs.insert(dir);
            exemplars.push((path.clone(), *hits_n, *score, snippet.clone(), *line));
        }
        // Backfill if directory diversity left slots empty.
        for (path, (hits_n, score, snippet, line)) in &ranked {
            if exemplars.len() >= max_examples {
                break;
            }
            if !exemplars.iter().any(|(p, ..)| p == path) {
                exemplars.push((path.clone(), *hits_n, *score, snippet.clone(), *line));
            }
        }

        // Graph context per exemplar in one blocking hop.
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let paths: Vec<String> = exemplars.iter().map(|(p, ..)| p.clone()).collect();
        type FileCtx = (Vec<String>, Vec<String>, Vec<String>); // symbols, data_edges, coupled
        let contexts: HashMap<String, FileCtx> = tokio::task::spawn_blocking(move || {
            let mut map = HashMap::new();
            for path in paths {
                let nodes = graph
                    .query_nodes(&pid, None, None, Some(&path), 200)
                    .unwrap_or_default();
                let mut symbols = Vec::new();
                let mut data_edges: Vec<String> = Vec::new();
                for n in &nodes {
                    if matches!(n.node_type.as_str(), "function" | "class") && symbols.len() < 10 {
                        symbols.push(format!(
                            "{} ({}) line {}",
                            n.name, n.node_type, n.start_line
                        ));
                    }
                    if n.node_type == "function" {
                        for kind in [
                            EdgeKind::SqlCalls,
                            EdgeKind::QueriesTable,
                            EdgeKind::ReadsState,
                            EdgeKind::WritesState,
                        ] {
                            if let Ok(neigh) = graph.neighbors(&pid, kind.clone(), &n.node_id, 10) {
                                for (target, _) in neigh {
                                    data_edges.push(format!("[{}] {target}", kind.as_str()));
                                }
                            }
                        }
                    }
                }
                data_edges.sort();
                data_edges.dedup();
                data_edges.truncate(12);

                let file_node = format!("file:{path}");
                let coupled: Vec<String> = graph
                    .neighbors(&pid, EdgeKind::TemporalCoupling, &file_node, 3)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(id, w)| format!("{} (co-changed {w}x)", id.trim_start_matches("file:")))
                    .collect();
                map.insert(path, (symbols, data_edges, coupled));
            }
            map
        })
        .await
        .unwrap_or_default();

        let mut out = format!(
            "# Implementation pattern exemplars: '{}'\n",
            req.pattern_query
        );
        let mut ingredient_counts: HashMap<String, usize> = HashMap::new();
        for (i, (path, hits_n, score, snippet, line)) in exemplars.iter().enumerate() {
            out.push_str(&format!(
                "\n## Exemplar #{}: {path} ({hits_n} match(es), score {score:.2})\n",
                i + 1
            ));
            if let Some((symbols, data_edges, coupled)) = contexts.get(path) {
                if !symbols.is_empty() {
                    out.push_str(&format!("symbols: {}\n", symbols.join("; ")));
                }
                if !data_edges.is_empty() {
                    out.push_str(&format!("data/state: {}\n", data_edges.join("; ")));
                    for e in data_edges {
                        *ingredient_counts.entry(e.clone()).or_default() += 1;
                    }
                }
                if !coupled.is_empty() {
                    out.push_str(&format!("co-changes with: {}\n", coupled.join("; ")));
                }
            }
            if !snippet.is_empty() {
                let trimmed: String = snippet.chars().take(600).collect();
                out.push_str(&format!("snippet (line {line}):\n```\n{trimmed}\n```\n"));
            }
        }

        let mut common: Vec<(&String, &usize)> =
            ingredient_counts.iter().filter(|(_, c)| **c >= 2).collect();
        common.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        if !common.is_empty() {
            out.push_str("\n## Common ingredients (in ≥2 exemplars — the house pattern)\n");
            for (ing, c) in common.iter().take(10) {
                out.push_str(&format!("- {ing} ({c}x)\n"));
            }
        }
        out.push_str(
            "\nnext: get_chunk / get_full_method_body on the best exemplar to imitate it; \
             get_concept_footprint for the domain concept you're wiring in.\n",
        );
        out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concept_stems_cover_plural_and_compound() {
        let stems = concept_stems("Code Categories");
        assert!(stems.contains(&"code categories".to_string()));
        assert!(stems.contains(&"code categorie".to_string())); // naive singular
        assert!(stems.contains(&"codecategories".to_string())); // compact
        assert!(matches_concept("ddlCodeCategories", &stems));
        assert!(matches_concept("CODE_CATEGORIES", &stems));
    }

    #[test]
    fn matches_concept_ignores_case_and_separators() {
        let stems = concept_stems("photo");
        assert!(matches_concept("PhotoCount", &stems));
        assert!(matches_concept("MIN_PHOTOS_REQUIRED", &stems));
        assert!(matches_concept("tbl_photos", &stems));
        assert!(!matches_concept("OrderStatus", &stems));
    }

    #[test]
    fn path_token_bag_extracts_shape_tokens() {
        let bag = path_token_bag(&["Admin/CodeCategories.aspx.cs".to_string()]);
        assert!(bag.contains("dir:admin"));
        assert!(bag.contains("ext:cs"));
        assert!(bag.contains("ext:aspx.cs"));
        assert!(bag.contains("w:codecategories"));
    }

    #[test]
    fn bag_jaccard_orders_similarity_sensibly() {
        let plan = path_token_bag(&[
            "PhotoSettings.aspx".to_string(),
            "PhotoSettings.aspx.cs".to_string(),
        ]);
        let similar = path_token_bag(&[
            "OrderSettings.aspx".to_string(),
            "OrderSettings.aspx.cs".to_string(),
            "Admin/menu.xml".to_string(),
        ]);
        let unrelated = path_token_bag(&["Scripts/jquery.min.js".to_string()]);
        assert!(bag_jaccard(&plan, &similar) > bag_jaccard(&plan, &unrelated));
        assert_eq!(bag_jaccard(&plan, &HashSet::new()), 0.0);
    }

    #[test]
    fn dir_ext_shape_reports_pattern() {
        assert_eq!(
            dir_ext_shape("Admin/Users.aspx").as_deref(),
            Some("Admin/*.aspx")
        );
        assert_eq!(dir_ext_shape("menu.xml").as_deref(), Some("*.xml"));
        assert_eq!(dir_ext_shape("Makefile"), None);
    }

    #[test]
    fn change_set_paths_captures_build_output_dirs() {
        // A build-output directory named `~.js/` itself ends in a code
        // extension. The extractor must greedily capture the FULL file, not
        // truncate to the directory — the bug that silently dropped co-change's
        // strongest partner (map.js) from every change set.
        let p =
            change_set_paths("- `modules/map/~.js/map.js` (45 co-changes with `Site/x/map.aspx`)");
        assert!(p.contains(&"modules/map/~.js/map.js".to_string()), "{p:?}");
        assert!(p.contains(&"site/x/map.aspx".to_string()), "{p:?}");
        // `.css` is a recognized extension and survives a `~.css/` dir.
        assert_eq!(
            change_set_paths("- modules/map/~.css/map.css"),
            vec!["modules/map/~.css/map.css".to_string()]
        );
        // Compound extensions still resolve to the longest valid match.
        assert_eq!(
            change_set_paths("foo/page.aspx.vb changed"),
            vec!["foo/page.aspx.vb".to_string()]
        );
        // A trailing sentence period must not be swallowed into the path.
        assert_eq!(
            change_set_paths("see modules/foo.cs."),
            vec!["modules/foo.cs".to_string()]
        );
        // A bare filename with no directory is still dropped (keep filter).
        assert!(change_set_paths("map.js").is_empty());
        // OpenAPI/config specs (yaml/yml) are recognized so co-change can surface
        // them (e.g. an API spec that co-changes with its controllers). json is
        // deliberately NOT recognized — package.json/tsconfig.json co-change with
        // too much to be useful signal.
        assert_eq!(
            change_set_paths("- `docs/openapi/ox-fiber.yaml`"),
            vec!["docs/openapi/ox-fiber.yaml".to_string()]
        );
    }

    #[test]
    fn concept_stems_plural_matches_singular_identifiers() {
        // A plural concept must match singular code identifiers (the PR1913 gap:
        // concept "roqentries" vs files RoqEntryService / RoqEntryDto).
        let stems = concept_stems("roqentries");
        assert!(stems.contains(&"roqentry".to_string()), "{stems:?}");
        assert!(matches_concept("RoqEntryService", &stems));
        assert!(matches_concept("RoqEntriesController", &stems)); // plural still matches
        // categories -> category
        assert!(concept_stems("categories").contains(&"category".to_string()));
        // a singular concept is unaffected (no bogus stem corrupts matching).
        assert!(matches_concept("InvoiceStatus", &concept_stems("invoice")));
    }

    #[test]
    fn story_concepts_prefer_distinctive_identifiers_over_crud_verbs() {
        // PR1913 shape: the narrative preamble is generic CRUD ("update ... status"),
        // the high-signal token is the API resource named in the endpoint path.
        // Generic CRUD verbs must not crowd it out of the top-3 concepts.
        // Real-shape: prose preamble BEFORE the endpoint path, so document-order
        // alone would pick invoice/status/customer and never reach the resource.
        let story = "As an admin I would like to be able to update RoQ invoice status \
                     from the API. The customer pulls the RoQ data every night from their \
                     own system and now wants to update the entries invoice status. We \
                     handle this as a specific command rather than a CRUD endpoint. \
                     POST api/v2/roqentries/{id}/setasbilled. Only admins may call it.";
        let c = extract_story_concepts(story);
        assert!(
            c.contains(&"roqentries".to_string()),
            "endpoint-path resource must surface despite being buried: {c:?}"
        );
        assert!(
            !c.contains(&"update".to_string()),
            "generic CRUD verb must be demoted: {c:?}"
        );
    }

    #[test]
    fn story_concepts_add_auth_concept_when_permission_gated() {
        // PR1913 shape: "Restricted to the Administrator role" — the auth layer
        // (role/user model) is the most-missed file. "role" surfaces it but sits
        // past the top-3 cutoff; the supplement must add it without displacing
        // the domain concepts.
        let c = extract_story_concepts(
            "POST api/v2/roqentries/{id}/setasbilled. Restricted to the Administrator role.",
        );
        assert!(
            c.contains(&"roqentries".to_string()),
            "domain concept kept: {c:?}"
        );
        assert!(c.contains(&"role".to_string()), "auth concept added: {c:?}");

        // Other auth phrasings also trigger.
        assert!(
            extract_story_concepts("the endpoint requires the Export permission")
                .contains(&"role".to_string())
        );
        assert!(
            extract_story_concepts("only authorized managers can approve")
                .contains(&"role".to_string())
        );

        // No auth language -> no auth concept (no false trigger / noise).
        let plain = extract_story_concepts("Show the invoice filter form on the report page");
        assert!(
            !plain.contains(&"role".to_string()),
            "no spurious auth concept: {plain:?}"
        );
    }

    #[test]
    fn story_concepts_reject_hash_and_id_garbage() {
        // PR1938 shape: a commit SHA + a board/username ID leaked into the story
        // and stole the concept slots, tanking recall. They must be rejected so
        // the real domain tokens surface.
        let c = extract_story_concepts(
            "patric0375 a778c06a field worker searches the RoQ code list by redovisning category",
        );
        assert!(
            !c.contains(&"a778c06a".to_string()),
            "commit hash must be rejected: {c:?}"
        );
        assert!(
            !c.contains(&"patric0375".to_string()),
            "username/id must be rejected: {c:?}"
        );
        // Garbage is gone; real word tokens fill the slots instead.
        assert!(
            !c.is_empty()
                && c.iter()
                    .all(|t| t.chars().filter(char::is_ascii_digit).count() < 3),
            "no 3+-digit garbage among concepts: {c:?}"
        );
        // Tokens with <3 digits (e.g. an api version) are still allowed.
        assert!(extract_story_concepts("update the apiv2 endpoint").contains(&"apiv2".to_string()));
    }

    #[test]
    fn transpile_pair_candidates_links_ts_and_committed_js() {
        // .ts source -> same-dir .js AND the ts/ -> ~.js/ output-dir swap.
        let c = transpile_pair_candidates("modules/map/ts/map.ts");
        assert!(c.contains(&"modules/map/ts/map.js".to_string()), "{c:?}");
        assert!(c.contains(&"modules/map/~.js/map.js".to_string()), "{c:?}");
        assert!(c.contains(&"modules/map/js/map.js".to_string()), "{c:?}");
        // reverse: committed bundle -> its .ts source (edit-the-bundle case).
        let r = transpile_pair_candidates("modules/map/~.js/map.js");
        assert!(r.contains(&"modules/map/ts/map.ts".to_string()), "{r:?}");
        assert!(r.contains(&"modules/map/~.js/map.ts".to_string()), "{r:?}");
        // .tsx/.jsx handled; basename preserved (incl. dotted stems).
        assert!(
            transpile_pair_candidates("a/b/Grid.view.tsx")
                .contains(&"a/b/Grid.view.js".to_string())
        );
        // non-TS/JS paths yield nothing (no false pairing for .json/.css/.vb).
        assert!(transpile_pair_candidates("a/b/config.json").is_empty());
        assert!(transpile_pair_candidates("a/b/page.aspx.vb").is_empty());
    }

    #[test]
    fn scaffold_stem_groups_a_feature_cohort() {
        // All files of one api-v2 feature must reduce to the same entity stem so
        // the cohort groups together. (RoqEntriesController plural, RoqEntry-In/-Out
        // DTOs, IRoqEntryService interface.)
        for f in [
            "RoqEntriesController",
            "RoqEntryService",
            "IRoqEntryService",
            "RoqEntry-In",
            "RoqEntry-Out",
            "RoqEntryQuery",
        ] {
            assert_eq!(scaffold_stem(f), "roqentry", "{f}");
        }
        // Leading-I is only stripped for an interface (I + Uppercase), not real words.
        assert_eq!(scaffold_stem("InstallationPlan"), "installationplan");
    }

    #[test]
    fn derive_scaffold_entity_prefers_index_grounded_ngram() {
        let index = vec![
            "modules/dashboard/pages/public/markerinspection/markerinspection.aspx.vb".to_string(),
            "App_Code/api-v2/Controllers/timeReporting/TimeReportEntriesController.vb".to_string(),
        ];
        let got = super::derive_scaffold_entity(
            "Add a REST API endpoint to expose marker inspection statuses",
            &index,
        );
        assert_eq!(got.as_deref(), Some("MarkerInspection"));
        // No grounded match → None (caller falls back to concepts).
        assert_eq!(
            super::derive_scaffold_entity("Add an API for flux capacitors", &index),
            None
        );
        // Pass 2 (the real PR1890 shape): the story names the domain
        // indirectly — the page DIR is markerinspection, the story only
        // says "inspection". Needs ≥2 files under the dir.
        let index2 = vec![
            "modules/dashboard/pages/public/markerinspection/markerinspection.aspx".to_string(),
            "modules/dashboard/pages/public/markerinspection/markerinspection.aspx.vb".to_string(),
        ];
        let got2 = super::derive_scaffold_entity(
            "As a user I would like an inspection report API endpoint like the Inspection module",
            &index2,
        );
        assert_eq!(got2.as_deref(), Some("MarkerInspection"));
    }

    #[test]
    fn propose_scaffold_paths_parameterizes_template() {
        let cohort = vec![
            "App_Code/api-v2/Controllers/timeReporting/TimeReportEntriesController.vb".to_string(),
            "App_Code/api-v2/Services/timeReporting/TimeReportEntryService.vb".to_string(),
            "App_Code/api-v2/Services/timeReporting/interfaces/ITimeReportEntryService.vb"
                .to_string(),
            "App_Code/api-v2/QueryParams/timeReporting/TimeReportEntryQuery.vb".to_string(),
            "App_Code/api-v2/DataTransferObjects/timeReporting/TimeReportEntry-Out.vb".to_string(),
        ];
        let got = super::propose_scaffold_paths(&cohort, "MarkerInspection");
        assert!(
            got.contains(
                &"App_Code/api-v2/Controllers/markerInspection/MarkerInspectionsController.vb"
                    .to_string()
            ),
            "{got:?}"
        );
        assert!(got.contains(
            &"App_Code/api-v2/Services/markerInspection/MarkerInspectionService.vb".to_string()
        ));
        assert!(
            got.contains(
                &"App_Code/api-v2/Services/markerInspection/interfaces/IMarkerInspectionService.vb"
                    .to_string()
            )
        );
        assert!(got.contains(
            &"App_Code/api-v2/QueryParams/markerInspection/MarkerInspectionQuery.vb".to_string()
        ));
        assert!(
            got.contains(
                &"App_Code/api-v2/DataTransferObjects/markerInspection/MarkerInspection-Out.vb"
                    .to_string()
            )
        );
    }

    #[test]
    fn find_analog_cohort_picks_a_complete_feature() {
        // Original-case paths (as the index provides them) — capital I matters.
        let index: Vec<String> = [
            "App_Code/api-v2/Controllers/Roq/RoqEntriesController.vb",
            "App_Code/api-v2/Services/Roq/RoqEntryService.vb",
            "App_Code/api-v2/Services/Roq/interfaces/IRoqEntryService.vb",
            "App_Code/api-v2/DataTransferObjects/Roq/RoqEntry-In.vb",
            "App_Code/api-v2/QueryParams/Roq/RoqEntryQuery.vb",
            // an unrelated, shallow group (should not win)
            "App_Code/grunddata/code/Projekt.vb",
            "App_Code/grunddata/code/Detaljer.vb",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let cohort = find_analog_cohort(&index, "controller").expect("cohort");
        let lc: Vec<String> = cohort.iter().map(|f| f.to_lowercase()).collect();
        assert!(
            lc.iter().any(|f| f.contains("roqentriescontroller")),
            "{cohort:?}"
        );
        assert!(
            lc.iter().any(|f| f.contains("iroqentryservice")),
            "interface dropped: {cohort:?}"
        );
        assert!(cohort.len() >= 5, "{cohort:?}");
        // cue filter: a cue absent from any cohort yields nothing.
        assert!(find_analog_cohort(&index, "nonexistentcue").is_none());
    }
}

// ── map_guards_and_settings + plan_user_story ────────────────────────────────

/// Settings-shaped table names: tables that store configuration rows.
pub(crate) fn is_settings_table_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    ["setting", "config", "option", "param", "preference"]
        .iter()
        .any(|p| lower.contains(p))
}

/// Does the story ask for ANALYTICS OVER TIME (time-to-X, aging, trends,
/// audit history)? Such stories need the domain entity's STATUS-CHANGE
/// HISTORY — which lives in log/history tables the concept/co-change arms
/// miss, because the story names the entity, not its log table.
pub(crate) fn story_asks_analytics_over_time(story: &str) -> bool {
    let s = story.to_lowercase();
    [
        "time to",
        "aging",
        "over time",
        "history",
        "audit",
        "trend",
        "duration",
        "elapsed",
        "performance of",
        "completion",
    ]
    .iter()
    .any(|t| s.contains(t))
}

/// Log/history/audit table-name shape (db_table node names are lowercase).
/// The "log" check is boundary-guarded so catalog/dialog/login/blog/logistics
/// don't match — the same false-positive class the token-boundary concept
/// matching in this file exists to prevent.
pub(crate) fn is_history_log_table_name(name: &str) -> bool {
    let n = name.to_lowercase();
    if n.contains("hist") || n.contains("audit") {
        return true;
    }
    n.match_indices("log").any(|(i, _)| {
        let pre = &n[..i];
        let post = &n[i + 3..];
        !(pre.ends_with("cata")
            || pre.ends_with("dia")
            || pre.ends_with('b')
            || post.starts_with("in")
            || post.starts_with("ist")
            || post.starts_with("ic"))
    })
}

/// Stopword filter for deterministic concept extraction from a user story.
pub(crate) fn extract_story_concepts(story: &str) -> Vec<String> {
    const STOPWORDS: &[&str] = &[
        "as",
        "an",
        "a",
        "the",
        "i",
        "we",
        "to",
        "of",
        "in",
        "on",
        "for",
        "and",
        "or",
        "is",
        "are",
        "be",
        "able",
        "would",
        "like",
        "want",
        "need",
        "needs",
        "should",
        "must",
        "can",
        "could",
        "set",
        "sets",
        "add",
        "adds",
        "new",
        "get",
        // Generic CRUD / UI verbs: almost never the DISTINCTIVE concept (the
        // domain noun or identifier is). Demoting them stops the narrative
        // preamble ("update invoice status") from crowding out the high-signal
        // token the story actually names (e.g. the API resource `roqentries`).
        // Keep DOMAIN nouns/features (report, export, import, search, filter,
        // map, invoice, status, …) as real concepts — only pure verbs here.
        // NOTE: only UNAMBIGUOUS generic verbs. Deliberately NOT stopwording
        // verbs that double as OciusX domain nouns — change ("Change Requests"),
        // view (DB/map views), select (SQL), process (business process), create
        // (creation flows) — to avoid hurting recall on those concepts.
        "update",
        "updates",
        "modify",
        "edit",
        "edits",
        "include",
        "includes",
        "included",
        "including",
        "display",
        "show",
        "shows",
        "manage",
        "enable",
        "enabled",
        "disable",
        "disabled",
        "toggle",
        "choose",
        "save",
        "remove",
        "delete",
        "handle",
        "avoid",
        "prevent",
        // Story-structure / Gherkin labels and HTTP verbs: scaffolding, not
        // domain concepts (they otherwise steal a top-3 slot from real tokens).
        "acceptance",
        "criteria",
        "scenario",
        "given",
        "post",
        "patch",
        "call",
        "calls",
        "have",
        "has",
        "when",
        "with",
        "that",
        "this",
        "it",
        "my",
        "our",
        "so",
        "user",
        "users",
        "admin",
        "admins",
        "administrator",
        "system",
        "page",
        "allow",
        "allows",
        "make",
        "required",
        "require",
        "minimum",
        "maximum",
        "number",
        "amount",
        "count",
        "via",
        "from",
        "into",
        "their",
        "them",
        "they",
        "if",
        "then",
        "also",
        "story",
    ];
    let candidate = |w: &str| -> Option<String> {
        let lower = w.to_lowercase();
        if lower.len() < 4 || STOPWORDS.contains(&lower.as_str()) {
            return None;
        }
        // Reject hash/ID garbage that leaks into PR-derived stories: commit SHAs
        // ("a778c06a"), usernames/board IDs ("patric0375"), ticket numbers. A real
        // domain concept almost never carries 3+ digits; such tokens otherwise
        // steal the limited concept slots and tank recall. Generic, no per-repo
        // names — robustness to noisy story input.
        if lower.chars().filter(char::is_ascii_digit).count() >= 3 {
            return None;
        }
        Some(lower)
    };
    let mut seen = HashSet::new();
    let mut out: Vec<String> = Vec::new();

    // 1. High-signal first: tokens the story puts in a resource/endpoint PATH
    //    (e.g. `api/v2/roqentries/{id}/setasbilled`) name a concrete code
    //    resource and are almost always the exact change target — but they sit
    //    deep in the description, past the narrative preamble, where a plain
    //    document-order scan (top-3) never reaches them. Stories without such
    //    paths skip this loop entirely (near-zero regression). Cap at 2 so
    //    ordinary prose concepts still get a slot.
    let path_re = regex::Regex::new(r"[/\\]([A-Za-z][A-Za-z0-9_]{3,})")
        .expect("extract_story_concepts path regex");
    for cap in path_re.captures_iter(story) {
        if out.len() >= 2 {
            break;
        }
        if let Some(c) = candidate(&cap[1])
            && seen.insert(c.clone())
        {
            out.push(c);
        }
    }

    // 2. Fill remaining slots in document order.
    for word in story.split(|c: char| !c.is_alphanumeric()) {
        if out.len() >= 3 {
            break;
        }
        if let Some(c) = candidate(word)
            && seen.insert(c.clone())
        {
            out.push(c);
        }
    }

    // 3. Auth/permission supplement (appended BEYOND the domain cap so it never
    //    displaces a domain concept). When a story gates the change on a role,
    //    permission or authorization — "restricted to the Administrator role",
    //    "requires X permission" — the auth/permission layer (the role/user model,
    //    authorize filters) is the file most often missed, because "admin"/
    //    "administrator" are stopwords and "role" sits past the top-3 cutoff. The
    //    concept `role` footprints to that layer (validated: surfaces the user/role
    //    model) in any ASP.NET app. Generic — no per-repo names.
    const AUTH_CUES: &[&str] = &[
        "role",
        "permission",
        "authoriz",
        "privilege",
        "access control",
        "rbac",
    ];
    let lower_story = story.to_lowercase();
    if AUTH_CUES.iter().any(|cue| lower_story.contains(cue)) {
        let auth = "role".to_string();
        if seen.insert(auth.clone()) {
            out.push(auth);
        }
    }
    out
}

impl Engram {
    pub async fn handle_map_guards_and_settings(
        &self,
        req: crate::models::MapGuardsAndSettingsRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let _ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;
        let scope = req
            .scope
            .as_deref()
            .map(|s| s.replace('\\', "/").to_lowercase());

        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let scope_b = scope.clone();
        let report = tokio::task::spawn_blocking(move || {
            let nodes = graph
                .query_nodes(&pid, None, None, None, crate::handlers::NODE_SCAN_LIMIT)
                .unwrap_or_default();

            let in_scope = |file_path: &str, name: &str| -> bool {
                match &scope_b {
                    None => true,
                    Some(s) => {
                        let fp = file_path.replace('\\', "/").to_lowercase();
                        fp.contains(s.as_str()) || name.to_lowercase() == *s
                    }
                }
            };

            let mut fn_total = 0usize;
            let mut guarded: Vec<(String, String, String, String)> = Vec::new();
            let mut unguarded: Vec<(String, String)> = Vec::new();
            let mut house: HashMap<String, usize> = HashMap::new();
            let mut roles_seen: HashMap<String, usize> = HashMap::new();
            let mut scoped_fn_ids: Vec<(String, String)> = Vec::new();
            let mut app_settings_defined = 0usize;
            let mut settings_tables: Vec<(String, String)> = Vec::new();

            for n in &nodes {
                let checks = n
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("permission_checks"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let roles = n
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("guard_roles"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if n.node_type == "function" {
                    if !checks.is_empty() {
                        for g in checks.split(';') {
                            *house.entry(g.to_string()).or_default() += 1;
                        }
                        for r in roles.split(';').filter(|r| !r.is_empty()) {
                            *roles_seen.entry(r.to_string()).or_default() += 1;
                        }
                    }
                    if in_scope(n.file_path.as_str(), &n.name) {
                        fn_total += 1;
                        scoped_fn_ids.push((n.node_id.clone(), n.name.clone()));
                        if checks.is_empty() {
                            unguarded.push((n.name.clone(), n.file_path.as_str().to_string()));
                        } else {
                            guarded.push((
                                n.name.clone(),
                                n.file_path.as_str().to_string(),
                                checks.to_string(),
                                roles.to_string(),
                            ));
                        }
                    }
                } else if n.node_type == "app_setting" {
                    app_settings_defined += 1;
                } else if n.node_type == "db_table" && is_settings_table_name(&n.name) {
                    settings_tables.push((n.node_id.clone(), n.name.clone()));
                }
            }

            // Settings consumed by in-scope functions (bounded).
            let mut settings_read: HashMap<String, Vec<String>> = HashMap::new();
            for (fn_id, fn_name) in scoped_fn_ids.iter().take(300) {
                if let Ok(neigh) = graph.neighbors(&pid, EdgeKind::ReadsSetting, fn_id, 20) {
                    for (target, _) in neigh {
                        let key = if let Some(rest) = target.strip_prefix("::") {
                            format!("{rest} (not in web.config — DB/env setting?)")
                        } else {
                            graph
                                .get_node(&pid, &target)
                                .ok()
                                .flatten()
                                .map(|n| n.name)
                                .unwrap_or(target)
                        };
                        settings_read.entry(key).or_default().push(fn_name.clone());
                    }
                }
            }

            // Settings-table consumer counts.
            let mut table_consumers: Vec<(String, usize)> = Vec::new();
            for (table_id, table_name) in settings_tables.iter().take(10) {
                let count = graph
                    .find_incoming_edges_with_kind(&pid, None, table_id, 500)
                    .map(|v| {
                        v.into_iter()
                            .filter(|(_, k, _)| {
                                matches!(
                                    k,
                                    EdgeKind::QueriesTable
                                        | EdgeKind::SqlCalls
                                        | EdgeKind::ReadsColumn
                                        | EdgeKind::StoredProcReadsTable
                                        | EdgeKind::StoredProcWritesTable
                                )
                            })
                            .count()
                    })
                    .unwrap_or(0);
                table_consumers.push((table_name.clone(), count));
            }

            (
                fn_total,
                guarded,
                unguarded,
                house,
                roles_seen,
                settings_read,
                table_consumers,
                app_settings_defined,
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let (
            fn_total,
            guarded,
            unguarded,
            house,
            roles_seen,
            settings_read,
            table_consumers,
            app_settings_count,
        ) = report;

        let mut out = format!(
            "# Guards & settings{}\n",
            scope
                .as_deref()
                .map(|s| format!(" — scope: {s}"))
                .unwrap_or_else(|| " — project-wide".into())
        );
        out.push_str(&format!(
            "\n## Guard parity\n{} of {} function(s) in scope have permission checks.\n",
            guarded.len(),
            fn_total
        ));
        if !guarded.is_empty() && !unguarded.is_empty() && scope.is_some() {
            out.push_str(
                "WARNING: mixed guarding in this scope — verify each unguarded function is \
                 intentionally public:\n",
            );
            for (name, file) in unguarded.iter().take(10) {
                out.push_str(&format!("  - UNGUARDED: {name} ({file})\n"));
            }
        }
        if !guarded.is_empty() {
            out.push_str("\n## Guarded functions in scope\n");
            for (name, file, checks, roles) in guarded.iter().take(20) {
                let role_str = if roles.is_empty() {
                    String::new()
                } else {
                    format!(" roles=[{roles}]")
                };
                out.push_str(&format!("- {name} ({file}) checks: {checks}{role_str}\n"));
            }
        }
        if !settings_read.is_empty() {
            out.push_str("\n## Settings read in scope\n");
            let mut keys: Vec<_> = settings_read.iter().collect();
            keys.sort_by_key(|(k, _)| k.to_string());
            for (key, fns) in keys.iter().take(20) {
                let mut consumers = fns.to_vec();
                consumers.sort();
                consumers.dedup();
                out.push_str(&format!("- {key} <- read by {}\n", consumers.join(", ")));
            }
        }
        if !table_consumers.is_empty() {
            out.push_str("\n## Settings-shaped tables (config stored in the DB)\n");
            for (table, count) in &table_consumers {
                out.push_str(&format!(
                    "- {table} — {count} code/SP consumer edge(s); changes to settings \
                     semantics ripple here\n"
                ));
            }
        }
        if !house.is_empty() {
            let mut house_sorted: Vec<_> = house.into_iter().collect();
            house_sorted.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            out.push_str("\n## House auth patterns (project-wide guard helpers)\n");
            for (g, c) in house_sorted.iter().take(8) {
                out.push_str(&format!("- {g} ({c} function(s))\n"));
            }
            if !roles_seen.is_empty() {
                let mut rs: Vec<_> = roles_seen.into_iter().collect();
                rs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
                let names: Vec<String> = rs
                    .into_iter()
                    .take(10)
                    .map(|(r, c)| format!("{r} ({c})"))
                    .collect();
                out.push_str(&format!("roles referenced: {}\n", names.join(", ")));
            }
        } else {
            out.push_str(
                "\n## House auth patterns\nNo guard calls detected anywhere — either the \
                 project predates this extraction (re-run update_project) or authorization \
                 is enforced purely via web.config (see map_auth_config).\n",
            );
        }
        out.push_str(&format!(
            "\napp settings defined in config files: {app_settings_count}\n\
             next: map_auth_config for web.config authorization rules; \
             get_table_schema for each settings table; trace_state_usage for role/session keys.\n"
        ));
        out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    /// One call: a weak user story in, an implementation brief out.
    pub async fn handle_plan_user_story(
        &self,
        req: crate::models::PlanUserStoryRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        if req.story.trim().is_empty() {
            return Err(McpError::invalid_params("story must not be empty", None));
        }
        let concepts: Vec<String> = match &req.concepts {
            Some(c) if !c.is_empty() => c.iter().take(3).cloned().collect(),
            _ => extract_story_concepts(&req.story),
        };

        let mut out = format!("# Implementation brief\n\nstory: {}\n", req.story.trim());
        out.push_str(&format!("concepts: {}\n", concepts.join(", ")));

        // Per-concept footprint (trimmed to keep the brief readable). One
        // failing concept must not kill the whole brief — degrade per-concept.
        for concept in &concepts {
            let sub = self
                .handle_get_concept_footprint(crate::models::GetConceptFootprintRequest {
                    project_id: req.project_id.clone(),
                    concept: concept.clone(),
                    max_per_group: 5,
                })
                .await;
            match sub {
                Ok(sub) => {
                    if let Some(text) = sub.content.first().and_then(|c| c.as_text()) {
                        let trimmed: String = text
                            .text
                            .lines()
                            .take_while(|l| !l.starts_with("next:") && !l.starts_with("---"))
                            .take(30)
                            .collect::<Vec<_>>()
                            .join("\n");
                        out.push_str(&format!("\n{trimmed}\n"));
                    }
                }
                Err(e) => {
                    out.push_str(&format!(
                        "\n(concept '{concept}': footprint unavailable — {})\n",
                        e.message
                    ));
                }
            }
        }

        // Pattern exemplars for the story's action.
        let sub = self
            .handle_find_implementation_pattern(crate::models::FindImplementationPatternRequest {
                project_id: req.project_id.clone(),
                pattern_query: concepts.join(" "),
                max_examples: 2,
            })
            .await?;
        if let Some(text) = sub.content.first().and_then(|c| c.as_text()) {
            let trimmed: String = text
                .text
                .lines()
                .take_while(|l| !l.starts_with("next:") && !l.starts_with("---"))
                .take(35)
                .collect::<Vec<_>>()
                .join("\n");
            out.push_str(&format!("\n{trimmed}\n"));
        }

        // Guards & settings overview (house patterns + settings tables).
        let sub = self
            .handle_map_guards_and_settings(crate::models::MapGuardsAndSettingsRequest {
                project_id: req.project_id.clone(),
                scope: None,
            })
            .await?;
        if let Some(text) = sub.content.first().and_then(|c| c.as_text()) {
            let mut lines: Vec<&str> = Vec::new();
            let mut keep = false;
            for l in text.text.lines() {
                if l.starts_with("## House auth patterns")
                    || l.starts_with("## Settings-shaped tables")
                {
                    keep = true;
                }
                if l.starts_with("next:") || l.starts_with("---") {
                    keep = false;
                }
                if keep {
                    lines.push(l);
                }
                if lines.len() > 25 {
                    break;
                }
            }
            if !lines.is_empty() {
                out.push_str(&format!("\n{}\n", lines.join("\n")));
            }
        }

        out.push_str(
            "\n## Checklist (work through ALL of it — partial implementations are how \
             features ship without their admin page)\n\
             - [ ] Storage: does the new value/entity follow the house pattern above \
             (web.config key vs settings-table row)? Mirror the exemplar.\n\
             - [ ] Admin/config UI: where do users SET this? Find the page that manages \
             the sibling setting and extend it (or clone its pattern).\n\
             - [ ] Enforcement: apply the new rule at EVERY touchpoint listed in the \
             concept footprint above — uploads, edits, imports, APIs.\n\
             - [ ] Guards: match the house auth patterns — call map_guards_and_settings \
             with scope=<your service/page> and fix any UNGUARDED finding.\n\
             - [ ] Messages/UX: error/validation text wherever the rule can reject input.\n\
             - [ ] Then run: find_similar_changes(files=<your planned file list>) and \
             close every 'MISSING from your set' item.\n\
             - [ ] Per touched method: check_edit_safety. Before commit: pre_commit_review.\n",
        );
        out.push_str(
            "\nnext: get_change_set(story=<this story>) for the RANKED FILE LIST \
             (concept+history+co-change+vector fused) — this brief explains the \
             domain; that tool names the files.\n",
        );
        let gen_ = self
            .get_active_generation(&req.project_id)
            .await
            .unwrap_or(1);
        out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }
}

#[cfg(test)]
mod guards_settings_tests {
    use super::*;

    #[test]
    fn settings_table_names_match_generic_shapes() {
        assert!(is_settings_table_name("ss_systemsettings"));
        assert!(is_settings_table_name("EmailSettings"));
        assert!(is_settings_table_name("app_config"));
        assert!(is_settings_table_name("UserOptions"));
        assert!(!is_settings_table_name("Orders"));
        assert!(!is_settings_table_name("Photos"));
    }

    #[test]
    fn history_log_table_names_match_and_reject_lookalikes() {
        for n in [
            "io_pr_iom_log",
            "order_status_history",
            "audit_trail",
            "pris_hist",
            "event_log",
            "log_entries",
        ] {
            assert!(is_history_log_table_name(n), "{n} should match");
        }
        for n in [
            "catalog",
            "dialog_settings",
            "user_login",
            "blog_posts",
            "logistics",
            "logic_rules",
            "orders",
        ] {
            assert!(!is_history_log_table_name(n), "{n} should NOT match");
        }
    }

    #[test]
    fn analytics_over_time_intent_detects_temporal_asks_only() {
        assert!(story_asks_analytics_over_time(
            "Report on TIME TO completion for purchase orders"
        ));
        assert!(story_asks_analytics_over_time(
            "Show order aging per customer"
        ));
        assert!(story_asks_analytics_over_time("visualize approval trends"));
        assert!(story_asks_analytics_over_time(
            "how long items stay in each status (duration)"
        ));
        assert!(!story_asks_analytics_over_time(
            "Add a new field to the customer form"
        ));
        assert!(!story_asks_analytics_over_time(
            "Rename the export button on the invoice page"
        ));
    }

    #[test]
    fn story_concepts_skip_stopwords_and_keep_domain_terms() {
        let c = extract_story_concepts(
            "As an admin I would like to set minimum number of photos required",
        );
        assert_eq!(c, vec!["photos".to_string()]);

        let c2 = extract_story_concepts("Allow adding users to a company via the public api");
        assert!(c2.contains(&"company".to_string()), "got {c2:?}");
        assert!(!c2.contains(&"users".to_string()), "users is generic");
    }
}

// ── generate_agent_integration ───────────────────────────────────────────────

/// Render the `.claude/rules/engram-workflow.md` content: the mandated loop.
pub(crate) fn render_workflow_rules(project_id: &str) -> String {
    format!(
        r#"# Engram workflow (generated by generate_agent_integration)

This project is indexed by Engram (project_id: `{project_id}`). These rules are
MANDATORY — the tools exist because skipping them is how regressions ship.

## For every feature request / user story
1. `plan_user_story(story=<verbatim request>)` — concepts, footprints, house
   patterns, checklist. START HERE, even for "simple" stories.
2. `get_concept_footprint(concept=...)` for every domain concept you touch —
   change ALL touchpoints or justify each one you skip.
3. `find_implementation_pattern(pattern_query=...)` — imitate the house
   pattern; do not invent new approaches for solved problems.
4. `map_guards_and_settings(scope=<your file/service>)` — match the sibling
   guards and settings handling for ANY new endpoint or admin operation.

## Before and after editing
- `begin_edit_session(planned_files=[...])` BEFORE the first edit — the
  expectation brief names the couplings your plan must cover.
- Before modifying a method: `get_method_edit_context` or `check_edit_safety`.
- `complete_edit_session(edited_files=[...])` when done — scope drift +
  completeness in one call.
- After choosing your file set: `find_similar_changes(files=[...])` and close
  every item under "MISSING from your set".

## Before every commit
- `detect_incomplete_changes(edited_files=[...])` — history and state wiring
  name the files you forgot. Touch them or justify each one.
- `pre_commit_review(diff_source="staged")` — fix or explicitly justify every
  finding (eleven gates, including guard_parity).
- `pre_push_audit(code=<your diff>, file_path=<file>)` — checks the change
  against the team's accumulated "what to avoid" knowledge (coding rules,
  copilot-instructions, CodeRabbit/SonarQube history, the recurring-issues
  board). Fix every rule it surfaces before pushing — these are mistakes the
  team has already flagged.
- If results ever look stale: `get_index_freshness`, then `update_project`.

## One-time project setup (so pre_push_audit has rules to check)
- For already-GENERIC sources, `ingest_quality_gates(source_path=..., source_type=...)`:
  `copilot` (copilot-instructions.md / coding-rules markdown) and `board` (DevOps
  recurring-issues export) — these are conventions, store them as-is.
- For raw FINDING corpora (a CodeRabbit or SonarQube export, often thousands of
  file/line-specific comments), use `distill_quality_gates(source_path=...,
  source_type=coderabbit|sonarqube)` instead. It clusters the findings and
  LLM-summarizes them into a small set of GENERIC rules ("the team keeps shipping
  un-parameterized SQL" -> "always parameterize") that apply to ANY change.
  Ingesting findings 1:1 is the wrong move — a finding tied to one file/line only
  ever helps someone editing that exact line; distillation is what makes the
  corpus reusable. Exclude/skip findings the team marked won't-fix.
- Re-run when a source is updated.
"#
    )
}

/// Render `.claude/settings.json` hook entries (Windows PowerShell or POSIX).
/// Hooks can't call MCP tools directly, so these are deterministic reminders:
/// their stdout is injected back into the conversation at exactly the moments
/// agents historically skip the workflow.
pub(crate) fn render_hooks_json(windows: bool) -> String {
    let (edit_cmd, stop_cmd) = if windows {
        (
            "powershell -NoProfile -Command \"Write-Output 'ENGRAM: source file changed. Before moving on: check_edit_safety for each touched method; map_guards_and_settings(scope=<file>) if you added/changed an endpoint. Before commit: detect_incomplete_changes(edited_files) + pre_commit_review(staged).'\"",
            "powershell -NoProfile -Command \"if (git status --porcelain 2>$null) { Write-Output 'ENGRAM: uncommitted changes present. Run detect_incomplete_changes(edited_files=<changed files>) and pre_commit_review(diff_source=staged) before finishing.' }\"",
        )
    } else {
        (
            "echo 'ENGRAM: source file changed. Before moving on: check_edit_safety for each touched method; map_guards_and_settings(scope=<file>) if you added/changed an endpoint. Before commit: detect_incomplete_changes(edited_files) + pre_commit_review(staged).'",
            "sh -c 'if [ -n \"$(git status --porcelain 2>/dev/null)\" ]; then echo \"ENGRAM: uncommitted changes present. Run detect_incomplete_changes(edited_files=<changed files>) and pre_commit_review(diff_source=staged) before finishing.\"; fi'",
        )
    };
    let v = serde_json::json!({
        "hooks": {
            "PostToolUse": [{
                "matcher": "Edit|Write|NotebookEdit",
                "hooks": [{ "type": "command", "command": edit_cmd }]
            }],
            "Stop": [{
                "matcher": "",
                "hooks": [{ "type": "command", "command": stop_cmd }]
            }]
        }
    });
    serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".to_string())
}

impl Engram {
    /// Emit (and optionally install) the Claude Code integration pack:
    /// workflow rules + reminder hooks that fire at the moments agents
    /// historically skip the Engram loop.
    pub async fn handle_generate_agent_integration(
        &self,
        req: crate::models::GenerateAgentIntegrationRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let rec = self.ensure_project_record(&req.project_id).await?;

        let rules = render_workflow_rules(&req.project_id);
        let hooks = render_hooks_json(req.windows);

        let mut out = String::with_capacity(4096);
        if req.write_files {
            let root = std::path::PathBuf::from(&rec.directory);
            let rules_rel = ".claude/rules/engram-workflow.md";
            let hooks_rel = ".claude/settings.json";
            let rules_path = engram_core::safe_join(&root, rules_rel)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            if let Some(dir) = rules_path.parent() {
                std::fs::create_dir_all(dir)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            }
            std::fs::write(&rules_path, &rules)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            out.push_str(&format!("wrote: {}\n", rules_path.display()));

            let hooks_path = engram_core::safe_join(&root, hooks_rel)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            if hooks_path.exists() {
                // Never clobber an existing settings.json — hooks must be
                // merged by a human (or the agent) deliberately.
                out.push_str(&format!(
                    "SKIPPED {} (exists) — merge the hooks block below manually.\n",
                    hooks_path.display()
                ));
                out.push_str(&format!(
                    "\n## hooks block to merge\n```json\n{hooks}\n```\n"
                ));
            } else {
                if let Some(dir) = hooks_path.parent() {
                    std::fs::create_dir_all(dir)
                        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                }
                std::fs::write(&hooks_path, &hooks)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                out.push_str(&format!("wrote: {}\n", hooks_path.display()));
            }
        } else {
            out.push_str(&format!(
                "# Engram agent integration pack (write_files=false — contents below)\n\n\
                 ## .claude/rules/engram-workflow.md\n```markdown\n{rules}\n```\n\n\
                 ## .claude/settings.json (merge if one exists)\n```json\n{hooks}\n```\n"
            ));
        }
        out.push_str(
            "\nNOTE: hooks are deterministic REMINDERS (their output is injected back \
             into the agent's context at edit/stop time). Hard blocking requires an \
             Engram CLI entry point — tracked in TODO.md.\n",
        );
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }
}

// ── ingest_review_findings ───────────────────────────────────────────────────

/// Normalize a SonarQube /api/issues/search export into findings.
/// Component strings are "projectKey:path/to/file.ext".
pub(crate) fn parse_sonarqube_issues(json: &str) -> Vec<crate::models::ReviewFindingIn> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(issues) = v.get("issues").and_then(|i| i.as_array()) else {
        return Vec::new();
    };
    issues
        .iter()
        .filter_map(|i| {
            let message = i.get("message")?.as_str()?.to_string();
            let file = i
                .get("component")
                .and_then(|c| c.as_str())
                .and_then(|c| c.split_once(':').map(|(_, p)| p.to_string()));
            Some(crate::models::ReviewFindingIn {
                file,
                rule: i.get("rule").and_then(|r| r.as_str()).map(String::from),
                message,
                severity: i.get("severity").and_then(|s| s.as_str()).map(String::from),
            })
        })
        .collect()
}

impl Engram {
    /// Feed external review findings (CTO comments, SonarQube issues) into
    /// the anti-pattern index — what a reviewer caught once becomes something
    /// immune_check and pre_commit_review catch automatically forever.
    pub async fn handle_ingest_review_findings(
        &self,
        req: crate::models::IngestReviewFindingsRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;

        let mut findings: Vec<crate::models::ReviewFindingIn> =
            req.findings.clone().unwrap_or_default();
        let mut sq_count = 0usize;
        if let Some(ref json) = req.sonarqube_json {
            let sq = parse_sonarqube_issues(json);
            sq_count = sq.len();
            findings.extend(sq);
        }
        if findings.is_empty() {
            return Err(McpError::invalid_params(
                "no findings provided — pass `findings` and/or `sonarqube_json` \
                 (the {\"issues\":[...]} export from /api/issues/search)",
                None,
            ));
        }

        // Build anti-pattern docs (same namespace immune_check and the
        // pre-commit antipattern gate search).
        let mut docs = Vec::with_capacity(findings.len());
        let now = crate::utils::now_ms();
        for f in &findings {
            let source = if f.rule.as_deref().unwrap_or("").contains(':') {
                "sonarqube"
            } else {
                "manual_review"
            };
            let content = format!(
                "REVIEW FINDING\nSource: {source}\nRule: {}\nFile: {}\nSeverity: {}\n\n{}",
                f.rule.as_deref().unwrap_or("unspecified"),
                f.file.as_deref().unwrap_or("project-wide"),
                f.severity.as_deref().unwrap_or("unspecified"),
                f.message
            );
            let ch = engram_core::ContentHash::compute(content.as_bytes());
            let rel = f
                .file
                .clone()
                .unwrap_or_else(|| "review/manual".to_string());
            let doc_id = engram_core::DocIdStr::compute(&rel, 0, 0, &ch).0;
            docs.push(engram_index::IndexDoc {
                generation: gen_,
                chunk_id: engram_index::chunk_id_from_content_hash(&ch),
                path: engram_core::RelPath::new(&rel),
                language: "text".into(),
                content,
                namespace: "antipattern".into(),
                author: Some(source.to_string()),
                timestamp: Some(now / 1000),
                start_line: 0,
                end_line: 0,
                doc_id,
                content_hash: ch.0,
            });
        }
        let doc_count = docs.len();
        ps.search
            .index_docs(
                &req.project_id,
                &docs,
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // Promote severe, file-scoped findings to repo rules so they're
        // injected into get_chunk and checked by the immune gate.
        let mut promoted = 0usize;
        if req.promote_rules {
            let reg = self.state.registry.clone();
            let pid = req.project_id.clone();
            let candidates: Vec<(String, String)> = findings
                .iter()
                .filter(|f| {
                    f.file.is_some()
                        && matches!(
                            f.severity.as_deref().map(|s| s.to_lowercase()).as_deref(),
                            Some("blocker") | Some("critical") | Some("high")
                        )
                })
                .map(|f| {
                    let text: String = f.message.chars().take(300).collect();
                    (f.file.clone().unwrap_or_default(), text)
                })
                .collect();
            promoted = candidates.len();
            if !candidates.is_empty() {
                tokio::task::spawn_blocking(move || {
                    for (file, text) in candidates {
                        let hash =
                            engram_core::ContentHash::compute(format!("{file}:{text}").as_bytes());
                        let rule = engram_core::registry::RepoRule {
                            rule_id: format!("review_{}", &hash.0[..12]),
                            file_pattern: file,
                            rule_text: text,
                            priority: 2,
                            updated_at_ms: now,
                        };
                        let _ = reg.put_repo_rule(&pid, &rule);
                    }
                })
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            }
        }

        let mut out = format!(
            "Ingested {doc_count} review finding(s) into the anti-pattern index \
             ({sq_count} from SonarQube), {promoted} promoted to repo rules.\n\
             They are now checked by: immune_check (scoring), pre_commit_review's \
             antipattern + immune gates, and injected into get_chunk for the \
             affected files.\n"
        );
        out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }
}

#[cfg(test)]
mod review_ingest_tests {
    use super::*;

    #[test]
    fn sonarqube_issue_export_parses_to_findings() {
        let json = r#"{"issues":[
            {"component":"ociusx:Site/App_Code/dal/users.vb","rule":"vbnet:S2077",
             "message":"Make sure using a dynamically formatted SQL query is safe here.",
             "severity":"CRITICAL","line":42},
            {"component":"ociusx:Site/Default.aspx.vb","rule":"vbnet:S1481",
             "message":"Remove the unused local variable 'x'.","severity":"MINOR"}
        ]}"#;
        let findings = parse_sonarqube_issues(json);
        assert_eq!(findings.len(), 2);
        assert_eq!(
            findings[0].file.as_deref(),
            Some("Site/App_Code/dal/users.vb")
        );
        assert_eq!(findings[0].severity.as_deref(), Some("CRITICAL"));
        assert!(findings[0].message.contains("dynamically formatted SQL"));
    }

    #[test]
    fn malformed_sonarqube_json_yields_empty_not_panic() {
        assert!(parse_sonarqube_issues("not json").is_empty());
        assert!(parse_sonarqube_issues("{\"foo\":1}").is_empty());
    }
}

/// Extract repo-relative source-file paths from a planning tool's text output
/// (prose paths AND `kind:PATH:name:line` node-ids). Compound .NET code-behind
/// extensions are matched first so `foo.aspx.vb` is not truncated to `foo.aspx`.
fn change_set_paths(text: &str) -> Vec<String> {
    // GREEDY match (`*`, not `*?`): consume the whole path-char run, then
    // backtrack to the LAST valid extension. Lazy matching stopped at the first
    // extension-like token — e.g. a build-output directory named `~.js/` looks
    // like a `.js` file, so `modules/map/~.js/map.js` truncated to the dir.
    // The class also admits `~ @ +` (legal in build-output / scoped dirs) so
    // such paths aren't fragmented into slashless pieces the `keep` filter drops.
    let re = regex::Regex::new(
        r"(?i)[\w./\\~@+-]*\.(?:aspx\.vb|ascx\.vb|asax\.vb|asmx\.vb|ashx\.vb|svc\.vb|master\.vb|aspx\.cs|ascx\.cs|asax\.cs|asmx\.cs|ashx\.cs|svc\.cs|master\.cs|aspx|ascx|asax|ashx|asmx|svc|master|vb|css|cs|ts|tsx|js|jsx|sql|config|vbhtml|cshtml|resx|html|yaml|yml)\b",
    )
    .expect("change_set_paths regex");
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for m in re.find_iter(text) {
        let mut p = m.as_str().replace('\\', "/").to_lowercase();
        while p.contains("//") {
            p = p.replace("//", "/");
        }
        let p = p.trim_start_matches('/').to_string();
        if p.starts_with("http")
            || p.starts_with("c:")
            || p.starts_with("f:")
            || p.starts_with("d:")
        {
            continue;
        }
        let keep = p.contains('/') || p.ends_with(".config") || p.ends_with(".asax");
        if keep && seen.insert(p.clone()) {
            out.push(p);
        }
    }
    out
}

/// Architectural role suffix tokens shared across enterprise codebases. Used to
/// reduce a filename to its ENTITY stem so sibling files of one feature group
/// together (RoqEntriesController, RoqEntryService, IRoqEntryService, RoqEntry-In
/// all -> "roqentry"). Generic OO/enterprise vocabulary, not per-repo.
const SCAFFOLD_ROLE_SUFFIXES: &[&str] = &[
    "controller",
    "service",
    "repository",
    "manager",
    "provider",
    "factory",
    "handler",
    "validator",
    "mapper",
    "builder",
    "helper",
    "extensions",
    "job",
    "worker",
    "listener",
    "command",
    "query",
    "request",
    "response",
    "viewmodel",
    "model",
    "dto",
    "config",
];

/// Reduce a filename (no extension) to its entity stem: drop a leading interface
/// `I` (I + Uppercase), a trailing `-In`/`-Out` DTO marker, one trailing role
/// suffix, then singularize. Lowercased. Generic.
fn scaffold_stem(file_no_ext: &str) -> String {
    let mut s = file_no_ext;
    if s.len() > 2 && s.starts_with('I') && s.as_bytes().get(1).is_some_and(u8::is_ascii_uppercase)
    {
        s = &s[1..];
    }
    let mut lower = s.to_lowercase();
    for marker in ["-in", "-out", "_in", "_out"] {
        if let Some(p) = lower.strip_suffix(marker) {
            lower = p.to_string();
            break;
        }
    }
    for suf in SCAFFOLD_ROLE_SUFFIXES {
        if lower.len() > suf.len() + 2
            && let Some(p) = lower.strip_suffix(suf)
        {
            lower = p.to_string();
            break;
        }
    }
    let lower = lower.trim_end_matches(['-', '_', '.']);
    if let Some(p) = lower.strip_suffix("ies") {
        format!("{p}y")
    } else if lower.ends_with("ss") {
        lower.to_string()
    } else {
        lower.strip_suffix('s').unwrap_or(lower).to_string()
    }
}

/// Find the existing feature COHORT most useful as a scaffold template: an
/// (area, entity-stem) whose files span >=3 distinct directories AND (when a cue
/// token is given, e.g. "controller" for an API story) contains a file matching
/// the cue. Returns the cohort's real file paths (the structural template the
/// agent should mirror for a NEW feature). Generic — learns the repo's own
/// convention; no hardcoded layout. `index` = web-root-stripped paths in their
/// ORIGINAL case (case matters for the interface `I`-prefix detection); grouping
/// keys are lowercased internally and the cue match is case-insensitive.
/// Pick the scaffold entity from the story by grounding it in the index:
/// the longest story n-gram (3→1 words) whose compact form names an
/// existing path segment wins — a new API domain almost always mirrors an
/// existing page/dir name (markerinspection page → MarkerInspection API),
/// so "marker inspection" beats the bare concept "inspection".
fn derive_scaffold_entity(story: &str, index: &[String]) -> Option<String> {
    let words: Vec<String> = story
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3)
        .map(str::to_string)
        .collect();
    let seg_set: std::collections::HashSet<String> = index
        .iter()
        .flat_map(|p| p.split('/'))
        .map(|s| s.split('.').next().unwrap_or(s).to_lowercase())
        .collect();
    // Candidates from BOTH passes compete; the most SPECIFIC (longest)
    // grounded segment wins. Returning early on an exact unigram match
    // ("inspection") hid the more specific feature dir (markerinspection).
    let mut best: Option<(usize, String)> = None; // (segment len, pascal)
    let mut offer = |len: usize, pascal: String| {
        if best.as_ref().is_none_or(|(l, _)| len > *l) {
            best = Some((len, pascal));
        }
    };
    let cap = |s: &str| -> String {
        let mut s = s.to_string();
        if let Some(f) = s.get_mut(0..1) {
            f.make_ascii_uppercase();
        }
        s
    };
    // Pass 1: exact story n-gram = path segment.
    for n in (1..=3usize).rev() {
        if words.len() < n {
            continue;
        }
        for win in words.windows(n) {
            let compact: String = win.concat();
            if compact.len() >= 6 && seg_set.contains(&compact) {
                offer(compact.len(), win.iter().map(|w| cap(w)).collect());
            }
        }
    }
    // Pass 2: the story often names the DOMAIN indirectly ("the Inspection
    // module in the dashboard") while the codebase dir is more specific
    // (markerinspection). Feature DIRECTORIES (≥2 files under them) whose
    // name CONTAINS a story word count too — but only when the matched
    // word covers at least half the segment, so "report" cannot claim
    // reportingofquantities.
    let mut dir_counts: std::collections::HashMap<&str, usize> = Default::default();
    for p in index {
        if let Some((dirs, _file)) = p.rsplit_once('/') {
            for seg in dirs.split('/') {
                *dir_counts.entry(seg).or_default() += 1;
            }
        }
    }
    for (seg, count) in dir_counts {
        if count < 2 || seg.len() < 8 {
            continue;
        }
        let seg_l = seg.to_lowercase();
        for w in &words {
            if w.len() < 6
                || seg_l == *w
                || !seg_l.contains(w.as_str())
                || w.len() * 2 < seg_l.len()
            {
                continue;
            }
            let idx = seg_l.find(w.as_str()).unwrap_or(0);
            let pascal = format!(
                "{}{}{}",
                cap(&seg_l[..idx]),
                cap(w),
                cap(&seg_l[idx + w.len()..])
            );
            offer(seg.len(), pascal);
        }
    }
    best.map(|(_, p)| p)
}

/// Parameterize a scaffold template with the story's entity: rewrite each
/// cohort path's domain directory (the camelCase feature dir) and basename
/// entity stem so the agent gets the CONCRETE files to create, not just an
/// example family to reverse-engineer. PR1890 showed agents (and the
/// dossier recall metric) need the target paths spelled out.
fn propose_scaffold_paths(cohort: &[String], entity_pascal: &str) -> Vec<String> {
    if entity_pascal.is_empty() {
        return Vec::new();
    }
    let mut camel = entity_pascal.to_string();
    if let Some(c) = camel.get_mut(0..1) {
        c.make_ascii_lowercase();
    }
    const ROLE_DIRS: [&str; 8] = [
        "app_code",
        "api-v2",
        "controllers",
        "services",
        "datatransferobjects",
        "queryparams",
        "interfaces",
        "site",
    ];
    const MARKERS: [&str; 5] = ["Controller", "Service", "Query", "-In", "-Out"];
    let mut out = Vec::new();
    for f in cohort {
        let mut segs: Vec<String> = f.split('/').map(str::to_string).collect();
        let Some(base) = segs.pop() else { continue };
        // Replace the feature-domain directory segment.
        for s in segs.iter_mut() {
            if !ROLE_DIRS.contains(&s.to_lowercase().as_str())
                && s.chars().next().is_some_and(|c| c.is_lowercase())
            {
                *s = camel.clone();
            }
        }
        // Rewrite the basename's entity stem, keeping the role suffix.
        let (stem, ext) = base.split_once('.').unwrap_or((base.as_str(), "vb"));
        let mut new_base = None;
        for m in MARKERS {
            if let Some(idx) = stem.find(m) {
                let prefix = &stem[..idx];
                let iface = prefix.len() >= 2
                    && prefix.starts_with('I')
                    && prefix.chars().nth(1).is_some_and(|c| c.is_uppercase());
                let plural = m == "Controller" && prefix.ends_with('s');
                new_base = Some(format!(
                    "{}{}{}{}.{}",
                    if iface { "I" } else { "" },
                    entity_pascal,
                    if plural { "s" } else { "" },
                    &stem[idx..],
                    ext
                ));
                break;
            }
        }
        let Some(nb) = new_base else { continue };
        segs.push(nb);
        out.push(segs.join("/"));
    }
    out.sort();
    out.dedup();
    out
}

fn find_analog_cohort(index: &[String], cue: &str) -> Option<Vec<String>> {
    let cue = cue.to_lowercase();
    let mut groups: HashMap<(String, String), (HashSet<String>, Vec<String>)> = HashMap::new();
    for f in index {
        let segs: Vec<&str> = f.split('/').filter(|s| !s.is_empty()).collect();
        if segs.len() < 3 {
            continue;
        }
        let area2 = format!("{}/{}", segs[0].to_lowercase(), segs[1].to_lowercase());
        let fname = segs[segs.len() - 1];
        let base = fname.rsplit_once('.').map(|(b, _)| b).unwrap_or(fname);
        let stem = scaffold_stem(base); // uses original case for the I-prefix rule
        if stem.len() < 4 {
            continue;
        }
        let parent = segs[..segs.len() - 1].join("/").to_lowercase();
        let e = groups.entry((area2, stem)).or_default();
        e.0.insert(parent);
        e.1.push(f.clone());
    }
    let mut cands: Vec<(usize, Vec<String>)> = groups
        .into_values()
        .filter(|(dirs, files)| {
            dirs.len() >= 3
                && (cue.is_empty() || files.iter().any(|f| f.to_lowercase().contains(&cue)))
        })
        .map(|(dirs, mut files)| {
            files.sort();
            (dirs.len(), files)
        })
        .collect();
    cands.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.len().cmp(&a.1.len())));
    cands.into_iter().next().map(|(_, files)| files)
}

/// Co-change-first tier for ranking: history/co-change (the most predictive
/// signal) ranks above multi-arm, above concept-only, above graph-only.
fn change_set_tier(sigs: &BTreeSet<&'static str>) -> u8 {
    let golden = sigs.contains("cochange") || sigs.contains("history");
    if golden && sigs.len() >= 2 {
        0
    } else if golden {
        1
    } else if sigs.len() >= 2 {
        2
    } else if sigs.contains("concept") {
        3
    } else {
        4
    }
}

/// Split an identifier into lowercase tokens on camelCase, snake_case and
/// kebab-case boundaries (plus letter/digit transitions), handling acronym
/// runs: `ddlBillingStatusMainContractor` -> [ddl, billing, status, main,
/// contractor]; `btn_from_date` -> [btn, from, date].
pub(crate) fn split_symmetric_name_tokens(name: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for part in name.split(|c: char| !c.is_alphanumeric()) {
        if part.is_empty() {
            continue;
        }
        let chars: Vec<char> = part.chars().collect();
        let mut cur = String::new();
        for (i, &c) in chars.iter().enumerate() {
            let boundary = i > 0
                && ((c.is_uppercase() && chars[i - 1].is_lowercase())
                    // Acronym-run end: "HTMLParser" -> HTML | Parser.
                    || (c.is_uppercase()
                        && chars[i - 1].is_uppercase()
                        && chars.get(i + 1).is_some_and(|n| n.is_lowercase()))
                    || (c.is_ascii_digit() != chars[i - 1].is_ascii_digit()));
            if boundary && !cur.is_empty() {
                tokens.push(cur.to_lowercase());
                cur.clear();
            }
            cur.push(c);
        }
        if !cur.is_empty() {
            tokens.push(cur.to_lowercase());
        }
    }
    tokens
}

/// Symmetric sibling pairs: two identifiers pair when their token sequences
/// (camelCase/snake_case split) have EQUAL length and differ in EXACTLY ONE
/// position, sharing at least 2 tokens. This is the name-shape of PAIRED
/// WebForms/WinForms controls and handlers (Main/Sub, Left/Right, From/To,
/// Start/End, Sender/Receiver) — a change to one side usually requires
/// deciding the twin's behavior. Dedupes (a,b)/(b,a); capped at 6 pairs.
pub(crate) fn symmetric_sibling_pairs(names: &[String]) -> Vec<(String, String)> {
    const MAX_PAIRS: usize = 6;
    let mut seen: HashSet<&str> = HashSet::new();
    let uniq: Vec<&String> = names.iter().filter(|n| seen.insert(n.as_str())).collect();
    let toks: Vec<Vec<String>> = uniq
        .iter()
        .map(|n| split_symmetric_name_tokens(n))
        .collect();
    let mut out: Vec<(String, String)> = Vec::new();
    for i in 0..uniq.len() {
        // Equal length + exactly one differing position => shared = len-1,
        // so "share >= 2 tokens" is equivalent to len >= 3.
        if toks[i].len() < 3 {
            continue;
        }
        for j in (i + 1)..uniq.len() {
            if out.len() >= MAX_PAIRS {
                return out;
            }
            if toks[i].len() != toks[j].len() {
                continue;
            }
            let diff = toks[i]
                .iter()
                .zip(toks[j].iter())
                .filter(|(a, b)| a != b)
                .count();
            if diff == 1 {
                out.push((uniq[i].clone(), uniq[j].clone()));
            }
        }
    }
    out
}

#[cfg(test)]
mod symmetric_sibling_tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn tokenizer_splits_camel_snake_and_acronyms() {
        assert_eq!(
            split_symmetric_name_tokens("ddlBillingStatusMainContractor"),
            vec!["ddl", "billing", "status", "main", "contractor"]
        );
        assert_eq!(
            split_symmetric_name_tokens("btn_from_date"),
            vec!["btn", "from", "date"]
        );
        assert_eq!(
            split_symmetric_name_tokens("HTMLParser"),
            vec!["html", "parser"]
        );
    }

    #[test]
    fn pairs_camel_case_one_token_apart() {
        let pairs = symmetric_sibling_pairs(&names(&[
            "ddlBillingStatusMainContractor",
            "ddlBillingStatusSubContractor",
        ]));
        assert_eq!(
            pairs,
            vec![(
                "ddlBillingStatusMainContractor".to_string(),
                "ddlBillingStatusSubContractor".to_string()
            )]
        );
    }

    #[test]
    fn pairs_snake_case_one_token_apart() {
        let pairs = symmetric_sibling_pairs(&names(&["btn_from_date", "btn_to_date"]));
        assert_eq!(
            pairs,
            vec![("btn_from_date".to_string(), "btn_to_date".to_string())]
        );
    }

    #[test]
    fn rejects_unrelated_and_multi_token_differences() {
        // Unrelated names: no pair.
        assert!(symmetric_sibling_pairs(&names(&["btnSave", "gvOrders", "lblTitle"])).is_empty());
        // Two differing token positions: no pair.
        assert!(
            symmetric_sibling_pairs(&names(&[
                "ddlBillingStatusMainContractor",
                "ddlPaymentStatusSubContractor"
            ]))
            .is_empty()
        );
        // Different token counts: no pair.
        assert!(
            symmetric_sibling_pairs(&names(&["btn_from_date", "btn_to_date_extra"])).is_empty()
        );
        // Only 2 tokens => fewer than 2 shared tokens: no pair.
        assert!(symmetric_sibling_pairs(&names(&["btnFrom", "btnTo"])).is_empty());
    }

    #[test]
    fn dedupes_names_and_caps_at_six_pairs() {
        // Duplicate name must not pair with itself.
        assert!(symmetric_sibling_pairs(&names(&["btn_from_date", "btn_from_date"])).is_empty());
        // 8 mutually-pairing names (differ only in the middle token) would
        // produce C(8,2)=28 ordered-deduped pairs; cap holds at 6.
        let many: Vec<String> = (0..8).map(|i| format!("pnlFilterVariant{i}Side")).collect();
        let pairs = symmetric_sibling_pairs(&many);
        assert_eq!(pairs.len(), 6);
        // (a,b)/(b,a) dedupe: every pair is emitted in input order, once.
        for (a, b) in &pairs {
            assert!(many.iter().position(|n| n == a) < many.iter().position(|n| n == b));
        }
    }
}

/// Render the change set: layer-grouped, co-change-first, with a completeness
/// checklist. Golden (co-change/history) files are never capped; the concept/
/// graph tail is capped per layer so flood can't crowd out real companions.
fn render_change_set(
    story: &str,
    concepts: &[String],
    prov: &BTreeMap<String, BTreeSet<&'static str>>,
    temporal_section: Option<&str>,
    sibling_section: Option<&str>,
) -> String {
    const LAYERS: &[(&str, &[&str])] = &[
        (
            "Server (VB / code-behind / markup)",
            &[
                ".vb", ".cs", ".aspx", ".ascx", ".master", ".asmx", ".ashx", ".svc", ".asax",
            ],
        ),
        (
            "Client (TypeScript / JavaScript)",
            &[".ts", ".tsx", ".js", ".jsx"],
        ),
        ("Resources (.resx — translate EVERY language)", &[".resx"]),
        ("Data (SQL)", &[".sql"]),
        (
            "Markup / styles / config",
            &[".html", ".css", ".config", ".vbhtml", ".cshtml"],
        ),
    ];
    let layer_of = |p: &str| -> usize {
        for (i, (_, exts)) in LAYERS.iter().enumerate() {
            if exts.iter().any(|e| p.ends_with(*e)) {
                return i;
            }
        }
        LAYERS.len()
    };
    let mut s = String::new();
    s.push_str("# Change set — candidate files for this story\n\n");
    s.push_str(&format!(
        "story: {story}\nconcepts: {}\n\n",
        concepts.join(", ")
    ));
    s.push_str(
        "Ranked by corroboration — git CO-CHANGE / history first (files that \
         historically shipped together with this kind of work), then concept/graph. \
         A starting map, not exhaustive: verify each against the code and add the \
         files it misses.\n\n",
    );
    // A live A/B showed strong planners SKIP ranked candidates whose signal
    // they don't understand (the source page edit was ranked and still
    // dropped). Spell out what each tag proves and raise the bar for
    // skipping the evidence-backed ones.
    s.push_str(
        "Signal legend — [cochange]: this file SHIPPED TOGETHER with other \
         candidates in past MERGED changes of this kind (strongest evidence; \
         skipping one requires POSITIVE evidence of irrelevance grounded in the code - and a .ts with its committed .js bundle listed together is part of the change until PROVEN otherwise (a live A/B dismissed exactly that pair as bleed-through; it was real)). [history]: past \
         commits in this story's domain touched it. [concept]: name/content \
         matches the story's concepts. [semantic]/[graph]: embedding or \
         dependency-graph association (weakest — verify before trusting).\n\n",
    );
    // Temporal-analytics section renders BEFORE the checklist so the
    // checklist's "log/history tables above" pointer is literally true.
    if let Some(sec) = temporal_section {
        s.push_str(sec);
    }
    // Symmetric-sibling section likewise renders BEFORE the checklist so the
    // checklist's "pair(s) above" pointer is literally true.
    if let Some(sec) = sibling_section {
        s.push_str(sec);
    }
    s.push_str(
        "## Completeness checklist - REQUIRED decision points\n\
         Treat EACH item below as a decision you must make and state \
         explicitly (the highest-scoring plans in live A/Bs stated every \
         decision; silent omissions were the top failure mode):\n",
    );
    s.push_str(
        "- Every page touched: edit BOTH the .aspx/.ascx markup AND its \
         .aspx.vb/.ascx.vb code-behind (and .designer.vb if present).\n",
    );
    s.push_str(
        "- Every user-facing string: update the .resx in EVERY language present, \
         not only the default - and use the resx FAMILY the ranked candidates \
         show (co-change evidence names the exact family, e.g. text vs label \
         vs control); do not add families on principle.\n",
    );
    s.push_str(
        "- UI look/feel changes: check whether the surface pulls page-specific \
         stylesheets (map.css-style files) - cross-cutting UI work in this \
         codebase frequently ships CSS alongside the TS/bundles.\n",
    );
    s.push_str("- Every schema / setting / column change: include the SQL migration.\n");
    s.push_str("- Every .ts that compiles into a committed bundle: update the bundle.\n");
    if temporal_section.is_some() {
        s.push_str(
            "- Analytics-over-time ask (time-to-X/aging/history): check the domain \
             entity's log/history tables above — computing durations needs the \
             status-change history, and teams typically expose it.\n",
        );
    }
    if sibling_section.is_some() {
        s.push_str(
            "- Symmetric pair(s) above: apply the change to BOTH sides or explicitly \
             state their interaction (mutually exclusive / mirrored / independent).\n",
        );
    }
    s.push_str(
        "- Story changes DEFAULT behaviour (UI or otherwise)? Decide EXPLICITLY \
         whether this team's convention calls for a NEW SETTING to gate it - \
         mature codebases frequently ship behaviour changes as configurable \
         toggles (settings store + admin UI + default), and stories rarely say \
         so. Check how similar merged work did it (the exemplars below).\n\n",
    );
    s.push_str("## Candidate files (grouped by layer — order within a group is NOT priority)\n");

    let layer_names: Vec<&str> = LAYERS
        .iter()
        .map(|(n, _)| *n)
        .chain(std::iter::once("Other"))
        .collect();
    for (li, lname) in layer_names.iter().enumerate() {
        let mut items: Vec<(&String, &BTreeSet<&'static str>)> =
            prov.iter().filter(|(p, _)| layer_of(p) == li).collect();
        if items.is_empty() {
            continue;
        }
        items.sort_by(|a, b| {
            change_set_tier(a.1)
                .cmp(&change_set_tier(b.1))
                .then(a.0.matches('/').count().cmp(&b.0.matches('/').count()))
                .then(a.0.cmp(b.0))
        });
        s.push_str(&format!("\n**{lname}:**\n"));
        let mut tail = 0usize;
        for (p, sigs) in &items {
            // Exempt from the concept/graph tail cap: (1) "vtop" top-rank vector
            // hits (the cross-language bridge, bounded by the vector arm); (2)
            // "family" resx language siblings (an atomic localized set — never show
            // it half-complete). Both are bounded, so they can't flood; without the
            // exemption a busy layer's concept matches crowd them out (tier-4).
            let exempt = sigs.contains("vtop") || sigs.contains("family");
            if change_set_tier(sigs) >= 2 && !exempt {
                tail += 1;
                if tail > 18 {
                    continue;
                }
            }
            // Display: "vtop" is just a high-rank vector hit; "family" is an
            // internal completeness marker — neither belongs in the signal labels.
            let labels: Vec<&str> = sigs
                .iter()
                .filter(|s| **s != "family")
                .map(|s| if *s == "vtop" { "vector" } else { *s })
                .collect();
            s.push_str(&format!("- `{p}`  [{}]\n", labels.join("|")));
        }
    }
    s
}

impl Engram {
    /// ONE call: the ranked, co-change-confirmed, family-aware change set for a
    /// user story. The OciusX-validated recipe ported into Engram — concept
    /// footprint + git co-change, co-change/history ranked first, vendor noise
    /// filtered. Generic; no per-repo hardcoding.
    pub async fn handle_get_change_set(
        &self,
        req: crate::models::GetChangeSetRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        if req.story.trim().is_empty() {
            return Err(McpError::invalid_params("story must not be empty", None));
        }
        let mut concepts: Vec<String> = match &req.concepts {
            Some(c) if !c.is_empty() => c.iter().take(3).cloned().collect(),
            _ => extract_story_concepts(&req.story),
        };

        // KB language bridge: the team's wiki/docs corpus (memory_bank
        // sections) frequently names the same feature in BOTH the story's
        // language and the code's (English story "resource planning" vs
        // Swedish identifiers "resurs*"). Mine the top sections matching
        // the story for identifier-ish tokens that (a) recur across
        // sections, (b) are NOT already reachable from the story's own
        // concepts, and (c) actually exist in the code graph - and add up
        // to TWO of them as extra concepts. Generic: no language tables.
        if let Ok(ps) = self.ensure_project_runtime(&req.project_id).await {
            let q = engram_index::HybridQuery {
                project_id: req.project_id.clone(),
                namespace: engram_core::namespaces::NAMESPACE_MEMORY_BANK.into(),
                generation: 0,
                text: req.story.clone(),
                top_k: 3,
                fts_mode: "loose".into(),
                include_path_prefixes: None,
                exclude_path_prefixes: None,
                language_filters: None,
                author_filter: None,
                date_after: None,
                date_before: None,
                use_mmr: false,
            };
            let engine = ps.search.clone();
            let hits = tokio::task::spawn_blocking(move || engine.lexical_search(&q))
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or_default();
            let mut counts: std::collections::HashMap<String, usize> = Default::default();
            for h in hits.iter().take(3) {
                let Ok(Some((_, _, content, _, _))) = ps.search.get_doc_by_doc_id(
                    &req.project_id,
                    engram_core::namespaces::NAMESPACE_MEMORY_BANK,
                    0,
                    &h.doc_id,
                ) else {
                    continue;
                };
                let mut seen_in_doc: std::collections::HashSet<String> = Default::default();
                for tok in content
                    .split(|c: char| !c.is_alphanumeric())
                    .filter(|t| t.len() >= 5 && t.chars().all(|c| c.is_alphabetic()))
                {
                    let t = tok.to_lowercase();
                    if seen_in_doc.insert(t.clone()) {
                        *counts.entry(t).or_default() += 1;
                    }
                }
            }
            let covered = concepts.join(" ").to_lowercase();
            let mut cands: Vec<(usize, String)> = counts
                .into_iter()
                .filter(|(t, n)| *n >= 2 && !covered.contains(t.as_str()))
                .map(|(t, n)| (n, t))
                .collect();
            cands.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            let graph_b = self.state.graph.clone();
            let pid_b = req.project_id.clone();
            let picked = tokio::task::spawn_blocking(move || {
                let mut picked: Vec<String> = Vec::new();
                for (_, t) in cands.into_iter().take(24) {
                    if picked.len() >= 2 {
                        break;
                    }
                    let in_code = graph_b
                        .query_nodes(&pid_b, None, Some(&t), None, 3)
                        .map(|v| !v.is_empty())
                        .unwrap_or(false);
                    if in_code {
                        picked.push(t);
                    }
                }
                picked
            })
            .await
            .unwrap_or_default();
            for t in picked {
                if concepts.len() < 5 {
                    concepts.push(t);
                }
            }
        }
        let mut prov: BTreeMap<String, BTreeSet<&'static str>> = BTreeMap::new();
        let mut seed_order: Vec<String> = Vec::new(); // concept hits in relevance order

        // Concept arm — typed footprint of each domain concept.
        for c in &concepts {
            if let Ok(r) = self
                .handle_get_concept_footprint(crate::models::GetConceptFootprintRequest {
                    project_id: req.project_id.clone(),
                    concept: c.clone(),
                    max_per_group: 12,
                })
                .await
                && let Some(t) = r.content.first().and_then(|x| x.as_text())
            {
                for p in change_set_paths(&t.text) {
                    if !engram_core::is_vendor_path(&p) {
                        if !prov.contains_key(&p) {
                            seed_order.push(p.clone());
                        }
                        prov.entry(p).or_default().insert("concept");
                    }
                }
            }
        }

        // History arm — commit-message search surfaces the files of past similar
        // changes (the universal co-change signal; carries stories whose real
        // files share no concept keyword). Golden tier.
        if let Ok(r) = self
            .handle_search_history(crate::models::SearchHistoryRequest {
                project_id: req.project_id.clone(),
                query: req.story.clone(),
                file_filter: None,
                exclude_paths: None,
                author_filter: None,
                date_after: None,
                date_before: None,
                limit: 12,
                fts_mode: crate::models::FtsMode::Loose,
                use_mmr: false,
                max_content_chars: 0,
            })
            .await
            && let Some(t) = r.content.first().and_then(|x| x.as_text())
        {
            for p in change_set_paths(&t.text) {
                if !engram_core::is_vendor_path(&p) {
                    if !prov.contains_key(&p) {
                        seed_order.push(p.clone());
                    }
                    prov.entry(p).or_default().insert("history");
                }
            }
        }

        // Co-change arm — confirm/expand real companions. HISTORY hits rank
        // first (the strongest co-change anchors), then concept hits in
        // relevance order, so history-carried stories aren't crowded out.
        //
        // The two co-change tools have opposite economics, so they get
        // different seed widths:
        //  • find_similar_changes RE-WALKS GIT (slow) → seed it NARROW (top 12).
        //  • detect_incomplete_changes is a graph-neighbour lookup (cheap) and
        //    SELF-FILTERING: a file with no strong (weight≥5) co-change history
        //    returns nothing. So seed it BROAD. A central file — e.g. a settings
        //    store — can rank deep in concept order yet be the ONLY anchor that
        //    pulls in the tight companion family the story needs (the full .resx
        //    language set + the SQL seed migration). Tangential anchors cost one
        //    lookup and contribute nothing; the tool's internal weight-sort and
        //    cap bound the output regardless of how wide we seed. Generic: any
        //    framework where a hub file co-changes with a consistent satellite set.
        let mut ranked: Vec<String> = prov
            .iter()
            .filter(|(_, s)| s.contains("history"))
            .map(|(p, _)| p.clone())
            .collect();
        for p in &seed_order {
            if !ranked.contains(p) {
                ranked.push(p.clone());
            }
        }
        if !ranked.is_empty() {
            // Resx are family LEAVES, not co-change ANCHORS: a resource file
            // co-changes only with its own language siblings, never with the
            // diverse set a story spans. Indexing resx (so the family expansion
            // can backfill the language set) floods concept matches, which can
            // shove the real code anchor — e.g. a settings store whose history
            // pulls the whole resx family — past the seed cap. Keep resx out of
            // the co-change seed so code anchors keep their slots; resx still
            // reach the change set via concept, co-change PARTNERS, and family.
            let anchor = |p: &&String| !p.ends_with(".resx");
            let fsc_seed: Vec<String> = ranked.iter().filter(anchor).take(12).cloned().collect();
            let dic_seed: Vec<String> = ranked.iter().filter(anchor).take(40).cloned().collect();
            let mut texts: Vec<String> = Vec::new();
            if let Ok(r) = self
                .handle_find_similar_changes(crate::models::FindSimilarChangesRequest {
                    project_id: req.project_id.clone(),
                    files: fsc_seed,
                    max_commits: 800,
                    top: 8,
                })
                .await
                && let Some(t) = r.content.first().and_then(|x| x.as_text())
            {
                texts.push(t.text.clone());
            }
            if let Ok(r) = self
                .handle_detect_incomplete_changes(crate::models::DetectIncompleteChangesRequest {
                    project_id: req.project_id.clone(),
                    edited_files: dic_seed,
                    max_partners: 12,
                })
                .await
                && let Some(t) = r.content.first().and_then(|x| x.as_text())
            {
                texts.push(t.text.clone());
            }
            for text in texts {
                for p in change_set_paths(&text) {
                    if !engram_core::is_vendor_path(&p) {
                        prov.entry(p).or_default().insert("cochange");
                    }
                }
            }
        }

        // Semantic arm — embedding search reaches files the LEXICAL signals miss:
        // a new architectural layer (e.g. an api-v2 controller) with sparse git
        // history is invisible to concept/co-change/history, but its meaning
        // ("update RoQ invoice status from the API") still matches the story
        // vector. Tagged "vector"; ranked LOW alone (semantic hits are noisier),
        // so it fills the capped tail to reach those files without displacing the
        // corroborated ones. MMR for file diversity. Generic; no per-repo logic.
        if let Ok(r) = self
            .handle_vector_search(crate::models::VectorSearchRequest {
                project_id: req.project_id.clone(),
                query: req.story.clone(),
                namespace: "memory".to_string(),
                top_k: 40,
                use_mmr: true,
                include_path_prefixes: None,
                exclude_path_prefixes: None,
                language_filters: None,
                include_content: false,
                max_content_chars: 0,
            })
            .await
            && let Some(t) = r.content.first().and_then(|x| x.as_text())
        {
            // change_set_paths preserves the similarity order of the results.
            // The TOP-12 distinct hits are the cross-LANGUAGE bridge — an English
            // story matching Swedish/internal code identifiers that concept and
            // co-change cannot reach — so tag them "vtop" to ALWAYS survive the
            // render tail cap (bounded → never floods). Ranks 13+ stay plain
            // "vector" (normal tail behaviour, so no regression where the layer
            // had room for them).
            for (i, p) in change_set_paths(&t.text)
                .into_iter()
                .filter(|p| !engram_core::is_vendor_path(p))
                .enumerate()
            {
                prov.entry(p)
                    .or_default()
                    .insert(if i < 12 { "vtop" } else { "vector" });
            }
        }

        // TS -> committed JS/CSS BUNDLE via co-change. A .ts usually ships with a
        // compiled bundle, but MANY .ts often compile into ONE module bundle with
        // a DIFFERENT basename (map.ts/iomarker.ts/… -> map.js), so the 1:1
        // transpile-pair expansion misses it. The bundle IS among the .ts's
        // strongest co-change partners, but the broad 40-anchor co-change seed
        // above buries those moderate-weight links under the partner truncation.
        // Re-run the cheap neighbour lookup seeded with ONLY the .ts anchors so
        // each source's committed bundle survives, and keep just the .js/.css
        // partners (the bundles). Generic — any framework that commits compiled
        // front-end bundles; no per-repo names.
        // Generalised to the whole PRESENTATION layer: markup, code's compiled
        // bundle, and stylesheet all ship together but rarely share a basename
        // (map.ts/iomarker.ts -> map.js; map.js <-> map.aspx/index.vbhtml/map.css;
        // marker_edit.aspx <-> marker.edit.js), so 1:1 pairing misses them and the
        // broad co-change seed buries the moderate-weight links. Seed the cheap
        // neighbour lookup with ONLY the presentation anchors and keep their
        // presentation-type partners. Hub-trim inside the tool bounds it.
        const PRESENTATION: &[&str] = &[
            ".ts", ".tsx", ".js", ".jsx", ".aspx", ".ascx", ".master", ".vbhtml", ".cshtml", ".css",
        ];
        let pres_anchors: Vec<String> = prov
            .keys()
            .filter(|p| {
                let pl = p.to_lowercase();
                PRESENTATION.iter().any(|e| pl.ends_with(e))
            })
            .cloned()
            .collect();
        if !pres_anchors.is_empty()
            && let Ok(r) = self
                .handle_detect_incomplete_changes(crate::models::DetectIncompleteChangesRequest {
                    project_id: req.project_id.clone(),
                    // Wider than the per-file default: this seeds the WHOLE
                    // presentation anchor set in one call, so a small cap would let
                    // strong anchors' partners crowd out a specific bundle (e.g.
                    // roqQtyManager.js, weight ~8) under truncation.
                    edited_files: pres_anchors,
                    max_partners: 25,
                })
                .await
            && let Some(t) = r.content.first().and_then(|x| x.as_text())
        {
            for p in change_set_paths(&t.text) {
                let pl = p.to_lowercase();
                if PRESENTATION.iter().any(|e| pl.ends_with(e)) && !engram_core::is_vendor_path(&p)
                {
                    prov.entry(p).or_default().insert("cochange");
                }
            }
        }

        // Family expansion — add deterministic .NET companions that EXIST in the
        // index: code-behind/designer of a page, and the full .resx language set.
        // Generic framework patterns, not per-repo. Match prefix-insensitively
        // (the "Site/" web-root prefix is stripped on both sides).
        if let Ok(meta) = self.state.graph.list_file_node_metadata(&req.project_id) {
            let strip = |p: &str| -> String {
                let p = p.replace('\\', "/").to_lowercase();
                p.strip_prefix("site/").unwrap_or(&p).to_string()
            };
            let index: Vec<String> = meta.iter().map(|(rp, _)| strip(rp.as_str())).collect();
            let index_set: HashSet<&String> = index.iter().collect();
            let mut fam: Vec<(String, BTreeSet<&'static str>)> = Vec::new();
            for (p, sigs) in &prov {
                let ps = strip(p);
                if ps.ends_with(".aspx") || ps.ends_with(".ascx") {
                    for ext in [".vb", ".cs", ".designer.vb", ".designer.cs"] {
                        let sib = format!("{ps}{ext}");
                        if index_set.contains(&sib) {
                            fam.push((sib, sigs.clone()));
                        }
                    }
                }
                if ps.ends_with(".resx")
                    && let Some(slash) = ps.rfind('/')
                {
                    let dir = &ps[..slash + 1];
                    let stem = ps[slash + 1..].split('.').next().unwrap_or("");
                    if !stem.is_empty() {
                        let want = format!("{dir}{stem}");
                        for f in &index {
                            if f.starts_with(&want) && f.ends_with(".resx") {
                                // Tag "family": a localized resx set is atomic — if
                                // one language is in the set, ALL must be (the
                                // completeness rule). Tagging lets render exempt the
                                // language siblings from the per-layer tail cap, so a
                                // seeded family is never shown half-complete.
                                let mut fs = sigs.clone();
                                fs.insert("family");
                                fam.push((f.clone(), fs));
                            }
                        }
                    }
                }
                // TypeScript source <-> its committed compiled JS bundle. The
                // committed bundle MUST change with its source (a recurring recall
                // miss: editing a .ts but forgetting the shipped .js). Only add a
                // partner that EXISTS in the index.
                for c in transpile_pair_candidates(&ps) {
                    if index_set.contains(&c) {
                        fam.push((c, sigs.clone()));
                    }
                }
            }
            for (k, v) in fam {
                prov.entry(k).or_default().extend(v);
            }
        }

        // Collapse path-prefix variants. The graph stores some files under two
        // node spellings — index_project keeps the web-root prefix
        // (Site/App_Code/…) while index_git_history drops it (App_Code/…) — so
        // one file can appear twice in prov with its signal tags split across
        // the two spellings, padding the change set and weakening per-file
        // corroboration. Merge any path that is a ONE-SEGMENT trailing suffix of
        // a longer path (exactly the web-root-prefix pattern) into the longer,
        // fully-qualified one, unioning tags (which can also lift the survivor's
        // tier as it gains a second signal). Pure within-set rule — robust even
        // when BOTH spellings exist as graph nodes; generic, no web-root name.
        {
            let mut keys: Vec<String> = prov.keys().cloned().collect();
            keys.sort_by(|a, b| b.len().cmp(&a.len())); // longest first
            let mut kept: Vec<String> = Vec::new();
            let mut remap: HashMap<String, String> = HashMap::new();
            for k in keys {
                let k_segs = k.matches('/').count();
                if let Some(longer) = kept
                    .iter()
                    .find(|a| a.matches('/').count() == k_segs + 1 && a.ends_with(&format!("/{k}")))
                {
                    remap.insert(k.clone(), longer.clone());
                } else {
                    kept.push(k);
                }
            }
            if !remap.is_empty() {
                let mut merged: BTreeMap<String, BTreeSet<&'static str>> = BTreeMap::new();
                for (p, sigs) in std::mem::take(&mut prov) {
                    let key = remap.get(&p).cloned().unwrap_or(p);
                    merged.entry(key).or_default().extend(sigs);
                }
                prov = merged;
            }
        }

        // Temporal-analytics expansion: a story asking for analytics OVER TIME
        // ("time to approve", aging, trends) needs the domain entity's
        // STATUS-CHANGE HISTORY — which lives in log/history tables the
        // concept/co-change arms miss because the story names the entity, not
        // its log table (a live A/B: the dossier missed the log table an
        // approved PR queried for exactly this ask). Surface the graph's
        // log/history/audit tables plus their accessor functions so the plan
        // decides explicitly. Generic: name-shape scan, no per-repo names.
        let temporal_section: Option<String> = if story_asks_analytics_over_time(&req.story) {
            let graph = self.state.graph.clone();
            let pid_t = req.project_id.clone();
            let cand_files: Vec<String> = prov.keys().cloned().collect();
            tokio::task::spawn_blocking(move || {
                let tables = graph
                    .query_nodes(
                        &pid_t,
                        Some("db_table"),
                        None,
                        None,
                        crate::handlers::NODE_SCAN_LIMIT,
                    )
                    .unwrap_or_default();
                let cand_norm: Vec<String> = cand_files
                    .iter()
                    .map(|f| f.replace('\\', "/").to_lowercase())
                    .collect();
                let cand_dirs: HashSet<String> = cand_norm
                    .iter()
                    .filter_map(|f| f.rfind('/').map(|i| f[..i].to_string()))
                    .collect();
                let cand_words: HashSet<String> = path_token_bag(&cand_norm)
                    .into_iter()
                    .filter(|t| t.starts_with("w:"))
                    .collect();
                // (related-to-candidates, accessor count, table, accessor rows)
                let mut rows: Vec<(bool, usize, String, Vec<String>)> = Vec::new();
                for t in tables.iter().filter(|t| is_history_log_table_name(&t.name)) {
                    let incoming = graph
                        .find_incoming_edges_with_kind(&pid_t, None, &t.node_id, 50)
                        .unwrap_or_default();
                    let mut accessors: Vec<String> = Vec::new();
                    let mut related = false;
                    let mut total = 0usize;
                    for (src, _, _) in &incoming {
                        let Ok(Some(f)) = graph.get_node(&pid_t, src) else {
                            continue;
                        };
                        if f.node_type != "function" {
                            continue;
                        }
                        total += 1;
                        let fp = f.file_path.as_str().replace('\\', "/");
                        let fpl = fp.to_lowercase();
                        if !related {
                            // "Related" = an accessor file shares a directory
                            // prefix or a basename word with the ranked set.
                            let dir_match = fpl
                                .rfind('/')
                                .map(|i| fpl[..i].to_string())
                                .is_some_and(|d| {
                                    cand_dirs
                                        .iter()
                                        .any(|c| c.starts_with(&d) || d.starts_with(c.as_str()))
                                });
                            let word_match = path_token_bag(std::slice::from_ref(&fpl))
                                .iter()
                                .any(|w| w.starts_with("w:") && cand_words.contains(w));
                            related = dir_match || word_match;
                        }
                        if accessors.len() < 3 {
                            accessors.push(format!("{} ({}:{})", f.name, fp, f.start_line));
                        }
                    }
                    rows.push((related, total, t.name.clone(), accessors));
                }
                if rows.is_empty() {
                    return None;
                }
                // Prefer tables whose accessors overlap the ranked candidates;
                // when NONE overlap, show all matches with a note rather than
                // guess (the overlap heuristic is a ranking aid, not an oracle).
                let any_related = rows.iter().any(|(r, ..)| *r);
                if any_related {
                    rows.retain(|(r, ..)| *r);
                }
                rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.2.cmp(&b.2)));
                let mut s = String::from(
                    "## History/log tables (analytics-over-time signal)\n\
                     This story asks for analytics OVER TIME (time-to-X / aging / \
                     history). Computing a duration needs the STATUS-CHANGE \
                     HISTORY, and this codebase keeps it in tables like these — \
                     decide explicitly which one holds this story's domain entity:\n",
                );
                if !any_related {
                    s.push_str(
                        "(no accessor file overlaps the ranked candidates — showing \
                         all log/history tables in the graph; verify domain \
                         relevance)\n",
                    );
                }
                for (_, total, name, accessors) in rows.iter().take(8) {
                    if accessors.is_empty() {
                        s.push_str(&format!(
                            "- `{name}` (no accessor functions in the graph)\n"
                        ));
                    } else {
                        s.push_str(&format!(
                            "- `{name}` — accessed by: {}{}\n",
                            accessors.join(", "),
                            if *total > accessors.len() {
                                format!(" (+{} more)", total - accessors.len())
                            } else {
                                String::new()
                            }
                        ));
                    }
                }
                s.push('\n');
                Some(s)
            })
            .await
            .ok()
            .flatten()
        } else {
            None
        };

        // Symmetric sibling controls/handlers: WebForms/WinForms pages often
        // contain PAIRED controls/handlers whose names differ by exactly one
        // token (Main/Sub, Left/Right, From/To, Start/End, Sender/Receiver);
        // a change to one usually requires deciding the interaction with the
        // other (a live case: a merged fix made two symmetric filter panels
        // mutually exclusive — the plan targeting one control under-specified
        // the twin). Scan control + function nodes in the top provisional
        // files and surface one-token-apart name pairs. Generic: name-shape
        // scan, no per-repo names.
        let sibling_section: Option<String> = {
            // "Top" = the same corroboration-tier order render_change_set uses
            // (co-change/history first), not BTreeMap alphabetical order.
            let mut ranked: Vec<(&String, &BTreeSet<&'static str>)> = prov.iter().collect();
            ranked.sort_by(|a, b| {
                change_set_tier(a.1)
                    .cmp(&change_set_tier(b.1))
                    .then_with(|| a.0.cmp(b.0))
            });
            let top_files: Vec<String> = ranked
                .into_iter()
                .take(10)
                .map(|(p, _)| p.clone())
                .collect();
            let graph = self.state.graph.clone();
            let pid_s = req.project_id.clone();
            tokio::task::spawn_blocking(move || {
                let mut rows: Vec<(String, String, String)> = Vec::new();
                for f in &top_files {
                    let mut names: Vec<String> = Vec::new();
                    for nt in ["control", "function"] {
                        for n in graph
                            .query_nodes(&pid_s, Some(nt), None, Some(f), 500)
                            .unwrap_or_default()
                        {
                            names.push(n.name);
                        }
                    }
                    for (a, b) in symmetric_sibling_pairs(&names) {
                        if rows.len() >= 8 {
                            break;
                        }
                        rows.push((f.clone(), a, b));
                    }
                    if rows.len() >= 8 {
                        break;
                    }
                }
                if rows.is_empty() {
                    return None;
                }
                let mut s = String::from(
                    "## Symmetric sibling controls/handlers\n\
                     These controls/handlers in the candidate files come in \
                     NAME-SYMMETRIC pairs (names differ by exactly one token — \
                     Main/Sub, Left/Right, From/To, Start/End). A change to one \
                     side of a pair usually requires deciding the twin's \
                     behavior:\n",
                );
                for (f, a, b) in &rows {
                    s.push_str(&format!("- `{a}` <-> `{b}` ({f})\n"));
                }
                s.push('\n');
                Some(s)
            })
            .await
            .ok()
            .flatten()
        };

        let mut out = render_change_set(
            req.story.trim(),
            &concepts,
            &prov,
            temporal_section.as_deref(),
            sibling_section.as_deref(),
        );

        // Scaffold template: when the story ADDS a new API/structural feature,
        // surface the codebase's existing feature COHORT as a template to mirror.
        // Validated to make the agent produce complete, convention-correct new
        // files (controller + service + interface + DTOs + query + registration)
        // instead of an ad-hoc subset. Generic — learns the cohort from the index,
        // no hardcoded layout.
        {
            let sl = req.story.to_lowercase();
            let creation = [
                "add",
                "new ",
                "create",
                "introduce",
                "expose",
                "implement",
                "ability to",
            ]
            .iter()
            .any(|c| sl.contains(c));
            let api = ["api", "endpoint", "rest", "webapi", "web api"]
                .iter()
                .any(|c| sl.contains(c));
            if creation && api {
                // Original-case, web-root-stripped paths (case matters for the
                // interface I-prefix rule inside find_analog_cohort). Dedup: the
                // graph stores some files under two node spellings (Site/-prefixed
                // from index_project, bare from index_git_history) which collapse
                // to the same path here and would otherwise double every cohort row.
                let mut index: Vec<String> = self
                    .state
                    .graph
                    .list_file_node_metadata(&req.project_id)
                    .map(|m| {
                        m.into_iter()
                            .map(|(rp, _)| {
                                let p = rp.as_str().replace('\\', "/");
                                if p.len() >= 5 && p[..5].eq_ignore_ascii_case("site/") {
                                    p[5..].to_string()
                                } else {
                                    p
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                index.sort();
                index.dedup();
                if let Some(cohort) = find_analog_cohort(&index, "controller") {
                    let area = cohort
                        .first()
                        .map(|f| {
                            let s: Vec<&str> = f.split('/').collect();
                            if s.len() >= 2 {
                                format!("{}/{}", s[0], s[1]).to_lowercase()
                            } else {
                                String::new()
                            }
                        })
                        .unwrap_or_default();
                    let reg: Vec<&String> = index
                        .iter()
                        .filter(|f| {
                            let fl = f.to_lowercase();
                            fl.starts_with(&area)
                                && ["webapiconfig", "routeconfig", "startup", "global.asax"]
                                    .iter()
                                    .any(|r| fl.contains(r))
                        })
                        .collect();
                    out.push_str(
                        "\n## Likely a NEW feature — scaffold from the codebase's template\n",
                    );
                    out.push_str(
                        "This story adds a new API/structural feature. This codebase builds such a \
                         feature as a FIXED COHORT of files — mirror this complete existing example \
                         for THIS story's entity (same roles, folders, naming convention):\n",
                    );
                    for f in cohort.iter().take(12) {
                        out.push_str(&format!("- `{f}`\n"));
                    }
                    // Spell out the CONCRETE files to create: template rows
                    // alone made agents reverse-engineer the naming and miss
                    // pieces (PR1890: 6 new-domain files, 0 proposed).
                    // Ground the entity in the RANKED file set — the funnel
                    // already knows which files this story is about. Grounding
                    // in the whole index let any long dir containing a story
                    // word win (live: "personalinformation" via the word
                    // "information" from an unrelated customer quote).
                    let ranked_paths: Vec<String> = prov.keys().cloned().collect();
                    let entity_pascal: String = derive_scaffold_entity(&req.story, &ranked_paths)
                        .or_else(|| derive_scaffold_entity(&req.story, &index))
                        .or_else(|| {
                            concepts
                                .iter()
                                .find(|c| c.contains(' '))
                                .or_else(|| concepts.first())
                                .map(|c| {
                                    c.split_whitespace()
                                        .map(|w| {
                                            let mut w = w.to_lowercase();
                                            if let Some(f) = w.get_mut(0..1) {
                                                f.make_ascii_uppercase();
                                            }
                                            w
                                        })
                                        .collect()
                                })
                        })
                        .unwrap_or_default();
                    let proposed = propose_scaffold_paths(&cohort, &entity_pascal);
                    if !proposed.is_empty() {
                        out.push_str(&format!(
                            "Concrete files to CREATE for this story (entity `{entity_pascal}` \
                             derived from the story — adjust the name if the domain calls it \
                             something else):\n"
                        ));
                        for f in proposed.iter().take(12) {
                            out.push_str(&format!("- `{f}`\n"));
                        }
                    }
                    if !reg.is_empty() {
                        out.push_str("Register the new pieces where the codebase wires them up:\n");
                        for f in reg.iter().take(3) {
                            out.push_str(&format!("- `{f}`\n"));
                        }
                    }
                    out.push_str(
                        "Create the ANALOGOUS complete set for the new entity — do NOT omit the \
                         interface, query-params, DTOs, or the registration.\n",
                    );
                    out.push_str(
                        "Beyond the cohort, check the WIRING a live A/B showed even \
                         strong planners miss: (1) permission/role definitions - if \
                         the endpoint is permission-gated, the role catalog file \
                         defining those permissions changes too; (2) the dashboard \
                         page showing the same data - its filter handlers define \
                         your query parameters and often need a shared-service \
                         refactor; (3) DTO VARIANTS - mirror the template's -Out \
                         naming per projection (Item-Out, StatusLog-Out), not one \
                         generic DTO.\n",
                    );
                }
            }
        }

        // Approved-work exemplars: the top merged PRs matching this story are
        // direct evidence for the proposal — their cohorts show the SHAPE of
        // an accepted change (and often contain the files ranking missed).
        // merged_before keeps replays/evals leak-free (see find_merged_work).
        if let Ok(ps) = self.ensure_project_runtime(&req.project_id).await {
            // With a cutoff active, most of the freshest matches get
            // filtered - fetch deeper so older exemplars can surface.
            let fetch_k = if req.merged_before.is_some() { 24 } else { 6 };
            let q = engram_index::HybridQuery {
                project_id: req.project_id.clone(),
                namespace: engram_core::namespaces::NAMESPACE_HISTORY.into(),
                generation: 0,
                text: req.story.clone(),
                top_k: fetch_k,
                fts_mode: "loose".into(),
                include_path_prefixes: Some(vec!["pr:".into()]),
                exclude_path_prefixes: None,
                language_filters: None,
                author_filter: None,
                date_after: None,
                date_before: None,
                use_mmr: false,
            };
            let engine = ps.search.clone();
            let hits = tokio::task::spawn_blocking(move || engine.lexical_search(&q))
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or_default();
            let cutoff = req
                .merged_before
                .as_deref()
                .map(str::trim)
                .filter(|d| !d.is_empty());
            let mut shown = 0usize;
            for h in &hits {
                if shown >= 2 {
                    break;
                }
                let Ok(Some((_, _, content, _, _))) = ps.search.get_doc_by_doc_id(
                    &req.project_id,
                    engram_core::namespaces::NAMESPACE_HISTORY,
                    0,
                    &h.doc_id,
                ) else {
                    continue;
                };
                if let Some(cut) = cutoff {
                    let leaks = content
                        .lines()
                        .find_map(|l| l.split("merged: ").nth(1))
                        .and_then(|rest| rest.get(..10))
                        .is_none_or(|d| d >= cut);
                    if leaks {
                        continue;
                    }
                }
                if shown == 0 {
                    out.push_str("\n## Approved exemplars — how similar merged work was shaped\n");
                }
                shown += 1;
                let head: String = content.chars().take(500).collect();
                out.push_str(&format!("{}\n", head.trim_end()));
                if content.chars().count() > 500 {
                    out.push_str("(truncated)\n");
                }
            }
            if shown > 0 {
                out.push_str(
                    "next: find_merged_work(story=...) for the complete approved file cohorts.\n",
                );
            }
        }

        // Permission gates in the candidate set: when ranked files carry
        // guard metadata, name the gates - a live A/B showed even strong
        // planners miss permission-catalog changes because nothing in the
        // brief said the surface was gated (PR1890's role.vb lesson).
        {
            let graph = self.state.graph.clone();
            let pid_g = req.project_id.clone();
            let top_files: Vec<String> = prov.keys().take(15).cloned().collect();
            let gates = tokio::task::spawn_blocking(move || {
                let mut gates: std::collections::BTreeMap<String, usize> = Default::default();
                for f in &top_files {
                    for n in graph
                        .query_nodes(&pid_g, None, None, Some(f), 500)
                        .unwrap_or_default()
                    {
                        let Some(meta) = n.metadata.as_ref() else {
                            continue;
                        };
                        for key in ["permission_checks", "guard_roles"] {
                            if let Some(v) = meta.get(key).and_then(|v| v.as_str()) {
                                for g in v.split(';').filter(|g| !g.trim().is_empty()) {
                                    *gates.entry(g.trim().to_string()).or_default() += 1;
                                }
                            }
                        }
                    }
                }
                gates
            })
            .await
            .unwrap_or_default();
            if !gates.is_empty() {
                out.push_str(
                    "\n## Permission gates in the candidate set\n\
                     These surfaces are permission/role-gated. Decide explicitly whether \
                     your change needs a NEW permission-catalog entry (and its admin \
                     wiring) or reuses one of these:\n",
                );
                let mut rows: Vec<(usize, String)> =
                    gates.into_iter().map(|(g, n)| (n, g)).collect();
                rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
                for (n, g) in rows.into_iter().take(10) {
                    out.push_str(&format!("- {g} ({n} gated symbol(s) in the set)\n"));
                }
            }
        }

        // The ranked file set is the single most freshness-sensitive output
        // in the funnel: a stale index proposes the wrong files. This was
        // the only primary funnel tool with no staleness signal.
        let gen_ = self
            .get_active_generation(&req.project_id)
            .await
            .unwrap_or(1);
        out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    /// TODO-29: the loop-closing check. Given the files an agent edited,
    /// report what history and the graph say should ALSO have changed:
    /// strong co-change partners left untouched, and state keys whose other
    /// readers/writers live outside the edit set.
    pub async fn handle_detect_incomplete_changes(
        &self,
        req: crate::models::DetectIncompleteChangesRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        if req.edited_files.is_empty() {
            return Err(McpError::invalid_params(
                "edited_files must not be empty".to_string(),
                None,
            ));
        }
        let _ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;

        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let edited: Vec<String> = req
            .edited_files
            .iter()
            // Normalise to forward slashes AND strip any leading "/" — callers
            // (and PR ground-truth) often pass "/Site/...", but file node ids
            // are "Site/..."; an unstripped leading slash silently resolves to
            // nothing and the audit returns a false "all clear".
            .map(|f| f.replace('\\', "/").trim_start_matches('/').to_string())
            .collect();
        let max_partners = req.max_partners.clamp(1, 20);

        let (partner_findings, state_findings, unresolved_inputs) =
            tokio::task::spawn_blocking(move || {
                let edited_set: HashSet<String> = edited.iter().map(|f| f.to_lowercase()).collect();
                // TemporalCoupling nodes are keyed by REAL git case, but callers
                // (e.g. get_change_set's path extractor) may pass lowercased paths.
                // An exact-match neighbour lookup then silently misses every
                // PascalCase file — i.e. most .NET class files (SystemSettingStore.vb
                // etc.), the very hub files whose co-change family the caller needs.
                // Resolve each edited path to its real-case node id once. Generic:
                // any case-insensitive caller against a case-sensitive graph.
                let real_case: HashMap<String, String> = graph
                    .list_file_node_metadata(&pid)
                    .map(|m| {
                        m.into_iter()
                            .map(|(rp, _)| {
                                let r = rp.as_str().replace('\\', "/");
                                (r.to_lowercase(), r)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                // A path counts as "covered" if any edited path tail-matches it
                // (handles Site/-prefix spelling variants from history).
                let covered = |path: &str| -> bool {
                    let p = path.to_lowercase();
                    edited_set.iter().any(|e| {
                        e == &p
                            || e.ends_with(&format!("/{p}"))
                            || p.ends_with(&format!("/{}", e.as_str()))
                    })
                };

                // ── Co-change partners not in the edit set ──────────────────
                // Collect raw candidates (edited_file, partner, weight); weak
                // couplings are noise, so demand real history.
                let mut raw: Vec<(String, String, u32)> = Vec::new();
                let mut unresolved_inputs: Vec<String> = Vec::new();
                for f in &edited {
                    let resolved = match real_case.get(&f.to_lowercase()) {
                        Some(r) => r.as_str(),
                        None => {
                            // A completeness tool that silently skips an input
                            // file can print "looks complete" while having seen
                            // NOTHING — flag it instead.
                            unresolved_inputs.push(f.clone());
                            f.as_str()
                        }
                    };
                    let fid = format!("file:{resolved}");
                    let Ok(neigh) = graph.neighbors(&pid, EdgeKind::TemporalCoupling, &fid, 500)
                    else {
                        continue;
                    };
                    for (nid, weight) in neigh {
                        let Some(partner) = nid.strip_prefix("file:") else {
                            continue;
                        };
                        if weight < 5 || covered(partner) {
                            continue;
                        }
                        raw.push((f.clone(), partner.to_string(), weight));
                    }
                }
                // Keep the single strongest coupling per partner.
                raw.sort_by(|a, b| b.2.cmp(&a.2));
                let mut best_per_partner: Vec<(String, String, u32)> = Vec::new();
                {
                    let mut seen: HashSet<String> = HashSet::new();
                    for r in raw {
                        if seen.insert(r.1.clone()) {
                            best_per_partner.push(r);
                        }
                    }
                }
                // Hub down-weighting: a partner that co-changes with a HUGE number of
                // DISTINCT files (a global resx bundle, a shared script bundle) is
                // touched by almost every change, so it carries no specific "you
                // missed this companion" signal — surfacing it only adds noise and
                // steers the agent toward the wrong family (e.g. label.resx when the
                // change actually needs text.resx). Drop partners whose co-change
                // DEGREE marks them ubiquitous. Generic — degree-based, the same IDF
                // insight as the cross-section map; no per-repo names. The default
                // is a high floor so it NO-OPS on sparse/young repos (no file reaches
                // it) and only trims genuinely ubiquitous hubs on dense histories
                // like this one (label.resx ~1063, text.resx ~900 get dropped;
                // moderately-specific companions ~300-700 survive). Env-overridable.
                let hub_degree: usize = std::env::var("ENGRAM_HUB_DEGREE")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(800);
                // Co-change degree per candidate partner (distinct neighbours).
                let degree_of = |partner: &str| -> usize {
                    let pfid = format!("file:{partner}");
                    graph
                        .neighbors(&pid, EdgeKind::TemporalCoupling, &pfid, 2000)
                        .map(|v| v.len())
                        .unwrap_or(0)
                };
                let mut partner_findings: Vec<(String, String, u32, usize)> = best_per_partner
                    .into_iter()
                    .map(|(e, p, w)| {
                        let d = degree_of(&p);
                        (e, p, w, d)
                    })
                    .filter(|(_, _, _, d)| *d < hub_degree)
                    .collect();
                partner_findings.truncate(max_partners);

                // ── State keys shared with untouched files ──────────────────
                // Symbols in edited files -> state targets -> other touchers.
                let mut state_findings: Vec<(String, String)> = Vec::new();
                let mut seen_keys: HashSet<String> = HashSet::new();
                let nodes = graph
                    .query_nodes(&pid, None, None, None, crate::handlers::NODE_SCAN_LIMIT)
                    .unwrap_or_default();
                let edited_symbol_ids: Vec<String> = nodes
                    .iter()
                    .filter(|n| covered(n.file_path.as_str()))
                    .map(|n| n.node_id.clone())
                    .collect();
                let node_file: std::collections::HashMap<&str, &str> = nodes
                    .iter()
                    .map(|n| (n.node_id.as_str(), n.file_path.as_str()))
                    .collect();
                for sid in edited_symbol_ids.iter().take(500) {
                    for kind in [EdgeKind::ReadsState, EdgeKind::WritesState] {
                        let Ok(neigh) = graph.neighbors(&pid, kind, sid, 20) else {
                            continue;
                        };
                        for (state_id, _) in neigh {
                            if !state_id.starts_with("state:")
                                || !seen_keys.insert(state_id.clone())
                            {
                                continue;
                            }
                            // Other touchers of this key outside the edit set.
                            let Ok(touchers) =
                                graph.find_incoming_edges_with_kind(&pid, None, &state_id, 100)
                            else {
                                continue;
                            };
                            let outside: Vec<&str> = touchers
                                .iter()
                                .filter_map(|(src, _, _)| node_file.get(src.as_str()).copied())
                                .filter(|f| !covered(f))
                                .take(3)
                                .collect();
                            if !outside.is_empty() {
                                state_findings.push((
                                    state_id
                                        .strip_prefix("state:")
                                        .unwrap_or(&state_id)
                                        .to_string(),
                                    outside.join(", "),
                                ));
                            }
                        }
                    }
                }
                state_findings.truncate(10);
                (partner_findings, state_findings, unresolved_inputs)
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let mut out = String::from("# Edit completeness check\n");
        if !unresolved_inputs.is_empty() {
            out.push_str(&format!(
                "\n⚠ {} of your edited files were NOT found in the index ({}) — typo, \
                 moved, or not yet indexed. Findings below may be incomplete; run \
                 update_project if the files are new.\n",
                unresolved_inputs.len(),
                unresolved_inputs
                    .iter()
                    .take(5)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if partner_findings.is_empty() && state_findings.is_empty() {
            out.push_str(
                "\nNo strong couplings point outside your edit set. History and state \
                 wiring are consistent with a complete change.\n",
            );
        } else {
            if !partner_findings.is_empty() {
                out.push_str("\n## Co-change partners you did NOT touch\n");
                out.push_str(
                    "History says these files change together with yours — verify each:\n",
                );
                for (edited, partner, weight, _degree) in &partner_findings {
                    out.push_str(&format!(
                        "- `{partner}` ({weight} co-changes with `{edited}`)\n"
                    ));
                }
            }
            if !state_findings.is_empty() {
                out.push_str("\n## Shared state with untouched files\n");
                for (key, files) in &state_findings {
                    out.push_str(&format!(
                        "- state key `{key}` is also read/written in: {files}\n"
                    ));
                }
            }
        }
        out.push_str("\nnext: pre_commit_review before committing; find_similar_changes for companion-artifact patterns.\n");
        out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }
}

const EDIT_SESSION_META_KEY: &str = "edit_session_v1";

impl Engram {
    /// TODO-29: open the edit-session bookend. Persists intent and returns
    /// the expectation brief (partners + state couplings of the planned
    /// files) BEFORE any line changes — so the agent knows the blast
    /// surface going in, not after.
    pub async fn handle_begin_edit_session(
        &self,
        req: crate::models::BeginEditSessionRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        if req.planned_files.is_empty() {
            return Err(McpError::invalid_params(
                "planned_files must not be empty".to_string(),
                None,
            ));
        }
        let session = serde_json::json!({
            "planned_files": req.planned_files,
            "story": req.story,
            "started_ms": crate::utils::now_ms(),
        });
        let reg = self.state.registry.clone();
        let pid = req.project_id.clone();
        let payload = session.to_string();
        tokio::task::spawn_blocking(move || reg.set_meta(&pid, EDIT_SESSION_META_KEY, &payload))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // The expectation brief is the same engine, run on the PLANNED set:
        // anything it reports now is coupling the agent should plan for.
        let brief = self
            .handle_detect_incomplete_changes(crate::models::DetectIncompleteChangesRequest {
                project_id: req.project_id.clone(),
                edited_files: req.planned_files.clone(),
                max_partners: 5,
            })
            .await?;
        let brief_text = brief
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");

        Ok(CallToolResult::success(vec![Content::text(format!(
            "# Edit session OPEN\nplanned files: {}\n\n## Expectation brief \
             (couplings of your planned set — plan for these now)\n{}\n\
             next: edit; then complete_edit_session(edited_files=[...]) before committing.\n",
            req.planned_files.join(", "),
            brief_text
        ))]))
    }

    /// TODO-29: close the bookend — completeness check against the actual
    /// edit set plus drift vs the original plan, then clear the session.
    pub async fn handle_complete_edit_session(
        &self,
        req: crate::models::CompleteEditSessionRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let reg = self.state.registry.clone();
        let pid = req.project_id.clone();
        let stored = tokio::task::spawn_blocking(move || reg.get_meta(&pid, EDIT_SESSION_META_KEY))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let Some(stored) = stored.filter(|s| !s.trim().is_empty()) else {
            return Err(McpError::invalid_params(
                "no open edit session — call begin_edit_session first (or use \
                 detect_incomplete_changes directly for a stateless check)"
                    .to_string(),
                None,
            ));
        };
        let session: serde_json::Value = serde_json::from_str(&stored)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let planned: Vec<String> = session["planned_files"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let edited = if req.edited_files.is_empty() {
            planned.clone()
        } else {
            req.edited_files.clone()
        };

        // Plan drift: planned-but-not-edited is the silent scope shrink that
        // reviews catch late.
        let edited_lower: HashSet<String> = edited.iter().map(|f| f.to_lowercase()).collect();
        let unedited_plan: Vec<&String> = planned
            .iter()
            .filter(|f| !edited_lower.contains(&f.to_lowercase()))
            .collect();

        let check = self
            .handle_detect_incomplete_changes(crate::models::DetectIncompleteChangesRequest {
                project_id: req.project_id.clone(),
                edited_files: edited.clone(),
                max_partners: 5,
            })
            .await?;
        let check_text = check
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");

        // Session consumed either way — completing twice is a usage error.
        let reg2 = self.state.registry.clone();
        let pid2 = req.project_id.clone();
        tokio::task::spawn_blocking(move || reg2.set_meta(&pid2, EDIT_SESSION_META_KEY, ""))
            .await
            .ok();

        let mut out = String::from("# Edit session COMPLETE\n");
        if !unedited_plan.is_empty() {
            out.push_str("\n## Planned but NOT edited (scope drift — confirm intentional)\n");
            for f in unedited_plan {
                out.push_str(&format!("- {f}\n"));
            }
        }
        out.push_str("\n");
        out.push_str(&check_text);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }
}

// ── get_gis_inventory ────────────────────────────────────────────────────────

/// One grouped row of spatial-call usage: (library, map class, modern
/// equivalent, call-site count, distinct source files).
pub(crate) type GisUsageRow = (
    String,
    String,
    String,
    usize,
    std::collections::BTreeSet<String>,
);

/// Group raw spatial-call rows (library, class, modern_equivalent, file,
/// call-site count) into per-(library, class) usage rows sorted by count.
/// The count comes from the extractor's `count` edge metadata (ingest
/// collapses duplicate edge keys, so multiplicity rides in metadata).
pub(crate) fn group_spatial_calls(
    rows: &[(String, String, String, String, usize)],
) -> Vec<GisUsageRow> {
    let mut grouped: BTreeMap<
        (String, String),
        (String, usize, std::collections::BTreeSet<String>),
    > = BTreeMap::new();
    for (lib, class, modern, file, count) in rows {
        let e = grouped
            .entry((lib.clone(), class.clone()))
            .or_insert_with(|| (modern.clone(), 0, Default::default()));
        e.1 += (*count).max(1);
        if !file.is_empty() {
            e.2.insert(file.clone());
        }
    }
    let mut out: Vec<GisUsageRow> = grouped
        .into_iter()
        .map(|((lib, class), (modern, count, files))| (lib, class, modern, count, files))
        .collect();
    out.sort_by(|a, b| {
        b.3.cmp(&a.3)
            .then_with(|| a.0.cmp(&b.0))
            .then_with(|| a.1.cmp(&b.1))
    });
    out
}

impl Engram {
    /// GIS surface inventory: which map libraries/classes the project uses and
    /// where, per-file map configurations (api key / zoom / center), and the
    /// WMS/XYZ/Esri layer inventory — the map-stack documentation an agent
    /// needs before touching any map feature.
    pub async fn handle_get_gis_inventory(
        &self,
        req: crate::models::ProjectIdRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let _ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;

        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let (configs, layers, usage, spatial_total) = tokio::task::spawn_blocking(move || {
            let nodes = graph
                .query_nodes(&pid, None, None, None, crate::handlers::NODE_SCAN_LIMIT)
                .unwrap_or_default();

            // Per-file map configuration facts ("gis_config:{file}:{kind}").
            let mut configs: BTreeMap<String, Vec<String>> = BTreeMap::new();
            // Layer inventory from layer_type metadata: (type, name, file).
            let mut layers: Vec<(String, String, String)> = Vec::new();
            for n in &nodes {
                if n.node_type == "gis_config"
                    && let Some(rest) = n.name.strip_prefix("gis_config:")
                    && let Some((file, kind)) = rest.rsplit_once(':')
                {
                    configs
                        .entry(file.to_string())
                        .or_default()
                        .push(kind.to_string());
                }
                if let Some(lt) = n
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("layer_type"))
                    .and_then(|v| v.as_str())
                {
                    layers.push((
                        lt.to_string(),
                        n.name.clone(),
                        n.file_path.as_str().to_string(),
                    ));
                }
            }
            layers.sort();
            layers.dedup();

            // Spatial call edges → (library, class, modern_equivalent, file).
            let spatial = graph
                .list_edges_by_kind(&pid, EdgeKind::SpatialCall, 100_000)
                .unwrap_or_default();
            let rows: Vec<(String, String, String, String, usize)> = spatial
                .iter()
                .map(|e| {
                    let m = e.metadata.as_ref();
                    let get = |k: &str| {
                        m.and_then(|m| m.get(k))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string()
                    };
                    let count = m
                        .and_then(|m| m.get("count"))
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.parse::<usize>().ok())
                        .unwrap_or(1);
                    let file = e
                        .source_id
                        .strip_prefix("file:")
                        .unwrap_or(&e.source_id)
                        .to_string();
                    (
                        get("gis_library"),
                        get("map_class"),
                        get("modern_equivalent"),
                        file,
                        count,
                    )
                })
                .collect();
            // Edges without gis_library metadata are config/layer references
            // (file → gis_config node), not API call sites — the configs and
            // layer sections already cover them.
            let rows: Vec<_> = rows.into_iter().filter(|r| !r.0.is_empty()).collect();
            let spatial_total: usize = rows.iter().map(|r| r.4.max(1)).sum();
            (configs, layers, group_spatial_calls(&rows), spatial_total)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if configs.is_empty() && layers.is_empty() && usage.is_empty() {
            let mut out = String::from(
                "No GIS surface detected (no gis_config nodes, layer metadata, or spatial_call \
                 edges). If this project uses maps, its library may not be covered by the GIS \
                 extractors yet.",
            );
            out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
            return Ok(CallToolResult::success(vec![Content::text(out)]));
        }

        let mut out = String::from("# GIS surface inventory\n");

        if !usage.is_empty() {
            out.push_str(&format!(
                "\n## Map API usage ({spatial_total} call sites)\n"
            ));
            for (lib, class, modern, count, files) in usage.iter().take(30) {
                out.push_str(&format!(
                    "- {lib}.{class}: {count} call site(s) in {} file(s)",
                    files.len()
                ));
                if !modern.is_empty() {
                    out.push_str(&format!(" — modern equivalent: {modern}"));
                }
                out.push('\n');
                for f in files.iter().take(3) {
                    out.push_str(&format!("    {f}\n"));
                }
                if files.len() > 3 {
                    out.push_str(&format!("    ... and {} more file(s)\n", files.len() - 3));
                }
            }
            if usage.len() > 30 {
                out.push_str(&format!("  ... and {} more API(s)\n", usage.len() - 30));
            }
        }

        if !configs.is_empty() {
            out.push_str(&format!(
                "\n## Map configurations ({} file(s))\n",
                configs.len()
            ));
            for (file, kinds) in configs.iter().take(20) {
                let mut ks = kinds.clone();
                ks.sort();
                ks.dedup();
                let key_flag = if ks.iter().any(|k| k == "api_key") {
                    " — note: API key referenced in client code"
                } else {
                    ""
                };
                out.push_str(&format!("- {file}: {}{}\n", ks.join(", "), key_flag));
            }
            if configs.len() > 20 {
                out.push_str(&format!("  ... and {} more\n", configs.len() - 20));
            }
        }

        if !layers.is_empty() {
            out.push_str(&format!("\n## Layer inventory ({})\n", layers.len()));
            for (lt, name, file) in layers.iter().take(25) {
                out.push_str(&format!("- [{lt}] {name} ({file})\n"));
            }
            if layers.len() > 25 {
                out.push_str(&format!("  ... and {} more\n", layers.len() - 25));
            }
        }

        out.push_str(
            "\nnext: get_concept_footprint(concept=\"map\"/\"layer\") for full touchpoints; \
             find_implementation_pattern(pattern=\"map init\") for the house style; \
             blast_radius on the map config class before changing layer settings.\n",
        );
        out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }
}

#[cfg(test)]
mod gis_inventory_tests {
    use super::group_spatial_calls;

    fn row(
        lib: &str,
        class: &str,
        modern: &str,
        file: &str,
        count: usize,
    ) -> (String, String, String, String, usize) {
        (lib.into(), class.into(), modern.into(), file.into(), count)
    }

    #[test]
    fn groups_by_library_and_class_summing_counts() {
        let rows = vec![
            // a.js has 5 Polygon call sites collapsed into one edge with count=5
            row("google_maps", "Polygon", "MapLibre fill layer", "a.js", 5),
            row("google_maps", "Polygon", "MapLibre fill layer", "b.js", 1),
            row("leaflet", "TileLayer", "MapLibre raster source", "c.js", 1),
        ];
        let grouped = group_spatial_calls(&rows);
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[0].0, "google_maps");
        assert_eq!(grouped[0].1, "Polygon");
        assert_eq!(grouped[0].3, 6, "5 + 1 call sites");
        assert_eq!(grouped[0].4.len(), 2, "two distinct files");
        assert_eq!(grouped[1].0, "leaflet");
    }

    #[test]
    fn zero_count_clamps_to_one_and_empty_inputs_handled() {
        assert!(group_spatial_calls(&[]).is_empty());
        let grouped = group_spatial_calls(&[row("esri", "FeatureLayer", "", "", 0)]);
        assert_eq!(grouped[0].3, 1, "count 0 clamps to 1");
        assert!(grouped[0].4.is_empty(), "empty file string is dropped");
    }
}
