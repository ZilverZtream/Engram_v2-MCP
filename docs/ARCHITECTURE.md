# Architecture Notes

## Storage
- **Registry (Redb)**: configuration-like mutable metadata and job tracking
- **Graph (Redb)**: adjacency-list edges with weights
- **Index (Tantivy)**: append-only search index, filtered by `generation`

## Hot path
- `search_memory`:
  1) Tantivy lexical search
  2) emit search session event (async) for dreaming co-occurrence
  3) return hits

## Background path
- Dreamer:
  - records co-occurrence edges
  - runs clustering when idle
  - inserts insight nodes back into graph

## Data directories
All project data lives under:
`{data_dir}/projects/{project_id}/...`

## Crash consistency
- Redb is ACID.
- Tantivy commits are atomic at segment level.
- Generation switching should be a two-step commit:
  1) write new generation docs
  2) update registry `active_generation`
  3) (future) GC old gens

## Scaling notes
- Use Rayon for parsing/indexing (future milestone)
- Keep embedding generation in a dedicated worker pool (Candle)
- Prefer message passing between actors over shared locks
