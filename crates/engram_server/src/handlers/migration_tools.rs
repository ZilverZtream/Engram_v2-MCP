use crate::models::{
    AnalyzeDatabaseIntelligenceRequest, AnalyzeFullProjectMigrationRequest,
    AnalyzeSyncHazardsRequest, AnalyzeViewStateDepsRequest, CheckMigrationCoverageRequest,
    GenerateCharacterizationTestsRequest, GenerateInstrumentationCodeRequest,
    GenerateMigrationBlueprintRequest, GenerateMigrationPlanRequest,
    GenerateMigrationScaffoldRequest, GenerateStranglerFigRequest, GetInstrumentationPackRequest,
    GetJQueryInventoryRequest, GetMigrationDossierRequest, GetMigrationProgressRequest,
    GetSpDetailsRequest, IngestInstrumentationLogsRequest, ListTriggersRequest,
    MapAuthConfigRequest, MapPageLifecycleRequest, MapValidationControlsRequest, MinSeverity,
    ReconcileRuntimeEvidenceRequest, SuggestMigrationBoundariesRequest,
    SuggestMigrationOrderRequest, SuggestStateMigrationRequest, TraceDataFlowRequest,
    UpdateMigrationStatusRequest,
};
use crate::services::{full_project_migration_service as full_mig, graph_service};
use crate::tools::Engram;
use crate::utils::files::{
    discover_files_recursive, find_aspx_for_codebehind, find_codebehind_path,
};
use engram_core::safe_join;
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use std::path::Path;

impl Engram {
    pub async fn handle_suggest_migration_boundaries(
        &self,
        req: SuggestMigrationBoundariesRequest,
    ) -> Result<CallToolResult, McpError> {
        let _ = self.ensure_project_record(&req.project_id).await?;

        let boundaries = self
            .cognitive_suggest_boundaries(
                &req.project_id,
                req.sanitized_min_frequency(),
                req.sanitized_max_clusters(),
                req.sanitized_timeout_secs(),
                req.include_cross_cluster_deps,
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if boundaries.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No temporal coupling data found. Index git history first (index_git_history) to populate coupling edges.",
            )]));
        }

        if req.output_json {
            let json = serde_json::to_string_pretty(&boundaries)
                .unwrap_or_else(|_| format!("{boundaries:?}"));
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let mut out = String::with_capacity(4096);
        out.push_str(&format!(
            "Migration Boundary Suggestions ({} contexts)\n",
            boundaries.len()
        ));
        out.push_str(&format!(
            "parameters: min_frequency={}, max_clusters={}, timeout={}s\n\n",
            req.sanitized_min_frequency(),
            req.sanitized_max_clusters(),
            req.sanitized_timeout_secs(),
        ));

        for (i, b) in boundaries.iter().enumerate() {
            out.push_str(&format!("--- Context {}: {} ---\n", i + 1, b.context_name));
            out.push_str(&format!("  risk: {}\n", b.risk));
            out.push_str(&format!(
                "  files ({}): {}\n",
                b.files.len(),
                b.files.join(", ")
            ));
            if !b.owned_data.is_empty() {
                out.push_str(&format!("  owned_data: {}\n", b.owned_data.join(", ")));
            }
            if !b.depends_on.is_empty() {
                out.push_str(&format!("  depends_on: {}\n", b.depends_on.join(", ")));
            }
            if !b.seam_files.is_empty() {
                out.push_str(&format!("  seam_files: {}\n", b.seam_files.join(", ")));
            }
            if !b.shared_across.is_empty() {
                out.push_str(&format!(
                    "  shared_data_with: {}\n",
                    b.shared_across.join(", ")
                ));
            }
            out.push('\n');
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_generate_migration_blueprint(
        &self,
        req: GenerateMigrationBlueprintRequest,
    ) -> Result<CallToolResult, McpError> {
        let max_depth = req.sanitized_max_depth();
        let output_json = req.output_json;
        let graph = self.state.graph.clone();
        let project_id = req.project_id.clone();
        let entry_raw = req.entry_node.clone();

        let out = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let entry_node_id = if graph
                .get_node(&project_id, &entry_raw)
                .map_err(|e| e.to_string())?
                .is_some()
            {
                entry_raw.clone()
            } else {
                let candidates = [
                    format!("file:{entry_raw}"),
                    format!("sym:class:{entry_raw}"),
                    format!("sym:function:{entry_raw}"),
                    format!("page:{entry_raw}"),
                    format!("control:{entry_raw}"),
                ];
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
                if found.is_none() {
                    let nodes = graph
                        .query_nodes(&project_id, None, Some(&entry_raw), None, 10)
                        .map_err(|e| e.to_string())?;
                    if let Some(n) = nodes.first() {
                        found = Some(n.node_id.clone());
                    }
                }
                match found {
                    Some(id) => id,
                    None => {
                        return Err(format!(
                            "No node found matching '{}'. Try query_graph_nodes to discover node IDs.",
                            entry_raw
                        ));
                    }
                }
            };

            let slice = graph_service::compile_migration_slice(
                &graph,
                &project_id,
                &entry_node_id,
                max_depth,
            )
            .map_err(|e| e.to_string())?;

            if output_json {
                serde_json::to_string_pretty(&serde_json::json!({
                    "entry_node_id": slice.entry_node_id,
                    "entry_node_type": slice.entry_node_type,
                    "entry_file": slice.entry_file,
                    "nodes_visited": slice.nodes_visited,
                    "dead_code_skipped": slice.dead_code_skipped,
                    "frontend_deps": slice.frontend_deps.iter().map(|s| serde_json::json!({
                        "node_id": s.node_id, "node_type": s.node_type,
                        "file_path": s.file_path, "edge_kind": s.edge_kind, "depth": s.depth
                    })).collect::<Vec<_>>(),
                    "backend_methods": slice.backend_methods.iter().map(|s| serde_json::json!({
                        "node_id": s.node_id, "node_type": s.node_type,
                        "file_path": s.file_path, "edge_kind": s.edge_kind, "depth": s.depth
                    })).collect::<Vec<_>>(),
                    "state_mutations": slice.state_mutations.iter().map(|s| serde_json::json!({
                        "node_id": s.node_id, "node_type": s.node_type,
                        "file_path": s.file_path, "edge_kind": s.edge_kind, "depth": s.depth
                    })).collect::<Vec<_>>(),
                    "database_deps": slice.database_deps.iter().map(|s| serde_json::json!({
                        "node_id": s.node_id, "node_type": s.node_type,
                        "file_path": s.file_path, "edge_kind": s.edge_kind, "depth": s.depth
                    })).collect::<Vec<_>>(),
                    "component_deps": slice.component_deps.iter().map(|s| serde_json::json!({
                        "node_id": s.node_id, "node_type": s.node_type,
                        "file_path": s.file_path, "edge_kind": s.edge_kind, "depth": s.depth
                    })).collect::<Vec<_>>(),
                    "data_bindings": slice.data_bindings,
                    "config_deps": slice.config_deps.iter().map(|s| serde_json::json!({
                        "node_id": s.node_id, "node_type": s.node_type,
                        "file_path": s.file_path, "edge_kind": s.edge_kind, "depth": s.depth
                    })).collect::<Vec<_>>(),
                    "lifecycle_info": slice.lifecycle_info.iter().map(|(id, stage, seq)| {
                        serde_json::json!({"node_id": id, "stage": stage, "sequence": seq})
                    }).collect::<Vec<_>>(),
                    "side_effects": slice.side_effects.iter().map(|(id, fx)| {
                        serde_json::json!({"node_id": id, "effects": fx})
                    }).collect::<Vec<_>>(),
                })).map_err(|e| e.to_string())
            } else {
                Ok(graph_service::format_migration_blueprint(&slice))
            }
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_generate_migration_plan(
        &self,
        req: GenerateMigrationPlanRequest,
    ) -> Result<CallToolResult, McpError> {
        use crate::services::migration_service as mig;

        let pid = req.project_id.clone();
        let _ps = self.ensure_project_runtime(&pid).await?;

        let graph = self.state.graph.clone();
        let pid2 = pid.clone();
        let now = crate::utils::now_ms();
        let plan = tokio::task::spawn_blocking(move || -> anyhow::Result<mig::MigrationPlan> {
            let file_nodes = graph
                .query_nodes(&pid2, Some("file"), None, None, 5000)
                .unwrap_or_default();
            let db_files: Vec<String> = graph
                .query_nodes(&pid2, Some("db_table"), None, None, 1000)
                .unwrap_or_default()
                .iter()
                .map(|n| n.name.clone())
                .collect();
            let global_files: Vec<String> = graph
                .query_nodes(&pid2, Some("global_state"), None, None, 1000)
                .unwrap_or_default()
                .iter()
                .map(|n| n.name.clone())
                .collect();

            let mut dir_clusters: std::collections::HashMap<String, Vec<String>> =
                std::collections::HashMap::new();
            for node in &file_nodes {
                let dir = if let Some(pos) = node.name.rfind('/').or_else(|| node.name.rfind('\\'))
                {
                    node.name[..pos].to_string()
                } else {
                    "root".to_string()
                };
                dir_clusters.entry(dir).or_default().push(node.name.clone());
            }

            let boundaries: Vec<mig::BoundaryCluster> = dir_clusters
                .into_iter()
                .enumerate()
                .map(|(i, (dir, files))| mig::BoundaryCluster {
                    cluster_id: format!("cluster_{}", i),
                    name: dir,
                    files,
                    internal_edges: 0,
                    shared_across: vec![],
                })
                .collect();

            let input = mig::PlanInput {
                project_id: pid2,
                boundaries,
                cross_boundary_edges: vec![],
                global_state_files: global_files,
                database_files: db_files,
                timestamp_ms: now,
                solution_structure: None,
            };
            Ok(mig::generate_migration_plan(&input))
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if req.output_json {
            let json = serde_json::to_string_pretty(&plan)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            Ok(CallToolResult::success(vec![Content::text(json)]))
        } else {
            let mut out = String::with_capacity(8192);
            out.push_str(&format!(
                "Migration Plan for {} ({} waves)\n",
                plan.project_id, plan.total_waves
            ));
            out.push_str(&format!("Generated at: {}\n", plan.generated_at_ms));
            out.push_str(&format!(
                "Risk: {} high-risk items, {} total items\n\n",
                plan.risk_summary.high_risk_items, plan.risk_summary.total_items,
            ));
            for wave in &plan.waves {
                out.push_str(&format!(
                    "=== Wave {} — {} (risk: {:?}, effort: {}) ===\n",
                    wave.wave_number, wave.name, wave.risk_level, wave.estimated_effort
                ));
                out.push_str(&format!("{}\n", wave.description));
                if !wave.depends_on.is_empty() {
                    out.push_str(&format!("  Depends on waves: {:?}\n", wave.depends_on));
                }
                for item in &wave.items {
                    out.push_str(&format!(
                        "  - {} ({:?}, {:?})\n",
                        item.path, item.item_type, item.complexity
                    ));
                }
                if !wave.contract_tests.is_empty() {
                    out.push_str(&format!(
                        "  Contract tests: {}\n",
                        wave.contract_tests.len()
                    ));
                }
                if !wave.adapters.is_empty() {
                    out.push_str(&format!("  Adapters: {}\n", wave.adapters.len()));
                }
                out.push('\n');
            }
            if !plan.seams.is_empty() {
                out.push_str(&format!("--- Seams ({}) ---\n", plan.seams.len()));
                for seam in &plan.seams {
                    out.push_str(&format!(
                        "  {} <-> {} ({:?}): {}\n",
                        seam.legacy_endpoint, seam.modern_endpoint, seam.seam_type, seam.contract
                    ));
                }
            }
            if !plan.rollback_playbook.waves.is_empty() {
                out.push_str(&format!(
                    "\n--- Rollback Playbook ({} waves) ---\n",
                    plan.rollback_playbook.waves.len()
                ));
                for rb in &plan.rollback_playbook.waves {
                    out.push_str(&format!(
                        "  Wave {}: {} steps\n",
                        rb.wave_number,
                        rb.steps.len()
                    ));
                }
            }
            Ok(CallToolResult::success(vec![Content::text(
                out.trim().to_string(),
            )]))
        }
    }

    pub async fn handle_generate_migration_scaffold(
        &self,
        req: GenerateMigrationScaffoldRequest,
    ) -> Result<CallToolResult, McpError> {
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let file_path = req.file_path.clone();
        let target = req.target_stack.as_str().to_owned();
        let include_tests = req.include_test_scaffold;
        let format = req.output_format.clone();

        let result = tokio::task::spawn_blocking(move || {
            crate::services::scaffold_service::generate_scaffold(
                &graph,
                &pid,
                &file_path,
                &target,
                include_tests,
                &format,
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let mut out = format!("# Migration Scaffold ({})\n\n", result.target_stack);
        out.push_str("## Component Code\n```\n");
        out.push_str(&result.component_code);
        out.push_str("```\n\n");

        if let Some(ref repo) = result.repository_interface {
            out.push_str("## Repository Interface\n```csharp\n");
            out.push_str(repo);
            out.push_str("```\n\n");
        }
        if let Some(ref dto) = result.dto_classes {
            out.push_str("## DTO Classes\n```csharp\n");
            out.push_str(dto);
            out.push_str("```\n\n");
        }
        if let Some(ref test) = result.test_scaffold {
            out.push_str("## Test Scaffold\n```\n");
            out.push_str(test);
            out.push_str("```\n\n");
        }

        if !result.mapping_report.is_empty() {
            out.push_str("## Mapping Report\n");
            for entry in &result.mapping_report {
                out.push_str(&format!(
                    "- **{}** → {} [{}] {}\n",
                    entry.legacy_element, entry.modern_element, entry.category, entry.notes
                ));
            }
        }

        if !result.warnings.is_empty() {
            out.push_str("\n## Warnings\n");
            for w in &result.warnings {
                out.push_str(&format!("- {w}\n"));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_generate_instrumentation_code(
        &self,
        req: GenerateInstrumentationCodeRequest,
    ) -> Result<CallToolResult, McpError> {
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let files = req.target_files.clone();
        let lang = req.language.clone();

        let result = tokio::task::spawn_blocking(move || {
            crate::services::instrumentation_service::generate_instrumentation_code(
                &graph, &pid, &files, &lang,
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let mut out = String::from("# Runtime Instrumentation Package\n\n");

        out.push_str("## C# Module\n```csharp\n");
        out.push_str(&result.csharp_module);
        out.push_str("```\n\n");

        out.push_str("## VB.NET Module\n```vbnet\n");
        out.push_str(&result.vb_module);
        out.push_str("```\n\n");

        if let Some(ref wrapper) = result.session_wrapper {
            out.push_str(
                "## Session State Wrapper (InstrumentedSessionStateWrapper.cs)\n```csharp\n",
            );
            out.push_str(wrapper);
            out.push_str("```\n\n");
        }

        if let Some(ref wrapper) = result.sql_wrapper {
            out.push_str("## SQL Command Wrapper (InstrumentedDbCommand.cs)\n```csharp\n");
            out.push_str(wrapper);
            out.push_str("```\n\n");
        }

        out.push_str("## web.config Entries\n```xml\n");
        out.push_str(&result.webconfig_entries);
        out.push_str("```\n\n");

        out.push_str("## Captured Events\n");
        for evt in &result.captured_events {
            out.push_str(&format!("- {evt}\n"));
        }

        out.push_str("\n## Installation Steps\n");
        for step in &result.installation_steps {
            out.push_str(&format!("{step}\n"));
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_get_instrumentation_pack(
        &self,
        req: GetInstrumentationPackRequest,
    ) -> Result<CallToolResult, McpError> {
        let lang = req.language.to_lowercase();

        let (snippet, instructions) = match lang.as_str() {
            "csharp" | "cs" => {
                let s = r#"protected void LogEngramEvent(...) { ... }"#;
                let i = "1. Add to Global.asax.cs...";
                (s, i)
            }
            _ => {
                return Err(McpError::invalid_params("Unsupported language", None));
            }
        };

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Snippet:\n{snippet}\n\nInstructions:\n{instructions}"
        ))]))
    }

    pub async fn handle_ingest_instrumentation_logs(
        &self,
        req: IngestInstrumentationLogsRequest,
    ) -> Result<CallToolResult, McpError> {
        let _ps = self.ensure_project_runtime(&req.project_id).await?;
        let active_gen = self.get_active_generation(&req.project_id).await?;

        let graph = self.state.graph.clone();
        let project_id = req.project_id.clone();

        let count = tokio::task::spawn_blocking(move || -> anyhow::Result<usize> {
            let mut edge_batch: Vec<(engram_graph::EdgeKind, String, String, u32)> = Vec::new();
            for line in req.log_content.lines() {
                if !line.contains("ENGRAM_LOG|") {
                    continue;
                }
                let Some(log_part) = line.split("ENGRAM_LOG|").nth(1).filter(|s| !s.is_empty())
                else {
                    continue;
                };
                let parts: Vec<&str> = log_part.split('|').collect();
                if parts.len() < 5 {
                    continue;
                }

                let _timestamp = parts[0];
                let path = parts[1];
                let _event_name = parts[2];
                let control_id = parts[3];
                let sql_hash = parts[4];

                let rel_path = path
                    .trim_start_matches("~/")
                    .trim_start_matches('/')
                    .trim_start_matches('\\');

                let safe = engram_core::RelPath::new(rel_path);
                if safe.is_empty() {
                    tracing::warn!(path = %path, "Rejecting instrumentation log line with empty normalized path");
                    continue;
                }
                let rel_path = safe.as_str();

                let source_id = if !control_id.is_empty() {
                    engram_core::ids::NodeId::control(rel_path, control_id).0
                } else {
                    engram_core::ids::NodeId::page(rel_path).0
                };

                if !sql_hash.is_empty() {
                    let target_id = format!("sql:inline:{}", sql_hash);
                    edge_batch.push((
                        engram_graph::EdgeKind::SqlCalls,
                        source_id,
                        target_id,
                        1,
                    ));
                }
            }
            let edges_added = edge_batch.len();
            if !edge_batch.is_empty() {
                graph.batch_increment_edges(
                    &project_id,
                    engram_core::namespaces::NAMESPACE_HISTORY,
                    "text",
                    active_gen,
                    &edge_batch,
                )?;
            }
            Ok(edges_added)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "✅ Ingested logs, added {} runtime SQL call edges.",
            count
        ))]))
    }

    pub async fn handle_trace_data_flow(
        &self,
        req: TraceDataFlowRequest,
    ) -> Result<CallToolResult, McpError> {
        let rec = self.ensure_project_record(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let file_path = req.file_path.clone();
        let entry_point = req.entry_point.clone();

        let cb_full = safe_join(Path::new(&rec.directory), &file_path)
            .map_err(|e| McpError::internal_error(format!("Path validation: {e}"), None))?;
        let cb_content = tokio::fs::read_to_string(&cb_full).await.map_err(|e| {
            McpError::internal_error(format!("Failed to read {}: {e}", cb_full.display()), None)
        })?;

        let result = tokio::task::spawn_blocking(move || {
            crate::services::data_flow_service::trace_data_flow(
                &graph,
                &pid,
                &file_path,
                &entry_point,
                &cb_content,
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Trace: {:?}",
            result.steps
        ))]))
    }

    pub async fn handle_get_migration_dossier(
        &self,
        req: GetMigrationDossierRequest,
    ) -> Result<CallToolResult, McpError> {
        let rec = self.ensure_project_record(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let file_path = req.file_path.clone();
        let target_stack = req.target_stack.as_str().to_owned();
        let project_dir = rec.directory.clone();

        let aspx_full = safe_join(Path::new(&project_dir), &file_path)
            .map_err(|e| McpError::internal_error(format!("Path validation: {e}"), None))?;
        let aspx_content = tokio::fs::read_to_string(&aspx_full).await.map_err(|e| {
            McpError::internal_error(format!("Failed to read {}: {e}", aspx_full.display()), None)
        })?;

        let cb_path = find_codebehind_path(&aspx_full);
        let cb_content = if let Some(ref p) = cb_path {
            tokio::fs::read_to_string(p).await.unwrap_or_default()
        } else {
            String::new()
        };

        let result = tokio::task::spawn_blocking(move || {
            crate::services::dossier_service::build_migration_dossier(
                &graph,
                &pid,
                &file_path,
                &aspx_content,
                &cb_content,
                None,
                &target_stack,
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Dossier for {}",
            result.file_path
        ))]))
    }

    pub async fn handle_check_migration_coverage(
        &self,
        req: CheckMigrationCoverageRequest,
    ) -> Result<CallToolResult, McpError> {
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let original_file = req.original_file.clone();
        let modern_code = req.modern_code.clone();

        let result = tokio::task::spawn_blocking(move || {
            crate::services::coverage_service::check_migration_coverage(
                &graph,
                &pid,
                &original_file,
                &modern_code,
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Coverage: {:.1}%",
            result.coverage_score * 100.0
        ))]))
    }

    pub async fn handle_update_migration_status(
        &self,
        req: UpdateMigrationStatusRequest,
    ) -> Result<CallToolResult, McpError> {
        let _ = self.ensure_project_record(&req.project_id).await?;
        let store = self.state.migration_progress.clone();
        let pid = req.project_id.clone();
        let fp = req.file_path.clone();
        let notes = req.notes.clone();
        let risk = req.risk_score;
        let blocked_reason = req.blocked_reason.clone();
        let blocking_deps = req.blocking_dependencies.clone();

        let status = match req.status.to_lowercase().as_str() {
            "not_started" => {
                crate::services::migration_progress_service::MigrationStatus::NotStarted
            }
            "in_progress" => {
                crate::services::migration_progress_service::MigrationStatus::InProgress
            }
            "migrated" => crate::services::migration_progress_service::MigrationStatus::Migrated,
            "verified" => crate::services::migration_progress_service::MigrationStatus::Verified,
            "blocked" => crate::services::migration_progress_service::MigrationStatus::Blocked,
            other => {
                return Err(McpError::invalid_params(
                    format!(
                        "Invalid status '{other}'. Must be one of: not_started, in_progress, migrated, verified, blocked"
                    ),
                    None,
                ));
            }
        };

        let fp_for_msg = fp.clone();
        tokio::task::spawn_blocking(move || {
            store.update_status(
                &pid,
                &fp,
                status,
                &notes,
                risk,
                blocked_reason.as_deref(),
                blocking_deps,
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "✅ Updated migration status for {fp_for_msg}"
        ))]))
    }

    pub async fn handle_get_migration_progress(
        &self,
        req: GetMigrationProgressRequest,
    ) -> Result<CallToolResult, McpError> {
        let _ = self.ensure_project_record(&req.project_id).await?;
        let store = self.state.migration_progress.clone();
        let pid = req.project_id.clone();

        let progress = tokio::task::spawn_blocking(move || store.get_progress(&pid))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if req.output_json {
            let json = serde_json::to_string_pretty(&progress)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let mut out = format!(
            "# Migration Progress — {}\n\n\
             **Total files**: {} | **Completion**: {:.1}%\n\n\
             | Status | Count |\n|--------|-------|\n\
             | Not Started | {} |\n\
             | In Progress | {} |\n\
             | Migrated | {} |\n\
             | Verified | {} |\n\
             | Blocked | {} |\n\n",
            progress.project_id,
            progress.total_files,
            progress.completion_pct,
            progress.not_started,
            progress.in_progress,
            progress.migrated,
            progress.verified,
            progress.blocked,
        );

        if !progress.by_file_type.is_empty() {
            out.push_str("## By File Type\n");
            for (ext, tp) in &progress.by_file_type {
                out.push_str(&format!(
                    "- `{ext}`: {}/{} ({:.0}%)\n",
                    tp.completed, tp.total, tp.pct,
                ));
            }
            out.push('\n');
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_suggest_migration_order(
        &self,
        req: SuggestMigrationOrderRequest,
    ) -> Result<CallToolResult, McpError> {
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();

        let plan = tokio::task::spawn_blocking(move || {
            crate::services::migration_order_service::suggest_migration_order(&graph, &pid)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if req.output_json {
            let json = serde_json::to_string_pretty(&plan)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let mut out = format!(
            "# Migration Order — {}\n\n**Total files**: {}\n\n",
            plan.project_id, plan.total_files,
        );

        if !plan.summary.is_empty() {
            out.push_str(&plan.summary);
            out.push_str("\n\n");
        }

        for wave in &plan.waves {
            out.push_str(&format!("## Wave {} — {}\n", wave.wave_number, wave.theme,));
            if !wave.prerequisites.is_empty() {
                out.push_str(&format!(
                    "Prerequisites: {}\n",
                    wave.prerequisites.join(", "),
                ));
            }
            for wf in &wave.files {
                out.push_str(&format!(
                    "- `{}` ({}, deps: {}, dependents: {}) — {}\n",
                    wf.path,
                    wf.estimated_complexity,
                    wf.dependency_count,
                    wf.dependent_count,
                    wf.reason,
                ));
            }
            if wave.strangler_fig_checkpoint {
                out.push_str("**Strangler fig checkpoint after this wave.**\n");
            }
            out.push('\n');
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_analyze_full_project_migration(
        &self,
        req: AnalyzeFullProjectMigrationRequest,
    ) -> Result<CallToolResult, McpError> {
        let rec = self.ensure_project_record(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let target_stack = req.target_stack.as_str().to_owned();
        let max_files = req.max_files;
        let project_dir = rec.directory.clone();

        // ── Async phase: discover and read all files from disk ────────────

        let graph_clone = graph.clone();
        let pid_clone = pid.clone();
        let file_nodes = tokio::task::spawn_blocking(move || {
            graph_clone.query_nodes(&pid_clone, Some("file"), None, None, 50_000)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let mut markup_paths: Vec<String> = Vec::new();
        let mut js_paths: Vec<String> = Vec::new();
        let mut asp_paths: Vec<String> = Vec::new();
        let mut report_paths: Vec<String> = Vec::new();
        let mut code_paths: Vec<String> = Vec::new();
        let mut has_global_asax = false;

        for n in &file_nodes {
            let name_lower = n.name.to_lowercase();
            if name_lower.ends_with(".aspx")
                || name_lower.ends_with(".ascx")
                || name_lower.ends_with(".master")
            {
                markup_paths.push(n.name.clone());
            } else if name_lower.ends_with(".js") {
                js_paths.push(n.name.clone());
            } else if name_lower.ends_with(".asp") {
                asp_paths.push(n.name.clone());
            } else if name_lower.ends_with(".rdl") || name_lower.ends_with(".rdlc") {
                report_paths.push(n.name.clone());
            } else if name_lower.ends_with(".cs") || name_lower.ends_with(".vb") {
                code_paths.push(n.name.clone());
            }
            if name_lower == "global.asax"
                || name_lower.ends_with("/global.asax")
                || name_lower.ends_with("\\global.asax")
            {
                has_global_asax = true;
            }
        }

        if markup_paths.is_empty() {
            let all_extensions = &[".aspx", ".ascx", ".master", ".js", ".asp", ".rdl", ".rdlc"];
            let discovered = discover_files_recursive(
                std::path::Path::new(&project_dir),
                all_extensions,
                max_files * 5,
            )
            .await;

            for path_str in discovered {
                let lower = path_str.to_lowercase();
                if lower.ends_with(".aspx")
                    || lower.ends_with(".ascx")
                    || lower.ends_with(".master")
                {
                    markup_paths.push(path_str);
                } else if lower.ends_with(".js") {
                    js_paths.push(path_str);
                } else if lower.ends_with(".asp") {
                    asp_paths.push(path_str);
                } else if lower.ends_with(".rdl") || lower.ends_with(".rdlc") {
                    report_paths.push(path_str);
                }
            }

            let code_discovered = discover_files_recursive(
                std::path::Path::new(&project_dir),
                &[".cs", ".vb"],
                max_files * 10,
            )
            .await;
            code_paths = code_discovered;
        }

        markup_paths.truncate(max_files);

        use crate::services::full_project_migration_service::{FileContent, ProjectFileBundle};

        let read_markup_futures: Vec<_> = markup_paths
            .iter()
            .map(|rel_path| {
                let dir = project_dir.clone();
                let rel = rel_path.clone();
                async move {
                    let full_path = safe_join(Path::new(&dir), &rel).ok()?;
                    let markup = tokio::fs::read_to_string(&full_path).await.ok()?;
                    let cb_path = find_codebehind_path(&full_path);
                    let cb_content = if let Some(ref p) = cb_path {
                        tokio::fs::read_to_string(p).await.ok()
                    } else {
                        None
                    };
                    Some(FileContent {
                        file_path: rel,
                        markup_content: markup,
                        codebehind_content: cb_content,
                    })
                }
            })
            .collect();

        let read_js_futures: Vec<_> = js_paths
            .iter()
            .map(|rel_path| {
                let dir = project_dir.clone();
                let rel = rel_path.clone();
                async move {
                    let full_path = safe_join(Path::new(&dir), &rel).ok()?;
                    tokio::fs::read_to_string(&full_path)
                        .await
                        .ok()
                        .map(|content| (rel, content))
                }
            })
            .collect();

        let read_asp_futures: Vec<_> = asp_paths
            .iter()
            .map(|rel_path| {
                let dir = project_dir.clone();
                let rel = rel_path.clone();
                async move {
                    let full_path = safe_join(Path::new(&dir), &rel).ok()?;
                    tokio::fs::read_to_string(&full_path)
                        .await
                        .ok()
                        .map(|content| (rel, content))
                }
            })
            .collect();

        let read_report_futures: Vec<_> = report_paths
            .iter()
            .map(|rel_path| {
                let dir = project_dir.clone();
                let rel = rel_path.clone();
                async move {
                    let full_path = safe_join(Path::new(&dir), &rel).ok()?;
                    tokio::fs::read_to_string(&full_path)
                        .await
                        .ok()
                        .map(|content| (rel, content))
                }
            })
            .collect();

        let (markup_results, js_results, asp_results, report_results) = tokio::join!(
            futures::future::join_all(read_markup_futures),
            futures::future::join_all(read_js_futures),
            futures::future::join_all(read_asp_futures),
            futures::future::join_all(read_report_futures),
        );

        let markup_files: Vec<FileContent> = markup_results.into_iter().flatten().collect();
        let js_files: Vec<(String, String)> = js_results.into_iter().flatten().collect();
        let classic_asp_files: Vec<(String, String)> = asp_results.into_iter().flatten().collect();
        let report_files: Vec<(String, String)> = report_results.into_iter().flatten().collect();

        let webconfig_path = safe_join(Path::new(&project_dir), "web.config");
        let webconfig_content = if let Ok(wc) = webconfig_path {
            tokio::fs::read_to_string(&wc).await.ok()
        } else {
            None
        };
        let webconfig_content = if webconfig_content.is_none() {
            if let Ok(alt) = safe_join(Path::new(&project_dir), "Web.config") {
                tokio::fs::read_to_string(&alt).await.ok()
            } else {
                None
            }
        } else {
            webconfig_content
        };

        let global_asax = {
            let ga_path = safe_join(Path::new(&project_dir), "Global.asax");
            let ga_exists = if let Ok(ref p) = ga_path {
                has_global_asax || p.exists()
            } else {
                false
            };
            if ga_exists {
                let ga_path = ga_path.expect("ga_exists implies Ok"); // safe
                let markup = match tokio::fs::read_to_string(&ga_path).await {
                    Ok(s) => s,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                    Err(e) => {
                        tracing::warn!(
                            "ENG-AUD-2026-S10-0003: failed to read Global.asax at {:?}: {}",
                            ga_path,
                            e
                        );
                        String::new()
                    }
                };
                let cb = {
                    let cs = safe_join(Path::new(&project_dir), "Global.asax.cs");
                    let vb = safe_join(Path::new(&project_dir), "Global.asax.vb");
                    if let Ok(ref cs_path) = cs {
                        if let Ok(content) = tokio::fs::read_to_string(cs_path).await {
                            Some(content)
                        } else if let Ok(ref vb_path) = vb {
                            tokio::fs::read_to_string(vb_path).await.ok()
                        } else {
                            None
                        }
                    } else if let Ok(ref vb_path) = vb {
                        tokio::fs::read_to_string(vb_path).await.ok()
                    } else {
                        None
                    }
                };
                Some(FileContent {
                    file_path: "Global.asax".to_string(),
                    markup_content: markup,
                    codebehind_content: cb,
                })
            } else {
                None
            }
        };

        let code_files: Vec<(String, String)> = code_paths
            .into_iter()
            .filter_map(|rel| {
                let full = safe_join(Path::new(&project_dir), &rel).ok()?;
                match std::fs::read_to_string(&full) {
                    Ok(c) => Some((rel, c)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => {
                        tracing::warn!(
                            "ENG-AUD-2026-S10-0003: failed to read code file {:?}: {}",
                            full,
                            e
                        );
                        None
                    }
                }
            })
            .collect();

        use crate::services::full_project_migration_service::ProjectReferenceBundle;
        let proj_file_paths = discover_files_recursive(
            std::path::Path::new(&project_dir),
            &[".csproj", ".vbproj"],
            50,
        )
        .await;

        let project_references: Vec<ProjectReferenceBundle> = proj_file_paths
            .into_iter()
            .filter_map(|rel| {
                let full = safe_join(Path::new(&project_dir), &rel).ok()?;
                let content = match std::fs::read_to_string(&full) {
                    Ok(c) => c,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
                    Err(e) => {
                        tracing::warn!(
                            "ENG-AUD-2026-S10-0003: failed to read project file {:?}: {}",
                            full,
                            e
                        );
                        return None;
                    }
                };
                let info = engram_index::solution_parser::parse_project_file(&content, &rel);
                let mut nuget_refs = Vec::new();
                let mut asm_refs = Vec::new();
                for pr in &info.package_references {
                    if pr.version.is_some() {
                        nuget_refs.push(pr.clone());
                    } else {
                        asm_refs.push(pr.name.clone());
                    }
                }
                Some(ProjectReferenceBundle {
                    project_path: rel,
                    target_framework: info.target_framework,
                    assembly_name: info.assembly_name,
                    root_namespace: info.root_namespace,
                    package_references: nuget_refs,
                    assembly_references: asm_refs,
                    project_dependencies: info.project_references,
                })
            })
            .collect();

        let proj_dir_ref = std::path::Path::new(&project_dir);
        let (sql_file_paths, pkgconfig_paths, config_paths, resx_paths, master_paths) = tokio::join!(
            discover_files_recursive(proj_dir_ref, &[".sql"], 100),
            discover_files_recursive(proj_dir_ref, &["packages.config"], 20),
            discover_files_recursive(proj_dir_ref, &[".config"], 100),
            discover_files_recursive(proj_dir_ref, &[".resx"], 200),
            discover_files_recursive(proj_dir_ref, &[".master"], 50),
        );

        let sql_files: Vec<(String, String)> = sql_file_paths
            .into_iter()
            .filter_map(|rel| {
                let full = safe_join(Path::new(&project_dir), &rel).ok()?;
                match std::fs::read_to_string(&full) {
                    Ok(c) => Some((rel, c)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => {
                        tracing::warn!(
                            "ENG-AUD-2026-S10-0003: failed to read SQL file {:?}: {}",
                            full,
                            e
                        );
                        None
                    }
                }
            })
            .collect();

        let packages_config_files: Vec<(String, String)> = pkgconfig_paths
            .into_iter()
            .filter(|p| p.ends_with("packages.config"))
            .filter_map(|rel| {
                let full = safe_join(Path::new(&project_dir), &rel).ok()?;
                match std::fs::read_to_string(&full) {
                    Ok(c) => Some((rel, c)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => {
                        tracing::warn!(
                            "ENG-AUD-2026-S10-0003: failed to read packages.config {:?}: {}",
                            full,
                            e
                        );
                        None
                    }
                }
            })
            .collect();

        let config_transform_files: Vec<(String, String)> = config_paths
            .into_iter()
            .filter(|p| {
                let lower = p.to_lowercase();
                lower.contains("web.")
                    && lower.ends_with(".config")
                    && !lower.ends_with("web.config")
                    && !lower.ends_with("packages.config")
            })
            .filter_map(|rel| {
                let full = safe_join(Path::new(&project_dir), &rel).ok()?;
                match std::fs::read_to_string(&full) {
                    Ok(c) => Some((rel, c)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => {
                        tracing::warn!(
                            "ENG-AUD-2026-S10-0003: failed to read config transform {:?}: {}",
                            full,
                            e
                        );
                        None
                    }
                }
            })
            .collect();

        let resx_files: Vec<(String, String)> = resx_paths
            .into_iter()
            .filter_map(|rel| {
                let full = safe_join(Path::new(&project_dir), &rel).ok()?;
                match std::fs::read_to_string(&full) {
                    Ok(c) => Some((rel, c)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => {
                        tracing::warn!(
                            "ENG-AUD-2026-S10-0003: failed to read resx file {:?}: {}",
                            full,
                            e
                        );
                        None
                    }
                }
            })
            .collect();

        let master_files: Vec<(String, String)> = master_paths
            .into_iter()
            .filter_map(|rel| {
                let full = safe_join(Path::new(&project_dir), &rel).ok()?;
                match std::fs::read_to_string(&full) {
                    Ok(c) => Some((rel, c)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => {
                        tracing::warn!(
                            "ENG-AUD-2026-S10-0003: failed to read master file {:?}: {}",
                            full,
                            e
                        );
                        None
                    }
                }
            })
            .collect();

        let bundle = ProjectFileBundle {
            markup_files,
            js_files,
            classic_asp_files,
            report_files,
            global_asax,
            web_config_content: webconfig_content,
            code_files,
            project_references,
            sql_files,
            packages_config_files,
            config_transform_files,
            resx_files,
            master_files,
        };

        // MIG1: create a fresh cancel token for this migration; wire it to the
        // handler shutdown if available in future.  Passing it into the
        // synchronous function gives callers cooperative abort capability.
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();
        let _ = cancel; // token not cancelled here — migration runs to completion unless signalled
        let report = tokio::task::spawn_blocking(move || {
            full_mig::analyze_full_project(&graph, &pid, &target_stack, &bundle, max_files, &cancel_clone)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // MIG1: explicitly surface partial-failure state.  Callers (and operators)
        // must be able to distinguish a complete report from a degraded one without
        // parsing the full content.  This satisfies the machine-readable completeness
        // metadata requirement: JSON consumers get structured fields; markdown consumers
        // get a parseable HTML comment header.
        if !report.report_is_complete {
            tracing::warn!(
                project_id = %req.project_id,
                degraded_count = report.degraded_sections.len(),
                degraded_sections = ?report.degraded_sections,
                "MIG1: migration report is INCOMPLETE — {} graph section(s) returned \
                 degraded data; report_is_complete=false",
                report.degraded_sections.len()
            );
        }

        if req.output_json {
            let json = serde_json::to_string_pretty(&report)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        // MIG1: prepend a machine-readable completeness header to markdown output
        // so automation can detect partial reports via regex without JSON parsing.
        let markdown = if report.report_is_complete {
            report.markdown_report
        } else {
            format!(
                "<!-- MIG1:INCOMPLETE degraded_sections={count} -->\n\
                 > **Warning:** This migration report is incomplete. \
                 {count} graph analysis section(s) returned degraded data: {sections}\n\n\
                 {body}",
                count = report.degraded_sections.len(),
                sections = report.degraded_sections.join(", "),
                body = report.markdown_report,
            )
        };
        Ok(CallToolResult::success(vec![Content::text(markdown)]))
    }

    pub async fn handle_reconcile_runtime_evidence(
        &self,
        req: ReconcileRuntimeEvidenceRequest,
    ) -> Result<CallToolResult, McpError> {
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();

        let batch: engram_core::runtime_evidence::RuntimeEvidenceBatch =
            serde_json::from_str(&req.evidence_json).map_err(|e| {
                McpError::invalid_params(format!("Invalid evidence JSON: {e}"), None)
            })?;

        let report = tokio::task::spawn_blocking(move || {
            crate::services::instrumentation_service::reconcile_runtime_evidence(
                &graph, &pid, &batch,
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let mut out = format!(
            "# Reconciliation Report\n\n\
             **Total static paths**: {}\n\
             **Confirmed**: {}\n\
             **Contradicted**: {}\n\
             **Inconclusive**: {}\n\
             **Confidence delta**: {:.3}\n\n",
            report.summary.total_static_paths,
            report.summary.confirmed_count,
            report.summary.contradicted_count,
            report.summary.inconclusive_count,
            report.summary.confidence_delta,
        );

        if !report.confirmed_paths.is_empty() {
            out.push_str("## Confirmed Paths\n");
            for p in &report.confirmed_paths {
                out.push_str(&format!(
                    "- {} → {} [{}] (evidence: {})\n",
                    p.source,
                    p.target,
                    p.edge_kind,
                    p.runtime_evidence.as_deref().unwrap_or("confirmed")
                ));
            }
            out.push('\n');
        }

        if !report.contradicted_paths.is_empty() {
            out.push_str("## Contradicted Paths\n");
            for p in &report.contradicted_paths {
                out.push_str(&format!(
                    "- {} → {} [{}] (source/target seen but not this path)\n",
                    p.source, p.target, p.edge_kind
                ));
            }
            out.push('\n');
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_suggest_state_migration(
        &self,
        req: SuggestStateMigrationRequest,
    ) -> Result<CallToolResult, McpError> {
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();

        let report = tokio::task::spawn_blocking(move || {
            crate::services::state_migration_service::analyze_state_migration(&graph, &pid)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if req.output_json {
            let json = serde_json::to_string_pretty(&report)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let s = &report.summary;
        let mut out = format!(
            "# State Migration Report\n\n**Total state keys**: {}\n\n",
            s.total_state_keys
        );

        if !s.by_store.is_empty() {
            out.push_str("## By Store Type\n");
            for (store, count) in &s.by_store {
                out.push_str(&format!("- {store}: {count}\n"));
            }
            out.push('\n');
        }

        if !s.by_target.is_empty() {
            out.push_str("## By Migration Target\n");
            for (target, count) in &s.by_target {
                out.push_str(&format!("- {target}: {count}\n"));
            }
            out.push('\n');
        }

        if !s.high_risk_keys.is_empty() {
            out.push_str("## High-Risk Keys\n");
            for k in &s.high_risk_keys {
                out.push_str(&format!("- {k}\n"));
            }
            out.push('\n');
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_generate_characterization_tests(
        &self,
        req: GenerateCharacterizationTestsRequest,
    ) -> Result<CallToolResult, McpError> {
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let file_path = req.file_path.clone();
        let framework = req.framework.as_str().to_owned();

        let result = tokio::task::spawn_blocking(move || {
            crate::services::characterization_test_service::generate_characterization_tests(
                &graph, &pid, &file_path, &framework,
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if req.output_json {
            let json = serde_json::to_string_pretty(&result)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let mut out = format!(
            "# Characterization Tests ({} tests, {})\n\n",
            result.test_count, result.framework
        );

        out.push_str("## Generated Test Code\n```csharp\n");
        out.push_str(&result.test_code);
        out.push_str("```\n\n");

        if !result.coverage_map.is_empty() {
            out.push_str("## Coverage Map\n");
            for entry in &result.coverage_map {
                out.push_str(&format!(
                    "- **{}** [{:?}]: {} edges covered\n",
                    entry.test_name,
                    entry.category,
                    entry.covered_edges.len()
                ));
            }
        }

        if !result.warnings.is_empty() {
            out.push_str("\n## Warnings\n");
            for w in &result.warnings {
                out.push_str(&format!("- {w}\n"));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_generate_strangler_fig_config(
        &self,
        req: GenerateStranglerFigRequest,
    ) -> Result<CallToolResult, McpError> {
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let legacy_url = req.legacy_base_url.clone();
        let modern_url = req.modern_base_url.clone();

        let result = tokio::task::spawn_blocking(move || {
            crate::services::strangler_fig_service::generate_strangler_fig_config(
                &graph,
                &pid,
                &legacy_url,
                &modern_url,
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let mut out = String::from("# Strangler Fig Migration Infrastructure\n\n");

        out.push_str(&format!(
            "**Pages discovered**: {} total ({} migrated, {} unmigrated)\n\n",
            result.migrated_pages.len() + result.unmigrated_pages.len(),
            result.migrated_pages.len(),
            result.unmigrated_pages.len(),
        ));

        out.push_str("## YARP Reverse Proxy (appsettings.YARP.json)\n```json\n");
        out.push_str(&result.yarp_config);
        out.push_str("```\n\n");

        out.push_str("## Feature Flags (appsettings.FeatureFlags.json + FeatureFlagMiddleware.cs)\n```csharp\n");
        out.push_str(&result.feature_flags_config);
        out.push_str("```\n\n");

        out.push_str(
            "## Strangler Fig Routing Middleware (StranglerFigMiddleware.cs)\n```csharp\n",
        );
        out.push_str(&result.routing_middleware);
        out.push_str("```\n\n");

        out.push_str("## Migration Health Check (MigrationHealthCheck.cs)\n```csharp\n");
        out.push_str(&result.health_check);
        out.push_str("```\n\n");

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_map_validation_controls(
        &self,
        req: MapValidationControlsRequest,
    ) -> Result<CallToolResult, McpError> {
        let rec = self.ensure_project_record(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let file_path = req.file_path.clone();

        let aspx_full = safe_join(Path::new(&rec.directory), &file_path)
            .map_err(|e| McpError::internal_error(format!("Path validation: {e}"), None))?;
        let aspx_content = tokio::fs::read_to_string(&aspx_full).await.map_err(|e| {
            McpError::internal_error(format!("Failed to read {aspx_full:?}: {e}"), None)
        })?;

        let cb_path = find_codebehind_path(&aspx_full);
        let cb_content = if let Some(ref p) = cb_path {
            tokio::fs::read_to_string(p).await.ok()
        } else {
            None
        };

        let result = tokio::task::spawn_blocking(move || {
            crate::services::validation_mapping_service::analyze_validation_controls(
                &graph,
                &pid,
                &file_path,
                &aspx_content,
                cb_content.as_deref(),
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if req.output_json {
            let json = serde_json::to_string_pretty(&result)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let mut out = format!(
            "# Validation Controls — {}\n\n\
             **Total validators**: {} | **Complexity**: {}\n\n",
            result.file_path, result.total_validators, result.migration_complexity
        );

        if !result.validators.is_empty() {
            out.push_str("## Validators\n");
            for v in &result.validators {
                out.push_str(&format!("### {} ({})\n", v.validator_id, v.validator_type));
                out.push_str(&format!("- **Modern strategy**: {}\n", v.modern_blazor));
                out.push('\n');
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_map_auth_config(
        &self,
        req: MapAuthConfigRequest,
    ) -> Result<CallToolResult, McpError> {
        let rec = self.ensure_project_record(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let project_dir = rec.directory.clone();

        let webconfig_path = safe_join(Path::new(&project_dir), "web.config");
        let webconfig_content = if let Ok(wc) = webconfig_path {
            tokio::fs::read_to_string(&wc).await.ok()
        } else {
            None
        };
        let webconfig_content = if webconfig_content.is_none() {
            if let Ok(alt) = safe_join(Path::new(&project_dir), "Web.config") {
                tokio::fs::read_to_string(&alt).await.ok()
            } else {
                None
            }
        } else {
            webconfig_content
        };

        let code_files = if let Some(ref scope) = req.file_scope {
            let full = safe_join(Path::new(&project_dir), scope)
                .map_err(|e| McpError::internal_error(format!("Path validation: {e}"), None))?;
            match tokio::fs::read_to_string(&full).await {
                Ok(content) => vec![(scope.clone(), content)],
                Err(_) => vec![],
            }
        } else {
            let g = graph.clone();
            let p = pid.clone();
            let dir = project_dir.clone();
            tokio::task::spawn_blocking(move || -> Vec<(String, String)> {
                let file_nodes = g
                    .query_nodes(&p, Some("file"), None, None, 50_000)
                    .unwrap_or_default();
                let mut files = Vec::new();
                for node in &file_nodes {
                    let path = &node.name;
                    if (path.ends_with(".vb")
                        || path.ends_with(".cs")
                        || path.ends_with(".aspx.vb")
                        || path.ends_with(".aspx.cs"))
                        && let Ok(full) = safe_join(Path::new(&dir), path)
                        && let Ok(content) = std::fs::read_to_string(&full)
                    {
                        files.push((path.clone(), content));
                    }
                }
                files
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
        };

        let result = tokio::task::spawn_blocking(move || {
            let code_files_refs: Vec<(&str, &str)> = code_files
                .iter()
                .map(|(p, c)| (p.as_str(), c.as_str()))
                .collect();
            crate::services::auth_config_service::analyze_auth_config(
                &graph,
                &pid,
                webconfig_content.as_deref(),
                &code_files_refs,
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if req.output_json {
            let json = serde_json::to_string_pretty(&result)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let mut out = String::from("# Authentication & Authorization Map\n\n");
        out.push_str(&format!("**Auth Mode**: {}\n", result.auth_mode));

        if !result.recommendations.is_empty() {
            out.push_str("\n## Recommendations\n");
            for rec in &result.recommendations {
                out.push_str(&format!(
                    "- **[{}]** {}: {}\n",
                    rec.severity, rec.category, rec.recommendation
                ));
            }
            out.push('\n');
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_map_page_lifecycle(
        &self,
        req: MapPageLifecycleRequest,
    ) -> Result<CallToolResult, McpError> {
        let rec = self.ensure_project_record(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let file_path = req.file_path.clone();

        let cb_full = safe_join(Path::new(&rec.directory), &file_path)
            .map_err(|e| McpError::internal_error(format!("Path validation: {e}"), None))?;
        let cb_content = tokio::fs::read_to_string(&cb_full).await.map_err(|e| {
            McpError::internal_error(format!("Failed to read {cb_full:?}: {e}"), None)
        })?;

        let aspx_path = find_aspx_for_codebehind(&cb_full);
        let aspx_content = if let Some(ref p) = aspx_path {
            tokio::fs::read_to_string(p).await.ok()
        } else {
            None
        };

        let result = tokio::task::spawn_blocking(move || {
            crate::services::lifecycle_service::analyze_page_lifecycle(
                &graph,
                &pid,
                &file_path,
                &cb_content,
                aspx_content.as_deref(),
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if req.output_json {
            let json = serde_json::to_string_pretty(&result)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let mut out = format!("# Page Lifecycle — {}\n\n", result.file_path);

        if let Some(ref bc) = result.base_class {
            out.push_str(&format!("**Base class**: `{bc}`\n\n"));
        }

        if !result.lifecycle_events.is_empty() {
            out.push_str("## Lifecycle Events\n");
            for ev in &result.lifecycle_events {
                out.push_str(&format!("### {}\n", ev.event_name));
                out.push_str(&format!("- **Modern Equivalent**: {}\n", ev.modern_blazor));
                out.push('\n');
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_analyze_viewstate_deps(
        &self,
        req: AnalyzeViewStateDepsRequest,
    ) -> Result<CallToolResult, McpError> {
        let rec = self.ensure_project_record(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let file_path = req.file_path.clone();

        let cb_full = safe_join(Path::new(&rec.directory), &file_path)
            .map_err(|e| McpError::internal_error(format!("Path validation: {e}"), None))?;
        let cb_content = tokio::fs::read_to_string(&cb_full).await.map_err(|e| {
            McpError::internal_error(format!("Failed to read {cb_full:?}: {e}"), None)
        })?;

        let aspx_path = find_aspx_for_codebehind(&cb_full);
        let aspx_content = if let Some(ref p) = aspx_path {
            tokio::fs::read_to_string(p).await.ok()
        } else {
            None
        };

        let result = tokio::task::spawn_blocking(move || {
            crate::services::viewstate_service::analyze_viewstate_dependencies(
                &graph,
                &pid,
                &file_path,
                &cb_content,
                aspx_content.as_deref(),
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if req.output_json {
            let json = serde_json::to_string_pretty(&result)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let mut out = format!(
            "# ViewState Dependencies — {}\n\n\
             **Total state fields**: {} | **Complexity**: {}\n",
            result.file_path, result.total_state_fields, result.migration_complexity,
        );

        if !result.modern_state_model.is_empty() {
            out.push_str("\n## Recommended Modern State Model\n");
            for field in &result.modern_state_model {
                out.push_str(&format!("### {}\n", field.field_name));
                out.push_str(&format!("- **Source**: {}\n", field.source));
                out.push_str(&format!(
                    "- **Modern strategy**: {}\n",
                    field.blazor_declaration
                ));
                out.push('\n');
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    // ── Phase 37: Wiring — Expose Existing Services ──────────────────────────

    /// 37-W1: Full database intelligence report.
    pub async fn handle_analyze_database_intelligence(
        &self,
        req: AnalyzeDatabaseIntelligenceRequest,
    ) -> Result<CallToolResult, McpError> {
        let rec = self.ensure_project_record(&req.project_id).await?;
        let project_dir = rec.directory.clone();
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let sp_limit = req.sp_limit;
        let output_json = req.output_json;
        let sql_file_path = req.sql_file_path.clone();

        // ── Discover SQL files ──────────────────────────────────────────────
        let sql_files: Vec<(String, String)> = if let Some(ref specific_path) = sql_file_path {
            let full = safe_join(Path::new(&project_dir), specific_path)
                .map_err(|e| McpError::invalid_params(format!("Path validation: {e}"), None))?;
            match tokio::fs::read_to_string(&full).await {
                Ok(content) => vec![(specific_path.clone(), content)],
                Err(e) => {
                    return Err(McpError::invalid_params(
                        format!("Cannot read SQL file '{}': {}", specific_path, e),
                        None,
                    ));
                }
            }
        } else {
            let discovered =
                discover_files_recursive(std::path::Path::new(&project_dir), &[".sql"], 200).await;
            let mut files = Vec::with_capacity(discovered.len());
            for rel in discovered {
                if let Ok(full) = safe_join(Path::new(&project_dir), &rel)
                    && let Ok(content) = std::fs::read_to_string(&full)
                {
                    files.push((rel, content));
                }
            }
            files
        };

        if sql_files.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No .sql files found. Place a database.sql (or other .sql files) in the project directory and re-run.",
            )]));
        }

        // ── Discover code files for SP cross-referencing ────────────────────
        let graph_clone = graph.clone();
        let pid_clone = pid.clone();
        let file_nodes = tokio::task::spawn_blocking(move || {
            graph_clone.query_nodes(&pid_clone, Some("file"), None, None, 50_000)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let mut code_paths: Vec<String> = Vec::new();
        for n in &file_nodes {
            let lower = n.name.to_lowercase();
            if lower.ends_with(".vb") || lower.ends_with(".cs") {
                code_paths.push(n.name.clone());
            }
        }

        // Fallback: disk discovery if graph is empty
        if code_paths.is_empty() {
            let disc = discover_files_recursive(
                std::path::Path::new(&project_dir),
                &[".vb", ".cs"],
                5_000,
            )
            .await;
            code_paths = disc;
        }

        let code_files: Vec<(String, String)> = code_paths
            .into_iter()
            .filter_map(|rel| {
                let full = safe_join(Path::new(&project_dir), &rel).ok()?;
                match std::fs::read_to_string(&full) {
                    Ok(c) => Some((rel, c)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => {
                        tracing::warn!(
                            "ENG-AUD-2026-S10-0003: failed to read code file {:?}: {}",
                            full,
                            e
                        );
                        None
                    }
                }
            })
            .collect();

        // ── Build SP catalog + database intelligence in blocking task ───────
        let sql_files_owned = sql_files;
        let result = tokio::task::spawn_blocking(move || {
            use crate::services::full_project_migration_service as full_mig;

            let code_refs: Vec<(&str, &str)> = code_files
                .iter()
                .map(|(p, c)| (p.as_str(), c.as_str()))
                .collect();

            // Build SP catalog (reuse the same function the full report uses)
            let sp_catalog =
                full_mig::build_sp_catalog_public(&sql_files_owned, &code_refs, sp_limit);

            // Collect code tables for cross-referencing
            let mut code_tables = std::collections::HashSet::new();
            for sp in &sp_catalog.procedures {
                for t in &sp.tables_read {
                    code_tables.insert(t.clone());
                }
                for t in &sp.tables_written {
                    code_tables.insert(t.clone());
                }
            }

            // Also collect table references from graph edges (QueriesTable edges)
            if let Ok(edges) =
                graph.list_edges_by_kind(&pid, engram_graph::EdgeKind::QueriesTable, 50_000)
            {
                for e in &edges {
                    code_tables.insert(e.target_id.clone());
                }
            }

            let intel = crate::services::database_intelligence_service::build_database_intelligence(
                &sp_catalog,
                &sql_files_owned,
                &code_tables,
            );

            (sp_catalog, intel)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let (_sp_catalog, intel) = result;

        if output_json {
            let json = serde_json::to_string_pretty(&intel)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let md =
            crate::services::database_intelligence_service::render_database_intelligence_markdown(
                &intel,
            );
        Ok(CallToolResult::success(vec![Content::text(md)]))
    }

    /// 37-W2: Deep analysis for a single stored procedure.
    pub async fn handle_get_sp_details(
        &self,
        req: GetSpDetailsRequest,
    ) -> Result<CallToolResult, McpError> {
        let rec = self.ensure_project_record(&req.project_id).await?;
        let project_dir = rec.directory.clone();
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let sp_name = req.sp_name.clone();

        // Discover SQL files
        let sql_paths =
            discover_files_recursive(std::path::Path::new(&project_dir), &[".sql"], 200).await;

        let sql_files: Vec<(String, String)> = sql_paths
            .into_iter()
            .filter_map(|rel| {
                let full = safe_join(Path::new(&project_dir), &rel).ok()?;
                match std::fs::read_to_string(&full) {
                    Ok(c) => Some((rel, c)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => {
                        tracing::warn!(
                            "ENG-AUD-2026-S10-0003: failed to read SQL file {:?}: {}",
                            full,
                            e
                        );
                        None
                    }
                }
            })
            .collect();

        if sql_files.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No .sql files found in project.",
            )]));
        }

        // Discover code files for caller detection
        let code_disc =
            discover_files_recursive(std::path::Path::new(&project_dir), &[".vb", ".cs"], 5_000)
                .await;
        let code_files: Vec<(String, String)> = code_disc
            .into_iter()
            .filter_map(|rel| {
                let full = safe_join(Path::new(&project_dir), &rel).ok()?;
                match std::fs::read_to_string(&full) {
                    Ok(c) => Some((rel, c)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => {
                        tracing::warn!(
                            "ENG-AUD-2026-S10-0003: failed to read code file {:?}: {}",
                            full,
                            e
                        );
                        None
                    }
                }
            })
            .collect();

        let result = tokio::task::spawn_blocking(move || {
            use crate::services::database_intelligence_service as dbi;

            // Find the SP body across all SQL files using two-step approach
            // (Rust regex crate does not support lookahead assertions)
            let header_re = regex::Regex::new(&format!(
                r"(?ims)CREATE\s+(?:OR\s+ALTER\s+)?PROC(?:EDURE)?\s+\[?(?:dbo\.)?\]?\[?{}\]?\b",
                regex::escape(&sp_name)
            ))
            .map_err(|e| format!("Invalid SP name regex: {}", e))?;
            let next_object_re = regex::Regex::new(
                r"(?ims)\bCREATE\s+(?:OR\s+ALTER\s+)?(?:PROC(?:EDURE)?|FUNCTION|TRIGGER|VIEW)\b",
            )
            .expect("valid regex");

            let mut sp_body: Option<String> = None;
            let mut sp_source_file: Option<String> = None;
            for (path, content) in &sql_files {
                if let Some(hdr) = header_re.find(content) {
                    let start = hdr.start();
                    // Find the next CREATE after the header to delimit body
                    let rest = &content[hdr.end()..];
                    let end = next_object_re
                        .find(rest)
                        .map(|m| hdr.end() + m.start())
                        .unwrap_or(content.len());
                    sp_body = Some(content[start..end].trim_end().to_string());
                    sp_source_file = Some(path.clone());
                    break;
                }
            }

            let sp_body = match sp_body {
                Some(b) => b,
                None => {
                    return Err(format!(
                        "Stored procedure '{}' not found in any .sql file.",
                        sp_name
                    ));
                }
            };

            // Deterministic summary
            let logic = dbi::deterministic_sp_summary(&sp_name, &sp_body);

            // Detect SP call chains
            let chains = dbi::detect_sp_call_chains(&sql_files);

            // Find chains involving this SP
            let relevant_chains: Vec<&dbi::SpCallChain> = chains
                .iter()
                .filter(|c| c.chain.iter().any(|name| name.eq_ignore_ascii_case(&sp_name)))
                .collect();

            // Detect triggers on tables this SP writes to
            let triggers = dbi::detect_triggers(&sql_files);
            let affected_triggers: Vec<&dbi::TriggerInfo> = triggers
                .iter()
                .filter(|t| {
                    logic.data_tables.iter().any(|dt| {
                        dt.eq_ignore_ascii_case(&t.target_table)
                    })
                })
                .collect();

            // Find code callers via SP extractor
            let mut called_by_code: Vec<String> = Vec::new();
            for (path, content) in &code_files {
                let rel = engram_core::RelPath::new(path);
                let (_, edges) = engram_index::sp_extractor::extract_code_side_sp_calls(&rel, content);
                for edge in &edges {
                    if edge.kind == "calls_stored_procedure"
                        && edge.target_name.eq_ignore_ascii_case(&sp_name)
                    {
                        called_by_code.push(path.clone());
                        break;
                    }
                }
            }

            // Find reverse SP callers (other SPs that EXEC this one)
            let mut called_by_sps: Vec<String> = Vec::new();
            let exec_re = regex::Regex::new(&format!(
                r"(?i)\b(?:EXEC(?:UTE)?)\s+\[?(?:dbo\.)?\]?\[?{}\]?\b",
                regex::escape(&sp_name)
            ))
            .map_err(|e| format!("Invalid EXEC regex: {}", e))?;
            {
                // Look for other SP bodies that call this one
                let sp_def_re = regex::Regex::new(
                    r"(?ims)CREATE\s+(?:OR\s+ALTER\s+)?PROC(?:EDURE)?\s+\[?(?:dbo\.)?\]?\[?(\w+)\]?"
                ).expect("valid regex");
                for (_path, content) in &sql_files {
                    for cap in sp_def_re.captures_iter(content) {
                        let other_name = cap[1].to_string();
                        if other_name.eq_ignore_ascii_case(&sp_name) {
                            continue; // skip self
                        }
                        // Delimit body: from end of header to next CREATE object (any type)
                        let start = cap.get(0).expect("group 0 always present").end();
                        let remaining = &content[start..];
                        let end = next_object_re
                            .find(remaining)
                            .map(|m| m.start())
                            .unwrap_or(remaining.len());
                        let other_body = &remaining[..end];
                        if exec_re.is_match(other_body) {
                            called_by_sps.push(other_name);
                        }
                    }
                }
            }
            called_by_sps.sort();
            called_by_sps.dedup();

            // Also check graph for callers via incoming SqlCalls edges
            if let Ok(incoming) = graph.find_incoming_edges(
                &pid,
                Some(engram_graph::EdgeKind::SqlCalls),
                &sp_name,
                500,
            ) {
                for (source_id, _weight) in &incoming {
                    if !called_by_code.contains(source_id) {
                        called_by_code.push(source_id.clone());
                    }
                }
            }

            called_by_code.sort();
            called_by_code.dedup();

            // Determine complexity
            let complexity = if logic.side_effects.iter().any(|s| s.contains("dynamic SQL"))
                || logic.side_effects.iter().any(|s| s.contains("cursor"))
                || logic.calls_other_sps.len() > 3
                || logic.data_tables.len() > 5
            {
                "high"
            } else if logic.calls_other_sps.len() > 1
                || logic.data_tables.len() > 3
                || logic.side_effects.iter().any(|s| s.contains("transaction"))
            {
                "medium"
            } else {
                "low"
            };

            // Build response markdown
            let mut out = format!("# Stored Procedure: {}\n\n", sp_name);
            if let Some(ref src) = sp_source_file {
                out.push_str(&format!("**Source file**: {}\n", src));
            }
            out.push_str(&format!("**Complexity**: {}\n", complexity));
            out.push_str(&format!("**Content hash**: {}\n\n", logic.content_hash));

            out.push_str("## Purpose\n\n");
            out.push_str(&logic.purpose);
            out.push_str("\n\n");

            if !logic.parameters.is_empty() {
                out.push_str("## Parameters\n\n");
                for p in &logic.parameters {
                    out.push_str(&format!("- `{}`\n", p));
                }
                out.push('\n');
            }

            if !logic.steps.is_empty() {
                out.push_str("## Steps\n\n");
                for (i, s) in logic.steps.iter().enumerate() {
                    out.push_str(&format!("{}. {}\n", i + 1, s));
                }
                out.push('\n');
            }

            // Tables written (via DML statements)
            let write_re = regex::Regex::new(
                r"(?i)\b(?:INSERT\s+INTO|UPDATE|DELETE\s+FROM|MERGE\s+INTO)\s+\[?(?:dbo\.)?\]?\[?(\w+)\]?"
            ).expect("valid regex");
            let tables_written: std::collections::HashSet<String> = write_re
                .captures_iter(&sp_body)
                .map(|c| c[1].to_string())
                .collect();

            // Tables read = all referenced tables minus written-only tables
            let tables_read: Vec<String> = logic
                .data_tables
                .iter()
                .filter(|t| {
                    !tables_written
                        .iter()
                        .any(|w| w.eq_ignore_ascii_case(t))
                })
                .cloned()
                .collect();
            let tables_written_sorted: Vec<String> = {
                let mut v: Vec<String> = tables_written.into_iter().collect();
                v.sort();
                v
            };

            out.push_str("## Tables Read\n\n");
            if tables_read.is_empty() {
                out.push_str("_(none detected)_\n\n");
            } else {
                for t in &tables_read {
                    out.push_str(&format!("- {}\n", t));
                }
                out.push('\n');
            }

            out.push_str("## Tables Written\n\n");
            if tables_written_sorted.is_empty() {
                out.push_str("_(none detected)_\n\n");
            } else {
                for t in &tables_written_sorted {
                    out.push_str(&format!("- {}\n", t));
                }
                out.push('\n');
            }

            if !logic.calls_other_sps.is_empty() {
                out.push_str("## Calls Other Stored Procedures\n\n");
                for sp in &logic.calls_other_sps {
                    out.push_str(&format!("- `{}`\n", sp));
                }
                out.push('\n');
            }

            if !called_by_sps.is_empty() {
                out.push_str("## Called By Other Stored Procedures\n\n");
                for sp in &called_by_sps {
                    out.push_str(&format!("- `{}`\n", sp));
                }
                out.push('\n');
            }

            if !called_by_code.is_empty() {
                out.push_str("## Called From Code Files\n\n");
                for f in &called_by_code {
                    out.push_str(&format!("- {}\n", f));
                }
                out.push('\n');
            }

            if !affected_triggers.is_empty() {
                out.push_str("## Triggers That May Fire\n\n");
                out.push_str("| Trigger | Table | Event | Type |\n");
                out.push_str("|---------|-------|-------|------|\n");
                for t in &affected_triggers {
                    out.push_str(&format!(
                        "| {} | {} | {} | {} |\n",
                        t.name.replace('|', "\\|"),
                        t.target_table.replace('|', "\\|"),
                        t.event_types.join(", ").replace('|', "\\|"),
                        t.trigger_type.replace('|', "\\|"),
                    ));
                }
                out.push('\n');
            }

            if !logic.side_effects.is_empty() {
                out.push_str("## Side Effects & Warnings\n\n");
                for s in &logic.side_effects {
                    out.push_str(&format!("- {}\n", s));
                }
                out.push('\n');
            }

            if !relevant_chains.is_empty() {
                out.push_str("## SP Call Chains Involving This Procedure\n\n");
                for chain in &relevant_chains {
                    let marker = if chain.is_cycle { " ⚠️ CYCLE" } else { "" };
                    out.push_str(&format!(
                        "- {}{}\n",
                        chain.chain.join(" → "),
                        marker,
                    ));
                }
                out.push('\n');
            }

            Ok(out)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(result)]))
    }

    /// 37-W3: List all triggers, optionally filtered by table.
    pub async fn handle_list_triggers(
        &self,
        req: ListTriggersRequest,
    ) -> Result<CallToolResult, McpError> {
        let rec = self.ensure_project_record(&req.project_id).await?;
        let project_dir = rec.directory.clone();
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let table_filter = req.table_name.clone();
        let output_json = req.output_json;

        // Discover SQL files
        let sql_paths =
            discover_files_recursive(std::path::Path::new(&project_dir), &[".sql"], 200).await;

        let sql_files: Vec<(String, String)> = sql_paths
            .into_iter()
            .filter_map(|rel| {
                let full = safe_join(Path::new(&project_dir), &rel).ok()?;
                match std::fs::read_to_string(&full) {
                    Ok(c) => Some((rel, c)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => {
                        tracing::warn!(
                            "ENG-AUD-2026-S10-0003: failed to read SQL file {:?}: {}",
                            full,
                            e
                        );
                        None
                    }
                }
            })
            .collect();

        if sql_files.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No .sql files found in project.",
            )]));
        }

        let result = tokio::task::spawn_blocking(move || {
            use crate::services::database_intelligence_service as dbi;

            let mut triggers = dbi::detect_triggers(&sql_files);

            // Filter by table if requested
            if let Some(ref table) = table_filter {
                triggers.retain(|t| t.target_table.eq_ignore_ascii_case(table));
            }

            // Collect code tables that write to trigger target tables
            // for cross-reference (which code paths indirectly fire each trigger)

            // Pre-fetch SqlCalls edges once (not per-trigger)
            let sql_call_edges = graph
                .list_edges_by_kind(&pid, engram_graph::EdgeKind::SqlCalls, 5_000)
                .unwrap_or_default();

            let mut trigger_code_paths: std::collections::HashMap<String, Vec<String>> =
                std::collections::HashMap::new();
            for trigger in &triggers {
                let table_lower = trigger.target_table.to_lowercase();
                // Check graph for code that writes to this table via incoming QueriesTable edges
                if let Ok(incoming) = graph.find_incoming_edges(
                    &pid,
                    Some(engram_graph::EdgeKind::QueriesTable),
                    &trigger.target_table,
                    500,
                ) {
                    for (source_id, _weight) in &incoming {
                        trigger_code_paths
                            .entry(trigger.name.clone())
                            .or_default()
                            .push(source_id.clone());
                    }
                }
                // Cross-reference SqlCalls edges that target this table
                for e in &sql_call_edges {
                    if e.target_id.to_lowercase().contains(&table_lower) {
                        trigger_code_paths
                            .entry(trigger.name.clone())
                            .or_default()
                            .push(e.source_id.clone());
                    }
                }
            }
            // Dedup code paths
            for paths in trigger_code_paths.values_mut() {
                paths.sort();
                paths.dedup();
            }

            (triggers, trigger_code_paths)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let (triggers, trigger_code_paths) = result;

        if triggers.is_empty() {
            let msg = if let Some(ref table) = req.table_name {
                format!("No triggers found on table '{}'.", table)
            } else {
                "No triggers found in SQL files.".to_string()
            };
            return Ok(CallToolResult::success(vec![Content::text(msg)]));
        }

        if output_json {
            let json = serde_json::to_string_pretty(&triggers)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let mut out = format!("# Database Triggers ({})\n\n", triggers.len());
        out.push_str("| Trigger | Table | Event | Type | Body Summary |\n");
        out.push_str("|---------|-------|-------|------|-------------|\n");
        for t in &triggers {
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                t.name,
                t.target_table,
                t.event_types.join(", "),
                t.trigger_type,
                t.body_summary.replace('|', "\\|"),
            ));
        }
        out.push('\n');

        // Show code paths that indirectly fire each trigger
        let mut has_code_refs = false;
        for t in &triggers {
            if let Some(paths) = trigger_code_paths.get(&t.name)
                && !paths.is_empty()
            {
                if !has_code_refs {
                    out.push_str("## Code Paths That Fire Triggers\n\n");
                    has_code_refs = true;
                }
                out.push_str(&format!("### {} (on {})\n\n", t.name, t.target_table));
                for p in paths {
                    out.push_str(&format!("- {}\n", p));
                }
                out.push('\n');
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    /// 37-W4: Detect sync-over-async hazards.
    pub async fn handle_analyze_sync_hazards(
        &self,
        req: AnalyzeSyncHazardsRequest,
    ) -> Result<CallToolResult, McpError> {
        let rec = self.ensure_project_record(&req.project_id).await?;
        let project_dir = rec.directory.clone();
        let output_json = req.output_json;

        // MinSeverity is a validated enum — exhaustive match, no fallback needed.
        let severity_threshold = match req.min_severity {
            MinSeverity::Medium => 0,
            MinSeverity::High => 1,
            MinSeverity::Critical => 2,
        };

        // Collect files to analyze
        let files_to_scan: Vec<(String, String, bool)> = if let Some(ref specific) = req.file_path {
            let full = safe_join(Path::new(&project_dir), specific)
                .map_err(|e| McpError::invalid_params(format!("Path validation: {e}"), None))?;
            match tokio::fs::read_to_string(&full).await {
                Ok(content) => {
                    let is_vb = specific.to_lowercase().ends_with(".vb");
                    vec![(specific.clone(), content, is_vb)]
                }
                Err(e) => {
                    return Err(McpError::invalid_params(
                        format!("Cannot read file '{}': {}", specific, e),
                        None,
                    ));
                }
            }
        } else {
            // Scan all .vb/.cs files
            let disc = discover_files_recursive(
                std::path::Path::new(&project_dir),
                &[".vb", ".cs"],
                5_000,
            )
            .await;
            let mut files = Vec::with_capacity(disc.len());
            for rel in disc {
                if let Ok(full) = safe_join(Path::new(&project_dir), &rel)
                    && let Ok(content) = std::fs::read_to_string(&full)
                {
                    let is_vb = rel.to_lowercase().ends_with(".vb");
                    files.push((rel, content, is_vb));
                }
            }
            files
        };

        if files_to_scan.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No .vb or .cs files found to analyze.",
            )]));
        }

        let result = tokio::task::spawn_blocking(move || {
            use engram_index::sync_hazard_detector::{HazardSeverity, detect_sync_hazards};

            let files_scanned = files_to_scan.len();

            let severity_num = |s: &HazardSeverity| -> u8 {
                match s {
                    HazardSeverity::Medium => 0,
                    HazardSeverity::High => 1,
                    HazardSeverity::Critical => 2,
                }
            };

            // Run detection on all files, keeping only those with qualifying hazards
            let mut per_file_reports: Vec<(
                String,
                engram_index::sync_hazard_detector::SyncHazardReport,
            )> = Vec::new();

            for (path, content, is_vb) in &files_to_scan {
                let report = detect_sync_hazards(content, *is_vb);
                let has_qualifying = report
                    .hazards
                    .iter()
                    .any(|h| severity_num(&h.severity) >= severity_threshold);
                if has_qualifying {
                    per_file_reports.push((path.clone(), report));
                }
            }

            // Sort files by number of hazards (most hazardous first)
            per_file_reports.sort_by(|a, b| b.1.hazards.len().cmp(&a.1.hazards.len()));

            // Compute totals from retained files only (accurate counts)
            let mut total_critical = 0usize;
            let mut total_high = 0usize;
            let mut total_medium = 0usize;
            for (_, report) in &per_file_reports {
                for h in &report.hazards {
                    if severity_num(&h.severity) >= severity_threshold {
                        match h.severity {
                            HazardSeverity::Critical => total_critical += 1,
                            HazardSeverity::High => total_high += 1,
                            HazardSeverity::Medium => total_medium += 1,
                        }
                    }
                }
            }

            (
                per_file_reports,
                total_critical,
                total_high,
                total_medium,
                files_scanned,
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let (per_file_reports, total_critical, total_high, total_medium, files_scanned) = result;

        if output_json {
            let json_data: Vec<serde_json::Value> = per_file_reports
                .iter()
                .map(|(path, report)| {
                    serde_json::json!({
                        "file_path": path,
                        "async_readiness": report.async_readiness,
                        "critical_count": report.critical_count,
                        "high_count": report.high_count,
                        "medium_count": report.medium_count,
                        "hazards": report.hazards,
                    })
                })
                .collect();
            let json = serde_json::to_string_pretty(&serde_json::json!({
                "files_scanned": files_scanned,
                "total_critical": total_critical,
                "total_high": total_high,
                "total_medium": total_medium,
                "files_with_hazards": per_file_reports.len(),
                "reports": json_data,
            }))
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let mut out = format!(
            "# Sync Hazard Analysis\n\n\
             **Files scanned**: {} | **Files with hazards**: {}\n\
             **Critical**: {} | **High**: {} | **Medium**: {}\n\
             **Min severity filter**: {}\n\n",
            files_scanned,
            per_file_reports.len(),
            total_critical,
            total_high,
            total_medium,
            req.min_severity.as_str(),
        );

        if per_file_reports.is_empty() {
            out.push_str("No sync hazards found above the severity threshold. The codebase is relatively clean for async migration.\n");
        } else {
            for (path, report) in &per_file_reports {
                out.push_str(&format!(
                    "## {} (readiness: {:.0}%)\n\n",
                    path,
                    report.async_readiness * 100.0,
                ));
                out.push_str(
                    "| Line | Pattern | Severity | Risk | Matched | Modern Equivalent |\n",
                );
                out.push_str(
                    "|------|---------|----------|------|---------|-------------------|\n",
                );

                for h in &report.hazards {
                    let sev_num: u8 = match h.severity {
                        engram_index::sync_hazard_detector::HazardSeverity::Medium => 0,
                        engram_index::sync_hazard_detector::HazardSeverity::High => 1,
                        engram_index::sync_hazard_detector::HazardSeverity::Critical => 2,
                    };
                    if sev_num < severity_threshold {
                        continue;
                    }
                    let matched_clean = h.matched_text.replace('|', "\\|");
                    let modern_clean = h.modern_equivalent.replace('|', "\\|");
                    out.push_str(&format!(
                        "| {} | {} | {} | {:?} | `{}` | {} |\n",
                        h.line_number,
                        h.pattern_type,
                        h.severity,
                        h.migration_risk,
                        if matched_clean.len() > 60 {
                            format!("{}...", &matched_clean[..57])
                        } else {
                            matched_clean
                        },
                        modern_clean,
                    ));
                }
                out.push('\n');
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    /// 37-W5: jQuery usage inventory.
    pub async fn handle_get_jquery_inventory(
        &self,
        req: GetJQueryInventoryRequest,
    ) -> Result<CallToolResult, McpError> {
        let rec = self.ensure_project_record(&req.project_id).await?;
        let project_dir = rec.directory.clone();
        let file_filter = req.file_filter.clone();
        let output_json = req.output_json;

        // Discover JS + markup files
        let (js_disc, markup_disc) = tokio::join!(
            discover_files_recursive(std::path::Path::new(&project_dir), &[".js"], 5_000,),
            discover_files_recursive(
                std::path::Path::new(&project_dir),
                &[".aspx", ".ascx", ".master", ".html", ".htm"],
                5_000,
            ),
        );

        // Apply file filter if present
        let filter_matches = |path: &str| -> bool {
            if let Some(ref filter) = file_filter {
                let path_lower = path.to_lowercase();
                let filter_lower = filter.to_lowercase();
                // Support simple glob-like filtering
                if let Some(suffix) = filter_lower.strip_prefix('*') {
                    path_lower.ends_with(suffix)
                } else if let Some(prefix) = filter_lower.strip_suffix('*') {
                    let base = Path::new(path)
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy();
                    base.to_lowercase().starts_with(prefix)
                } else {
                    path_lower.contains(&filter_lower)
                }
            } else {
                true
            }
        };

        let js_files: Vec<(String, String)> = js_disc
            .into_iter()
            .filter(|p| filter_matches(p))
            .filter_map(|rel| {
                let full = safe_join(Path::new(&project_dir), &rel).ok()?;
                match std::fs::read_to_string(&full) {
                    Ok(c) => Some((rel, c)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => {
                        tracing::warn!(
                            "ENG-AUD-2026-S10-0003: failed to read JS file {:?}: {}",
                            full,
                            e
                        );
                        None
                    }
                }
            })
            .collect();

        let markup_files: Vec<(String, String)> = markup_disc
            .into_iter()
            .filter(|p| filter_matches(p))
            .filter_map(|rel| {
                let full = safe_join(Path::new(&project_dir), &rel).ok()?;
                match std::fs::read_to_string(&full) {
                    Ok(c) => Some((rel, c)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => {
                        tracing::warn!(
                            "ENG-AUD-2026-S10-0003: failed to read markup file {:?}: {}",
                            full,
                            e
                        );
                        None
                    }
                }
            })
            .collect();

        if js_files.is_empty() && markup_files.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No JS or markup files found matching the filter.",
            )]));
        }

        let result = tokio::task::spawn_blocking(move || {
            let js_refs: Vec<(&str, &str)> = js_files
                .iter()
                .map(|(p, c)| (p.as_str(), c.as_str()))
                .collect();
            let markup_refs: Vec<(&str, &str)> = markup_files
                .iter()
                .map(|(p, c)| (p.as_str(), c.as_str()))
                .collect();
            engram_index::jquery_inventory::build_jquery_inventory(&js_refs, &markup_refs)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if output_json {
            let json = serde_json::to_string_pretty(&result)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let mut out = String::with_capacity(4096);
        out.push_str("# jQuery Inventory\n\n");
        out.push_str(&format!(
            "**Files analyzed**: {} | **Total usages**: {}\n\n",
            result.files_analyzed, result.total_usages,
        ));

        // Core version
        if let Some(ref ver) = result.core_version {
            out.push_str(&format!("## jQuery Core: v{}", ver));
            if result.core_vulnerable {
                out.push_str(" ⚠️ VULNERABLE");
            }
            out.push('\n');
            if !result.vulnerability_notes.is_empty() {
                for note in &result.vulnerability_notes {
                    out.push_str(&format!("- {}\n", note));
                }
            }
            out.push('\n');
        } else {
            out.push_str("## jQuery Core: _version not detected_\n\n");
        }

        // UI Widgets
        if !result.ui_widgets.is_empty() {
            out.push_str("## jQuery UI Widgets\n\n");
            out.push_str("| Widget | File | Line | Modern Equivalent | Complexity |\n");
            out.push_str("|--------|------|------|-------------------|------------|\n");
            for w in &result.ui_widgets {
                out.push_str(&format!(
                    "| {} | {} | {} | {} | {} |\n",
                    w.name.replace('|', "\\|"),
                    w.file_path.replace('|', "\\|"),
                    w.line_number,
                    w.modern_equivalent.replace('|', "\\|"),
                    w.migration_complexity,
                ));
            }
            out.push('\n');
        }

        // Third-party plugins
        if !result.third_party_plugins.is_empty() {
            out.push_str("## Third-Party Plugins\n\n");
            out.push_str("| Plugin | File | Line | Modern Equivalent | Complexity |\n");
            out.push_str("|--------|------|------|-------------------|------------|\n");
            for p in &result.third_party_plugins {
                out.push_str(&format!(
                    "| {} | {} | {} | {} | {} |\n",
                    p.name.replace('|', "\\|"),
                    p.file_path.replace('|', "\\|"),
                    p.line_number,
                    p.modern_equivalent.replace('|', "\\|"),
                    p.migration_complexity,
                ));
            }
            out.push('\n');
        }

        // Custom plugins
        if !result.custom_plugins.is_empty() {
            out.push_str("## Custom Plugins\n\n");
            out.push_str("| Plugin | File | Line |\n");
            out.push_str("|--------|------|------|\n");
            for p in &result.custom_plugins {
                out.push_str(&format!(
                    "| {} | {} | {} |\n",
                    p.name.replace('|', "\\|"),
                    p.file_path.replace('|', "\\|"),
                    p.line_number,
                ));
            }
            out.push('\n');
        }

        // Deprecated patterns
        if !result.deprecated_patterns.is_empty() {
            out.push_str("## Deprecated Patterns\n\n");
            out.push_str("| Pattern | File | Line | Recommendation | Complexity |\n");
            out.push_str("|---------|------|------|----------------|------------|\n");
            for d in &result.deprecated_patterns {
                out.push_str(&format!(
                    "| {} | {} | {} | {} | {} |\n",
                    d.name.replace('|', "\\|"),
                    d.file_path.replace('|', "\\|"),
                    d.line_number,
                    d.modern_equivalent.replace('|', "\\|"),
                    d.migration_complexity,
                ));
            }
            out.push('\n');
        }

        if result.total_usages == 0 {
            out.push_str("No jQuery usage detected in the analyzed files.\n");
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    // ── 37-W6: get_session_workflows ──────────────────────────────────────────

    pub async fn handle_get_session_workflows(
        &self,
        req: crate::models::GetSessionWorkflowsRequest,
    ) -> Result<CallToolResult, McpError> {
        let _rec = self.ensure_project_record(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let project_id = req.project_id.clone();
        let scope_filter = req.scope_filter.clone();
        let key_filter = req.key_filter.clone();
        let warnings_only = req.warnings_only;
        let output_json = req.output_json;

        let result = tokio::task::spawn_blocking(move || {
            use crate::services::session_workflow_service::{
                FlowPattern, reconstruct_session_workflows,
            };

            let mut report = reconstruct_session_workflows(&graph, &project_id);

            // Apply scope filter
            if let Some(ref scope) = scope_filter {
                let scope_lower = scope.to_lowercase();
                report
                    .workflows
                    .retain(|f| f.scope.to_string().to_lowercase() == scope_lower);
                report.total_keys = report.workflows.len();
                report.cross_page_chains = report
                    .workflows
                    .iter()
                    .filter(|f| {
                        let all_files: std::collections::HashSet<&str> = f
                            .writers
                            .iter()
                            .chain(f.readers.iter())
                            .map(|op| op.file.as_str())
                            .collect();
                        all_files.len() > 1
                    })
                    .count();
            }

            // Apply key filter (case-insensitive partial match)
            if let Some(ref key_pat) = key_filter {
                let pat_lower = key_pat.to_lowercase();
                report
                    .workflows
                    .retain(|f| f.key.to_lowercase().contains(&pat_lower));
                report.total_keys = report.workflows.len();
            }

            // Warnings-only filter
            if warnings_only {
                report.workflows.retain(|f| {
                    matches!(
                        f.pattern,
                        FlowPattern::MissingWriter
                            | FlowPattern::MissingReader
                            | FlowPattern::ComplexWorkflow
                    )
                });
                report.total_keys = report.workflows.len();
            }

            (report, output_json)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let (report, output_json) = result;

        if output_json {
            let json = serde_json::to_string_pretty(&report)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let md =
            crate::services::session_workflow_service::render_session_workflows_markdown(&report);
        if md.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No session/state workflows found. Ensure the project has been indexed with WebForms extraction enabled.",
            )]));
        }

        Ok(CallToolResult::success(vec![Content::text(md)]))
    }

    // ── 37-W7: get_vb_translation_traps ───────────────────────────────────────

    pub async fn handle_get_vb_translation_traps(
        &self,
        req: crate::models::GetVbTranslationTrapsRequest,
    ) -> Result<CallToolResult, McpError> {
        let rec = self.ensure_project_record(&req.project_id).await?;
        let project_dir = rec.directory.clone();
        let file_path = req.file_path.clone();
        let risk_filter = req.risk_filter.clone();
        let output_json = req.output_json;

        let result = tokio::task::spawn_blocking(move || {
            use engram_index::vb_translation_traps::detect_vb_translation_traps;

            // Collect VB files
            let vb_files: Vec<(String, String)> = if let Some(ref specific) = file_path {
                let full = safe_join(Path::new(&project_dir), specific)
                    .map_err(|e| format!("Path validation: {e}"))?;
                match std::fs::read_to_string(&full) {
                    Ok(content) => vec![(specific.clone(), content)],
                    Err(e) => {
                        return Err(format!("Cannot read file '{}': {}", specific, e));
                    }
                }
            } else {
                // Discover all .vb files
                let mut files = Vec::new();
                fn walk_vb(
                    dir: &std::path::Path,
                    base: &std::path::Path,
                    out: &mut Vec<(String, String)>,
                ) {
                    let Ok(entries) = std::fs::read_dir(dir) else {
                        return;
                    };
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.is_dir() {
                            let name = path.file_name().unwrap_or_default().to_string_lossy();
                            if name.starts_with('.')
                                || name == "node_modules"
                                || name == "bin"
                                || name == "obj"
                                || name == "packages"
                            {
                                continue;
                            }
                            walk_vb(&path, base, out);
                        } else if path
                            .extension()
                            .map(|e| e.eq_ignore_ascii_case("vb"))
                            .unwrap_or(false)
                            && let Ok(content) = std::fs::read_to_string(&path)
                        {
                            let rel = path.strip_prefix(base).unwrap_or(&path);
                            out.push((rel.to_string_lossy().to_string(), content));
                        }
                    }
                }
                walk_vb(
                    std::path::Path::new(&project_dir),
                    std::path::Path::new(&project_dir),
                    &mut files,
                );
                files
            };

            let code_refs: Vec<(&str, &str)> = vb_files
                .iter()
                .map(|(p, c)| (p.as_str(), c.as_str()))
                .collect();
            let mut report = detect_vb_translation_traps(&code_refs);

            // Apply risk filter
            if let Some(ref rf) = risk_filter {
                let rf_lower = rf.to_lowercase();
                report.traps.retain(|t| t.risk.to_lowercase() == rf_lower);
                report.total_traps = report.traps.len();
                report.silent_bug_count = report
                    .traps
                    .iter()
                    .filter(|t| t.risk == "silent_bug")
                    .count();
                report.compile_error_count = report
                    .traps
                    .iter()
                    .filter(|t| t.risk == "compile_error")
                    .count();
                // Recompute by-category
                report.traps_by_category.clear();
                for t in &report.traps {
                    *report.traps_by_category.entry(t.trap.clone()).or_insert(0) += 1;
                }
            }

            Ok(report)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let report = result.map_err(|e| McpError::invalid_params(e, None))?;

        if output_json {
            let json = serde_json::to_string_pretty(&report)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        // Render markdown
        let mut md = String::with_capacity(4_000);
        md.push_str("# VB.NET Translation Traps\n\n");
        md.push_str(&format!(
            "- **Files analyzed**: {}\n- **Total traps**: {}\n- **Silent bugs**: {} ⚠️\n- **Compile errors**: {}\n\n",
            report.files_analyzed, report.total_traps, report.silent_bug_count, report.compile_error_count
        ));

        if !report.traps_by_category.is_empty() {
            md.push_str("## Traps by Category\n\n");
            md.push_str("| Category | Count |\n");
            md.push_str("|----------|-------|\n");
            for (cat, count) in &report.traps_by_category {
                md.push_str(&format!("| {} | {} |\n", cat, count));
            }
            md.push('\n');
        }

        if !report.traps.is_empty() {
            md.push_str("## All Traps\n\n");
            md.push_str("| Location | Category | Risk | VB Code | Guidance |\n");
            md.push_str("|----------|----------|------|---------|----------|\n");
            for t in &report.traps {
                md.push_str(&format!(
                    "| {} | {} | {} | `{}` | {} |\n",
                    t.location.replace('|', "\\|"),
                    t.trap.replace('|', "\\|"),
                    if t.risk == "silent_bug" {
                        "⚠️ silent_bug"
                    } else {
                        "🔴 compile_error"
                    },
                    t.vb_code.replace('|', "\\|").replace('`', "'"),
                    t.guidance.replace('|', "\\|"),
                ));
            }
            md.push('\n');
        }

        if report.total_traps == 0 {
            md.push_str("No VB.NET translation traps detected.\n");
        }

        Ok(CallToolResult::success(vec![Content::text(md)]))
    }
}
