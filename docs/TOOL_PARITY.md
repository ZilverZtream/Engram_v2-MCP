# Tool Parity Matrix (v1 vs v2)

| Tool Name (v1) | Inputs (v1) | Behavior Notes | v2 Status | Action Needed |
| :--- | :--- | :--- | :--- | :--- |
| `index_project` | `directory`, `name`, `type`, `wait`, `dedupe` | Chunks files, indexes FTS + Vector, registers in DB. | Implemented | None. |
| `update_project` | `project_id`, `wait` | Incremental re-index of changed files. | Implemented | Ensure real incremental diff logic works. |
| `list_projects` | None | Queries DB for all projects. | Implemented | None. |
| `project_info` | `project_id` | Queries DB for specific project. | Implemented | None. |
| `project_health` | `project_id` | Checks index sync, file existence, counts. | Implemented (Scaffold) | Add real Tantivy/LanceDB/Graph stats. |
| `repair_project` | `project_id`, `wait` | Rebuilds index from DB. | Implemented (Scaffold) | Implement actual generation-based GC/rebuild. |
| `delete_project` | `project_id` | Deletes DB records and on-disk index files. | Implemented | None. |
| `watch_project` | `project_id` | Starts `notify`-based watcher. | Implemented (Scaffold) | Wire up actual `notify` crate watcher. |
| `unwatch_project` | `project_id` | Stops watcher. | Implemented (Scaffold) | Wire up actual `notify` crate watcher. |
| `search_memory` | `query`, `project_id`, `max_results`, ... | Hybrid search (FTS + Vector) + RRF + MMR. | Implemented | Ensure RRF/MMR parity; wire up LanceDB vectors. |
| `get_chunk` | `project_id`, `chunk_id`, `include_content` | Fetches specific chunk. | Implemented | Ensure repo-rule injection works. |
| `update_memory_bank` | `project_id`, `section`, `content` | Saves virtual file to DB + Index. | Implemented | None. |
| `list_memory_bank` | `project_id` | Queries DB for VFS files. | Implemented | None. |
| `read_memory_bank` | `project_id`, `section` | Fetches virtual file content. | Implemented | None. |
| `delete_memory_bank` | `project_id`, `section` | Deletes virtual file. | Implemented | None. |
| `add_repo_rule` | `project_id`, `file_pattern`, `rule_text`, ... | Saves rule to DB. | Implemented | None. |
| `list_repo_rules` | `project_id` | Queries DB for rules. | Implemented | None. |
| `delete_repo_rule` | `rule_id` | Deletes rule. | Implemented | None. |
| `get_codebase_overview`| `project_id` | AST stats + top files. | Implemented (Scaffold) | Implement real AST summary logic. |
| `find_symbol_references`| `symbol_name`, `project_id` | Queries AST index / Graph. | Implemented | Currently uses lexical search; needs real graph lookup. |
| `analyze_error_stack` | `traceback`, `project_id` | Parses trace, finds files/lines. | Implemented | Improve traceback parsing for more languages. |
| `dream_project` | `project_id`, `wait`, `max_pairs` | Co-occurrence clustering -> Insights. | Implemented | Wire up `engram_ml` clusters + LLM. |
| `trigger_rem_cycle` | `project_id` | Alias for `dream_project`. | Implemented | None. |
| `analyze_file_coding_style`| `project_id`, `file_path`, `limit` | Git diffs -> LLM summarization. | Implemented | Wire up `engram_ml` style extraction. |
| `list_jobs` | None | Queries DB for jobs. | Implemented | None. |
| `cancel_job` | `job_id` | Aborts task + updates status. | Implemented | Ensure cooperative cancellation works. |
| `query_graph_nodes` | `project_id`, `node_type`, ...| Queries Graph store. | Implemented | None. |
| `find_references` | `project_id`, `node_id` | Queries Graph store. | Implemented | None. |
| `graph_search` | `project_id`, `query`, `max_results` | FTS + Graph symbol boost. | Implemented | Implement symbol boosting. |
| `index_git_history` | `project_id`, `limit`, `branch`, `wait` | Walks git, indexes commits + diffs. | Implemented | Ensure diff indexing is efficient. |
| `search_history` | `query`, `project_id`, ... | Hybrid search in history namespace. | Implemented (Scaffold) | Index history commits. |
| `analyze_temporal_couplings`| `project_id`, ... | Graph edge analysis. | Implemented | None. |
| `analyze_reverts` | `project_id` | Immune system: reverts -> anti-patterns. | Implemented | Implement auto-rule generation from reverts. |
| `immune_check` (new v2) | `project_id`, `code` | Checks draft against anti-patterns. | Implemented | None. |

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
