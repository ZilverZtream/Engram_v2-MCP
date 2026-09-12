# Enable vectors on an existing lexical index

An index created with `embedding_backend: fts_only` can retain its stored source,
history and knowledge while gaining vector search. Configure an embedding backend,
restart the server so that configuration is loaded, then call:

```json
{
  "project_id": "your-project-id",
  "scope": "initialize_vectors"
}
```

Pass these arguments to `repair_project`. The call is synchronous and can take a
long time for a large corpus. Keep the client connected with a suitable request
timeout. Source updates are serialized behind this maintenance operation.

Engram embeds the existing stored documents in bounded batches into a new local
LanceDB staging table. It publishes that table only after the processed and stored
row counts match the source snapshot. Document IDs, text, namespace metadata,
generation and history watermarks remain unchanged. This adds embeddings of stored
content; it does not refresh outdated source or re-extract business rules.

The operation refuses an existing vector table and cannot be combined with
`wipe_and_reindex`. It is not a model migration or repair of a partially populated
existing table. `vector_only` also does not re-embed: that scope purges superseded
generations.

On a provider error or cooperative cancellation, Engram drops only the staging
table created by that operation. A forced process termination can leave an
unfinished staging table. A later initialization refuses to accumulate another;
inspect the reported project staging table before retrying. It never deletes a
pre-existing staging table automatically.

After success, use `project_health` and `get_index_freshness`, then exercise semantic
search and exact chunk recovery. Matching row counts establish storage coverage,
not semantic ranking quality or the correctness of extracted knowledge.
