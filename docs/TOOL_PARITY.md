# Tool Parity Matrix (v1 vs v2)

| Tool Name (v1) | Inputs (v1) | Behavior Notes | v2 Status | Action Needed |
| :--- | :--- | :--- | :--- | :--- |
| `index_project` | `directory`, `name`, `type`, `wait`, `dedupe` | Chunks files, indexes FTS + Vector, registers in DB. | Implemented | None. |
| `update_project` | `project_id`, `wait` | Incremental re-index of changed files. | Implemented | None. |
| `list_projects` | None | Queries DB for all projects. | Implemented | None. |
| `project_info` | `project_id` | Queries DB for specific project. | Implemented | None. |
| `project_health` | `project_id` | Per-namespace stats, disk usage, graph/vector/FTS counts, integrity warnings. | Implemented | None. |
| `repair_project` | `project_id`, `wait` | Generation-based GC + forced full re-index. | Implemented | None. |
| `delete_project` | `project_id` | Deletes DB records and on-disk index files. | Implemented | None. |
| `watch_project` | `project_id` | `notify`-based file watcher with debounce and registry persistence. | Implemented | None. |
| `unwatch_project` | `project_id` | Stops watcher and persists disabled state. | Implemented | None. |
| `search_memory` | `query`, `project_id`, `max_results`, ... | Hybrid search (FTS + Vector) + RRF + MMR. | Implemented | None. |
| `get_chunk` | `project_id`, `chunk_id`, `include_content` | Fetches specific chunk with repo-rule injection. | Implemented | None. |
| `update_memory_bank` | `project_id`, `section`, `content` | Saves virtual file to DB + Index. | Implemented | None. |
| `list_memory_bank` | `project_id` | Queries DB for VFS files. | Implemented | None. |
| `read_memory_bank` | `project_id`, `section` | Fetches virtual file content. | Implemented | None. |
| `delete_memory_bank` | `project_id`, `section` | Deletes virtual file. | Implemented | None. |
| `add_repo_rule` | `project_id`, `file_pattern`, `rule_text`, ... | Saves rule to DB. | Implemented | None. |
| `list_repo_rules` | `project_id` | Queries DB for rules. | Implemented | None. |
| `delete_repo_rule` | `rule_id` | Deletes rule. | Implemented | None. |
| `get_codebase_overview`| `project_id` | Language breakdown, symbol-type aggregation, edge-kind stats, architectural layers, PageRank, DB tables, state keys, temporal couplings. | Implemented | None. |
| `find_symbol_references`| `symbol_name`, `project_id` | All-edge-kind graph lookup with FQN suffix matching, grouped by edge kind, with outgoing deps + lexical fallback. | Implemented | None. |
| `analyze_error_stack` | `traceback`, `project_id` | Multi-language structured parser (Python, .NET, Java, Node.js, Rust, Go, PHP, Ruby, ASP.NET, generic) with frame-boosted search + graph centrality. | Implemented | None. |
| `dream_project` | `project_id`, `wait`, `max_pairs` | Co-occurrence clustering + LLM insights with deterministic fallback. | Implemented | None. |
| `trigger_rem_cycle` | `project_id` | Alias for `dream_project`. | Implemented | None. |
| `analyze_file_coding_style`| `project_id`, `file_path`, `limit` | Git diffs + LLM style summarization. | Implemented | None. |
| `list_jobs` | None | Queries DB for jobs. | Implemented | None. |
| `cancel_job` | `job_id` | Cooperative cancellation with token-based abort. | Implemented | None. |
| `query_graph_nodes` | `project_id`, `node_type`, ...| Queries Graph store. | Implemented | None. |
| `find_references` | `project_id`, `node_id` | Queries Graph store. | Implemented | None. |
| `graph_search` | `project_id`, `query`, `max_results` | Hybrid text search + graph symbol name matching + multi-edge neighbor expansion (Dependency, Contains, Imports, SqlCalls, ApiCall). | Implemented | None. |
| `index_git_history` | `project_id`, `limit`, `branch`, `wait` | Walks git, indexes commits + diffs with streaming batches. | Implemented | None. |
| `search_history` | `query`, `project_id`, ... | Hybrid search in "history" namespace with author/date/file filters. | Implemented | None. |
| `analyze_temporal_couplings`| `project_id`, ... | Graph edge analysis. | Implemented | None. |
| `analyze_reverts` | `project_id` | Immune system: detects reverts, generates LLM-powered descriptive anti-pattern rules with deterministic fallback, indexes reverted diffs. | Implemented | None. |
| `immune_check` (new v2) | `project_id`, `code` | Checks draft code against indexed anti-patterns. | Implemented | None. |

## JSON Request/Response Schemas

### `index_project`
**Request (v1/v2 Parity):**
```json
{
  "directory": "string",
  "project_name": "string",
  "project_type": "string",
  "wait": "boolean (default: true)",
  "dedupe_by_directory": "boolean (default: true)"
}
```
**Response (v2):**
- Text summary of files indexed and `project_id`.

---

### `update_project`
**Request (v1/v2 Parity):**
```json
{
  "project_id": "string",
  "wait": "boolean (default: true)",
  "max_commits": "integer (default: 200)",
  "index_antipatterns": "boolean (default: false)"
}
```
**Response (v2):**
- Text summary of changes and git update status.

---

### `search_memory`
**Request (v1):**
```json
{
  "query": "string",
  "project_id": "string",
  "max_results": "integer (default: 10)",
  "use_mmr": "boolean (default: true)",
  "fts_mode": "string (default: 'strict')",
  "include_content": "boolean (default: true)",
  "max_content_chars_per_result": "integer (default: 1200)",
  "metadata_filter": "object (optional)"
}
```
**Request (v2):**
```json
{
  "query": "string",
  "project_id": "string",
  "namespace": "string (default: 'memory')",
  "max_results": "integer (default: 10)",
  "use_mmr": "boolean (default: true)",
  "fts_mode": "string (default: 'strict')",
  "include_content": "boolean (default: true)",
  "max_content_chars_per_result": "integer (default: 1200)",
  "metadata_filter": "object (optional)"
}
```
**Response (v1 - JSON):**
```json
{
  "results": [
    {
      "id": "string",
      "score": "float",
      "content": "string",
      "metadata": "object"
    }
  ]
}
```
**Response (v2 - Text):**
- Formatted text list of matches with scores and snippets.

---

### `get_chunk`
**Request (v1/v2 Parity):**
```json
{
  "project_id": "string",
  "chunk_id": "string (v1) / integer (v2)",
  "namespace": "string (v2 default: 'memory')"
}
```
**Response (v2):**
- Full content of the chunk with path and generation info.

---

### `project_health`
**Request:**
```json
{
  "project_id": "string"
}
```
**Response (v2):**
- Per-namespace Tantivy doc counts (memory, history, antipattern, vfs).
- LanceDB vector count.
- Graph node + edge counts.
- Disk usage (human-readable).
- Integrity warnings (empty graph with non-empty index, missing vectors, etc.).

---

### `get_codebase_overview`
**Request (v1/v2 Parity):**
```json
{
  "project_id": "string"
}
```
**Response (v2):**
- Language breakdown with percentage per language.
- Symbol-type aggregation (classes, functions, interfaces, files, controls, etc.).
- Edge-type distribution (dependency, imports, sql_calls, api_call, etc.).
- Architectural summary (source files, types, UI controls, service endpoints, DB tables, config).
- Top PageRank central nodes.
- Database tables list.
- Global state keys ranked by read/write frequency.
- Top temporal couplings.

---

### `find_symbol_references`
**Request:**
```json
{
  "symbol_name": "string",
  "project_id": "string"
}
```
**Response (v2):**
- Graph-based references across all edge kinds (not just Dependency).
- FQN suffix matching (e.g., "GetUser" matches "MyApp.Services.GetUser").
- Incoming references grouped by edge kind with source IDs and weights.
- Outgoing dependencies grouped by edge kind.
- Falls back to lexical search if no graph symbol found.

---

### `analyze_error_stack`
**Request (v1/v2 Parity):**
```json
{
  "traceback": "string",
  "project_id": "string"
}
```
**Response (v2):**
- Structured frame extraction summary (file, line, function, FQN).
- Supported languages: Python, .NET/C#, Java, Node.js, Rust, Go, PHP, Ruby, ASP.NET (.aspx/.ascx/.vb/.cs), and generic file:line patterns.
- Frame-boosted search results (files matching stack frames get score boost).
- Graph centrality labels (Hub, Utility).
- Function-level matching against graph nodes in each result file.

---

### `graph_search`
**Request:**
```json
{
  "project_id": "string",
  "query": "string",
  "max_results": "integer (default: 10)",
  "symbol_boost": "float (default: 0.03)"
}
```
**Response (v2):**
- Combined text search hits + graph symbol name matches.
- Symbol nodes matched by name get boosted scores (exact match > substring).
- Parent file nodes of matched symbols get secondary boost.
- Multi-edge neighbor expansion: Dependency, Contains, Imports, SqlCalls, ApiCall.
- Results show node type labels for symbol matches.

---

### `analyze_reverts`
**Request:**
```json
{
  "project_id": "string",
  "max_commits": "integer (default: 200)"
}
```
**Response (v2):**
- LLM-generated descriptive rule text explaining why each reverted pattern should be avoided.
- Deterministic fallback when no LLM is configured.
- Anti-pattern diffs indexed into "antipattern" namespace for `immune_check` queries.
- Repo rules persisted for file-level pattern matching.
