# Roadmap (status-aligned)

Status labels: `implemented`, `partial`, `experimental`, `planned`.

The matrix below is generated from capability flags and acts as the roadmap baseline for current maturity.

<!-- CAPABILITIES_MATRIX:START -->
| Tool / Feature | Status |
| :--- | :--- |
| `index_project` | implemented |
| `update_project` | implemented |
| `list_projects` | implemented |
| `project_info` | implemented |
| `project_health` | implemented |
| `repair_project` | implemented |
| `delete_project` | implemented |
| `watch_project` | implemented |
| `unwatch_project` | implemented |
| `search_memory` | implemented |
| `get_chunk` | implemented |
| `update_memory_bank` | implemented |
| `list_memory_bank` | implemented |
| `read_memory_bank` | implemented |
| `delete_memory_bank` | implemented |
| `add_repo_rule` | implemented |
| `list_repo_rules` | implemented |
| `delete_repo_rule` | implemented |
| `query_graph_nodes` | implemented |
| `find_references` | implemented |
| `graph_search` | implemented |
| `traverse_graph` | implemented |
| `index_git_history` | implemented |
| `ingest_zip_history` | implemented |
| `search_history` | implemented |
| `analyze_temporal_couplings` | implemented |
| `analyze_reverts` | implemented |
| `impact_analysis` | implemented |
| `get_table_schema` | implemented |
| `trace_state_usage` | implemented |
| `trace_ui_event` | implemented |
| `trace_ui_action` | implemented |
| `export_capture_pack` | implemented |
| `get_ui_blueprint` | implemented |
| `get_codebase_overview` | implemented |
| `find_symbol_references` | implemented |
| `analyze_error_stack` | implemented |
| `dream_project` | implemented |
| `trigger_rem_cycle` | implemented |
| `analyze_file_coding_style` | implemented |
| `list_jobs` | implemented |
| `cancel_job` | implemented |
| `get_job_status` | implemented |
| `immune_check` | implemented |
| `anti_pattern_guard` | implemented |
| `get_instrumentation_pack` | implemented |
| `suggest_migration_boundaries` | implemented |
| `ingest_instrumentation_logs` | implemented |
| `generate_migration_blueprint` | implemented |
| `ast_dependency_graph` | implemented |
| `vector_search` | implemented |
| `incremental_indexing_gc` | implemented |
| `dedicated_antipattern_index` | implemented |
| `get_metrics` | implemented |
| `check_integrity` | implemented |
| `evaluate_safety` | implemented |
| `generate_migration_plan` | implemented |
| `benchmark_retrieval` | implemented |
| `get_extraction_confidence` | implemented |
| `get_checkpoint_status` | implemented |
| `get_memory_budget` | implemented |
| `compute_blast_radius` | implemented |
| `detect_design_patterns` | implemented |
| `autonomous_decision_gate` | implemented |
| `graph_centrality_rerank` | implemented |
| `generate_migration_scaffold` | implemented |
| `generate_instrumentation_code` | implemented |
| `reconcile_runtime_evidence` | implemented |
| `suggest_state_migration` | implemented |
| `generate_characterization_tests` | implemented |
| `generate_strangler_fig_config` | implemented |
<!-- CAPABILITIES_MATRIX:END -->

## Focus areas

- **All tools at `implemented` status** — no `experimental`, `partial`, or `planned` entries remain. The final two tools were promoted in Phase 28:
  - `analyze_file_coding_style`: multi-language AST support (C#, TypeScript, Java, Go + existing Rust, Python), improved confidence calibration, edge-case hardening (parse error detection), line length + async pattern detection.
  - `graph_centrality_rerank`: multi-algorithm centrality (PageRank + degree + betweenness approximation), 3 modes (search+rerank, node scoring, top-N), configurable algorithm weights, Brandes k-pivot betweenness.

## Changelog

### Phase 30: End-to-End Migration Engine (8 gaps) + Phase 30a Hardening (7 fixes)
- **6 new tools**: `generate_migration_scaffold`, `generate_instrumentation_code`, `reconcile_runtime_evidence`, `suggest_state_migration`, `generate_characterization_tests`, `generate_strangler_fig_config`
- **Control mapping catalog** (`engram_index::control_mapping`): 50-entry WebForms → Blazor/React/Angular control mappings with accessibility, data binding, and event equivalents
- **Scaffold service** (`scaffold_service.rs`): Blazor/React/Angular component generator with real business logic from graph edges (SQL→repository, state→get/set, navigation→routing), async/await conversion guidance, repository interfaces, DTOs, test scaffolds
- **DB strategy service** (`db_strategy_service.rs`): Data access pattern classifier (8 patterns), repository interface generator, SQL injection risk scorer
- **Instrumentation service** (`instrumentation_service.rs`): C#/VB.NET HttpModule generator with route/session/SQL/postback/error tracing, InstrumentedSessionStateWrapper (IHttpSessionState impl), InstrumentedDbCommand (DbCommand subclass with timing), plus static-vs-runtime reconciliation
- **State migration service** (`state_migration_service.rs`): Per-key migration recommendations with access pattern analysis, ViewState lifecycle classification, and affinity grouping
- **Characterization test service** (`characterization_test_service.rs`): NUnit/xUnit/MSTest test generator producing executable test bodies with TestPageFactory, MockHttpSession, TestDbFactory, MockResponseRecorder helper infrastructure
- **Strangler fig service** (`strangler_fig_service.rs`): YARP reverse proxy config, Microsoft.FeatureManagement feature flags, routing middleware with percentage-based rollout and sticky sessions, health check endpoint, Program.cs registration with Polly circuit breaker/retry, CorrelationId middleware
- **VB.NET deep extraction**: Nested With block stack, On Error GoTo label resolution (two-pass), CreateObject return value propagation and alias tracking, late-bound method call detection, My. namespace, ReDim Preserve
- **GIS deep extraction**: 30+ Google Maps classes (Places, StreetView, Heatmap, KML, Directions, DistanceMatrix, Elevation, Geometry), 80+ Esri/ArcGIS ES module classes (widgets, tasks, renderers, geometry, portal, auth, 3D), migration complexity assessment
- **Classic ASP extractor** (`asp_classic_extractor.rs`): 7 detection categories with 31 tests
- **Report extractor** (`report_extractor.rs`): SSRS and Crystal Reports detection with 16 tests
- **Windows Service detection**: ServiceBase/TopShelf/BackgroundService recognition in pattern detection service
- **Config fields**: `scaffold_default_target_stack`, `scaffold_include_tests`, `enable_classic_asp_extraction`, `enable_report_extraction`, `characterization_test_framework`
- **138+ tests** across all Phase 30 modules (121 original + 17 strangler fig)

### Phase 27: Gold Standard Hardening (10 tickets)
- **Benchmark schemas** (`engram_core::benchmark`): `BenchmarkPack`, `AdpCorpus`, `TraceScenarioLibrary`, `DriftReport` with versioned schemas, per-class thresholds, and drift detection
- **ADP replay**: Deterministic `replay_from_scenario()` and batch `run_corpus()` with `AdpConfusionMatrix` for false-allow/false-deny calibration reporting
- **JSON audit reports**: `AdpDecisionReport` with `build_decision_report()` — immutable per-verdict report with gate-by-gate evidence, config snapshots, and input replay data
- **Rollout policy engine**: `RolloutPhase` (shadow/advisory/guarded/autonomous) with `apply_rollout_policy()` — kill-switch forces all decisions to deny
- **Runtime evidence schemas** (`engram_core::runtime_evidence`): `RuntimeEvent`, `RuntimeEvidenceBatch`, `ReconciliationResult` with `validate_batch()` schema validator
- **Trace provenance**: `trace_ui_event` enhanced with structured provenance block, unresolved candidates, per-hop evidence, follow-up probes
- **Safety calibration**: 7-scenario labeled corpus with false-allow rate ≤ 1% assertion
- **WebForms mutation tests**: 12 mutation tests for extraction robustness
- **Integrity canary tests**: 9 canary tests with synthetic drift injection
- **Benchmark CI**: `.github/workflows/benchmark-ci.yml` with artifact upload
- **Config fields**: `adp_rollout_phase`, `adp_kill_switch`
- **Bugfix**: `saturating_sub` overflow in ADP evidence sufficiency gate

### Phase 26: Autonomous Decision Protocol (ADP v1)
- **New tool**: `autonomous_decision_gate` — mandatory 8-gate verification pipeline for autonomous code changes (allow/deny/abstain verdicts)
- **Gate pipeline**: extraction confidence → trace certainty → safety policy → retrieval quality → blast radius → anti-pattern → runtime evidence → evidence sufficiency
- **Trace ambiguity fix**: `trace_ui_event` now emits fallback candidate metadata and confidence penalties instead of silently resolving to first match
- **Config fields**: `adp_enabled`, `adp_min_extraction_confidence`, `adp_max_blast_radius`
- **Service**: `autonomous_decision_service.rs` with 11 unit tests covering all acceptance criteria
- **Bugfix**: Fixed pre-existing `cs: Query` vs `Option<Query>` type mismatch in `engram_index/src/parsing.rs`
