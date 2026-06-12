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
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

// ── Pure helpers ─────────────────────────────────────────────────────────────

/// Lowercase concept stems used for substring matching: the term itself,
/// a naive singular (trailing 's' stripped), and a compacted form without
/// separators so "code category" also matches "CodeCategory"/"code_category".
pub(crate) fn concept_stems(concept: &str) -> Vec<String> {
    let lower = concept.trim().to_lowercase();
    let mut stems = vec![lower.clone()];
    if let Some(singular) = lower.strip_suffix('s')
        && singular.len() >= 3
    {
        stems.push(singular.to_string());
    }
    let compact: String = lower.chars().filter(|c| c.is_alphanumeric()).collect();
    if !stems.contains(&compact) && compact.len() >= 3 {
        stems.push(compact);
    }
    stems.retain(|s| !s.is_empty());
    stems
}

/// Does `name` contain any stem, ignoring case and separators?
pub(crate) fn matches_concept(name: &str, stems: &[String]) -> bool {
    let lower = name.to_lowercase();
    let compact: String = lower.chars().filter(|c| c.is_alphanumeric()).collect();
    stems
        .iter()
        .any(|s| lower.contains(s) || compact.contains(s))
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
        let (groups, consumers) = tokio::task::spawn_blocking(move || {
            let nodes = graph
                .query_nodes(&pid, None, None, None, 50_000)
                .unwrap_or_default();

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
            (groups, consumers)
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
        let lexical_only: Vec<&String> = lexical_files
            .iter()
            .filter(|f| !grouped_files.contains(f.as_str()))
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
        let result = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let repo = GitWalker::open_repo(&repo_dir)?;
            let cancel = tokio_util::sync::CancellationToken::new();
            let oids = GitWalker::walk_older_commits(
                &repo,
                None,
                max_commits,
                MergeCommitPolicy::FirstParentOnly,
                &cancel,
            )?;

            let mut scored: Vec<(f64, String, String, Vec<String>)> = Vec::new();
            let scanned = oids.len();
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
                let score = bag_jaccard(&input_bag, &path_token_bag(&files));
                if score <= 0.0 {
                    continue;
                }
                let summary = repo
                    .find_commit(oid)
                    .ok()
                    .and_then(|c| c.summary().map(|s| s.to_string()))
                    .unwrap_or_default();
                scored.push((score, oid.to_string(), summary, files));
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
}

// ── map_guards_and_settings + plan_user_story ────────────────────────────────

/// Settings-shaped table names: tables that store configuration rows.
pub(crate) fn is_settings_table_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    ["setting", "config", "option", "param", "preference"]
        .iter()
        .any(|p| lower.contains(p))
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
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for word in story.split(|c: char| !c.is_alphanumeric()) {
        let lower = word.to_lowercase();
        if lower.len() < 4 || STOPWORDS.contains(&lower.as_str()) {
            continue;
        }
        if seen.insert(lower.clone()) {
            out.push(lower);
        }
        if out.len() >= 3 {
            break;
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
                .query_nodes(&pid, None, None, None, 50_000)
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

        // Per-concept footprint (trimmed to keep the brief readable).
        for concept in &concepts {
            let sub = self
                .handle_get_concept_footprint(crate::models::GetConceptFootprintRequest {
                    project_id: req.project_id.clone(),
                    concept: concept.clone(),
                    max_per_group: 5,
                })
                .await?;
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
- Before modifying a method: `get_method_edit_context` or `check_edit_safety`.
- After choosing your file set: `find_similar_changes(files=[...])` and close
  every item under "MISSING from your set".

## Before every commit
- `pre_commit_review(diff_source="staged")` — fix or explicitly justify every
  finding (eleven gates, including guard_parity).
- If results ever look stale: `get_index_freshness`, then `update_project`.
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
            "powershell -NoProfile -Command \"Write-Output 'ENGRAM: source file changed. Before moving on: check_edit_safety for each touched method; map_guards_and_settings(scope=<file>) if you added/changed an endpoint. Before commit: pre_commit_review(staged) + find_similar_changes.'\"",
            "powershell -NoProfile -Command \"if (git status --porcelain 2>$null) { Write-Output 'ENGRAM: uncommitted changes present. Run pre_commit_review(diff_source=staged) and find_similar_changes(files=<changed files>) before finishing.' }\"",
        )
    } else {
        (
            "echo 'ENGRAM: source file changed. Before moving on: check_edit_safety for each touched method; map_guards_and_settings(scope=<file>) if you added/changed an endpoint. Before commit: pre_commit_review(staged) + find_similar_changes.'",
            "sh -c 'if [ -n \"$(git status --porcelain 2>/dev/null)\" ]; then echo \"ENGRAM: uncommitted changes present. Run pre_commit_review(diff_source=staged) and find_similar_changes(files=<changed files>) before finishing.\"; fi'",
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
                .query_nodes(&pid, None, None, None, 50_000)
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
