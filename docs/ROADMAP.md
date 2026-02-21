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
| `analyze_file_coding_style` | experimental |
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
| `graph_centrality_rerank` | planned |
<!-- CAPABILITIES_MATRIX:END -->

## Focus areas

- **Graduate `experimental`**: `analyze_file_coding_style` is the sole remaining experimental tool — path to `implemented` requires edge-case hardening across more language/git-history combinations.
- **Implement `planned`**: `graph_centrality_rerank` is the only planned item; baseline behavior requires integrating PageRank scores into hybrid search re-ranking at query time.
- **No `partial` entries remain** — all formerly partial tools were fully promoted across Phases 21–25.

## Changelog

### Phase 26: Autonomous Decision Protocol (ADP v1)
- **New tool**: `autonomous_decision_gate` — mandatory 8-gate verification pipeline for autonomous code changes (allow/deny/abstain verdicts)
- **Gate pipeline**: extraction confidence → trace certainty → safety policy → retrieval quality → blast radius → anti-pattern → runtime evidence → evidence sufficiency
- **Trace ambiguity fix**: `trace_ui_event` now emits fallback candidate metadata and confidence penalties instead of silently resolving to first match
- **Config fields**: `adp_enabled`, `adp_min_extraction_confidence`, `adp_max_blast_radius`
- **Service**: `autonomous_decision_service.rs` with 11 unit tests covering all acceptance criteria
- **Bugfix**: Fixed pre-existing `cs: Query` vs `Option<Query>` type mismatch in `engram_index/src/parsing.rs`
