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
<!-- CAPABILITIES_MATRIX:END -->

## Phase 27: Gold Standard Hardening

### New modules

| Module | Crate | Purpose |
|--------|-------|---------|
| `benchmark.rs` | `engram_core` | Benchmark schemas: `BenchmarkPack`, `AdpCorpus`, `TraceScenarioLibrary`, `DriftReport` with versioned schemas and per-class thresholds |
| `runtime_evidence.rs` | `engram_core` | Runtime evidence: `RuntimeEvent`, `RuntimeEvidenceBatch`, `ReconciliationResult` with `validate_batch()` schema validator |

### New test files

| Test file | Crate | Tests | Purpose |
|-----------|-------|-------|---------|
| `tests/webforms_mutation_test.rs` | `engram_index` | 12 | Mutation tests for WebForms extraction robustness (renamed handlers, duplicate IDs, malformed directives, empty control IDs, etc.) |
| `tests/integrity_canary_test.rs` | `engram_server` | 9 | Canary tests with synthetic drift injection (Tantivy/docstore orphans, vector bloat, namespace skew, repair policy override) |

### Modified service: `autonomous_decision_service.rs`

New APIs added to the existing ADP service:
- **Rollout policy**: `RolloutPhase` enum (`Shadow`, `Advisory`, `Guarded`, `Autonomous`), `apply_rollout_policy()` with kill-switch support
- **JSON reports**: `AdpDecisionReport`, `GateEvidence`, `ConfigSnapshot`, `build_decision_report()`
- **Replay**: `replay_from_scenario()`, `run_corpus()`, `AdpCorpusResult`, `AdpConfusionMatrix`
- **Bugfix**: `saturating_sub` overflow in evidence sufficiency gate

### Modified service: `safety_service.rs`

- 7-scenario `calibration_corpus()` with labeled expected verdicts
- `SafetyConfusionMatrix` for tracking true/false allow/deny rates
- `false_allow_rate ≤ 1%` assertion on high-risk scenarios

### Modified tool handler: `trace_ui_event`

- Structured `## Trace Provenance` block with ambiguity metadata
- Per-hop `evidence:` annotation with node type, file, line range
- `### Follow-up Probes` section with specific disambiguation actions

### New config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `adp_rollout_phase` | String | `"shadow"` | Rollout phase: `shadow`, `advisory`, `guarded`, `autonomous` |
| `adp_kill_switch` | bool | `false` | Emergency kill-switch — forces all ADP verdicts to Deny |

### CI workflow

`.github/workflows/benchmark-ci.yml`: Runs benchmark unit tests, ADP corpus replay, WebForms mutations, and reproducibility tests on every PR and push to main. Generates artifacts with 90-day retention.

## Phase 28: Tool Graduation

Promoted `analyze_file_coding_style` from experimental to implemented and `graph_centrality_rerank` from planned to implemented.

### Modified files

| File | Changes |
|------|---------|
| `crates/engram_ml/src/mimicry.rs` | Language detection and coding style analysis upgrades |
| `crates/engram_graph/src/analysis.rs` | Graph centrality algorithms |
| `crates/engram_server/src/tools.rs` | Tool handler wiring for graduated tools |
| `crates/engram_server/src/models/requests.rs` | New `GraphCentralityRerankRequest` struct |
| `crates/engram_server/src/capabilities.rs` | Status promotions: experimental/planned → implemented |
| `crates/engram_ml/Cargo.toml` | Added tree-sitter grammar dependencies |

### New APIs

| API | Module | Description |
|-----|--------|-------------|
| `compute_multi_centrality` | `engram_graph::analysis` | Computes multiple centrality measures (degree, betweenness, closeness) in a single pass |
| `MultiCentrality` | `engram_graph::analysis` | Struct holding combined centrality scores per node |
| `blended_score` | `engram_graph::analysis` | Weighted combination of centrality measures for reranking |
| `DetectedLanguage` | `engram_ml::mimicry` | Enum/struct for language detection results used by coding style analysis |
| `approximate_betweenness` | `engram_graph::analysis` | Sampling-based betweenness centrality for large graphs |

### New request struct

| Struct | Crate | Description |
|--------|-------|-------------|
| `GraphCentralityRerankRequest` | `engram_server` | Request parameters for `graph_centrality_rerank` tool: query, node IDs, centrality weights, top-k |

### New tree-sitter grammars

| Grammar | Purpose |
|---------|---------|
| `tree-sitter-c-sharp` | C# parsing for coding style analysis |
| `tree-sitter-typescript` | TypeScript parsing for coding style analysis |
| `tree-sitter-java` | Java parsing for coding style analysis |
| `tree-sitter-go` | Go parsing for coding style analysis |

## Source-of-truth sync rule

When changing feature maturity:
1. Update `crates/engram_server/src/capabilities.rs`.
2. Run `python3 scripts/check_capabilities_matrix.py --write`.
3. Ensure `python3 scripts/check_capabilities_matrix.py` passes locally and in CI.
