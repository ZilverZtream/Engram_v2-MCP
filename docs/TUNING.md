# Tuning indexing for a large repository

Start with the existing configuration and a known source query. Record the
indexing job's duration, `get_metrics` output, process memory and
`get_index_freshness` before changing one setting. Recheck source correctness
afterward. For OciusX, retain its established source scope, VB sidecar and
embedding provider unless the change being evaluated specifically concerns them.

The profiles below are **starting suggestions**, not measured optima or throughput
promises. Count eligible source files after project-type and ignore filtering;
file size, generated/minified code, VB compilation and history can dominate cost.
The small fixture used to verify these settings does not model a full OciusX index.

## Starting values by source size

| Eligible source files | `tantivy_writer_memory` (bytes) | `max_concurrent_jobs` | `max_parse_concurrency` | `watch_debounce_secs` | `max_commits_per_watch` |
| --- | ---: | ---: | ---: | ---: | ---: |
| Up to 10,000 | 50000000 | 2 | 4 | 5 | 50 |
| 10,001–100,000 | 50000000 | 1 | 4 | 10 | 50 |
| Above 100,000, with measured spare RAM | 150000000 | 1 | 2 | 10 | 50 |

The current defaults are 50,000,000 writer bytes, two concurrent jobs, five
seconds of debounce and 50 commits per watcher update. Parse concurrency defaults
to available parallelism capped at 16 (fallback 4); the explicit values above
trade potential throughput for less simultaneous parsing. The larger writer
setting follows the existing configuration guidance for 100k+ files; increase it
only when memory headroom and indexing observations support the change.

Writer memory is allocated per active writer, not once for the whole machine.
Other indexes, parsing, graph storage, embedding and the VB companion also need
memory. Under pressure, try fewer concurrent jobs/parsers before increasing
writer memory. A longer debounce groups edits but also delays automatic freshness.
Changing the delay does not enable watching: call `watch_project` for the project
and inspect the watcher status returned by `get_index_freshness`.

For example, merge this fragment into an existing configuration for a medium
repository; retain its actual `allowed_roots`, `data_dir` and provider settings:

<!-- tuning-medium -->
```json
{
  "tantivy_writer_memory": 50000000,
  "max_concurrent_jobs": 1,
  "max_parse_concurrency": 4,
  "watch_debounce_secs": 10,
  "max_commits_per_watch": 50,
  "memory_budget_bytes": 0
}
```

For the large profile, use this fragment only after checking available memory:

<!-- tuning-large -->
```json
{
  "tantivy_writer_memory": 150000000,
  "max_concurrent_jobs": 1,
  "max_parse_concurrency": 2,
  "watch_debounce_secs": 10,
  "max_commits_per_watch": 50,
  "memory_budget_bytes": 0
}
```

Restart the daemon and reconnect clients to load configuration changes. Let
active work finish first. Keep the previous configuration so a measured
regression can be reversed; no index deletion is needed just to change these
scheduling settings.

## Memory and admission limits

`memory_budget_bytes: 0` selects the current **2 GiB tracked-allocation budget**.
It is not unlimited and is not an OS limit on resident memory. Engram reserves
estimated subsystem costs; process memory, native libraries and companion
processes require separate observation. An allocation refusal is a real limit,
not evidence that an operation completed. Reduce simultaneous work or investigate
the reported cost before considering a larger budget.

`max_project_files` and `max_project_bytes` are admission limits, not performance
targets. Set them to a consciously chosen allowed source size; do not lower them
and then interpret missing source as a complete index. Keep ignore rules aligned
with required application source. Verify admitted files and freshness after any
scope change.

Despite its legacy name, `max_chunks_per_file` is a target **character count per
source chunk**, default 2000; it does not cap the number of chunks in a file.
Changing it changes retrieval granularity and is not a generic memory fix.
`verify_unchanged_hashes` defaults to false, allowing matching mtime/size signatures
to avoid rehashing unchanged files. True adds byte-reading cost; use it when that
integrity tradeoff matters, not as a speed setting.

## Choose embeddings for the task

| Backend | Current behavior | When to consider it |
| --- | --- | --- |
| `fts_only` | Lexical retrieval; vector search disabled | Offline identifier/source lookup, a clean retrieval baseline, or avoiding provider work. Use `semantic: false` explicitly in baseline searches. |
| `local` / `candle` | Deterministic 384-dimensional trigram projection, reported as degraded semantic quality. Current source falls back to lexical ranking for combined search; explicit vector lookup remains degraded. | Existing offline projection workflows. Neither name currently selects a learned sentence model; setting `embedding_model` does not turn this implementation into one. The fallback is source-validated, pending runtime deployment; see the [delivery status](../eval/ociusx-lexical-fallback-progress-20261113.json). |
| `ollama` | Configured embedding model through the Ollama endpoint | Semantic retrieval with an available, correctly configured model/service; measure indexing and query costs. |
| `openai` / `remote` | Configured compatible remote embedding service | Semantic retrieval where provider access, dimensions, latency and cost fit the application. |

The loaded default is `local`. Keep an existing provider while tuning scheduling
so the before/after comparison measures the intended change. Provider/model/
dimension changes can make existing vectors incompatible or incomplete; follow
the reported migration/reindex requirement. Preserve source and configuration;
do not delete data as a shortcut. `llm_backend` controls text generation separately
from embedding. Setting it to `none` does not disable an embedding provider.

## Bound history work separately

`max_commits_per_watch` applies to watcher-triggered updates; it is not the total
history retained for the project. The watcher does not request anti-pattern
indexing. Manual `update_project` has its own `max_commits` argument, default
10,000, and `index_antipatterns`, default false. `index_project` does not accept a
`max_commits` field. There is no `git_history_depth` configuration key.

For an interactive source refresh, an explicit 200-commit work limit is a
conservative starting point independent of source-file count:

```json
{
  "project_id": "<PROJECT_ID>",
  "wait": false,
  "max_commits": 200,
  "index_antipatterns": false
}
```

Pass this object to `update_project`, poll the returned job with `get_job_status`,
then check `get_index_freshness`. The limit bounds requested history work; it does
not establish full historical coverage. Plan explicit history ingestion when
temporal coupling, reverts or older changes are required. A source generation
being current does not prove those history tasks are complete.

## Evaluate the change

Keep the repository revision, admitted scope, provider and query set fixed.
Compare indexing duration, observed memory, refusals and query response time,
then inspect expected source results. Watcher delay, indexing work and query
latency are separate measurements. If results regress, restore the prior setting
and investigate the reported cause. The existing fixture latency checks can catch
some regressions; their small-corpus timings are not production sizing evidence.

Implementation references: [configuration](../crates/engram_core/src/config.rs),
[memory accounting](../crates/engram_core/src/memory.rs),
[retrieval backend selection](../crates/engram_index/src/hybrid.rs),
[embedding implementations](../crates/engram_ml/src/embed.rs),
[watcher](../crates/engram_server/src/actors/watcher.rs), and
[tool request arguments](../crates/engram_server/src/models/requests.rs).
