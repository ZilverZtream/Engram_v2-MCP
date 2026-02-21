# Tool Parity Matrix (v1 vs v2)

Status legend: `implemented`, `partial`, `experimental`, `planned`.

This matrix is generated from `crates/engram_server/src/capabilities.rs` and must be kept in sync by `scripts/check_capabilities_matrix.py`.

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
| `analyze_reverts` | partial |
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
| `dream_project` | experimental |
| `trigger_rem_cycle` | experimental |
| `analyze_file_coding_style` | experimental |
| `list_jobs` | implemented |
| `cancel_job` | implemented |
| `get_job_status` | implemented |
| `immune_check` | experimental |
| `anti_pattern_guard` | experimental |
| `get_instrumentation_pack` | implemented |
| `suggest_migration_boundaries` | experimental |
| `ingest_instrumentation_logs` | implemented |
| `generate_migration_blueprint` | implemented |
| `ast_dependency_graph` | implemented |
| `vector_search` | experimental |
| `incremental_indexing_gc` | implemented |
| `dedicated_antipattern_index` | implemented |
| `graph_centrality_rerank` | planned |
<!-- CAPABILITIES_MATRIX:END -->

## Notes

- `planned` entries are roadmap targets and do not have shipping tool behavior yet.
- `experimental` entries are functional but may require LLM/embedding backends and have not been hardened for all edge cases.
- All formerly `partial` tools were promoted to `implemented` in Phase 21 (tool parity completion).
