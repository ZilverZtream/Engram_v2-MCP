# Tool Parity Matrix (v1 vs v2)

Status legend: `implemented`, `partial`, `experimental`, `planned`.

| Tool Name (v1) | v2 Status | Behavior / Parity Notes |
| :--- | :--- | :--- |
| `index_project` | implemented | Chunks files, indexes FTS + Vector, registers in DB; covered by integration tests. |
| `update_project` | implemented | Incremental re-index of changed files; runs generation bump flow. |
| `list_projects` | implemented | Queries DB for all projects; registry-backed. |
| `project_info` | implemented | Queries DB for specific project; registry-backed. |
| `project_health` | partial | Per-namespace stats and disk usage; deeper backend diagnostics still limited. |
| `repair_project` | partial | Recovery flow exists; full generation-based GC / rebuild semantics incomplete. |
| `delete_project` | implemented | Deletes DB records and on-disk index files. |
| `watch_project` | implemented | `notify`-based file watcher with debounce; wired to actor system. |
| `unwatch_project` | implemented | Stops watcher and persists disabled state. |
| `search_memory` | implemented | Hybrid search (FTS + Vector) + RRF + MMR. |
| `get_chunk` | implemented | Fetches specific chunk with repo-rule injection. |
| `update_memory_bank` | implemented | Saves virtual file to DB + Index. |
| `list_memory_bank` | implemented | Queries DB for VFS files. |
| `read_memory_bank` | implemented | Fetches virtual file content. |
| `delete_memory_bank` | implemented | Deletes virtual file. |
| `add_repo_rule` | implemented | Saves rule to DB; creates persisted entries. |
| `list_repo_rules` | implemented | Queries DB for rules. |
| `delete_repo_rule` | implemented | Deletes rule by ID. |
| `get_codebase_overview`| partial | Language breakdown and symbol aggregation; richer AST/centrality in progress. |
| `find_symbol_references`| partial | Graph lookup with FQN matching; full symbol-graph fidelity still evolving. |
| `analyze_error_stack` | implemented | Multi-lang parser (Rust, Node, etc.) with frame-boosted search + graph centrality. |
| `dream_project` | experimental | Co-occurrence clustering + LLM insights; still iterative. |
| `trigger_rem_cycle` | experimental | Alias for `dream_project`; same maturity level. |
| `analyze_file_coding_style`| experimental | Git diffs + LLM style summarization; currently maturing. |
| `list_jobs` | implemented | Queries DB for background jobs. |
| `cancel_job` | implemented | Cooperative cancellation with token-based abort. |
| `query_graph_nodes` | implemented | Queries Graph store by filters/substrings. |
| `find_references` | implemented | Traverses graph references for a node. |
| `graph_search` | partial | Hybrid search + neighbor expansion; centrality boosting not fully integrated. |
| `index_git_history` | implemented | Walks git, indexes commits + diffs with streaming batches. |
| `search_history` | partial | Hybrid search in "history" namespace; depends on index coverage quality. |
| `analyze_temporal_couplings`| implemented | Graph edge analysis; reads temporal coupling edges. |
| `analyze_reverts` | partial | Detects reverts, generates LLM anti-pattern rules; anti-pattern quality iterative. |
| `immune_check` | experimental | Checks draft code against indexed anti-patterns; thresholding maturing. |
| `ast_dependency_graph` | partial | AST extraction exists for several languages; full graph incomplete. |
| `vector_search` | experimental | Vector path enabled by feature flags; tuning ongoing. |
| `incremental_indexing_gc` | partial | Watcher updates exist; old-generation GC remains incomplete. |
| `dedicated_antipattern_index` | partial | Dedicated mature ranking/indexing still in progress. |
| `graph_centrality_rerank` | planned | Targeted roadmap item; not yet integrated into pipeline. |

## JSON Request/Response Schemas

### `index_project`
**Request (v1/v2 Parity):**
{
  "directory": "string",
  "project_name": "string",
  "project_type": "string",
  "wait": "boolean (default: true)",
  "dedupe_by_directory": "boolean (default: true)"
}

**Response (v2):**
- Text summary of files indexed and `project_id`.

---

### `update_project`
**Request (v1/v2 Parity):**
{
  "project_id": "string",
  "wait": "boolean (default: true)",
  "max_commits": "integer (default: 200)",
  "index_antipatterns": "boolean (default: false)"
}

**Response (v2):**
- Text summary of changes and git update status.

---

### `search_memory`
**Request (v1):**
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

**Request (v2):**
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

**Response (v1 - JSON):**
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

**Response (v2 - Text):**
- Formatted text list of matches with scores and snippets.

---

### `get_chunk`
**Request (v1/v2 Parity):**
{
  "project_id": "string",
  "chunk_id": "integer",
  "namespace": "string"
}

**Response (v2):**
- Full content of the chunk with path and generation info.

---

### `project_health`
**Request:**
{
  "project_id": "string"
}

**Response (v2):**
- Per-namespace Tantivy doc counts.
- LanceDB vector count.
- Graph node + edge counts.
- Disk usage (human-readable).

---

### `get_codebase_overview`
**Request:**
{
  "project_id": "string"
}

**Response (v2):**
- Language breakdown.
- Symbol-type aggregation.
- Architectural summary.

---

### `find_symbol_references`
**Request:**
{
  "symbol_name": "string",
  "project_id": "string"
}

**Response (v2):**
- Graph-based references.
- Incoming/Outgoing dependencies.

---

### `analyze_error_stack`
**Request:**
{
  "traceback": "string",
  "project_id": "string"
}

**Response (v2):**
- Structured frame extraction.
- Frame-boosted search results.

---

### `graph_search`
**Request:**
{
  "project_id": "string",
  "query": "string"
}

**Response (v2):**
- Combined text search + graph matches.

---

### `analyze_reverts`
**Request:**
{
  "project_id": "string"
}

**Response (v2):**
- LLM-generated descriptive anti-pattern rules.
- Deterministic fallback when no LLM is configured.
- Anti-pattern diffs indexed into "antipattern" namespace for `immune_check` queries.
- Repo rules persisted for file-level pattern matching.
