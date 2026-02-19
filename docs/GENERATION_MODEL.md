# Generation Model

Engram v2 uses a "Generation-based" approach to indexing to ensure atomic updates and crash safety across multiple stores (Tantivy, Redb).

## Active Generation

- Each project has an `active_generation` stored in Redb (`meta` table).
- When a project is first indexed, `active_generation = 1`.
- All documents in Tantivy and nodes/edges in Redb (where applicable) are tagged with this generation.

## Atomic Updates (The Swap)

When `update_project` is called:
1. Increment `target_generation = active_generation + 1`.
2. Index files into the stores using `target_generation`.
3. Only after the entire process succeeds, update the `active_generation` in Redb to `target_generation`.
4. Any subsequent queries will automatically filter for documents matching the new `active_generation`.

## Crash Safety

- If the server crashes during indexing, the `active_generation` remains unchanged.
- On restart, the system continues to serve the old generation.
- Partial data from the failed generation still exists but is filtered out by queries.

## Garbage Collection (GC)

- Stale generations (where `generation < active_generation`) should be purged periodically.
- **Tantivy purge**: Delete documents where `generation != active_generation`.
- **Redb purge**: Delete nodes/edges from older generations.
- GC is triggered by `repair_project` or periodically by a background task.

## Recovery

If `active_generation` is lost or corrupted, `repair_project` can re-scan the stores to identify the highest complete generation or trigger a full re-index.
