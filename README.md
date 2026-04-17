# Engram MCP

**A graph-aware safety net for AI coding agents.**

Engram is a production-grade [Model Context Protocol](https://modelcontextprotocol.io) server, written in Rust, that gives AI agents deep structural understanding of real codebases — not just what the code says, but how its parts relate, how they have changed together over time, and what has broken before.

Connect Engram once and any MCP-capable agent (Claude Desktop, Claude Code, Codex, your own SDK client) gains an evidence-backed picture of the project: a typed knowledge graph, full-text and semantic search, git temporal intelligence, an immune system built from reverted commits, per-file coding conventions, and a ten-gate pre-commit review that blocks destructive edits with receipts.

- **Deterministic by default.** Core tools — graph traversal, search, temporal coupling, pre-commit review, immune checks, secret scanning — do not call an LLM. You can run Engram entirely offline.
- **Language-aware.** Tree-sitter parsers for Rust, Python, Go, Java, C, C++, C#, VB.NET, TypeScript, JavaScript, plus first-class handling for ASP.NET WebForms (ASPX / ASCX / Master) and Classic ASP.
- **Per-project, per-agent.** Each project lives in its own index; each query is scoped by `project_id`. No cross-project leakage.
- **Append-only and crash-safe.** Generation-based indexing, Blake3 fingerprints, Redb-backed durable checkpoints, integrity sentinels with auto-repair.

---

## Why Engram exists

LLM coding agents without codebase context are dangerous: they hallucinate APIs, miss cross-file dependencies, re-introduce patterns that were reverted last quarter, and silently break shared state.

Reading raw files into the prompt doesn't fix it. A 500k-line project doesn't fit. Even if it did, the agent can't see git history, can't see blast radius, can't see which files always change together, can't see the `DeleteAllOnSubmit` that was reverted three months ago because it bypassed multi-tenant scoping.

Engram solves this by **building and persisting** the context an agent needs:

| Problem | How Engram solves it |
|---|---|
| Agent invents method signatures | Typed graph of every function, class, and call edge — queried by name or FQN |
| Agent touches a high-blast-radius file | `compute_blast_radius` returns a 1-10 score with incoming / outgoing / downstream counts |
| Agent re-introduces a reverted pattern | Immune system indexes every reverted diff as an anti-pattern; `immune_check` scores new code against it |
| Agent changes a file but not its coupled partner | Temporal coupling tracks files that change together in git history |
| Agent ships a regression | `pre_commit_review` runs 10 deterministic gates over the staged diff before commit |
| Agent gets lost in a 5000-file legacy codebase | `get_codebase_overview`, `generate_migration_blueprint`, `analyze_full_project_migration` |

---

## Flagship feature — `pre_commit_review`

Ten deterministic gates run over a unified diff (raw text, `staged`, `unstaged`, `head`, or a `.patch` path) and emit severity-ranked findings with concrete evidence and fix suggestions. No LLM calls. Typical run: < 2 seconds.

| Gate | What it catches |
|---|---|
| `immune` | Modifications to files flagged by prior reverts — CRITICAL when paired with destructive patterns |
| `blast_radius` | High-impact files where a change ripples outward |
| `style` | Per-file convention violations (naming, Using blocks, Try/Catch, SafeRedirect, etc.) detected from the file's existing code |
| `temporal` | Strongly-coupled files that change together in history but aren't in the diff |
| `state` | Session / ViewState / Application / Cache keys touched by the diff, with their other readers and writers |
| `audit` | Database mutations missing the project's established audit-log convention |
| `antipattern` | Added code that resembles indexed anti-patterns (reverted diffs) via hybrid search |
| `new_file` | New files that break the folder's extension / prefix conventions; ASPX pages without codebehind |
| `test_coverage` | Non-test files changed without their coupled test file |
| `secret_leakage` | Hardcoded AWS / GitHub / OpenAI / Anthropic / JWT / connection-string secrets — redacted in output |

Every finding includes: a stable ID (for CI run-to-run tracking), specific line numbers, diff-context snippets, evidence lines (graph data, revert hashes, coupling weights, convention statistics), an actionable fix suggestion, and a `Next` tool recommendation for agents that want to dig deeper.

Output is either rendered markdown with a single 🟢 / 🟡 / 🔴 verdict badge, or a stable JSON payload (`output_json: true`) for CI pipelines.

**Real output on a dangerous diff:**

```markdown
# Pre-Commit Review — 🔴 RED — do not merge as-is

**Findings**: 27 total (2 critical · 2 warning · 22 info · 1 style)
**Files analysed**: 1 | **Gates run**: 10/10 | **Time**: 1975ms

## 🔴 CRITICAL (2)

### 7 gates flagged this file
**File**: `Site/App_Code/fbinstplan/code/fiberjobb.vb`
**Gate**: `corroboration`
**Evidence**: gates = antipattern, audit, blast_radius, immune, style, temporal, test_coverage
**Fix**: Investigate findings on this file first — agreement across gates is a strong
signal the change deserves extra scrutiny.

### Destructive code on immune-flagged file
**File**: `Site/App_Code/fbinstplan/code/fiberjobb.vb`
**Gate**: `immune`
**Evidence**:
- immune_rule_ids = immune_f7766bb1…
- destructive_patterns = DeleteAllOnSubmit
- revert_hashes = f7766bb1
**Fix**: Run immune_check before committing. The immune flag exists specifically to
prevent this pattern — either prove the operation is scoped (multitenant WHERE,
transaction, explicit test) or rethink the change.
**Next**: `immune_check(project_id="…", file_path="Site/App_Code/fbinstplan/code/fiberjobb.vb")`

## 🟡 WARNING (2)

### High-blast-radius file modified
**File**: `Site/App_Code/fbinstplan/code/fiberjobb.vb`
**Evidence**: migration_risk = 7/10 (High), total_incoming = 1043, total_downstream = 1960

### Destructive patterns detected in added code
**Evidence**: destructive_patterns = DeleteAllOnSubmit
```

---

## What Engram builds from your code

Indexing a project produces five layered data stores, all persistent on disk:

```
┌─────────────────────────────────────────────────────────┐
│  Full-text index (Tantivy)                              │
│    camelCase / snake_case tokenisation · namespace-aware│
├─────────────────────────────────────────────────────────┤
│  Vector index (LanceDB)                                 │
│    local trigram · Ollama · OpenAI — RRF + MMR fusion   │
├─────────────────────────────────────────────────────────┤
│  Knowledge graph (Redb)                                 │
│    ~40 edge kinds · O(degree) adjacency · PageRank      │
├─────────────────────────────────────────────────────────┤
│  Git intelligence (libgit2)                             │
│    commit graph · revert detection · temporal coupling  │
├─────────────────────────────────────────────────────────┤
│  Registry (Redb)                                        │
│    projects · jobs · memory bank · repo rules · meta    │
└─────────────────────────────────────────────────────────┘
```

The graph tracks **code structure** (`Calls`, `Contains`, `Imports`, `Dependency`), **data access** (`QueriesTable`, `HasColumn`, `ForeignKey`, `CallsStoredProcedure`, `StoredProcReadsTable`), **state flow** (`ReadsState`, `WritesState`, `StateAffinity`), **UI wiring** (`ContainsUi`, `DataBinding`, `TriggersPostback`, `ManipulatesDom`, `FillsRegion`), **web surface** (`ExposesWebService`, `ExposesHttpHandler`, `ExposesWcfService`), **change coupling** (`TemporalCoupling`, `CoOccurrence`), **reverts** (`AntiPattern`), and **runtime evidence** (`ObservedRuntimeControl`, `ObservedRuntimeSql`) — among others.

---

## Capabilities at a glance

### Search
Hybrid FTS + vector with Reciprocal Rank Fusion, Maximal Marginal Relevance diversity, namespace filtering, path and language filters, and content preview. Standalone vector search with 3x oversampling. Multi-language structured stacktrace parser. Graph-aware search that expands hits along configurable edge kinds.

### Knowledge graph
Query nodes by type / name / file. Find incoming or outgoing edges. Multi-hop BFS traversal. Impact analysis. Compute blast radius with seam-candidate identification. Detect structural design patterns (Repository, Factory, Singleton, Observer…).

### Git & temporal intelligence
Index commit history. Detect temporal coupling. Harvest reverted commits into the anti-pattern index. Search commit messages and diffs. Track repeated-pattern reverts as `immune_*` repo rules tied to the reverting commit hash.

### Code review (`pre_commit_review`)
Ten deterministic gates over a unified diff with severity-ranked output, stable finding IDs, cross-gate corroboration, auto-tuned temporal thresholds, secret redaction, and a single verdict badge. Markdown or JSON.

### Modernization engine (ASP.NET WebForms / Classic ASP)
End-to-end migration tooling: control mapping catalog (WebForms → Blazor / React / Angular), scaffold generation with real business logic pulled from graph edges, database strategy advisor, state migration recommendations, characterization test generation, strangler-fig infrastructure (YARP + feature flags + Polly), runtime instrumentation code, topologically-ordered migration waves, and `analyze_full_project_migration` — a single call that reads the whole project and returns a comprehensive report with cross-cutting summary.

### Safety & autonomous decisions
Autonomous Decision Protocol: 8-gate mandatory pipeline for automated edits (extraction confidence, trace certainty, safety policy, retrieval quality, blast radius, anti-pattern clearance, runtime evidence, evidence sufficiency). Three verdicts: `allow` / `deny` / `abstain`. Progressive rollout — `shadow` → `advisory` → `guarded` → `autonomous` — with an emergency kill-switch. Immutable JSON audit reports.

### Memory bank & repo rules
Per-project persistent notes the agent can read and write across sessions. File-pattern-matched rules automatically injected into chunk retrieval (e.g., "files matching `*Repository.cs` must use `IUnitOfWork`").

### Observability & integrity
Lock-free metrics (job latency, queue depth, index drift, memory pressure, checkpoint recovery, extraction confidence, safety decisions). Cross-store integrity sentinels with 5% tolerance and auto-repair. Crash-safe job orchestration with resume-from-failure. Retrieval quality benchmarking (NDCG@10, Recall@10, MRR) with configurable pass/fail thresholds.

### Cognitive features (LLM-optional)
REM Dreaming — clusters co-occurrence patterns into Insight nodes. Style mimicry — per-file coding conventions extracted from the file's current state. Business logic comprehension — local-Ollama method-level analysis with deterministic fallback, validation gate, and hallucination detection.

---

## Languages & ecosystems

| Language / stack | Parser | Extra capabilities |
|---|---|---|
| Rust, Python, Go, Java, C, C++ | Tree-sitter | AST extraction, call graphs, imports |
| TypeScript, JavaScript (+ JSX / TSX) | Tree-sitter | Framework signal detection (React / Vue / Angular / RxJS / Node / Express / Jest), jQuery inventory, cross-layer AJAX tracing, transpiled-TS fingerprinting |
| C# | Tree-sitter | LINQ / async / records / pattern matching detection, ASP.NET WebForms codebehind tracing |
| VB.NET | Tree-sitter | Handles clauses, WithEvents, On Error resolution, Module vs Class style, Optional-context-injection convention, SafeRedirect pattern, ReDim Preserve |
| ASP.NET WebForms (ASPX, ASCX, Master) | Custom | Control ID extraction, event wiring, master-page FillsRegion, UpdatePanel regions, ViewState lifecycle, deep layout extraction |
| Classic ASP (.asp) | Custom | COM objects, ADO connection/recordset, Session/Application/Request/Response, SSI includes, Server.Transfer |
| SQL (T-SQL) | Custom | CREATE TABLE/VIEW/PROCEDURE parsing, column / PK / FK / CHECK extraction, stored-procedure call chains, trigger detection |
| Google Maps / Esri ArcGIS JS | Custom | 80+ widget classes, migration complexity assessment |
| SSRS (.rdlc / .rdl), Crystal Reports | Custom | Parameter, dataset, subreport, table-reference extraction |

Unknown extensions fall through to a generic text chunker + full-text indexing path.

---

## Quick start

### Prerequisites
- Rust 1.81+ (edition 2024)
- On Linux/macOS: `libgit2` and `cmake` development packages
- Optional: [Ollama](https://ollama.ai) for local LLM / embedding backends

### Build

```bash
git clone https://github.com/your-org/engram-mcp-v2
cd engram-mcp-v2
cargo build --release
```

Binary lands at `target/release/engram_server`.

### Configure

Minimal `engram_mcp.yaml`:

```yaml
allowed_roots:
  - /home/user/projects
data_dir: /home/user/.engram-data

embedding_backend: local       # "local" | "ollama" | "openai"
```

Point the server at it via environment variable:

```bash
export ENGRAM_CONFIG_PATH=/path/to/engram_mcp.yaml
./target/release/engram_server
```

The server runs over **STDIO**. All stdout is reserved for MCP protocol messages; logs go to stderr. Enable debug logs with `RUST_LOG=debug`.

---

## MCP client setup

### Claude Desktop

```json
{
  "mcpServers": {
    "engram": {
      "command": "/path/to/engram_server",
      "env": { "ENGRAM_CONFIG_PATH": "/path/to/engram_mcp.yaml" }
    }
  }
}
```

### Claude Code

```json
{
  "mcpServers": {
    "engram": {
      "type": "stdio",
      "command": "/path/to/engram_server",
      "env": { "ENGRAM_CONFIG_PATH": "/path/to/engram_mcp.yaml" }
    }
  }
}
```

### First session

Paste into your agent:

```
Index /home/user/myapp as project "myapp" (project_type: dotnetwebformscs).
Wait for indexing to complete. Then give me a codebase overview.
```

Then, after any set of staged changes:

```
Run pre_commit_review on the staged diff for project "myapp".
```

---

## Tool catalog

**~100 MCP tools** organised by concern. Full parameter reference in [`docs/TOOL_CONTRACT.md`](docs/TOOL_CONTRACT.md).

<details>
<summary><b>Project lifecycle</b> — index, update, list, info, health, repair, delete, watch</summary>

| Tool | Purpose |
|---|---|
| `index_project` | Initial indexing of a local directory |
| `update_project` | Incremental re-index of changed files |
| `list_projects` · `project_info` · `project_health` | Inspection |
| `repair_project` | Targeted index repair (`full` / `graph_only` / `tantivy_only` / `vector_only`) |
| `delete_project` | Delete a project and its stored data |
| `watch_project` · `unwatch_project` | Automatic re-index on filesystem changes |
</details>

<details>
<summary><b>Search</b> — hybrid, vector, graph, symbol, error-stack</summary>

| Tool | Purpose |
|---|---|
| `search_memory` | Hybrid FTS + vector with RRF, MMR, filters |
| `vector_search` | Pure semantic vector search with oversampling + MMR |
| `get_chunk` | Fetch content for a chunk ID |
| `graph_search` | Hybrid text + symbol match with edge-expansion |
| `find_symbol_references` | Incoming / outgoing edges for a symbol across all edge kinds |
| `get_codebase_overview` | Languages, symbols, edge-kind distribution, PageRank, DB tables, state keys, temporal couplings, dead code |
| `analyze_error_stack` | Multi-language structured stacktrace parser with frame-boosted search |
</details>

<details>
<summary><b>Knowledge graph</b> — query, traverse, impact, blast radius, patterns</summary>

| Tool | Purpose |
|---|---|
| `query_graph_nodes` · `find_references` · `traverse_graph` | Core graph ops |
| `impact_analysis` | Estimate what breaks if a file or symbol changes |
| `ast_dependency_graph` | BFS with edge-kind filters, up to 12 hops, tree or JSON output |
| `compute_blast_radius` | 1-10 risk score with seam candidates and per-node risk tier |
| `detect_design_patterns` | Structural pattern detection (Repository, Factory, Singleton…) |
</details>

<details>
<summary><b>Git & temporal intelligence</b></summary>

| Tool | Purpose |
|---|---|
| `index_git_history` · `ingest_zip_history` | Commit history indexing |
| `search_history` | Search commit messages / diffs with structured metadata |
| `analyze_temporal_couplings` | Files that frequently change together |
| `analyze_reverts` | Detect reverts, generate anti-pattern rules, index reverted diffs |
</details>

<details>
<summary><b>Review & safety</b></summary>

| Tool | Purpose |
|---|---|
| `pre_commit_review` | Ten-gate deterministic diff review with verdict badge and CI-stable finding IDs |
| `immune_check` | Score code against the anti-pattern index (hybrid FTS + vector, configurable thresholds) |
| `anti_pattern_guard` | Pattern matching with revert-commit extraction and remediation guidance |
| `autonomous_decision_gate` | 8-gate mandatory pipeline for automated edits; returns `allow` / `deny` / `abstain` |
| `evaluate_safety` | Safety policy check for a proposed edit |
| `check_edit_safety` | Per-method green / yellow / red verdict |
</details>

<details>
<summary><b>Database & schema</b></summary>

| Tool | Purpose |
|---|---|
| `get_table_schema` | DDL, columns, FK relationships, code references |
| `trace_state_usage` | Readers / writers of Session / ViewState / Application / Cache |
| `trace_ui_event` · `trace_ui_action` | ASPX page → control → code-behind → SQL tracing |
| `get_instrumentation_pack` · `generate_instrumentation_code` | Runtime tracing code generation |
| `get_sp_details` · `list_triggers` | Stored procedure deep analysis, trigger detection |
</details>

<details>
<summary><b>Migration (ASP.NET WebForms → modern)</b></summary>

| Tool | Purpose |
|---|---|
| `analyze_full_project_migration` | One-call full-project analysis producing a comprehensive report |
| `generate_migration_blueprint` | BFS context compiled into a 9-section migration dossier |
| `generate_migration_plan` | Dependency-ordered waves with seams, contract tests, adapters, rollback playbooks |
| `generate_migration_scaffold` | Blazor / React / Angular component generation with real business logic |
| `generate_strangler_fig_config` | YARP reverse proxy + feature flags + Polly resilience |
| `generate_characterization_tests` | NUnit / xUnit / MSTest characterization tests |
| `suggest_state_migration` · `suggest_migration_order` | Per-key state advice; Kahn-sorted migration waves |
| `map_validation_controls` · `map_auth_config` · `map_page_lifecycle` · `map_ajax_regions` | Per-concern migration mapping |
| `analyze_viewstate_deps` · `trace_data_flow` · `get_migration_dossier` · `check_migration_coverage` | Deep per-file analysis |
| `update_migration_status` · `get_migration_progress` | Standalone Redb-backed migration tracker |
</details>

<details>
<summary><b>Cognitive & reasoning</b></summary>

| Tool | Purpose |
|---|---|
| `dream_project` · `trigger_rem_cycle` | Cluster co-occurrence into Insight nodes |
| `analyze_file_coding_style` | Per-file convention extraction from git history |
| `analyze_business_logic` · `query_business_logic` | LLM-powered method-level analysis with validation gate |
| `suggest_migration_boundaries` | LLM + deterministic boundary suggestion with cross-cluster dependency analysis |
</details>

<details>
<summary><b>Access layer (per-method)</b></summary>

| Tool | Purpose |
|---|---|
| `get_method_info` · `get_full_method_body` · `get_method_edit_context` | Precise method context retrieval |
| `get_page_context` · `prepare_implementation_context` | ASPX page / implementation context |
| `validate_generated_code` · `validate_sql_fragment` | Pre-commit code and SQL validation |
| `find_tests_for_method` · `find_dead_methods` · `check_edit_safety` | Discovery and safety checks |
</details>

<details>
<summary><b>Memory bank & repo rules</b></summary>

| Tool | Purpose |
|---|---|
| `update_memory_bank` · `list_memory_bank` · `read_memory_bank` · `delete_memory_bank` | Persistent agent notes |
| `add_repo_rule` · `list_repo_rules` · `delete_repo_rule` | File-pattern-matched rules injected into chunk reads |
</details>

<details>
<summary><b>Observability & operations</b></summary>

| Tool | Purpose |
|---|---|
| `get_metrics` | Server-wide metrics (job latency, queue depth, drift, memory, safety…) |
| `check_integrity` | Cross-store consistency check with optional auto-repair |
| `get_memory_budget` · `get_checkpoint_status` | Resource and recovery status |
| `benchmark_retrieval` | NDCG@10 / Recall@10 / MRR retrieval quality gate |
| `get_extraction_confidence` | Signal-weighted extraction confidence scoring |
| `incremental_indexing_gc` · `dedicated_antipattern_index` | Index hygiene |
| `get_job_status` · `list_jobs` · `cancel_job` | Background job control |
| `export_capture_pack` | Export a zip for offline agentic usage |
</details>

---

## Architecture

```
engram_core/     Core types, config, security boundary, Redb registry
engram_index/    Tantivy FTS + LanceDB vectors + DocStore + hybrid search
engram_graph/    Redb-backed graph store + BFS / PageRank algorithms
engram_git/      libgit2 walker, temporal coupling, revert detection
engram_ml/       Dreaming engine, embedders, style mimicry, immune system
engram_server/   MCP server (rmcp), tool handlers, background actors
```

**Storage layers**

| Layer | Technology | Purpose |
|---|---|---|
| Registry | Redb | Projects, jobs, memory bank, repo rules, metadata |
| Graph | Redb | Nodes, edges, composite-key adjacency lists, bincode |
| Full-text index | Tantivy | Namespaced search (code, history, antipatterns) |
| Vector index | LanceDB | Semantic embeddings, optional |
| DocStore | Redb | File fingerprints, chunk-to-file mapping |
| Checkpoints | Redb | Crash-safe job stage tracking |
| Migration progress | Redb | Per-file migration status |

**Background actors**

- **Dreamer** — co-occurrence clustering → Insight generation
- **Immune actor** — git revert harvesting → anti-pattern indexing
- **Watcher** — directory monitoring → incremental re-index
- **GC scheduler** — orphaned job cleanup
- **Integrity sentinel** — cross-store consistency checks with configurable auto-repair
- **Memory budget watcher** — per-subsystem soft/hard limits with CAS allocation and backpressure

---

## Configuration reference

Full YAML:

```yaml
# Required
allowed_roots:
  - /home/user/projects
data_dir: /home/user/.engram-data

# Embedding backend
embedding_backend: local              # "local" | "ollama" | "openai"
embedding_model: nomic-embed-text
ollama_url: http://localhost:11434
openai_api_key: sk-...
openai_api_base: https://api.openai.com/v1

# LLM backend for cognitive features (optional — everything except `dream`,
# `analyze_business_logic`, `suggest_migration_boundaries` works without it)
llm_backend: none                     # "none" | "ollama" | "openai" | "openrouter"
llm_provider: openrouter
llm_model: llama3.2
llm_ollama_url: http://localhost:11434
llm_openai_api_key: sk-...
llm_http_referer: https://your-app.example

# Limits
max_project_files: 100000
max_project_bytes: 5368709120         # 5 GiB
max_chunks_per_file: 2000
max_concurrent_jobs: 2
max_commits_per_watch: 50

# Safety & ADP
safety_policy_enabled: true
safety_min_confidence: 0.7
safety_min_coverage: 0.6
adp_enabled: true
adp_min_extraction_confidence: 0.5
adp_max_blast_radius: 6
adp_rollout_phase: shadow             # shadow | advisory | guarded | autonomous
adp_kill_switch: false                # emergency kill-switch → forces every verdict to deny
```

### Project types

| Type string | Languages | Key extra extensions |
|---|---|---|
| `rust` | Rust | `rs, toml` |
| `python` | Python | `py, ipynb` |
| `typescript` | TS / JS | `ts, tsx, js, jsx` |
| `java` | Java | `java, xml, gradle` |
| `go` | Go | `go, mod` |
| `dotnetwebformscs` | C# ASP.NET WebForms | `cs, aspx, ascx, master, config, sln, csproj, sql, rdlc` |
| `dotnetwebformsvb` | VB.NET ASP.NET WebForms | `vb, aspx, ascx, master, config, sln, vbproj, sql, rdlc` |

Any unknown type falls back to a broad default covering common source + config extensions.

---

## Example agent prompts

**Orientation**
```
Give me a codebase overview for project "myapp". Then run pre_commit_review on
the staged diff.
```

**Safe refactor**
```
Before I refactor UserRepository.cs in project "myapp":
1. Run impact_analysis for this file
2. Run compute_blast_radius
3. Find all temporal couplings so I know which files to change together
4. Show me the top 5 reverted commits that touched this file
```

**Pre-commit review**
```
Run pre_commit_review on the unstaged diff for project "myapp" with
output_json: true, then summarise the CRITICAL and WARNING findings.
```

**Legacy modernization**
```
Run analyze_full_project_migration on project "myapp-legacy".
Share the cross-cutting summary and the top 3 files by migration risk.
```

**Anti-pattern guard**
```
Here's code I'm about to commit. Run immune_check on it against project "myapp"
with file_path="Site/App_Code/dal/orders.vb":

[paste code]
```

---

## Security model

- **Path confinement.** All file access is validated against `allowed_roots`. Paths that escape via symlinks, `..`, or canonicalisation tricks are rejected.
- **Project ID validation.** Restricted to `[A-Za-z0-9_-]{1,128}` at the handler boundary.
- **Single-writer semantics.** Per-project mutexes serialise concurrent index updates.
- **No shell execution.** Every git operation uses libgit2 directly; no subprocess is spawned.
- **Redacted secrets in output.** `pre_commit_review` fingerprints matched secrets — the raw value is never echoed back in findings.
- **Fail-closed on integrity.** The FTS watcher is fail-closed (no silent downgrades); integrity sentinels detect cross-store drift and can auto-repair.

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

This project is licensed under **PolyForm Noncommercial License 1.0.0** (`PolyForm-Noncommercial-1.0.0`).

- Noncommercial use — personal, hobby, learning, research — is allowed under PolyForm Noncommercial 1.0.0.
- Commercial, business, or for-profit use requires a separate written commercial license.
- A company using this code — internally, in product development, SaaS, services, or any revenue-generating work — must obtain a commercial license.

See [`LICENSE`](LICENSE) for the full terms and [`COMMERCIAL-LICENSE.md`](COMMERCIAL-LICENSE.md) for the commercial-use notice.
