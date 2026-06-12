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
