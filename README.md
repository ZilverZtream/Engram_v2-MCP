# Engram MCP v2

A production-grade **Model Context Protocol (MCP) server** written in Rust that gives AI agents deep, structured understanding of codebases. Engram combines full-text search, semantic vector search, knowledge graphs, git intelligence, and cognitive reasoning into a unified tool suite for LLM-powered development workflows.

## What It Does

Engram connects your AI agent (Claude, etc.) to one or more local code repositories. Once a project is indexed, the agent can search code semantically, traverse dependency graphs, detect temporal coupling in git history, analyze coding style, flag anti-patterns from reverted commits, and reason about database schemas and UI event paths — all without reading raw files directly.

The server communicates over **STDIO** using the MCP protocol and is designed to run as a sidecar alongside your AI client.

---

## Features

### Hybrid Search
- **Full-text search** via Tantivy with camelCase/snake_case tokenization
- **Semantic vector search** via LanceDB with configurable embedding backends (local trigram projection, Ollama, OpenAI)
- **Reciprocal Rank Fusion (RRF)** combining FTS and vector rankings
- **Maximal Marginal Relevance (MMR)** for result diversity
- Namespace-aware search across code, notes, git history, and anti-pattern indexes

### Knowledge Graph
- Builds a typed property graph of your codebase using Tree-sitter AST parsing
- 33 edge kinds: `CoOccurrence`, `TemporalCoupling`, `Insight`, `Dependency`, `AntiPattern`, `Contains`, `Imports`, `SqlCalls`, `HasColumn`, `ForeignKey`, `QueriesTable`, `ReadsState`, `WritesState`, `DataBinding`, `RegistersControl`, `IncludesFile`, `UnresolvedStateRead`, `UnresolvedStateWrite`, `ExposesWebService`, `ExposesHttpHandler`, `ExposesWcfService`, `ContainsUi`, `UiLayoutNeighbor`, `ReadsColumn`, `RegistersModule`, `RegistersHandler`, `ManipulatesDom`, `TriggersPostback`, `ApiCall`, `ParameterBinding`, `SpatialCall`, `StateAffinity`, `InjectsScript`
- Node types: `function`, `class`, `interface`, `file`, `db_table`, `db_column`, `global_state`, `control`, `ui_container`, `control_layout`, `web_service`, `http_handler`, `wcf_service`, `application`, `http_module`, `route_handler`, `app_setting`, `connection_string`, `binding_field`, `insight`, `memory_bank_section`
- O(degree) adjacency lookups via Redb-backed composite-key adjacency lists (bincode serialization)
- PageRank scoring for codebase overview ranking
- Per-type and per-kind aggregation for architectural analysis

### Git Intelligence
- Indexes full commit history and diffs
- Detects **temporal coupling** — files that change together frequently, revealing hidden dependencies
- **Immune system**: harvests reverted commits and indexes their content as anti-patterns; new code submissions are scored against this history
- Supports both `git2`-backed repos and zip snapshot archives (for repos without git)

### Cognitive Features
- **REM Dreaming**: clusters search co-occurrence patterns into insight nodes that surface non-obvious relationships
- **Style mimicry**: analyzes recent diffs for a file and generates a style guide (naming, indentation, patterns) suitable for injection into LLM prompts
- **Impact analysis**: multi-hop graph traversal to estimate what breaks when a file or symbol changes
- **Anti-pattern guard**: scores a code draft against the immune index with remediation suggestions

### Language Support
Tree-sitter parsers for: **Rust, Python, Go, Java, C#, TypeScript, JavaScript, C, C++, VB.NET**

Special handling for **ASP.NET WebForms** (ASPX, ASCX, Master pages): control ID extraction, event wiring, code-behind `Inherits` tracing, UI-to-SQL path tracing.

Special handling for **Classic ASP** (.asp): COM object detection, ADO connection/recordset, Session/Application/Request/Response access, inline SQL, Server.Transfer, Response.Redirect, SSI include files.

### Database Analysis
- SQL DDL extraction (tables, columns, foreign keys)
- Cross-references SQL identifiers to application code
- `get_table_schema` returns columns, FK relationships, and every code location that references the table
- `trace_state_usage` tracks readers and writers of global state (Session, ViewState, Application, Cache)

### Memory Bank & Repo Rules
- Per-project **memory bank**: structured notes the agent writes and reads across sessions (architectural decisions, constraints, known issues)
- **Repo rules**: file-pattern-matched constraints injected into chunk retrieval (e.g., "all files matching `*Repository.cs` must use the Unit of Work pattern")

### Autonomous Decision Protocol (ADP)
- **Mandatory gate pipeline** for auto-applied changes: 8 ordered verification gates that must all pass before an agent can modify code autonomously
- Gates: extraction confidence, trace certainty, safety policy, retrieval quality, blast radius, anti-pattern, runtime evidence, evidence sufficiency
- Three verdicts: `allow` (all gates pass), `deny` (hard failure), `abstain` (insufficient evidence — agent must gather more data)
- Machine-readable output with per-gate results, failed gate IDs, and required follow-up actions
- Configurable thresholds: `adp_min_extraction_confidence`, `adp_max_blast_radius`, safety thresholds
- Trace ambiguity scoring: `trace_ui_event` emits fallback candidate metadata and confidence penalties when control lookup is ambiguous
- **Trace provenance**: structured provenance block in `trace_ui_event` with unresolved candidates, per-hop evidence, follow-up probes, and disambiguation guidance
- **Rollout policy engine**: four-phase rollout (shadow → advisory → guarded → autonomous) with emergency kill-switch that forces all verdicts to `deny`
- **JSON decision reports**: immutable per-verdict audit reports with gate-by-gate evidence, config snapshots, and input replay data via `build_decision_report()`
- **Deterministic ADP replay**: `replay_from_scenario()` and batch `run_corpus()` with confusion matrix calibration (false-allow rate ≤ 1%)

### Incremental Indexing & Watching
- Blake3 file fingerprinting for change detection; unchanged files are copy-forwarded without re-parsing
- Generation-based append-only model: queries always filter to the active generation
- Optional **file watcher** that triggers incremental re-index on directory changes

---

## Architecture

```
engram_core/     Core types, config, security boundary, Redb registry
engram_index/    Tantivy FTS + LanceDB vectors + DocStore + hybrid search
engram_graph/    Redb-backed graph store + BFS/PageRank algorithms
engram_git/      libgit2 walker, temporal coupling, revert detection
engram_ml/       Dreaming engine, embedders, style mimicry, immune system
engram_server/   MCP server (rmcp), tool handlers, background actors
```

**Storage layers:**
| Layer | Technology | Purpose |
|-------|-----------|---------|
| Registry | Redb | Projects, jobs, memory bank, repo rules, metadata |
| Graph | Redb | Nodes, edges, adjacency lists |
| Full-text index | Tantivy | Namespaced search (code, history, anti-patterns) |
| Vector index | LanceDB | Semantic embeddings |
| DocStore | Redb | File fingerprints, chunk-to-file mapping |

**Background actors:**
- **Dreamer** — co-occurrence clustering → insight generation
- **Immune Actor** — git revert harvesting → anti-pattern indexing
- **Watcher** — directory monitoring → incremental re-index
- **GC Scheduler** — orphaned job cleanup
- **Integrity Sentinel** — periodic cross-store consistency checks with auto-repair

### Enterprise Features (Phase 23)

- **Observability**: Lock-free metrics (counters, gauges, histograms) for job latency, queue depth, index drift, cardinality, repair outcomes, memory pressure, checkpoint recovery, extraction confidence, and safety decisions
- **Memory Budget & Backpressure**: Per-subsystem memory tracking (Tantivy, LanceDB, Graph, DocStore, ParseBuffer), soft/hard limits with CAS-based allocation, backpressure rejection for OOM prevention
- **Crash-safe Job Orchestration**: Redb-backed durable checkpoints with phase tracking (Scanning→Parsing→TantivyIndexing→VectorIndexing→GraphBuilding→PostProcessing), idempotency keys, and resume-from-failure
- **Data Integrity Sentinels**: Cross-store consistency verification (Tantivy vs LanceDB vs Graph vs Docstore doc counts), mismatch detection with 5% tolerance, configurable auto-repair, periodic background checker
- **Retrieval Production Gates**: NDCG@10, Recall@10, MRR benchmarking against known-relevant query sets; configurable pass/fail thresholds for search quality gating
- **WebForms Confidence Scoring**: Signal-weighted scoring for event wiring (5 signals), SQL trace (5 signals), and control binding (4 signals) extractions, with High/Medium/Low band classification
- **Safety Rails**: Policy engine blocking high-risk refactors based on impact confidence, test coverage, anti-pattern clearance, blast radius, global state safety, and database safety checks
- **Migration Execution Workflow**: Wave-ordered migration plans with topological sort, seam identification, contract test templates, compatibility adapter patterns, and per-wave rollback playbooks
- **Complete Revert Pipeline**: `analyze_reverts` promoted to Implemented with LLM-powered descriptive anti-pattern rules, graph edge creation, and metrics recording
- **Deterministic Reproducibility**: Golden-repo fixture tests verifying stable chunk IDs, graph edges, and search results across clean vs incremental indexing

### Gold Standard Hardening (Phase 27)

- **Benchmark Schemas**: Versioned `BenchmarkPack`, `AdpCorpus`, `TraceScenarioLibrary`, and `DriftReport` types with per-class thresholds and regression detection
- **ADP Replay & Calibration**: Deterministic scenario replay with `AdpConfusionMatrix` for false-allow/false-deny calibration, 7-scenario safety corpus with ≤ 1% false-allow assertion
- **Runtime Evidence Schemas**: Normalized `RuntimeEvent` format for control interactions, SQL execution, state mutations, and routes, with batch validation and reconciliation
- **Trace Provenance**: Enhanced `trace_ui_event` with structured provenance blocks, unresolved candidates, per-hop evidence, confidence penalties, and follow-up probes
- **WebForms Mutation Tests**: 12 mutation tests validating extraction robustness against renamed handlers, duplicate IDs, malformed directives, and edge cases
- **Integrity Canary Tests**: 9 canary tests with synthetic drift injection for Tantivy, docstore, and vector store orphan detection
- **Rollout Policy Engine**: Four-phase progressive rollout (shadow → advisory → guarded → autonomous) with emergency kill-switch
- **JSON Audit Reports**: Immutable `AdpDecisionReport` with gate-by-gate evidence, config snapshots, and input replay data
- **Benchmark CI**: GitHub Actions workflow running benchmark, ADP corpus, mutation, and reproducibility tests with 90-day artifact retention

### End-to-End Migration Engine (Phase 30 + 30a Hardening)

- **Control Mapping Catalog**: 50-entry WebForms → modern UI control mapping with Blazor, React, and Angular targets, accessibility attributes, data binding patterns, and event equivalents
- **Migration Scaffold Generator**: Produces full component code for Blazor/React/Angular with real business logic from graph edges (SQL→repository calls, state→get/set, navigation→routing), async/await conversion guidance, repository interfaces, DTOs, and test scaffolds
- **Database Strategy Advisor**: Classifies data access patterns (inline SQL, stored proc, DataSet, DataReader, Entity Framework, LINQ-to-SQL), generates repository interfaces, and scores SQL injection risk
- **Runtime Instrumentation Pipeline**: Generates injectable C# and VB.NET HttpModule code for runtime tracing (routes, session, SQL, postbacks, errors) with web.config entries, auto-generated `InstrumentedSessionStateWrapper` (IHttpSessionState) and `InstrumentedDbCommand` (DbCommand with timing), plus reconciliation of static graph paths against runtime evidence
- **State Migration Advisor**: Per-key state migration recommendations (Session, ViewState, Application, Cache, Cookie, QueryString, HiddenField) with access pattern analysis, ViewState lifecycle classification, and affinity grouping
- **Characterization Test Generator**: Produces executable NUnit/xUnit/MSTest test classes with TestPageFactory, MockHttpSession, TestDbFactory, MockResponseRecorder helper infrastructure covering event handlers, data flows, state transitions, navigation, and API contracts
- **Strangler Fig Infrastructure Generator**: Complete incremental cutover infrastructure — YARP reverse proxy configuration, Microsoft.FeatureManagement per-page feature flags, routing middleware with percentage-based rollout and sticky session affinity, migration health check endpoint, Program.cs registration with Polly circuit breaker/retry, and CorrelationId middleware for cross-boundary tracing
- **VB.NET Deep Extraction**: Nested With block stack, On Error GoTo label resolution (two-pass), CreateObject return value propagation with alias tracking, late-bound method call detection, My. namespace, ReDim Preserve
- **GIS Deep Extraction**: 30+ Google Maps classes (Places, StreetView, Heatmap, KML, Directions, DistanceMatrix, Elevation, Geometry), 80+ Esri/ArcGIS ES module classes (widgets, tasks, renderers, geometry, portal, auth, 3D), migration complexity assessment
- **Classic ASP Extractor**: 7 detection categories (COM objects, ADO, state access, SQL, navigation, includes, inline functions) with 31 tests
- **Report Extractor**: SSRS (.rdlc/.rdl) and Crystal Reports detection with parameter, dataset, subreport, and table reference extraction
- **Windows Service Detection**: ServiceBase/TopShelf/BackgroundService pattern recognition in graph topology

### Migration Workflow Engine (Phase 31)

- **Per-File Analysis Primitives**: 6 tools for deep per-file analysis — validation mapping (WebForms → DataAnnotation/FluentValidation), auth config (Forms/Windows/code-level patterns), page lifecycle mapping (Page_Init → OnInitialized), ViewState dependency extraction, UpdatePanel/AJAX region mapping, and entry-to-sink data flow tracing
- **Migration Dossier**: Orchestrates all per-file analysis services into a single comprehensive dossier with lifecycle events, ViewState keys, AJAX regions, validators, auth patterns, blast radius, and scaffold preview
- **Coverage Checker**: Compares original legacy page against modern migrated code to identify gaps across 6 categories (lifecycle, data binding, validation, state, navigation, auth)
- **Progress Tracker**: Standalone Redb-backed migration status database tracking per-file status (not_started/in_progress/done/blocked) with blocking dependency chains
- **Migration Wave Planner**: Kahn's algorithm topological sort producing parallelizable migration waves with cycle detection and bottleneck identification
- **Full Project Analysis** (`analyze_full_project_migration`): The single-call "give me everything" tool — reads all markup + code-behind files concurrently, orchestrates migration order, state migration, auth config, data access classification, and per-file dossiers in one call. Produces a `FullProjectMigrationReport` with cross-cutting summary (shared SQL tables, shared state keys, shared user controls, risk distribution) and a comprehensive rendered markdown report

---

## Installation

### Prerequisites
- Rust 1.81+ (edition 2024)
- On Linux/macOS: `libgit2` and `cmake` development packages
- Optional: [Ollama](https://ollama.ai) for local LLM/embedding backends

### Build

```bash
git clone https://github.com/your-org/engram-mcp-v2
cd engram-mcp-v2
cargo build --release
```

The compiled binary is at `target/release/engram_server`.

---

## Configuration

Create a YAML config file (default path: `engram_mcp.yaml`):

```yaml
# Required
allowed_roots:                        # Directories the server is permitted to index
  - /home/user/projects
  - /home/user/work

data_dir: /home/user/.engram-data     # Where all persistent data is stored

# Embedding backend (default: "local")
embedding_backend: local              # "local" | "ollama" | "openai"
embedding_model: nomic-embed-text     # Model name (for ollama/openai)
ollama_url: http://localhost:11434
openai_api_key: sk-...
openai_api_base: https://api.openai.com/v1   # Optional custom base

# LLM backend for cognitive features (default: "none")
llm_backend: none                     # "none" | "ollama" | "openai"
llm_model: llama3.2                   # e.g. gpt-4o-mini, llama3.2, mistral
llm_ollama_url: http://localhost:11434
llm_openai_api_key: sk-...
llm_openai_api_base: https://api.openai.com/v1

# Optional limits
max_project_files: 100000
max_project_bytes: 5368709120         # 5 GB
max_chunks_per_file: 2000
max_concurrent_jobs: 2
max_commits_per_watch: 50

# Safety & Autonomous Decision Protocol (ADP)
safety_policy_enabled: true           # Enable safety gates for automated edits
safety_min_confidence: 0.7            # Minimum impact confidence to allow edits
safety_min_coverage: 0.6              # Minimum test coverage to allow edits
adp_enabled: true                     # Enable mandatory ADP gate pipeline
adp_min_extraction_confidence: 0.5    # Minimum extraction confidence for ADP
adp_max_blast_radius: 6               # Max blast radius score (1-10) for auto-apply
adp_rollout_phase: shadow             # Rollout phase: shadow|advisory|guarded|autonomous
adp_kill_switch: false                # Emergency kill-switch — forces all ADP verdicts to Deny
```

Set the config path via environment variable:

```bash
export ENGRAM_CONFIG_PATH=/path/to/engram_mcp.yaml
```

---

## Running the Server

```bash
# Development
ENGRAM_CONFIG_PATH=./engram_mcp.yaml cargo run -p engram_server

# Production (built binary)
ENGRAM_CONFIG_PATH=/etc/engram/config.yaml ./target/release/engram_server

# With debug logging
RUST_LOG=debug ENGRAM_CONFIG_PATH=./engram_mcp.yaml cargo run -p engram_server
```

The server runs over **STDIO**. Do not print anything to stdout from your application — all stdout is reserved for MCP protocol messages. Logs go to stderr.

---

## MCP Client Setup

### Claude Desktop (`claude_desktop_config.json`)

```json
{
  "mcpServers": {
    "engram": {
      "command": "/path/to/engram_server",
      "env": {
        "ENGRAM_CONFIG_PATH": "/path/to/engram_mcp.yaml"
      }
    }
  }
}
```

### Claude Code (`.mcp.json`)

```json
{
  "mcpServers": {
    "engram": {
      "type": "stdio",
      "command": "/path/to/engram_server",
      "env": {
        "ENGRAM_CONFIG_PATH": "/path/to/engram_mcp.yaml"
      }
    }
  }
}
```

---

## Tool Reference

### Project Lifecycle

| Tool | Description |
|------|-------------|
| `index_project` | Index a local directory. Parameters: `directory`, `project_name`, `project_type`, `wait`, `dedupe_by_directory` |
| `update_project` | Incremental re-index of changed files. Parameters: `project_id`, `wait`, `max_commits`, `index_antipatterns` |
| `list_projects` | List all indexed projects |
| `project_info` | Detailed project metadata |
| `project_health` | Comprehensive health check: per-namespace doc counts, graph/vector stats, disk usage, language/symbol breakdown, integrity warnings with actionable repair suggestions |
| `delete_project` | Delete a project and all its stored data |
| `repair_project` | Targeted index repair with scoped rebuild (`full`, `graph_only`, `tantivy_only`, `vector_only`), optional full wipe-and-reindex |

### Search

| Tool | Description |
|------|-------------|
| `search_memory` | Hybrid FTS + vector search. Parameters: `query`, `project_id`, `namespace`, `max_results`, `use_mmr`, `fts_mode`, `include_content`, `max_content_chars_per_result`, language/path filters |
| `vector_search` | Standalone pure vector (semantic) search with 3x oversampling, configurable timeout, MMR reranking, path/language filters, and content preview |
| `get_chunk` | Fetch full content for a specific chunk by ID, with optional repo rule injection |
| `graph_search` | Hybrid text + graph symbol name matching with multi-edge neighbor expansion, configurable FTS modes (strict/loose/regex), MMR diversity, content preview, and edge-kind-filtered expansion |
| `find_symbol_references` | All-edge-kind graph lookup with FQN suffix matching, incoming/outgoing grouping by edge kind, configurable limits, edge kind and file scope filters, lexical fallback |
| `get_codebase_overview` | Language breakdown, symbol-type aggregation, edge-kind distribution, architectural layers, PageRank, DB tables, state keys, temporal couplings, dead code detection, test coverage stats |
| `analyze_error_stack` | Multi-language structured stacktrace parser (Python, .NET, Java, Node.js, Rust, Go, PHP, Ruby, ASP.NET) with frame-boosted search and graph centrality |

### Knowledge Graph

| Tool | Description |
|------|-------------|
| `query_graph_nodes` | Query nodes by type, name pattern, or file path |
| `find_references` | Find incoming or outgoing edges from a node |
| `traverse_graph` | Multi-hop BFS traversal from a start node |
| `impact_analysis` | Estimate what breaks if a file or symbol changes |

### Git & Temporal Analysis

| Tool | Description |
|------|-------------|
| `index_git_history` | Index commit history for temporal coupling + anti-patterns |
| `ingest_zip_history` | Ingest a folder of zip snapshots as pseudo git history |
| `search_history` | Search commit messages and diffs with structured metadata extraction, configurable FTS modes, MMR, path exclusions, and content preview |
| `analyze_temporal_couplings` | Detect files that frequently change together |
| `analyze_reverts` | Detect reverted commits, generate LLM-powered descriptive anti-pattern rules, and index reverted diffs |

### Cognitive Features

| Tool | Description |
|------|-------------|
| `vector_search` | Standalone semantic vector search with configurable top-k, 3x oversampling, MMR reranking, path/language filters, and per-query timeout |
| `dream_project` | Cluster co-occurrence patterns and generate insight nodes. Configurable clustering params (min_edge_weight, min_cluster_size, max_clusters), per-call timeout, config-driven defaults |
| `trigger_rem_cycle` | Alias for `dream_project` with the same configurable parameters |
| `analyze_file_coding_style` | Analyze a file's git history and produce a style guide |
| `immune_check` | Hybrid FTS + vector search against the anti-pattern index with configurable thresholds, structured verdict/severity/confidence output, and empty-index detection |
| `anti_pattern_guard` | Score code against anti-patterns with regex-based revert commit extraction, hybrid search, single-fetch content retrieval, and structured remediation guidance |
| `suggest_migration_boundaries` | LLM + deterministic migration boundary suggestion using iterative union-find with rank heuristic, cross-cluster dependency analysis, shared data ownership detection, configurable timeout, and JSON output option |
| `generate_migration_blueprint` | BFS context compilation from an entry node into a 9-section Markdown dossier or structured JSON, with configurable depth and edge kind filters |

### Knowledge Graph (Advanced)

| Tool | Description |
|------|-------------|
| `ast_dependency_graph` | BFS graph traversal from an entry node with configurable direction (outgoing/incoming/both), compile-time edge filtering (Dependency+Imports+Contains), depth up to 12 hops, and JSON or text tree output |
| `compute_blast_radius` | Multi-hop impact estimation from a node or file: propagates through graph edges to rank affected symbols by reachability and edge-weight, returns a scored blast surface with per-node risk tier |
| `detect_design_patterns` | Structural pattern detection across the graph (Repository, Factory, Singleton, Observer, etc.) using node-type and edge-kind fingerprints, with confidence scores and file-level attribution |

### Index Maintenance

| Tool | Description |
|------|-------------|
| `incremental_indexing_gc` | Manual GC trigger with pre/post delta reporting for graph nodes, edges, tantivy docs, and lance vectors; optional vector compaction |
| `dedicated_antipattern_index` | Manage the antipattern namespace: `stats` (doc count + repo rules), `list` (browse), `search` (hybrid search with content preview), `clear` (purge namespace) |

### Database & Schema

| Tool | Description |
|------|-------------|
| `get_table_schema` | DDL, columns, FK relationships, and code references for a table |
| `trace_state_usage` | Trace readers/writers of global state (Session, ViewState, Application, Cache) |
| `trace_ui_event` | Trace a path from ASPX page + control ID to SQL |
| `trace_ui_action` | Trace a UI action to code-behind handler and call chain |
| `get_instrumentation_pack` | Generate a minimal instrumentation snippet for legacy .NET apps |

### Memory Bank

| Tool | Description |
|------|-------------|
| `update_memory_bank` | Create or update a named memory bank section |
| `list_memory_bank` | List all memory bank sections for a project |
| `read_memory_bank` | Read a specific section |
| `delete_memory_bank` | Delete a section |

### Repo Rules

| Tool | Description |
|------|-------------|
| `add_repo_rule` | Add a file-pattern-matched rule injected into chunk reads |
| `list_repo_rules` | List all rules for a project |
| `delete_repo_rule` | Delete a rule |

### Project Watching

| Tool | Description |
|------|-------------|
| `watch_project` | Enable directory watching for automatic re-index on changes |
| `unwatch_project` | Disable watching |

### Observability & Operations

| Tool | Description |
|------|-------------|
| `get_metrics` | Server-wide metrics snapshot: job latencies, queue depths, index drift, cardinality, repair outcomes, memory, checkpoints, confidence scoring, safety. JSON or human-readable output |
| `check_integrity` | Cross-store consistency check for a project (Tantivy, LanceDB, Graph, Docstore). Detects mismatches, optionally auto-repairs |
| `get_memory_budget` | Current memory budget status: usage, limits, per-subsystem breakdown, pressure state |
| `get_checkpoint_status` | Crash-recovery checkpoint status for jobs. Shows resumable jobs, phase, and progress |
| `evaluate_safety` | Safety policy evaluation for a proposed automated edit. Returns go/no-go with risk level, checks, and mitigations |
| `benchmark_retrieval` | NDCG@10, Recall@10, MRR benchmarking against known-relevant queries. Gates vector_search for production readiness |
| `get_extraction_confidence` | Score WebForms extraction confidence (event wiring, SQL trace, control binding) with signal-weighted breakdown |
| `generate_migration_plan` | Executable migration plan with dependency-ordered waves, seams, contract tests, adapters, and rollback playbooks |
| `autonomous_decision_gate` | Mandatory 8-gate verification pipeline for autonomous code changes. Runs extraction confidence, trace certainty, safety policy, retrieval quality, blast radius, anti-pattern, runtime evidence, and evidence sufficiency gates. Returns allow/deny/abstain verdict with machine-readable failed gate IDs and required follow-ups |
| `generate_migration_scaffold` | Generate Blazor/React/Angular component skeletons from a legacy WebForms file's graph context, with repository interfaces, DTOs, and test scaffolds |
| `generate_instrumentation_code` | Produce injectable C# and VB.NET HttpModule instrumentation code for runtime tracing, with web.config entries |
| `reconcile_runtime_evidence` | Compare static graph paths (SQL calls, state access, dependencies, postbacks) against a runtime evidence batch and produce confirmed/contradicted/inconclusive report |
| `suggest_state_migration` | Analyze state usage (Session, ViewState, Application, Cache, Cookie, QueryString) and produce per-key migration recommendations with ViewState lifecycle report |
| `generate_characterization_tests` | Generate NUnit/xUnit/MSTest characterization test classes covering event handlers, data flows, state transitions, navigation, and API contracts |
| `generate_strangler_fig_config` | Generate complete strangler fig migration infrastructure: YARP reverse proxy, feature flags, routing middleware with rollout and sticky sessions, health check, Program.cs with Polly resilience |

### Migration Workflow (Phase 31)

| Tool | Description |
|------|-------------|
| `map_validation_controls` | Map WebForms validators to DataAnnotation/FluentValidation equivalents with validation group analysis |
| `map_auth_config` | Analyze web.config auth mode (Forms/Windows/None), location rules, membership/role providers, and code-level auth patterns |
| `map_page_lifecycle` | Map Page lifecycle events to modern framework equivalents, detect IsPostBack branching, identify implicit behaviors |
| `analyze_viewstate_deps` | Extract explicit/implicit ViewState usage, recommend modern state types per field |
| `map_ajax_regions` | Inventory UpdatePanel/ScriptManager regions, map triggers, suggest component decomposition |
| `trace_data_flow` | Trace data flow from entry point to sinks (SQL, state, response) with cross-file dependency detection |
| `get_migration_dossier` | Build a comprehensive per-file migration dossier orchestrating all analysis sub-services |
| `check_migration_coverage` | Compare original page vs modern code to find migration gaps across 6 categories |
| `update_migration_status` | Track per-file migration status (not_started/in_progress/done/blocked) with blocking dependencies |
| `get_migration_progress` | Retrieve overall migration progress for a project |
| `suggest_migration_order` | Topological sort via Kahn's algorithm producing parallelizable waves with cycle detection |
| `analyze_full_project_migration` | **One-call full project analysis**: reads all markup + code-behind files, orchestrates migration order, state, auth, data access, and per-file dossiers into a single comprehensive report with cross-cutting summary |

### Utilities

| Tool | Description |
|------|-------------|
| `export_capture_pack` | Export a comprehensive zip for offline agentic usage |
| `ingest_instrumentation_logs` | Ingest runtime logs from a legacy .NET app |
| `get_job_status` | Get status and progress of a background job |
| `list_jobs` | List all jobs, optionally filtered by project |
| `cancel_job` | Cancel a running background job |

---

## Project Types

Engram uses the `project_type` parameter to select appropriate file extensions and parsers.

| Type String | Languages | Extra Extensions |
|-------------|-----------|-----------------|
| `rust` (default for Rust) | Rust | `rs, toml` |
| `python` | Python | `py, ipynb` |
| `typescript` | TypeScript/JavaScript | `ts, tsx, js, jsx` |
| `java` | Java | `java, xml, gradle` |
| `go` | Go | `go, mod` |
| `dotnetwebformscs` | C# ASP.NET WebForms | `cs, aspx, ascx, master, config, xml, sln, csproj, sql, rdlc` |
| `dotnetwebformsvb` | VB.NET ASP.NET WebForms | `vb, aspx, ascx, master, config, xml, sln, vbproj, sql, rdlc` |

Any unrecognized type falls back to a broad default set covering most common source and config file extensions.

---

## How to Prompt Your LLM to Use Engram

Once the MCP server is connected, use natural language. Example prompts:

**Initial indexing:**
```
Index /home/user/myapp as project "myapp" (project_type: dotnetwebformscs).
Wait for indexing to complete.
```

**Codebase orientation:**
```
Give me a codebase overview for project "myapp". Then search for "authentication"
and show me the top 5 results with content.
```

**Impact analysis:**
```
I'm about to refactor UserRepository.cs. Run impact_analysis to find everything
that could break, then traverse the graph 3 hops from UserRepository to show
its full dependency surface.
```

**Temporal coupling:**
```
Analyze temporal couplings for OrderService.cs in project "myapp".
Inject any discovered coupling edges into the graph.
Which files should I consider changing together with it?
```

**Style mimicry:**
```
Before I write new code for AuthService.cs, analyze its coding style
from git history and give me the style guide to follow.
```

**Anti-pattern guard:**
```
Here is a code snippet I'm about to commit. Run immune_check against
project "myapp" to see if it matches any previously reverted code:

[paste code here]
```

**Database tracing (WebForms):**
```
Trace the full call path from the btnSave_Click event on Default.aspx
through to any SQL queries it touches.
```

**Memory bank (persistent notes):**
```
Update the memory bank for project "myapp", section "architecture",
with: "All database access must go through the Repository layer.
Direct DataContext calls in controllers are not allowed."
```

**REM dreaming:**
```
Run a dream cycle on project "myapp" and summarize any insights generated.
Then query the graph for new Insight nodes.
```

**Git history search:**
```
Search git history in project "myapp" for commits mentioning "fix deadlock"
between 2024-01-01 and 2024-12-31.
```

**Repo rules:**
```
Add a repo rule to project "myapp": any file matching "*Service.cs" must
not directly instantiate DbContext — use the injected IUnitOfWork instead.
```

---

## Security Model

- **Path confinement**: All file access is validated against `allowed_roots`. Any path that escapes the allowed roots is rejected, including symlinks and path traversal sequences.
- **Project ID validation**: Project IDs are restricted to `[a-zA-Z0-9_-]` to prevent injection.
- **Single-writer semantics**: Per-project mutexes serialize concurrent index updates.
- **No shell execution**: The server never spawns shell commands; all git operations use libgit2 directly.

---

## Docs

- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — system design and component interactions
- [`docs/DATA_MODEL.md`](docs/DATA_MODEL.md) — storage layer schemas
- [`docs/TOOL_CONTRACT.md`](docs/TOOL_CONTRACT.md) — full tool parameter reference
- [`docs/COGNITIVE_PIPELINES.md`](docs/COGNITIVE_PIPELINES.md) — dreaming, immune system, style analysis
- [`docs/GENERATION_MODEL.md`](docs/GENERATION_MODEL.md) — append-only generation semantics
- [`docs/DEVELOPER_SPEC.md`](docs/DEVELOPER_SPEC.md) — contributing and internals
- [`docs/ROADMAP.md`](docs/ROADMAP.md) — planned features

---

## License

This project is licensed under the **Business Source License 1.1 (BUSL-1.1)**.

- Non-commercial use (personal, educational, internal evaluation) is allowed.
- Production/commercial use is restricted until the Change Date.
- On the Change Date, this code transitions to Apache-2.0.

See [`LICENSE`](LICENSE) for the full terms, including the Additional Use Grant and Change Date.
