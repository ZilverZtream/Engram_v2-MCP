# Tool Contract

Engram MCP v2 keeps v1 tool names and basic intent, with expanded parameters.

## Project tools
- `index_project(directory, project_name, project_type, wait=true, dedupe_by_directory=true)`
- `update_project(project_id, wait=true, max_commits=200, index_antipatterns=false)`
- `list_projects()`
- `project_info(project_id)`
- `project_health(project_id)` — per-namespace doc counts, graph/vector stats, disk usage, language/symbol breakdown, integrity warnings with actionable repair suggestions
- `repair_project(project_id, scope="full", wipe_and_reindex=false, max_commits=500, index_antipatterns=true)` — targeted repair scopes: `full`, `graph_only`, `tantivy_only`, `vector_only`; optional full wipe and re-index
- `delete_project(project_id)`
- `watch_project(project_id, enabled=true)`
- `unwatch_project(project_id)`

## Search
- `search_memory(project_id, query, top_k=10)`
- `get_chunk(project_id, chunk_id, namespace="memory")`
- `graph_search(project_id, query, max_results=10, symbol_boost=0.1, namespace="memory", fts_mode="strict", use_mmr=false, hop_depth=1, include_content=false, max_content_chars=400, expansion_edge_kinds=null)` — hybrid text + graph symbol name matching with multi-edge neighbor expansion, configurable FTS modes (strict/loose/regex), MMR diversity, and content preview
- `find_symbol_references(project_id, symbol_name, max_incoming=200, max_outgoing_per_kind=50, edge_kind_filter=null, file_scope=null)` — all-edge-kind graph lookup with FQN suffix matching, incoming/outgoing grouping by edge kind, edge kind and file scope filters, lexical fallback
- `get_codebase_overview(project_id)` — language breakdown, symbol-type aggregation, edge-kind distribution, architectural layers, PageRank, DB tables, state keys, temporal couplings, dead code detection, test coverage stats
- `analyze_error_stack(project_id, traceback, top_k=10)` — multi-language structured stacktrace parser with frame-boosted search

## Knowledge Graph
- `query_graph_nodes(project_id, query, top_k=10)`
- `find_references(project_id, node_id, top_k=10)`
- `traverse_graph(project_id, start_node, depth=3)`
- `impact_analysis(project_id, file_or_symbol, max_depth=3)`
- `ast_dependency_graph(project_id, entry, max_depth=6, direction="outgoing", compile_time_only=false, output_json=false)` — BFS graph traversal from an entry node with configurable direction (outgoing/incoming/both), compile-time edge filtering, and JSON or text tree output

## Git/history
- `index_git_history(project_id, max_commits=200, index_antipatterns=false, wait=true)`
- `ingest_zip_history(project_id, directory, wait=true)`
- `search_history(project_id, query, limit=5, file_filter=null, exclude_paths=null, author_filter=null, date_after=null, date_before=null, fts_mode="strict", use_mmr=false, max_content_chars=800)` — structured commit metadata extraction, configurable FTS modes, MMR, path exclusions, and content preview
- `analyze_temporal_couplings(project_id, file_path, top_k=10)`
- `analyze_reverts(project_id, max_commits=200)` — LLM-powered descriptive anti-pattern rule generation

## Cognitive / agent
- `dream_project(project_id)`
- `trigger_rem_cycle(project_id)` (alias)
- `analyze_file_coding_style(project_id, file_path, max_commits=200)`
- `immune_check(project_id, code)`
- `anti_pattern_guard(project_id, code, limit=5)`
- `suggest_migration_boundaries(project_id)` — LLM+deterministic migration boundary suggestion
- `generate_migration_blueprint(project_id, entry_node, max_depth=5, output_json=false, include_edge_kinds=null, exclude_dead_code=true)` — BFS context compilation from entry node with 9-section Markdown dossier or structured JSON output
- `autonomous_decision_gate(project_id, proposed_change, target_files, risk_profile="medium", require_runtime_evidence=false, output_json=false, extraction_confidence=null, immune_verdict=null, trace_used_fallback=false, trace_candidate_count=0, has_runtime_evidence=false)` — mandatory 8-gate verification pipeline (extraction confidence, trace certainty, safety policy, retrieval quality, blast radius, anti-pattern, runtime evidence, evidence sufficiency) returning allow/deny/abstain verdict with machine-readable failed gate IDs and required follow-ups

## Index Maintenance
- `incremental_indexing_gc(project_id, target_generation=null, compact_vectors=false)` — manual GC trigger with pre/post delta reporting for graph nodes, edges, tantivy docs, and lance vectors
- `dedicated_antipattern_index(project_id, action="stats", query=null, file_filter=null, limit=50)` — four actions: `stats` (doc count + repo rules), `list` (browse antipattern docs), `search` (hybrid search with content preview), `clear` (purge antipattern namespace)

## Memory bank + repo rules
- `update_memory_bank(project_id, section_id?, title, content)`
- `list_memory_bank(project_id)`
- `read_memory_bank(project_id, section_id)`
- `delete_memory_bank(project_id, section_id)`
- `add_repo_rule(project_id, file_pattern, rule_text)`
- `list_repo_rules(project_id)`
- `delete_repo_rule(project_id, rule_id)`

## UI & Schema Tracing
- `get_table_schema(project_id, table_name)`
- `trace_state_usage(project_id, state_key)`
- `trace_ui_event(project_id, page, control_id)` — traces ASPX page + control to SQL; now emits structured **Trace Provenance** block with `trace_used_fallback`, `trace_candidate_count`, `trace_confidence_penalty`, unresolved candidate list, per-hop evidence (node_type, file, line range), ambiguity warnings, and follow-up probes for disambiguation
- `trace_ui_action(project_id, page, control_id)`
- `get_ui_blueprint(project_id, page)`
- `get_instrumentation_pack(project_id)`
- `ingest_instrumentation_logs(project_id, log_data)`
- `export_capture_pack(project_id)`

## Safety & Autonomous Decision
- `evaluate_safety(project_id, affected_files, refactor_type, impact_node_count, impact_confidence, test_coverage, anti_pattern_clear, downstream_dependents, touches_global_state, touches_database)` — policy gate that blocks high-risk refactors unless confidence/coverage thresholds are met; returns go/no-go decision with risk level, individual check results, and suggested mitigations
- `get_extraction_confidence(project_id, extraction_type, source_content, codebehind_content=null)` — WebForms extraction confidence scoring for event_wiring / sql_trace / control_binding; returns 0.0–1.0 score with individual signal breakdown
- `compute_blast_radius(project_id, file_path=null, symbol_fqn=null, include_guidance=true)` — migration complexity score (1–10) with event wiring, SQL, PageRank, state coupling, GIS, and script injection sub-scores; returns seam candidates and agentic guidance
- `detect_design_patterns(project_id, pattern_filter=[], limit=50)` — detects God Object, Spaghetti Code, Session Soup and other anti-patterns using graph topology
- `autonomous_decision_gate(project_id, proposed_change, target_files, risk_profile="medium", require_runtime_evidence=false, output_json=false, extraction_confidence=null, immune_verdict=null, trace_used_fallback=false, trace_candidate_count=0, has_runtime_evidence=false)` — mandatory 8-gate verification pipeline (extraction confidence, trace certainty, safety policy, retrieval quality, blast radius, anti-pattern, runtime evidence, evidence sufficiency) returning allow/deny/abstain verdict with machine-readable failed gate IDs and required follow-ups

## Observability & Operations
- `get_metrics(output_json=false)` — global atomic counters (docs indexed, searches, refactors approved/blocked, extractions by confidence band)
- `get_memory_budget(output_json=false)` — per-subsystem memory breakdown (tantivy, lancedb, graph, docstore, parse_buffer) with pressure detection
- `get_checkpoint_status(project_id=null, job_id=null)` — crash-safe job resume via Redb-backed checkpoint store
- `check_integrity(project_id=null, auto_repair=false)` — background integrity checker for index/graph/docstore consistency
- `benchmark_retrieval(project_id, custom_queries=null, output_json=false)` — NDCG@10 + recall@10 + MRR with configurable ground-truth queries; returns production-ready gate decision
- `generate_migration_plan(project_id, output_json=false)` — builds PlanInput from graph nodes and outputs wave-based migration plan with rollback playbook

## Jobs
- `list_jobs(project_id?)`
- `cancel_job(job_id)`
- `get_job_status(job_id)`

## End-to-End Migration Engine (Phase 30)
- `generate_migration_scaffold(project_id, file_path, target_stack="blazor", include_test_scaffold=false, output_format="markdown")` — scaffold generator producing Blazor/React/Angular component skeletons with real business logic from graph edges (SQL→repository calls, state→get/set, navigation→routing), async/await conversion guidance, repository interfaces, DTOs, and test scaffolds
- `generate_instrumentation_code(project_id, target_files=[], language="csharp")` — runtime instrumentation code generator (C#/VB.NET HttpModule, web.config entries) with route/session/SQL/postback/error tracing. Generates `InstrumentedSessionStateWrapper` (IHttpSessionState implementation) and `InstrumentedDbCommand` (DbCommand subclass with timing/error logging)
- `reconcile_runtime_evidence(project_id, evidence_json)` — compares static graph paths (SQL calls, state reads/writes, dependencies, postbacks) against a `RuntimeEvidenceBatch` JSON blob; returns confirmed/contradicted/inconclusive paths with confidence ratio
- `suggest_state_migration(project_id, output_json=false)` — per-key state migration recommendations (Session → distributed cache, ViewState → URL/component state, etc.) with access pattern analysis, ViewState lifecycle classification, and affinity grouping
- `generate_characterization_tests(project_id, file_path, framework="nunit", output_json=false)` — NUnit/xUnit/MSTest characterization test generator producing executable test bodies with TestPageFactory, MockHttpSession, TestDbFactory, MockResponseRecorder helper infrastructure covering event handlers, data flows, state transitions, navigation, and API contracts
- `generate_strangler_fig_config(project_id, legacy_base_url="http://localhost:5000", modern_base_url="http://localhost:5001")` — generates complete strangler fig migration infrastructure: YARP reverse proxy configuration, Microsoft.FeatureManagement per-page feature flags, routing middleware with percentage-based rollout and sticky session affinity, health check endpoint with migration progress, Program.cs registration with Polly circuit breaker/retry, and CorrelationId middleware for cross-boundary tracing

## Migration Workflow Engine (Phase 31)

### Per-file analysis tools
- `map_validation_controls(project_id, file_path, output_json=false)` — maps WebForms validators (RequiredFieldValidator, CompareValidator, CustomValidator, etc.) to DataAnnotation/FluentValidation equivalents with validation group analysis
- `map_auth_config(project_id, file_path=null, output_json=false)` — analyzes web.config auth mode (Forms/Windows/None), location authorization rules, membership/role provider config, and code-level auth patterns (IsInRole, FormsAuthentication, Authorize attributes)
- `map_page_lifecycle(project_id, file_path, output_json=false)` — maps Page lifecycle events (Page_Init, Page_Load, Page_PreRender, etc.) to modern framework equivalents, detects IsPostBack branching, identifies implicit WebForms behaviors (ViewState, event wiring, control tree)
- `analyze_viewstate_deps(project_id, file_path, output_json=false)` — extracts explicit ViewState["key"] usage and implicit control-level ViewState, recommends modern state type per field (component state, URL parameter, distributed cache, etc.)
- `map_ajax_regions(project_id, file_path, output_json=false)` — inventories UpdatePanel/ScriptManager regions, maps partial postback triggers, suggests component decomposition boundaries
- `trace_data_flow(project_id, file_path, entry_point=null, output_json=false)` — traces data flow from entry point to sinks (SQL, state writes, response), tracks state reads/writes and SQL table access, detects cross-file dependencies

### Orchestration tools
- `get_migration_dossier(project_id, file_path, target_stack="blazor", output_json=false)` — builds a comprehensive per-file migration dossier orchestrating lifecycle, ViewState, AJAX, validation, auth, blast radius, and scaffold generation into one report
- `check_migration_coverage(project_id, original_file, modern_code, output_json=false)` — compares original page against modern code to identify migration gaps across 6 categories: lifecycle, data binding, validation, state management, navigation, authentication

### Progress tracking tools
- `update_migration_status(project_id, file_path, status, notes=null, blocked_reason=null, blocking_dependencies=[])` — updates migration status for a file (not_started/in_progress/done/blocked) with optional notes and blocking dependency tracking
- `get_migration_progress(project_id, output_json=false)` — retrieves overall migration progress for a project with per-file status breakdown
- `suggest_migration_order(project_id, output_json=false)` — computes topologically sorted migration plan via Kahn's algorithm, groups files into parallelizable waves, detects dependency cycles, identifies bottleneck files

### Full project analysis
- `analyze_full_project_migration(project_id, target_stack="blazor", max_files=200, output_json=false)` — the single-call migration analysis tool. Reads all markup + code-behind files concurrently, then orchestrates migration_order + state_migration + auth_config + db_strategy + per-file dossiers. Produces comprehensive FullProjectMigrationReport with CrossCuttingSummary (shared SQL tables, shared state keys, shared user controls, risk distribution, complexity distribution) and rendered markdown report covering Executive Summary, Auth, State Management, Data Access, Wave Plan, Cross-Cutting Concerns, Page-by-Page Dossiers, and Risk Assessment

## ADP Infrastructure (Phase 27)

The `autonomous_decision_gate` tool is backed by production hardening infrastructure:

### Rollout Policy Engine
ADP verdicts pass through `apply_rollout_policy()` which applies phase-specific behavior:
- **Shadow**: Log verdict, always return Allow (no enforcement)
- **Advisory**: Log verdict, return Allow but include warning annotation
- **Guarded**: Enforce verdict — Allow, Deny, Abstain applied as-is
- **Autonomous**: Enforce verdict — identical to Guarded (full autonomous operation)
- **Kill-switch override**: When `adp_kill_switch=true`, ALL verdicts forced to Deny regardless of rollout phase

Config fields: `adp_rollout_phase` (default: `"shadow"`), `adp_kill_switch` (default: `false`)

### Deterministic Replay
- `replay_from_scenario(scenario)` — converts serialized `AdpScenarioInput` into `AdpInput` for reproducible verdict computation
- `run_corpus(corpus)` — batch-runs a labeled corpus and produces `AdpConfusionMatrix` with false-allow/false-deny rates

### JSON Decision Reports
- `build_decision_report(input, decision)` — generates `AdpDecisionReport` with:
  - Per-gate `GateEvidence` (gate_id, passed, score, detail)
  - `ConfigSnapshot` (rollout_phase, kill_switch, min_extraction_confidence, max_blast_radius)
  - Full `AdpScenarioInput` for replay
  - ISO 8601 timestamps, verdict, failed gate IDs

### Runtime Evidence Schemas
- `RuntimeEvent` — typed event: `ControlInteraction`, `Route`, `SqlExecution`, `StateMutation`
- `RuntimeEvidenceBatch` — collection of events with `validate_batch()` schema validation
- `ReconciliationResult` — per-path reconciliation with `Confirmed`/`Contradicted`/`Unmatched` statuses

### Notes
- All tools are fully implemented and compile. See `capabilities.rs` for the status matrix.
- v2 adds extra tools (e.g. `immune_check`, `ast_dependency_graph`, `dedicated_antipattern_index`) beyond v1 parity.
