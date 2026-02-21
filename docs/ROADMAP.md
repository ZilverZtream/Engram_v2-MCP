# Roadmap (status-aligned)

Status labels: `implemented`, `partial`, `experimental`, `planned`.

## Platform baseline
- Core tool surface and v1 parity coverage: **implemented**
- Capability matrix/code-flag reconciliation checks: **implemented**

## Phase 1: Incremental indexing hardening
- Watcher + debounce + auto-update loop: **implemented**
- Changed-file targeting quality improvements: **partial**
- Old-generation GC + reclaim policy: **planned**

## Phase 2: Graph enrichment
- Existing graph node/reference traversal: **implemented**
- AST-derived dependency/symbol graph depth: **partial**
- Background centrality/PageRank cache and ranking usage: **planned**

## Phase 3: Hybrid semantic search
- Lexical + vector query pipeline: **experimental**
- RRF fusion quality tuning: **partial**
- Graph-aware reranking boost: **planned**

## Phase 4: Immune system
- Revert harvesting + anti-pattern capture: **partial**
- Dedicated anti-pattern indexing/ranking hardening: **partial**
- Strict block/warn policy thresholds and confidence gating: **experimental**
