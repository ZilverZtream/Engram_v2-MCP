# Engram MCP

A graph-aware codebase context server for AI coding agents. Engram is a
[Model Context Protocol](https://modelcontextprotocol.io) server, written in
Rust, that gives any MCP-capable agent (Claude Desktop, Claude Code, Codex,
your own SDK client) a persistent, evidence-backed picture of a real
codebase: a typed knowledge graph, full-text and semantic search, git
temporal intelligence, a revert-derived anti-pattern index, per-file coding
conventions, a ten-gate pre-commit review, a CLAUDE.md generator, a diff
narrator, and an Autonomous Decision Protocol for agents that want to edit
without asking every time.

- **Deterministic by default.** Core tools — graph traversal, search,
  temporal coupling, pre-commit review, immune checks, secret scanning,
  CLAUDE.md generation — never call an LLM. The server runs fully offline.
- **Language-aware.** Tree-sitter parsers for Rust, Python, Go, Java, C,
  C++, C#, VB.NET, TypeScript, and JavaScript; custom extractors for
  ASP.NET WebForms (ASPX / ASCX / Master), Classic ASP, T-SQL, SSRS,
  Crystal Reports, Google Maps, and Esri ArcGIS JS.
- **Per-project, per-agent.** Each project has its own index; every query
  is scoped by `project_id`. No cross-project leakage.
- **Append-only and crash-safe.** Generation-based indexing, Blake3
  fingerprints, Redb-backed durable checkpoints, integrity sentinels with
  auto-repair, and a multi-client dispatcher that lets several agents
  share one daemon without races.

---

## What Engram gives an agent

| Agent failure mode | Engram response |
|---|---|
| Invents method signatures | Typed graph of every function, class, and call edge, queryable by name or FQN |
| Touches a high-impact file blindly | `compute_blast_radius` returns a 1–10 score with incoming / outgoing / downstream counts |
| Re-introduces a reverted pattern | Immune system indexes every reverted diff; `immune_check` scores new code against it |
| Changes a file but not its coupled partner | Temporal coupling surfaces files that change together in git history |
| Ships a regression | `pre_commit_review` runs 11 deterministic gates over the staged diff, with secret redaction and CI-stable finding IDs |
| Adds a public endpoint that skips the admin check | `guard_parity` gate + `map_guards_and_settings` compare new code against the sibling guards and settings that gate the area |
| Implements 2 of the 17 places a concept lives | `plan_user_story` → `get_concept_footprint` → `find_similar_changes` map every touchpoint and the companion artifacts changes like this always include |
| Gets lost in a 5,000-file legacy codebase | `get_codebase_overview`, `generate_migration_blueprint`, `analyze_full_project_migration` |
| Writes project-context guidance from scratch every session | `produce_claude_md` generates a `CLAUDE.md` and `.claude/rules/*` from indexed signals |
| Can't explain the diff it just produced | `explain_change` narrates a diff as commit message, PR description, or changelog |
| Edits autonomously with no audit trail | Autonomous Decision Protocol — 8-gate verdict pipeline with progressive rollout and a kill-switch |

---

## Capabilities

Each capability is independently usable. Every tool is scoped by `project_id`
and every file path is validated against configured `allowed_roots`.

The sections below are ordered by how much they change an agent's behaviour
compared to running without Engram — the top ones are the reason Engram
exists; the bottom ones are operational surface.

### Pre-commit review (`pre_commit_review`)

Eleven deterministic gates run over a unified diff (raw text, `staged`,
`unstaged`, `head`, or a `.patch` path) and emit severity-ranked findings
with concrete evidence and fix suggestions. No LLM calls. Typical run: under
two seconds.

| Gate | What it catches |
|---|---|
| `immune` | Modifications to files flagged by prior reverts; CRITICAL when paired with destructive patterns |
| `blast_radius` | High-impact files where a change ripples outward |
| `style` | Per-file convention violations (naming, `Using` blocks, `Try/Catch`, `SafeRedirect`, etc.) derived from the file's current code |
| `temporal` | Strongly-coupled partner files missing from the diff |
| `state` | Session / ViewState / Application / Cache keys touched by the diff, with their other readers and writers |
| `audit` | Database mutations missing the project's established audit-log convention |
| `antipattern` | Added code that resembles indexed anti-patterns (reverted diffs) via hybrid search |
| `new_file` | New files that break the folder's extension / prefix conventions; ASPX pages without codebehind |
| `test_coverage` | Non-test files changed without their coupled test file |
| `secret_leakage` | Hardcoded AWS / GitHub / OpenAI / Anthropic / JWT / connection-string secrets — redacted in output |

Every finding carries a stable ID (for run-to-run tracking in CI), specific
line numbers, diff-context snippets, evidence lines (graph data, revert
hashes, coupling weights, convention statistics), an actionable fix
suggestion, and a `Next` tool recommendation. Output is either rendered
Markdown with a single 🟢 / 🟡 / 🔴 verdict badge, or a stable JSON payload
(`output_json: true`) for CI pipelines.

### Immune system (revert-derived anti-patterns)

Every reverted commit in the git history is treated as a lesson. The
immune actor harvests each revert, extracts the pattern that was undone,
and indexes it as an anti-pattern with the reverting commit hash attached
as evidence.

| Tool | What it does |
|---|---|
| `analyze_reverts` | Walks git history, detects reverts, generates `immune_*` repo rules |
| `immune_check` | Scores proposed code against the anti-pattern index (hybrid FTS + vector) and returns matched rules with revert hashes |
| `anti_pattern_guard` | Pattern matching with remediation guidance and the originating commit |
| `dedicated_antipattern_index` | Maintenance — keeps the anti-pattern index fresh |

This is how an agent stops re-introducing the `DeleteAllOnSubmit` that
was reverted three months ago: `immune_check` flags the proposed code
with the revert hash, and `pre_commit_review`'s `immune` gate blocks the
commit automatically.

### Autonomous Decision Protocol

Eight-gate mandatory pipeline for autonomous edits:

1. Extraction confidence — do we actually understand the method?
2. Trace certainty — are the call / state traces verified?
3. Safety policy — does the policy allow this class of edit?
4. Retrieval quality — did search return relevant context?
5. Blast radius — is the change contained?
6. Anti-pattern clearance — does it avoid indexed reverts?
7. Runtime evidence — does observed behaviour support the edit?
8. Evidence sufficiency — is there enough signal to commit?

Three verdicts: `allow` / `deny` / `abstain`. Progressive rollout —
`shadow` → `advisory` → `guarded` → `autonomous` — with an emergency
kill-switch that forces every verdict to `deny`. Every decision is
written to an immutable JSON audit trail with inputs, gate outputs, and
final verdict for offline replay.

This is the surface an agent framework drives when it wants to edit code
without a human in every loop.

### Knowledge graph (~40 edge kinds)

Typed graph across seven concern domains:

- **Code structure** — `Calls`, `Contains`, `Imports`, `Dependency`
- **Data access** — `QueriesTable`, `HasColumn`, `ForeignKey`,
  `CallsStoredProcedure`, `StoredProcReadsTable`
- **State flow** — `ReadsState`, `WritesState`, `StateAffinity`
- **UI wiring** — `ContainsUi`, `DataBinding`, `TriggersPostback`,
  `ManipulatesDom`, `FillsRegion`
- **Web surface** — `ExposesWebService`, `ExposesHttpHandler`,
  `ExposesWcfService`
- **Change coupling** — `TemporalCoupling`, `CoOccurrence`
- **Runtime evidence** — `ObservedRuntimeControl`, `ObservedRuntimeSql`

Query by type / name / file. BFS traversal up to 12 hops with edge-kind
filters. `compute_blast_radius` returns a 1–10 risk score with
seam-candidate identification; `impact_analysis` estimates what breaks
if a file or symbol changes; `detect_design_patterns` surfaces
Repository / Factory / Singleton / Observer structures. Runtime-evidence
edges let you cross-check static extraction against actual observed
behaviour.

### Legacy modernization engine

End-to-end migration tooling for ASP.NET WebForms and Classic ASP
codebases:

- Control mapping catalog (WebForms → Blazor / React / Angular)
- Scaffold generation with real business logic pulled from graph edges
  (not invented)
- Database-strategy advisor; per-key state-migration recommendations
- Characterization test generation (NUnit / xUnit / MSTest)
- Strangler-fig infrastructure (YARP + feature flags + Polly)
- Runtime instrumentation code generation
- Topologically-ordered migration waves via Kahn sort
- `analyze_full_project_migration` — a single call that produces a
  comprehensive per-file migration report with a cross-cutting summary,
  suitable for handing to a team or a migration agent

### Git temporal intelligence

- `index_git_history` — libgit2-backed commit-graph indexing
- `analyze_temporal_couplings` — files that frequently change together
- `search_history` — structured search over commit messages and diffs
- The immune system (above) is built on top of this layer

Temporal coupling is the quiet superpower: it surfaces files that
aren't related by any import or call edge but are consistently changed
in the same commit — the kind of hidden coupling that turns a one-file
edit into a four-file regression.

### Hybrid search

Full-text (Tantivy) + vector (LanceDB) with Reciprocal Rank Fusion,
Maximal Marginal Relevance diversity, namespace / path / language
filters, and content preview. Standalone vector search with 3×
oversampling. Multi-language structured stacktrace parser with
frame-boosted search. Graph-aware search that expands hits along
configurable edge kinds so you get the method PLUS its callers, callees,
or temporal-coupling partners in one call.

### Per-method access layer

Precise per-method context retrieval so an agent can touch one method
without re-reading an entire file:

| Tool | Purpose |
|---|---|
| `get_method_info` | Signature, surrounding class, edges |
| `get_full_method_body` | Full source |
| `get_method_edit_context` | Source + immediate call / caller neighbourhood |
| `get_page_context` · `prepare_implementation_context` | ASPX-aware context for WebForms edits |
| `validate_generated_code` · `validate_sql_fragment` | Pre-commit validation |
| `find_tests_for_method` · `find_dead_methods` · `check_edit_safety` | Discovery and safety checks |

### Cognitive features (LLM-optional)

Everything in the sections above runs without an LLM. These extras use
one when available:

- **REM Dreaming** — clusters co-occurrence patterns into `Insight`
  nodes. Runs on a schedule or on demand.
- **Style mimicry** — per-file coding conventions extracted from the
  file's current state (no LLM needed for the core extraction).
- **Business-logic comprehension** — local-Ollama method-level analysis
  with a deterministic fallback, validation gate, and hallucination
  detection.
- **Migration boundary suggestion** — LLM + deterministic boundary
  detection with cross-cluster dependency analysis.

### Project memory and diff artefacts

Nice-to-haves rather than core capabilities, grouped here so they
don't clutter the top:

- **`produce_claude_md`** — generate `CLAUDE.md` + `.claude/rules/*`
  from indexed signals. Rules bucketed into three confidence tiers
  (**Hard** — live reverts and security invariants; **Strong** —
  team-enforced conventions; **Observed** — pattern observations,
  advisory). Convention files use an `Applies to / Mandatory / Strong /
  Observed / Avoid` template with explicit sample-scoping. Merge modes:
  `splice`, `optimize`, `replace`. Optional `use_llm: true` polish pass
  is registry-cached so reruns against the same inputs cost zero tokens.
- **`explain_change`** — narrates a diff as commit message, PR
  description, or changelog entry. Deterministic pass with optional LLM
  polish; cached by diff hash.

### Memory bank and repo rules

Per-project persistent notes the agent reads and writes across sessions.
File-pattern rules automatically injected into chunk retrieval (for
example, "files matching `*Repository.cs` must use `IUnitOfWork`" attaches
to every read of those files).

### Multi-client daemon

Auto-daemon dispatcher so several MCP clients can share one Engram
process. First client spawns the daemon; subsequent clients attach as
proxies; the daemon lingers past the last disconnect so repeated short
sessions don't pay the warm-up cost every time. File-based lock,
race-free session counting, startup retry.

### Observability and integrity

Lock-free metrics (job latency, queue depth, index drift, memory
pressure, checkpoint recovery, extraction confidence, safety decisions).
Cross-store integrity sentinels with 5 % tolerance and auto-repair.
Crash-safe job orchestration with resume-from-failure. Retrieval quality
benchmarking (NDCG@10, Recall@10, MRR) with configurable pass / fail
thresholds.

---

## Languages and ecosystems

| Stack | Parser | Extra capabilities |
|---|---|---|
| Rust, Python, Go, Java, C, C++ | Tree-sitter | AST extraction, call graphs, imports |
| TypeScript, JavaScript (+ JSX / TSX) | Tree-sitter | Framework detection (React, Vue, Angular, RxJS, Node, Express, Jest), jQuery inventory, cross-layer AJAX tracing, transpiled-TS fingerprinting |
| C# | Tree-sitter | LINQ / async / records / pattern-matching detection, ASP.NET WebForms codebehind tracing |
| VB.NET | Tree-sitter | `Handles` clauses, `WithEvents`, `On Error` resolution, `Module` vs `Class`, `Optional` context-injection convention, `SafeRedirect` pattern, `ReDim Preserve` |
| ASP.NET WebForms (.aspx, .ascx, .master) | Custom | Control ID extraction, event wiring, master-page `FillsRegion`, `UpdatePanel` regions, ViewState lifecycle, deep layout extraction |
| Classic ASP (.asp) | Custom | COM objects, ADO connection / recordset, Session / Application / Request / Response, SSI includes, `Server.Transfer` |
| SQL (T-SQL) | Custom | `CREATE TABLE / VIEW / PROCEDURE` parsing, column / PK / FK / CHECK extraction, stored-procedure call chains, trigger detection |
| Google Maps / Esri ArcGIS JS | Custom | 80+ widget classes, migration complexity assessment |
| SSRS (.rdlc / .rdl), Crystal Reports | Custom | Parameter, dataset, subreport, table-reference extraction |

Unknown extensions fall through to a generic text chunker + full-text
indexing path.

---

## Install

### Prerequisites

- Rust 1.81+ (edition 2024)
- On Linux / macOS: `libgit2` and `cmake` development packages
- Optional: [Ollama](https://ollama.ai) for local LLM / embedding backends

### Build

```bash
git clone https://github.com/your-org/engram-mcp-v2
cd engram-mcp-v2
cargo build --release
```

The binary lands at `target/release/engram_server`.

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

The server runs over **stdio**. All stdout is reserved for MCP protocol
messages; logs go to stderr. Enable debug logs with `RUST_LOG=debug`.

---

## Connect

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

```
Index /home/user/myapp as project "myapp" (project_type: dotnetwebformscs).
Wait for indexing to complete. Then give me a codebase overview.
```

After any set of staged changes:

```
Run pre_commit_review on the staged diff for project "myapp".
```

---

## Tool reference

Engram exposes **roughly 110 MCP tools**, all scoped by `project_id`. The
groups below match the capability hierarchy above — revolutionary tools
first, operational surface last. Full parameter reference in
[`docs/TOOL_CONTRACT.md`](docs/TOOL_CONTRACT.md).

<details>
<summary><b>Project lifecycle</b> — index, update, list, info, health, repair, delete, watch</summary>

| Tool | Purpose |
|---|---|
| `index_project` | Initial indexing of a local directory |
| `update_project` | Incremental re-index of changed files |
| `list_projects` · `project_info` · `project_health` | Inspection |
| `repair_project` | Targeted repair (`full` / `graph_only` / `tantivy_only` / `vector_only`) |
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
| `graph_search` | Hybrid text + symbol match with edge expansion |
| `find_symbol_references` | Incoming / outgoing edges for a symbol across all edge kinds |
| `get_codebase_overview` | Languages, symbols, edge-kind distribution, PageRank, DB tables, state keys, temporal couplings, dead code |
| `analyze_error_stack` | Multi-language structured stacktrace parser with frame-boosted search |

</details>

<details>
<summary><b>Knowledge graph</b> — query, traverse, impact, blast radius, patterns</summary>

| Tool | Purpose |
|---|---|
| `query_graph_nodes` · `find_references` · `traverse_graph` | Core graph operations |
| `impact_analysis` | Estimate what breaks if a file or symbol changes |
| `ast_dependency_graph` | BFS with edge-kind filters, up to 12 hops; tree or JSON output |
| `compute_blast_radius` | 1–10 risk score with seam candidates and per-node risk tier |
| `detect_design_patterns` | Structural pattern detection (Repository, Factory, Singleton…) |

</details>

<details>
<summary><b>Git and temporal intelligence</b></summary>

| Tool | Purpose |
|---|---|
| `index_git_history` · `ingest_zip_history` | Commit history indexing |
| `search_history` | Search commit messages / diffs with structured metadata |
| `analyze_temporal_couplings` | Files that frequently change together |
| `analyze_reverts` | Detect reverts, generate anti-pattern rules, index reverted diffs |

</details>

<details>
<summary><b>Review and safety</b></summary>

| Tool | Purpose |
|---|---|
| `pre_commit_review` | Ten-gate deterministic diff review with verdict badge and CI-stable finding IDs |
| `immune_check` | Score code against the anti-pattern index (hybrid FTS + vector, configurable thresholds) |
| `anti_pattern_guard` | Pattern matching with revert-commit extraction and remediation guidance |
| `autonomous_decision_gate` | 8-gate mandatory pipeline for automated edits; returns `allow` / `deny` / `abstain` |
| `evaluate_safety` | Safety-policy check for a proposed edit |
| `check_edit_safety` | Per-method green / yellow / red verdict |

</details>

<details>
<summary><b>Project-memory generation</b></summary>

| Tool | Purpose |
|---|---|
| `produce_claude_md` | Generate `CLAUDE.md` + `.claude/rules/*` with three-tier confidence bucketing and structured convention files |
| `explain_change` | Turn a diff into a commit message, PR description, or changelog entry |

</details>

<details>
<summary><b>Database and schema</b></summary>

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
| `analyze_full_project_migration` | Single call that produces a comprehensive per-file migration report |
| `generate_migration_blueprint` | BFS context compiled into a 9-section migration dossier |
| `generate_migration_plan` | Dependency-ordered waves with seams, contract tests, adapters, rollback playbooks |
| `generate_migration_scaffold` | Blazor / React / Angular component generation with real business logic |
| `generate_strangler_fig_config` | YARP reverse proxy + feature flags + Polly resilience |
| `generate_characterization_tests` | NUnit / xUnit / MSTest characterization tests |
| `suggest_state_migration` · `suggest_migration_order` | Per-key state advice; Kahn-sorted migration waves |
| `map_validation_controls` · `map_auth_config` · `map_page_lifecycle` · `map_ajax_regions` | Per-concern migration mapping |
| `analyze_viewstate_deps` · `trace_data_flow` · `get_migration_dossier` · `check_migration_coverage` | Deep per-file analysis |
| `update_migration_status` · `get_migration_progress` | Redb-backed migration tracker |

</details>

<details>
<summary><b>Cognitive and reasoning</b></summary>

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
| `get_method_info` · `get_full_method_body` · `get_method_edit_context` | Precise method-context retrieval |
| `get_page_context` · `prepare_implementation_context` | ASPX page / implementation context |
| `validate_generated_code` · `validate_sql_fragment` | Pre-commit code and SQL validation |
| `find_tests_for_method` · `find_dead_methods` · `check_edit_safety` | Discovery and safety checks |

</details>

<details>
<summary><b>Memory bank and repo rules</b></summary>

| Tool | Purpose |
|---|---|
| `update_memory_bank` · `list_memory_bank` · `read_memory_bank` · `delete_memory_bank` | Persistent agent notes |
| `add_repo_rule` · `list_repo_rules` · `delete_repo_rule` | File-pattern rules injected into chunk reads |

</details>

<details>
<summary><b>Observability and operations</b></summary>

| Tool | Purpose |
|---|---|
| `get_metrics` | Server-wide metrics (job latency, queue depth, drift, memory, safety) |
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

### Storage layers

Indexing a project produces five persistent stores:

```
┌─────────────────────────────────────────────────────────┐
│  Full-text index (Tantivy)                              │
│    camelCase / snake_case tokenisation, namespace-aware │
├─────────────────────────────────────────────────────────┤
│  Vector index (LanceDB)                                 │
│    local trigram / Ollama / OpenAI — RRF + MMR fusion   │
├─────────────────────────────────────────────────────────┤
│  Knowledge graph (Redb)                                 │
│    ~40 edge kinds, O(degree) adjacency, PageRank        │
├─────────────────────────────────────────────────────────┤
│  Git intelligence (libgit2)                             │
│    commit graph, revert detection, temporal coupling    │
├─────────────────────────────────────────────────────────┤
│  Registry (Redb)                                        │
│    projects, jobs, memory bank, repo rules, meta        │
└─────────────────────────────────────────────────────────┘
```

### Crates

| Crate | Purpose |
|---|---|
| `engram_core` | Core types, config, security boundary, Redb registry |
| `engram_index` | Tantivy FTS + LanceDB vectors + DocStore + hybrid search |
| `engram_graph` | Redb-backed graph store + BFS / PageRank algorithms |
| `engram_git` | libgit2 walker, temporal coupling, revert detection |
| `engram_ml` | Dreaming engine, embedders, style mimicry, immune system |
| `engram_server` | MCP server (rmcp), tool handlers, background actors, multi-client dispatcher |

### Background actors

- **Dreamer** — co-occurrence clustering into Insight generation.
- **Immune actor** — git-revert harvesting into anti-pattern indexing.
- **Watcher** — directory monitoring for incremental re-index.
- **GC scheduler** — orphaned-job cleanup.
- **Integrity sentinel** — cross-store consistency checks with
  configurable auto-repair.
- **Memory-budget watcher** — per-subsystem soft / hard limits with CAS
  allocation and backpressure.
- **Multi-client dispatcher** — auto-daemon, file-based lock, proxy
  forwarding, race-free session counting.

---

## Configuration

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

# LLM backend for cognitive features (optional — everything except
# `dream`, `analyze_business_logic`, `suggest_migration_boundaries`,
# and the optional `use_llm` curation pass on `produce_claude_md`
# works without it)
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

# Safety and Autonomous Decision Protocol
safety_policy_enabled: true
safety_min_confidence: 0.7
safety_min_coverage: 0.6
adp_enabled: true
adp_min_extraction_confidence: 0.5
adp_max_blast_radius: 6
adp_rollout_phase: shadow             # shadow | advisory | guarded | autonomous
adp_kill_switch: false                # emergency kill-switch — forces every verdict to deny
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

Any unknown type falls back to a broad default covering common source and
config extensions.

---

## Example agent prompts

**Orientation**

```
Give me a codebase overview for project "myapp". Then run pre_commit_review
on the staged diff.
```

**Safe refactor**

```
Before I refactor UserRepository.cs in project "myapp":
1. Run impact_analysis for this file
2. Run compute_blast_radius
3. Find all temporal couplings so I know which files to change together
4. Show me the top 5 reverted commits that touched this file
```

**Pre-commit review (JSON for CI)**

```
Run pre_commit_review on the unstaged diff for project "myapp" with
output_json: true, then summarise the CRITICAL and WARNING findings.
```

**Generate project memory**

```
Run produce_claude_md on project "myapp" with write_to_disk: true and
merge_mode: "optimize". Show me the summary notes.
```

**Narrate a diff**

```
Run explain_change on the staged diff for project "myapp" — I need a PR
description.
```

**Legacy modernization**

```
Run analyze_full_project_migration on project "myapp-legacy". Share the
cross-cutting summary and the top 3 files by migration risk.
```

**Anti-pattern guard**

```
Here's code I'm about to commit. Run immune_check on it against project
"myapp" with file_path="Site/App_Code/dal/orders.vb":

[paste code]
```

---

## Security model

- **Path confinement.** All file access is validated against
  `allowed_roots`. Paths that escape via symlinks, `..`, or canonicalisation
  tricks are rejected.
- **Project-ID validation.** Restricted to `[A-Za-z0-9_-]{1,128}` at the
  handler boundary.
- **Single-writer semantics.** Per-project mutexes serialise concurrent
  index updates.
- **No shell execution.** Every git operation uses libgit2 directly; no
  subprocess is spawned.
- **Secrets redacted in output.** `pre_commit_review` fingerprints matched
  secrets; the raw value is never echoed back in findings.
- **Fail-closed on integrity.** The FTS watcher is fail-closed (no silent
  downgrades); integrity sentinels detect cross-store drift and can
  auto-repair.

---

## Documentation

- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — system design and component interactions
- [`docs/DATA_MODEL.md`](docs/DATA_MODEL.md) — storage-layer schemas
- [`docs/TOOL_CONTRACT.md`](docs/TOOL_CONTRACT.md) — full tool parameter reference
- [`docs/COGNITIVE_PIPELINES.md`](docs/COGNITIVE_PIPELINES.md) — dreaming, immune system, style analysis
- [`docs/GENERATION_MODEL.md`](docs/GENERATION_MODEL.md) — append-only generation semantics
- [`docs/DEVELOPER_SPEC.md`](docs/DEVELOPER_SPEC.md) — contributing and internals
- [`docs/ROADMAP.md`](docs/ROADMAP.md) — planned features

---

## License

Licensed under **PolyForm Noncommercial License 1.0.0**
(`PolyForm-Noncommercial-1.0.0`).

- Noncommercial use — personal, hobby, learning, research — is allowed under
  PolyForm Noncommercial 1.0.0.
- Commercial, business, or for-profit use requires a separate written
  commercial license.
- A company using this code — internally, in product development, SaaS,
  services, or any revenue-generating work — must obtain a commercial
  license.

See [`LICENSE`](LICENSE) for the full terms and
[`COMMERCIAL-LICENSE.md`](COMMERCIAL-LICENSE.md) for the commercial-use
notice.
