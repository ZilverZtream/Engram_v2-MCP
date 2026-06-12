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

    #[tool(
        description = "Index a local directory into the project's search, graph, and vector stores. Run once per project; long-running (returns a job — poll get_job_status). Use update_project for refreshes afterwards."
    )]
    pub async fn index_project(
        &self,
        params: Parameters<IndexProjectRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_index_project(params.0).await
    }

    #[tool(
        description = "Incrementally re-index changed files and refresh git intelligence. Cheap when little changed. Use when get_index_freshness reports drift; watch_project automates this."
    )]
    pub async fn update_project(
        &self,
        params: Parameters<UpdateProjectRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_update_project(params.0).await
    }

    #[tool(
        description = "List indexed projects with their project_ids. Start here when you don't know the project_id that every other tool requires."
    )]
    pub async fn list_projects(&self) -> Result<CallToolResult, McpError> {
        self.handle_list_projects().await
    }

    #[tool(
        description = "Project record for a project_id: name, type, source directory, index locations."
    )]
    pub async fn project_info(
        &self,
        params: Parameters<ProjectIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_project_info(params.0).await
    }

    #[tool(
        description = "Index health snapshot: generation, graph node/edge counts, doc/vector counts, and semantic-search tier. Use right after indexing to verify it worked, or when any tool returns surprisingly little."
    )]
    pub async fn project_health(
        &self,
        params: Parameters<ProjectIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_project_health(params.0).await
    }

    #[tool(
        description = "Repair a damaged project index (scopes: tantivy_only, vector_only, graph_only, full). Use when check_integrity or project_health reports inconsistencies."
    )]
    pub async fn repair_project(
        &self,
        params: Parameters<RepairProjectRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_repair_project(params.0).await
    }

    #[tool(
        description = "Permanently delete a project's indexes, graph, and metadata. Irreversible — the source directory is untouched."
    )]
    pub async fn delete_project(
        &self,
        params: Parameters<ProjectIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_delete_project(params.0).await
    }

    #[tool(
        description = "Enable filesystem watching: changed files are re-indexed automatically after a debounce (watch_debounce_secs). Keeps search/graph current during active editing."
    )]
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

    #[tool(
        description = "Check whether a project's index is current: active generation, time since last index, watcher status, and (by default) a count of files modified on disk since the last index. Use before trusting search/graph results, or when results look stale. Related: update_project to refresh, watch_project for auto-refresh."
    )]
    pub async fn get_index_freshness(
        &self,
        params: Parameters<GetIndexFreshnessRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_index_freshness(params.0).await
    }

    #[tool(
        description = "Convert any Engram identifier into all its identities: pass a graph node_id, a symbol name/FQN, or a search doc_id and get back node_id, name, type, file, line range, and which tools accept it. Use when chaining search output into graph tools, or when a name is ambiguous (returns all candidates)."
    )]
    pub async fn resolve_id(
        &self,
        params: Parameters<ResolveIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_resolve_id(params.0).await
    }

    #[tool(
        description = "Map EVERY touchpoint of a domain concept (e.g. 'photo', 'code category'): tables, columns, stored procs, Session/ViewState keys, pages, controls, functions, endpoints, plus who reads/writes the core anchors and files that only mention it in text. Call this FIRST when a user story names a domain concept — it's how you avoid changing 2 of the 17 places the concept lives. Related: find_implementation_pattern, find_similar_changes."
    )]
    pub async fn get_concept_footprint(
        &self,
        params: Parameters<GetConceptFootprintRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_concept_footprint(params.0).await
    }

    #[tool(
        description = "Given the files you plan to change, find the most similar historical commits and report the recurring companion artifacts MISSING from your set (admin pages, menu/sitemap entries, registrations — the things reviewers notice are absent). Call before implementing and again before committing. Scans recent git history at request time (max_commits, default 500)."
    )]
    pub async fn find_similar_changes(
        &self,
        params: Parameters<FindSimilarChangesRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_find_similar_changes(params.0).await
    }

    #[tool(
        description = "Find concrete exemplars of how THIS codebase implements a pattern (e.g. 'admin settings page save', 'dropdown bound to lookup table'): top matching files with their symbols, SQL/table/state edges, co-changed partners, and a snippet — plus the ingredients common across exemplars. Imitate the best exemplar instead of inventing a new approach. Related: get_chunk for full source."
    )]
    pub async fn find_implementation_pattern(
        &self,
        params: Parameters<FindImplementationPatternRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_find_implementation_pattern(params.0).await
    }

    #[tool(
        description = "Map the permission checks and settings that gate an area: guarded vs UNGUARDED functions in scope, settings each one reads (web.config keys + DB/env), settings-shaped DB tables with consumer counts, and the project's house auth patterns (guard helper names + roles). Call with scope=<file/dir> before adding any endpoint or admin operation; call without scope to learn how this codebase does authorization. Detection covers .NET idioms (AppSettings, My.Settings, IsInRole/Is*Admin*/Check*Access* name shapes)."
    )]
    pub async fn map_guards_and_settings(
        &self,
        params: Parameters<MapGuardsAndSettingsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_map_guards_and_settings(params.0).await
    }

    #[tool(
        description = "Re-run placeholder edge resolution on the existing graph (no reindex). Use after upgrading Engram so resolver improvements apply to already-indexed projects; returns the count of '::name' placeholder edges resolved to concrete nodes."
    )]
    pub async fn resolve_graph_edges(
        &self,
        params: Parameters<ProjectIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_resolve_graph_edges(params.0).await
    }

    #[tool(
        description = "GIS surface inventory: every map library/class the project uses with call-site counts, files, and modern equivalents (Google Maps/Leaflet/OpenLayers/Esri), per-file map configurations (api key, zoom, center), and the WMS/XYZ/Esri layer inventory. Call before touching any map feature; pairs with get_concept_footprint and blast_radius for the change plan."
    )]
    pub async fn get_gis_inventory(
        &self,
        params: Parameters<ProjectIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_gis_inventory(params.0).await
    }

    #[tool(
        description = "ONE call from a weak user story (e.g. 'As an admin I would like to set minimum number of photos required') to an implementation brief: extracted domain concepts with their full touchpoint footprints, exemplars of the house pattern to imitate, the project's auth/settings conventions, and a completion checklist wired to find_similar_changes, check_edit_safety, and pre_commit_review. START HERE for any feature request."
    )]
    pub async fn plan_user_story(
        &self,
        params: Parameters<PlanUserStoryRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_plan_user_story(params.0).await
    }

    #[tool(
        description = "Generate the Claude Code integration pack for a project: .claude/rules/engram-workflow.md (the mandated planning/safety loop) and .claude/settings.json reminder hooks that re-inject the workflow at edit/stop time. write_files=true installs them into the project (never overwrites an existing settings.json). Run once per project after indexing."
    )]
    pub async fn generate_agent_integration(
        &self,
        params: Parameters<GenerateAgentIntegrationRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_generate_agent_integration(params.0).await
    }

    #[tool(
        description = "Feed external review findings into Engram's anti-pattern memory: pass CTO/manual findings as a list and/or a SonarQube issues export JSON. What a reviewer caught once is then caught automatically by immune_check, pre_commit_review's gates, and get_chunk rule injection. Blocker/critical file-scoped findings auto-promote to repo rules. Run after every review cycle."
    )]
    pub async fn ingest_review_findings(
        &self,
        params: Parameters<IngestReviewFindingsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_ingest_review_findings(params.0).await
    }

    // ---- Search + chunks ----

    #[tool(
        description = "Hybrid lexical+vector search over indexed code/docs. Hits carry path, line range, doc_id, covering symbols (node_ids), and content/snippet. Set semantic=false for exact-identifier lookups (faster, BM25-only). For literal/regex matching prefer grep_project; for full chunk text use get_chunk."
    )]
    pub async fn search_memory(
        &self,
        params: Parameters<SearchMemoryRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_search_memory(params.0).await
    }

    #[tool(
        description = "Vector-only similarity search (no lexical fusion). Quality depends on the configured embedding backend — the response states which tier is active. Prefer search_memory for most queries."
    )]
    pub async fn vector_search(
        &self,
        params: Parameters<VectorSearchRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_vector_search(params.0).await
    }

    #[tool(
        description = "Fast literal/regex grep over the indexed file set. Uses the Tantivy trigram index as a prefilter — typically beats ripgrep on warm queries. Returns file:line:col matches with optional context lines."
    )]
    pub async fn grep_project(
        &self,
        params: Parameters<GrepProjectRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_grep_project(params.0).await
    }

    #[tool(
        description = "Fetch a chunk's full untruncated content by doc_id (from search_memory hits). Supports logical_slice (signatures/sql/state) to trim output and optional repo-rule injection. doc_ids are generation-scoped — re-search after reindexing."
    )]
    pub async fn get_chunk(
        &self,
        params: Parameters<GetChunkRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_chunk(params.0).await
    }

    // ---- Memory bank + repo rules ----

    #[tool(
        description = "Write a persistent agent note (named markdown section) scoped to the project. Survives reindexing and is searchable — use for decisions, gotchas, and session handoffs."
    )]
    pub async fn update_memory_bank(
        &self,
        params: Parameters<UpdateMemoryBankRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_update_memory_bank(params.0).await
    }

    #[tool(description = "List the project's memory bank section names.")]
    pub async fn list_memory_bank(
        &self,
        params: Parameters<ProjectIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_list_memory_bank(params.0).await
    }

    #[tool(description = "Read one memory bank section by name (see list_memory_bank).")]
    pub async fn read_memory_bank(
        &self,
        params: Parameters<MemorySectionRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_read_memory_bank(params.0).await
    }

    #[tool(description = "Delete one memory bank section by name.")]
    pub async fn delete_memory_bank(
        &self,
        params: Parameters<MemorySectionRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_delete_memory_bank(params.0).await
    }

    #[tool(
        description = "Add a persistent repo constraint (e.g. 'always use SafeRedirect'). Rules are injected into get_chunk output and enforced by review tools."
    )]
    pub async fn add_repo_rule(
        &self,
        params: Parameters<AddRepoRuleRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_add_repo_rule(params.0).await
    }

    #[tool(description = "List the project's repo rules with their rule_ids.")]
    pub async fn list_repo_rules(
        &self,
        params: Parameters<ProjectIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_list_repo_rules(params.0).await
    }

    #[tool(description = "Delete a repo rule by rule_id (see list_repo_rules).")]
    pub async fn delete_repo_rule(
        &self,
        params: Parameters<DeleteRepoRuleRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_delete_repo_rule(params.0).await
    }

    // ---- Graph tools ----

    #[tool(
        description = "Find graph nodes by type, name pattern, and/or file path. Returns node_ids with line ranges for use in traverse_graph / find_references / compute_blast_radius."
    )]
    pub async fn query_graph_nodes(
        &self,
        params: Parameters<QueryGraphNodesRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_query_graph_nodes(params.0).await
    }

    #[tool(
        description = "Edges in/out of a node_id grouped by kind (calls, SQL, state, UI, imports...). Use resolve_id first if you only have a name. For name-based lookup use find_symbol_references."
    )]
    pub async fn find_references(
        &self,
        params: Parameters<FindReferencesRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_find_references(params.0).await
    }

    #[tool(
        description = "Search ranked by text relevance blended with graph centrality — surfaces the load-bearing code for a topic, not just the best textual match. Use when you want 'the important file about X'."
    )]
    pub async fn graph_search(
        &self,
        params: Parameters<GraphSearchRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_graph_search(params.0).await
    }

    #[tool(
        description = "Multi-hop BFS from a start node_id along chosen edge kinds. Answers 'what is reachable from X within N hops'. Get node_ids from query_graph_nodes, resolve_id, or search_memory symbols."
    )]
    pub async fn traverse_graph(
        &self,
        params: Parameters<TraverseGraphRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_traverse_graph(params.0).await
    }

    // ---- Git/history tools ----

    #[tool(
        description = "Index commit history into temporal intelligence: co-change couplings, revert detection, author data. Run once after index_project; long-running on big repos (job — poll get_job_status). Unlocks analyze_temporal_couplings and analyze_reverts."
    )]
    pub async fn index_git_history(
        &self,
        params: Parameters<IndexGitHistoryRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_index_git_history(params.0).await
    }

    #[tool(
        description = "Reconstruct change history from dated zip snapshots when no git repo exists (common for legacy codebases). Feeds the same temporal intelligence as index_git_history."
    )]
    pub async fn ingest_zip_history(
        &self,
        params: Parameters<IngestZipHistoryRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_ingest_zip_history(params.0).await
    }

    #[tool(
        description = "Search indexed commit messages and changes. Use for 'when/why did X change' questions after index_git_history."
    )]
    pub async fn search_history(
        &self,
        params: Parameters<SearchHistoryRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_search_history(params.0).await
    }

    #[tool(
        description = "Files that historically change together with a target file (co-change strength from git history). Surfaces the coupled partner file you'd otherwise forget to edit."
    )]
    pub async fn analyze_temporal_couplings(
        &self,
        params: Parameters<AnalyzeTemporalCouplingsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_analyze_temporal_couplings(params.0).await
    }

    #[tool(
        description = "Harvest reverted commits from git history into the immune system as anti-patterns (generates immune_* repo rules). Run after index_git_history; then immune_check scores new code against them."
    )]
    pub async fn analyze_reverts(
        &self,
        params: Parameters<AnalyzeRevertsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_analyze_reverts(params.0).await
    }

    // ---- Agent / cognitive tools ----

    #[tool(
        description = "Graph impact of changing a file or symbol: dependents and depth-limited ripple. Lighter than compute_blast_radius (which adds a 1-10 score and downstream counts)."
    )]
    pub async fn impact_analysis(
        &self,
        params: Parameters<ImpactAnalysisRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_impact_analysis(params.0).await
    }

    #[tool(
        description = "Columns, types, keys, and code references for a database table, extracted from DDL/SQL indexed in the codebase. Use before writing queries against a table."
    )]
    pub async fn get_table_schema(
        &self,
        params: Parameters<GetTableSchemaRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_table_schema(params.0).await
    }

    #[tool(
        description = "[.NET legacy] All readers and writers of Session/ViewState/Application/Cache keys. Use before touching shared state — hidden readers are the classic WebForms regression."
    )]
    pub async fn trace_state_usage(
        &self,
        params: Parameters<TraceStateUsageRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_trace_state_usage(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Trace a UI control event down through code-behind to the SQL it ultimately executes."
    )]
    pub async fn trace_ui_event(
        &self,
        params: Parameters<TraceUiEventRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_trace_ui_event(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Map a user-visible action (button text, link caption) to its handler in code-behind."
    )]
    pub async fn trace_ui_action(
        &self,
        params: Parameters<TraceUiActionRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_trace_ui_action(params.0).await
    }

    #[tool(
        description = "Export a portable evidence bundle (overview, hotspots, rules) for use outside this MCP session."
    )]
    pub async fn export_capture_pack(
        &self,
        params: Parameters<ExportCapturePackRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_export_capture_pack(params.0).await
    }

    #[tool(
        description = "[.NET legacy] UI layout tree for a page/form: containers, controls, nesting (WebForms markup + WinForms designer)."
    )]
    pub async fn get_ui_blueprint(
        &self,
        params: Parameters<GetUiBlueprintRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_ui_blueprint(params.0).await
    }

    #[tool(
        description = "Orientation report: languages, file counts, key directories, hub files, graph summary. THE entry point for a first look at an indexed project — call before drilling into search or graph tools."
    )]
    pub async fn get_codebase_overview(
        &self,
        params: Parameters<ProjectIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_codebase_overview(params.0).await
    }

    #[tool(
        description = "Generate CLAUDE.md + .claude/rules/*.md (and optional AGENTS.md) from \
                       the project's indexed graph. Language-agnostic: sections are driven by \
                       what the graph actually contains. Fully deterministic, no LLM calls."
    )]
    pub async fn produce_claude_md(
        &self,
        params: Parameters<crate::models::ProduceClaudeMdRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_produce_claude_md(params.0).await
    }

    #[tool(
        description = "All references to a symbol name: graph edges in/out grouped by kind, with lexical fallback when the graph has no match. Accepts plain names; use resolve_id when the name is ambiguous."
    )]
    pub async fn find_symbol_references(
        &self,
        params: Parameters<FindSymbolReferencesRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_find_symbol_references(params.0).await
    }

    #[tool(
        description = "Heuristic stacktrace triage: parses frames, searches the index for matching files/functions, ranks by frame match + centrality. Good first pass at 'where is this crash' — not a debugger."
    )]
    pub async fn analyze_error_stack(
        &self,
        params: Parameters<AnalyzeErrorStackRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_analyze_error_stack(params.0).await
    }

    #[tool(
        description = "Run a consolidation ('dream') cycle: clusters co-retrieved chunks into insight nodes. Background maintenance that improves future retrieval — not a query tool."
    )]
    pub async fn dream_project(
        &self,
        params: Parameters<DreamProjectRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_dream_project(params.0).await
    }

    #[tool(description = "Deprecated alias of dream_project — call dream_project instead.")]
    pub async fn trigger_rem_cycle(
        &self,
        params: Parameters<DreamProjectRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_dream_project(params.0).await
    }

    #[tool(
        description = "Per-file convention profile from AST: naming, error handling, idioms, with violation examples. Feed into code generation so new code matches the file it lands in."
    )]
    pub async fn analyze_file_coding_style(
        &self,
        params: Parameters<AnalyzeFileCodingStyleRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_analyze_file_coding_style(params.0).await
    }

    #[tool(description = "List background jobs (indexing, git history) with status and progress.")]
    pub async fn list_jobs(
        &self,
        params: Parameters<ListJobsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_list_jobs(params.0).await
    }

    #[tool(description = "Cancel a running background job by job_id (see list_jobs).")]
    pub async fn cancel_job(
        &self,
        params: Parameters<CancelJobRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_cancel_job(params.0).await
    }

    #[tool(
        description = "Status and progress of one background job by job_id. Poll this after index_project / index_git_history return a job."
    )]
    pub async fn get_job_status(
        &self,
        params: Parameters<CancelJobRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_job_status(params.0).await
    }

    #[tool(
        description = "Score proposed code against revert-derived anti-patterns (hybrid FTS+vector); returns matched rules with the reverting commit hash as evidence. Run before committing risky changes — this is how you avoid re-introducing what was already reverted once."
    )]
    pub async fn immune_check(
        &self,
        params: Parameters<ImmuneCheckRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_immune_check(params.0).await
    }

    #[tool(
        description = "Check code against indexed anti-patterns and get remediation guidance with originating commits. Prefer immune_check for a numeric score; this tool for the 'what to do instead'."
    )]
    pub async fn anti_pattern_guard(
        &self,
        params: Parameters<AntiPatternGuardRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_anti_pattern_guard(params.0).await
    }

    #[tool(
        description = "Pre-commit review — runs eleven graph-backed gates (immune, blast-radius, \
                       style, temporal, state, audit, anti-pattern, new-file, test-coverage, \
                       secret-leakage, guard-parity) over a unified diff and returns \
                       severity-ranked, evidence-backed findings. Accepts a raw diff, `staged`, \
                       `unstaged`, `head`, or a `.patch` path. No LLM calls."
    )]
    pub async fn pre_commit_review(
        &self,
        params: Parameters<PreCommitReviewRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_pre_commit_review(params.0).await
    }

    #[tool(
        description = "Ingest code-review history (CodeRabbit via Azure DevOps live fetch or \
                       pre-scraped JSONL) into Engram's anti-pattern index. Parses each review \
                       comment, clusters duplicates by token-overlap Jaccard similarity, and \
                       writes three sinks: (1) positive rules to the antipattern namespace, \
                       (2) wontFix rules to a file-scoped suppression namespace, (3) graph \
                       review_pattern nodes with AntiPattern edges to every flagged file. \
                       High-confidence rules auto-promote to repo rules. Incremental across \
                       runs via a per-source last_pr_id marker."
    )]
    pub async fn ingest_code_review_history(
        &self,
        params: Parameters<IngestCodeReviewHistoryRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_ingest_code_review_history(params.0).await
    }

    #[tool(
        description = "Explain a diff — natural dual of pre_commit_review. Takes the same \
                       inputs (staged / unstaged / head / raw / .patch) and produces a \
                       Conventional-Commits commit message, a structured PR description, and \
                       a Keep-a-Changelog entry, all derived deterministically from the \
                       graph, CodeRabbit rules, blast-radius data, and temporal couplings. \
                       Output is markdown (default) or JSON for CI. No LLM calls."
    )]
    pub async fn explain_change(
        &self,
        params: Parameters<ExplainChangeRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_explain_change(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Instrumentation guidance pack for capturing runtime evidence before migration (pairs with ingest_runtime_artifacts)."
    )]
    pub async fn get_instrumentation_pack(
        &self,
        params: Parameters<GetInstrumentationPackRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_instrumentation_pack(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Propose strangler-fig module boundaries from graph cohesion (call, SQL, and state edges)."
    )]
    pub async fn suggest_migration_boundaries(
        &self,
        params: Parameters<SuggestMigrationBoundariesRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_suggest_migration_boundaries(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Ingest instrumentation logs to add observed-runtime edges to the graph (confirms or contradicts static analysis)."
    )]
    pub async fn ingest_instrumentation_logs(
        &self,
        params: Parameters<IngestInstrumentationLogsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_ingest_instrumentation_logs(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Ingest runtime artifacts (IIS logs, traces, lifecycle snapshots, SQL profiler exports) and merge runtime evidence into the static graph."
    )]
    pub async fn ingest_runtime_artifacts(
        &self,
        params: Parameters<IngestRuntimeArtifactsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_ingest_runtime_artifacts(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Compile a migration blueprint: inventory, risk hotspots, suggested boundaries and order. For the exhaustive one-call report use analyze_full_project_migration."
    )]
    pub async fn generate_migration_blueprint(
        &self,
        params: Parameters<GenerateMigrationBlueprintRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_generate_migration_blueprint(params.0).await
    }

    #[tool(
        description = "Function-level dependency graph for a file/module straight from AST (imports + calls). Quick local structure; for project-wide questions use the graph tools."
    )]
    pub async fn ast_dependency_graph(
        &self,
        params: Parameters<AstDependencyGraphRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_ast_dependency_graph(params.0).await
    }

    #[tool(
        description = "Garbage-collect stale index generations (old snapshots). Maintenance; safe to run anytime, frees disk."
    )]
    pub async fn incremental_indexing_gc(
        &self,
        params: Parameters<IncrementalIndexingGcRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_incremental_indexing_gc(params.0).await
    }

    #[tool(
        description = "Maintain (rebuild/refresh) the dedicated anti-pattern index. Maintenance only — to query anti-patterns use immune_check or anti_pattern_guard."
    )]
    pub async fn dedicated_antipattern_index(
        &self,
        params: Parameters<AntipatternIndexRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_dedicated_antipattern_index(params.0).await
    }

    #[tool(description = "Server metrics: counters, latencies, memory budget, subsystem stats.")]
    pub async fn get_metrics(
        &self,
        params: Parameters<GetMetricsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_metrics(params.0).await
    }

    #[tool(
        description = "Verify index integrity (Tantivy/Redb sentinels), optionally auto-repair. Use when results look wrong or after a crash; repair_project for targeted rebuilds."
    )]
    pub async fn check_integrity(
        &self,
        params: Parameters<CheckIntegrityRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_check_integrity(params.0).await
    }

    #[tool(
        description = "Policy-based safety evaluation of a proposed change (confidence + coverage thresholds). Part of the autonomous-edit pipeline; for a quick per-method verdict use check_edit_safety."
    )]
    pub async fn evaluate_safety(
        &self,
        params: Parameters<EvaluateSafetyRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_evaluate_safety(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Step-ordered migration plan for selected pages/modules with risks and prerequisites."
    )]
    pub async fn generate_migration_plan(
        &self,
        params: Parameters<GenerateMigrationPlanRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_generate_migration_plan(params.0).await
    }

    #[tool(
        description = "Measure retrieval quality (NDCG/recall) against a golden query set. Maintenance/CI tool, not for everyday queries."
    )]
    pub async fn benchmark_retrieval(
        &self,
        params: Parameters<BenchmarkRetrievalRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_benchmark_retrieval(params.0).await
    }

    #[tool(
        description = "How well Engram understood a file/page: per-signal extraction confidence (bindings, event wiring, SQL traces). Low confidence means verify against source before acting on graph answers."
    )]
    pub async fn get_extraction_confidence(
        &self,
        params: Parameters<GetExtractionConfidenceRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_extraction_confidence(params.0).await
    }

    #[tool(description = "Resumable-job checkpoint status (crash recovery bookkeeping).")]
    pub async fn get_checkpoint_status(
        &self,
        params: Parameters<GetCheckpointStatusRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_checkpoint_status(params.0).await
    }

    #[tool(description = "Memory budget usage per subsystem (OOM-prevention accounting).")]
    pub async fn get_memory_budget(
        &self,
        params: Parameters<GetMemoryBudgetRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_memory_budget(params.0).await
    }

    #[tool(
        description = "1-10 impact score for changing a file/symbol, with incoming/outgoing/downstream counts and the contributing edges. Call before risky edits; treat 7+ as 'plan carefully'. Quick verdict variant: check_edit_safety."
    )]
    pub async fn compute_blast_radius(
        &self,
        params: Parameters<ComputeBlastRadiusRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_compute_blast_radius(params.0).await
    }

    #[tool(
        description = "Detect structural patterns/anti-patterns in the graph (god classes, circular dependencies, hub overload)."
    )]
    pub async fn detect_design_patterns(
        &self,
        params: Parameters<DetectDesignPatternsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_detect_design_patterns(params.0).await
    }

    #[tool(
        description = "8-gate Autonomous Decision Protocol verdict (allow/deny/abstain) for an autonomous edit, with an immutable audit trail. For agent frameworks editing without a human in the loop."
    )]
    pub async fn autonomous_decision_gate(
        &self,
        params: Parameters<AutonomousDecisionGateRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_autonomous_decision_gate(params.0).await
    }

    #[tool(
        description = "Rerank a candidate list of files/nodes by graph centrality (PageRank/betweenness/degree). Use to prioritize among search results or migration candidates."
    )]
    pub async fn graph_centrality_rerank(
        &self,
        params: Parameters<GraphCentralityRerankRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_graph_centrality_rerank(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Generate target-stack scaffold code (controllers/views/models) for a page or module migration."
    )]
    pub async fn generate_migration_scaffold(
        &self,
        params: Parameters<GenerateMigrationScaffoldRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_generate_migration_scaffold(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Generate instrumentation snippets to capture runtime evidence before migrating a page/method."
    )]
    pub async fn generate_instrumentation_code(
        &self,
        params: Parameters<GenerateInstrumentationCodeRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_generate_instrumentation_code(params.0).await
    }

    #[tool(
        description = "Compare observed-runtime edges against static graph edges; reports confirmed/contradicted assumptions (feeds the ADP reconciliation gate)."
    )]
    pub async fn reconcile_runtime_evidence(
        &self,
        params: Parameters<ReconcileRuntimeEvidenceRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_reconcile_runtime_evidence(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Per-key strategy for migrating Session/ViewState/Application/Cache state to modern equivalents (claims, cache, route/query, DB)."
    )]
    pub async fn suggest_state_migration(
        &self,
        params: Parameters<SuggestStateMigrationRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_suggest_state_migration(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Generate characterization (golden-master) tests for methods so behaviour is pinned before refactoring/migration."
    )]
    pub async fn generate_characterization_tests(
        &self,
        params: Parameters<GenerateCharacterizationTestsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_generate_characterization_tests(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Generate routing/proxy config (YARP-style) for strangler-fig incremental migration."
    )]
    pub async fn generate_strangler_fig_config(
        &self,
        params: Parameters<GenerateStranglerFigRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_generate_strangler_fig_config(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Inventory WebForms validators per page: validator type, target control, rules, client/server enforcement."
    )]
    pub async fn map_validation_controls(
        &self,
        params: Parameters<MapValidationControlsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_map_validation_controls(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Authentication/authorization map from web.config and code: forms auth, roles, per-location rules, protected pages."
    )]
    pub async fn map_auth_config(
        &self,
        params: Parameters<MapAuthConfigRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_map_auth_config(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Page lifecycle hooks per page (Init, Page_Load, PreRender...) and what each one does — essential before reordering page logic."
    )]
    pub async fn map_page_lifecycle(
        &self,
        params: Parameters<MapPageLifecycleRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_map_page_lifecycle(params.0).await
    }

    #[tool(
        description = "[.NET legacy] ViewState keys per page: writers, readers, postback dependencies, bloat candidates."
    )]
    pub async fn analyze_viewstate_deps(
        &self,
        params: Parameters<AnalyzeViewStateDepsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_analyze_viewstate_deps(params.0).await
    }

    #[tool(
        description = "[.NET legacy] UpdatePanels/AJAX regions per page with triggers and partial-postback scope."
    )]
    pub async fn map_ajax_regions(
        &self,
        params: Parameters<MapAjaxRegionsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_map_ajax_regions(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Trace a data item from UI control through code-behind to database column (and back through bindings)."
    )]
    pub async fn trace_data_flow(
        &self,
        params: Parameters<TraceDataFlowRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_trace_data_flow(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Full per-page migration dossier: everything needed to rewrite one page (controls, handlers, data, state, AJAX, risks). The per-page workhorse — prefer this over the full-project report inside agent loops."
    )]
    pub async fn get_migration_dossier(
        &self,
        params: Parameters<GetMigrationDossierRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_migration_dossier(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Which extraction dimensions are covered for a page — uncovered dimensions mean 'verify manually before migrating'."
    )]
    pub async fn check_migration_coverage(
        &self,
        params: Parameters<CheckMigrationCoverageRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_check_migration_coverage(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Record per-page migration status (not_started/in_progress/migrated/verified) in the durable progress tracker."
    )]
    pub async fn update_migration_status(
        &self,
        params: Parameters<UpdateMigrationStatusRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_update_migration_status(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Project-wide migration progress dashboard from the durable tracker."
    )]
    pub async fn get_migration_progress(
        &self,
        params: Parameters<GetMigrationProgressRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_migration_progress(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Dependency-aware page/module migration order (graph topology + risk + shared-state coupling)."
    )]
    pub async fn suggest_migration_order(
        &self,
        params: Parameters<SuggestMigrationOrderRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_suggest_migration_order(params.0).await
    }

    #[tool(
        description = "[.NET legacy] One-call full migration report: inventory, DB, state, AJAX, JS, risks, order. LARGE output intended for humans/docs — inside agent loops prefer get_migration_dossier per page."
    )]
    pub async fn analyze_full_project_migration(
        &self,
        params: Parameters<AnalyzeFullProjectMigrationRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_analyze_full_project_migration(params.0).await
    }

    #[tool(
        description = "LLM-powered business-rule extraction for files/methods into queryable summaries. Requires llm_backend configured (Ollama/OpenAI); degrades to deterministic summaries offline. Query results later with query_business_logic."
    )]
    pub async fn analyze_business_logic(
        &self,
        params: Parameters<crate::models::requests::AnalyzeBusinessLogicRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_analyze_business_logic(params.0).await
    }

    #[tool(
        description = "Query stored business-logic summaries previously produced by analyze_business_logic."
    )]
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
        description = "[.NET legacy] Detect sync-over-async hazards (.Result, .Wait(), Thread.Sleep, HttpContext.Current) that cause deadlocks during async migration."
    )]
    pub async fn analyze_sync_hazards(
        &self,
        params: Parameters<AnalyzeSyncHazardsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_analyze_sync_hazards(params.0).await
    }

    #[tool(
        description = "[.NET legacy] jQuery usage inventory: core version, vulnerabilities, UI widgets, third-party plugins, deprecated patterns with migration guidance."
    )]
    pub async fn get_jquery_inventory(
        &self,
        params: Parameters<GetJQueryInventoryRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_jquery_inventory(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Reconstruct session/state workflows: trace how Session, Application, ViewState, Cache, and Cookie keys flow across pages. Detects MissingWriter, MissingReader, and complex cross-page chains."
    )]
    pub async fn get_session_workflows(
        &self,
        params: Parameters<GetSessionWorkflowsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_session_workflows(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Detect VB.NET → C# translation traps: 14 categories of semantic differences (silent bugs like Nothing/ValueType, Is vs =; compile errors like On Error GoTo, ReDim Preserve, My.* namespace)."
    )]
    pub async fn get_vb_translation_traps(
        &self,
        params: Parameters<GetVbTranslationTrapsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_vb_translation_traps(params.0).await
    }

    #[tool(
        description = "Detect C# diagnostics: async/ConfigureAwait pitfalls, event-leak patterns, and IDisposable misuse hotspots."
    )]
    pub async fn get_csharp_diagnostics(
        &self,
        params: Parameters<GetCsharpDiagnosticsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_csharp_diagnostics(params.0).await
    }

    #[tool(
        description = "Detect C diagnostics: buffer safety and ownership heuristics plus unsafe API hotspots."
    )]
    pub async fn get_c_diagnostics(
        &self,
        params: Parameters<GetCDiagnosticsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_c_diagnostics(params.0).await
    }

    #[tool(
        description = "Detect C++ diagnostics: RAII violations, raw new/delete hotspots, and exception-safety flags."
    )]
    pub async fn get_cpp_diagnostics(
        &self,
        params: Parameters<GetCppDiagnosticsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_cpp_diagnostics(params.0).await
    }

    #[tool(
        description = "Detect Rust diagnostics: unwrap/panic hotspots, blocking-in-async, and unsafe boundary checks."
    )]
    pub async fn get_rust_diagnostics(
        &self,
        params: Parameters<GetRustDiagnosticsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_rust_diagnostics(params.0).await
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
        description = "[.NET legacy] Full page context for a WebForms page: control tree, all event handlers with complete bodies, data layer, session state, AJAX regions, validation, auth requirements. The starting point for all WebForms work."
    )]
    pub async fn get_page_context(
        &self,
        params: Parameters<GetPageContextRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_page_context(params.0).await
    }

    #[tool(
        description = "LLM context packer: assembles coding style profile, pattern examples from callers, database schema for referenced tables, SP signatures, session state context, control mappings, VB translation traps, language-family diagnostics (C#/C/C++/Rust), and sync hazards — everything an LLM needs to generate correct code in one call."
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
            instructions: Some(
                "Engram gives agents a persistent, indexed memory of a codebase: hybrid \
                 search, a typed code graph, git temporal intelligence, and edit-safety \
                 checks. Typical flow: list_projects (get the project_id) → \
                 get_codebase_overview (orient) → search_memory or grep_project (find \
                 code; hits carry doc_ids, line ranges, and symbol node_ids) → get_chunk \
                 (full text) → resolve_id / find_symbol_references / traverse_graph \
                 (structure) → check_edit_safety or get_method_edit_context (before \
                 editing) → pre_commit_review (on your diff). If results look stale, \
                 call get_index_freshness; update_project refreshes incrementally. \
                 Tools prefixed [.NET legacy] target ASP.NET WebForms / VB.NET / Classic \
                 ASP migration work and return little on other stacks."
                    .to_string(),
            ),
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
