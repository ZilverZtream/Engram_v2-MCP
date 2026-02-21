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

## Indexing concurrency + memory under load
- All `index_files(...)` calls in `engram_server` now pass through a single parse guard
  (`AppState.parse_semaphore`) before entering the indexer.
- This includes:
  - synchronous initial indexing (`index_project(wait=true)`),
  - incremental updates (`update_project_impl`),
  - background index jobs (`spawn_job_index_directory`), and
  - background update jobs (`spawn_job_update_project`).

Expected behavior when many projects index concurrently:
- `max_concurrent_jobs` limits how many jobs can be active.
- `max_parse_concurrency` further limits how many jobs can be in the parse/chunk stage at once.
- Jobs above the parse limit wait on the semaphore instead of allocating parse/chunk working sets.

Memory impact:
- Peak parse/chunk memory is roughly bounded by
  `O(max_parse_concurrency × batch_working_set)` rather than
  `O(active_jobs × batch_working_set)`.
- This prevents bursty background indexing from multiplying transient file-buffer/chunk
  allocations across all jobs.
