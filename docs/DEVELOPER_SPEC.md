# Engram MCP v2 Developer Specification

This repo is a **Rust workspace scaffold** for Engram MCP v2: a developer tool that unifies

- **Fast code/text search** (Tantivy, Sourcegraph-style tokenization later)
- **Graph cognition** (Redb-backed adjacency lists + analysis)
- **Git intelligence** (temporal coupling + reverts/anti-patterns)
- **Cognitive features** (REM-style Dreaming, Style Mimicry, Immune System)

The goal is to keep v1 tool contracts stable while moving the implementation to a modular, crash-safe, parallel Rust stack.

---

## 1) Workspace layout

```
engram-mcp-v2-scaffold/
├── Cargo.toml
├── crates/
│   ├── engram_core/     # shared types, config, security boundary, registry
│   ├── engram_index/    # Tantivy + (future) LanceDB hybrid index
│   ├── engram_graph/    # Redb graph store + clustering/coupling algorithms
│   ├── engram_git/      # libgit2 walker, temporal coupling, revert harvesting
│   ├── engram_ml/       # dreaming summarizer, style mimicry, immune decisions
│   └── engram_server/   # MCP server (rmcp), actors, tool router
└── docs/
```

---

## 2) Core invariants

### 2.1 Security boundary (allowed_roots)
All paths that touch the filesystem must pass through `engram_core::PathContext::resolve_path`.
Any tool that accepts `directory` must reject paths outside `allowed_roots`.

### 2.2 Project registry is the source of truth
`engram_core::Registry` (Redb) stores:

- projects (project_id → directory, name, type)
- meta (project_id + key → value): `active_generation`, `last_git_oid`, etc.
- memory bank sections
- repo rules
- watches
- jobs

### 2.3 Generations prevent “duplicate indexing”
Tantivy is append-only. To avoid duplicates on re-index:

- each project has `active_generation`
- every indexed doc has a `generation` field
- queries filter on `generation == active_generation`

**Scaffold status:** implemented in `engram_index::HybridQuery.generation` and server tools.

### 2.4 Namespaces
Index namespaces allow multiple “spaces” in the same search engine:

- `memory` – repository code/docs
- `memory_bank` – user-provided notes/constraints
- `history` – (future) commit/diff summaries
- `antipattern` – reverted patterns (immune system)

---

## 3) Cognitive features: “v1 parity + v2 upgrade path”

### 3.1 REM-Style Dreaming
Pipeline:

1. `search_memory` emits a `SearchSession` event with hit chunk_ids.
2. Dreamer actor records **co-occurrence edges** in `engram_graph` (EdgeKind::CoOccurrence).
3. When idle (or via `dream_project`), the actor finds dense clusters (Leiden/Louvain later).
4. `engram_ml::DreamingEngine` summarizes cluster context into an “Insight”.
5. Insight is inserted into the graph as a node + edges.

**Scaffold status:** co-occurrence recording + simple clustering + deterministic summarizer.

### 3.2 Temporal coupling (“hidden dependencies”)
Streaming design:

- `engram_git::GitWalker` walks commits incrementally.
- For each commit, we add/update undirected temporal edges between changed files.
- `analyze_temporal_couplings` reads neighbors instantly from the graph.

**Scaffold status:** incremental commit walking + graph edge increments are present; needs tighter integration (single pass, exact stop_oid).

### 3.3 Style mimicry (“chameleon”)
Design:

- use semantic diffs (tree-sitter later) and summarize patterns
- output a human-usable “style guide” that can be injected into prompts

**Scaffold status:** uses raw diffs and a deterministic summarizer in `engram_ml::StyleMimicryEngine`.

### 3.4 Immune system (revert analysis)
Design:

- detect reverts structurally
- index reverted diffs into `antipattern` namespace
- block/warn generation if new code matches prior reverted patterns

**Scaffold status:** revert harvesting is wired; anti-pattern indexing is a stubbed namespace (lexical for now).

---

## 4) Where to implement what

### engram_index
- Add tree-sitter parsing and symbol extraction (future)
- Add LanceDB vector search (feature-flagged)
- Add “doc lookup by chunk_id” (already included)

### engram_graph
- Keep the store minimal: nodes + typed edges with weights
- Algorithms live in `src/algorithms/*`
- Keep “expensive” computations off the hot path; compute & cache centrality/PR in background

### engram_git
- Only libgit2; no shelling out
- Stream pairs → graph temporal edges
- Harvest reverts → anti-pattern docs

### engram_server
- Tool router (v1 parity)
- Actors:
  - dreamer (low priority)
  - immune (future)
- Job manager (spawn/track/cancel long work)

---

## 5) Next engineering milestones

1. **Dependency graph from AST** (tree-sitter → symbol graph edges)
2. **Vector search MVP** (Candle embedder + LanceDB)
3. **Incremental indexing** (watcher + generation + GC of old generations)
4. **Anti-pattern index** (separate Tantivy schema + embedding space)
5. **RRR fusion** (RRF + graph centrality boosting at ranking time)
