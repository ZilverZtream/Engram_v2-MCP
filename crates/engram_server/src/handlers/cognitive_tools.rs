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
use engram_index::HybridQuery;
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use std::path::PathBuf;

/// (node_id, node_type, name, file_path) tuple used in centrality reranking.
type NodeMetaTuple = (String, Option<String>, Option<String>, Option<String>);

/// Strip a trailing `:<digits>` line-suffix that VB metadata can append to FQNs.
fn strip_fqn_line_suffix(s: &str) -> &str {
    if let Some((head, tail)) = s.rsplit_once(':')
        && !tail.is_empty()
        && tail.bytes().all(|b| b.is_ascii_digit())
    {
        return head;
    }
    s
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
                    let table_id = engram_core::ids::NodeId::table(fqn).0;
                    if graph
                        .get_node(&req.project_id, &table_id)
                        .map_err(|e| e.to_string())?
                        .is_some()
                    {
                        table_id
                    } else {
                        let short = fqn.split('.').next_back().unwrap_or(fqn);
                        if let Ok(candidates) =
                            graph.query_nodes(&req.project_id, None, Some(short), None, 500)
                        && !candidates.is_empty()
                        {
                            let want = strip_fqn_line_suffix(fqn);
                            if let Some(exact) = candidates.iter().find(|n| {
                                n.metadata
                                    .as_ref()
                                    .and_then(|m| m.get("fqn"))
                                    .and_then(|v| v.as_str())
                                    .map(strip_fqn_line_suffix)
                                    == Some(want)
                            }) {
                                exact.node_id.clone()
                            } else {
                                let suggestions: Vec<String> = candidates
                                    .iter()
                                    .take(5)
                                    .map(|n| {
                                        let fqn_meta = n
                                            .metadata
                                            .as_ref()
                                            .and_then(|m| m.get("fqn"))
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("(no fqn)");
                                        format!(
                                            "  - {} [{}] fqn={}",
                                            n.node_id, n.node_type, fqn_meta
                                        )
                                    })
                                    .collect();
                                return Ok(format!(
                                    "Symbol '{fqn}' not uniquely resolvable. {} name-substring candidates found:\n{}\n\nUse the full node_id as symbol_fqn, or use find_symbol_references for non-unique matches.",
                                    candidates.len(),
                                    suggestions.join("\n")
                                ));
                            }
                        } else {
                            return Ok(format!("Symbol '{fqn}' not found in graph."));
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
            let incoming = graph
                .find_incoming_edges_with_kind(&req.project_id, None, &target_id, capped_limit)
                .map_err(|e| e.to_string())?;

            if incoming.is_empty() {
                return Ok(format!("No dependent nodes found for {target_id}."));
            }
            let incoming_edge_count = incoming.len();

            let mut out = format!("Impact Analysis for {target_id}:\n\n");
            out.push_str("Nodes that depend on or are related to this:\n");

            let mut grouped: std::collections::HashMap<String, (Vec<engram_graph::EdgeKind>, u32)> =
                std::collections::HashMap::new();
            for (src_id, kind, weight) in incoming {
                let entry = grouped.entry(src_id).or_insert((Vec::new(), 0));
                entry.0.push(kind);
                if weight > entry.1 {
                    entry.1 = weight;
                }
            }

            let mut sorted: Vec<_> = grouped.into_iter().collect();
            sorted.sort_by(|a, b| b.1.1.cmp(&a.1.1));

            tracing::info!(
                project_id = %req.project_id,
                target_id = %target_id,
                incoming_edge_count = incoming_edge_count,
                grouped_source_count = sorted.len(),
                "impact_analysis: pre-render counts"
            );

            let mut unresolved_count = 0usize;
            for (src_id, (kinds, weight)) in sorted {
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

                out.push_str(&format!(
                    "- {} [{}] (weight: {weight}) - {reason_str}\n",
                    display_id, display_type
                ));
            }

            if unresolved_count > 0 {
                out.push_str(&format!(
                    "\n(Note: {unresolved_count} source edges pointed at node_ids with no persisted node record. \
                     This indicates an indexing integrity issue — edges were created but corresponding nodes \
                     were not. The entries above are still real dependencies.)\n"
                ));
            }

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
                            "- {} [{}] in {} (weight: {})\n",
                            node.name,
                            node.node_type,
                            node.file_path.as_str(),
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
                            "- {} [{}] in {} (weight: {})\n",
                            node.name,
                            node.node_type,
                            node.file_path.as_str(),
                            weight
                        ));
                    } else {
                        out.push_str(&format!("- {} (weight: {})\n", reader_id, weight));
                    }
                }
            } else {
                out.push_str("### Readers\nNo readers found.\n");
            }

            Ok(out)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(out)]))
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
        if self
            .state
            .graph
            .get_node(&req.project_id, &start_id)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .is_none()
            && let Some(ref ctrl) = req.control_id
            && let Ok(candidates) =
                self.state
                    .graph
                    .query_nodes(&req.project_id, Some("control"), Some(ctrl), None, 10)
            && !candidates.is_empty()
        {
            trace_used_fallback = true;
            trace_candidate_count = candidates.len();
            unresolved_candidates = candidates.iter().map(|n| n.node_id.clone()).collect();
            start_id = candidates[0].node_id.clone();
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
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "No paths found from {start_id} to any SQL nodes within {} hops.",
                req.max_hops
            ))]));
        }

        let mut out = format!("Found {} path(s) to SQL:\n", paths.len());

        let confidence_penalty = if trace_used_fallback {
            (trace_candidate_count as f64 * 0.2).min(0.8)
        } else {
            0.0
        };

        out.push_str("\n## Trace Provenance\n");
        out.push_str(&format!("trace_used_fallback: {}\n", trace_used_fallback));
        out.push_str(&format!(
            "trace_candidate_count: {}\n",
            trace_candidate_count
        ));
        out.push_str(&format!(
            "trace_confidence_penalty: {:.2}\n",
            confidence_penalty
        ));
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
            out.push_str("\n### Follow-up Probes\n");
            out.push_str("- Provide explicit `handler_fqn` to disambiguate\n");
            out.push_str("- Verify control ID uniqueness across master/user controls\n");
            out.push_str("- Check code-behind inheritance chain for handler shadowing\n");
        }

        for (i, path) in paths.iter().enumerate() {
            out.push_str(&format!("\n## Path #{}\n", i + 1));
            for (step, node) in path.iter().enumerate() {
                let label = match node.node_type.as_str() {
                    "page" => "ASPX Page",
                    "control" => "UI Control",
                    "function" => "Code-Behind Handler",
                    "stored_proc" => "Stored Procedure",
                    "inline_sql" => "Inline SQL",
                    _ => &node.node_type,
                };

                let justification = if step == 0 {
                    "Starting point".to_string()
                } else {
                    let prev = &path[step - 1];
                    match (prev.node_type.as_str(), node.node_type.as_str()) {
                        ("page", "class") => "Inherits class".to_string(),
                        ("control", "function") => "Event wiring (OnClick/Handles)".to_string(),
                        ("function", "function") => "Method call".to_string(),
                        (_, "inline_sql") | (_, "stored_proc") => "Executes SQL".to_string(),
                        _ => "Dependency".to_string(),
                    }
                };

                let evidence = format!(
                    "node_type={}, file={}, lines={}-{}",
                    node.node_type,
                    node.file_path.as_str(),
                    node.start_line,
                    node.end_line
                );

                let indent = "  ".repeat(step);
                out.push_str(&format!(
                    "{indent}Step {}: {} [{}] ({}) - {} | evidence: {}\n",
                    step + 1,
                    node.name,
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

        Ok(CallToolResult::success(vec![Content::text(format!(
            "AJAX regions for {}",
            result.file_path
        ))]))
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
                let language = crate::services::business_logic_service::detect_language(&content);
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

                let Some((body, _start, _end, _lines)) = body_opt else {
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
                )
                .await;

                if p.output_json {
                    let json = serde_json::to_string_pretty(&result)
                        .unwrap_or_else(|e| format!("JSON error: {e}"));
                    return Ok(CallToolResult::success(vec![Content::text(json)]));
                }
                return Ok(CallToolResult::success(vec![Content::text(
                    crate::services::business_logic_service::render_method_as_doc(&result),
                )]));
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

            if p.output_json {
                let json = serde_json::to_string_pretty(&file_logic)
                    .unwrap_or_else(|e| format!("JSON error: {e}"));
                return Ok(CallToolResult::success(vec![Content::text(json)]));
            }

            let mut md = format!(
                "# Business Logic — {}\n\n*{}*\n\n- Methods analyzed: {analyzed}\n- Cached (skipped): {skipped}\n\n",
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

        if p.output_json {
            let json = serde_json::to_string_pretty(&report)
                .unwrap_or_else(|e| format!("JSON error: {e}"));
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let md = crate::services::business_logic_service::render_compact_markdown(&report);
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
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Hits: {}",
            hits.len()
        ))]))
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

        let mut out = format!(
            "# Immune Check Result\n\n**Matches Found**: {}\n\n",
            hits.len()
        );

        let mut highest_score = 0.0;
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

        let status = if highest_score > 0.8 {
            "🔴 BLOCKED"
        } else if highest_score > warn_t {
            "🟡 WARNING"
        } else {
            "🟢 CLEAN"
        };

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
                let candidates = [format!("file:{entry_raw}")];
                let mut found = None;
                for cand in &candidates {
                    if graph
                        .get_node(&project_id, cand)
                        .map_err(|e| e.to_string())?
                        .is_some()
                    {
                        found = Some(cand.clone());
                        break;
                    }
                }
                if found.is_none()
                    && let Ok(nodes) = graph.query_nodes(&project_id, None, None, None, 2000)
                {
                    found = nodes
                        .into_iter()
                        .find(|n| {
                            n.metadata
                                .as_ref()
                                .and_then(|m| m.get("fqn"))
                                .and_then(|v| v.as_str())
                                .is_some_and(|fqn| fqn == entry_raw)
                        })
                        .map(|n| n.node_id);
                }
                if found.is_none() {
                    let nodes = graph
                        .query_nodes(&project_id, None, Some(&entry_raw), None, 10)
                        .map_err(|e| e.to_string())?;
                    if let Some(n) = nodes.first() {
                        found = Some(n.node_id.clone());
                    }
                }
                found.ok_or_else(|| {
                    format!(
                        "No node found matching '{}'. Try query_graph_nodes to discover node IDs.",
                        entry_raw
                    )
                })?
            };

            let edge_kinds: Vec<EdgeKind> = if compile_time_only {
                vec![EdgeKind::Dependency, EdgeKind::Imports, EdgeKind::Contains]
            } else {
                EdgeKind::ALL.to_vec()
            };

            // Direction enum: exhaustive match — no silent fallback possible.
            let graph_direction = direction.as_str();

            let traversal = graph
                .traverse(
                    &project_id,
                    &entry_node_id,
                    max_depth,
                    Some(edge_kinds),
                    graph_direction,
                )
                .map_err(|e| e.to_string())?;

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
                }))
                .map_err(|e| e.to_string())
            } else {
                let mut tree = format!("# AST Dependency Tree: {}\n\n", entry_node_id);
                for (node, depth) in traversal {
                    let indent = "  ".repeat(depth);
                    tree.push_str(&format!(
                        "{indent}- {} [{}] ({})\n",
                        node.name,
                        node.node_type,
                        node.file_path.as_str()
                    ));
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
                // Step 1: exact node_id path
                if (fqn_c.starts_with("sym:")
                    || fqn_c.starts_with("file:")
                    || fqn_c.starts_with("page:"))
                    && graph.get_node(&pid, &fqn_c).ok().flatten().is_some()
                {
                    return Some(fqn_c.clone());
                }

                // Step 2: exact match against node.name (canonical VB FQN location)
                if let Ok(nodes) = graph.query_nodes(&pid, None, Some(&fqn_c), None, 100) {
                    if let Some(node) = nodes.iter().find(|n| n.name == fqn_c) {
                        return Some(node.node_id.clone());
                    }
                    if nodes.len() == 1 {
                        return Some(nodes[0].node_id.clone());
                    }
                }

                // Step 3: legacy metadata.fqn exact match
                if let Ok(nodes) = graph.query_nodes(&pid, None, None, None, 5000)
                    && let Some(node) = nodes.into_iter().find(|n| {
                        n.metadata
                            .as_ref()
                            .and_then(|m| m.get("fqn"))
                            .and_then(|v| v.as_str())
                            .is_some_and(|node_fqn| node_fqn == fqn_c)
                    })
                {
                    return Some(node.node_id);
                }

                // Step 4: short-name fallback with disambiguation
                let short = fqn_c.split('.').next_back().unwrap_or(&fqn_c);
                if let Ok(nodes) = graph.query_nodes(&pid, None, Some(short), None, 50) {
                    if let Some(node) = nodes.iter().find(|n| n.name == short) {
                        return Some(node.node_id.clone());
                    }
                    if nodes.len() == 1 {
                        return Some(nodes[0].node_id.clone());
                    }
                }

                None
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            found.ok_or_else(|| {
                McpError::invalid_params(
                    format!(
                        "Could not resolve symbol_fqn '{}' to a graph node. Try passing the full node_id (e.g. 'sym:function:path/to/file.vb:ClassName.MethodName:LINE'), or use 'file:path/to/file' for file-level analysis.",
                        fqn
                    ),
                    None,
                )
            })?
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

        let report = tokio::task::spawn_blocking(move || {
            crate::services::blast_radius_service::compute_blast_radius(
                &graph,
                &pid,
                &target_id,
                gen_,
                include_guidance,
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let mut out = format!(
            "# Blast Radius Analysis: {}\n\n\
             **Overall Risk**: {}/10 ({})\n\
             **Total Downstream**: {} (incoming: {}, outgoing: {})\n",
            report.target,
            report.migration_risk,
            report.risk_band,
            report.total_downstream,
            report.total_incoming,
            report.total_outgoing
        );

        if !report.guidance.is_empty() {
            out.push_str("\n## Migration Guidance\n");
            for g in &report.guidance {
                out.push_str(&format!(
                    "- **[{}]** {}: {}\n",
                    g.severity, g.concern, g.recommendation
                ));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
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
