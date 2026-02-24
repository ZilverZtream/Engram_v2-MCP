use crate::models::*;
use crate::state::AppState;
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::{tool::Parameters, tool::ToolRouter},
    model::*,
    tool, tool_handler, tool_router,
    transport::stdio,
};

#[derive(Clone)]
pub struct Engram {
    pub state: AppState,
    pub tool_router: ToolRouter<Engram>,
}

#[tool_router]
impl Engram {
    pub fn new(state: AppState) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }

    pub async fn process_ingest_stats_for_test(
        &self,
        project_id: &str,
        generation: u64,
        stats: &engram_index::IngestStats,
    ) -> anyhow::Result<()> {
        self.process_ingest_stats(project_id, generation, stats)
            .await
    }

    // ---- Project lifecycle ----

    #[tool(description = "Index a local directory to make it searchable.")]
    pub async fn index_project(
        &self,
        params: Parameters<IndexProjectRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_index_project(params.0).await
    }

    #[tool(description = "Update a project index + git intelligence.")]
    pub async fn update_project(
        &self,
        params: Parameters<UpdateProjectRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_update_project(params.0).await
    }

    #[tool(description = "List indexed projects.")]
    pub async fn list_projects(&self) -> Result<CallToolResult, McpError> {
        self.handle_list_projects().await
    }

    #[tool(description = "Get info about a project.")]
    pub async fn project_info(
        &self,
        params: Parameters<ProjectIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_project_info(params.0).await
    }

    #[tool(description = "Comprehensive project health check.")]
    pub async fn project_health(
        &self,
        params: Parameters<ProjectIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_project_health(params.0).await
    }

    #[tool(description = "Repair a project index with targeted scope.")]
    pub async fn repair_project(
        &self,
        params: Parameters<RepairProjectRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_repair_project(params.0).await
    }

    #[tool(description = "Delete a project and its stored data.")]
    pub async fn delete_project(
        &self,
        params: Parameters<ProjectIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_delete_project(params.0).await
    }

    #[tool(description = "Enable/disable watching a project directory.")]
    pub async fn watch_project(
        &self,
        params: Parameters<WatchProjectRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_watch_project(params.0).await
    }

    #[tool(description = "Disable watching a project directory.")]
    pub async fn unwatch_project(
        &self,
        params: Parameters<ProjectIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_unwatch_project(params.0).await
    }

    // ---- Search + chunks ----

    #[tool(description = "Search the indexed code/docs.")]
    pub async fn search_memory(
        &self,
        params: Parameters<SearchMemoryRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_search_memory(params.0).await
    }

    #[tool(description = "Pure semantic vector search.")]
    pub async fn vector_search(
        &self,
        params: Parameters<VectorSearchRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_vector_search(params.0).await
    }

    #[tool(description = "Fetch full content for a chunk.")]
    pub async fn get_chunk(
        &self,
        params: Parameters<GetChunkRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_chunk(params.0).await
    }

    // ---- Memory bank + repo rules ----

    #[tool(description = "Create/update a memory bank section.")]
    pub async fn update_memory_bank(
        &self,
        params: Parameters<UpdateMemoryBankRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_update_memory_bank(params.0).await
    }

    #[tool(description = "List memory bank sections.")]
    pub async fn list_memory_bank(
        &self,
        params: Parameters<ProjectIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_list_memory_bank(params.0).await
    }

    #[tool(description = "Read a memory bank section.")]
    pub async fn read_memory_bank(
        &self,
        params: Parameters<MemorySectionRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_read_memory_bank(params.0).await
    }

    #[tool(description = "Delete a memory bank section.")]
    pub async fn delete_memory_bank(
        &self,
        params: Parameters<MemorySectionRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_delete_memory_bank(params.0).await
    }

    #[tool(description = "Add a repo rule/constraint.")]
    pub async fn add_repo_rule(
        &self,
        params: Parameters<AddRepoRuleRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_add_repo_rule(params.0).await
    }

    #[tool(description = "List repo rules.")]
    pub async fn list_repo_rules(
        &self,
        params: Parameters<ProjectIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_list_repo_rules(params.0).await
    }

    #[tool(description = "Delete a repo rule.")]
    pub async fn delete_repo_rule(
        &self,
        params: Parameters<DeleteRepoRuleRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_delete_repo_rule(params.0).await
    }

    // ---- Graph tools ----

    #[tool(description = "Query graph nodes by substring.")]
    pub async fn query_graph_nodes(
        &self,
        params: Parameters<QueryGraphNodesRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_query_graph_nodes(params.0).await
    }

    #[tool(description = "Find graph references from a node.")]
    pub async fn find_references(
        &self,
        params: Parameters<FindReferencesRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_find_references(params.0).await
    }

    #[tool(description = "Graph-boosted search.")]
    pub async fn graph_search(
        &self,
        params: Parameters<GraphSearchRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_graph_search(params.0).await
    }

    #[tool(description = "Multi-hop graph traversal (BFS).")]
    pub async fn traverse_graph(
        &self,
        params: Parameters<TraverseGraphRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_traverse_graph(params.0).await
    }

    // ---- Git/history tools ----

    #[tool(description = "Index git history.")]
    pub async fn index_git_history(
        &self,
        params: Parameters<IndexGitHistoryRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_index_git_history(params.0).await
    }

    #[tool(description = "Ingest zip snapshots as history.")]
    pub async fn ingest_zip_history(
        &self,
        params: Parameters<IngestZipHistoryRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_ingest_zip_history(params.0).await
    }

    #[tool(description = "Search git history.")]
    pub async fn search_history(
        &self,
        params: Parameters<SearchHistoryRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_search_history(params.0).await
    }

    #[tool(description = "Analyze temporal couplings for a file.")]
    pub async fn analyze_temporal_couplings(
        &self,
        params: Parameters<AnalyzeTemporalCouplingsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_analyze_temporal_couplings(params.0).await
    }

    #[tool(description = "Analyze reverts (Immune System).")]
    pub async fn analyze_reverts(
        &self,
        params: Parameters<AnalyzeRevertsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_analyze_reverts(params.0).await
    }

    // ---- Agent / cognitive tools ----

    #[tool(description = "Analyze impact of changes.")]
    pub async fn impact_analysis(
        &self,
        params: Parameters<ImpactAnalysisRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_impact_analysis(params.0).await
    }

    #[tool(description = "Get database table schema.")]
    pub async fn get_table_schema(
        &self,
        params: Parameters<GetTableSchemaRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_table_schema(params.0).await
    }

    #[tool(description = "Trace global state usage.")]
    pub async fn trace_state_usage(
        &self,
        params: Parameters<TraceStateUsageRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_trace_state_usage(params.0).await
    }

    #[tool(description = "Trace paths from UI to SQL.")]
    pub async fn trace_ui_event(
        &self,
        params: Parameters<TraceUiEventRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_trace_ui_event(params.0).await
    }

    #[tool(description = "Trace UI action to code.")]
    pub async fn trace_ui_action(
        &self,
        params: Parameters<TraceUiActionRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_trace_ui_action(params.0).await
    }

    #[tool(description = "Export capture pack for agents.")]
    pub async fn export_capture_pack(
        &self,
        params: Parameters<ExportCapturePackRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_export_capture_pack(params.0).await
    }

    #[tool(description = "Get UI layout tree.")]
    pub async fn get_ui_blueprint(
        &self,
        params: Parameters<GetUiBlueprintRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_ui_blueprint(params.0).await
    }

    #[tool(description = "Get codebase overview.")]
    pub async fn get_codebase_overview(
        &self,
        params: Parameters<ProjectIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_codebase_overview(params.0).await
    }

    #[tool(description = "Find all references to a symbol.")]
    pub async fn find_symbol_references(
        &self,
        params: Parameters<FindSymbolReferencesRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_find_symbol_references(params.0).await
    }

    #[tool(description = "Analyze error stacktrace.")]
    pub async fn analyze_error_stack(
        &self,
        params: Parameters<AnalyzeErrorStackRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_analyze_error_stack(params.0).await
    }

    #[tool(description = "Trigger a dream cycle.")]
    pub async fn dream_project(
        &self,
        params: Parameters<DreamProjectRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_dream_project(params.0).await
    }

    #[tool(description = "Alias for dream_project.")]
    pub async fn trigger_rem_cycle(
        &self,
        params: Parameters<DreamProjectRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_dream_project(params.0).await
    }

    #[tool(description = "Analyze coding style.")]
    pub async fn analyze_file_coding_style(
        &self,
        params: Parameters<AnalyzeFileCodingStyleRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_analyze_file_coding_style(params.0).await
    }

    #[tool(description = "List background jobs.")]
    pub async fn list_jobs(
        &self,
        params: Parameters<ListJobsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_list_jobs(params.0).await
    }

    #[tool(description = "Cancel a background job.")]
    pub async fn cancel_job(
        &self,
        params: Parameters<CancelJobRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_cancel_job(params.0).await
    }

    #[tool(description = "Get job status.")]
    pub async fn get_job_status(
        &self,
        params: Parameters<CancelJobRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_job_status(params.0).await
    }

    #[tool(description = "Immune system check.")]
    pub async fn immune_check(
        &self,
        params: Parameters<ImmuneCheckRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_immune_check(params.0).await
    }

    #[tool(description = "Anti-pattern guard.")]
    pub async fn anti_pattern_guard(
        &self,
        params: Parameters<AntiPatternGuardRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_anti_pattern_guard(params.0).await
    }

    #[tool(description = "Generate instrumentation pack.")]
    pub async fn get_instrumentation_pack(
        &self,
        params: Parameters<GetInstrumentationPackRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_instrumentation_pack(params.0).await
    }

    #[tool(description = "Suggest migration boundaries.")]
    pub async fn suggest_migration_boundaries(
        &self,
        params: Parameters<SuggestMigrationBoundariesRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_suggest_migration_boundaries(params.0).await
    }

    #[tool(description = "Ingest instrumentation logs.")]
    pub async fn ingest_instrumentation_logs(
        &self,
        params: Parameters<IngestInstrumentationLogsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_ingest_instrumentation_logs(params.0).await
    }

    #[tool(
        description = "Ingest runtime artifacts (IIS logs, traces, lifecycle snapshots, SQL profiler exports) and merge runtime/static graph evidence."
    )]
    pub async fn ingest_runtime_artifacts(
        &self,
        params: Parameters<IngestRuntimeArtifactsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_ingest_runtime_artifacts(params.0).await
    }

    #[tool(description = "Compile migration blueprint.")]
    pub async fn generate_migration_blueprint(
        &self,
        params: Parameters<GenerateMigrationBlueprintRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_generate_migration_blueprint(params.0).await
    }

    #[tool(description = "Generate AST dependency graph.")]
    pub async fn ast_dependency_graph(
        &self,
        params: Parameters<AstDependencyGraphRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_ast_dependency_graph(params.0).await
    }

    #[tool(description = "Incremental indexing GC.")]
    pub async fn incremental_indexing_gc(
        &self,
        params: Parameters<IncrementalIndexingGcRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_incremental_indexing_gc(params.0).await
    }

    #[tool(description = "Manage anti-pattern index.")]
    pub async fn dedicated_antipattern_index(
        &self,
        params: Parameters<AntipatternIndexRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_dedicated_antipattern_index(params.0).await
    }

    #[tool(description = "Get server metrics.")]
    pub async fn get_metrics(
        &self,
        params: Parameters<GetMetricsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_metrics(params.0).await
    }

    #[tool(description = "Run integrity check.")]
    pub async fn check_integrity(
        &self,
        params: Parameters<CheckIntegrityRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_check_integrity(params.0).await
    }

    #[tool(description = "Evaluate safety of changes.")]
    pub async fn evaluate_safety(
        &self,
        params: Parameters<EvaluateSafetyRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_evaluate_safety(params.0).await
    }

    #[tool(description = "Generate migration plan.")]
    pub async fn generate_migration_plan(
        &self,
        params: Parameters<GenerateMigrationPlanRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_generate_migration_plan(params.0).await
    }

    #[tool(description = "Benchmark retrieval quality.")]
    pub async fn benchmark_retrieval(
        &self,
        params: Parameters<BenchmarkRetrievalRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_benchmark_retrieval(params.0).await
    }

    #[tool(description = "Score extraction confidence.")]
    pub async fn get_extraction_confidence(
        &self,
        params: Parameters<GetExtractionConfidenceRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_extraction_confidence(params.0).await
    }

    #[tool(description = "Get checkpoint status.")]
    pub async fn get_checkpoint_status(
        &self,
        params: Parameters<GetCheckpointStatusRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_checkpoint_status(params.0).await
    }

    #[tool(description = "Get memory budget status.")]
    pub async fn get_memory_budget(
        &self,
        params: Parameters<GetMemoryBudgetRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_memory_budget(params.0).await
    }

    #[tool(description = "Compute migration blast radius.")]
    pub async fn compute_blast_radius(
        &self,
        params: Parameters<ComputeBlastRadiusRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_compute_blast_radius(params.0).await
    }

    #[tool(description = "Detect design anti-patterns.")]
    pub async fn detect_design_patterns(
        &self,
        params: Parameters<DetectDesignPatternsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_detect_design_patterns(params.0).await
    }

    #[tool(description = "Autonomous decision gate.")]
    pub async fn autonomous_decision_gate(
        &self,
        params: Parameters<AutonomousDecisionGateRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_autonomous_decision_gate(params.0).await
    }

    #[tool(description = "Graph centrality rerank.")]
    pub async fn graph_centrality_rerank(
        &self,
        params: Parameters<GraphCentralityRerankRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_graph_centrality_rerank(params.0).await
    }

    #[tool(description = "Generate migration scaffold.")]
    pub async fn generate_migration_scaffold(
        &self,
        params: Parameters<GenerateMigrationScaffoldRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_generate_migration_scaffold(params.0).await
    }

    #[tool(description = "Generate instrumentation code.")]
    pub async fn generate_instrumentation_code(
        &self,
        params: Parameters<GenerateInstrumentationCodeRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_generate_instrumentation_code(params.0).await
    }

    #[tool(description = "Reconcile runtime evidence.")]
    pub async fn reconcile_runtime_evidence(
        &self,
        params: Parameters<ReconcileRuntimeEvidenceRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_reconcile_runtime_evidence(params.0).await
    }

    #[tool(description = "Suggest state migration strategies.")]
    pub async fn suggest_state_migration(
        &self,
        params: Parameters<SuggestStateMigrationRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_suggest_state_migration(params.0).await
    }

    #[tool(description = "Generate characterization tests.")]
    pub async fn generate_characterization_tests(
        &self,
        params: Parameters<GenerateCharacterizationTestsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_generate_characterization_tests(params.0).await
    }

    #[tool(description = "Generate strangler fig config.")]
    pub async fn generate_strangler_fig_config(
        &self,
        params: Parameters<GenerateStranglerFigRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_generate_strangler_fig_config(params.0).await
    }

    #[tool(description = "Map validation controls.")]
    pub async fn map_validation_controls(
        &self,
        params: Parameters<MapValidationControlsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_map_validation_controls(params.0).await
    }

    #[tool(description = "Map auth config.")]
    pub async fn map_auth_config(
        &self,
        params: Parameters<MapAuthConfigRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_map_auth_config(params.0).await
    }

    #[tool(description = "Map page lifecycle.")]
    pub async fn map_page_lifecycle(
        &self,
        params: Parameters<MapPageLifecycleRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_map_page_lifecycle(params.0).await
    }

    #[tool(description = "Analyze ViewState dependencies.")]
    pub async fn analyze_viewstate_deps(
        &self,
        params: Parameters<AnalyzeViewStateDepsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_analyze_viewstate_deps(params.0).await
    }

    #[tool(description = "Map AJAX regions.")]
    pub async fn map_ajax_regions(
        &self,
        params: Parameters<MapAjaxRegionsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_map_ajax_regions(params.0).await
    }

    #[tool(description = "Trace data flow.")]
    pub async fn trace_data_flow(
        &self,
        params: Parameters<TraceDataFlowRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_trace_data_flow(params.0).await
    }

    #[tool(description = "Get migration dossier.")]
    pub async fn get_migration_dossier(
        &self,
        params: Parameters<GetMigrationDossierRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_migration_dossier(params.0).await
    }

    #[tool(description = "Check migration coverage.")]
    pub async fn check_migration_coverage(
        &self,
        params: Parameters<CheckMigrationCoverageRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_check_migration_coverage(params.0).await
    }

    #[tool(description = "Update migration status.")]
    pub async fn update_migration_status(
        &self,
        params: Parameters<UpdateMigrationStatusRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_update_migration_status(params.0).await
    }

    #[tool(description = "Get migration progress.")]
    pub async fn get_migration_progress(
        &self,
        params: Parameters<GetMigrationProgressRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_migration_progress(params.0).await
    }

    #[tool(description = "Suggest migration order.")]
    pub async fn suggest_migration_order(
        &self,
        params: Parameters<SuggestMigrationOrderRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_suggest_migration_order(params.0).await
    }

    #[tool(description = "Analyze full project migration.")]
    pub async fn analyze_full_project_migration(
        &self,
        params: Parameters<AnalyzeFullProjectMigrationRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_analyze_full_project_migration(params.0).await
    }

    #[tool(description = "Analyze business logic.")]
    pub async fn analyze_business_logic(
        &self,
        params: Parameters<crate::models::requests::AnalyzeBusinessLogicRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_analyze_business_logic(params.0).await
    }

    #[tool(description = "Query business logic summaries.")]
    pub async fn query_business_logic(
        &self,
        params: Parameters<crate::models::requests::QueryBusinessLogicRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_query_business_logic(params.0).await
    }

    // ── Phase 37: Wiring — Expose Existing Services ──────────────────────────

    #[tool(
        description = "Full database intelligence: schema, stored procedures, triggers, SP call chains, cross-reference warnings."
    )]
    pub async fn analyze_database_intelligence(
        &self,
        params: Parameters<AnalyzeDatabaseIntelligenceRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_analyze_database_intelligence(params.0).await
    }

    #[tool(
        description = "Deep analysis of a single stored procedure: purpose, parameters, tables, callers, call chain, triggers, side effects."
    )]
    pub async fn get_sp_details(
        &self,
        params: Parameters<GetSpDetailsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_sp_details(params.0).await
    }

    #[tool(
        description = "List all database triggers, optionally filtered by table. Shows which code paths indirectly fire each trigger."
    )]
    pub async fn list_triggers(
        &self,
        params: Parameters<ListTriggersRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_list_triggers(params.0).await
    }

    #[tool(
        description = "Detect sync-over-async hazards (.Result, .Wait(), Thread.Sleep, HttpContext.Current) that cause deadlocks during async migration."
    )]
    pub async fn analyze_sync_hazards(
        &self,
        params: Parameters<AnalyzeSyncHazardsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_analyze_sync_hazards(params.0).await
    }

    #[tool(
        description = "jQuery usage inventory: core version, vulnerabilities, UI widgets, third-party plugins, deprecated patterns with migration guidance."
    )]
    pub async fn get_jquery_inventory(
        &self,
        params: Parameters<GetJQueryInventoryRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_jquery_inventory(params.0).await
    }

    #[tool(
        description = "Reconstruct session/state workflows: trace how Session, Application, ViewState, Cache, and Cookie keys flow across pages. Detects MissingWriter, MissingReader, and complex cross-page chains."
    )]
    pub async fn get_session_workflows(
        &self,
        params: Parameters<GetSessionWorkflowsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_session_workflows(params.0).await
    }

    #[tool(
        description = "Detect VB.NET → C# translation traps: 14 categories of semantic differences (silent bugs like Nothing/ValueType, Is vs =; compile errors like On Error GoTo, ReDim Preserve, My.* namespace)."
    )]
    pub async fn get_vb_translation_traps(
        &self,
        params: Parameters<GetVbTranslationTrapsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_vb_translation_traps(params.0).await
    }

    // ── Phase 38: The Access Layer ────────────────────────────────────────────

    #[tool(
        description = "Fast per-method metadata lookup from the method index. Returns signature, callers, callees, DB tables, session keys, complexity, VB traps, and method kind. Sub-200ms."
    )]
    pub async fn get_method_info(
        &self,
        params: Parameters<GetMethodInfoRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_method_info(params.0).await
    }

    #[tool(
        description = "Retrieve the complete, untruncated source code of a method. Supports FQN lookup or explicit file:line range. Optionally includes caller bodies for pattern understanding."
    )]
    pub async fn get_full_method_body(
        &self,
        params: Parameters<GetFullMethodBodyRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_full_method_body(params.0).await
    }

    #[tool(
        description = "Pre-edit oracle: assembles method info, full body, all callers, database footprint, session state flows, VB traps, sync hazards, blast radius, and business logic into one response. Call this BEFORE modifying any method."
    )]
    pub async fn get_method_edit_context(
        &self,
        params: Parameters<GetMethodEditContextRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_method_edit_context(params.0).await
    }

    #[tool(
        description = "Full page context for a WebForms page: control tree, all event handlers with complete bodies, data layer, session state, AJAX regions, validation, auth requirements. The starting point for all WebForms work."
    )]
    pub async fn get_page_context(
        &self,
        params: Parameters<GetPageContextRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_page_context(params.0).await
    }

    #[tool(
        description = "LLM context packer: assembles coding style profile, pattern examples from callers, database schema for referenced tables, SP signatures, session state context, control mappings, VB translation traps, and sync hazards — everything an LLM needs to generate correct code in one call."
    )]
    pub async fn prepare_implementation_context(
        &self,
        params: Parameters<PrepareImplementationContextRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_prepare_implementation_context(params.0).await
    }

    #[tool(
        description = "Post-generation safety net: validates generated code against the project's extracted knowledge. Checks SQL table/column references, VB trap avoidance, state key consistency, SP call correctness, control ID validity, caller compatibility, and sync hazard introduction. Returns pass/warn/fail per category."
    )]
    pub async fn validate_generated_code(
        &self,
        params: Parameters<ValidateGeneratedCodeRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_validate_generated_code(params.0).await
    }

    #[tool(
        description = "Validate a SQL fragment against the project's schema knowledge: table/column existence, SP parameter types, join correctness, and common SQL anti-patterns."
    )]
    pub async fn validate_sql_fragment(
        &self,
        params: Parameters<ValidateSqlFragmentRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_validate_sql_fragment(params.0).await
    }

    #[tool(
        description = "Find existing tests that exercise a given method by searching for references in test files (*Test*, *Spec*, *_test*)."
    )]
    pub async fn find_tests_for_method(
        &self,
        params: Parameters<FindTestsForMethodRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_find_tests_for_method(params.0).await
    }

    #[tool(
        description = "Find dead methods: functions with zero callers, no Handles clause, and not lifecycle hooks. Candidates for safe removal during migration."
    )]
    pub async fn find_dead_methods(
        &self,
        params: Parameters<FindDeadMethodsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_find_dead_methods(params.0).await
    }

    #[tool(
        description = "Standalone edit safety check: returns green/yellow/red verdict for a method based on blast radius, caller count, session writes, triggers, and complexity. Faster than get_method_edit_context when you only need the verdict."
    )]
    pub async fn check_edit_safety(
        &self,
        params: Parameters<CheckEditSafetyRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_check_edit_safety(params.0).await
    }
}

#[tool_handler]
impl ServerHandler for Engram {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some("Engram MCP v2 (Rust)".to_string()),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

pub async fn run_stdio(state: AppState) -> anyhow::Result<()> {
    let service = Engram::new(state)
        .serve(stdio())
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    service
        .waiting()
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    Ok(())
}
