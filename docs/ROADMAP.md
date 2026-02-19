# Roadmap (suggested)

## Phase 0: Scaffold (done here)
- Workspace compiles
- v1 tool names exist
- registry + generation semantics
- dreamer actor skeleton

## Phase 1: Incremental indexing
- file watcher (notify)
- maintain “changed files” queue
- per-file reindex into next generation
- GC for old generations

## Phase 2: Graph enrichment
- tree-sitter AST extraction
- symbol nodes (function/type/module)
- dependency edges + reference edges
- centrality + pagerank cache

## Phase 3: Hybrid semantic search
- Candle embeddings for chunks
- LanceDB vector store keyed by chunk_id
- RRF fusion (lexical + vector)
- graph-aware reranking (centrality boost)

## Phase 4: Immune system
- reliable revert detection
- dedicated anti-pattern index
- similarity thresholding + “block/warn”
