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
| `get_index_freshness` | implemented |
| `resolve_id` | implemented |
| `get_concept_footprint` | implemented |
| `find_similar_changes` | implemented |
| `find_implementation_pattern` | implemented |
| `map_guards_and_settings` | implemented |
| `plan_user_story` | implemented |
| `get_change_set` | implemented |
| `validate_feature_contract` | implemented |
| `ingest_quality_gates` | implemented |
| `distill_quality_gates` | implemented |
| `pre_push_audit` | implemented |
| `get_gis_inventory` | implemented |
| `resolve_graph_edges` | implemented |
| `find_connection_path` | implemented |
| `find_dependency_cycles` | implemented |
| `detect_incomplete_changes` | implemented |
| `ask_codebase` | implemented |
| `begin_edit_session` | implemented |
| `complete_edit_session` | implemented |
| `generate_agent_integration` | implemented |
| `ingest_review_findings` | implemented |
| `search_memory` | implemented |
| `get_chunk` | implemented |
| `update_memory_bank` | implemented |
| `list_memory_bank` | implemented |
| `read_memory_bank` | implemented |
| `delete_memory_bank` | implemented |
| `import_memory_bank` | implemented |
| `produce_support_kb` | implemented |
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
| `produce_claude_md` | implemented |
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
| `pre_commit_review` | implemented |
| `ingest_code_review_history` | implemented |
| `explain_change` | implemented |
| `get_instrumentation_pack` | implemented |
| `suggest_migration_boundaries` | implemented |
| `ingest_instrumentation_logs` | implemented |
| `ingest_runtime_artifacts` | implemented |
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
| `map_validation_controls` | implemented |
| `map_auth_config` | implemented |
| `map_page_lifecycle` | implemented |
| `analyze_viewstate_deps` | implemented |
| `map_ajax_regions` | implemented |
| `trace_data_flow` | implemented |
| `get_migration_dossier` | implemented |
| `check_migration_coverage` | implemented |
| `update_migration_status` | implemented |
| `get_migration_progress` | implemented |
| `suggest_migration_order` | implemented |
| `analyze_full_project_migration` | implemented |
| `analyze_business_logic` | implemented |
| `query_business_logic` | implemented |
| `analyze_database_intelligence` | implemented |
| `get_sp_details` | implemented |
| `list_triggers` | implemented |
| `analyze_sync_hazards` | implemented |
| `get_jquery_inventory` | implemented |
| `get_session_workflows` | implemented |
| `get_vb_translation_traps` | implemented |
| `get_csharp_diagnostics` | implemented |
| `get_c_diagnostics` | implemented |
| `get_cpp_diagnostics` | implemented |
| `get_rust_diagnostics` | implemented |
| `get_method_info` | implemented |
| `get_full_method_body` | implemented |
| `get_method_edit_context` | implemented |
| `get_page_context` | implemented |
| `prepare_implementation_context` | implemented |
| `validate_generated_code` | implemented |
| `validate_sql_fragment` | implemented |
| `find_tests_for_method` | implemented |
| `find_dead_methods` | implemented |
| `check_edit_safety` | implemented |
<!-- CAPABILITIES_MATRIX:END -->

## Notes

- `planned` entries are roadmap targets and do not have shipping tool behavior yet.
- `experimental` entries are functional but may require LLM/embedding backends and have not been hardened for all edge cases.
- All formerly `partial` tools were promoted to `implemented` in Phase 21 (tool parity completion).
