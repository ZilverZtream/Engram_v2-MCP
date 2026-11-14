use crate::models::*;
use crate::state::AppState;
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::{tool::Parameters, tool::ToolRouter},
    model::*,
    tool, tool_router,
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

    /// The full tool registry (names + input schemas) without an `AppState`.
    /// Lets generated agent-facing text be checked against the schemas the
    /// server actually advertises.
    pub fn tool_registry() -> ToolRouter<Engram> {
        Self::tool_router()
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
        description = "One-call corpus freshness: incrementally refreshes the file index, git history (temporal coupling), and the merged-PR exemplar corpus in sequence. Watermark-incremental - a no-change repo returns in seconds. Run at session start or after pulling to keep Engram's knowledge current."
    )]
    pub async fn refresh_corpora(
        &self,
        params: Parameters<crate::models::RefreshCorporaRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_refresh_corpora(params.0).await
    }

    #[tool(
        description = "Incrementally re-index changed files and refresh git intelligence. Optional only_paths (1..4096 exact eligible paths, wait=true) exclusively re-extracts those files and inherits outside revisions without checking current outside bytes; it skips Git/global enrichment and requires an already verified source identity with no foreign unpublished attempt. Extractors may read referenced companion files. only_paths cannot combine with reindex_paths, anti-pattern refresh or instruction maintenance. Otherwise reindex_paths adds forced paths to the normal whole-project change scan. Optional instruction_maintenance with wait=true observes guidance after the normal update; it does not write policy."
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
        description = "Browse tools outside the core tier, with an optional substring filter on name/description. All tools are advertised by default. If configured with advertise_all_tools=false, enable it and reconnect to make advanced tools available to clients."
    )]
    pub async fn list_advanced_tools(
        &self,
        params: Parameters<ListAdvancedToolsRequest>,
    ) -> Result<CallToolResult, McpError> {
        let filter = params.0.filter.unwrap_or_default().to_ascii_lowercase();
        let mut out = String::new();
        let mut n = 0usize;
        for t in crate::tool_surface::advanced(self.tool_router.list_all()) {
            let desc = t.description.as_deref().unwrap_or("");
            if !filter.is_empty()
                && !t.name.to_ascii_lowercase().contains(&filter)
                && !desc.to_ascii_lowercase().contains(&filter)
            {
                continue;
            }
            n += 1;
            let first = desc.split(". ").next().unwrap_or(desc);
            out.push_str(&format!("- `{}` — {}\n", t.name, first));
        }
        let header = format!(
            "# Advanced tools — {n} tool(s) outside the core tier ({} core tools){}\n\n",
            crate::tool_surface::CORE_TOOLS.len(),
            if filter.is_empty() {
                String::new()
            } else {
                format!(", filter `{filter}`")
            }
        );
        let availability = if self.state.cfg.advertise_all_tools {
            "active_profile: full\nadvertised_in_active_profile: true\nUse the schemas returned by tools/list. Backend and project-data requirements still apply.\n\n"
        } else {
            "active_profile: core\nadvertised_in_active_profile: false\nThis catalog describes registered tools; it does not add callable tools to this client. To use these tools, set advertise_all_tools=true in the server configuration, restart Engram, then reconnect the client to refresh tools/list.\n\n"
        };
        Ok(CallToolResult::success(vec![Content::text(format!(
            "{header}{availability}{out}"
        ))]))
    }

    #[tool(
        description = "UI conformance (spec Layer 2, external audit row 5): PULL the house family contract for a region (a file, a directory prefix, or a glob) - exemplar + typed axes with evidence counts - BEFORE writing UI there; with candidate_classes, CHECK that class list against the same contract and get every axis at once (ok / deviation with the expected value). Derived from the indexed ui_container/control_layout nodes; nothing is a default."
    )]
    pub async fn get_ui_conformance(
        &self,
        params: Parameters<GetUiConformanceRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let region = req.region.clone();
        let min = req.min_instances.unwrap_or(2).max(2);
        let families = tokio::task::spawn_blocking(move || {
            crate::services::ui_catalog::families_for_region(&graph, &pid, &region, min)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        if families.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "result: no_families\nregion: {}\nNo UI family with >= {min} instances has an instance in this region - there is no house contract to conform to here. Try the nearest sibling directory, a wider glob, or index the project first.",
                req.region
            ))]));
        }
        let verdicts = req
            .candidate_classes
            .as_deref()
            .map(|c| crate::services::ui_catalog::check_classes(&families[0], c));
        Ok(CallToolResult::success(vec![Content::text(
            crate::services::ui_catalog::render_conformance(&families, verdicts.as_deref()),
        )]))
    }

    #[tool(
        description = "Index health snapshot: generation, graph node/edge counts, doc/vector counts, and semantic-search tier. Optional instruction_maintenance observes bounded selected guidance and linked references against an exact base commit, reporting dependent changes and incomplete coverage separately from index health. No authored guidance or ownership is changed."
    )]
    pub async fn project_health(
        &self,
        params: Parameters<ProjectHealthRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_project_health_with_instructions(params.0).await
    }

    #[tool(
        description = "Repair a project index (scopes: tantivy_only, vector_only, graph_only, full). graph_only re-extracts unchanged source with current parsers while preserving search/vector stores and source revisions; it never embeds or replays Git. Update changed source first. Use other scopes when check_integrity or project_health reports storage inconsistencies."
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
        description = "Check whether a project's index is current: active generation, time since last index, watcher status, and (by default) a count of files modified on disk since the last index. For historical replays or pinned checkouts, pass expected_git_commit to verify both HEAD and tracked-worktree cleanliness even when check_disk is false. Use before trusting search/graph results, or when results look stale. Related: update_project to refresh, watch_project for auto-refresh."
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
        description = "Discover indexed touchpoints of a domain concept (e.g. 'photo', 'code category'): data, SQL, state, UI, logic, endpoints, anchor consumers and textual mentions. Use when scoping a concept across the codebase. Results are bounded by index coverage, concept matching and reported caps; inspect coverage/failures and verify cited consumers before treating the scope as complete. Applicable authored guidance follows returned paths. max_tokens budgets the complete reply (default40KiB); recover an oversized original through response_delivery.next_call without repeating retrieval. output_json names footprint_markdown, source_paths, completeness and policy; it does not imply a complete typed graph. Related: find_implementation_pattern, detect_incomplete_changes."
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
        description = "Find concrete exemplars of how THIS codebase implements a pattern (e.g. 'admin settings page save', 'dropdown bound to lookup table'): top matching files with their symbols, SQL/table/state edges, co-changed partners, and a snippet — plus the ingredients common across exemplars. Applicable authored guidance accompanies returned exemplar files in JSON and Markdown; preserve its conditions/exceptions and recheck guidance for the final edit scope. max_tokens budgets the complete reply (default40KiB); recover an oversized original through response_delivery.next_call without repeating retrieval. Imitate the best exemplar instead of inventing a new approach. Related: get_chunk for full source."
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
        description = "Inventory of dependency cycles (strongly-connected components over Calls/Dependency/Imports): the code that CANNOT be migrated one piece at a time. Run before planning migration waves or strangler-fig extraction; each cycle must move as a unit or have an edge broken first. Related: suggest_migration_boundaries, get_migration_order."
    )]
    pub async fn find_dependency_cycles(
        &self,
        params: Parameters<FindDependencyCyclesRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_find_dependency_cycles(params.0).await
    }

    #[tool(
        description = "Shortest connection path between two code entities ('how does this button reach that table?'): BFS over the full edge graph, directed first then undirected fallback, every hop labeled with its edge kind. Accepts node_ids, names, or FQNs; ambiguous names return a candidate list. Related: blast_radius for impact, trace_data_flow for value-level detail."
    )]
    pub async fn find_connection_path(
        &self,
        params: Parameters<FindConnectionPathRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_find_connection_path(params.0).await
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
        description = "ONE natural-language front door. Ask anything ('what breaks if I change X?', 'why is Y enforced?', 'where is Z used?', 'how does W work?') and get a deterministic, typed, authority-ranked EVIDENCE REPORT (retrieval_only): multi-intent query understanding, entity resolution, parallel retrieval across code/docs/memory/insights/business-rules/history/graph, conflict + freshness aware, with an HONEST status (answered/partial/ambiguous/stale/unsupported/failed) so it abstains rather than guessing. Returns cited evidence for the calling agent to interpret. Set route_with_model=true for one optional configured model request that suggests additional retrieval intents and exact question mentions before ordinary resolution. Scope and answer constraints remain; failures retain deterministic routing with an explicit reason. routing_timeout_ms controls this request only (default5000). Set synthesize=true for an optional model-generated answer with checked evidence IDs and exact quotations; model interpretation is not semantically verified and does not upgrade retrieval status. Generation failure retains the evidence report. Optional envelope: depth (quick|standard|deep), as_of, output_format (markdown|json|both), deadline_ms, synthesis_timeout_ms (generation only, default30000). max_tokens bounds the complete reply (default40KiB); oversized reports retain status/coverage and an exact recovery next_call. Recovery uses response_id/response_offset without a question or new retrieval/model calls. START HERE when unsure which tool fits."
    )]
    pub async fn ask_codebase(
        &self,
        params: Parameters<AskCodebaseRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_ask_codebase(params.0).await
    }

    #[tool(description = "Read up to 16 method-information, graph/document identity or stored-chunk requests in one call. Each request has a unique request_id and a typed request with tool and its normal arguments. Returns ordered complete underlying results/errors under one shared max_tokens budget; whole results omitted for budget retain exact single-tool recovery. Existing source/citation validation applies. Reads run sequentially and are not an atomic snapshot. JSON response.")]
    pub async fn batch_read(
        &self,
        params: Parameters<BatchReadRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_batch_read(params.0).await
    }

    #[tool(description = "Trace a URL/page and optional action to source-cited WebForms or conventional VB Web API routes, authored handlers, indexed calls, SQL/procedures/tables, state and response source. Prefix API URLs with an HTTP verb, for example POST /webhook/vetro/42; api_traces retains controller and handler candidates. declared_dependency_registrations supplies literal VB RegisterType(Of Contract, Implementation) declarations from the selected route configuration file, with current source lines/hash and explicit coverage limits. Type spellings are not resolved or used to select/suppress interface candidates; active container/provider, conditions and runtime execution remain unproven. Oversized replies retain all selected paths in a compact node/edge view; follow response_delivery.next_call for exact full-evidence recovery. Static navigation, not proof of runtime execution. Ambiguous routing, stale graph/source and missing trigger bindings remain explicit gaps. Requires indexed project for a new trace; JSON response.")]
    pub async fn trace_user_request(
        &self,
        params: Parameters<TraceUserRequestRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_trace_user_request(params.0).await
    }

    #[tool(
        description = "Open an edit session BEFORE changing code: persists your planned file set and returns the expectation brief — co-change partners and shared state the plan must account for. Bookend with complete_edit_session. Related: plan_user_story for the planning stage."
    )]
    pub async fn begin_edit_session(
        &self,
        params: Parameters<BeginEditSessionRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_begin_edit_session(params.0).await
    }

    #[tool(
        description = "Close the edit session: reports scope drift (planned-but-unedited files) plus the full completeness check on what you actually edited. Run before pre_commit_review."
    )]
    pub async fn complete_edit_session(
        &self,
        params: Parameters<CompleteEditSessionRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_complete_edit_session(params.0).await
    }

    #[tool(
        description = "Edit-completeness check: given the files you edited, reports the strong co-change partners you did NOT touch (with historical evidence) and state keys shared with untouched files. Run before committing any multi-file change — it is the 'you forgot the other side' detector. Related: pre_commit_review."
    )]
    pub async fn detect_incomplete_changes(
        &self,
        params: Parameters<DetectIncompleteChangesRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_detect_incomplete_changes(params.0).await
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
        description = "ONE call from a weak user story (e.g. 'As an admin I would like to set minimum number of photos required') to an implementation brief: extracted domain concepts with their full touchpoint footprints, exemplars of the house pattern to imitate, the project's auth/settings conventions, and a completion checklist. Gives UNDERSTANDING; for the ranked FILE LIST to edit, call get_change_set with the same story."
    )]
    pub async fn plan_user_story(
        &self,
        params: Parameters<PlanUserStoryRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_plan_user_story(params.0).await
    }

    #[tool(
        description = "Assemble WebForms/VB.NET feature context and optional implementation proposals. Set dossier_only=true to retrieve bounded source excerpts, scoped change-set guidance and patterns for the calling AI, with zero server text-generation calls and no generated plan or edit-safety verdict. Otherwise uses the configured LLM for requirements, cited proposals, ordered steps, per-method safety and acceptance checks; ReadyToCode adds proposed templates. Read-only: never edits application files or executes SQL. Generated proposals require source and requirement review."
    )]
    pub async fn one_shot_feature_plan(
        &self,
        params: Parameters<OneShotFeaturePlanRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_one_shot_feature_plan(params.0).await
    }

    #[tool(
        description = "Settings catalog: EVERY setting the app has (web.config appSettings, connection strings, Session/Application/Cache keys, settings-store code accessors like ConfigSettings.*) with reader counts and top usage sites, most-read first. The 'only the PO knows the settings' problem, as a queryable database. Filter with scope=<path substring>. Drill in with get_setting."
    )]
    pub async fn list_settings(
        &self,
        params: Parameters<crate::models::ListSettingsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_list_settings(params.0).await
    }

    #[tool(
        description = "LLM wiki for one database table: the domain entity it represents, key columns, who reads/writes it, and test implications - built from the graph plus accessor-method excerpts. Persisted so query_business_logic finds it. Requires llm_backend."
    )]
    pub async fn describe_table(
        &self,
        params: Parameters<crate::models::DescribeTableRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_describe_table(params.0).await
    }

    #[tool(
        description = "LLM-inferred description of one setting from indexed reader-method bodies: behaviour, user-type interactions and test implications. Read-only preview by default. Set persist=true to publish the inferred description to business_logic for later query_business_logic retrieval. Requires llm_backend. Use get_setting for raw usage sites."
    )]
    pub async fn describe_setting(
        &self,
        params: Parameters<crate::models::DescribeSettingRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_describe_setting(params.0).await
    }

    #[tool(
        description = "QA/test-plan generator: given requested changed/planned files, derives WHAT TO TEST: settings that fork behaviour, roles/permissions, shared Session/Application keys, source-verified business-rule cases, and (for VB members named in change_intent or added by the diff) a residual string-literal call-site sweep. Supplies scoped repository guidance with hashes, complete exceptions and explicit omissions, separately from inferred cases. Hot-loads bounded organization rules from data_dir/rules/test-risk-rules.yaml and repository rules from .engram/test-risk-rules.yaml; repository rule IDs override organization IDs without a rebuild. For historical replay, knowledge_before withholds live repository guidance and excludes undated rules and rules introduced on/after the exclusive YYYY-MM-DD cutoff. Returns proposed cases with evidence limits; it does not execute tests."
    )]
    pub async fn derive_test_matrix(
        &self,
        params: Parameters<crate::models::DeriveTestMatrixRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_derive_test_matrix(params.0).await
    }

    #[tool(
        description = "One setting's complete story: where it's declared, EVERY indexed usage site (containing symbol + file:line, grouped by edge kind), and test guidance (toggle-and-exercise sites, role/permission axis, literal-sweep fallback). Use after list_settings, or whenever a change touches code gated by a setting."
    )]
    pub async fn get_setting(
        &self,
        params: Parameters<crate::models::GetSettingRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_setting(params.0).await
    }

    #[tool(
        description = "Build/refresh the merged-work corpus from first-parent git history. Azure DevOps and GitHub merge messages supply PR identities without a PAT. Indexes title, author, date, domains, change kinds and shipped file cohorts per PR/change unit. Direct commits may also appear. Git history does not verify review approvals or implementation correctness. Incremental via watermark. Query with find_merged_work."
    )]
    pub async fn ingest_merged_prs(
        &self,
        params: Parameters<crate::models::IngestMergedPrsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_ingest_merged_prs(params.0).await
    }

    #[tool(
        description = "Find similar indexed merged work by story/domain terms, optionally filtered by change kind (ui-markup|ui-code|js|database|settings|resources|api|backend). Returns ranked PR/change examples and shipped file cohorts from ingest_merged_prs. Direct commits may appear; Git-derived examples do not establish review approval or correctness. Inspect the cited evidence and verify applicability before reusing a pattern."
    )]
    pub async fn find_merged_work(
        &self,
        params: Parameters<crate::models::FindMergedWorkRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_find_merged_work(params.0).await
    }

    #[tool(
        description = "Suggest candidate files for a user story using exact code-shaped entities, concepts, historical co-change and structural graph evidence. Ranks candidates, expands deployable families, follows direct caller and asset-bundle links, supplies task-scoped hash-bound repository instructions with explicit omissions, applicable learned rules, and a contract checkpoint that turns release-critical configured planning rules into a hard_items packet plus a copy-ready workflow_scaffold whose MISSING fields must be resolved. Engram ships no built-in domain checklist. Hot-loads bounded organization rules from data_dir/rules/planning-contract-rules.yaml and repository rules from .engram/planning-contract-rules.yaml, with project overrides and historical-cutoff provenance. Each file row distinguishes behavioral, historical and structural evidence, preserves its causal anchor, asks a mechanism-specific impact question, and states the source evidence required to exclude it; repeated guidance in compact/reconciled responses is losslessly referenced through row_guidance. Primary and dependency rows carry stable P/A/C IDs plus a SHA-256 reconciliation receipt; copy every row and classify it before planning. Use detail=reconciled for the complete receipt-bound planning payload without the unbounded companion tail; use full only for forensic diagnostics. Oversized output supplies immutable response_id/response_offset recovery calls; recover all pages before completing intake. Results do not establish complete scope, required edits or reviewer approval."
    )]
    pub async fn get_change_set(
        &self,
        params: Parameters<GetChangeSetRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_change_set(params.0).await
    }

    #[tool(
        description = "Hard gate between get_change_set and planning/implementation. Validates the exact checkpoint receipt and row order; requires exactly one completed ledger row for every release-critical hard_items ID (an empty checkpoint passes with nothing to disposition); verifies allowed dispositions, concrete evidence, numbered human questions, scenario mappings, and oracle-guard results. Returns BLOCKED when a product decision is still BLOCKING_UNKNOWN and FAIL on missing, duplicate, reordered, malformed, or unevidenced rows. Pass the exact contract_checkpoint object, completed feature-contract Markdown, and independently drafted acceptance-scenario Markdown."
    )]
    pub async fn validate_feature_contract(
        &self,
        params: Parameters<ValidateFeatureContractRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_validate_feature_contract(params.0).await
    }

    #[tool(
        description = "Ingest reviewed guidance/findings into quality_gate (4 MiB source limit). Authored copilot/rules policies refresh their receipt-owned documents/promotions, retiring removed rows; historical exports coderabbit/sonarqube/board/text append unless replace_source=true. Manual ownership conflicts reject before mutation. JSON supports one path_scope or file_pattern using existing repository-rule matching; invalid scope rejects the whole import. Markdown retains full context. Managed policy freshness is checked by planning/review/injection; stale rules disclose incomplete coverage. Use adoption to preview/apply source-bound legacy ownership, or alias_review to review unchanged-only alias rebinds and exact owned-row retirement. Apply requires the exact preview token and caller decision reference; a reference does not authenticate human authority. Reviewed alias changes remain pending until normal source reimport completes. pre_push_audit retrieves evidence, not compliance proof."
    )]
    pub async fn ingest_quality_gates(
        &self,
        params: Parameters<IngestQualityGatesRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_ingest_quality_gates(params.0).await
    }

    #[tool(
        description = "Distill raw review findings (source_type=coderabbit|sonarqube|board|text) into reusable advisory quality_gate candidates through bounded LLM batches. Needs a configured LLM. Authored copilot/rules policies are rejected: use ingest_quality_gates to retain their scope, exceptions and source context. Distillation does not establish approval or prove a suggestion applies to every change."
    )]
    pub async fn distill_quality_gates(
        &self,
        params: Parameters<DistillQualityGatesRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_distill_quality_gates(params.0).await
    }

    #[tool(
        description = "Pre-push audit: given inline code or a project-relative diff_file with exact diff_file_blake3, retrieve relevant quality-gate rules/findings. Rules scoped to file_path rank first; retrieval is not compliance proof. Alternatively, instruction_maintenance collects complete authored instructions and Git-before/disk-after evidence. For new_instruction_targets, optional area_context (scope, max_files, exclude_paths) automatically assembles bounded indexed area files, direct consumers, examples and applicable shared guidance; evidence_paths become optional seeds. Discovery gaps and advisory review dispositions remain explicit. A drafting agent submits grounded exact edits or explained no_change/conflict outcomes bound to packet_blake3; Engram rejects stale dependencies, false citations and invalid anchors and returns proposed instruction/provenance files. Independent semantic review and human approval remain required. Never writes authored policy or guesses legacy ownership."
    )]
    pub async fn pre_push_audit(
        &self,
        params: Parameters<PrePushAuditRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_pre_push_audit(params.0).await
    }

    #[tool(
        description = "Generate the agent integration pack for a project: .claude/rules/engram-workflow.md (the mandated planning/safety loop, Claude Code), AGENTS.md (same loop for Codex/Copilot/other agents), .claude/settings.json reminder hooks that re-inject the workflow at edit/stop time, and a mergeable .mcp.json entry for this server. write_files=true installs the files into the project (never overwrites an existing settings.json or AGENTS.md; .mcp.json is only emitted). Re-run after every reindex that changes the project_id."
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
        description = "Hybrid lexical+vector search over indexed code/docs. Hits carry path, line range, doc_id, covering symbols (node_ids), and a 500-char snippet (include_content=true for full bodies). search_scope: 'code' (default) searches code; 'knowledge' searches curated agent memory (memory_bank notes, dreamer insights, business_logic, antipattern/wontfix/quality_gate rules); 'all' searches both, fused by rank and labelled by source — use it to recall what past sessions learned, not just where code lives. Knowledge scopes also fold in USER-LEVEL memory (the reserved __user__ project) so a standing preference recorded once surfaces everywhere, labelled source: user:* (set include_user_memory=false to exclude). Filter by author_filter / date_after / date_before (unix ms). Page with offset. Optional max_tokens bounds the whole reply using the documented UTF-8 estimate; complete hits and next-page offsets preserve recovery. Set output_json=true for the shared versioned search envelope: typed results and recovery actions, warnings, guidance, response_page and truncation metadata. The budget includes serialized JSON. Set semantic=false for exact-identifier lookups. Source results include applicable live repository instructions with hashes and explicit omissions. Follow full_chunk for source content plus guidance; citation_chunk recovers exact stored content without injected instructions. For literal/regex matching prefer grep_project."
    )]
    pub async fn search_memory(
        &self,
        params: Parameters<SearchMemoryRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_search_memory(params.0).await
    }

    #[tool(
        description = "Vector-only similarity search (no lexical fusion). Quality depends on the configured embedding backend — the response states which tier is active. Optional max_tokens bounds complete reply text using a UTF-8 estimate. output_json=true returns the shared versioned search envelope with typed hits/recovery actions, warnings and response_page metadata; JSON escaping counts toward the budget. Follow next_page offset within the same top_k ranking window; keep query, filters, top_k and use_mmr fixed. Full_chunk retrieves content with live repository guidance; citation_chunk recovers exact stored source. Prefer search_memory for most queries."
    )]
    pub async fn vector_search(
        &self,
        params: Parameters<VectorSearchRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_vector_search(params.0).await
    }

    #[tool(
        description = "Fast literal/regex grep over the indexed file set. Freshness stat/hash reads and source overlays honor path_prefix, exclude_path_prefixes and language; warnings describe that scope, not project-wide freshness. Uses the Tantivy trigram index as a prefilter — typically beats ripgrep on warm queries. Returns file:line:col matches with optional context lines. Optional max_tokens bounds complete Markdown or JSON text using a UTF-8 estimate. Follow response_page.next_offset within the same max_results window; keep other arguments fixed. Safe source hits supply current-file excerpt actions and a first-page fallback, separate from stored get_chunk citations; compare current hashes and coverage before relying on a stale locator."
    )]
    pub async fn grep_project(
        &self,
        params: Parameters<GrepProjectRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_grep_project(params.0).await
    }

    #[tool(
        description = "Fetch stored chunk content by doc_id. Default text has a 40000-byte content cap; logical_slice and repo-rule injection are optional. For exact stored UTF-8 line/byte pages and raw hashes, pass citation: {}, then its hash-bound continuation with the same project_id/namespace/doc_id; citation cannot use transformations. Optional max_tokens bounds complete serialized text including JSON escaping and guidance. output_json returns a structured document envelope. Follow response_page.next for hash-bound recovery; transformed output is indivisible. Re-search if the document identity is no longer available after indexing."
    )]
    pub async fn get_chunk(
        &self,
        params: Parameters<GetChunkRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_chunk(params.0).await
    }

    // ---- Memory bank + repo rules ----

    #[tool(
        description = "Write a persistent agent note (named markdown section) scoped to the project. Survives reindexing and is searchable (recall via search_memory search_scope=knowledge) — use for decisions, gotchas, preferences, and session handoffs. Set kind (preference|decision|gotcha|reference|note), author, tags, and related_files (files/symbols the note is about) to make recall meaningful and staleness detectable. append=true adds to the existing body; pass expected_updated_at_ms (from read_memory_bank) to avoid clobbering a concurrent session's edit. Long bodies are chunked automatically. Write to project_id=__user__ for cross-project preferences that should surface in every project's recall."
    )]
    pub async fn update_memory_bank(
        &self,
        params: Parameters<UpdateMemoryBankRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_update_memory_bank(params.0).await
    }

    #[tool(
        description = "Restore or copy a memory section from portable markdown (the `memory/*.md` files export_capture_pack produces). Front-matter (created_at, kind, author, tags, related_files) is preserved, so a restored note keeps its identity and age. Use to recover memories after a data_dir reset or to copy a note between projects."
    )]
    pub async fn import_memory_bank(
        &self,
        params: Parameters<crate::models::ImportMemoryBankRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_import_memory_bank(params.0).await
    }

    #[tool(description = "List the project's memory bank section names.")]
    pub async fn list_memory_bank(
        &self,
        params: Parameters<ProjectIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_list_memory_bank(params.0).await
    }

    #[tool(
        description = "Read one memory bank section by name. Discover sections with search_memory(search_scope=knowledge): memory hits return full_section and citation_section recovery arguments. The full profile also offers list_memory_bank. Optional citation returns exact stored UTF-8 content and hash-bound line/byte pages as JSON; Optional max_tokens bounds complete Markdown or JSON replies including provenance and escaping; output_json returns a structured document envelope. Follow response_page.next for hash-bound recovery. Explicit citation ranges and quote proofs remain indivisible."
    )]
    pub async fn read_memory_bank(
        &self,
        params: Parameters<ReadMemoryBankRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_read_memory_bank(params.0).await
    }

    #[tool(
        description = "Generate a SUPPORT-audience knowledge base: one card per product feature (taxonomy from ingested end-user docs sections `docs/**/<feature>/index`), each with role visibility, the docs' purpose/workflow prose, and code-derived business rules. write_to_disk=true writes <project>/support-kb/*.md (engram-owned, regenerated wholesale). Feeds support agents, a RAG support assistant, or email-support drafting."
    )]
    pub async fn produce_support_kb(
        &self,
        params: Parameters<crate::models::ProduceSupportKbRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_produce_support_kb(params.0).await
    }

    #[tool(description = "Delete one memory bank section by name.")]
    pub async fn delete_memory_bank(
        &self,
        params: Parameters<MemorySectionRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_delete_memory_bank(params.0).await
    }

    #[tool(
        description = "Add a persistent repository constraint. Rules are injected into get_chunk, surfaced during change-set planning, and enforced by review tools. Supply introduced_at and provenance when the rule must participate in point-in-time historical work."
    )]
    pub async fn add_repo_rule(
        &self,
        params: Parameters<AddRepoRuleRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_add_repo_rule(params.0).await
    }

    #[tool(
        description = "List repository rules with text, scope, priority, evidence date, provenance and stable rule_id. ownership_inventory=true returns exact raw rows/hashes, all receipt claims including malformed entries, existing project identity, source fingerprints and unfiltered enforcement observations. Optional source_paths supplies explicit candidates; matching names/text never proves ownership. Read-only; no adoption or policy acceptance."
    )]
    pub async fn list_repo_rules(
        &self,
        params: Parameters<ListRepoRulesRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_list_repo_rules_with_ownership(params.0).await
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
        description = "Edges in/out of a node_id grouped by kind (calls, SQL, state, UI, imports...). Use edge_kind='parameter_type' or 'returns_type' with direction='out' on a callable to inspect authored type expressions and parameter positions; follow 'type_definition' from a type reference for candidate declarations (not compiler proofs). Use resolve_id first if you only have a name. For name-based lookup use find_symbol_references."
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
        description = "Index commit history into temporal intelligence: co-change couplings, revert detection, author data. Run once after index_project; long-running on big repos (job — poll get_job_status). Unlocks analyze_temporal_couplings and analyze_reverts. Use mode='refresh' to regenerate already indexed diff text without changing graph relationships or history watermarks; repeat while more=true."
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
        description = "Search indexed commit messages, diffs and merged-PR records; one result per commit (author, date, subject, files it touched). Use for 'when/why did X change' questions and for precedents of a story after index_git_history. The query is literal text ranked by word-level BM25 plus semantic similarity (story prose is safe; no query syntax). By default only published history (the origin default branch) is searched. For leak-free replays pass as_of_rev = the story's base commit; date_before is exclusive, but dates alone admit branch commits dated before their merge. Precedent mode discloses its semantic, degraded trigram, or lexical fallback baseline. rerank_timeout_ms bounds only optional LLM reranking (0 skips it; default/max 90000); failures preserve baseline order. Optional max_tokens budgets the complete UTF-8 reply at bytes/4; output_json returns typed records. Offset pages whole records within the same limit window; mutable inputs and LLM reranking can reorder pages. Recovery identifies exact indexed documents and Git objects, with card clipping and matched-file coverage disclosed."
    )]
    pub async fn search_history(
        &self,
        params: Parameters<SearchHistoryRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_search_history(params.0).await
    }

    #[tool(
        description = "Import pull-request review threads as advisory verdict evidence; display-name replies, likes and labels do not authenticate authority or establish an exception's scope. Search summaries clip finding/reasoning text; new imports also retain complete hash-addressed supplied discussion captures for get_review_decisions recovery. Legacy uncaptured summaries remain incomplete. Imports refuse beyond 256 KiB/thread, 4 MiB capture batch or 1024 input rows, or the retained project quota (4096 total capture-metadata/raw-body objects/128 MiB); no old evidence is deleted. Configured decision-maker names classify replies and likes; PR-author arguments remain distinct. Feeds get_review_precedents; re-evaluate current code and never automatically suppress findings."
    )]
    pub async fn ingest_review_verdicts(
        &self,
        params: Parameters<IngestReviewVerdictsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_ingest_review_verdicts(params.0).await
    }

    #[tool(
        description = "Retrieve advisory prior review evidence; imported labels and quotations do not authenticate authority or establish accepted scope, revision applicability, or permission to suppress a current finding. Requires ingest_review_verdicts. Finding and reasoning may be clipped; follow the returned hash-bound get_review_decisions discussion reference for full captured supplied input. Legacy uncaptured records remain incomplete, and provider captures do not certify unreturned remote history. before_pr_id selects older PR IDs only, not as-of decisions; current author/trust aggregates are withheld under that cutoff. Re-evaluate the exact current code and exception conditions."
    )]
    pub async fn get_review_precedents(
        &self,
        params: Parameters<GetReviewPrecedentsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_review_precedents(params.0).await
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
        description = "Provide exactly one of file_path or symbol_fqn (symbol name or exact node ID). Trace revision-admitted incoming dependents of a file or symbol. Choose detail=summary for a bounded12KiB report; default graph can be large. For bodies, follow source_routes.reads before graph expansion; consumer_navigation offers small fresh steps with non-additive counts. Source ranges are indexed hints, not verified callsites. File queries exclude same-file dependents. Indexed dependencies, potential interface callers, unresolved lexical leads and historical co-change remain separate evidence. Follow gaps/recovery; reverse impact can miss input producers and runtime consumers. No edit simulation, exhaustive coverage or safety guarantee."
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
        description = "Query indexed SQL source declarations: tables, columns, foreign keys, indexes, triggers, views and stored procedures. query_type uses a kind discriminator (e.g. table with name=dbo.Orders). Preserves qualified identities and ambiguous definitions, verifies selected source hashes, and returns structured cited evidence with coverage gaps. No live database or migration execution. TablesWithColumn uses a literal substring; paginate with offset/limit."
    )]
    pub async fn query_schema(
        &self,
        params: Parameters<QuerySchemaRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_query_schema(params.0).await
    }

    #[tool(description = "Suggest SQL Server indexes from selected indexed SQL and inline VB query evidence. Compares WHERE/JOIN/ORDER BY access columns with source-declared leading keys, INCLUDE columns and applicable filters. Returns ranked source-site counts, citations, review-only CREATE INDEX candidates and coverage/selectivity warnings. No live database, execution plan, actual frequency or performance benefit is established. Optional file_path narrows query evidence; limit/offset page results.")]
    pub async fn find_missing_indexes(&self, params: Parameters<FindMissingIndexesRequest>) -> Result<CallToolResult, McpError> {
        self.handle_find_missing_indexes(params.0).await
    }

    #[tool(
        description = "Find possible repeated database calls in indexed VB.NET loops and authored WebForms RowDataBound/ItemDataBound handlers. Uses fresh Roslyn invocation ownership and bounded helper-call paths with source revision/hash checks. Returns repeated-call and database-call citations, heuristic severity and batching/JOIN guidance. Runtime execution, cache behavior and row counts remain unknown. Use file_path for one file, or file_offset/max_files to page selected VB files."
    )]
    pub async fn detect_n_plus_one(
        &self,
        params: Parameters<DetectNPlusOneRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_detect_n_plus_one(params.0).await
    }

    #[tool(
        description = "Indexed readers and writers of shared state. Modern types: Environment, Module, Singleton (JS/TS, Python, Rust, Go), LocalStorage, SessionStorage, ReactContext and Redux (JS/TS); also legacy Session/ViewState/Application/Cache. Environment and browser storage keys are literal names; other modern keys use project-relative path::declaration with any property suffix. Static extraction has explicit coverage limits; absence does not prove no runtime access."
    )]
    pub async fn trace_state_usage(
        &self,
        params: Parameters<TraceStateUsageRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(result) = self.state_tool_applicability(&params.0.project_id, &params.0.state_type).await? {
            return Ok(result);
        }
        self.handle_trace_state_usage(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Trace a UI control event down through code-behind to the SQL it ultimately executes."
    )]
    pub async fn trace_ui_event(
        &self,
        params: Parameters<TraceUiEventRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(result) = self.legacy_tool_applicability("trace_ui_event", &params.0.project_id, &[params.0.page_path.as_str()]).await? {
            return Ok(result);
        }
        self.handle_trace_ui_event(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Map a user-visible action (button text, link caption) to its handler in code-behind."
    )]
    pub async fn trace_ui_action(
        &self,
        params: Parameters<TraceUiActionRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(result) = self.legacy_tool_applicability("trace_ui_action", &params.0.project_id, &[]).await? {
            return Ok(result);
        }
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
        if let Some(result) = self.legacy_tool_applicability("get_ui_blueprint", &params.0.project_id, &[params.0.file_path.as_str()]).await? {
            return Ok(result);
        }
        self.handle_get_ui_blueprint(params.0).await
    }

    #[tool(
        description = "Navigate selected files and declaration candidates. Start discovery with include_declarations=false; follow returned file, symbol and page actions. For behavior in a known file, use include_source=true: current source accompanies navigation (8KiB source,12KiB reply). Named previews share120whole lines total; literal previews have up to3windows of120lines each. A sole named/ID candidate anchors a source page; multiple candidates get separate indexed-span windows without overload selection. For literal body text use source_contains with declarations off. Source reads are opt-in; hashes identify fresh snapshots, not indexed-boundary verification. Follow recovery/continuations and compare hashes across reads. Names, paths and text matches do not prove architecture, accepted policy, binding, uniqueness or absence. Missing, truncated and uninspected coverage remains unknown. No whole-project body scan."
    )]
    pub async fn get_codebase_overview(
        &self,
        params: Parameters<CodebaseOverviewRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_codebase_overview(params.0).await
    }

    #[tool(
        description = "Preview or publish CLAUDE.md, companion guidance and optional AGENTS.md. Preserves original repository-rule identity, scope and exceptions; accepted manual rules use an explicitly conditional catalog, not native path filtering. Returns complete recovery data for overflow, legacy authority questions and partial/conflicted writes. Source-managed policy stays referenced to its source. Optional use_llm ranks evidence IDs only; it cannot rewrite or approve policy. Existing human edits are preserved in tested conflict paths; inspect the publication report before relying on installed companions."
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
        description = "Status and persisted phase progress of one background job by job_id. Poll after index_project, update_project or index_git_history returns a job. Reports observed counts/unit, phase percentage, timestamps and estimated phase time remaining (null when unknown); phase100% is not whole-job completion."
    )]
    pub async fn get_job_status(
        &self,
        params: Parameters<CancelJobRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_job_status(params.0).await
    }

    #[tool(
        description = "Score proposed code against revert-derived anti-patterns (hybrid retrieval; the verdict follows how much of each reverted change the code repeats); returns matched rules with the reverting commit hash as evidence. Run before committing risky changes — this is how you avoid re-introducing what was already reverted once. Input: inline code OR project-relative code_file with exact raw code_file_blake3 (4 MiB UTF-8 maximum); file mode returns checked byte count/hash/path. Do not mix inputs; target context must match code_file."
    )]
    pub async fn immune_check(
        &self,
        params: Parameters<ImmuneCheckRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_immune_check(params.0).await
    }

    #[tool(
        description = "Check code against indexed anti-patterns and get remediation guidance with originating commits. Prefer immune_check for a numeric score; this tool for the 'what to do instead'. Input: inline code OR project-relative code_file with exact raw code_file_blake3 (4 MiB UTF-8 maximum); file mode returns checked byte count/hash/path. Do not mix inputs."
    )]
    pub async fn anti_pattern_guard(
        &self,
        params: Parameters<AntiPatternGuardRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_anti_pattern_guard(params.0).await
    }

    #[tool(
        description = "Pre-commit review — runs Engram's full registered gate set (19 graph-backed \
                       gates unless you pass `skip_gates`: immune, repo-rule, blast-radius, style, \
                       temporal, state, audit, anti-pattern, new-file, test-coverage, secret-leakage, \
                       guard-parity, unwired, product-intent, sync-contract, co-added-family, \
                       complexity, added-conventions, ui-house-style) over a unified diff and returns \
                       severity-ranked, evidence-backed findings. The response lists every gate that \
                       actually ran and its outcome, so you can see exactly what was and was not \
                       checked. Accepts a raw diff, `staged`, `unstaged`, `head`, or a `.patch` path. \
                       When the agent edits a linked worktree instead of the indexed checkout, pass \
                       `working_directory`; Engram verifies repository identity and binds both the diff \
                       and current-file snapshots to that worktree. Markdown defaults to a bounded compact \
                       ledger; pass `detail_level=full` for all explanations and snippets. No LLM calls."
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
                       runs via a per-source last_pr_id marker. For historical replays, set \
                       completed_before to the replay's exclusive evidence date, optionally \
                       max_pr_id as a second boundary, and force a full rescan; later and \
                       undated reviews are excluded before parsing or storage."
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
        description = "[.NET legacy] Complete manual event helper for VB.NET or C#, compatible with ingest_instrumentation_logs. Requires an explicit log sink and existing inline-SQL node IDs; no automatic interception, installation, or query hashing."
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
        if let Some(result) = self.legacy_tool_applicability("suggest_migration_boundaries", &params.0.project_id, &[]).await? {
            return Ok(result);
        }
        self.handle_suggest_migration_boundaries(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Ingest instrumentation logs to add observed-runtime edges to the graph (confirms or contradicts static analysis)."
    )]
    pub async fn ingest_instrumentation_logs(
        &self,
        params: Parameters<IngestInstrumentationLogsRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(result) = self.legacy_tool_applicability("ingest_instrumentation_logs", &params.0.project_id, &[]).await? {
            return Ok(result);
        }
        self.handle_ingest_instrumentation_logs(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Ingest runtime artifacts (IIS logs, traces, lifecycle snapshots, SQL profiler exports) and merge runtime evidence into the static graph."
    )]
    pub async fn ingest_runtime_artifacts(
        &self,
        params: Parameters<IngestRuntimeArtifactsRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(result) = self.legacy_tool_applicability("ingest_runtime_artifacts", &params.0.project_id, &[]).await? {
            return Ok(result);
        }
        self.handle_ingest_runtime_artifacts(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Compile a migration blueprint: inventory, risk hotspots, suggested boundaries and order. For the exhaustive one-call report use analyze_full_project_migration."
    )]
    pub async fn generate_migration_blueprint(
        &self,
        params: Parameters<GenerateMigrationBlueprintRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(result) = self.legacy_tool_applicability("generate_migration_blueprint", &params.0.project_id, &[]).await? {
            return Ok(result);
        }
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
        description = "Collect stale index generations while preserving selected source revisions and active readers. Optional native compaction combines small fragments, then attempts verified history pruning and reports old versions/bytes removed separately. Active native readers, uncertain completion, unsupported metadata or unverified files defer pruning; successful pruning removes old native versions."
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
        if let Some(result) = self.legacy_tool_applicability("generate_migration_plan", &params.0.project_id, &[]).await? {
            return Ok(result);
        }
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

    #[tool(
        description = "Tracked allocation reservations per subsystem, not process private memory or all native allocations. Optional project_id observes an already-cached runtime's snapshot/reader/cache/ranking retention without opening it, refreshing LRU or running cache maintenance; counts are bounded, non-atomic observations."
    )]
    pub async fn get_memory_budget(
        &self,
        params: Parameters<GetMemoryBudgetRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_memory_budget(params.0).await
    }

    #[tool(
        description = "Provide exactly one of file_path or symbol_fqn (symbol name or exact node ID). Defaults to a bounded12KiB summary with incoming-dependency witnesses, source reads and explicit graph recovery. Follow source_routes.reads for bodies and consumer_navigation for small steps; fresh counts are non-additive. File queries exclude same-file dependents. Indexed ranges are not verified callsites; potential interface callers, lexical leads and history are not bound primary callers. Explicit detail=graph adds full evidence, history and separate unverified guidance; include_guidance=false skips the guidance worker. Summary omits guidance/history. Inspect gaps and current source: reverse impact can miss producers and runtime consumers. No proposed-edit simulation, exhaustive coverage or safety guarantee."
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
        if let Some(result) = self.legacy_tool_applicability("generate_migration_scaffold", &params.0.project_id, &[params.0.file_path.as_str()]).await? {
            return Ok(result);
        }
        self.handle_generate_migration_scaffold(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Generate instrumentation snippets to capture runtime evidence before migrating a page/method."
    )]
    pub async fn generate_instrumentation_code(
        &self,
        params: Parameters<GenerateInstrumentationCodeRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(result) = self.legacy_tool_applicability("generate_instrumentation_code", &params.0.project_id, &params.0.target_files.iter().map(String::as_str).collect::<Vec<_>>()).await? {
            return Ok(result);
        }
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
        if let Some(result) = self.legacy_tool_applicability("suggest_state_migration", &params.0.project_id, &[]).await? {
            return Ok(result);
        }
        self.handle_suggest_state_migration(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Generate characterization (golden-master) tests for methods so behaviour is pinned before refactoring/migration."
    )]
    pub async fn generate_characterization_tests(
        &self,
        params: Parameters<GenerateCharacterizationTestsRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(result) = self.legacy_tool_applicability("generate_characterization_tests", &params.0.project_id, &[params.0.file_path.as_str()]).await? {
            return Ok(result);
        }
        self.handle_generate_characterization_tests(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Generate routing/proxy config (YARP-style) for strangler-fig incremental migration."
    )]
    pub async fn generate_strangler_fig_config(
        &self,
        params: Parameters<GenerateStranglerFigRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(result) = self.legacy_tool_applicability("generate_strangler_fig_config", &params.0.project_id, &[]).await? {
            return Ok(result);
        }
        self.handle_generate_strangler_fig_config(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Inventory WebForms validators per page: validator type, target control, rules, client/server enforcement."
    )]
    pub async fn map_validation_controls(
        &self,
        params: Parameters<MapValidationControlsRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(result) = self.legacy_tool_applicability("map_validation_controls", &params.0.project_id, &[params.0.file_path.as_str()]).await? {
            return Ok(result);
        }
        self.handle_map_validation_controls(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Authentication/authorization map from web.config and code: forms auth, roles, per-location rules, protected pages."
    )]
    pub async fn map_auth_config(
        &self,
        params: Parameters<MapAuthConfigRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(result) = self.legacy_tool_applicability("map_auth_config", &params.0.project_id, &[]).await? {
            return Ok(result);
        }
        self.handle_map_auth_config(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Page lifecycle hooks per page (Init, Page_Load, PreRender...) and what each one does — essential before reordering page logic."
    )]
    pub async fn map_page_lifecycle(
        &self,
        params: Parameters<MapPageLifecycleRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(result) = self.legacy_tool_applicability("map_page_lifecycle", &params.0.project_id, &[params.0.file_path.as_str()]).await? {
            return Ok(result);
        }
        self.handle_map_page_lifecycle(params.0).await
    }

    #[tool(
        description = "[.NET legacy] ViewState keys per page: writers, readers, postback dependencies, bloat candidates."
    )]
    pub async fn analyze_viewstate_deps(
        &self,
        params: Parameters<AnalyzeViewStateDepsRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(result) = self.legacy_tool_applicability("analyze_viewstate_deps", &params.0.project_id, &[params.0.file_path.as_str()]).await? {
            return Ok(result);
        }
        self.handle_analyze_viewstate_deps(params.0).await
    }

    #[tool(
        description = "[.NET legacy] UpdatePanels/AJAX regions per page with triggers and partial-postback scope."
    )]
    pub async fn map_ajax_regions(
        &self,
        params: Parameters<MapAjaxRegionsRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(result) = self.legacy_tool_applicability("map_ajax_regions", &params.0.project_id, &[params.0.file_path.as_str()]).await? {
            return Ok(result);
        }
        self.handle_map_ajax_regions(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Trace an event handler's ordered flow through controls, helper calls, SQL, bindings, redirects and shared state. Includes bounded source-cited C#/VB local value lineage and distinct helper-call contexts; unsupported effects and unbound calls are explicit, with no runtime-value proof. Bounded graph follow includes indirect state writes proved by uniquely resolved property setters; unresolved or ambiguous paths are reported rather than guessed."
    )]
    pub async fn trace_data_flow(
        &self,
        params: Parameters<TraceDataFlowRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(result) = self.legacy_tool_applicability("trace_data_flow", &params.0.project_id, &[params.0.file_path.as_str()]).await? {
            return Ok(result);
        }
        self.handle_trace_data_flow(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Full per-page migration dossier: everything needed to rewrite one page (controls, handlers, data, state, AJAX, risks). The per-page workhorse — prefer this over the full-project report inside agent loops."
    )]
    pub async fn get_migration_dossier(
        &self,
        params: Parameters<GetMigrationDossierRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(result) = self.legacy_tool_applicability("get_migration_dossier", &params.0.project_id, &[params.0.file_path.as_str()]).await? {
            return Ok(result);
        }
        self.handle_get_migration_dossier(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Which extraction dimensions are covered for a page — uncovered dimensions mean 'verify manually before migrating'."
    )]
    pub async fn check_migration_coverage(
        &self,
        params: Parameters<CheckMigrationCoverageRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(result) = self.legacy_tool_applicability("check_migration_coverage", &params.0.project_id, &[params.0.original_file.as_str()]).await? {
            return Ok(result);
        }
        self.handle_check_migration_coverage(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Record per-page migration status (not_started/in_progress/migrated/verified) in the durable progress tracker."
    )]
    pub async fn update_migration_status(
        &self,
        params: Parameters<UpdateMigrationStatusRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(result) = self.legacy_tool_applicability("update_migration_status", &params.0.project_id, &[params.0.file_path.as_str()]).await? {
            return Ok(result);
        }
        self.handle_update_migration_status(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Project-wide migration progress dashboard from the durable tracker."
    )]
    pub async fn get_migration_progress(
        &self,
        params: Parameters<GetMigrationProgressRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(result) = self.legacy_tool_applicability("get_migration_progress", &params.0.project_id, &[]).await? {
            return Ok(result);
        }
        self.handle_get_migration_progress(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Dependency-aware page/module migration order (graph topology + risk + shared-state coupling)."
    )]
    pub async fn suggest_migration_order(
        &self,
        params: Parameters<SuggestMigrationOrderRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(result) = self.legacy_tool_applicability("suggest_migration_order", &params.0.project_id, &[]).await? {
            return Ok(result);
        }
        self.handle_suggest_migration_order(params.0).await
    }

    #[tool(
        description = "[.NET legacy] One-call full migration report: inventory, DB, state, AJAX, JS, risks, order. LARGE output intended for humans/docs — inside agent loops prefer get_migration_dossier per page."
    )]
    pub async fn analyze_full_project_migration(
        &self,
        params: Parameters<AnalyzeFullProjectMigrationRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(result) = self.legacy_tool_applicability("analyze_full_project_migration", &params.0.project_id, &[]).await? {
            return Ok(result);
        }
        self.handle_analyze_full_project_migration(params.0).await
    }

    #[tool(
        description = "LLM-powered business-rule extraction: testable IF/THEN rules with source-line anchors and concrete refs (DB columns, Session keys, control IDs), persisted per executable member into the searchable business_logic namespace. Includes explicit VB property blocks with distinct Get/Set behavior; bodyless properties are excluded. Requires a configured LLM provider for nonempty members; failures are reported without publishing unusable rules. Source anchors and heuristic confidence do not establish semantic correctness. Retrieve later with query_business_logic."
    )]
    pub async fn analyze_business_logic(
        &self,
        params: Parameters<crate::models::requests::AnalyzeBusinessLogicRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_analyze_business_logic(params.0).await
    }

    #[tool(
        description = "Search stored business rules by domain concept (e.g. 'credit limit', 'checkout'). Returns anchored rule cards (purpose, IF/THEN rules with file:line, data flow, side effects) persisted by analyze_business_logic. Each card links to get_chunk for its full document; when the response budget omits cards, use the exact continuation call returned or the offset parameter. Run analyze_business_logic first to populate."
    )]
    pub async fn query_business_logic(
        &self,
        params: Parameters<crate::models::requests::QueryBusinessLogicRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_query_business_logic(params.0).await
    }

    // ── Phase 37: Wiring — Expose Existing Services ──────────────────────────

    #[tool(
        description = "Database intelligence from project SQL: schema, procedures, triggers, call chains and warnings. Defaults to a bounded reply; use response_delivery.next_call to recover an oversized report exactly without rerunning analysis."
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
        if let Some(result) = self.legacy_tool_applicability("analyze_sync_hazards", &params.0.project_id, &params.0.file_path.as_deref().into_iter().collect::<Vec<_>>()).await? {
            return Ok(result);
        }
        self.handle_analyze_sync_hazards(params.0).await
    }

    #[tool(
        description = "[.NET legacy] jQuery usage inventory: core version, vulnerabilities, UI widgets, third-party plugins, deprecated patterns with migration guidance."
    )]
    pub async fn get_jquery_inventory(
        &self,
        params: Parameters<GetJQueryInventoryRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(result) = self.legacy_tool_applicability("get_jquery_inventory", &params.0.project_id, &[]).await? {
            return Ok(result);
        }
        self.handle_get_jquery_inventory(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Reconstruct session/state workflows: trace how Session, Application, ViewState, Cache, and Cookie keys flow across pages. Detects MissingWriter, MissingReader, and complex cross-page chains."
    )]
    pub async fn get_session_workflows(
        &self,
        params: Parameters<GetSessionWorkflowsRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(result) = self.legacy_tool_applicability("get_session_workflows", &params.0.project_id, &[]).await? {
            return Ok(result);
        }
        self.handle_get_session_workflows(params.0).await
    }

    #[tool(
        description = "[.NET legacy] Detect VB.NET → C# translation traps: 14 categories of semantic differences (silent bugs like Nothing/ValueType, Is vs =; compile errors like On Error GoTo, ReDim Preserve, My.* namespace)."
    )]
    pub async fn get_vb_translation_traps(
        &self,
        params: Parameters<GetVbTranslationTrapsRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(result) = self.legacy_tool_applicability("get_vb_translation_traps", &params.0.project_id, &params.0.file_path.as_deref().into_iter().collect::<Vec<_>>()).await? {
            return Ok(result);
        }
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
        description = "Per-method indexed metadata: signature, callers/callees, DB tables, session keys, complexity, VB traps and kind. Complete applicable authored guidance is scoped to each returned method file; guidance and indexed previews do not verify current source or runtime behavior. JSON retains an array of method rows with project_policy_sources per row. max_tokens budgets the complete direct reply (default40KiB); oversized reports defer through response_delivery.next_call for exact immutable recovery without a new index/policy read. Recover the complete report before interpreting deferred metadata. Narrow large matches with file_path or a specific FQN. batch_read applies its own combined budget to complete method evidence."
    )]
    pub async fn get_method_info(
        &self,
        params: Parameters<GetMethodInfoRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_method_info(params.0).await
    }

    #[tool(
        description = "Read an indexed method by unique FQN, a strict explicit file+line_start+line_end range, or a bounded current-file excerpt using file_path without line_end (line_start defaults1). Alternatively file_path+source_ranges reads one to three ordered nonoverlapping inclusive {line_start,line_end} windows, at most120 total lines, from one current snapshot/hash. source_ranges excludes fqn/line_start/line_end and requires context_lines=0, include_caller_bodies=false. Invalid bounds or oversized original source windows fail without partial windows; gaps are not continuous source or verified method boundaries. Excerpts and source_ranges retain the4MiB file-read/32KiB rendered-output caps and raw full-file BLAKE3. Excerpts return actual bounds, total lines and next_page; each page is a fresh read, so compare hashes. Excerpts omit extra context and cannot expand callers; they prove neither method boundaries nor indexed freshness. Explicit-end ranges remain strict. FQN checks known fingerprints; legacy missing fingerprints remain unverified. Use unique FQN or get_method_edit_context for indexed method spans. Complete applicable authored guidance accompanies the returned source files and caller bodies in JSON/Markdown; recheck guidance for the final edit scope. max_tokens budgets the complete reply (default40KiB for methods,32KiB ceiling for excerpts/ranges/recovery). Valid source plus complete guidance may defer through response_delivery.next_call with exact original recovery and no new source read. Source-window limits remain strict; fresh excerpt next_page carries an explicitly chosen effective budget."
    )]
    pub async fn get_full_method_body(
        &self,
        params: Parameters<GetFullMethodBodyRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_full_method_body(params.0).await
    }

    #[tool(
        description = "Pre-edit context: assembles method info, full body, top callers (identities plus bounded fingerprint-verified lexical source excerpts; include_caller_bodies=true for full source; distinct indexed caller totals are separate from display caps), database footprint, session state flows, VB traps, sync hazards, bounded revision-admitted downstream paths, business-rule evidence, historical shipped-file leads (include_history=false to omit; merged_before for a historical cutoff), and the shared edit-safety verdict. Provider COVERAGE (complete / partial / truncated / failed / not run) describes available evidence, not exhaustive codebase coverage. Missing downstream or unassessed method-effect/call semantics prevent green. Empty semantic metadata is not absence of mutations, cancellation or outbound calls; unsupported sync-hazard languages are explicit. Static paths do not prove breakage or runtime safety. One-hop scores and confidence are advisory heuristics. Same-class overloads are refused until `line` selects one. Downstream evidence defaults to a bounded summary with counts, coverage, representative witnesses, bounded consumer-navigation steps when space permits and executable fresh graph recovery; downstream_detail=graph restores the legacy bounded graph payload. Safety decisions use the same original evidence in both modes. Optional bodies/history/business content have separate sizes. Use before modifying a method. Summary source_routes.reads contains bounded current-source calls, coalescing connected indexed declarations within120lines. Prefer those reads when bodies are needed before another Summary expansion. Links are selected static evidence, not value flow; input producers may lie outside reverse impact."
    )]
    pub async fn get_method_edit_context(
        &self,
        params: Parameters<GetMethodEditContextRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_method_edit_context(params.0).await
    }

    #[tool(
        description = "[.NET legacy] WebForms page context: controls, event handlers (bodies opt-in), data/state/AJAX, auth, composition and neighboring-page style with provider coverage. Read the completeness and runtime caveats; this is not exhaustive or runtime-verified UI behavior. Optional max_tokens bounds complete text/JSON replies at ceil(UTF-8 bytes/4), retaining identity, coverage, warnings and qualifications on every page. Follow literal context_offset/context_expected_hash continuations to recover deferred whole entries and method-body fragments; changed assembled evidence/options requires restart. No retained filesystem snapshot. Omit budget/continuation for the existing format. include_master_page/include_codebehind/include_house_style are honored."
    )]
    pub async fn get_page_context(
        &self,
        params: Parameters<GetPageContextRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(result) = self.legacy_tool_applicability("get_page_context", &params.0.project_id, &[params.0.aspx_file.as_str()]).await? {
            return Ok(result);
        }
        self.handle_get_page_context(params.0).await
    }

    #[tool(
        description = "Context for editing an existing method: style, caller examples, database/SP/state/control evidence, language diagnostics, sync hazards and scoped repository policy. Resolve the exact method first; cross-class/overload ambiguity and stale indexed source are refused. Providers remain best-effort: read warnings and method_coverage; an empty section is not proof of absence or complete extraction. Optional max_tokens bounds the complete UTF-8 reply at bytes/4 in Markdown or JSON. Budgeted pages retain policy qualifications, warnings, identity, coverage and freshness, and expose whole context entries or byte-addressed source fragments. Follow the literal continuation with context_offset/context_expected_hash and unchanged options; changed assembled evidence requires restarting. Recover every page before claiming complete context. Omit the budget and continuation fields for the existing complete response format."
    )]
    pub async fn prepare_implementation_context(
        &self,
        params: Parameters<PrepareImplementationContextRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_prepare_implementation_context(params.0).await
    }

    #[tool(
        description = "Post-generation SANITY net (NOT a compiler or semantic verifier). It runs project-derived checks, caller-assertion presence checks, and generic language lints with explicit evidence classes. Optional generator_receipt_file + exact SHA-256 adds independently re-hashed provenance from an external IDE/compiler/generator; it requires hash-bound code_file input, covers source and target, and fails on stale/tampered member hashes. Engram verifies the receipt and current bytes but does not invoke or trust a particular toolchain. The overall verdict is fail-closed: FAIL on any failing check; INSUFFICIENT when a modification lacks an exact indexed or receipt-verified target or no project-derived verification ran; WARN on warnings; PASS only when project-derived evidence verified and nothing warned. It does not parse syntax, compare signatures, execute code, or prove product behavior. Input: inline code OR project-relative code_file (4 MiB UTF-8 maximum) with exact raw code_file_blake3; generator workflows may instead use the target SHA-256 already present in their receipt as code_file_sha256. Hash fields are mutually exclusive and target context must match code_file."
    )]
    pub async fn validate_generated_code(
        &self,
        params: Parameters<ValidateGeneratedCodeRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_validate_generated_code(params.0).await
    }

    #[tool(
        description = "Check indexed table identities, simple single-table SELECT columns/aliases, and SQL anti-patterns. Other SQL shapes report incomplete coverage; joins, predicates, types, and stored-procedure contracts require database validation."
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
        description = "Append immutable advisory review-decision evidence; declared authors, accepted_exception labels and external attestations are not authenticated authority. Kinds: open, accepted_exception, claimed_fix, verified_fix. Atomically retain an exact review-ID companion for bounded ordinary-review discovery when current event text mentions a complete changed path. Older hashed rows remain undiscoverable until rerecorded with their original ID. Engram executes no imported checks and suppresses no findings."
    )]
    pub async fn record_review_decisions(
        &self,
        params: Parameters<crate::handlers::review_decisions::RecordReviewDecisionsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_record_review_decisions(params.0).await
    }

    #[tool(
        description = "Recover complete recorded immutable decision events for an exact review_id; imported declarations are advisory and never establish authenticated authority or accepted scope. Default event history is not a complete original discussion. discussion:{} lists retained imported discussion captures; thread_id plus capture_blake3 selects bounded immutable JSON pages, with UTF-8 offset continuation. Verify recovery_blake3 for assembled pages, capture_blake3 for their exact capture_metadata_json string, then the metadata raw_utf8_blake3/length for raw_utf8. Unchanged raw bodies are reused across provenance captures. No imported authors, URLs or accepted scope are authenticated. External verified-fix attestations are current only at the recorded commit with a clean worktree; Engram does not execute or authenticate those checks. Resolved threads alone are not proof of a fix."
    )]
    pub async fn get_review_decisions(
        &self,
        params: Parameters<crate::handlers::review_decisions::GetReviewDecisionsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_get_review_decisions(params.0).await
    }

    #[tool(
        description = "Find candidate methods with zero indexed incoming caller/dependency edges after common event/lifecycle exclusions. Inspects up to 10000 matching graph methods and returns a limited subset with coverage. Runtime reachability remains unknown, including private or missing-access methods; dynamic, unresolved and external calls are not ruled out. This does not establish global unreachability or safe removal."
    )]
    pub async fn find_dead_methods(
        &self,
        params: Parameters<FindDeadMethodsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_find_dead_methods(params.0).await
    }

    #[tool(
        description = "Standalone edit safety check: green/yellow/red advisory verdict from bounded revision-admitted downstream paths, one-hop blast radius, exact caller count, session writes, and body complexity, computed from the SAME evidence as get_method_edit_context with per-provider coverage. Missing or incomplete downstream evidence and unassessed method-effect/call semantics prevent green. Query completion and low branching complexity do not establish absence of state changes or cancellation. Static paths do not prove breakage or runtime safety; confidence is an uncalibrated heuristic. Downstream evidence defaults to a bounded summary with counts, coverage, representative witnesses, bounded consumer-navigation steps when space permits and executable fresh graph recovery; downstream_detail=graph restores the legacy bounded graph payload. Display projection never changes the safety decision. Use when you only need the verdict and its impact evidence. Summary source_routes.reads contains bounded current-source calls, coalescing connected indexed declarations within120lines. Prefer those reads when bodies are needed before another Summary expansion. Links are selected static evidence, not value flow; input producers may lie outside reverse impact."
    )]
    pub async fn check_edit_safety(
        &self,
        params: Parameters<CheckEditSafetyRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_check_edit_safety(params.0).await
    }
}

impl ServerHandler for Engram {
    fn get_info(&self) -> ServerInfo {
        let core_instructions = "Engram's limited profile advertises up to 32 tools, filtered by registered project capabilities. Start with \
            `list_projects` and `project_info` to select the project. Use `search_memory` \
            or `grep_project` to find indexed code, then pass returned citation arguments \
            to `get_chunk` for exact stored evidence. Discover stored project memory with \
            `search_memory(search_scope=knowledge)` and pass the returned citation_section \
            arguments to `read_memory_bank` for the complete section. \
            Follow symbols with `resolve_id` and `find_symbol_references`; \
            use `get_method_edit_context(include_full_body=true)` for complete method \
            content. Use `check_edit_safety` before edits and `pre_commit_review` on the \
            diff. Check `get_index_freshness`; index or refresh using `index_project` \
            and `update_project`. Poll asynchronous work with `get_job_status` and use \
            `cancel_job` to stop unwanted work. \
            Some responses refer to specialized tools in the full profile. Only the \
            tools advertised in this session can be assumed available to this client. \
            Follow returned review evidence calls with `get_review_decisions` for \
            exact retained events or discussions; workflow closure is not policy acceptance. \
            Set `advertise_all_tools=true`, restart Engram and reconnect the client \
            when full-profile capabilities are required.";
        ServerInfo {
            instructions: if !self.state.cfg.advertise_all_tools {
                Some(core_instructions.to_owned())
            } else {
                Some(
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
                 ASP migration work. Discovery reflects the union of registered project capabilities; \
                 project_info reports source evidence and coverage. Re-list tools after indexing jobs finish."
                        .to_string(),
                )
            },
            capabilities: ServerCapabilities::builder().enable_tools().enable_tool_list_changed().build(),
            ..Default::default()
        }
    }

    // Manual replacement for `#[tool_handler]`. `call_tool` still delegates to
    // the full router, so every tool stays callable; only `list_tools` is
    // curated + schema-sanitized (see crate::tool_surface) so a large or
    // `$ref`-laden advertised surface can't make a strict client (GitHub
    // Copilot) reject the whole request with HTTP 400.
    async fn call_tool(
        &self,
        request: CallToolRequestParam,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        // Per-call observability at INFO: tool name, RESPONSE SIZE, latency. The
        // response size is what a review's cumulative request budget is spent on
        // — logging it here is what turns "the review 400s somewhere" into
        // "grep_project on <x> returned N bytes". One line per call, cheap.
        let tool_name = request.name.to_string();
        let guidance_input = if matches!(crate::tool_guidance::policy(&tool_name), crate::tool_guidance::Policy::Preserve) {
            None
        } else {
            Some(crate::tool_guidance::context(request.arguments.as_ref()))
        };
        let peer = context.peer.clone();
        let started = std::time::Instant::now();
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        let mut result = self.tool_router.call(tcc).await;
        if let (Some(guidance_input), Ok(response)) = (&guidance_input, &mut result) {
            let guidance = crate::tool_guidance::annotate(
                &tool_name, guidance_input, response, &self.tool_router.list_all(),
                self.state.cfg.advertise_all_tools,
            );
            tracing::debug!(tool = %tool_name, guidance, "specialty tool navigation");
        }
        if matches!(tool_name.as_str(), "index_project" | "update_project" | "delete_project" | "repair_project" | "get_job_status") && result.is_ok() {
            // Async writers can finish later; clients can always re-list after
            // polling their job. This notification covers foreground changes.
            if let Err(error) = peer.notify_tool_list_changed().await {
                tracing::debug!(%error, "tool discovery notification unavailable");
            }
        }
        let ms = started.elapsed().as_millis();
        match &result {
            Ok(r) => {
                let bytes: usize = r
                    .content
                    .iter()
                    .filter_map(|c| c.as_text().map(|t| t.text.len()))
                    .sum();
                tracing::info!(tool = %tool_name, bytes, ms, "tool_call ok");
            }
            Err(e) => {
                tracing::warn!(tool = %tool_name, ms, error = %e, "tool_call failed");
            }
        }
        result
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParam>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        // Full discovery by default; constrained clients can opt into core.
        let mut items = crate::tool_surface::advertised(
            self.tool_router.list_all(),
            self.state.cfg.advertise_all_tools,
        );
        let registry = self.state.registry.clone();
        let projects = tokio::task::spawn_blocking(move || registry.list_projects()).await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let mut capabilities = crate::capabilities::project::ProjectCapabilities::default();
        let mut unknown = false;
        for project in projects {
            if project.directory.is_empty() { continue; } // user memory is not a source project
            capabilities.include_explicit_profile(&project.project_type);
            // Narrow explicit presets cannot admit WebForms/VB files. Broad
            // and WebForms profiles need their actual published file evidence.
            if matches!(crate::models::ProjectType::from_registry_str(&project.project_type),
                Some(crate::models::ProjectType::Rust | crate::models::ProjectType::Python
                    | crate::models::ProjectType::JavaScript | crate::models::ProjectType::CSharp
                    | crate::models::ProjectType::C | crate::models::ProjectType::Cpp)) { continue; }
            let observation = async {
                let runtime = self.ensure_project_runtime(&project.project_id).await?;
                let (generation, view) = self.pin_active_source_generation(&project.project_id, &runtime.search).await?;
                Ok::<_, McpError>((crate::services::project_service::selected_project_capabilities(
                    &project.project_type, generation, &view).0, view.is_managed()))
            }.await;
            match observation {
                Ok((observed, complete)) => { capabilities.union(&observed); unknown |= !complete; }
                Err(error) => { unknown = true; tracing::debug!(project_id=%project.project_id,%error,"project capability coverage unavailable"); }
            }
        }
        items.retain(|tool| unknown || !tool.description.as_deref().is_some_and(|d| d.starts_with("[.NET legacy]"))
            || capabilities.allows_legacy_tool(tool.name.as_ref()));
        for t in items.iter_mut() {
            let cleaned = crate::tool_surface::sanitize_schema(&t.input_schema);
            t.input_schema = std::sync::Arc::new(cleaned);
        }
        Ok(ListToolsResult::with_all_items(items))
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
