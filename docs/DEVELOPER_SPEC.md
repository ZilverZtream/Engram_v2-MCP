# Engram MCP v2 Developer Specification

This repo is a Rust workspace for Engram MCP v2: a developer tool that combines code search, graph reasoning, git history analysis, and cognitive helper tools.

Canonical capability status labels are tracked in code (`crates/engram_server/src/capabilities.rs`) and mirrored in `docs/TOOL_PARITY.md`.

- `implemented`: production-ready behavior in-tree
- `partial`: available but missing notable parity/quality pieces
- `experimental`: usable but intentionally iterative/tunable
- `planned`: design target only

---

## 1) Workspace layout

```
engram-v2/
├── Cargo.toml
├── crates/
│   ├── engram_core/     # shared types, config, security boundary, registry
│   ├── engram_index/    # Tantivy + LanceDB-backed hybrid index
│   ├── engram_graph/    # Redb graph store + coupling/clustering algorithms
│   ├── engram_git/      # libgit2 walker, temporal coupling, revert harvesting
│   ├── engram_ml/       # dreaming summarizer, style mimicry, immune helpers
│   └── engram_server/   # MCP server (rmcp), actors, tool router
└── docs/
```

---

## 2) Core invariants

### 2.1 Security boundary (allowed_roots)
All filesystem access flows through `engram_core::PathContext::resolve_path`. Tools receiving directory/file paths reject paths outside `allowed_roots`.

### 2.2 Project registry is the source of truth
`engram_core::Registry` stores projects, generation metadata, memory bank sections, repo rules, watches, and jobs.

### 2.3 Generations prevent duplicate indexing
Each indexed document is tagged with `generation`, and search queries filter to `active_generation`.

### 2.4 Namespaces
- `memory`: repository content
- `memory_bank`: user-managed notes/rules context
- `history`: commit/diff history docs
- `antipattern`: revert-derived anti-pattern docs

---

## 3) Capability reconciliation (current)

### 3.1 Search and indexing
- Core indexing/search tools are **implemented**.
- Incremental watcher-driven updates are **implemented**, but old-generation GC is **partial**.
- Vector search is available and enabled by feature defaults, but still **experimental** for ranking quality/perf tuning.

### 3.2 Graph and references
- Graph query/reference primitives are **implemented**.
- Symbol/reference intelligence using richer AST graph semantics is **partial**.
- Graph centrality-aware reranking remains **planned**.

### 3.3 Git intelligence
- Git history indexing and temporal couplings are **implemented**.
- Revert analysis and anti-pattern enforcement are **partial** to **experimental**, depending on tool path.

### 3.4 Cognitive features
- Dreaming and style mimicry are present but **experimental**.
- Immune checks are **experimental**, with quality and thresholding still under active iteration.

---

## 4) Source-of-truth sync rule

When changing feature maturity:
1. Update `crates/engram_server/src/capabilities.rs`.
2. Update the corresponding row(s) in `docs/TOOL_PARITY.md`.
3. Ensure `scripts/check_capabilities_matrix.py` passes locally and in CI.
