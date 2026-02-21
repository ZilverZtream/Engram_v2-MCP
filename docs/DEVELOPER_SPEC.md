# Engram MCP v2 Developer Specification

This repo is a Rust workspace for Engram MCP v2: a developer tool that combines code search, graph reasoning, git history analysis, and cognitive helper tools.

Canonical capability status labels are tracked in code (`crates/engram_server/src/capabilities.rs`) and mirrored in docs through the generated matrix below.

- `implemented`: production-ready behavior in-tree
- `partial`: available but missing notable parity/quality pieces
- `experimental`: usable but intentionally iterative/tunable
- `planned`: design target only

## Workspace layout

```text
engram-v2/
├── Cargo.toml
├── crates/
│   ├── engram_core/
│   ├── engram_index/
│   ├── engram_graph/
│   ├── engram_git/
│   ├── engram_ml/
│   └── engram_server/
└── docs/
```

## Core invariants

1. Filesystem access is gated by `allowed_roots` via `PathContext::resolve_path`.
2. `engram_core::Registry` is the source of truth for projects, jobs, watches, rules, and memory-bank records.
3. Generational indexing (`active_generation`) prevents duplicate/legacy result leakage.
4. Namespaces: `memory`, `memory_bank`, `history`, `antipattern`.

## Capabilities matrix (generated)

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
| `graph_centrality_rerank` | planned |
<!-- CAPABILITIES_MATRIX:END -->

## Source-of-truth sync rule

When changing feature maturity:
1. Update `crates/engram_server/src/capabilities.rs`.
2. Run `python3 scripts/check_capabilities_matrix.py --write`.
3. Ensure `python3 scripts/check_capabilities_matrix.py` passes locally and in CI.
