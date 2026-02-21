# Tool Parity Matrix (v1 vs v2)

Status legend: `implemented`, `partial`, `experimental`, `planned`.

| Tool / Feature | v2 Status | Notes |
| :--- | :--- | :--- |
| `index_project` | implemented | Indexes project files into search/graph stores; covered by integration tests. |
| `update_project` | implemented | Runs generation bump + incremental update flow. |
| `list_projects` | implemented | Registry-backed listing. |
| `project_info` | implemented | Registry-backed project lookup. |
| `project_health` | partial | Basic checks exist; deeper backend diagnostics are still limited. |
| `repair_project` | partial | Recovery flow exists, but full generation GC/rebuild semantics are incomplete. |
| `delete_project` | implemented | Removes project metadata and indexed content. |
| `watch_project` | implemented | `notify` watcher actor is wired with debounce + cancellation. |
| `unwatch_project` | implemented | Disables active watch + pending updates. |
| `search_memory` | implemented | Hybrid lexical/vector search path is available. |
| `get_chunk` | implemented | Fetches indexed chunk by id. |
| `update_memory_bank` | implemented | Writes/updates memory-bank virtual docs. |
| `list_memory_bank` | implemented | Lists memory-bank sections. |
| `read_memory_bank` | implemented | Reads memory-bank section content. |
| `delete_memory_bank` | implemented | Deletes memory-bank section content. |
| `add_repo_rule` | implemented | Creates persisted repository rule entries. |
| `list_repo_rules` | implemented | Lists repository rules from registry. |
| `delete_repo_rule` | implemented | Deletes a repository rule by id. |
| `get_codebase_overview` | partial | Produces lightweight overview; richer AST/centrality output remains in progress. |
| `find_symbol_references` | partial | Graph + lexical fallback exists; full symbol-graph fidelity is still evolving. |
| `analyze_error_stack` | implemented | Parses stack traces and maps likely code locations. |
| `dream_project` | experimental | Works with deterministic clustering/summarization, still iterative. |
| `trigger_rem_cycle` | experimental | Alias for dream pipeline; same maturity level. |
| `analyze_file_coding_style` | experimental | Current style extraction is deterministic and still maturing. |
| `list_jobs` | implemented | Lists background jobs from job manager. |
| `cancel_job` | implemented | Cooperative cancellation path exists. |
| `query_graph_nodes` | implemented | Graph node query by filters/substrings. |
| `find_references` | implemented | Traverses graph references for a node. |
| `graph_search` | partial | Search works; centrality/symbol boosting is not fully integrated. |
| `index_git_history` | implemented | Walks git history and indexes commit/diff docs. |
| `search_history` | partial | History search exists; depends on indexed commit/diff coverage quality. |
| `analyze_temporal_couplings` | implemented | Reads temporal coupling edges from graph store. |
| `analyze_reverts` | partial | Revert harvesting and rule insertion exists; anti-pattern quality still iterative. |
| `immune_check` | experimental | Anti-pattern matching flow exists, but thresholding/quality are still maturing. |
| `ast_dependency_graph` | partial | AST extraction exists for several languages; full dependency graph remains incomplete. |
| `vector_search` | experimental | Vector path is enabled by feature flags; quality/perf tuning ongoing. |
| `incremental_indexing_gc` | partial | Incremental watcher updates exist; old-generation GC remains incomplete. |
| `dedicated_antipattern_index` | partial | Anti-pattern namespace exists; dedicated mature indexing/ranking is still in progress. |
| `graph_centrality_rerank` | planned | Targeted roadmap item; not yet integrated into ranking pipeline. |

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

### `get_codebase_overview`
**Request (v1/v2 Parity):**
```json
{
  "project_id": "string"
}
```
**Response (v2):**
- Text summary of project stats and top PageRank nodes.

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
- List of likely related code chunks.
