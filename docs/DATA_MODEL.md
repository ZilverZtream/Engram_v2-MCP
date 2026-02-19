# Data Model

This document defines the canonical entities and their storage in Engram v2.

## Storage backends

- **Redb**: Primary source of truth for metadata, project records, jobs, and the code graph (adjacency list).
- **Tantivy**: Lexical search index (trigram-based for code).
- **LanceDB**: Vector store for semantic search (to be implemented).

## Entities

### Project
- **Stored in**: Redb (Table `projects`)
- **Fields**: `project_id`, `project_name`, `project_type`, `directory`, `created_at_ms`, `updated_at_ms`.
- **Note**: `project_id` is a UUID v4 string.

### File
- **Stored in**: Implicitly via paths in Tantivy/Graph.
- **Node in Graph**: `node_type="file"`, `node_id="file:{relative_path}"`.

### Chunk
- **Stored in**: Tantivy (namespace `memory`)
- **Fields**: `chunk_id` (blake3 hash subset), `path`, `content`, `generation`.
- **Node in Graph**: `node_type="chunk"`, `node_id="chunk:{chunk_id}"`.

### Node
- **Stored in**: Redb (Table `nodes`)
- **Fields**: `node_id`, `node_type`, `name`, `file_path`, `start_line`, `end_line`, `metadata`.
- **Types**: `file`, `chunk`, `symbol` (class/function), `insight`.

### Edge
- **Stored in**: Redb (Table `edges`)
- **Fields**: `source_id`, `target_id`, `edge_kind`, `weight`, `metadata`, `updated_at_ms`.
- **Kinds**: `co_occurrence`, `temporal_coupling`, `insight`, `dependency`, `anti_pattern`.

### Insight
- **Stored in**: Redb (as a `Node`) + Tantivy (namespace `insights`).
- **Triggered by**: Dreaming pipeline.

### Job
- **Stored in**: Redb (Table `jobs`).
- **Fields**: `job_id`, `kind`, `project_id`, `status`, `message`, `created_at_ms`, `updated_at_ms`.
- **Status**: `running`, `done`, `failed`, `cancelled`.

### Rule (RepoRule)
- **Stored in**: Redb (Table `repo_rules`).
- **Fields**: `rule_id`, `file_pattern`, `rule_text`, `updated_at_ms`.

### Watch
- **Stored in**: Redb (Table `watches`).
- **Fields**: `watch_id`, `directory`, `enabled`, `updated_at_ms`.

## Keying Scheme (Redb)

- `projects`: `"{project_id}"`
- `memory_bank`: `"{project_id}\0{section_id}"`
- `repo_rules`: `"{project_id}\0{rule_id}"`
- `watches`: `"{project_id}\0{watch_id}"`
- `jobs`: `"{job_id}"`
- `meta`: `"{project_id}\0{key}"`
- `nodes`: `"{project_id}\0{node_id}"`
- `edges`: `"{project_id}\0{edge_kind}\0{source_id}\0{target_id}"`

## Garbage Collection (GC) Policy

Engram v2 uses a generation-based GC policy to keep the search index and graph store clean.

### Namespace Scopes
- **Generation-Scoped**: Tied to a specific `active_generation`. Purged when a new generation is committed and GC runs.
  - `memory`: Core code/text index.
  - `history`: Git commit messages and diffs.
  - `antipattern`: Reverted code patterns.
- **Global**: Persists across all generations. Never purged by automated GC.
  - `memory_bank`: Agent's persistent notes.
  - `insights`: Dreaming outputs.

### GC Rules
1. When `update_project` finishes, it increments the `active_generation`.
2. The `GCActor` periodically runs and triggers `purge_old_generations`.
3. `purge_old_generations` deletes all data where:
   - `project_id` matches AND
   - `namespace` is **Generation-Scoped** AND
   - `generation` is NOT the current `active_generation`.
