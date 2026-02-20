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
- 12 edge kinds: `CoOccurrence`, `TemporalCoupling`, `Insight`, `Dependency`, `AntiPattern`, `Contains`, `Imports`, `SqlCalls`, `HasColumn`, `ForeignKey`, `QueriesTable`, `ReadsState`, `WritesState`
- Node types: `Function`, `Class`, `File`, `Table`, `Column`, `Insight`, `MemoryBankSection`
- O(degree) adjacency lookups via Redb-backed adjacency lists
- PageRank scoring for codebase overview ranking

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

### Database Analysis
- SQL DDL extraction (tables, columns, foreign keys)
- Cross-references SQL identifiers to application code
- `get_table_schema` returns columns, FK relationships, and every code location that references the table
- `trace_state_usage` tracks readers and writers of global state (Session, ViewState, Application, Cache)

### Memory Bank & Repo Rules
- Per-project **memory bank**: structured notes the agent writes and reads across sessions (architectural decisions, constraints, known issues)
- **Repo rules**: file-pattern-matched constraints injected into chunk retrieval (e.g., "all files matching `*Repository.cs` must use the Unit of Work pattern")

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
| `project_health` | Quick health check (sync state, file counts) |
| `delete_project` | Delete a project and all its stored data |
| `repair_project` | Rebuild index from registry (GC + defrag) |

### Search

| Tool | Description |
|------|-------------|
| `search_memory` | Hybrid FTS + vector search. Parameters: `query`, `project_id`, `namespace`, `max_results`, `use_mmr`, `fts_mode`, `include_content`, `max_content_chars_per_result`, language/path filters |
| `get_chunk` | Fetch full content for a specific chunk by ID, with optional repo rule injection |
| `graph_search` | Search with knowledge graph node-boost reranking |
| `find_symbol_references` | Find all references to a symbol using graph + lexical fallback |
| `get_codebase_overview` | High-level stats + top PageRank nodes |
| `analyze_error_stack` | Parse a stack trace and identify likely source files |

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
| `search_history` | Search commit messages and diffs |
| `analyze_temporal_couplings` | Detect files that frequently change together |
| `analyze_reverts` | Detect reverted commits and harvest anti-patterns |

### Cognitive Features

| Tool | Description |
|------|-------------|
| `dream_project` | Cluster co-occurrence patterns and generate insight nodes |
| `trigger_rem_cycle` | Alias for `dream_project` |
| `analyze_file_coding_style` | Analyze a file's git history and produce a style guide |
| `immune_check` | Score a code draft against the anti-pattern index |
| `anti_pattern_guard` | Score code with remediation suggestions |

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

Apache-2.0
