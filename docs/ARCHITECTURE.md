# Architecture Notes

## Storage
- **Registry (Redb)**: configuration-like mutable metadata and job tracking
- **Graph (Redb)**: adjacency-list edges with weights
- **Index (Tantivy)**: append-only search index, filtered by `generation`

## Hot path
- `search_memory`:
  1) Tantivy lexical search
  2) emit search session event (async) for dreaming co-occurrence
  3) return hits

## Background path
- Dreamer:
  - records co-occurrence edges
  - runs clustering when idle
  - inserts insight nodes back into graph

## Data directories
All project data lives under:
`{data_dir}/projects/{project_id}/...`

## Crash consistency
- Redb is ACID.
- Tantivy commits are atomic at segment level.
- Generation switching should be a two-step commit:
  1) write new generation docs
  2) update registry `active_generation`
  3) (future) GC old gens

## Scaling notes
- Use Rayon for parsing/indexing (future milestone)
- Keep embedding generation in a dedicated worker pool (Candle)
- Prefer message passing between actors over shared locks

## Indexing concurrency + memory under load
- All `index_files(...)` calls in `engram_server` now pass through a single parse guard
  (`AppState.parse_semaphore`) before entering the indexer.
- This includes:
  - synchronous initial indexing (`index_project(wait=true)`),
  - incremental updates (`update_project_impl`),
  - background index jobs (`spawn_job_index_directory`), and
  - background update jobs (`spawn_job_update_project`).

Expected behavior when many projects index concurrently:
- `max_concurrent_jobs` limits how many jobs can be active.
- `max_parse_concurrency` further limits how many jobs can be in the parse/chunk stage at once.
- Jobs above the parse limit wait on the semaphore instead of allocating parse/chunk working sets.

Memory impact:
- Peak parse/chunk memory is roughly bounded by
  `O(max_parse_concurrency × batch_working_set)` rather than
  `O(active_jobs × batch_working_set)`.
- This prevents bursty background indexing from multiplying transient file-buffer/chunk
  allocations across all jobs.

## Autonomous Decision Protocol (ADP) architecture

### Gate pipeline

The ADP pipeline in `autonomous_decision_service.rs` is an ordered chain of 8 gates. Each gate evaluates independently and produces a pass/fail/skip result:

```
extraction_confidence → trace_certainty → safety_policy → retrieval_quality
       → blast_radius → anti_pattern → runtime_evidence → evidence_sufficiency
```

**Verdict rules**: All pass → Allow; any hard fail → Deny; insufficient evidence → Abstain.

### Rollout state machine (Phase 27)

```
  Shadow ──→ Advisory ──→ Guarded ──→ Autonomous
  (log only)  (warn)      (enforce)   (auto-apply)
```

- `apply_rollout_policy(phase, decision)` transforms the raw gate verdict according to the current phase
- Shadow/Advisory modes always return Allow but annotate the response with what *would* have happened
- Guarded/Autonomous modes enforce the raw verdict
- `adp_kill_switch = true` overrides ALL phases → forces Deny

### JSON audit reports

`build_decision_report()` creates an immutable `AdpDecisionReport` containing:
- Gate-by-gate `GateEvidence` (gate_id, passed, score, detail string)
- `ConfigSnapshot` (rollout phase, kill-switch state, threshold values)
- Full `AdpScenarioInput` for deterministic replay
- ISO 8601 timestamp and final verdict

### Deterministic replay

The replay subsystem enables reproducible ADP testing:
- `replay_from_scenario(scenario)` — deserializes a scenario fixture into `AdpInput` and re-evaluates
- `run_corpus(corpus)` — batch-processes a labeled `AdpCorpus` and produces an `AdpConfusionMatrix`
- Confusion matrix tracks: true_allow, true_deny, true_abstain, false_allow, false_deny

## Benchmark & quality infrastructure (Phase 27)

### Schemas (`engram_core::benchmark`)

| Type | Purpose |
|------|---------|
| `BenchmarkPack` | Versioned query set with per-class thresholds (NDCG@10, Recall@10, MRR) |
| `AdpCorpus` | Labeled ADP scenario collection for calibration |
| `TraceScenarioLibrary` | WebForms trace fixture library for regression testing |
| `DriftReport` | Compares two benchmark runs and flags regressions exceeding class thresholds |

### Runtime evidence (`engram_core::runtime_evidence`)

| Type | Purpose |
|------|---------|
| `RuntimeEvent` | Normalized event: ControlInteraction, Route, SqlExecution, StateMutation |
| `RuntimeEvidenceBatch` | Collection with `validate_batch()` schema enforcement |
| `ReconciliationResult` | Per-path matching of predictions vs. observed runtime behavior |

### CI pipeline

`.github/workflows/benchmark-ci.yml` runs on every PR and push to main:
1. Benchmark unit tests (`engram_core::benchmark`)
2. ADP corpus replay tests (`corpus_runner`)
3. WebForms mutation tests (12 mutation scenarios)
4. Reproducibility tests (golden fixture determinism)
5. Artifact upload with 90-day retention

### Integrity canaries

9 canary tests in `integrity_canary_test.rs` inject synthetic drift:
- Tantivy orphans, docstore orphans, vector bloat, count divergence
- Namespace skew, empty stores, tolerance boundaries
- Repair policy override verification

## End-to-End Migration Engine (Phase 30 + 30a Hardening)

Phase 30 closes 8 structural gaps between legacy code comprehension and autonomous migration. Phase 30a hardens 7 shortcomings to production quality. The architecture extends across three crates:

### engram_index (extraction layer)
- `control_mapping.rs`: Static lookup table of 50 WebForms controls mapped to Blazor/React/Angular targets with accessibility and data binding metadata
- `asp_classic_extractor.rs`: Regex-based Classic ASP extraction (COM, ADO, state, SQL, navigation, includes, inline functions)
- `report_extractor.rs`: SSRS/Crystal Reports detection via regex pattern matching on markup and code-behind content
- `vb_extractor.rs` (enhanced): Stack-based nested With blocks, two-pass On Error GoTo label resolution, CreateObject variable propagation and alias tracking, late-bound method call detection
- `js_extractor.rs` (enhanced): 30+ Google Maps classes (Places, StreetView, Heatmap, KML, Directions, DistanceMatrix, Elevation, Geometry), 80+ Esri/ArcGIS ES module classes (widgets, tasks, renderers, geometry, portal, auth, 3D), migration complexity assessment

### engram_server (service layer)
Six new services follow the established pure-function + Serialize pattern:
- `scaffold_service.rs`: Reads graph context and generates framework-specific component code with real business logic (SQL→repository calls, state→get/set, navigation→routing) and async/await conversion guidance per framework
- `db_strategy_service.rs`: Classifies `DataAccessPattern` from edge metadata, generates repository interfaces, scores SQL injection risk
- `instrumentation_service.rs`: Generates C#/VB.NET `IHttpModule` code with configurable tracing, `InstrumentedSessionStateWrapper` (IHttpSessionState impl), `InstrumentedDbCommand` (DbCommand subclass with timing/error logging); reconciliation engine compares static graph paths against `RuntimeEvidenceBatch` events
- `state_migration_service.rs`: Classifies state stores from graph node targets, analyzes access patterns, recommends modern equivalents
- `characterization_test_service.rs`: Generates executable test bodies with helper infrastructure (TestPageFactory, MockHttpSession, TestDbFactory, MockResponseRecorder, MockHttpContext)
- `strangler_fig_service.rs`: Generates complete strangler fig infrastructure — YARP reverse proxy, feature flags, routing middleware with sticky sessions, health check, Program.cs with Polly + CorrelationId

### Strangler fig architecture
```
                    ┌─────────────────────────────────────┐
                    │         Modern ASP.NET Core          │
                    │                                       │
 Client ──→ YARP ──┤  CorrelationIdMiddleware              │
                    │  FeatureFlagMiddleware                │
                    │  StranglerFigMiddleware ──→ modern    │
                    │       │ (sticky sessions)             │
                    │       └──→ ProxyToLegacy ──→ legacy   │
                    │  MigrationHealthCheck (/health)       │
                    └─────────────────────────────────────┘
```

Key design decisions:
- **Sticky sessions**: Once a user is assigned modern or legacy for a page, they stay on that backend for the session duration (prevents mid-session flips during percentage rollout)
- **Polly resilience**: Circuit breaker (5 failures → 30s break) + exponential retry (3 attempts) on legacy proxy
- **Correlation IDs**: X-Correlation-Id header propagated across modern↔legacy boundary for distributed tracing
- **Progressive rollout**: `StranglerFig:Rollout:{PageName} = 0..100` config per page

### Data flow
```
Graph (Redb) ──→ Service (pure fn) ──→ Tool handler ──→ MCP response
                     ↑                      ↑
              Config defaults          Request params
```

All services take `&Arc<GraphStore>` and project ID, query graph edges/nodes in `spawn_blocking`, and produce serializable result structs. Tool handlers format results as Markdown or JSON depending on request parameters.

## Migration Workflow Engine (Phase 31)

Phase 31 builds the complete migration analysis workflow — 11 specialized services plus a single-call orchestrator.

### Service architecture

```
                        ┌────────────────────────────────────────────┐
                        │    analyze_full_project_migration           │
                        │    (full_project_migration_service.rs)      │
                        │                                            │
                        │  ┌─ migration_order_service ──→ wave plan  │
                        │  ├─ state_migration_service ──→ state rpt  │
                        │  ├─ auth_config_service ──→ auth config    │
                        │  ├─ db_strategy_service ──→ data access    │
                        │  └─ per-file: dossier_service ──→ dossier  │
                        │       ├─ lifecycle_service                  │
                        │       ├─ viewstate_service                  │
                        │       ├─ ajax_region_service                │
                        │       ├─ validation_mapping_service         │
                        │       └─ blast_radius_service               │
                        │                                            │
                        │  → CrossCuttingSummary (aggregated)         │
                        │  → markdown_report (rendered)               │
                        └────────────────────────────────────────────┘
```

### Async → blocking data flow

The `analyze_full_project_migration` tool handler follows a two-phase pattern:

1. **Async phase** (tokio runtime):
   - Query all file nodes from graph (via `spawn_blocking`)
   - Identify `.aspx`/`.ascx`/`.master` markup files
   - Read all markup + code-behind files concurrently via `futures::future::join_all`
   - Read web.config
   - Collect all `.cs`/`.vb` files for auth scanning

2. **Blocking phase** (via `spawn_blocking`):
   - Call `analyze_full_project()` with all pre-read content
   - Orchestrate all sub-services synchronously
   - Build cross-cutting summary
   - Render markdown report

This avoids async I/O inside `spawn_blocking` while keeping the blocking analysis in a dedicated thread.

### Progress persistence

`MigrationProgressStore` in `AppState` uses a standalone Redb database (separate from the graph store) to track per-file migration status. Operations:
- `update_status(project_id, file_path, status, notes, blocked_reason, blocking_deps)`
- `get_progress(project_id)` → per-file status list with rollup counts
- `list_files(project_id, status_filter)` → filtered file list

### Cross-cutting summary

The `CrossCuttingSummary` aggregates data across all per-file dossiers:
- **Shared SQL tables**: tables accessed by multiple files (coupling indicator)
- **Shared state keys**: Session/ViewState/Cache keys used across files (state coupling)
- **Shared user controls**: ASCX controls registered by multiple pages
- **Risk distribution**: count of files per risk band
- **Complexity distribution**: count of files per complexity level (Low/Medium/High)
- **Aggregate counts**: total validators, UpdatePanels, lifecycle events, files with IsPostBack
