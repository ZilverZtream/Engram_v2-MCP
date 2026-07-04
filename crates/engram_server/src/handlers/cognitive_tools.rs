use crate::handlers::validate_project_id;
use crate::models::{
    AntiPatternGuardRequest, AstDependencyGraphRequest, ComputeBlastRadiusRequest,
    DetectDesignPatternsRequest, DreamProjectRequest, ExportCapturePackRequest,
    GetExtractionConfidenceRequest, GetUiBlueprintRequest, GraphCentralityRerankRequest,
    ImmuneCheckRequest, ImpactAnalysisRequest, MapAjaxRegionsRequest, ProjectIdRequest,
    QueryBusinessLogicRequest, TraceStateUsageRequest, TraceUiActionRequest, TraceUiEventRequest,
};
use crate::services::{cognitive_service, job_service};
use crate::state::AppEvent;
use crate::tools::Engram;
use crate::utils::now_ms;
use engram_core::safe_join;
use engram_graph::EdgeKind;
use engram_graph::store::ResolveResult;
use engram_index::HybridQuery;
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use std::fmt::Write;
use std::path::PathBuf;

/// (node_id, node_type, name, file_path) tuple used in centrality reranking.
type NodeMetaTuple = (String, Option<String>, Option<String>, Option<String>);

/// Score a trace path from 0.0 (unusable) to 1.0 (high-certainty direct
/// evidence). Starts from 1.0 and subtracts a penalty per hop based on
/// the hop's edge kind (inferred from the `(prev, curr)` node-type pair),
/// then subtracts any caller-supplied `fallback_penalty` (from ambiguous
/// control-lookup resolution). Result is floored at 0.10 so a path that
/// exists but is wholly declarative still reports something non-zero.
///
/// Rationale: declarative data-binding hops (control → control via
/// `DataSourceID`) fire on ASP.NET's schedule, not from a user action,
/// so they carry genuinely less certainty than direct event-wiring hops
/// (control → function via `OnClick` / `Handles`). Users reading the
/// provenance should see paths ranked accordingly.
fn path_confidence_score(path: &[engram_graph::Node], fallback_penalty: f64) -> f64 {
    let mut score = 1.0_f64;
    for w in path.windows(2) {
        let prev = w[0].node_type.as_str();
        let curr = w[1].node_type.as_str();
        let hop_penalty = match (prev, curr) {
            ("control", "control") => 0.25,   // DataBinding — declarative
            ("page", "control") => 0.05,      // Contains — structural
            ("page", "class") => 0.05,        // Inherits
            ("control", "function") => 0.00,  // EventWiring — direct
            ("function", "function") => 0.05, // Calls — indirect but typed
            (_, "inline_sql") | (_, "stored_proc") => 0.00, // terminal evidence
            _ => 0.10,                        // unknown / generic dependency
        };
        score -= hop_penalty;
    }
    score -= fallback_penalty;
    score.clamp(0.10, 1.0)
}

/// True iff `file_pattern` (from a `RepoRule`) matches `target_path`. The
/// supported forms mirror what `inject_repo_rules` understands: exact
/// equality, globs with `*` / `?`, and plain substring for short patterns
/// without metacharacters. Slash direction is normalised so Windows
/// callers don't get tripped up by backslashes.
fn immune_rule_matches_path(file_pattern: &str, target_path: &str) -> bool {
    if file_pattern.is_empty() {
        return false;
    }
    let pat = file_pattern.replace('\\', "/").to_lowercase();
    let path = target_path.replace('\\', "/").to_lowercase();
    if pat == path {
        return true;
    }
    if pat.contains('*') || pat.contains('?') {
        // Minimal glob → regex: escape everything except `*` and `?`.
        let mut re = String::with_capacity(pat.len() + 8);
        re.push('^');
        for c in pat.chars() {
            match c {
                '*' => re.push_str(".*"),
                '?' => re.push('.'),
                '.' | '+' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$' | '\\' => {
                    re.push('\\');
                    re.push(c);
                }
                _ => re.push(c),
            }
        }
        re.push('$');
        if let Ok(compiled) = regex::Regex::new(&re) {
            return compiled.is_match(&path);
        }
        return false;
    }
    // Fall back to substring for bare non-glob patterns.
    path.contains(&pat)
}

/// Scan a snippet for deterministically-dangerous operations. Returns a
/// sorted, de-duplicated list of pattern names that fired — used for both
/// verdict escalation and human-readable output.
///
/// The list is intentionally conservative: every matcher here is a
/// pattern that is rarely benign on a DAL / DDL surface. False-positive
/// tolerance is low because firing this list only proposes a verdict
/// floor — the full ladder still consults similarity and match counts.
fn detect_destructive_patterns(code: &str) -> Vec<String> {
    use std::sync::LazyLock;
    // (name, pattern) — names flow through to the output, patterns are
    // compiled once.
    static PATTERNS: LazyLock<Vec<(&'static str, regex::Regex)>> = LazyLock::new(|| {
        let raw: &[(&str, &str)] = &[
            // LINQ-to-SQL bulk helpers.
            ("DeleteAllOnSubmit", r"(?i)\bDeleteAllOnSubmit\s*\("),
            ("InsertAllOnSubmit", r"(?i)\bInsertAllOnSubmit\s*\("),
            // EF Core bulk helpers.
            ("RemoveRange", r"(?i)\bRemoveRange\s*\("),
            ("ExecuteDelete", r"(?i)\bExecuteDelete\s*\("),
            // Raw SQL DDL and bulk mutations. `\b` on both sides keeps
            // these from firing on `TRUNCATED_COLUMN_NAME` identifiers.
            ("DROP TABLE", r"(?i)\bDROP\s+TABLE\b"),
            ("TRUNCATE TABLE", r"(?i)\bTRUNCATE\s+TABLE\b"),
            // DELETE FROM — any occurrence is suspicious on a DAL file.
            // Deliberately broad: a false positive only proposes an
            // escalation floor, it never blocks on its own. Refining
            // this into "DELETE without WHERE" needs a real SQL parser
            // and is not worth the complexity at this stage.
            ("DELETE FROM", r"(?i)\bDELETE\s+FROM\s+[\[\]\w.]+"),
            // Raw SQL passed through ADO.NET / Dapper / EF execution.
            // When `ExecuteNonQuery` / `ExecuteSql` / `Execute` appears
            // ANYWHERE in the same snippet as a `DELETE` / `DROP` /
            // `TRUNCATE` literal, that's the textbook shape we flag.
            (
                "ExecuteNonQuery + destructive SQL",
                r#"(?is)\bExecute(?:NonQuery|Sql|SqlRaw|SqlInterpolated)\b[\s\S]*?\b(?:DELETE|DROP|TRUNCATE)\b|\b(?:DELETE|DROP|TRUNCATE)\b[\s\S]*?\bExecute(?:NonQuery|Sql|SqlRaw|SqlInterpolated)\b"#,
            ),
        ];
        raw.iter()
            .filter_map(|(name, pat)| regex::Regex::new(pat).ok().map(|re| (*name, re)))
            .collect()
    });
    let mut hits: Vec<String> = PATTERNS
        .iter()
        .filter(|(_, re)| re.is_match(code))
        .map(|(name, _)| name.to_string())
        .collect();
    hits.sort();
    hits.dedup();
    hits
}

fn format_ambiguous_symbol_error(input: &str, candidates: &[engram_graph::Node]) -> String {
    let details = candidates
        .iter()
        .take(10)
        .map(|n| {
            format!(
                "  - {} [{}] {}",
                n.node_id,
                n.node_type,
                n.file_path.as_str()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Symbol '{input}' is ambiguous ({} matches):\n{}\n\nPass a fully-qualified node_id (e.g., sym:/file:/table:) to disambiguate.",
        candidates.len(),
        details
    )
}

impl Engram {
    pub(crate) async fn cancel_job_internal(
        &self,
        job_id: &str,
    ) -> job_service::CancellationOutcome {
        job_service::cancel_job_internal(&self.state, job_id).await
    }

    pub(crate) async fn cognitive_analyze_file_style(
        &self,
        project_id: &str,
        file_path: &str,
        diff_limit: usize,
    ) -> cognitive_service::StyleAnalysisResult {
        cognitive_service::analyze_file_style(&self.state, project_id, file_path, diff_limit).await
    }

    pub(crate) async fn cognitive_suggest_boundaries(
        &self,
        project_id: &str,
        min_frequency: u32,
        max_clusters: usize,
        timeout_secs: u64,
        include_cross_cluster_deps: bool,
    ) -> anyhow::Result<Vec<engram_ml::MigrationBoundary>> {
        cognitive_service::suggest_migration_boundaries(
            &self.state,
            project_id,
            min_frequency,
            max_clusters,
            timeout_secs,
            include_cross_cluster_deps,
        )
        .await
    }

    pub async fn handle_impact_analysis(
        &self,
        req: ImpactAnalysisRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let _ = self.ensure_project_runtime(&req.project_id).await?;

        if req.symbol_fqn.is_none() && req.file_path.is_none() {
            return Err(McpError::invalid_params(
                "Either file_path or symbol_fqn must be provided.",
                None,
            ));
        }

        let file_path_for_confidence = req.file_path.clone();
        let project_id_outer = req.project_id.clone();
        let graph = self.state.graph.clone();
        let out = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let target_id = if let Some(ref fqn) = req.symbol_fqn {
                if fqn.starts_with("sym:") {
                    if graph
                        .get_node(&req.project_id, fqn)
                        .map_err(|e| e.to_string())?
                        .is_none()
                    {
                        return Ok(format!("node_id '{fqn}' not found in project."));
                    }
                    fqn.clone()
                } else if fqn.starts_with("sql:")
                    || fqn.starts_with("table:")
                    || fqn.starts_with("state:")
                {
                    fqn.clone()
                } else {
                    match graph
                        .resolve_symbol(&req.project_id, fqn, None, req.file_path.as_deref())
                        .map_err(|e| e.to_string())?
                    {
                        ResolveResult::Unique(node) => node.node_id,
                        ResolveResult::Ambiguous(candidates) => {
                            return Ok(format_ambiguous_symbol_error(fqn, &candidates));
                        }
                        ResolveResult::NotFound => {
                            return Ok(format!(
                                "Symbol '{fqn}' was not found. Use query_graph_nodes to find the exact node_id/name, then retry with that node_id."
                            ));
                        }
                    }
                }
            } else if let Some(ref path) = req.file_path {
                engram_core::ids::NodeId::file(path).0
            } else {
                unreachable!()
            };

            tracing::info!(
                project_id = %req.project_id,
                symbol_fqn = ?req.symbol_fqn,
                resolved_target_id = %target_id,
                "impact_analysis: resolved symbol to node_id"
            );

            let capped_limit = req.limit.clamp(1, 1000);
            let mut incoming = graph
                .find_incoming_edges_with_kind(&req.project_id, None, &target_id, capped_limit)
                .map_err(|e| e.to_string())?;
            // Parallel list of (kind, source, REAL target) triples for the
            // confidence batch lookup — kept in lockstep with `incoming`.
            let mut conf_triples: Vec<(engram_graph::EdgeKind, String, String)> = incoming
                .iter()
                .map(|(src, kind, _)| (kind.clone(), src.clone(), target_id.clone()))
                .collect();

            // File-level transitive aggregation — mirrors what
            // `compute_blast_radius` does in `blast_radius_service.rs`.
            // A raw `file:…` node carries almost no direct incoming
            // edges on a typical project; every real dependent lands
            // on the symbols inside the file via `Contains`. Without
            // this pass, `impact_analysis` on a shared utility file
            // (e.g. `Site/App_Code/shared-code/sharedfunc.vb` on
            // OciusX — 1000+ real dependents) returned zero.
            if target_id.starts_with("file:") {
                // Resolve the file's rel_path. Prefer the persisted
                // node metadata (handles slash / encoding quirks) and
                // fall back to stripping the `file:` prefix off the
                // id. This mirrors what `compute_blast_radius` does
                // in commit `64637ce`.
                let file_rel_path = graph
                    .get_node(&req.project_id, &target_id)
                    .ok()
                    .flatten()
                    .map(|n| n.file_path.as_str().to_string())
                    .unwrap_or_else(|| target_id[5..].to_string());

                // IMPORTANT: do NOT use `graph.neighbors(Contains, …)`
                // to find contained symbols. On several projects the
                // Contains edges for file-shaped sources live in the
                // EDGES table but not in ADJ_OUT (verified by
                // `traverse_graph(file:…, contains, outgoing)`
                // returning zero on OciusX). The authoritative
                // containment signal is `Node.file_path` equality —
                // every symbol node stores its owning file.
                //
                // `query_nodes` with a `Some(file_path)` filter already
                // does case-insensitive + slash-normalised substring
                // matching; we add an exact-equality post-filter so a
                // file whose path is a suffix of another file's path
                // can't bleed symbols in.
                let contained: std::collections::HashSet<String> = graph
                    .query_nodes(&req.project_id, None, None, Some(&file_rel_path), 50_000)
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .filter(|n| {
                        n.node_id != target_id && n.file_path.as_str() == file_rel_path
                    })
                    .map(|n| n.node_id)
                    .collect();

                tracing::info!(
                    project_id = %req.project_id,
                    target_id = %target_id,
                    file_rel_path = %file_rel_path,
                    contained_symbols = contained.len(),
                    "impact_analysis: resolved contained symbols via file_path equality"
                );

                if !contained.is_empty() {
                    // O(sum of in-degrees) via ADJ_IN per contained symbol —
                    // this used to be `list_edges(None)`, a full scan of
                    // EVERY edge in the project on each file-level call.
                    let mut added = 0usize;
                    'agg: for sym_id in &contained {
                        let Ok(sym_incoming) = graph.find_incoming_edges_with_kind(
                            &req.project_id,
                            None,
                            sym_id,
                            200,
                        ) else {
                            continue;
                        };
                        for (src, kind, weight) in sym_incoming {
                            if src == target_id {
                                continue;
                            }
                            // Do not re-include intra-file Contains edges
                            // (namespace → class, class → function) as
                            // "dependents" — structural parents, not usages.
                            if contained.contains(&src)
                                && kind == engram_graph::EdgeKind::Contains
                            {
                                continue;
                            }
                            conf_triples.push((kind.clone(), src.clone(), sym_id.clone()));
                            incoming.push((src, kind, weight));
                            added += 1;
                            if incoming.len() >= capped_limit {
                                break 'agg;
                            }
                        }
                    }
                    tracing::info!(
                        project_id = %req.project_id,
                        target_id = %target_id,
                        contained_symbols = contained.len(),
                        transitive_added = added,
                        "impact_analysis: file-level transitive aggregation"
                    );
                }
            }

            if incoming.is_empty() {
                return Ok(format!(
                    "No dependent nodes found for {target_id}.\n\
                     next: find_symbol_references(<name>) for a lexical fallback; \
                     resolve_id(<name>) to check you targeted the right node; \
                     get_index_freshness if the code is newer than the index."
                ));
            }
            let incoming_edge_count = incoming.len();

            // Same phantom-edge discount blast_radius applies (TODO-12):
            // bare-name bindings (app JS calling `new Map()` hitting a class
            // named Map) carry extraction confidence < 1.0 — without the
            // discount they inflate the dependent list at full weight.
            let confidences: Vec<f32> = graph
                .get_edge_confidences(&req.project_id, &conf_triples)
                .map(|v| {
                    v.into_iter()
                        .map(|c| c.unwrap_or(1.0).clamp(0.0, 1.0))
                        .collect()
                })
                .unwrap_or_else(|_| vec![1.0; incoming.len()]);

            let mut out = format!("Impact Analysis for {target_id}:\n\n");
            out.push_str("Nodes that depend on or are related to this:\n");

            type Grouped = (Vec<engram_graph::EdgeKind>, u32, f32);
            let mut grouped: std::collections::HashMap<String, Grouped> =
                std::collections::HashMap::new();
            for (i, (src_id, kind, weight)) in incoming.into_iter().enumerate() {
                let conf = confidences.get(i).copied().unwrap_or(1.0);
                let entry = grouped.entry(src_id).or_insert((Vec::new(), 0, 1.0));
                entry.0.push(kind);
                if weight > entry.1 {
                    entry.1 = weight;
                }
                if conf < entry.2 {
                    entry.2 = conf;
                }
            }

            let mut sorted: Vec<_> = grouped.into_iter().collect();
            // Confident dependents first; low-confidence (bare-name) ones
            // are still listed but demoted and tagged, not silently counted
            // at full strength.
            sorted.sort_by(|a, b| {
                let a_low = a.1.2 < 0.6;
                let b_low = b.1.2 < 0.6;
                a_low
                    .cmp(&b_low)
                    .then_with(|| b.1.1.cmp(&a.1.1))
            });

            tracing::info!(
                project_id = %req.project_id,
                target_id = %target_id,
                incoming_edge_count = incoming_edge_count,
                grouped_source_count = sorted.len(),
                "impact_analysis: pre-render counts"
            );

            let mut unresolved_count = 0usize;
            let mut low_conf_count = 0usize;
            for (src_id, (kinds, weight, min_conf)) in sorted {
                let src_node = graph
                    .get_node(&req.project_id, &src_id)
                    .map_err(|e| e.to_string())?;

                if src_node.is_none() {
                    unresolved_count += 1;
                    tracing::debug!(
                        project_id = %req.project_id,
                        src_id = %src_id,
                        "impact_analysis: source node_id has no persisted node record"
                    );
                }

                let mut reasons = Vec::new();
                for ek in kinds {
                    let r = match ek {
                        engram_graph::EdgeKind::Calls => "Calls this",
                        engram_graph::EdgeKind::Dependency => "Calls/Uses this",
                        engram_graph::EdgeKind::Contains => "Contains this",
                        engram_graph::EdgeKind::Imports => "Imports this",
                        engram_graph::EdgeKind::SqlCalls => "Executes this SQL",
                        engram_graph::EdgeKind::CoOccurrence => {
                            "Often searched with this (Co-occurrence)"
                        }
                        engram_graph::EdgeKind::TemporalCoupling => {
                            "Often changed with this (Temporal coupling)"
                        }
                        engram_graph::EdgeKind::QueriesTable => "Queries this table",
                        engram_graph::EdgeKind::ReadsState => "Reads this state",
                        engram_graph::EdgeKind::WritesState => "Writes this state",
                        engram_graph::EdgeKind::HasColumn => "Has column",
                        engram_graph::EdgeKind::ForeignKey => "Foreign key reference",
                        _ => "Related",
                    };
                    reasons.push(r);
                }
                reasons.sort();
                reasons.dedup();

                let reason_str = if reasons.is_empty() {
                    "Dependent".to_string()
                } else {
                    reasons.join(", ")
                };
                let (display_id, display_type) = match src_node.as_ref() {
                    Some(n) => (n.node_id.as_str(), n.node_type.as_str()),
                    None => (src_id.as_str(), "unresolved"),
                };
                // Show the FQN alongside the location-based node_id — that's
                // the name humans and agents actually identify symbols by.
                let fqn_suffix = src_node
                    .as_ref()
                    .and_then(|n| n.metadata.as_ref())
                    .and_then(|m| m.get("fqn"))
                    .and_then(|v| v.as_str())
                    .map(|f| format!(" ({f})"))
                    .unwrap_or_default();

                let conf_tag = if min_conf < 0.6 {
                    low_conf_count += 1;
                    format!(" ⚠ low-confidence match ({min_conf:.2}) — likely bare-name collision")
                } else {
                    String::new()
                };
                out.push_str(&format!(
                    "- {}{} [{}] (weight: {weight}) - {reason_str}{conf_tag}\n",
                    display_id, fqn_suffix, display_type
                ));
            }

            if low_conf_count > 0 {
                out.push_str(&format!(
                    "\n({low_conf_count} dependent(s) are LOW-CONFIDENCE bare-name matches — \
                     demoted to the bottom; verify with grep_project before treating them \
                     as real callers.)\n"
                ));
            }
            if unresolved_count > 0 {
                out.push_str(&format!(
                    "\n(Note: {unresolved_count} source edges pointed at node_ids with no persisted node record. \
                     This indicates an indexing integrity issue — edges were created but corresponding nodes \
                     were not. The entries above are still real dependencies.)\n"
                ));
            }
            out.push_str(
                "\nnext: compute_blast_radius(<symbol>) for the risk score + seam candidates; \
                 check_edit_safety(<method>) before editing.\n",
            );

            Ok(out)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        let mut result = out;
        if let Some(ref fp) = file_path_for_confidence {
            let rel = engram_core::RelPath::from(fp.as_str());
            let lang = engram_core::guess_language(std::path::Path::new(fp));
            result.push_str(&self.confidence_footer(&rel, lang));
        }
        let gen_ = self
            .get_active_generation(&project_id_outer)
            .await
            .unwrap_or(1);
        result.push_str(&self.freshness_footer(&project_id_outer, gen_).await);

        Ok(CallToolResult::success(vec![Content::text(result)]))
    }

    pub async fn handle_get_table_schema(
        &self,
        req: crate::models::GetTableSchemaRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let _ = self.ensure_project_runtime(&req.project_id).await?;

        let graph = self.state.graph.clone();
        let out = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let table_id = engram_core::ids::NodeId::table(&req.table_name).0;

            let table_node = graph
                .get_node(&req.project_id, &table_id)
                .map_err(|e| e.to_string())?;

            let Some(table_node) = table_node else {
                let candidates = graph
                    .query_nodes(
                        &req.project_id,
                        Some("db_table"),
                        Some(&req.table_name),
                        None,
                        10,
                    )
                    .map_err(|e| e.to_string())?;
                if candidates.is_empty() {
                    return Ok(format!(
                        "Table '{}' not found. Make sure the project has .sql DDL files indexed.",
                        req.table_name
                    ));
                }
                let names: Vec<_> = candidates.iter().map(|n| n.name.as_str()).collect();
                return Ok(format!(
                    "Table '{}' not found exactly. Did you mean one of: {}?",
                    req.table_name,
                    names.join(", ")
                ));
            };

            let mut out = format!("## Table: {}\n\n", table_node.name);

            if let Some(ref meta) = table_node.metadata
                && let Some(ddl) = meta.get("ddl").and_then(|v| v.as_str())
            {
                out.push_str("### DDL\n```sql\n");
                out.push_str(ddl);
                out.push_str("\n```\n\n");
            }

            let columns = graph
                .neighbors(&req.project_id, EdgeKind::HasColumn, &table_id, 200)
                .map_err(|e| e.to_string())?;

            if !columns.is_empty() {
                out.push_str("### Columns\n");
                for (col_id, _weight) in &columns {
                    if let Ok(Some(col_node)) = graph.get_node(&req.project_id, col_id) {
                        let data_type = col_node
                            .metadata
                            .as_ref()
                            .and_then(|m| m.get("data_type"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("?");
                        let nullable = col_node
                            .metadata
                            .as_ref()
                            .and_then(|m| m.get("nullable"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("?");
                        out.push_str(&format!(
                            "- **{}** {} (nullable: {})\n",
                            col_node.name, data_type, nullable
                        ));
                    }
                }
                out.push('\n');
            }

            let mut fk_lines = Vec::new();
            for (col_id, _) in &columns {
                let fks = graph
                    .neighbors(&req.project_id, EdgeKind::ForeignKey, col_id, 50)
                    .map_err(|e| e.to_string())?;
                for (ref_col_id, _) in fks {
                    fk_lines.push(format!("- {} -> {}", col_id, ref_col_id));
                }
            }
            if !fk_lines.is_empty() {
                out.push_str("### Foreign Keys\n");
                for line in &fk_lines {
                    out.push_str(line);
                    out.push('\n');
                }
                out.push('\n');
            }

            let referencing = graph
                .find_incoming_edges(&req.project_id, Some(EdgeKind::QueriesTable), &table_id, 50)
                .map_err(|e| e.to_string())?;

            if !referencing.is_empty() {
                out.push_str("### Referenced by SQL Nodes\n");
                for (sql_id, weight) in &referencing {
                    let callers = graph
                        .find_incoming_edges(&req.project_id, Some(EdgeKind::SqlCalls), sql_id, 20)
                        .map_err(|e| e.to_string())?;
                    let caller_strs: Vec<_> = callers.iter().map(|(id, _)| id.as_str()).collect();
                    if caller_strs.is_empty() {
                        out.push_str(&format!("- {} (weight: {})\n", sql_id, weight));
                    } else {
                        out.push_str(&format!(
                            "- {} (weight: {}) <- called from: {}\n",
                            sql_id,
                            weight,
                            caller_strs.join(", ")
                        ));
                    }
                }
            }

            Ok(out)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_trace_state_usage(
        &self,
        req: TraceStateUsageRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let _ = self.ensure_project_runtime(&req.project_id).await?;

        let project_id_outer = req.project_id.clone();
        let graph = self.state.graph.clone();
        let out = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let state_id = engram_core::ids::NodeId::state(&req.state_type, &req.state_key).0;

            let state_node = graph
                .get_node(&req.project_id, &state_id)
                .map_err(|e| format!("DB error looking up state node: {e}"))?;

            if state_node.is_none() {
                let candidates = graph
                    .query_nodes(
                        &req.project_id,
                        Some("global_state"),
                        Some(&req.state_key),
                        None,
                        20,
                    )
                    .map_err(|e| format!("DB error querying state candidates: {e}"))?;
                if candidates.is_empty() {
                    return Ok(format!(
                        "State key '{}[\"{}\"]' not found in the graph.\nMake sure the project has C#/VB files with {} access indexed.",
                        req.state_type, req.state_key, req.state_type
                    ));
                }
                let names: Vec<_> = candidates.iter().map(|n| n.name.as_str()).collect();
                return Ok(format!(
                    "State key '{}:{}' not found exactly. Similar keys found: {}",
                    req.state_type,
                    req.state_key,
                    names.join(", ")
                ));
            }

            let mut out = format!(
                "## State Usage: {}[\"{}\"]\n\n",
                req.state_type, req.state_key
            );

            // MCP1: use sanitized limit to prevent resource amplification.
            let limit = req.sanitized_limit();
            let writers = graph
                .find_incoming_edges(
                    &req.project_id,
                    Some(EdgeKind::WritesState),
                    &state_id,
                    limit,
                )
                .map_err(|e| format!("DB error querying writers: {e}"))?;

            if !writers.is_empty() {
                out.push_str("### Writers\n");
                for (writer_id, weight) in &writers {
                    if let Ok(Some(node)) = graph.get_node(&req.project_id, writer_id) {
                        out.push_str(&format!(
                            "- {} [{}] in {}:{} (weight: {})\n",
                            node.name,
                            node.node_type,
                            node.file_path.as_str(),
                            node.start_line,
                            weight
                        ));
                    } else {
                        out.push_str(&format!("- {} (weight: {})\n", writer_id, weight));
                    }
                }
                out.push('\n');
            } else {
                out.push_str("### Writers\nNo writers found.\n\n");
            }

            let readers = graph
                .find_incoming_edges(
                    &req.project_id,
                    Some(EdgeKind::ReadsState),
                    &state_id,
                    limit,
                )
                .map_err(|e| format!("DB error querying readers: {e}"))?;

            if !readers.is_empty() {
                out.push_str("### Readers\n");
                for (reader_id, weight) in &readers {
                    if let Ok(Some(node)) = graph.get_node(&req.project_id, reader_id) {
                        out.push_str(&format!(
                            "- {} [{}] in {}:{} (weight: {})\n",
                            node.name,
                            node.node_type,
                            node.file_path.as_str(),
                            node.start_line,
                            weight
                        ));
                    } else {
                        out.push_str(&format!("- {} (weight: {})\n", reader_id, weight));
                    }
                }
            } else {
                out.push_str("### Readers\nNo readers found.\n");
            }
            out.push_str(
                "\nnext: writers before readers when changing the shape of this value; \
                 detect_incomplete_changes(edited_files=[...]) after editing any \
                 subset of these sites — shared state is the classic missed-companion.\n",
            );

            Ok(out)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        let mut result = out;
        let gen_ = self
            .get_active_generation(&project_id_outer)
            .await
            .unwrap_or(1);
        result.push_str(&self.freshness_footer(&project_id_outer, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(result)]))
    }

    pub async fn handle_trace_ui_event(
        &self,
        req: TraceUiEventRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let _ = self.ensure_project_runtime(&req.project_id).await?;

        let mut start_id = if let Some(ref ctrl) = req.control_id {
            engram_core::ids::NodeId::control(&req.page_path, ctrl).0
        } else if let Some(ref handler) = req.handler_fqn {
            engram_core::ids::NodeId::symbol("function", Some(handler), &req.page_path, "", 0).0
        } else {
            engram_core::ids::NodeId::page(&req.page_path).0
        };

        let mut trace_used_fallback = false;
        let mut trace_candidate_count: usize = 0;
        let mut unresolved_candidates: Vec<String> = Vec::new();
        let start_missing = self
            .state
            .graph
            .get_node(&req.project_id, &start_id)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .is_none();
        if start_missing && let Some(ref ctrl) = req.control_id {
            match self
                .state
                .graph
                .resolve_symbol(&req.project_id, ctrl, Some("control"), Some(&req.page_path))
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
            {
                ResolveResult::Unique(node) => {
                    trace_used_fallback = true;
                    trace_candidate_count = 1;
                    unresolved_candidates = vec![node.node_id.clone()];
                    start_id = node.node_id;
                }
                ResolveResult::Ambiguous(candidates) => {
                    return Err(McpError::invalid_params(
                        format_ambiguous_symbol_error(ctrl, &candidates),
                        None,
                    ));
                }
                ResolveResult::NotFound => {}
            }
        } else if start_missing && let Some(ref handler) = req.handler_fqn {
            // Symbol node IDs are location-based; an FQN start point must be
            // resolved via resolve_symbol (exact name → metadata fqn →
            // terminal segment).
            match self
                .state
                .graph
                .resolve_symbol(&req.project_id, handler, Some("function"), None)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
            {
                ResolveResult::Unique(node) => {
                    start_id = node.node_id;
                }
                ResolveResult::Ambiguous(candidates) => {
                    return Err(McpError::invalid_params(
                        format_ambiguous_symbol_error(handler, &candidates),
                        None,
                    ));
                }
                ResolveResult::NotFound => {}
            }
        }

        let paths = self
            .state
            .graph
            .find_ui_paths(
                &req.project_id,
                &start_id,
                req.sanitized_max_hops(),
                req.sanitized_max_paths(),
            )
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if paths.is_empty() {
            // The MOST COMMON legacy case is a handler that never reaches
            // SQL (or wiring the extractor missed) — a bare dead-end here
            // wasted the agent's most likely follow-up moves.
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "No paths found from {start_id} to any SQL nodes within {} hops.\n\
                 next steps to trace it anyway:\n\
                 - raise max_hops (deep pages often need 4)\n\
                 - find_symbol_references(<the handler>) — the data access may live \
                 in a helper the handler calls\n\
                 - trace_ui_action(control_id=...) for the DOM/AJAX side of the wiring\n\
                 - if the control lives on a MasterPage/UserControl, trace from THAT \
                 file's handler instead\n\
                 - grep_project(\"<control id>\") to see every literal wiring site",
                req.max_hops
            ))]));
        }

        let mut out = format!("Found {} path(s) to SQL:\n", paths.len());

        let fallback_penalty = if trace_used_fallback {
            (trace_candidate_count as f64 * 0.2).min(0.8)
        } else {
            0.0
        };

        // Score every path by hop kinds. Paths that cross a
        // `control → control` hop (DataBinding — GridView DataSourceID
        // binding to a LinqDataSource / ObjectDataSource / SqlDataSource)
        // are declarative: the handler fires when ASP.NET chooses to
        // bind, not from an explicit user action. That's objectively
        // less certain evidence of "change in handler X affects this
        // control" than a direct `control → function` hop (EventWiring
        // via OnClick / Handles). Penalise DataBinding hops so callers
        // reading the provenance see the weaker paths ranked lower.
        //
        // Per-hop penalty table (subtracted from a starting confidence
        // of 1.0, then floored at 0.1 so a wholly declarative chain
        // still reports something non-zero):
        //   control→control   -0.25  (DataBinding — declarative)
        //   page→control      -0.05  (Contains — structural)
        //   control→function  -0.00  (EventWiring — direct)
        //   function→function -0.05  (Calls — indirect but typed)
        //   *→inline_sql      -0.00  (terminal evidence)
        //   *→stored_proc     -0.00  (terminal evidence)
        //   *→*               -0.10  (other / unknown)
        let path_scores: Vec<f64> = paths
            .iter()
            .map(|p| path_confidence_score(p, fallback_penalty))
            .collect();
        let best_path_score = path_scores
            .iter()
            .copied()
            .fold(0.0f64, |a, b| if a > b { a } else { b });

        out.push_str("\n## Trace Provenance\n");
        out.push_str(&format!("trace_used_fallback: {}\n", trace_used_fallback));
        out.push_str(&format!(
            "trace_candidate_count: {}\n",
            trace_candidate_count
        ));
        out.push_str(&format!(
            "trace_confidence_penalty_fallback: {:.2}\n",
            fallback_penalty
        ));
        out.push_str(&format!("best_path_confidence: {:.2}\n", best_path_score));
        out.push_str(&format!("selected_start_node: {}\n", start_id));

        if trace_used_fallback {
            out.push_str(&format!(
                "\n### Ambiguity Warning\n\
                 Control lookup used fallback candidate matching ({} candidates found).\n\
                 Penalty reason: {} candidate(s) matched control ID filter; first-match selected.\n\
                 Risk: Incorrect handler resolution may lead to wrong trace path.\n",
                trace_candidate_count, trace_candidate_count
            ));
            out.push_str("\n### Unresolved Candidates\n");
            for (i, cand) in unresolved_candidates.iter().enumerate() {
                let selected = if i == 0 { " ← SELECTED" } else { "" };
                out.push_str(&format!("  {}. {}{}\n", i + 1, cand, selected));
            }
        }

        // Low-confidence paths — emit actionable follow-up probes so the
        // caller can tighten the trace rather than accepting the
        // ambiguous result at face value. Fires for BOTH fallback
        // resolution AND paths that cross a DataBinding hop.
        let has_data_binding_hop = paths.iter().any(|p| {
            p.windows(2)
                .any(|w| w[0].node_type == "control" && w[1].node_type == "control")
        });
        if trace_used_fallback || has_data_binding_hop || best_path_score < 0.75 {
            out.push_str("\n### Follow-up Probes\n");
            if trace_used_fallback {
                out.push_str("- Provide explicit `handler_fqn` to disambiguate control lookup\n");
                out.push_str("- Verify control ID uniqueness across master / user controls\n");
                out.push_str("- Check code-behind inheritance chain for handler shadowing\n");
            }
            if has_data_binding_hop {
                out.push_str(
                    "- Path crosses a declarative data-binding hop (control → control). \
                     Run `trace_data_flow` on the data-source control's event handler for \
                     deeper SQL tracing\n",
                );
                out.push_str(
                    "- Run `find_symbol_references` on the resolved handler method to find \
                     alternate call paths that bypass this trace\n",
                );
            }
            if best_path_score < 0.75 && !trace_used_fallback && !has_data_binding_hop {
                out.push_str(
                    "- Best path confidence below 0.75. Consider narrowing with a more \
                     specific `handler_fqn` or re-running `trace_ui_action` against the \
                     button that initiates this flow\n",
                );
            }
        }

        for (i, path) in paths.iter().enumerate() {
            let score = path_scores.get(i).copied().unwrap_or(0.0);
            let confidence_tag = if score >= 0.85 {
                "high"
            } else if score >= 0.6 {
                "medium"
            } else {
                "low"
            };
            out.push_str(&format!(
                "\n## Path #{} (confidence {:.2} — {})\n",
                i + 1,
                score,
                confidence_tag
            ));
            for (step, node) in path.iter().enumerate() {
                let label = match node.node_type.as_str() {
                    "page" => "ASPX Page",
                    "control" => "UI Control",
                    "function" => "Code-Behind Handler",
                    "stored_proc" => "Stored Procedure",
                    "inline_sql" => "Inline SQL",
                    _ => &node.node_type,
                };

                let (justification, hop_kind) = if step == 0 {
                    ("Starting point".to_string(), "start")
                } else {
                    let prev = &path[step - 1];
                    match (prev.node_type.as_str(), node.node_type.as_str()) {
                        ("page", "class") => ("Inherits class".to_string(), "inherits"),
                        ("page", "control") => ("Contains control".to_string(), "contains"),
                        ("control", "control") => (
                            "Declarative data binding (DataSourceID)".to_string(),
                            "data_binding",
                        ),
                        ("control", "function") => (
                            "Event wiring (OnClick / Handles)".to_string(),
                            "event_wiring",
                        ),
                        ("function", "function") => ("Method call".to_string(), "calls"),
                        (_, "inline_sql") | (_, "stored_proc") => {
                            ("Executes SQL".to_string(), "sql_terminal")
                        }
                        _ => ("Dependency".to_string(), "dependency"),
                    }
                };

                let evidence = format!(
                    "node_type={}, hop_kind={}, file={}, lines={}-{}",
                    node.node_type,
                    hop_kind,
                    node.file_path.as_str(),
                    node.start_line,
                    node.end_line
                );

                // Prefer the metadata FQN for display: node IDs are
                // location-based, but humans and agents identify handlers by
                // their fully-qualified name.
                let display_name = node
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("fqn"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(&node.name);

                let indent = "  ".repeat(step);
                out.push_str(&format!(
                    "{indent}Step {}: {} [{}] ({}) - {} | evidence: {}\n",
                    step + 1,
                    display_name,
                    label,
                    node.node_id,
                    justification,
                    evidence
                ));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_trace_ui_action(
        &self,
        req: TraceUiActionRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;

        let hits = ps
            .search
            .search(
                &HybridQuery {
                    project_id: req.project_id.clone(),
                    namespace: "memory".into(),
                    generation: gen_,
                    text: req.query.clone(),
                    top_k: 10,
                    fts_mode: "loose".into(),
                    include_path_prefixes: None,
                    exclude_path_prefixes: None,
                    language_filters: None,
                    author_filter: None,
                    date_after: None,
                    date_before: None,
                    use_mmr: false,
                },
                None,
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let mut start_nodes = Vec::new();
        for h in &hits {
            let nodes = self
                .state
                .graph
                .query_nodes(&req.project_id, None, None, Some(h.path.as_str()), 10)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            for n in nodes {
                if matches!(n.node_type.as_str(), "control" | "page" | "function") {
                    start_nodes.push(n.node_id);
                }
            }
        }
        start_nodes.sort();
        start_nodes.dedup();

        if start_nodes.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No UI controls or handlers found for the query.",
            )]));
        }

        let mut out = format!("UI Trace results for '{}':\n", req.query);
        let mut paths_found = 0;

        let edge_kinds = vec![
            engram_graph::EdgeKind::Contains,
            engram_graph::EdgeKind::Dependency,
            // Raw `calls` edges map to their own kind (restored through the
            // ingest pipeline) — without it the trace stops at the handler
            // and never shows downstream calls.
            engram_graph::EdgeKind::Calls,
        ];

        for start_id in start_nodes {
            if paths_found >= req.sanitized_max_paths() {
                break;
            }

            let paths = self
                .state
                .graph
                .traverse(
                    &req.project_id,
                    &start_id,
                    req.sanitized_max_depth(),
                    Some(edge_kinds.clone()),
                    "out",
                )
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            if paths.len() > 1 {
                paths_found += 1;
                out.push_str(&format!("\nPath starting at {}:\n", start_id));
                for (n, depth) in paths {
                    let indent = "  ".repeat(depth);
                    out.push_str(&indent);
                    out.push_str(&format!(
                        "- {} | {} | {} (lines {}-{})\n",
                        n.node_id, n.node_type, n.file_path, n.start_line, n.end_line
                    ));
                }
            }
        }

        if paths_found == 0 {
            return Ok(CallToolResult::success(vec![Content::text(
                "No call chains found from identified UI elements.",
            )]));
        }

        Ok(CallToolResult::success(vec![Content::text(
            out.trim().to_string(),
        )]))
    }

    pub async fn handle_export_capture_pack(
        &self,
        req: ExportCapturePackRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let pid = req.project_id.clone();
        let _ps = self.ensure_project_runtime(&pid).await?;

        // Fetch overview text before entering spawn_blocking (it's async)
        let overview_result = self
            .handle_get_codebase_overview(ProjectIdRequest {
                project_id: pid.clone(),
            })
            .await?;
        let overview_text = overview_result
            .content
            .first()
            .and_then(|c| {
                if let rmcp::model::RawContent::Text(t) = &c.raw {
                    Some(t.text.clone())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        // Stream the zip directly to disk to avoid OOM for large projects.
        let timestamp = now_ms();
        let data_dir = self.state.cfg.data_dir.clone();
        let exports_dir = data_dir.join("exports").join(&pid);
        tokio::fs::create_dir_all(&exports_dir)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let zip_path = exports_dir.join(format!("{}.zip", timestamp));

        let graph = self.state.graph.clone();
        let pid_clone = pid.clone();
        let zip_path_clone = zip_path.clone();
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let file = std::fs::File::create(&zip_path_clone).map_err(|e| e.to_string())?;
            let writer = std::io::BufWriter::new(file);
            let mut zip = zip::ZipWriter::new(writer);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);

            // 1. overview.md
            zip.start_file("overview.md", options)
                .map_err(|e| e.to_string())?;
            std::io::Write::write_all(&mut zip, overview_text.as_bytes())
                .map_err(|e| e.to_string())?;

            // 2. graph_topology.json
            let all_nodes = graph
                .query_nodes(&pid_clone, None, None, None, 1000)
                .unwrap_or_default();
            let total_node_count = graph.count_nodes(&pid_clone).unwrap_or(all_nodes.len());
            let topo = serde_json::json!({
                "node_count": total_node_count,
                "nodes": all_nodes.iter().map(|n| {
                    serde_json::json!({
                        "id": n.node_id,
                        "type": n.node_type,
                        "name": n.name,
                        "path": n.file_path,
                        "language": n.language
                    })
                }).collect::<Vec<_>>()
            });
            zip.start_file("graph_topology.json", options)
                .map_err(|e| e.to_string())?;
            serde_json::to_writer_pretty(&mut zip, &topo).map_err(|e| e.to_string())?;

            // 3. ui_wiring.json
            let ui_nodes = graph
                .query_nodes(&pid_clone, Some("control"), None, None, 5000)
                .unwrap_or_default();
            let mut wiring = Vec::new();
            for ctrl in ui_nodes {
                let deps = graph
                    .neighbors(&pid_clone, EdgeKind::Dependency, &ctrl.node_id, 10)
                    .unwrap_or_default();
                wiring.push(serde_json::json!({
                    "control": ctrl.node_id,
                    "handlers": deps.iter().map(|(id, _)| id).collect::<Vec<_>>()
                }));
            }
            zip.start_file("ui_wiring.json", options)
                .map_err(|e| e.to_string())?;
            serde_json::to_writer_pretty(&mut zip, &wiring).map_err(|e| e.to_string())?;

            // 4. sql_map.json
            let sql_edges = graph
                .list_edges_by_kind(&pid_clone, EdgeKind::SqlCalls, 5000)
                .unwrap_or_default();
            zip.start_file("sql_map.json", options)
                .map_err(|e| e.to_string())?;
            serde_json::to_writer_pretty(&mut zip, &sql_edges).map_err(|e| e.to_string())?;

            zip.finish().map_err(|e| e.to_string())?;
            Ok(())
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "\u{2705} Capture pack exported to: {}",
            zip_path.to_string_lossy()
        ))]))
    }

    pub async fn handle_get_ui_blueprint(
        &self,
        req: GetUiBlueprintRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let _ = self.ensure_project_runtime(&req.project_id).await?;

        let graph = self.state.graph.clone();
        let out = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let all_containers = graph
                .query_nodes(&req.project_id, Some("ui_container"), None, Some(&req.file_path), 500)
                .map_err(|e| e.to_string())?;

            if all_containers.is_empty() {
                return Ok(format!(
                    "No UI layout data found for '{}'. Ensure the file has been indexed and contains container elements (Panel, Table, GroupBox, div).",
                    req.file_path
                ));
            }

            let mut tree = serde_json::Map::new();
            tree.insert("file".into(), serde_json::Value::String(req.file_path.clone()));

            let mut containers_json = Vec::new();
            for container in &all_containers {
                let mut cobj = serde_json::Map::new();
                cobj.insert("id".into(), serde_json::Value::String(container.name.clone()));
                cobj.insert("node_id".into(), serde_json::Value::String(container.node_id.clone()));

                if let Some(ref meta) = container.metadata {
                    for key in ["container_type", "layout_style", "logical_grouping", "css_class"] {
                        if let Some(val) = meta.get(key).and_then(|v| v.as_str()) {
                            cobj.insert(key.into(), serde_json::Value::String(val.to_string()));
                        }
                    }
                }

                let children = graph
                    .neighbors(&req.project_id, EdgeKind::ContainsUi, &container.node_id, 200)
                    .unwrap_or_default();

                let mut children_json = Vec::new();
                for (child_id, _weight) in &children {
                    let mut child_obj = serde_json::Map::new();
                    child_obj.insert("node_id".into(), serde_json::Value::String(child_id.clone()));

                    if let Ok(Some(child_node)) = graph.get_node(&req.project_id, child_id) {
                        child_obj.insert("name".into(), serde_json::Value::String(child_node.name.clone()));
                        child_obj.insert("type".into(), serde_json::Value::String(child_node.node_type.clone()));

                        if let Some(ref meta) = child_node.metadata {
                            for key in ["ui_label", "row", "col", "logical_grouping", "x", "y"] {
                                if let Some(val) = meta.get(key).and_then(|v| v.as_str()) {
                                    child_obj.insert(key.into(), serde_json::Value::String(val.to_string()));
                                }
                            }
                        }

                        let neighbors = graph
                            .neighbors(&req.project_id, EdgeKind::UiLayoutNeighbor, child_id, 5)
                            .unwrap_or_default();
                        if !neighbors.is_empty() {
                            let next_ids: Vec<serde_json::Value> = neighbors
                                .iter()
                                .map(|(nid, _)| serde_json::Value::String(nid.clone()))
                                .collect();
                            child_obj.insert("next_in_tab_order".into(), serde_json::Value::Array(next_ids));
                        }
                    }
                    children_json.push(serde_json::Value::Object(child_obj));
                }

                cobj.insert("children".into(), serde_json::Value::Array(children_json));
                containers_json.push(serde_json::Value::Object(cobj));
            }

            tree.insert("containers".into(), serde_json::Value::Array(containers_json));
            tree.insert("container_count".into(), serde_json::Value::Number(all_containers.len().into()));
            serde_json::to_string_pretty(&tree).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_get_codebase_overview(
        &self,
        req: ProjectIdRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let pid = req.project_id;
        let rec = self.ensure_project_record(&pid).await?;
        let gen_ = self.get_active_generation(&pid).await.unwrap_or(1);
        let ps = self.ensure_project_runtime(&pid).await?;

        let rules = self.state.registry.clone();
        let pid_clone = pid.clone();
        let rule_count = tokio::task::spawn_blocking(move || rules.list_repo_rules(&pid_clone))
            .await
            .ok()
            .and_then(|r| r.ok())
            .map(|v| v.len())
            .unwrap_or(0);

        let tantivy_docs = ps.search.count_docs(&pid).unwrap_or(0);
        let lancedb_rows = ps.search.count_vectors(&pid).await.unwrap_or(0);
        let ns_counts = ps.search.count_docs_by_namespace(&pid).unwrap_or_default();
        let antipattern_docs = ns_counts.get("antipattern").copied().unwrap_or(0);
        let history_docs = ns_counts.get("history").copied().unwrap_or(0);
        let lang_counts = ps.search.count_docs_by_language(&pid).unwrap_or_default();

        let graph = self.state.graph.clone();
        let pid_clone2 = pid.clone();
        let active_gen = gen_;
        let (
            node_type_counts,
            edge_kind_counts,
            centrality,
            state_usage_data,
            dead_code_count,
            test_file_count,
            total_file_count,
        ) = tokio::task::spawn_blocking(move || {
            let ntc = graph.count_nodes_by_type(&pid_clone2).unwrap_or_default();
            let ekc = graph.count_edges_by_kind(&pid_clone2).unwrap_or_default();
            let pr = engram_graph::analysis::compute_pagerank(&graph, &pid_clone2, active_gen).ok();

            let state_nodes = graph
                .query_nodes(&pid_clone2, Some("global_state"), None, None, 100)
                .unwrap_or_default();
            let mut state_usage: Vec<(String, usize, usize)> =
                Vec::with_capacity(state_nodes.len());
            for sn in &state_nodes {
                let reads = graph
                    .find_incoming_edges(&pid_clone2, Some(EdgeKind::ReadsState), &sn.node_id, 200)
                    .map(|v| v.len())
                    .unwrap_or(0);
                let writes = graph
                    .find_incoming_edges(&pid_clone2, Some(EdgeKind::WritesState), &sn.node_id, 200)
                    .map(|v| v.len())
                    .unwrap_or(0);
                state_usage.push((sn.name.clone(), reads, writes));
            }
            state_usage.sort_by(|a, b| (b.1 + b.2).cmp(&(a.1 + a.2)));

            let mut dead = 0usize;
            let mut test_files = 0usize;
            let file_nodes = graph
                .query_nodes(&pid_clone2, Some("file"), None, None, 5000)
                .unwrap_or_default();
            let total_files = file_nodes.len();
            for f in &file_nodes {
                let path_lower = f.file_path.as_str().to_lowercase();
                if path_lower.contains("test")
                    || path_lower.contains("spec")
                    || path_lower.contains("_test.")
                {
                    test_files += 1;
                }
            }
            let func_nodes = graph
                .query_nodes(&pid_clone2, Some("function"), None, None, 2000)
                .unwrap_or_default();
            for func in &func_nodes {
                let incoming = graph
                    .find_incoming_edges(&pid_clone2, None, &func.node_id, 1)
                    .unwrap_or_default();
                if incoming.is_empty() {
                    dead += 1;
                }
            }
            (ntc, ekc, pr, state_usage, dead, test_files, total_files)
        })
        .await
        .unwrap_or_default();

        let mut out = String::with_capacity(6144);
        out.push_str(&format!("Codebase Overview: {}\n", rec.project_name));
        out.push_str(&format!("project_id: {}\n", rec.project_id));
        out.push_str(&format!("project_type: {}\n", rec.project_type));
        out.push_str(&format!("directory: {}\n", rec.directory));
        out.push_str(&format!("active_generation: {}\n", gen_));
        out.push_str(&format!("repo_rules: {}\n", rule_count));
        out.push_str(&format!("chunks_indexed: {}\n", tantivy_docs));
        out.push_str(&format!("vectors_stored: {}\n", lancedb_rows));
        out.push_str(&format!("history_docs: {}\n", history_docs));
        out.push_str(&format!("antipattern_docs: {}\n", antipattern_docs));

        if !lang_counts.is_empty() {
            let mut lang_sorted: Vec<_> = lang_counts.into_iter().collect();
            lang_sorted.sort_by(|a, b| b.1.cmp(&a.1));
            out.push_str("\n--- Language Breakdown (chunks) ---\n");
            for (lang, count) in &lang_sorted {
                let pct = if tantivy_docs > 0 {
                    (*count as f64 / tantivy_docs as f64 * 100.0) as u32
                } else {
                    0
                };
                out.push_str(&format!("  {}: {} ({}%)\n", lang, count, pct));
            }
        }

        if !node_type_counts.is_empty() {
            let mut nts: Vec<_> = node_type_counts.iter().collect();
            nts.sort_by(|a, b| b.1.cmp(a.1));
            let total_nodes: usize = node_type_counts.values().sum();
            out.push_str(&format!("\n--- Symbol Types ({} total) ---\n", total_nodes));
            for (ntype, count) in &nts {
                out.push_str(&format!("  {}: {}\n", ntype, count));
            }
        }

        if !edge_kind_counts.is_empty() {
            let mut eks: Vec<_> = edge_kind_counts.iter().collect();
            eks.sort_by(|a, b| b.1.cmp(a.1));
            let total_edges: usize = edge_kind_counts.values().sum();
            out.push_str(&format!("\n--- Edge Types ({} total) ---\n", total_edges));
            for (ekind, count) in eks.iter().take(15) {
                out.push_str(&format!("  {}: {}\n", ekind, count));
            }
            if eks.len() > 15 {
                out.push_str(&format!("  ... and {} more kinds\n", eks.len() - 15));
            }
        }

        {
            let files = node_type_counts.get("file").copied().unwrap_or(0);
            let classes = node_type_counts.get("class").copied().unwrap_or(0);
            let functions = node_type_counts.get("function").copied().unwrap_or(0);
            let interfaces = node_type_counts.get("interface").copied().unwrap_or(0);
            let db_tables = node_type_counts.get("db_table").copied().unwrap_or(0);
            let web_services = node_type_counts.get("web_service").copied().unwrap_or(0);
            let http_handlers = node_type_counts.get("http_handler").copied().unwrap_or(0);
            let wcf_services = node_type_counts.get("wcf_service").copied().unwrap_or(0);
            let controls = node_type_counts.get("control").copied().unwrap_or(0);
            let ui_containers = node_type_counts.get("ui_container").copied().unwrap_or(0);
            let app_settings = node_type_counts.get("app_setting").copied().unwrap_or(0);
            let conn_strings = node_type_counts
                .get("connection_string")
                .copied()
                .unwrap_or(0);

            out.push_str("\n--- Architecture ---\n");
            if files > 0 {
                out.push_str(&format!("  Source files: {}\n", files));
            }
            if classes > 0 || interfaces > 0 {
                out.push_str(&format!(
                    "  Types: {} classes, {} interfaces\n",
                    classes, interfaces
                ));
            }
            if functions > 0 {
                out.push_str(&format!("  Functions/Methods: {}\n", functions));
            }
            if controls > 0 || ui_containers > 0 {
                out.push_str(&format!(
                    "  UI: {} controls, {} containers\n",
                    controls, ui_containers
                ));
            }
            if web_services + http_handlers + wcf_services > 0 {
                out.push_str(&format!(
                    "  Service endpoints: {} ASMX, {} ASHX, {} WCF\n",
                    web_services, http_handlers, wcf_services
                ));
            }
            if db_tables > 0 {
                out.push_str(&format!("  Database tables: {}\n", db_tables));
            }
            if app_settings > 0 || conn_strings > 0 {
                out.push_str(&format!(
                    "  Config: {} app settings, {} connection strings\n",
                    app_settings, conn_strings
                ));
            }
        }

        out.push_str("\n--- Code Quality ---\n");
        if total_file_count > 0 {
            let test_pct = (test_file_count as f64 / total_file_count as f64 * 100.0) as u32;
            out.push_str(&format!(
                "  Test files: {} / {} ({}%)\n",
                test_file_count, total_file_count, test_pct
            ));
        }
        if dead_code_count > 0 {
            out.push_str(&format!(
                "  Potential dead functions (zero incoming refs): {}\n",
                dead_code_count
            ));
        }
        if antipattern_docs > 0 {
            out.push_str(&format!("  Anti-patterns indexed: {}\n", antipattern_docs));
        }

        if let Some(metrics) = centrality {
            let mut top_nodes: Vec<_> = metrics.pagerank.into_iter().collect();
            top_nodes.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            out.push_str("\n--- Top Central Nodes (PageRank) ---\n");
            for (id, score) in top_nodes.iter().take(10) {
                out.push_str(&format!("  {} ({:.4})\n", id, score));
            }
        }

        let table_nodes = self
            .state
            .graph
            .query_nodes(&pid, Some("db_table"), None, None, 100)
            .unwrap_or_default();
        if !table_nodes.is_empty() {
            out.push_str(&format!(
                "\n--- Database Tables ({}) ---\n",
                table_nodes.len()
            ));
            let names: Vec<_> = table_nodes
                .iter()
                .take(20)
                .map(|n| n.name.as_str())
                .collect();
            out.push_str(&format!("  {}\n", names.join(", ")));
            if table_nodes.len() > 20 {
                out.push_str(&format!("  ... and {} more\n", table_nodes.len() - 20));
            }
        }

        if !state_usage_data.is_empty() {
            out.push_str(&format!(
                "\n--- Global State Keys ({} total) ---\n",
                state_usage_data.len()
            ));
            for (name, reads, writes) in state_usage_data.iter().take(10) {
                out.push_str(&format!(
                    "  {} (reads={}, writes={})\n",
                    name, reads, writes
                ));
            }
            if state_usage_data.len() > 10 {
                out.push_str(&format!("  ... and {} more\n", state_usage_data.len() - 10));
            }
        }

        let couplings =
            engram_graph::algorithms::coupling::top_project_couplings(&self.state.graph, &pid, 5)
                .unwrap_or_default();
        if !couplings.is_empty() {
            out.push_str("\n--- Top Temporal Couplings ---\n");
            for c in couplings {
                out.push_str(&format!(
                    "  {} <-> {} (w={})\n",
                    c.file_node_id, c.neighbor_node_id, c.weight
                ));
            }
        }

        // ── House conventions: auth, settings model, base-class hubs ────────
        // One additional scan; this is what tells a fresh agent HOW this
        // codebase does things, not just what it contains.
        {
            let graph = self.state.graph.clone();
            let pid_conv = pid.clone();
            let (guards, roles, settings_tables, app_settings, top_bases) =
                tokio::task::spawn_blocking(move || {
                    let nodes = graph
                        .query_nodes(&pid_conv, None, None, None, 50_000)
                        .unwrap_or_default();
                    let mut guards: std::collections::HashMap<String, usize> = Default::default();
                    let mut roles: std::collections::HashMap<String, usize> = Default::default();
                    let mut settings_tables: Vec<String> = Vec::new();
                    let mut app_settings = 0usize;
                    let mut base_ids: Vec<(String, String)> = Vec::new();
                    for n in &nodes {
                        if n.node_type == "function" {
                            if let Some(checks) = n
                                .metadata
                                .as_ref()
                                .and_then(|m| m.get("permission_checks"))
                                .and_then(|v| v.as_str())
                            {
                                for g in checks.split(';').filter(|g| !g.is_empty()) {
                                    *guards.entry(g.to_string()).or_default() += 1;
                                }
                            }
                            if let Some(r) = n
                                .metadata
                                .as_ref()
                                .and_then(|m| m.get("guard_roles"))
                                .and_then(|v| v.as_str())
                            {
                                for role in r.split(';').filter(|r| !r.is_empty()) {
                                    *roles.entry(role.to_string()).or_default() += 1;
                                }
                            }
                        } else if n.node_type == "app_setting" {
                            app_settings += 1;
                        } else if n.node_type == "db_table"
                            && crate::handlers::planning_tools::is_settings_table_name(&n.name)
                        {
                            settings_tables.push(n.name.clone());
                        } else if matches!(n.node_type.as_str(), "class" | "interface") {
                            base_ids.push((n.node_id.clone(), n.name.clone()));
                        }
                    }
                    // Most-inherited types: incoming InheritsFrom/Implements.
                    let mut top_bases: Vec<(String, usize)> = Vec::new();
                    for (id, name) in base_ids.iter().take(2000) {
                        let inherit_in = graph
                            .find_incoming_edges(&pid_conv, Some(EdgeKind::InheritsFrom), id, 200)
                            .map(|v| v.len())
                            .unwrap_or(0)
                            + graph
                                .find_incoming_edges(&pid_conv, Some(EdgeKind::Implements), id, 200)
                                .map(|v| v.len())
                                .unwrap_or(0);
                        if inherit_in > 0 {
                            top_bases.push((name.clone(), inherit_in));
                        }
                    }
                    top_bases.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
                    top_bases.truncate(8);
                    (guards, roles, settings_tables, app_settings, top_bases)
                })
                .await
                .unwrap_or_default();

            if !guards.is_empty() || app_settings > 0 || !settings_tables.is_empty() {
                out.push_str("\n--- House Conventions ---\n");
                if !guards.is_empty() {
                    let mut gs: Vec<_> = guards.into_iter().collect();
                    gs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
                    let names: Vec<String> = gs
                        .into_iter()
                        .take(5)
                        .map(|(g, c)| format!("{g} ({c})"))
                        .collect();
                    out.push_str(&format!("  auth guards: {}\n", names.join(", ")));
                }
                if !roles.is_empty() {
                    let mut rs: Vec<_> = roles.into_iter().collect();
                    rs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
                    let names: Vec<String> = rs.into_iter().take(8).map(|(r, _)| r).collect();
                    out.push_str(&format!("  roles: {}\n", names.join(", ")));
                }
                out.push_str(&format!("  app settings (config files): {app_settings}\n"));
                if !settings_tables.is_empty() {
                    out.push_str(&format!(
                        "  settings tables: {}\n",
                        settings_tables.join(", ")
                    ));
                }
                out.push_str(
                    "  details: map_guards_and_settings | start any story with plan_user_story\n",
                );
            }
            if !top_bases.is_empty() {
                out.push_str("\n--- Most-Inherited Types (edit with care — every derived class is blast radius) ---\n");
                for (name, c) in &top_bases {
                    out.push_str(&format!("  {name} <- {c} derived/implementing type(s)\n"));
                }
            }
        }

        out.push_str(&self.freshness_footer(&pid, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(
            out.trim().to_string(),
        )]))
    }

    pub async fn handle_get_extraction_confidence(
        &self,
        req: GetExtractionConfidenceRequest,
    ) -> Result<CallToolResult, McpError> {
        let src = &req.source_content;
        let cb = req.codebehind_content.as_deref().unwrap_or("");

        let confidence = match req.extraction_type.as_str() {
            "event_wiring" => {
                let has_inherits = src.contains("Inherits=") || src.contains("Inherits \"");
                let has_codebehind =
                    !cb.is_empty() || src.contains("CodeBehind=") || src.contains("CodeFile=");
                let has_handler = cb.contains("Handles ")
                    || cb.contains("_Click")
                    || cb.contains("_Load")
                    || cb.contains("EventHandler");
                let sig_valid =
                    cb.contains("Sub ") || cb.contains("void ") || cb.contains("Function ");
                let ctrl_explicit = src.contains("ID=\"") || src.contains("id=\"");
                engram_index::score_event_wiring(
                    has_inherits,
                    has_codebehind,
                    has_handler,
                    sig_valid,
                    ctrl_explicit,
                )
            }
            "sql_trace" => {
                let has_conn = src.contains("ConnectionString")
                    || src.contains("connectionString")
                    || src.contains("SqlConnection");
                let has_param = (src.contains("@") && src.contains("Parameters.Add"))
                    || src.contains("SqlParameter")
                    || src.contains("AddWithValue");
                let table_resolved = src.contains("FROM ")
                    || src.contains("INTO ")
                    || src.contains("UPDATE ")
                    || src.contains("JOIN ");
                let col_resolved = src.contains("SELECT ") && !src.contains("SELECT *");
                let sp_verified = src.contains("CommandType.StoredProcedure")
                    || src.contains("EXEC ")
                    || src.contains("sp_");
                engram_index::score_sql_trace(
                    has_conn,
                    has_param,
                    table_resolved,
                    col_resolved,
                    sp_verified,
                )
            }
            "control_binding" => {
                let runat = src.contains("runat=\"server\"") || src.contains("runat=\"Server\"");
                let explicit_id = src.contains("ID=\"") || src.contains("id=\"");
                let designer_field = !cb.is_empty()
                    && (cb.contains("Protected WithEvents") || cb.contains("protected "));
                let cb_ref = !cb.is_empty()
                    && (cb.contains(".Text")
                        || cb.contains(".Value")
                        || cb.contains(".SelectedValue")
                        || cb.contains("FindControl"));
                engram_index::score_control_binding(runat, explicit_id, designer_field, cb_ref)
            }
            other => {
                return Err(McpError::invalid_params(
                    format!(
                        "Unknown extraction_type '{}'. Must be: event_wiring, sql_trace, control_binding",
                        other
                    ),
                    None,
                ));
            }
        };

        match confidence.band {
            engram_index::ConfidenceBand::High => {
                engram_core::metrics().extractions_high_confidence.inc();
            }
            engram_index::ConfidenceBand::Medium => {
                engram_core::metrics().extractions_medium_confidence.inc();
            }
            engram_index::ConfidenceBand::Low => {
                engram_core::metrics().extractions_low_confidence.inc();
            }
        }

        let json = serde_json::to_string_pretty(&confidence)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    pub async fn handle_map_ajax_regions(
        &self,
        req: MapAjaxRegionsRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let rec = self.ensure_project_record(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let file_path = req.file_path.clone();

        let aspx_full = safe_join(std::path::Path::new(&rec.directory), &file_path)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let aspx_content = tokio::fs::read_to_string(&aspx_full).await.map_err(|e| {
            McpError::internal_error(format!("Failed to read {}: {e}", aspx_full.display()), None)
        })?;

        let result = tokio::task::spawn_blocking(move || {
            crate::services::ajax_region_service::analyze_ajax_regions(
                &graph,
                &pid,
                &file_path,
                &aspx_content,
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // Before: the handler returned a bare `"AJAX regions for <path>"`
        // string and discarded the entire `AjaxRegionMap` struct, so
        // callers got nothing useful out of the tool. Render the full
        // structured inventory via the service's existing formatter.
        let rendered = crate::services::ajax_region_service::format_ajax_region_map(&result);
        Ok(CallToolResult::success(vec![Content::text(rendered)]))
    }

    /// Persist analyzed business logic into the searchable `business_logic`
    /// namespace. Without this write, `query_business_logic` searches an
    /// empty namespace forever — the original Phase-36 wiring rendered the
    /// analysis to the caller and dropped it on the floor.
    ///
    /// Doc identity is PATH-stable (derived from the synthetic path, not the
    /// content), so re-analyzing a changed method overwrites its previous
    /// doc instead of accumulating stale duplicates. The namespace is
    /// GlobalMutable (generation 0, no generation filter at query time).
    async fn persist_business_logic(
        &self,
        project_id: &str,
        methods: &[crate::services::business_logic_service::MethodBusinessLogic],
    ) -> Result<usize, McpError> {
        use engram_core::{ContentHash, DocIdStr, RelPath};

        if methods.is_empty() {
            return Ok(0);
        }
        let ps = self.ensure_project_runtime(project_id).await?;
        let namespace = engram_core::namespaces::NAMESPACE_BUSINESS_LOGIC;

        let mut docs: Vec<engram_index::IndexDoc> = Vec::with_capacity(methods.len());
        for m in methods {
            // Skip empty analyses (LLM unavailable/failed) — a doc with no
            // purpose and no rules only pollutes retrieval.
            if m.purpose.is_empty() && m.business_rules.is_empty() {
                continue;
            }
            let mut content = crate::services::business_logic_service::render_method_as_doc(m);
            content.push_str(&format!("\n_Source: {}_\n", m.file_path));

            let synthetic_path = format!(
                "__business_logic/{}/{}.md",
                m.file_path.replace('\\', "/"),
                m.method_name
            );
            // Path-stable identity: doc_id/chunk_id derive from the path so
            // updated analyses replace (pk delete-then-add), never duplicate.
            let path_hash = ContentHash::compute(synthetic_path.as_bytes());
            let doc_id = DocIdStr::compute(&synthetic_path, 0, 0, &path_hash);
            let chunk_id = {
                let h = blake3::hash(synthetic_path.as_bytes());
                let mut b = [0u8; 8];
                b.copy_from_slice(&h.as_bytes()[..8]);
                u64::from_le_bytes(b)
            };
            let content_hash = ContentHash::compute(content.as_bytes());

            docs.push(engram_index::IndexDoc {
                generation: 0,
                chunk_id,
                path: RelPath::new(&synthetic_path),
                language: "markdown".into(),
                content,
                namespace: namespace.into(),
                author: None,
                timestamp: None,
                start_line: 0,
                end_line: 0,
                doc_id: doc_id.0,
                content_hash: content_hash.0,
            });
        }
        if docs.is_empty() {
            return Ok(0);
        }
        let n = docs.len();
        ps.search
            .index_docs(
                project_id,
                &docs,
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(n)
    }

    pub async fn handle_analyze_business_logic(
        &self,
        p: crate::models::requests::AnalyzeBusinessLogicRequest,
    ) -> Result<CallToolResult, McpError> {
        let rec = self.ensure_project_record(&p.project_id).await?;
        let project_dir = rec.directory.clone();
        let dreaming = self.state.dreaming.as_ref();

        let cached_hashes: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();

        if p.method_name.is_some() && p.file_path.is_none() {
            return Ok(CallToolResult::success(vec![Content::text(
                "Error: `method_name` requires `file_path` to be specified. \
                 Provide both to analyze a specific method, or just `file_path` for all methods in a file.",
            )]));
        }

        if let Some(file_path) = &p.file_path {
            let full_path = safe_join(std::path::Path::new(&project_dir), file_path)
                .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
            let content = std::fs::read_to_string(&full_path)
                .map_err(|e| McpError::invalid_params(format!("cannot read file: {e}"), None))?;

            if let Some(method_name) = &p.method_name {
                let language =
                    crate::services::business_logic_service::detect_language(file_path, &content);
                let class_name =
                    crate::services::business_logic_service::detect_class_name(&content);
                let body_opt = if language == "vb" {
                    crate::services::full_project_migration_service::extract_vb_method_body(
                        &content,
                        method_name,
                    )
                } else {
                    crate::services::full_project_migration_service::extract_cs_method_body(
                        &content,
                        method_name,
                    )
                };

                let Some((body, start, _end, _lines)) = body_opt else {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Method '{method_name}' not found in {file_path}"
                    ))]));
                };

                let result = crate::services::business_logic_service::analyze_method_logic(
                    dreaming,
                    file_path,
                    method_name,
                    &body,
                    &class_name,
                    language,
                    start as u32,
                )
                .await;

                let persisted = self
                    .persist_business_logic(&p.project_id, std::slice::from_ref(&result))
                    .await?;

                if p.output_json {
                    let json = serde_json::to_string_pretty(&result)
                        .unwrap_or_else(|e| format!("JSON error: {e}"));
                    return Ok(CallToolResult::success(vec![Content::text(json)]));
                }
                let mut md = crate::services::business_logic_service::render_method_as_doc(&result);
                md.push_str(&format!(
                    "\n_{persisted} doc(s) persisted to the business_logic namespace — retrieve later with query_business_logic._\n"
                ));
                return Ok(CallToolResult::success(vec![Content::text(md)]));
            }

            // File-level mode
            let (file_logic, analyzed, skipped) =
                crate::services::business_logic_service::analyze_file_logic(
                    dreaming,
                    file_path,
                    &content,
                    &cached_hashes,
                )
                .await;

            let persisted = self
                .persist_business_logic(&p.project_id, &file_logic.methods)
                .await?;

            if p.output_json {
                let json = serde_json::to_string_pretty(&file_logic)
                    .unwrap_or_else(|e| format!("JSON error: {e}"));
                return Ok(CallToolResult::success(vec![Content::text(json)]));
            }

            let mut md = format!(
                "# Business Logic — {}\n\n*{}*\n\n- Methods analyzed: {analyzed}\n- Cached (skipped): {skipped}\n- Persisted to business_logic namespace: {persisted}\n\n",
                file_logic.class_name, file_logic.file_purpose
            );
            for m in &file_logic.methods {
                md.push_str(&crate::services::business_logic_service::render_method_as_doc(m));
                md.push_str("---\n\n");
            }
            return Ok(CallToolResult::success(vec![Content::text(md)]));
        }

        // Full project mode
        let code_paths = crate::utils::files::discover_files_recursive(
            std::path::Path::new(&project_dir),
            &[".aspx.vb", ".aspx.cs", ".ascx.vb", ".ascx.cs", ".vb", ".cs"],
            500,
        )
        .await;

        let code_files: Vec<(String, String)> = code_paths
            .into_iter()
            .filter_map(|rel| {
                let full = safe_join(std::path::Path::new(&project_dir), &rel).ok()?;
                std::fs::read_to_string(&full).ok().map(|c| (rel, c))
            })
            .collect();

        let code_refs: Vec<(&str, &str)> = code_files
            .iter()
            .map(|(p, c)| (p.as_str(), c.as_str()))
            .collect();

        let report = crate::services::business_logic_service::analyze_project_logic(
            dreaming,
            &p.project_id,
            &code_refs,
            &cached_hashes,
            p.max_concurrent,
        )
        .await;

        let all_methods: Vec<crate::services::business_logic_service::MethodBusinessLogic> = report
            .file_summaries
            .iter()
            .flat_map(|f| f.methods.iter().cloned())
            .collect();
        let persisted = self
            .persist_business_logic(&p.project_id, &all_methods)
            .await?;

        if p.output_json {
            let json = serde_json::to_string_pretty(&report)
                .unwrap_or_else(|e| format!("JSON error: {e}"));
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let mut md = crate::services::business_logic_service::render_compact_markdown(&report);
        md.push_str(&format!(
            "\n_{persisted} method doc(s) persisted to the business_logic namespace — query with query_business_logic._\n"
        ));
        Ok(CallToolResult::success(vec![Content::text(md)]))
    }

    pub async fn handle_query_business_logic(
        &self,
        p: QueryBusinessLogicRequest,
    ) -> Result<CallToolResult, McpError> {
        let ps = self.ensure_project_runtime(&p.project_id).await?;
        let gen_ = self.get_active_generation(&p.project_id).await?;

        let query = HybridQuery {
            text: p.query.clone(),
            project_id: p.project_id.clone(),
            namespace: "business_logic".to_string(),
            generation: gen_,
            top_k: p.sanitized_top_k(), // MCP1: clamp to MAX_SEARCH_RESULTS
            fts_mode: "loose".to_string(),
            include_path_prefixes: None,
            exclude_path_prefixes: None,
            language_filters: None,
            author_filter: None,
            date_after: None,
            date_before: None,
            use_mmr: false,
        };

        let hits = ps
            .search
            .search(&query, None, &tokio_util::sync::CancellationToken::new())
            .await
            .unwrap_or_default();

        // Render the actual rules — the original implementation returned the
        // literal string "Hits: N" and threw the content away, which made
        // this tool useless to the agent regardless of extraction quality.
        if hits.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "result: no_hits in the business_logic namespace.\n\
                 hints: this namespace is only populated by analyze_business_logic — run it \
                 first (file mode for one page, project mode for everything). If analysis was \
                 already run, retry with broader domain terms.",
            )]));
        }

        let mut out = format!("# Business-logic matches for '{}'\n", p.query);
        for (i, h) in hits.iter().enumerate() {
            out.push_str(&format!(
                "\n## #{} {} (score {:.3})\n\n",
                i + 1,
                h.path,
                h.score
            ));
            // Business-logic docs are small (~1 KB rendered markdown) —
            // include the full stored document, not just a snippet.
            match ps.search.get_doc_by_doc_id(
                &p.project_id,
                engram_core::namespaces::NAMESPACE_BUSINESS_LOGIC,
                0, // GlobalMutable namespace stores at generation 0
                &h.doc_id,
            ) {
                Ok(Some((_, _, content, _, _))) => out.push_str(&content),
                _ => {
                    if let Some(sn) = &h.snippet {
                        out.push_str(sn);
                        out.push('\n');
                    }
                }
            }
        }
        out.push_str(&self.freshness_footer(&p.project_id, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_dream_project(
        &self,
        req: DreamProjectRequest,
    ) -> Result<CallToolResult, McpError> {
        let pid = req.project_id.clone();
        let _ = self.ensure_project_record(&pid).await?;

        let min_edge_weight = req.sanitized_min_edge_weight();
        let min_cluster_size = req.sanitized_min_cluster_size();
        let max_clusters = req.sanitized_max_clusters();

        if req.wait {
            let timeout_dur = std::time::Duration::from_secs(req.sanitized_timeout_secs());

            let result = tokio::time::timeout(
                timeout_dur,
                crate::actors::dreamer::dream_once(
                    &self.state,
                    &pid,
                    min_edge_weight,
                    min_cluster_size,
                    max_clusters,
                ),
            )
            .await;

            return match result {
                Ok(Ok(insights)) => Ok(CallToolResult::success(vec![Content::text(format!(
                    "Dream completed for project_id: {pid}\n\
                     insights_generated: {insights}\n\
                     parameters: max_clusters={max_clusters}, \
                     min_edge_weight={min_edge_weight}, \
                     min_cluster_size={min_cluster_size}"
                ))])),
                Ok(Err(e)) => Err(McpError::internal_error(e.to_string(), None)),
                Err(_) => Ok(CallToolResult::success(vec![Content::text(format!(
                    "Dream timed out after {}s for project_id: {pid}. \
                     Try increasing timeout_secs or reducing max_clusters.",
                    req.sanitized_timeout_secs()
                ))])),
            };
        }

        if let Err(e) = self.state.events_tx.send(AppEvent::TriggerDream {
            project_id: pid.clone(),
        }) {
            tracing::warn!("Failed to send TriggerDream event: {e}");
        }

        Ok(CallToolResult::success(vec![Content::text(format!(
            "🟡 Dream cycle triggered for project_id: {pid}. Use list_jobs to check status."
        ))]))
    }

    pub async fn handle_analyze_file_coding_style(
        &self,
        req: crate::models::AnalyzeFileCodingStyleRequest,
    ) -> Result<CallToolResult, McpError> {
        let ps = self.ensure_project_runtime(&req.project_id).await?;

        // MCP1: use safe_join to prevent path traversal; absolute paths and ".." rejected.
        let abs_path = safe_join(std::path::Path::new(&ps.info.directory), &req.file_path)
            .map_err(|e| McpError::invalid_request(e.to_string(), None))?;

        let resolved = self
            .state
            .paths
            .resolve_path(&abs_path)
            .map_err(|e| McpError::invalid_request(e.to_string(), None))?;

        let latest_oid = tokio::task::spawn_blocking({
            let repo_path = PathBuf::from(&ps.info.directory);
            move || -> anyhow::Result<String> {
                use engram_git::GitWalker;
                let repo = GitWalker::open_repo(&repo_path)?;
                let head = repo.head()?.peel_to_commit()?;
                Ok(head.id().to_string())
            }
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let cache_subject = if let Some(rel) =
            engram_core::RelPath::from_relative(std::path::Path::new(&ps.info.directory), &resolved)
        {
            rel.as_str().to_string()
        } else {
            req.file_path.clone()
        };
        let cache_key = format!("style_guide:{}:{}", cache_subject, latest_oid);

        if let Some(mut cached) = self
            .state
            .registry
            .get_meta(&req.project_id, &cache_key)
            .ok()
            .flatten()
        {
            if !cached.contains("(cached)") {
                cached.push_str("\n(cached)");
            }
            return Ok(CallToolResult::success(vec![Content::text(cached)]));
        }

        let diff_limit = req.sanitized_diff_limit();
        let result = self
            .cognitive_analyze_file_style(&req.project_id, &req.file_path, diff_limit)
            .await;

        if let Some(err) = result.error {
            return Err(McpError::internal_error(err, None));
        }

        let mut out = format!("Style Guide for {}\n\n", req.file_path);
        out.push_str("Confidence: 1.00\n\n");

        if let Some(guide) = result.style_guide {
            out.push_str(&guide);
        } else {
            out.push_str("No style patterns detected.");
        }

        out.push_str("\n\n---\n");
        out.push_str(&format!(
            "Analysed {} commits.",
            result.analyzed_commits.len()
        ));

        let _ = self
            .state
            .registry
            .set_meta(&req.project_id, &cache_key, &out);

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_immune_check(
        &self,
        req: ImmuneCheckRequest,
    ) -> Result<CallToolResult, McpError> {
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;

        let q = crate::utils::text::code_to_query(&req.code);
        let fts_mode = if req.use_vector { "loose" } else { "strict" };

        let hits = ps
            .search
            .search(
                &HybridQuery {
                    project_id: req.project_id.clone(),
                    namespace: "antipattern".into(),
                    generation: gen_,
                    text: q,
                    top_k: req.sanitized_top_k(),
                    fts_mode: fts_mode.into(),
                    include_path_prefixes: None,
                    exclude_path_prefixes: None,
                    language_filters: None,
                    author_filter: None,
                    date_after: None,
                    date_before: None,
                    use_mmr: false,
                },
                None,
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let warn_t = self
            .state
            .registry
            .get_meta(&req.project_id, "immune_warn_threshold")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<f32>().ok())
            .unwrap_or(0.6); // Default fallback

        // ── Repo-rule cross-reference ───────────────────────────────────────
        //
        // When the caller supplied a `file_path`, check which active repo
        // rules match it. `immune_*`-prefixed rules represent files that
        // were explicitly flagged (typically from a reverted commit); code
        // touching those files gets elevated scrutiny regardless of raw
        // similarity score. A CLEAN verdict on a snippet that deletes rows
        // from a previously-reverted DAL file is a false negative that
        // turns the tool into noise.
        let (is_immune_flagged, immune_rule_ids) = if let Some(ref fp) = req.file_path {
            let rules = self
                .state
                .registry
                .list_repo_rules(&req.project_id)
                .unwrap_or_default();
            let mut matched: Vec<String> = Vec::new();
            for rule in rules {
                if !rule.rule_id.starts_with("immune_") {
                    continue;
                }
                if immune_rule_matches_path(&rule.file_pattern, fp) {
                    matched.push(rule.rule_id);
                }
            }
            (!matched.is_empty(), matched)
        } else {
            (false, Vec::new())
        };

        // ── Destructive-pattern detection ───────────────────────────────────
        //
        // Non-LLM, deterministic scan for operations that are reliably
        // dangerous on a data-access file: bulk deletes, DROP / TRUNCATE,
        // mass-mutation LINQ helpers, `ExecuteNonQuery` with a DELETE /
        // DROP / TRUNCATE literal. When the snippet matches at least one
        // of these AND the target file is immune-flagged, we force at
        // least WARN even if similarity scores are low.
        let destructive_hits = detect_destructive_patterns(&req.code);

        let match_count = hits.len();
        let mut highest_score = 0.0;
        let mut out = format!(
            "# Immune Check Result\n\n**Matches Found**: {}\n\n",
            match_count
        );
        for (i, hit) in hits.iter().enumerate() {
            if hit.score > highest_score {
                highest_score = hit.score;
            }
            out.push_str(&format!(
                "### {}. {} (score: {:.3})\n\n{}\n\n",
                i + 1,
                hit.path,
                hit.score,
                hit.snippet.as_deref().unwrap_or("(no snippet)")
            ));
        }

        // ── Verdict ladder ──────────────────────────────────────────────────
        //
        // Rank is monotonic: 0 = CLEAN, 1 = WARNING, 2 = BLOCKED. Every
        // signal proposes a floor, and the final verdict is the max of all
        // floors. That way similarity, match-count, repo-rule, and
        // destructive-pattern signals compose additively rather than
        // letting a low raw score silently override everything else.
        let mut verdict_rank: u8 = if highest_score > 0.8 {
            2
        } else if highest_score > warn_t {
            1
        } else {
            0
        };

        // Match-count escalation: 3+ matches → WARN (compounding).
        const MATCH_COUNT_WARN_THRESHOLD: usize = 3;
        let match_count_warn = match_count >= MATCH_COUNT_WARN_THRESHOLD;
        if match_count_warn {
            verdict_rank = verdict_rank.max(1);
        }

        // Immune + any signal at all → WARN.
        let immune_any_signal =
            is_immune_flagged && (match_count > 0 || !destructive_hits.is_empty());
        if immune_any_signal {
            verdict_rank = verdict_rank.max(1);
        }

        // Immune + destructive + a match → BLOCKED. A revert-flagged file
        // with destructive code AND anti-pattern evidence is textbook
        // "do not apply".
        if is_immune_flagged && !destructive_hits.is_empty() && match_count > 0 {
            verdict_rank = verdict_rank.max(2);
        }

        let status = match verdict_rank {
            2 => "🔴 BLOCKED",
            1 => "🟡 WARNING",
            _ => "🟢 CLEAN",
        };

        // Surface the escalation reasoning so callers see WHY the verdict
        // landed where it did, not just the bare label.
        if is_immune_flagged || match_count_warn || !destructive_hits.is_empty() {
            out.push_str("## Escalation Signals\n\n");
            out.push_str(&format!(
                "- highest similarity score: {:.3} (warn threshold: {:.3})\n",
                highest_score, warn_t
            ));
            out.push_str(&format!(
                "- match count: {} (warn threshold: {})\n",
                match_count, MATCH_COUNT_WARN_THRESHOLD
            ));
            if is_immune_flagged {
                out.push_str(&format!(
                    "- target file `{}` is immune-flagged by: {}\n",
                    req.file_path.as_deref().unwrap_or(""),
                    immune_rule_ids.join(", ")
                ));
            }
            if !destructive_hits.is_empty() {
                out.push_str(&format!(
                    "- destructive patterns detected: {}\n",
                    destructive_hits.join(", ")
                ));
            }
            out.push('\n');
        }

        out.push_str(&format!("## Final Status: {}\n", status));
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_anti_pattern_guard(
        &self,
        req: AntiPatternGuardRequest,
    ) -> Result<CallToolResult, McpError> {
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;

        let ns_counts = ps
            .search
            .count_docs_by_namespace(&req.project_id)
            .unwrap_or_default();
        let ap_count = ns_counts.get("antipattern").copied().unwrap_or(0);

        if ap_count == 0 {
            return Ok(CallToolResult::success(vec![Content::text(
                "verdict: PASS\n\
                 note: No anti-patterns indexed for this project. \
                 Run analyze_reverts first to populate the anti-pattern index.",
            )]));
        }

        let q = crate::utils::text::code_to_query(&req.code);
        let fts_mode = if req.use_vector { "loose" } else { "strict" };

        let hits = ps
            .search
            .search(
                &HybridQuery {
                    project_id: req.project_id.clone(),
                    namespace: "antipattern".into(),
                    generation: gen_,
                    text: q,
                    top_k: req.sanitized_limit(),
                    fts_mode: fts_mode.into(),
                    include_path_prefixes: None,
                    exclude_path_prefixes: None,
                    language_filters: None,
                    author_filter: None,
                    date_after: None,
                    date_before: None,
                    use_mmr: false,
                },
                None,
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let highest_score = hits.first().map(|h| h.score).unwrap_or(0.0);
        // FTS scores (BM25) are much lower than vector scores; use separate thresholds.
        let (block_t, warn_t) = if req.use_vector {
            (0.85, 0.65)
        } else {
            (0.05, 0.005)
        };
        let verdict = if highest_score > block_t {
            "BLOCK"
        } else if highest_score > warn_t {
            "WARN"
        } else {
            "PASS"
        };

        let mut out = format!("verdict: {}\nscore: {:.3}\n\n", verdict, highest_score);
        if !hits.is_empty() {
            out.push_str("Matches in anti-pattern index:\n");
            for h in hits.iter().take(3) {
                out.push_str(&format!("- {} (score: {:.3})\n", h.path, h.score));
                if req.include_content
                    && let Some(ref snippet) = h.snippet
                {
                    // Skip diff headers and show up to 5 content lines
                    let content_lines: Vec<&str> = snippet
                        .lines()
                        .filter(|l| {
                            !l.starts_with("diff ")
                                && !l.starts_with("index ")
                                && !l.starts_with("---")
                                && !l.starts_with("+++")
                                && !l.starts_with("@@")
                        })
                        .take(5)
                        .collect();
                    if !content_lines.is_empty() {
                        out.push_str(&format!("  snippet: {}\n", content_lines.join(" | ")));
                    }
                }
            }
            out.push('\n');
            out.push_str("This pattern was previously reverted as risky code.\n");
            out.push_str(
                "Consider an alternative implementation that avoids the flagged construct.\n",
            );
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_ast_dependency_graph(
        &self,
        req: AstDependencyGraphRequest,
    ) -> Result<CallToolResult, McpError> {
        let max_depth = req.sanitized_max_depth();
        let output_json = req.output_json;
        let compile_time_only = req.compile_time_only;
        let direction = req.direction; // Direction enum — no string conversion needed
        let graph = self.state.graph.clone();
        let project_id = req.project_id.clone();
        let entry_raw = req.entry.clone();

        let out = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let entry_node_id = if graph
                .get_node(&project_id, &entry_raw)
                .map_err(|e| e.to_string())?
                .is_some()
            {
                entry_raw.clone()
            } else {
                match graph
                    .resolve_symbol(&project_id, &entry_raw, None, None)
                    .map_err(|e| e.to_string())?
                {
                    ResolveResult::Unique(node) => node.node_id,
                    ResolveResult::Ambiguous(candidates) => {
                        return Err(format_ambiguous_symbol_error(&entry_raw, &candidates));
                    }
                    ResolveResult::NotFound => {
                        return Err(format!(
                            "No node found matching '{}'. Use query_graph_nodes to discover valid names/node_ids, then retry.",
                            entry_raw
                        ));
                    }
                }
            };

            let edge_kinds: Vec<EdgeKind> = if compile_time_only {
                vec![EdgeKind::Dependency, EdgeKind::Imports, EdgeKind::Contains]
            } else {
                EdgeKind::ALL.to_vec()
            };

            // Direction enum: exhaustive match — no silent fallback possible.
            let graph_direction = direction.as_str();

            let mut traversal = graph
                .traverse(
                    &project_id,
                    &entry_node_id,
                    max_depth,
                    Some(edge_kinds),
                    graph_direction,
                )
                .map_err(|e| e.to_string())?;

            // Fallback: when the ADJ_OUT-driven traversal returns only
            // the root node, scan the raw EDGES table for any edge
            // sourced at `entry_node_id` and surface its targets as
            // depth-1 neighbours. This catches the case where an edge
            // lives in EDGES but isn't indexed into ADJ_OUT — a class
            // of bug we've seen for Contains and event_wiring edges
            // on specific node shapes — and also helps for VB LINQ
            // method chains that the extractor records as raw edges
            // without going through the full adjacency pipeline.
            //
            // Only fires for outgoing / both directions; "in" is not
            // affected because the incoming traversal uses ADJ_IN.
            let mut fallback_used = false;
            if traversal.len() <= 1 && (graph_direction == "out" || graph_direction == "both") {
                let direct_targets: Vec<engram_graph::Node> = graph
                    .list_edges(&project_id, None)
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .filter(|e| e.source_id == entry_node_id)
                    .filter_map(|e| {
                        graph
                            .get_node(&project_id, &e.target_id)
                            .ok()
                            .flatten()
                    })
                    .collect();
                if !direct_targets.is_empty() {
                    fallback_used = true;
                    for n in direct_targets {
                        // De-dup against the root in case some edge
                        // points back to the entry itself.
                        if n.node_id != entry_node_id {
                            traversal.push((n, 1));
                        }
                    }
                }
            }

            if output_json {
                let nodes_json: Vec<serde_json::Value> = traversal
                    .iter()
                    .map(|(node, depth)| {
                        serde_json::json!({
                            "node_id": node.node_id,
                            "name": node.name,
                            "node_type": node.node_type,
                            "file_path": node.file_path.as_str(),
                            "depth": depth,
                        })
                    })
                    .collect();
                serde_json::to_string_pretty(&serde_json::json!({
                    "entry_node": entry_node_id,
                    "direction": graph_direction,
                    "max_depth": max_depth,
                    "compile_time_only": compile_time_only,
                    "nodes": nodes_json,
                    "total_nodes": traversal.len(),
                    "fallback_used": fallback_used,
                }))
                .map_err(|e| e.to_string())
            } else {
                let mut tree = format!("# AST Dependency Tree: {}\n\n", entry_node_id);
                let traversal_len = traversal.len();
                for (node, depth) in traversal {
                    let indent = "  ".repeat(depth);
                    tree.push_str(&format!(
                        "{indent}- {} [{}] ({})\n",
                        node.name,
                        node.node_type,
                        node.file_path.as_str()
                    ));
                }
                if fallback_used {
                    tree.push_str(
                        "\n_Note: depth-1 neighbours below the root came from a raw-EDGES \
                         fallback scan because the ADJ_OUT-indexed traversal returned only \
                         the root. These edges exist in EDGES but aren't populated in \
                         ADJ_OUT — typically because they were written by an extractor \
                         path that doesn't maintain the adjacency index._\n",
                    );
                } else if traversal_len <= 1 {
                    tree.push_str(
                        "\n⚠️ No outgoing dependencies found. This may indicate:\n  \
                         - VB.NET LINQ method chains are not fully resolved to graph edges\n  \
                         - The symbol genuinely has no outgoing dependencies in this direction\n\
                         \n\
                         Try alternative tools:\n  \
                         - `trace_data_flow` for deeper data-access tracing\n  \
                         - `find_symbol_references` (or `direction: \"in\"` here) for \
                         incoming references\n  \
                         - `get_method_info` to inspect the symbol's body directly\n",
                    );
                }
                Ok(tree)
            }
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e: String| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_evaluate_safety(
        &self,
        req: crate::models::EvaluateSafetyRequest,
    ) -> Result<CallToolResult, McpError> {
        let eval_req = crate::services::safety_service::SafetyEvalRequest {
            project_id: req.project_id,
            affected_files: req.affected_files,
            refactor_type: req.refactor_type,
            impact_node_count: req.impact_node_count,
            impact_confidence: req.impact_confidence,
            test_coverage: req.test_coverage,
            anti_pattern_clear: req.anti_pattern_clear,
            downstream_dependents: req.downstream_dependents,
            touches_global_state: req.touches_global_state,
            touches_database: req.touches_database,
        };

        let decision = crate::services::safety_service::evaluate_safety(
            &eval_req,
            self.state.cfg.safety_policy_enabled,
            self.state.cfg.safety_min_confidence,
            self.state.cfg.safety_min_coverage,
        );

        let json = serde_json::to_string_pretty(&decision)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    pub async fn handle_dedicated_antipattern_index(
        &self,
        req: crate::models::AntipatternIndexRequest,
    ) -> Result<CallToolResult, McpError> {
        let limit = req.sanitized_limit();
        let pid = req.project_id;
        let action = req.action.to_lowercase();
        let query = req.query;
        let file_filter = req.file_filter;
        let ps = self.ensure_project_runtime(&pid).await?;
        let gen_ = self.get_active_generation(&pid).await.unwrap_or(1);

        match action.as_str() {
            "stats" => {
                let ns_counts = ps.search.count_docs_by_namespace(&pid).unwrap_or_default();
                let antipattern_docs = ns_counts.get("antipattern").copied().unwrap_or(0);

                let reg = self.state.registry.clone();
                let pid_r = pid.clone();
                let rules = tokio::task::spawn_blocking(move || reg.list_repo_rules(&pid_r))
                    .await
                    .ok()
                    .and_then(|r| r.ok())
                    .unwrap_or_default();

                let mut out = String::with_capacity(512);
                out.push_str(&format!("Anti-Pattern Index Stats: {}\n", pid));
                out.push_str(&format!("indexed_antipattern_docs: {}\n", antipattern_docs));
                out.push_str(&format!("repo_rules: {}\n", rules.len()));
                if !rules.is_empty() {
                    out.push_str("\n--- Repo Rules ---\n");
                    for r in rules.iter().take(20) {
                        out.push_str(&format!(
                            "  [{}] {} (priority={})\n",
                            r.rule_id, r.file_pattern, r.priority
                        ));
                    }
                    if rules.len() > 20 {
                        out.push_str(&format!("  ... and {} more\n", rules.len() - 20));
                    }
                }

                Ok(CallToolResult::success(vec![Content::text(
                    out.trim().to_string(),
                )]))
            }
            "list" | "search" => {
                let include_path_prefixes = file_filter.map(|f| vec![f]);
                let q_text = query.unwrap_or_else(|| "*".to_string());

                let hits = ps
                    .search
                    .search(
                        &engram_index::HybridQuery {
                            project_id: pid.clone(),
                            namespace: "antipattern".into(),
                            generation: gen_,
                            text: q_text,
                            top_k: limit,
                            fts_mode: "loose".into(),
                            include_path_prefixes,
                            exclude_path_prefixes: None,
                            language_filters: None,
                            author_filter: None,
                            date_after: None,
                            date_before: None,
                            use_mmr: false,
                        },
                        None,
                        &tokio_util::sync::CancellationToken::new(),
                    )
                    .await
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;

                if hits.is_empty() {
                    return Ok(CallToolResult::success(vec![Content::text(
                        "No matches found in anti-pattern index.",
                    )]));
                }

                let mut out = format!("# Anti-Pattern Matches ({})\n\n", hits.len());
                for hit in hits {
                    out.push_str(&format!("- **{}** (score: {:.3})\n", hit.path, hit.score));
                    if let Some(snippet) = hit.snippet {
                        out.push_str(&format!(
                            "  ```\n  {}\n  ```\n",
                            snippet.replace('\n', "\n  ")
                        ));
                    }
                }
                Ok(CallToolResult::success(vec![Content::text(out)]))
            }
            "clear" => {
                ps.search
                    .purge_old_generations(&pid, u64::MAX)
                    .await
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "\u{2705} Anti-pattern index cleared for project '{}'.",
                    pid
                ))]))
            }
            _ => Err(McpError::invalid_params(
                format!("Unknown action '{action}'"),
                None,
            )),
        }
    }

    pub async fn handle_compute_blast_radius(
        &self,
        req: ComputeBlastRadiusRequest,
    ) -> Result<CallToolResult, McpError> {
        let target_id = if let Some(ref fp) = req.file_path {
            let graph = self.state.graph.clone();
            let pid = req.project_id.clone();
            let file_node_id = format!("file:{fp}");
            let file_node_id_check = file_node_id.clone();
            let exists =
                tokio::task::spawn_blocking(move || graph.get_node(&pid, &file_node_id_check))
                    .await
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?
                    .is_some();
            if !exists {
                return Err(McpError::invalid_params(
                    format!(
                        "File path '{}' could not be resolved to '{}'. Use query_graph_nodes to locate the exact file node_id/path before retrying.",
                        fp, file_node_id
                    ),
                    None,
                ));
            }
            file_node_id
        } else if let Some(ref fqn) = req.symbol_fqn {
            let graph = self.state.graph.clone();
            let pid = req.project_id.clone();
            let fqn_c = fqn.clone();
            let found = tokio::task::spawn_blocking(move || {
                graph
                    .resolve_symbol(&pid, &fqn_c, None, None)
                    .map_err(|e| e.to_string())
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e, None))?;

            match found {
                ResolveResult::Unique(node) => node.node_id,
                ResolveResult::Ambiguous(candidates) => {
                    return Err(McpError::invalid_params(
                        format_ambiguous_symbol_error(fqn, &candidates),
                        None,
                    ));
                }
                ResolveResult::NotFound => {
                    return Err(McpError::invalid_params(
                        format!(
                            "Could not resolve symbol_fqn '{}'. Use query_graph_nodes to locate the exact symbol or pass a full node_id (for example sym:/file:/table:).",
                            fqn
                        ),
                        None,
                    ));
                }
            }
        } else {
            return Err(McpError::invalid_params(
                "Either file_path or symbol_fqn is required",
                None,
            ));
        };

        let gen_ = self.get_active_generation(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let include_guidance = req.include_guidance;

        type TopDependent = (String, String, u32); // label, kind, weight
        let (report, top_dependents) = tokio::task::spawn_blocking(move || {
            let report = crate::services::blast_radius_service::compute_blast_radius(
                &graph,
                &pid,
                &target_id,
                gen_,
                include_guidance,
            )?;
            // A score without names is unactionable: resolve the heaviest
            // incoming dependents to name (file:line) so the caller knows
            // exactly WHAT breaks first.
            let mut deps: Vec<TopDependent> = graph
                .find_incoming_edges_with_kind(&pid, None, &target_id, 200)
                .unwrap_or_default()
                .into_iter()
                .map(|(src, kind, w)| {
                    let label = graph
                        .get_node(&pid, &src)
                        .ok()
                        .flatten()
                        .map(|n| {
                            if n.file_path.as_str().is_empty() {
                                n.name
                            } else {
                                format!("{} ({}:{})", n.name, n.file_path, n.start_line)
                            }
                        })
                        .unwrap_or(src);
                    (label, kind.as_str().to_string(), w)
                })
                .collect();
            deps.sort_by(|a, b| b.2.cmp(&a.2));
            deps.truncate(10);
            Ok::<_, anyhow::Error>((report, deps))
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let mut out = format!(
            "# Blast Radius Analysis: {}\n\n\
             **Overall Risk**: {}/10 ({})\n\
             **Direct dependents (1-hop, NOT transitive)**: {} (incoming: {}, outgoing: {})\n",
            report.target,
            report.migration_risk,
            report.risk_band,
            report.total_downstream,
            report.total_incoming,
            report.total_outgoing
        );

        let bd = &report.complexity_breakdown;
        out.push_str("\n**Complexity Breakdown**:\n");
        out.push_str(&format!(
            "- Dependency density: {:.1}/10\n",
            bd.dependency_density_score
        ));
        out.push_str(&format!("- SQL risk: {:.1}/10\n", bd.sql_concat_score));
        out.push_str(&format!(
            "- State coupling: {:.1}/10\n",
            bd.state_coupling_score
        ));
        out.push_str(&format!(
            "- Event wiring: {:.1}/10\n",
            bd.handles_clause_score
        ));
        out.push_str(&format!(
            "- PageRank centrality: {:.1}/10\n",
            bd.pagerank_score
        ));
        out.push_str(&format!(
            "- GIS coupling: {:.1}/10\n",
            bd.gis_coupling_score
        ));
        out.push_str(&format!(
            "- Script injection: {:.1}/10\n",
            bd.script_injection_score
        ));

        if !top_dependents.is_empty() {
            out.push_str("\n## Top incoming dependents (what breaks first)\n");
            for (label, kind, w) in &top_dependents {
                out.push_str(&format!("- {label} [{kind}] (weight {w})\n"));
            }
        }

        if !report.seam_candidates.is_empty() {
            out.push_str("\n## Seam candidates (natural boundaries for splitting the change)\n");
            for s in report.seam_candidates.iter().take(8) {
                out.push_str(&format!(
                    "- {} ({}) — {} [crossing: {}]\n",
                    s.node_id,
                    s.node_type,
                    s.reason,
                    s.edge_kinds_crossing.join(", ")
                ));
            }
        }

        if !report.guidance.is_empty() {
            out.push_str("\n## Migration Guidance\n");
            for g in &report.guidance {
                out.push_str(&format!(
                    "- **[{}]** {}: {}\n",
                    g.severity, g.concern, g.recommendation
                ));
            }
        }

        out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    /// Generate `CLAUDE.md` + `.claude/rules/*.md` from the project's
    /// indexed graph. Language-agnostic: every section is driven by what
    /// the graph actually contains, so a Rust CLI gets language
    /// conventions + danger zones while a WebForms project gets the
    /// full treatment (state, db, auth, frontend).
    pub async fn handle_produce_claude_md(
        &self,
        req: crate::models::ProduceClaudeMdRequest,
    ) -> Result<CallToolResult, McpError> {
        use crate::services::produce_claude_md_service as svc;

        validate_project_id(&req.project_id)?;
        let rec = self.ensure_project_record(&req.project_id).await?;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let active_gen = self.get_active_generation(&pid).await.unwrap_or(1);

        // PERF: per-section wall-clock so slow regens name their culprit.
        let mut section_clock = std::time::Instant::now();
        let mut prev_section = "setup";
        // ── 1. Language breakdown ────────────────────────────────────
        tracing::info!(
            "produce_claude_md section [{}] took {:?}",
            prev_section,
            section_clock.elapsed()
        );
        section_clock = std::time::Instant::now();
        prev_section = "1. Language breakdown";
        let lang_counts = ps.search.count_docs_by_language(&pid).unwrap_or_default();
        let total_files: usize = lang_counts.values().copied().sum();
        let mut languages: Vec<svc::LanguageShare> = lang_counts
            .iter()
            .map(|(k, v)| svc::LanguageShare {
                language: k.clone(),
                file_count: *v,
                share_percent: if total_files == 0 {
                    0.0
                } else {
                    (*v as f32 / total_files as f32) * 100.0
                },
            })
            .collect();
        languages.sort_by(|a, b| b.file_count.cmp(&a.file_count));

        // ── 2. Role description (auto from languages + framework hint) ──
        // Role: graph-derived shape + multitenant / framework
        // quirks, prepended by the README blurb when present. The
        // README blurb almost always beats whatever we synthesise
        // because it's the project maintainer's own description.
        let mut role = build_role_description(&languages, &graph, &pid);
        if let Some(blurb) = read_readme_blurb(&rec.directory) {
            role = format!("{blurb} — {role}");
        }

        // ── 3. Repo rules (immune + anti-pattern) → critical rules ────
        tracing::info!(
            "produce_claude_md section [{}] took {:?}",
            prev_section,
            section_clock.elapsed()
        );
        section_clock = std::time::Instant::now();
        prev_section = "3. Repo rules (immune + anti-pattern) → critical";
        let registry = self.state.registry.clone();
        let pid_clone = pid.clone();
        let repo_rules = tokio::task::spawn_blocking(move || registry.list_repo_rules(&pid_clone))
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or_default();
        // Feed the raw repo-rule set through the rules pipeline
        // (noise filter → keyword meta-clustering → rule-text
        // templating → render-threshold split). Pipeline collapses
        // 30+ token-level CodeRabbit clusters into ~8 semantic
        // meta-rules and strips the LLM-rationalisation essays out
        // of immune rule text. Everything deterministic; no LLM
        // calls in this path.
        use crate::services::produce_claude_md_service::rules_pipeline as pipeline;
        let raw_for_pipeline: Vec<pipeline::RawRule> = repo_rules
            .iter()
            .filter_map(|r| {
                let source = if r.rule_id.starts_with("immune_") {
                    svc::RuleSource::Immune
                } else if r.rule_id.starts_with("cr_") {
                    svc::RuleSource::CodeRabbit
                } else {
                    svc::RuleSource::RepoRule
                };
                // Apply immune-rule cleanup at the entry point so
                // the pipeline sees crisp text instead of
                // three-paragraph essays. Returns None when the
                // immune rule is process-hygiene noise — those
                // entries disappear here.
                let text = if matches!(source, svc::RuleSource::Immune) {
                    match pipeline::render_immune_rule_text(
                        &r.rule_text,
                        &r.rule_id,
                        &r.file_pattern,
                    ) {
                        Some(t) => t,
                        None => return None,
                    }
                } else {
                    rule_text_from_repo_rule(r)
                };
                // Parse CodeRabbit aggregate stats out of the
                // rule's priority / rule_text footer. The ingest
                // writes "… — CodeRabbit pattern, N PRs, M% fix rate"
                // so we can recover the stats with a small regex.
                let (fix_rate, pr_count) = if matches!(source, svc::RuleSource::CodeRabbit) {
                    parse_coderabbit_stats(&r.rule_text)
                } else {
                    (None, None)
                };
                Some(pipeline::RawRule {
                    rule_id: r.rule_id.clone(),
                    file_pattern: r.file_pattern.clone(),
                    rule_text: text,
                    source,
                    fix_rate,
                    pr_count,
                })
            })
            .collect();
        let pipeline_output =
            pipeline::run_pipeline(raw_for_pipeline, pipeline::RenderThreshold::default());
        let mut critical_rules: Vec<svc::CriticalRule> = pipeline_output.root_rules;
        let mut pipeline_summary = pipeline_output.summary.clone();
        let _overflow = pipeline_output.per_language_overflow;
        // Overflow rules are kept for a future change where
        // .claude/rules/ files pick them up; right now they're
        // simply removed from the root's attention budget, which is
        // the primary fix the user asked for.

        // Optional LLM curation pass. Gated by `req.use_llm`. The
        // deterministic pipeline is always the floor; the LLM can
        // only ever make the output better or fall back to the
        // deterministic result. Never a blocker.
        if req.use_llm && !critical_rules.is_empty() {
            use crate::services::produce_claude_md_service::llm_curation as curation;
            let candidates = curation::prepare_candidates(&critical_rules);
            let input = curation::CurationInput {
                project_context: role.clone(),
                candidates,
                max_rules: 8,
            };
            let before = critical_rules.len();
            let curated = curation::curate_with_llm(
                self.state.dreaming.as_ref(),
                self.state.registry.as_ref(),
                &pid,
                input,
                critical_rules.clone(),
            )
            .await;
            // Only accept the curated set when the LLM actually
            // returned something different — `curate_with_llm`
            // returns the deterministic fallback unchanged on any
            // failure, so a same-length identical list signals
            // "no curation happened" and we skip the summary note.
            let curated_changed = curated.len() != before
                || curated
                    .iter()
                    .zip(critical_rules.iter())
                    .any(|(a, b)| a.text != b.text);
            if curated_changed {
                pipeline_summary.push_str(&format!(" → {} after LLM curation", curated.len()));
            }
            critical_rules = curated;
        }

        critical_rules.sort_by_key(|r| match r.source {
            svc::RuleSource::Immune => 0,
            svc::RuleSource::CodeRabbit => 1,
            svc::RuleSource::RepoRule => 2,
            svc::RuleSource::Existing => 3,
        });

        // ── 4. Existing CLAUDE.md — merge priority ────────────────────
        let existing_md = if req.merge_existing {
            read_claude_md_if_any(&rec.directory)
        } else {
            None
        };
        if let Some(ref md) = existing_md {
            let existing_rules = svc::extract_critical_rules_from_existing(md);
            critical_rules = svc::merge_with_existing(critical_rules, existing_rules);
        }

        // ── 5a. Compute PageRank + fetch file nodes ONCE ──────────────
        tracing::info!(
            "produce_claude_md section [{}] took {:?}",
            prev_section,
            section_clock.elapsed()
        );
        section_clock = std::time::Instant::now();
        prev_section = "5a. Compute PageRank + fetch file nodes ONCE";
        //
        // PageRank is the expensive part of this tool (iterative matrix
        // op over the full graph). Both the danger-zones pass and the
        // per-language style pass need a centrality ranking; compute
        // once, pass the HashMap by reference to both.
        //
        // Same story for the file-node list: one `query_nodes` scan
        // covers every language, we filter in-memory per language.
        let graph_for_prep = graph.clone();
        let pid_for_prep = pid.clone();
        let (pagerank_map, file_nodes): (
            std::collections::HashMap<String, f32>,
            Vec<engram_graph::Node>,
        ) = tokio::task::spawn_blocking(move || {
            let pr = engram_graph::analysis::compute_pagerank(
                &graph_for_prep,
                &pid_for_prep,
                active_gen,
            )
            .ok()
            .map(|m| m.pagerank)
            .unwrap_or_default();
            let files = graph_for_prep
                .query_nodes(&pid_for_prep, Some("file"), None, None, 5000)
                .unwrap_or_default();
            (pr, files)
        })
        .await
        .unwrap_or_default();

        // ── 5b. Top-10 file nodes by PageRank → candidates for blast radius.
        let top_file_ids: Vec<String> = {
            let mut pairs: Vec<(&String, &f32)> = pagerank_map
                .iter()
                .filter(|(id, _)| id.starts_with("file:"))
                .collect();
            pairs.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap_or(std::cmp::Ordering::Equal));
            pairs
                .into_iter()
                .take(10)
                .map(|(id, _)| id.clone())
                .collect()
        };

        // ── 5c. Run all blast-radius calls in parallel. ───────────────
        tracing::info!(
            "produce_claude_md section [{}] took {:?}",
            prev_section,
            section_clock.elapsed()
        );
        section_clock = std::time::Instant::now();
        prev_section = "5c. Run all blast-radius calls in parallel.";
        //
        // Each `compute_blast_radius` does heavy work (per-kind
        // `neighbors` calls + `list_edges` scan for file targets +
        // internal PageRank). Sequentially on OciusX-scale graphs
        // this was ~3s; spawn_blocking'd concurrently we bottleneck on
        // the tokio blocking threadpool instead and land around
        // ~500ms for the same 10 calls.
        let blast_handles: Vec<_> = top_file_ids
            .iter()
            .map(|file_id| {
                let g = graph.clone();
                let p = pid.clone();
                let t = file_id.clone();
                tokio::task::spawn_blocking(move || {
                    (
                        t.clone(),
                        crate::services::blast_radius_service::compute_blast_radius(
                            &g, &p, &t, active_gen, false,
                        ),
                    )
                })
            })
            .collect();

        let mut danger_zones: Vec<svc::DangerZone> = Vec::new();
        for handle in blast_handles {
            if let Ok((file_id, Ok(report))) = handle.await {
                if report.migration_risk >= 4 {
                    let path = file_id
                        .strip_prefix("file:")
                        .unwrap_or(&file_id)
                        .to_string();
                    danger_zones.push(svc::DangerZone {
                        file_path: path,
                        risk_score: report.migration_risk,
                        risk_band: report.risk_band.to_string(),
                        total_downstream: report.total_downstream,
                        reasons: reasons_from_breakdown(&report.complexity_breakdown),
                    });
                }
            }
        }
        danger_zones.sort_by(|a, b| {
            b.risk_score
                .cmp(&a.risk_score)
                .then(b.total_downstream.cmp(&a.total_downstream))
        });
        danger_zones.truncate(10);

        // ── 6. Static coding style per language (top-5% share) ────────
        tracing::info!(
            "produce_claude_md section [{}] took {:?}",
            prev_section,
            section_clock.elapsed()
        );
        section_clock = std::time::Instant::now();
        prev_section = "6. Static coding style per language (top-5% shar";
        //
        // Reuses the precomputed `pagerank_map` + `file_nodes` so the
        // per-language gather is an in-memory filter + rank, not a
        // fresh graph scan.
        let mut per_language_rules: Vec<svc::LanguageRules> = Vec::new();
        for lang in &languages {
            if lang.share_percent < 5.0 {
                continue;
            }
            let bullets = gather_language_style(
                &file_nodes,
                &pagerank_map,
                &rec.directory,
                &lang.language,
                3,
            )
            .await;
            if bullets.bullets.is_empty() {
                continue;
            }
            per_language_rules.push(svc::LanguageRules {
                language: lang.language.clone(),
                glob: svc::language_to_globs(&lang.language),
                bullets: bullets.bullets,
                sample_files: bullets.sample_files,
            });
        }

        // ── 7. Session workflow summary (only if state nodes exist) ───
        tracing::info!(
            "produce_claude_md section [{}] took {:?}",
            prev_section,
            section_clock.elapsed()
        );
        section_clock = std::time::Instant::now();
        prev_section = "7. Session workflow summary (only if state nodes";
        let state_summary = {
            let g = graph.clone();
            let p = pid.clone();
            let report = tokio::task::spawn_blocking(move || {
                crate::services::session_workflow_service::reconstruct_session_workflows(&g, &p)
            })
            .await
            .ok();
            report.filter(|r| r.total_keys > 0).map(|r| {
                use crate::services::session_workflow_service::StateScope;
                let mut session = 0;
                let mut viewstate = 0;
                let mut application = 0;
                for f in &r.workflows {
                    match f.scope {
                        StateScope::Session => session += 1,
                        StateScope::ViewState => viewstate += 1,
                        StateScope::Application => application += 1,
                        _ => {}
                    }
                }
                let mut by_fanin: Vec<(String, usize)> = r
                    .workflows
                    .iter()
                    .map(|f| (f.key.clone(), f.writers.len() + f.readers.len()))
                    .collect();
                by_fanin.sort_by(|a, b| b.1.cmp(&a.1));
                by_fanin.truncate(5);
                svc::StateSummary {
                    total_state_keys: r.total_keys,
                    session_keys: session,
                    viewstate_keys: viewstate,
                    application_keys: application,
                    cross_page_chains: r.cross_page_chains,
                    top_keys: by_fanin,
                    unresolved_accesses: 0, // filled below
                }
            })
        };
        // TODO-17: dynamic-key accesses make the inventory a lower bound —
        // count them so the generated rules say so.
        let state_summary = match state_summary {
            Some(mut s) => {
                let g = graph.clone();
                let p = pid.clone();
                let unresolved = tokio::task::spawn_blocking(move || {
                    let r = g
                        .list_edges_by_kind(
                            &p,
                            engram_graph::EdgeKind::UnresolvedStateRead,
                            usize::MAX,
                        )
                        .map(|v| v.len())
                        .unwrap_or(0);
                    let w = g
                        .list_edges_by_kind(
                            &p,
                            engram_graph::EdgeKind::UnresolvedStateWrite,
                            usize::MAX,
                        )
                        .map(|v| v.len())
                        .unwrap_or(0);
                    r + w
                })
                .await
                .unwrap_or(0);
                s.unresolved_accesses = unresolved;
                Some(s)
            }
            None => None,
        };

        // ── 8. Database summary (only if db_table nodes exist) ────────
        tracing::info!(
            "produce_claude_md section [{}] took {:?}",
            prev_section,
            section_clock.elapsed()
        );
        section_clock = std::time::Instant::now();
        prev_section = "8. Database summary (only if db_table nodes exis";
        let db_summary = {
            let g = graph.clone();
            let p = pid.clone();
            tokio::task::spawn_blocking(move || {
                let tables = g
                    .query_nodes(&p, Some("db_table"), None, None, 500)
                    .unwrap_or_default();
                if tables.is_empty() {
                    return None;
                }
                let mut with_refs: Vec<(String, usize)> = tables
                    .iter()
                    .map(|t| {
                        let refs = g
                            .find_incoming_edges_with_kind(&p, None, &t.node_id, 500)
                            .map(|v| v.len())
                            .unwrap_or(0);
                        (t.name.clone(), refs)
                    })
                    .collect();
                with_refs.sort_by(|a, b| b.1.cmp(&a.1));
                let top_tables = with_refs.iter().take(5).cloned().collect();
                Some(svc::DbSummary {
                    table_count: tables.len(),
                    top_tables,
                })
            })
            .await
            .ok()
            .flatten()
        };

        // ── 8b. CodeRabbit review_pattern nodes → per-language map ────
        tracing::info!(
            "produce_claude_md section [{}] took {:?}",
            prev_section,
            section_clock.elapsed()
        );
        section_clock = std::time::Instant::now();
        prev_section = "8b. CodeRabbit review_pattern nodes → per-langua";
        // Queries every `review_pattern` node (kind=pattern only;
        // wontFix/suppression clusters go elsewhere) and buckets them
        // by language. Metadata is the JSON blob written by
        // `code_review_ingest_service::cluster_metadata_value`.
        let coderabbit_rules_by_language = {
            let graph = self.state.graph.clone();
            let p = pid.clone();
            tokio::task::spawn_blocking(move || {
                graph.query_nodes(&p, Some("review_pattern"), None, None, 5_000)
            })
            .await
            .ok()
            .and_then(|r| r.ok())
            .map(|nodes| build_coderabbit_language_map(&nodes))
            .unwrap_or_default()
        };

        // ── 8c. GIS presence: one spatial_call edge is enough ─────────
        let has_gis = {
            let graph = self.state.graph.clone();
            let p = pid.clone();
            tokio::task::spawn_blocking(move || {
                graph.list_edges_by_kind(&p, EdgeKind::SpatialCall, 1)
            })
            .await
            .ok()
            .and_then(|r| r.ok())
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        };

        // ── 8c2. Co-change pairs from git temporal coupling. Empty until
        tracing::info!(
            "produce_claude_md section [{}] took {:?}",
            prev_section,
            section_clock.elapsed()
        );
        section_clock = std::time::Instant::now();
        prev_section = "8c2. Co-change pairs from git temporal coupling.";
        // index_git_history has run - the rules pipeline drops the section
        // when there are no pairs.
        let (co_change_pairs, cross_section_pairs) = {
            let graph = self.state.graph.clone();
            let p = pid.clone();
            tokio::task::spawn_blocking(move || {
                // Stream ALL temporal edges (1.3M on OciusX) — a capped list
                // takes the first N by key order, which biases the sample
                // alphabetically, not by strength. Accumulate BOTH the top file
                // pairs AND the section-level rollup in one pass; the section
                // rollup must see the full stream (the top file pairs are
                // dominated by intra-section families like the resx set).
                let mut best = std::collections::HashMap::new();
                let mut xsec = std::collections::HashMap::new();
                if let Err(e) =
                    graph.fold_edges_by_kind(&p, engram_graph::EdgeKind::TemporalCoupling, |edge| {
                        svc::accumulate_co_change(&mut best, edge);
                        svc::accumulate_cross_section(&mut xsec, edge);
                    })
                {
                    tracing::warn!("co-change fold failed: {e:#}");
                }
                (
                    svc::finalize_co_change_pairs(best, 20),
                    svc::finalize_cross_section(xsec, 400),
                )
            })
            .await
            .unwrap_or_default()
        };

        // ── 8d. Auth summary: house guard helpers + roles from guard
        // metadata stamped on function nodes by the extractors. ────────
        let auth_summary = {
            let graph = self.state.graph.clone();
            let p = pid.clone();
            tokio::task::spawn_blocking(move || {
                let nodes = graph
                    .query_nodes(&p, Some("function"), None, None, 50_000)
                    .unwrap_or_default();
                let mut house: std::collections::HashMap<String, usize> = Default::default();
                let mut roles: std::collections::HashMap<String, usize> = Default::default();
                let mut guarded = 0usize;
                for n in &nodes {
                    let m = n.metadata.as_ref();
                    let checks = m
                        .and_then(|m| m.get("permission_checks"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if checks.is_empty() {
                        continue;
                    }
                    guarded += 1;
                    for g in checks.split(';').filter(|g| !g.is_empty()) {
                        *house.entry(g.to_lowercase()).or_default() += 1;
                    }
                    let rs = m
                        .and_then(|m| m.get("guard_roles"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    for r in rs.split(';').filter(|r| !r.is_empty()) {
                        *roles.entry(r.to_string()).or_default() += 1;
                    }
                }
                (house, roles, guarded)
            })
            .await
            .ok()
            .and_then(|(house, roles, guarded)| {
                if house.is_empty() {
                    return None;
                }
                let mut hs: Vec<_> = house.into_iter().collect();
                hs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                let mode = format!(
                    "house guard helpers: {}",
                    hs.iter()
                        .take(3)
                        .map(|(name, c)| format!("{name} ({c}x)"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                let mut rv: Vec<_> = roles.into_iter().collect();
                rv.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                Some(svc::AuthSummary {
                    mode,
                    required_roles: rv.into_iter().map(|(r, _)| r).take(10).collect(),
                    session_auth_patterns: guarded,
                })
            })
        };

        // ── 9. Build the snapshot ─────────────────────────────────────
        tracing::info!(
            "produce_claude_md section [{}] took {:?}",
            prev_section,
            section_clock.elapsed()
        );
        section_clock = std::time::Instant::now();
        prev_section = "9. Build the snapshot";
        let project_name = project_name_from_dir(&rec.directory);
        let snapshot = svc::ProjectSnapshot {
            project_name,
            role_description: role,
            languages,
            build_commands: detect_build_commands(&rec.directory),
            danger_zones,
            critical_rules,
            per_language_rules,
            state_summary,
            db_summary,
            co_change_pairs,
            cross_section_pairs,
            auth_summary,
            frontend_warnings: Vec::new(),
            existing_claude_md: existing_md.clone(),
            coderabbit_rules_by_language,
            has_gis,
            generated_from: Some((active_gen, crate::utils::ymd_utc(crate::utils::now_ms()))),
        };

        // ── 10. Render ────────────────────────────────────────────────
        tracing::info!(
            "produce_claude_md section [{}] took {:?}",
            prev_section,
            section_clock.elapsed()
        );
        section_clock = std::time::Instant::now();
        prev_section = "10. Render";
        let root_md = svc::render_root_claude_md(&snapshot, req.max_root_lines);
        let rule_files = svc::render_rule_files(&snapshot);
        let agents_md = if req.generate_agents_md {
            Some(svc::render_agents_md(&snapshot, req.max_root_lines.max(80)))
        } else {
            None
        };

        // ── 11. Optional disk write ───────────────────────────────────
        //
        // Safety contract for the CLAUDE.md write path:
        //
        //  1. If CLAUDE.md does NOT exist → write engram output
        //     directly. Normal case.
        //
        //  2. If CLAUDE.md exists AND the caller set
        //     `overwrite_existing: false` (default) → NEVER touch
        //     the existing file. Engram output is diverted to
        //     `CLAUDE.engram.md` so the caller can inspect it
        //     without any risk of clobbering hand-authored content.
        //
        //  3. If CLAUDE.md exists AND `overwrite_existing: true` AND
        //     `merge_existing: true` → splice the new engram block
        //     into the existing file between
        //     `<!-- engram:begin --> ... <!-- engram:end -->`
        //     markers, preserving every other byte. Back up the
        //     pre-write state to `CLAUDE.md.<unix_ts>.bak` first.
        //
        //  4. If CLAUDE.md exists AND `overwrite_existing: true` AND
        //     `merge_existing: false` → full overwrite, but still
        //     back up to `CLAUDE.md.<unix_ts>.bak` first.
        //
        // This makes the DATA-LOSS case (overwrite a hand-authored
        // CLAUDE.md without any backup) structurally impossible
        // without the caller explicitly opting into `overwrite_existing`.
        let mut written: Vec<String> = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        // Always surface the rules-pipeline summary so the caller
        // sees the noise-filter / meta-clustering impact. Happens
        // regardless of whether we write to disk.
        notes.push(pipeline_summary);
        if req.write_to_disk {
            use engram_core::safe_join;
            let project_dir = std::path::PathBuf::from(&rec.directory);
            let root_path = safe_join(&project_dir, "CLAUDE.md")
                .map_err(|e| McpError::invalid_request(e.to_string(), None))?;
            let root_exists = root_path.exists();

            // The file we will actually write to depends on safety
            // layer 1 (divert) vs layers 3/4 (overwrite).
            let (final_path, final_content) = if !root_exists {
                // Case 1: nothing there → direct write.
                (root_path.clone(), root_md.clone())
            } else if !req.overwrite_existing {
                // Case 2: divert. Never clobber a hand-authored
                // CLAUDE.md without explicit consent.
                let divert = safe_join(&project_dir, "CLAUDE.engram.md")
                    .map_err(|e| McpError::invalid_request(e.to_string(), None))?;
                notes.push(
                    "Existing CLAUDE.md was left UNTOUCHED — engram output was written to \
                     CLAUDE.engram.md instead. Pass `overwrite_existing: true` to merge or \
                     replace it."
                        .into(),
                );
                (divert, root_md.clone())
            } else {
                // Case 3/4: explicit overwrite — always back up
                // first, then apply the requested merge mode.
                let existing = std::fs::read_to_string(&root_path)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let backup_name = format!("CLAUDE.md.{ts}.bak");
                let backup_path = safe_join(&project_dir, &backup_name)
                    .map_err(|e| McpError::invalid_request(e.to_string(), None))?;
                std::fs::write(&backup_path, &existing)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                written.push(backup_name.clone());
                notes.push(format!(
                    "Existing CLAUDE.md was backed up to `{backup_name}` before any write."
                ));
                let mode = req.merge_mode.as_str();
                let new_content = match mode {
                    // Full replace — user asked for it; backup still ran.
                    "replace" => root_md.clone(),
                    // Optimize rewrite — section-level classification
                    // replaces engram-owned sections with fresh output
                    // and preserves domain-specific human content.
                    "optimize" => {
                        let (rewritten, report) = svc::optimize_rewrite(&existing, &root_md);
                        notes.push(format!(
                            "Optimize rewrite: {original} → {rewritten} lines \
                             (replaced {replaced}, preserved {preserved} section(s))",
                            original = report.original_line_count,
                            rewritten = report.rewritten_line_count,
                            replaced = report.replaced_sections.len(),
                            preserved = report.preserved_sections.len(),
                        ));
                        if !report.preserved_sections.is_empty() {
                            notes.push(format!(
                                "Preserved human sections: {}",
                                report.preserved_sections.join(", ")
                            ));
                        }
                        if !report.replaced_sections.is_empty() {
                            notes.push(format!(
                                "Engram-owned sections replaced: {}",
                                report.replaced_sections.join(", ")
                            ));
                        }
                        rewritten
                    }
                    // Splice (default) — preserve everything, replace
                    // only the engram-markered block, or append if
                    // markers are absent. Conservative.
                    _ => svc::splice_engram_section(&existing, &root_md),
                };
                (root_path.clone(), new_content)
            };

            std::fs::write(&final_path, &final_content)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            written.push(
                final_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "CLAUDE.md".into()),
            );

            let rules_dir = safe_join(&project_dir, ".claude/rules")
                .map_err(|e| McpError::invalid_request(e.to_string(), None))?;
            std::fs::create_dir_all(&rules_dir)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            // Purge stale rule files from prior runs. We only prune
            // files that match the generator's naming patterns
            // (`*-conventions.md`, `danger-zones.md`, `state-and-data.md`,
            // `co-change-pairs.md`, `frontend-notes.md`) so any
            // hand-authored files under `.claude/rules/` survive. This
            // keeps the set of convention files in sync with what the
            // current run actually produced — e.g. an
            // `unknown-conventions.md` left over from a pre-filter run
            // disappears on the next refresh.
            let current_filenames: std::collections::HashSet<&str> =
                rule_files.iter().map(|f| f.filename.as_str()).collect();
            if let Ok(entries) = std::fs::read_dir(&rules_dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let is_engram_owned = name.ends_with("-conventions.md")
                        || matches!(
                            name.as_str(),
                            "danger-zones.md"
                                | "state-and-data.md"
                                | "co-change-pairs.md"
                                | "frontend-notes.md"
                        );
                    if is_engram_owned && !current_filenames.contains(name.as_str()) {
                        let _ = std::fs::remove_file(entry.path());
                        notes.push(format!(
                            "Removed stale .claude/rules/{name} (not emitted this run)"
                        ));
                    }
                }
            }

            for f in &rule_files {
                let path = safe_join(&project_dir, &format!(".claude/rules/{}", f.filename))
                    .map_err(|e| McpError::invalid_request(e.to_string(), None))?;
                std::fs::write(&path, &f.content)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                written.push(format!(".claude/rules/{}", f.filename));
            }
            if let Some(ref agents) = agents_md {
                let path = safe_join(&project_dir, "AGENTS.md")
                    .map_err(|e| McpError::invalid_request(e.to_string(), None))?;
                // AGENTS.md gets the same treatment — if present and
                // overwrite_existing=false, divert to AGENTS.engram.md.
                // Always back up on overwrite.
                let agents_exists = path.exists();
                let (a_path, a_content) = if !agents_exists {
                    (path.clone(), agents.clone())
                } else if !req.overwrite_existing {
                    let divert = safe_join(&project_dir, "AGENTS.engram.md")
                        .map_err(|e| McpError::invalid_request(e.to_string(), None))?;
                    notes.push(
                        "Existing AGENTS.md was left untouched — engram output diverted to \
                         AGENTS.engram.md."
                            .into(),
                    );
                    (divert, agents.clone())
                } else {
                    let existing = std::fs::read_to_string(&path)
                        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                    let ts = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let backup_name = format!("AGENTS.md.{ts}.bak");
                    let backup_path = safe_join(&project_dir, &backup_name)
                        .map_err(|e| McpError::invalid_request(e.to_string(), None))?;
                    std::fs::write(&backup_path, &existing)
                        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                    written.push(backup_name);
                    let new_content = if req.merge_existing {
                        svc::splice_engram_section(&existing, agents)
                    } else {
                        agents.clone()
                    };
                    (path.clone(), new_content)
                };
                std::fs::write(&a_path, &a_content)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                written.push(
                    a_path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "AGENTS.md".into()),
                );
            }
        }

        // ── 12. Build response ────────────────────────────────────────
        let mut output = String::with_capacity(8192);
        if !written.is_empty() {
            output.push_str("# Written files\n\n");
            for w in &written {
                let _ = writeln!(output, "- {w}");
            }
            output.push('\n');
        }
        if !notes.is_empty() {
            output.push_str("# Write-path notes\n\n");
            for n in &notes {
                let _ = writeln!(output, "- {n}");
            }
            output.push('\n');
        }
        let root_lines = root_md.lines().count();
        let _ = writeln!(output, "# CLAUDE.md ({root_lines} lines)\n\n{root_md}\n");
        for f in &rule_files {
            let lines = f.content.lines().count();
            let _ = writeln!(
                output,
                "# .claude/rules/{} ({} lines)\n\n{}\n",
                f.filename, lines, f.content
            );
        }
        if let Some(ref agents) = agents_md {
            let lines = agents.lines().count();
            let _ = writeln!(output, "# AGENTS.md ({lines} lines)\n\n{agents}\n");
        }

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    pub async fn handle_detect_design_patterns(
        &self,
        req: DetectDesignPatternsRequest,
    ) -> Result<CallToolResult, McpError> {
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let pattern_filter = req.pattern_filter.clone();
        let limit = req.sanitized_limit();

        let pid_copy = pid.clone();
        let mut patterns = tokio::task::spawn_blocking(move || {
            crate::services::pattern_detection_service::detect_design_antipatterns(
                &graph, &pid_copy, 20, // god_threshold
                10, // spaghetti_threshold
                5,  // soup_threshold
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if !pattern_filter.is_empty() {
            patterns.retain(|p| {
                pattern_filter
                    .iter()
                    .any(|f| p.pattern_name.to_lowercase().contains(&f.to_lowercase()))
            });
        }

        patterns.truncate(limit);

        let mut out = format!(
            "# Design Pattern Analysis — {}\n\n**Total patterns detected**: {}\n\n",
            pid,
            patterns.len()
        );

        for p in patterns {
            out.push_str(&format!("### {} [{}]\n", p.pattern_name, p.severity));
            out.push_str(&format!("- **Evidence**: {}\n", p.evidence.join(", ")));
            out.push_str(&format!("- **Modern strategy**: {}\n\n", p.modern_target));
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[allow(clippy::too_many_arguments)]
    async fn evaluate_wave_decision(
        &self,
        project_id: &str,
        _default_risk_profile: &str,
        wave_items: Vec<crate::models::WaveItemInput>,
        generation: u64,
        require_runtime_evidence: bool,
        evidence_depth: &str,
        output_json: bool,
    ) -> Result<CallToolResult, McpError> {
        use crate::services::autonomous_decision_service::{
            WaveAdpInput, evaluate_wave, format_wave_decision,
        };
        use crate::services::evidence_orchestration::{EvidenceDepth, EvidenceOverrides};

        let depth = EvidenceDepth::from_str(evidence_depth)
            .map_err(|e| McpError::invalid_params(e, None))?;
        let mut items = Vec::with_capacity(wave_items.len());

        for item in &wave_items {
            let overrides = EvidenceOverrides::default();
            let risk_profile = crate::services::autonomous_decision_service::RiskProfile::from_str(
                &item.risk_profile,
            )
            .map_err(|e| McpError::invalid_params(e, None))?;

            match crate::services::evidence_orchestration::gather_evidence(
                &self.state,
                project_id,
                std::slice::from_ref(&item.file_path),
                &item.change_description,
                risk_profile,
                depth,
                &overrides,
                require_runtime_evidence,
                generation,
            )
            .await
            {
                Ok(adp_input) => {
                    items.push((item.file_path.clone(), adp_input));
                }
                Err(e) => {
                    tracing::warn!(file = %item.file_path, error = %e, "Failed to gather evidence for wave item");
                }
            }
        }

        let wave_input = WaveAdpInput {
            wave_number: 1,
            wave_name: "Manual Wave".into(),
            items,
            cross_item_deps: 0,
        };

        let decision = evaluate_wave(&wave_input);

        if output_json {
            let json = serde_json::to_string_pretty(&decision)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        Ok(CallToolResult::success(vec![Content::text(
            format_wave_decision(&decision),
        )]))
    }

    pub async fn handle_autonomous_decision_gate(
        &self,
        req: crate::models::AutonomousDecisionGateRequest,
    ) -> Result<CallToolResult, McpError> {
        let _ = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;

        if let Some(wave_items) = req.wave_items {
            return self
                .evaluate_wave_decision(
                    &req.project_id,
                    &req.risk_profile,
                    wave_items,
                    gen_,
                    req.require_runtime_evidence,
                    &req.evidence_depth,
                    req.output_json,
                )
                .await;
        }

        let overrides = crate::services::evidence_orchestration::EvidenceOverrides {
            extraction_confidence: req.extraction_confidence,
            extraction_type: req.extraction_type,
            trace_used_fallback: if req.trace_used_fallback {
                Some(true)
            } else {
                None
            },
            trace_candidate_count: if req.trace_candidate_count > 0 {
                Some(req.trace_candidate_count)
            } else {
                None
            },
            immune_verdict: req.immune_verdict,
            immune_confidence: req.immune_confidence,
            has_runtime_evidence: if req.has_runtime_evidence {
                Some(true)
            } else {
                None
            },
            reconciliation: None,
            safety_decision: None,
            retrieval_production_ready: None,
            retrieval_ndcg: None,
            retrieval_recall: None,
            migration_class: req.migration_class,
        };

        let risk_profile =
            crate::services::autonomous_decision_service::RiskProfile::from_str(&req.risk_profile)
                .map_err(|e| McpError::invalid_params(e, None))?;
        let depth =
            crate::services::evidence_orchestration::EvidenceDepth::from_str(&req.evidence_depth)
                .map_err(|e| McpError::invalid_params(e, None))?;

        let adp_input = crate::services::evidence_orchestration::gather_evidence(
            &self.state,
            &req.project_id,
            &req.target_files,
            &req.proposed_change,
            risk_profile,
            depth,
            &overrides,
            req.require_runtime_evidence,
            gen_,
        )
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let raw_decision = crate::services::autonomous_decision_service::evaluate_gates(&adp_input);

        // ADP1/ADP kill-switch: apply the configured rollout policy so that the
        // deployment phase (shadow/advisory/guarded/autonomous) and the runtime
        // kill-switch are honoured.  Without this call, the raw ADP verdict is
        // returned regardless of rollout phase — bypassing the safety guardrails.
        let phase = crate::services::autonomous_decision_service::RolloutPhase::from_str(
            &self.state.cfg.adp_rollout_phase,
        )
        .map_err(|e| McpError::internal_error(format!("invalid adp_rollout_phase: {e}"), None))?;
        // ADP1: read the runtime kill-switch (OR of config + persisted registry
        // value) rather than the immutable Config field. This ensures the kill-
        // switch survives process restarts and can be toggled at runtime.
        let kill_switch = self
            .state
            .adp_kill_switch
            .load(std::sync::atomic::Ordering::Acquire);
        let decision = crate::services::autonomous_decision_service::apply_rollout_policy(
            &raw_decision,
            phase,
            kill_switch,
        );

        if req.output_json {
            let json = serde_json::to_string_pretty(&decision)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        Ok(CallToolResult::success(vec![Content::text(
            crate::services::autonomous_decision_service::format_decision(&decision),
        )]))
    }

    pub async fn handle_graph_centrality_rerank(
        &self,
        req: GraphCentralityRerankRequest,
    ) -> Result<CallToolResult, McpError> {
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;
        let top_k = req.sanitized_top_k();
        let samples = req.sanitized_betweenness_samples();

        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let active_gen = gen_;
        let centrality: engram_graph::analysis::MultiCentrality =
            tokio::task::spawn_blocking(move || {
                engram_graph::analysis::compute_multi_centrality(&graph, &pid, active_gen, samples)
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let pr_w = req.pagerank_weight;
        let deg_w = req.degree_weight;
        let bt_w = req.betweenness_weight;

        #[derive(serde::Serialize)]
        struct ScoredNode {
            node_id: String,
            blended_score: f32,
            pagerank: f32,
            in_degree: u32,
            out_degree: u32,
            betweenness: f32,
            #[serde(skip_serializing_if = "Option::is_none")]
            search_score: Option<f32>,
            #[serde(skip_serializing_if = "Option::is_none")]
            node_type: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            name: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            file_path: Option<String>,
        }

        let mut scored: Vec<ScoredNode> = Vec::new();

        if let Some(query) = &req.query {
            let hits = ps
                .search
                .search(
                    &engram_index::HybridQuery {
                        project_id: req.project_id.clone(),
                        namespace: req.namespace.clone(),
                        generation: gen_,
                        text: query.clone(),
                        top_k: top_k * 3,
                        fts_mode: "strict".to_string(),
                        include_path_prefixes: None,
                        exclude_path_prefixes: None,
                        language_filters: None,
                        author_filter: None,
                        date_after: None,
                        date_before: None,
                        use_mmr: false,
                    },
                    Some(&centrality.pagerank),
                    &tokio_util::sync::CancellationToken::new(),
                )
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            for hit in &hits {
                let file_node_id = format!("file:{}", hit.path.as_str());
                let blended = centrality.blended_score(&file_node_id, pr_w, deg_w, bt_w);
                let combined = hit.score * 0.7 + blended * 0.3;
                let (node_type, name, file_path) = if req.include_metadata {
                    (
                        Some("file".to_string()),
                        Some(hit.path.as_str().to_string()),
                        Some(hit.path.as_str().to_string()),
                    )
                } else {
                    (None, None, None)
                };
                scored.push(ScoredNode {
                    node_id: file_node_id.clone(),
                    blended_score: combined,
                    pagerank: centrality
                        .pagerank
                        .get(&file_node_id)
                        .copied()
                        .unwrap_or(0.0),
                    in_degree: centrality
                        .in_degree
                        .get(&file_node_id)
                        .copied()
                        .unwrap_or(0),
                    out_degree: centrality
                        .out_degree
                        .get(&file_node_id)
                        .copied()
                        .unwrap_or(0),
                    betweenness: centrality
                        .betweenness
                        .get(&file_node_id)
                        .copied()
                        .unwrap_or(0.0),
                    search_score: Some(hit.score),
                    node_type,
                    name,
                    file_path,
                });
            }
            scored.sort_by(|a, b| {
                b.blended_score
                    .partial_cmp(&a.blended_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        } else if let Some(node_ids) = &req.node_ids {
            let graph_store = self.state.graph.clone();
            let pid2 = req.project_id.clone();
            let node_ids_clone = node_ids.clone();
            let include_meta = req.include_metadata;
            let nodes_meta: Vec<NodeMetaTuple> = tokio::task::spawn_blocking(move || -> Vec<_> {
                let mut result = Vec::new();
                for nid in &node_ids_clone {
                    let meta = if include_meta {
                        graph_store
                            .get_node(&pid2, nid)
                            .ok()
                            .flatten()
                            .map(|n| (n.node_type, n.name, n.file_path.as_str().to_string()))
                    } else {
                        None
                    };
                    let (nt, nm, fp) = meta
                        .map(|(t, n, f)| (Some(t), Some(n), Some(f)))
                        .unwrap_or((None, None, None));
                    result.push((nid.clone(), nt, nm, fp));
                }
                result
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            for (nid, nt, nm, fp) in nodes_meta {
                let blended = centrality.blended_score(&nid, pr_w, deg_w, bt_w);
                scored.push(ScoredNode {
                    node_id: nid.clone(),
                    blended_score: blended,
                    pagerank: centrality.pagerank.get(&nid).copied().unwrap_or(0.0),
                    in_degree: centrality.in_degree.get(&nid).copied().unwrap_or(0),
                    out_degree: centrality.out_degree.get(&nid).copied().unwrap_or(0),
                    betweenness: centrality.betweenness.get(&nid).copied().unwrap_or(0.0),
                    search_score: None,
                    node_type: nt,
                    name: nm,
                    file_path: fp,
                });
            }
            scored.sort_by(|a, b| {
                b.blended_score
                    .partial_cmp(&a.blended_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        } else {
            // Mode 3: Top-N most central nodes
            let graph_store = self.state.graph.clone();
            let pid3 = req.project_id.clone();
            let include_meta = req.include_metadata;
            let mut all_scores: Vec<(String, f32)> = centrality
                .pagerank
                .keys()
                .map(|nid| {
                    let b = centrality.blended_score(nid, pr_w, deg_w, bt_w);
                    (nid.clone(), b)
                })
                .collect();
            all_scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            all_scores.truncate(top_k);
            let top_ids: Vec<String> = all_scores.iter().map(|(id, _)| id.clone()).collect();
            let nodes_meta: Vec<NodeMetaTuple> = tokio::task::spawn_blocking(move || -> Vec<_> {
                let mut result = Vec::new();
                for nid in &top_ids {
                    let meta = if include_meta {
                        graph_store
                            .get_node(&pid3, nid)
                            .ok()
                            .flatten()
                            .map(|n| (n.node_type, n.name, n.file_path.as_str().to_string()))
                    } else {
                        None
                    };
                    let (nt, nm, fp) = meta
                        .map(|(t, n, f)| (Some(t), Some(n), Some(f)))
                        .unwrap_or((None, None, None));
                    result.push((nid.clone(), nt, nm, fp));
                }
                result
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            for (nid, nt, nm, fp) in nodes_meta {
                let blended = centrality.blended_score(&nid, pr_w, deg_w, bt_w);
                scored.push(ScoredNode {
                    node_id: nid.clone(),
                    blended_score: blended,
                    pagerank: centrality.pagerank.get(&nid).copied().unwrap_or(0.0),
                    in_degree: centrality.in_degree.get(&nid).copied().unwrap_or(0),
                    out_degree: centrality.out_degree.get(&nid).copied().unwrap_or(0),
                    betweenness: centrality.betweenness.get(&nid).copied().unwrap_or(0.0),
                    search_score: None,
                    node_type: nt,
                    name: nm,
                    file_path: fp,
                });
            }
        }

        scored.truncate(top_k);

        if req.output_json {
            let json = serde_json::to_string_pretty(&scored)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let mode = if req.query.is_some() {
            "search+rerank"
        } else if req.node_ids.is_some() {
            "node scoring"
        } else {
            "top-N centrality"
        };
        let mut out = format!(
            "Graph Centrality Rerank ({mode})\nWeights: PR={pr_w:.2}, Degree={deg_w:.2}, Betweenness={bt_w:.2}\nResults: {}\n\n",
            scored.len()
        );
        for (i, node) in scored.iter().enumerate() {
            out.push_str(&format!(
                "{}. {} (blended={:.4})\n",
                i + 1,
                node.node_id,
                node.blended_score
            ));
            out.push_str(&format!(
                "   PR={:.6}  in_deg={}  out_deg={}  betw={:.4}",
                node.pagerank, node.in_degree, node.out_degree, node.betweenness
            ));
            if let Some(ss) = node.search_score {
                out.push_str(&format!("  search={ss:.4}"));
            }
            out.push('\n');
            if let Some(ref nt) = node.node_type {
                out.push_str(&format!("   type={nt}"));
            }
            if let Some(ref nm) = node.name {
                out.push_str(&format!("  name={nm}"));
            }
            if let Some(ref fp) = node.file_path {
                out.push_str(&format!("  path={fp}"));
            }
            if node.node_type.is_some() || node.name.is_some() || node.file_path.is_some() {
                out.push('\n');
            }
        }
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_benchmark_retrieval(
        &self,
        req: crate::models::BenchmarkRetrievalRequest,
    ) -> Result<CallToolResult, McpError> {
        let pid = req.project_id.clone();
        let ps = self.ensure_project_runtime(&pid).await?;
        let generation = self.get_active_generation(&pid).await?;

        let queries: Vec<crate::services::benchmark_service::BenchmarkQuery> =
            if let Some(custom) = req.custom_queries {
                custom
                    .into_iter()
                    .map(|q| crate::services::benchmark_service::BenchmarkQuery {
                        query: q.query,
                        relevant_paths: q.relevant_paths,
                    })
                    .collect()
            } else {
                crate::services::benchmark_service::generate_legacy_benchmark_queries()
            };

        let mut per_query: Vec<crate::services::benchmark_service::QueryBenchmarkResult> =
            Vec::new();
        let (mut total_ndcg, mut total_recall, mut total_mrr, mut total_latency, mut max_latency) =
            (0.0f64, 0.0f64, 0.0f64, 0u64, 0u64);
        let mut latencies: Vec<u64> = Vec::new();

        for bq in &queries {
            let start = std::time::Instant::now();
            let hits = ps
                .search
                .search(
                    &engram_index::HybridQuery {
                        project_id: pid.clone(),
                        namespace: "memory".into(),
                        generation,
                        text: bq.query.clone(),
                        top_k: 10,
                        fts_mode: "strict".into(),
                        include_path_prefixes: None,
                        exclude_path_prefixes: None,
                        language_filters: None,
                        author_filter: None,
                        date_after: None,
                        date_before: None,
                        use_mmr: false,
                    },
                    None,
                    &tokio_util::sync::CancellationToken::new(),
                )
                .await
                .unwrap_or_default();
            let elapsed_ms = start.elapsed().as_millis() as u64;

            let actual_paths: Vec<String> =
                hits.iter().map(|h| h.path.as_str().to_string()).collect();
            let ndcg = crate::services::benchmark_service::compute_ndcg(
                &actual_paths,
                &bq.relevant_paths,
                10,
            );
            let recall = crate::services::benchmark_service::compute_recall(
                &actual_paths,
                &bq.relevant_paths,
                10,
            );
            let mrr = crate::services::benchmark_service::compute_reciprocal_rank(
                &actual_paths,
                &bq.relevant_paths,
            );

            total_ndcg += ndcg;
            total_recall += recall;
            total_mrr += mrr;
            total_latency += elapsed_ms;
            if elapsed_ms > max_latency {
                max_latency = elapsed_ms;
            }
            latencies.push(elapsed_ms);

            per_query.push(crate::services::benchmark_service::QueryBenchmarkResult {
                query: bq.query.clone(),
                expected_top_paths: bq.relevant_paths.clone(),
                actual_top_paths: actual_paths,
                ndcg,
                recall,
                reciprocal_rank: mrr,
                latency_ms: elapsed_ms,
            });
        }

        let q_count = queries.len().max(1);
        let mean_ndcg = total_ndcg / q_count as f64;
        let mean_recall = total_recall / q_count as f64;
        let mean_mrr = total_mrr / q_count as f64;
        let mean_latency = total_latency as f64 / q_count as f64;
        latencies.sort();
        let p95_idx = ((latencies.len() as f64 * 0.95).ceil() as usize)
            .min(latencies.len())
            .saturating_sub(1);
        let p95_latency = latencies.get(p95_idx).copied().unwrap_or(0);

        let (passed_ndcg, passed_recall, production_ready) =
            crate::services::benchmark_service::evaluate_gates(
                mean_ndcg,
                mean_recall,
                self.state.cfg.retrieval_min_ndcg,
                self.state.cfg.retrieval_min_recall,
            );

        let result = crate::services::benchmark_service::BenchmarkResult {
            project_id: pid,
            timestamp_ms: now_ms(),
            query_count: queries.len(),
            ndcg_at_10: mean_ndcg,
            recall_at_10: mean_recall,
            mean_reciprocal_rank: mean_mrr,
            mean_latency_ms: mean_latency,
            p95_latency_ms: p95_latency as f64,
            passed_ndcg_gate: passed_ndcg,
            passed_recall_gate: passed_recall,
            production_ready,
            per_query_results: per_query,
        };

        if req.output_json {
            let json = serde_json::to_string_pretty(&result)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let mut out = String::with_capacity(4096);
        out.push_str(&format!(
            "Retrieval Benchmark ({} queries)\n",
            result.query_count
        ));
        out.push_str(&format!(
            "NDCG@10:  {:.3} (gate: {:.2}, {})\n",
            result.ndcg_at_10,
            self.state.cfg.retrieval_min_ndcg,
            if result.passed_ndcg_gate {
                "PASS"
            } else {
                "FAIL"
            }
        ));
        out.push_str(&format!(
            "Recall@10: {:.3} (gate: {:.2}, {})\n",
            result.recall_at_10,
            self.state.cfg.retrieval_min_recall,
            if result.passed_recall_gate {
                "PASS"
            } else {
                "FAIL"
            }
        ));
        out.push_str(&format!("MRR:      {:.3}\n", result.mean_reciprocal_rank));
        out.push_str(&format!(
            "Latency:  avg={:.0}ms p95={:.0}ms\n",
            result.mean_latency_ms, result.p95_latency_ms
        ));
        out.push_str(&format!(
            "\nProduction Ready: {}\n",
            if result.production_ready { "YES" } else { "NO" }
        ));
        for qr in &result.per_query_results {
            out.push_str(&format!(
                "\n  '{}': ndcg={:.3} recall={:.3} mrr={:.3} latency={}ms",
                qr.query, qr.ndcg, qr.recall, qr.reciprocal_rank, qr.latency_ms
            ));
        }
        Ok(CallToolResult::success(vec![Content::text(
            out.trim().to_string(),
        )]))
    }
}

// ── produce_claude_md helpers ────────────────────────────────────────────────

/// Derive a one-line role description from the language breakdown, with
/// simple framework hints based on what the graph contains. Intentionally
/// generic — no project-specific assumptions.
fn build_role_description(
    languages: &[crate::services::produce_claude_md_service::LanguageShare],
    graph: &engram_graph::GraphStore,
    project_id: &str,
) -> String {
    use crate::services::produce_claude_md_service::language_display;

    if languages.is_empty() {
        return "Indexed project.".into();
    }
    // Filter out data / markup formats before picking the top-3 —
    // "VB.NET + JavaScript + json" reads like an inventory bug.
    // JSON / XML / YAML / MD aren't programming languages; they're
    // file formats. Same for config / lockfiles.
    const NON_PROGRAMMING: &[&str] = &[
        "json",
        "xml",
        "yaml",
        "yml",
        "toml",
        "ini",
        "markdown",
        "md",
        "txt",
        "csv",
        "lock",
        "config",
        "properties",
    ];
    let is_programming = |lang: &str| -> bool {
        let lower = lang.to_ascii_lowercase();
        !NON_PROGRAMMING.iter().any(|n| *n == lower)
    };
    let names: Vec<&str> = languages
        .iter()
        .filter(|l| is_programming(&l.language))
        .take(3)
        .map(|l| language_display(&l.language))
        .collect();
    let lang_phrase = if names.is_empty() {
        // All detected languages were data formats — fall back to the
        // raw top-3 so we still say *something*.
        languages
            .iter()
            .take(3)
            .map(|l| language_display(&l.language))
            .collect::<Vec<_>>()
            .join(" + ")
    } else {
        names.join(" + ")
    };

    // Framework / data-layer / architecture hints — driven by the
    // graph's node-type inventory. What we're trying to tell the
    // agent in two sentences: "this is a <kind of app>, with <data
    // layer>, and <these architectural quirks that break generic
    // assumptions>."
    let counts = graph.count_nodes_by_type(project_id).unwrap_or_default();
    let has_pages = counts.get("page").copied().unwrap_or(0) > 0;
    let has_controls = counts.get("control").copied().unwrap_or(0) > 0;
    let has_tables = counts.get("db_table").copied().unwrap_or(0) > 0;
    let has_web_services = counts.get("web_service").copied().unwrap_or(0) > 0;
    let has_http_handlers = counts.get("http_handler").copied().unwrap_or(0) > 0;
    let has_wcf = counts.get("wcf_service").copied().unwrap_or(0) > 0;
    let has_route_handlers = counts.get("route_handler").copied().unwrap_or(0) > 0;

    // Fragment 1: architecture shape.
    let mut shape: Vec<String> = Vec::new();
    if has_pages && has_controls {
        shape.push("ASP.NET WebForms".into());
    }
    if has_route_handlers {
        shape.push("Web API".into());
    } else if has_web_services && !has_pages {
        shape.push("Web services (ASMX)".into());
    } else if has_web_services && has_pages {
        // Classic WebForms + ASMX combo like OciusX.
        shape.push("ASMX + Web API".into());
    }
    if has_wcf {
        shape.push("WCF services".into());
    }
    if has_http_handlers {
        shape.push("HTTP handlers".into());
    }

    // Fragment 2: multitenant detection. Pattern: nodes whose file
    // path lives in a `multitenant/` directory, or files whose name
    // contains `multitenant` or `tenant`. Cheap — one filename-substring
    // query over existing file nodes. If the graph returned ≥3
    // distinct files in a multitenant directory, it's a real feature,
    // not a one-off.
    let multitenant_files = graph
        .query_nodes(project_id, Some("file"), None, None, 5000)
        .ok()
        .map(|nodes| {
            nodes
                .iter()
                .filter(|n| {
                    let p = n.file_path.as_str().to_ascii_lowercase();
                    p.contains("/multitenant/") || p.contains("\\multitenant\\")
                })
                .count()
        })
        .unwrap_or(0);
    let multitenant_hint = multitenant_files >= 3;

    // Fragment 3: custom TypeScript framework detection. Pattern:
    // TypeScript files in a `q/` or `Q/` directory, or references to
    // `q.ctrl.` / `q.api.` / `q.page` / `q.bind.` namespaces in code.
    // This is OciusX-flavoured but generalises: any codebase with a
    // dominant internal TS namespace surfaces it here.
    let q_framework_files = graph
        .query_nodes(project_id, Some("file"), None, None, 5000)
        .ok()
        .map(|nodes| {
            nodes
                .iter()
                .filter(|n| {
                    let p = n.file_path.as_str().to_ascii_lowercase();
                    (p.ends_with(".ts") || p.ends_with(".tsx"))
                        && (p.contains("/q/") || p.contains("\\q\\"))
                })
                .count()
        })
        .unwrap_or(0);
    let custom_ts_framework = q_framework_files >= 5;

    // Fragment 4: compose. Keep it to two short sentences.
    // README blurb is prepended by the caller in the handler.
    let mut out = String::new();
    out.push_str(&lang_phrase);
    if !shape.is_empty() {
        out.push(' ');
        out.push_str(&shape.join(" + "));
    }
    if has_tables {
        out.push_str(" + SQL");
    }
    out.push('.');

    // Second sentence: quirks that break generic assumptions.
    let mut quirks: Vec<String> = Vec::new();
    if multitenant_hint {
        quirks.push("first-class multitenant mode".into());
    }
    if custom_ts_framework {
        quirks.push("custom `q` TypeScript framework".into());
    }
    if !quirks.is_empty() {
        out.push_str(" Notable: ");
        out.push_str(&quirks.join(", "));
        out.push('.');
    }

    out
}

/// Extract a 1-2 sentence blurb from the project's README if present,
/// usable as an opening line for the role description. Skips H1
/// headings, code fences, and HTML comments; returns the first
/// substantive paragraph. Cap at 280 characters — anything longer
/// bloats the root document.
fn read_readme_blurb(project_dir: &str) -> Option<String> {
    let dir = std::path::Path::new(project_dir);
    let candidates = ["README.md", "readme.md", "README.MD", "README"];
    let mut content: Option<String> = None;
    for name in &candidates {
        let full = dir.join(name);
        if let Ok(text) = std::fs::read_to_string(&full) {
            content = Some(text);
            break;
        }
    }
    let text = content?;
    // Skip H1 / blank lines / code fences; collect the first real
    // paragraph.
    let mut para = String::new();
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if trimmed.is_empty() {
            if !para.is_empty() {
                break; // paragraph end
            }
            continue;
        }
        if trimmed.starts_with('#') || trimmed.starts_with("<!--") {
            continue;
        }
        if !para.is_empty() {
            para.push(' ');
        }
        para.push_str(trimmed);
    }
    if para.is_empty() {
        return None;
    }
    // Strip trailing markdown link refs `[^1]`-style, trim quotes.
    let mut out = para.trim().to_string();
    if out.len() > 280 {
        out.truncate(280);
        while !out.is_empty() && !out.is_char_boundary(out.len()) {
            out.pop();
        }
        out.push('…');
    }
    Some(out)
}

/// Turn a [`engram_core::registry::RepoRule`] into a short rule text.
/// Prefers `rule_text` when present; falls back to a descriptive line
/// derived from the rule id / file pattern.
/// Parse a CodeRabbit repo-rule's embedded stats footer back into
/// `(fix_rate, pr_count)`. The ingest writes rules with a tail of the
/// form `"… — CodeRabbit pattern, <N> PRs, <M>% fix rate"`; the
/// pipeline needs those numbers to apply its render threshold.
/// Returns `(None, None)` when the footer is missing or malformed —
/// the pipeline treats that as zero confidence and routes the rule
/// to overflow.
fn parse_coderabbit_stats(rule_text: &str) -> (Option<f32>, Option<usize>) {
    use std::sync::LazyLock;
    static STATS_RE: LazyLock<Option<regex::Regex>> =
        LazyLock::new(|| regex::Regex::new(r"(?i)(\d+)\s*PRs?,\s*(\d+)\s*%\s*fix\s*rate").ok());
    let Some(re) = STATS_RE.as_ref() else {
        return (None, None);
    };
    let Some(caps) = re.captures(rule_text) else {
        return (None, None);
    };
    let prs = caps.get(1).and_then(|m| m.as_str().parse::<usize>().ok());
    let fix_rate = caps
        .get(2)
        .and_then(|m| m.as_str().parse::<f32>().ok())
        .map(|pct| pct / 100.0);
    (fix_rate, prs)
}

fn rule_text_from_repo_rule(r: &engram_core::registry::RepoRule) -> String {
    let clean = r.rule_text.trim();
    if !clean.is_empty() {
        return clean.to_string();
    }
    if r.rule_id.starts_with("immune_") {
        return format!(
            "File `{}` is immune-flagged. Run `immune_check` before editing.",
            r.file_pattern
        );
    }
    format!(
        "Repo rule `{}` applies to `{}` — review before editing.",
        r.rule_id, r.file_pattern
    )
}

/// Convert a blast-radius complexity breakdown into a short list of
/// "reasons" phrases for the danger-zones line. Only strong signals
/// (score ≥ 5.0) are surfaced.
fn reasons_from_breakdown(
    bd: &crate::services::blast_radius_service::ComplexityBreakdown,
) -> Vec<String> {
    let mut out = Vec::new();
    if bd.dependency_density_score >= 5.0 {
        out.push(format!(
            "hub ({:.0}/10 dependency)",
            bd.dependency_density_score
        ));
    }
    if bd.state_coupling_score >= 5.0 {
        out.push(format!("state ({:.0}/10)", bd.state_coupling_score));
    }
    if bd.sql_concat_score >= 5.0 {
        out.push(format!("SQL ({:.0}/10)", bd.sql_concat_score));
    }
    if bd.handles_clause_score >= 5.0 {
        out.push(format!("events ({:.0}/10)", bd.handles_clause_score));
    }
    if bd.gis_coupling_score >= 5.0 {
        out.push(format!("GIS ({:.0}/10)", bd.gis_coupling_score));
    }
    if bd.script_injection_score >= 5.0 {
        out.push(format!(
            "script-inject ({:.0}/10)",
            bd.script_injection_score
        ));
    }
    out
}

/// Turn a batch of `review_pattern` graph nodes into the per-language
/// map that `produce_claude_md` renders. Drops suppression-kind nodes
/// (wontFix clusters — those feed the antipattern gate's dampener,
/// not the agent's instructions) and skips anything whose metadata
/// blob can't be parsed.
///
/// Composite score matches `ReviewCluster::confidence`: biased toward
/// high fix_rate × PR saturation so a rule caught in 1 PR with 100%
/// fix rate still ranks below a rule caught in 5 PRs with 80% fix
/// rate. Ties broken by fix_commit presence (explicit `✅` signal).
fn build_coderabbit_language_map(
    nodes: &[engram_graph::Node],
) -> std::collections::HashMap<
    String,
    Vec<crate::services::produce_claude_md_service::CodeRabbitRule>,
> {
    use crate::services::produce_claude_md_service::CodeRabbitRule;

    let mut out: std::collections::HashMap<String, Vec<CodeRabbitRule>> =
        std::collections::HashMap::new();
    for n in nodes {
        let Some(meta) = n.metadata.as_ref() else {
            continue;
        };
        // Only positive clusters — suppression clusters aren't
        // guidance, they're anti-guidance and belong in the antipattern
        // gate's dampener path.
        let kind = meta.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        if kind != "pattern" {
            continue;
        }
        let fix_rate = meta.get("fix_rate").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
        let pr_count = meta
            .get("pr_ids")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let fix_commit = meta
            .get("fix_commit")
            .and_then(|v| v.as_str())
            .map(String::from);
        // Composite score: fix_rate is the dominant signal, PR count
        // via log₂(n+1) contributes diminishing returns, fix_commit
        // presence is a small nudge for explicit `✅ Addressed`
        // evidence.
        let pr_term = ((pr_count as f32) + 1.0).log2() / 4.0;
        let commit_term = if fix_commit.is_some() { 0.05 } else { 0.0 };
        let composite_score = (fix_rate * 0.75 + pr_term.min(0.25) + commit_term).clamp(0.0, 1.0);

        let rule = CodeRabbitRule {
            rule_text: n.name.clone(),
            fix_rate,
            pr_count,
            fix_commit,
            composite_score,
        };
        out.entry(n.language.clone()).or_default().push(rule);
    }
    // Sort each bucket by composite score desc so the renderer picks
    // the top-K deterministically.
    for list in out.values_mut() {
        list.sort_by(|a, b| {
            b.composite_score
                .partial_cmp(&a.composite_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    out
}

/// Read `CLAUDE.md` (or `claude.md`) from the project root if present.
fn read_claude_md_if_any(project_dir: &str) -> Option<String> {
    let dir = std::path::Path::new(project_dir);
    for candidate in &["CLAUDE.md", "claude.md", "CLAUDE.MD"] {
        if let Ok(full) = safe_join(dir, candidate) {
            if let Ok(text) = std::fs::read_to_string(&full) {
                return Some(text);
            }
        }
    }
    None
}

/// Inspect the project directory for well-known build/test command
/// conventions. Each entry is emitted as a single line in the
/// `<build>` block. Returns empty when nothing is detected — we never
/// guess.
fn detect_build_commands(project_dir: &str) -> Vec<String> {
    let dir = std::path::Path::new(project_dir);
    let has = |name: &str| dir.join(name).exists();
    let mut cmds = Vec::new();
    if has("Cargo.toml") {
        // Detect workspace vs single-crate and emit the right flag.
        let workspace = std::fs::read_to_string(dir.join("Cargo.toml"))
            .map(|s| s.contains("[workspace]"))
            .unwrap_or(false);
        if workspace {
            cmds.push("cargo build --release --workspace".into());
            cmds.push("cargo test --workspace".into());
        } else {
            cmds.push("cargo build --release".into());
            cmds.push("cargo test".into());
        }
        // Suggest clippy if it's configured.
        if has(".clippy.toml") || has("clippy.toml") {
            cmds.push("cargo clippy --all-targets -- -D warnings".into());
        }
    } else if has("package.json") {
        // Parse the scripts block so we emit `npm run build` /
        // `npm test` only when those scripts actually exist.
        let scripts = std::fs::read_to_string(dir.join("package.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v.get("scripts").cloned())
            .and_then(|v| v.as_object().cloned());
        cmds.push("npm install".into());
        if let Some(map) = scripts {
            for script in ["build", "test", "typecheck", "lint"] {
                if map.contains_key(script) {
                    cmds.push(format!("npm run {script}"));
                }
            }
        } else {
            cmds.push("npm test".into());
        }
    } else if has("pyproject.toml") || has("requirements.txt") {
        // Prefer `uv` when the project has a `uv.lock`.
        if has("uv.lock") {
            cmds.push("uv sync".into());
            cmds.push("uv run pytest".into());
        } else if has("poetry.lock") {
            cmds.push("poetry install".into());
            cmds.push("poetry run pytest".into());
        } else {
            cmds.push("pip install -e .".into());
            cmds.push("pytest".into());
        }
    } else if has("pom.xml") {
        cmds.push("mvn package".into());
        cmds.push("mvn test".into());
    } else if has("build.gradle") || has("build.gradle.kts") {
        let gradlew = has("gradlew") || has("gradlew.bat");
        let prefix = if gradlew { "./gradlew" } else { "gradle" };
        cmds.push(format!("{prefix} build"));
        cmds.push(format!("{prefix} test"));
    } else if has("go.mod") {
        cmds.push("go build ./...".into());
        cmds.push("go test ./...".into());
    } else if has("Makefile") || has("makefile") {
        // Scan Makefile targets and emit the first canonical one.
        let content = std::fs::read_to_string(dir.join("Makefile"))
            .or_else(|_| std::fs::read_to_string(dir.join("makefile")))
            .unwrap_or_default();
        for target in ["build", "all", "test"] {
            if content.contains(&format!("\n{target}:"))
                || content.starts_with(&format!("{target}:"))
            {
                cmds.push(format!("make {target}"));
            }
        }
        if cmds.is_empty() {
            cmds.push("make".into());
        }
    }
    // .NET — find the actual .sln (or .csproj/.vbproj) and cite it.
    // Runs AFTER the other language detectors because a project
    // might be primarily Rust/Node with a tooling .sln floating
    // around.
    if cmds.is_empty() || only_has_dotnet(dir) {
        if let Some(sln_name) = find_first_file_with_ext(dir, "sln") {
            cmds.push(format!(
                "msbuild \"{sln_name}\" /t:Build /p:Configuration=Debug"
            ));
            cmds.push(format!(
                "msbuild \"{sln_name}\" /t:Build /p:Configuration=Release"
            ));
            // Scan for a test project conventionally named
            // `<Something>.Tests.csproj` / `<Something>.Tests.vbproj`.
            if let Some(test_proj) = find_test_project(dir) {
                cmds.push(format!("vstest.console.exe \"{test_proj}\""));
            }
        } else if let Some(csproj) = find_first_file_with_ext(dir, "csproj")
            .or_else(|| find_first_file_with_ext(dir, "vbproj"))
        {
            cmds.push(format!("dotnet build \"{csproj}\""));
            cmds.push("dotnet test".into());
        }
    }
    cmds
}

/// True when the directory primarily contains .NET project files —
/// used to decide whether to also emit dotnet commands when another
/// ecosystem's files are present.
fn only_has_dotnet(dir: &std::path::Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    let mut has_dotnet = false;
    let mut has_other = false;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_ascii_lowercase();
        if name.ends_with(".sln") || name.ends_with(".csproj") || name.ends_with(".vbproj") {
            has_dotnet = true;
        } else if matches!(
            name.as_str(),
            "cargo.toml" | "package.json" | "go.mod" | "pom.xml" | "pyproject.toml"
        ) {
            has_other = true;
        }
    }
    has_dotnet && !has_other
}

fn find_first_file_with_ext(dir: &std::path::Path, ext: &str) -> Option<String> {
    std::fs::read_dir(dir).ok()?.flatten().find_map(|e| {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.to_ascii_lowercase().ends_with(&format!(".{ext}")) {
            Some(name)
        } else {
            None
        }
    })
}

fn find_test_project(dir: &std::path::Path) -> Option<String> {
    for candidate in std::fs::read_dir(dir).ok()?.flatten() {
        let path = candidate.path();
        if path.is_dir() {
            if let Some(p) = find_test_project(&path) {
                return Some(p);
            }
        } else {
            let name = candidate.file_name().to_string_lossy().into_owned();
            let lower = name.to_ascii_lowercase();
            if (lower.ends_with(".csproj") || lower.ends_with(".vbproj"))
                && (lower.contains(".tests.") || lower.contains(".test."))
            {
                return Some(
                    path.strip_prefix(dir)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    None
}

/// Derive a display-friendly project name from the project directory.
fn project_name_from_dir(project_dir: &str) -> String {
    std::path::Path::new(project_dir)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Project".into())
}

/// Result of per-language style gathering.
struct LanguageStyleBundle {
    bullets: Vec<String>,
    sample_files: Vec<String>,
}

/// Pick the top-N most central files in a given language (by
/// precomputed PageRank) and run `static_analyze_file_style` on each.
/// Dedup bullets across samples so the rule file isn't a repetitive
/// dump.
///
/// Both `file_nodes` and `pagerank_map` are precomputed once at the
/// top of the handler and shared across every language, so this
/// function performs no graph queries — just an in-memory filter +
/// rank + N file reads. File reads are issued concurrently via
/// `spawn_blocking` + `join_all` so the total time for, say, three
/// sample files is bounded by the slowest single read rather than
/// the sum.
async fn gather_language_style(
    file_nodes: &[engram_graph::Node],
    pagerank_map: &std::collections::HashMap<String, f32>,
    project_dir: &str,
    language: &str,
    sample_count: usize,
) -> LanguageStyleBundle {
    use crate::services::cognitive_service::static_analyze_file_style;

    // In-memory filter + rank. No graph round-trip.
    let mut ranked: Vec<(&engram_graph::Node, f32)> = file_nodes
        .iter()
        .filter(|n| n.language.eq_ignore_ascii_case(language))
        .map(|n| {
            let r = pagerank_map.get(&n.node_id).copied().unwrap_or(0.0);
            (n, r)
        })
        .collect();
    if ranked.is_empty() {
        return LanguageStyleBundle {
            bullets: Vec::new(),
            sample_files: Vec::new(),
        };
    }
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked.truncate(sample_count);

    // Read the sample files concurrently.
    let read_handles: Vec<_> = ranked
        .iter()
        .map(|(n, _)| {
            let path = n.file_path.as_str().to_string();
            let dir = std::path::PathBuf::from(project_dir);
            let path_for_read = path.clone();
            tokio::task::spawn_blocking(move || {
                let content = match safe_join(&dir, &path_for_read) {
                    Ok(full) => std::fs::read_to_string(full).ok(),
                    Err(_) => None,
                };
                (path, content)
            })
        })
        .collect();

    let mut all_bullets: Vec<String> = Vec::new();
    let mut sample_files: Vec<String> = Vec::new();
    for handle in read_handles {
        let Ok((path, Some(content))) = handle.await else {
            continue;
        };
        sample_files.push(path.clone());
        for b in static_analyze_file_style(&content, &path) {
            if !all_bullets.iter().any(|existing| existing == &b) {
                all_bullets.push(b);
            }
        }
    }

    LanguageStyleBundle {
        bullets: all_bullets,
        sample_files,
    }
}

#[cfg(test)]
mod tests {
    use super::{detect_destructive_patterns, immune_rule_matches_path, path_confidence_score};

    // ── path_confidence_score ───────────────────────────────────────────

    fn node(node_type: &str) -> engram_graph::Node {
        engram_graph::Node {
            node_id: format!("{node_type}:n"),
            node_type: node_type.to_string(),
            name: "n".into(),
            namespace: "memory".into(),
            language: "vb".into(),
            file_path: engram_core::RelPath::new("p.aspx"),
            start_line: 0,
            end_line: 0,
            generation: 1,
            metadata: None,
        }
    }

    #[test]
    fn path_confidence_direct_event_wiring_scores_high() {
        // page → control → function → inline_sql — the textbook
        // direct-event path: one 0.05 Contains penalty, rest are 0.
        let path = [
            node("page"),
            node("control"),
            node("function"),
            node("inline_sql"),
        ];
        let s = path_confidence_score(&path, 0.0);
        assert!(
            (s - 0.95).abs() < 0.001,
            "direct event path should score ~0.95, got {s}"
        );
    }

    #[test]
    fn path_confidence_declarative_data_binding_scores_lower() {
        // page → control(grid) → control(datasource) → function → inline_sql.
        // Extra `control → control` hop adds a 0.25 penalty, so we're
        // at 1.00 - 0.05 (page→control) - 0.25 (control→control) - 0.00
        // (control→function) - 0.00 (→inline_sql) = 0.70.
        let path = [
            node("page"),
            node("control"),
            node("control"),
            node("function"),
            node("inline_sql"),
        ];
        let s = path_confidence_score(&path, 0.0);
        assert!(
            (s - 0.70).abs() < 0.001,
            "data-binding path should score ~0.70, got {s}"
        );
    }

    #[test]
    fn path_confidence_data_binding_lower_than_direct() {
        // The task's central claim: data-binding paths score lower than
        // direct event-wiring paths on the same trace target.
        let direct = [
            node("page"),
            node("control"),
            node("function"),
            node("inline_sql"),
        ];
        let declarative = [
            node("page"),
            node("control"),
            node("control"),
            node("function"),
            node("inline_sql"),
        ];
        assert!(
            path_confidence_score(&direct, 0.0) > path_confidence_score(&declarative, 0.0),
            "direct event path must outrank declarative-binding path"
        );
    }

    #[test]
    fn path_confidence_fallback_penalty_applies() {
        let path = [
            node("page"),
            node("control"),
            node("function"),
            node("inline_sql"),
        ];
        let with_fallback = path_confidence_score(&path, 0.2);
        let without_fallback = path_confidence_score(&path, 0.0);
        assert!(
            with_fallback < without_fallback,
            "fallback penalty must lower the score"
        );
        assert!(
            (without_fallback - with_fallback - 0.2).abs() < 0.001,
            "fallback penalty must subtract exactly the requested amount"
        );
    }

    #[test]
    fn path_confidence_floored_at_ten_percent() {
        // Degenerate long chain of unknown hops — score should floor
        // at 0.10 rather than going negative.
        let path = vec![node("other"); 20];
        let s = path_confidence_score(&path, 0.8);
        assert!(
            (s - 0.10).abs() < 0.001,
            "score must floor at 0.10, got {s}"
        );
    }

    #[test]
    fn path_confidence_trivial_path_is_full() {
        let path = [node("control")];
        assert!((path_confidence_score(&path, 0.0) - 1.0).abs() < 0.001);
    }

    // ── immune_rule_matches_path ─────────────────────────────────────────

    #[test]
    fn immune_rule_matches_exact_path() {
        assert!(immune_rule_matches_path(
            "Site/App_Code/fiberjobb.vb",
            "Site/App_Code/fiberjobb.vb"
        ));
    }

    #[test]
    fn immune_rule_matches_path_ignores_slash_direction() {
        // The rule might have been stored with Windows backslashes but the
        // target path carries forward slashes (or vice versa).
        assert!(immune_rule_matches_path(
            "Site\\App_Code\\fiberjobb.vb",
            "Site/App_Code/fiberjobb.vb"
        ));
    }

    #[test]
    fn immune_rule_matches_path_is_case_insensitive() {
        assert!(immune_rule_matches_path(
            "site/app_code/fiberjobb.vb",
            "Site/App_Code/FiberJobb.vb"
        ));
    }

    #[test]
    fn immune_rule_matches_glob_star() {
        assert!(immune_rule_matches_path(
            "Site/App_Code/*.vb",
            "Site/App_Code/fiberjobb.vb"
        ));
        assert!(immune_rule_matches_path(
            "**/fiberjobb.vb",
            "Site/App_Code/fiberjobb.vb"
        ));
        assert!(!immune_rule_matches_path(
            "Site/App_Code/*.cs",
            "Site/App_Code/fiberjobb.vb"
        ));
    }

    #[test]
    fn immune_rule_matches_plain_substring_without_globs() {
        // Bare patterns without metacharacters fall back to substring match
        // so a rule keyed on `fiberjobb.vb` catches the file regardless of
        // which directory the caller passes.
        assert!(immune_rule_matches_path(
            "fiberjobb.vb",
            "Site/App_Code/fiberjobb.vb"
        ));
    }

    #[test]
    fn immune_rule_empty_pattern_never_matches() {
        assert!(!immune_rule_matches_path("", "anything.vb"));
    }

    // ── detect_destructive_patterns ──────────────────────────────────────

    #[test]
    fn detect_destructive_flags_linq_bulk_delete() {
        let hits = detect_destructive_patterns(
            "db.fj_fiberjobb.DeleteAllOnSubmit(db.fj_fiberjobb.Where(x => x.active))",
        );
        assert!(hits.iter().any(|h| h == "DeleteAllOnSubmit"));
    }

    #[test]
    fn detect_destructive_flags_drop_and_truncate() {
        let hits = detect_destructive_patterns(
            "BEGIN TRANSACTION\nDROP TABLE audit_log\nTRUNCATE TABLE staging\nCOMMIT",
        );
        assert!(hits.iter().any(|h| h == "DROP TABLE"));
        assert!(hits.iter().any(|h| h == "TRUNCATE TABLE"));
    }

    #[test]
    fn detect_destructive_flags_execute_nonquery_with_delete() {
        let hits = detect_destructive_patterns(
            "var cmd = new SqlCommand(\"DELETE FROM orders\", conn); cmd.ExecuteNonQuery();",
        );
        // At least one destructive signal must fire — the regex family
        // catches either the unbounded DELETE or the ExecuteNonQuery+DELETE
        // combo, depending on spacing.
        assert!(
            !hits.is_empty(),
            "destructive regex family must match ExecuteNonQuery with DELETE literal, got: {hits:?}"
        );
    }

    #[test]
    fn detect_destructive_ignores_benign_snippets() {
        let hits = detect_destructive_patterns(
            "Public Function GetUser(id As Integer) As User\n  Return db.Users.FirstOrDefault(Function(u) u.Id = id)\nEnd Function",
        );
        assert!(
            hits.is_empty(),
            "benign read-only snippet must not trigger destructive flags, got: {hits:?}"
        );
    }

    #[test]
    fn detect_destructive_does_not_false_fire_on_identifier_substrings() {
        // `DROPPED_COLUMN_NAME` used as an identifier must not fire the
        // `DROP TABLE` pattern — `\b` boundaries guard against this.
        let hits = detect_destructive_patterns("Dim DROPPED_COLUMN_NAME As String = \"a\"");
        assert!(
            !hits.iter().any(|h| h == "DROP TABLE"),
            "DROPPED_COLUMN_NAME identifier must not trigger DROP TABLE, got: {hits:?}"
        );
    }
}
