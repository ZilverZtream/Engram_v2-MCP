use crate::models::{
    AnalyzeFullProjectMigrationRequest, AnalyzeViewStateDepsRequest, CheckMigrationCoverageRequest,
    GenerateCharacterizationTestsRequest, GenerateInstrumentationCodeRequest,
    GenerateMigrationBlueprintRequest, GenerateMigrationPlanRequest,
    GenerateMigrationScaffoldRequest, GenerateStranglerFigRequest, GetInstrumentationPackRequest,
    GetMigrationDossierRequest, GetMigrationProgressRequest, IngestInstrumentationLogsRequest,
    MapAuthConfigRequest, MapPageLifecycleRequest,
    MapValidationControlsRequest, ReconcileRuntimeEvidenceRequest,
    SuggestMigrationBoundariesRequest, SuggestMigrationOrderRequest, SuggestStateMigrationRequest,
    TraceDataFlowRequest, UpdateMigrationStatusRequest,
};
use crate::services::{full_project_migration_service as full_mig, graph_service};
use crate::tools::Engram;
use crate::utils::files::{discover_files_recursive, find_codebehind_path, find_aspx_for_codebehind};
use rmcp::model::{CallToolResult, Content};
use rmcp::ErrorData as McpError;
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
        let target = req.target_stack.clone();
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

        let cb_full = Path::new(&rec.directory).join(&file_path);
        let cb_content = tokio::fs::read_to_string(&cb_full).await.map_err(|e| {
            McpError::internal_error(format!("Failed to read {}: {e}", cb_full.display()), None)
        })?;

        let result = tokio::task::spawn_blocking(move || {
            crate::services::data_flow_service::trace_data_flow(
                &graph, &pid, &file_path, &entry_point, &cb_content,
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(format!("Trace: {:?}", result.steps))]))
    }

    pub async fn handle_get_migration_dossier(
        &self,
        req: GetMigrationDossierRequest,
    ) -> Result<CallToolResult, McpError> {
        let rec = self.ensure_project_record(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let file_path = req.file_path.clone();
        let target_stack = req.target_stack.clone();
        let project_dir = rec.directory.clone();

        let aspx_full = Path::new(&project_dir).join(&file_path);
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
                &graph, &pid, &file_path, &aspx_content, &cb_content, None, &target_stack,
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(format!("Dossier for {}", result.file_path))]))
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
                &graph, &pid, &original_file, &modern_code,
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(format!("Coverage: {:.1}%", result.coverage_score * 100.0))]))
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
        let target_stack = req.target_stack.clone();
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
                    let full_path = std::path::Path::new(&dir).join(&rel);
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
                    let full_path = std::path::Path::new(&dir).join(&rel);
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
                    let full_path = std::path::Path::new(&dir).join(&rel);
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
                    let full_path = std::path::Path::new(&dir).join(&rel);
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

        let webconfig_path = std::path::Path::new(&project_dir).join("web.config");
        let webconfig_content = tokio::fs::read_to_string(&webconfig_path).await.ok();
        let webconfig_content = if webconfig_content.is_none() {
            let alt = std::path::Path::new(&project_dir).join("Web.config");
            tokio::fs::read_to_string(&alt).await.ok()
        } else {
            webconfig_content
        };

        let global_asax = {
            let ga_path = std::path::Path::new(&project_dir).join("Global.asax");
            let ga_exists = has_global_asax || ga_path.exists();
            if ga_exists {
                let markup = tokio::fs::read_to_string(&ga_path)
                    .await
                    .unwrap_or_default();
                let cb = {
                    let cs = std::path::Path::new(&project_dir).join("Global.asax.cs");
                    let vb = std::path::Path::new(&project_dir).join("Global.asax.vb");
                    if let Ok(content) = tokio::fs::read_to_string(&cs).await {
                        Some(content)
                    } else {
                        tokio::fs::read_to_string(&vb).await.ok()
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
                let full = std::path::Path::new(&project_dir).join(&rel);
                std::fs::read_to_string(&full).ok().map(|c| (rel, c))
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
                let full = std::path::Path::new(&project_dir).join(&rel);
                let content = std::fs::read_to_string(&full).ok()?;
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
                let full = std::path::Path::new(&project_dir).join(&rel);
                std::fs::read_to_string(&full).ok().map(|c| (rel, c))
            })
            .collect();

        let packages_config_files: Vec<(String, String)> = pkgconfig_paths
            .into_iter()
            .filter(|p| p.ends_with("packages.config"))
            .filter_map(|rel| {
                let full = std::path::Path::new(&project_dir).join(&rel);
                std::fs::read_to_string(&full).ok().map(|c| (rel, c))
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
                let full = std::path::Path::new(&project_dir).join(&rel);
                std::fs::read_to_string(&full).ok().map(|c| (rel, c))
            })
            .collect();

        let resx_files: Vec<(String, String)> = resx_paths
            .into_iter()
            .filter_map(|rel| {
                let full = std::path::Path::new(&project_dir).join(&rel);
                std::fs::read_to_string(&full).ok().map(|c| (rel, c))
            })
            .collect();

        let master_files: Vec<(String, String)> = master_paths
            .into_iter()
            .filter_map(|rel| {
                let full = std::path::Path::new(&project_dir).join(&rel);
                std::fs::read_to_string(&full).ok().map(|c| (rel, c))
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

        let report = tokio::task::spawn_blocking(move || {
            full_mig::analyze_full_project(&graph, &pid, &target_stack, &bundle, max_files)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if req.output_json {
            let json = serde_json::to_string_pretty(&report)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        Ok(CallToolResult::success(vec![Content::text(report.markdown_report)]))
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
                out.push_str(&format!("- {} → {} [{}] (evidence: {})\n", p.source, p.target, p.edge_kind, p.runtime_evidence.as_deref().unwrap_or("confirmed")));
            }
            out.push('\n');
        }

        if !report.contradicted_paths.is_empty() {
            out.push_str("## Contradicted Paths\n");
            for p in &report.contradicted_paths {
                out.push_str(&format!("- {} → {} [{}] (source/target seen but not this path)\n", p.source, p.target, p.edge_kind));
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
        let framework = req.framework.clone();

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

        let aspx_full = Path::new(&rec.directory).join(&file_path);
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

        let webconfig_path = std::path::Path::new(&project_dir).join("web.config");
        let webconfig_content = tokio::fs::read_to_string(&webconfig_path).await.ok();
        let webconfig_content = if webconfig_content.is_none() {
            let alt = std::path::Path::new(&project_dir).join("Web.config");
            tokio::fs::read_to_string(&alt).await.ok()
        } else {
            webconfig_content
        };

        let code_files = if let Some(ref scope) = req.file_scope {
            let full = std::path::Path::new(&project_dir).join(scope);
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
                    if path.ends_with(".vb")
                        || path.ends_with(".cs")
                        || path.ends_with(".aspx.vb")
                        || path.ends_with(".aspx.cs")
                    {
                        let full = std::path::Path::new(&dir).join(path);
                        if let Ok(content) = std::fs::read_to_string(&full) {
                            files.push((path.clone(), content));
                        }
                    }
                }
                files
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
        };

        let result = tokio::task::spawn_blocking(move || {
            let code_files_refs: Vec<(&str, &str)> = code_files.iter().map(|(p, c)| (p.as_str(), c.as_str())).collect();
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
                out.push_str(&format!("- **[{}]** {}: {}\n", rec.severity, rec.category, rec.recommendation));
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

        let cb_full = std::path::Path::new(&rec.directory).join(&file_path);
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

        let cb_full = std::path::Path::new(&rec.directory).join(&file_path);
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
                out.push_str(&format!("- **Modern strategy**: {}\n", field.blazor_declaration));
                out.push('\n');
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }
}
