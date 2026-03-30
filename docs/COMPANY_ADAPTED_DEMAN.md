# Engram v2 — Company-Adapted Demands (DEMAN)

Generated from R5 audit. These are the requirements to close the gap between
Team-ready (current) and Company-adapted.

---

## D1 — VEC1: Mandatory Reindex Tracking (High priority)

**Demand**: When a vector table is recreated due to schema mismatch, the system
must durably record that a full reindex is required and make this state observable
to operators and search callers until reindex completes.

**Acceptance criteria**:
- `ProjectRecord` carries `reindex_required_since_ms: Option<u64>`.
- Flag is set in the registry when `FullReindexRequired` event is processed.
- Flag is cleared when a full index job completes successfully for the project.
- Search tool responses surface a degraded-mode warning when flag is set.
- At least one test verifies the flag lifecycle: set → indexed → cleared.

**Evidence**: `crates/engram_core/src/registry.rs` ProjectRecord; `crates/engram_server/src/actors/dreamer.rs` FullReindexRequired handler; `crates/engram_server/src/handlers/project_tools.rs` job completion path.

---

## D2 — MIG1: Migration Silent Fallbacks (Medium priority)

**Demand**: All silent fallback sites in `full_project_migration_service.rs` that
can emit degraded data without operator visibility must either log a warn/error at
the fallback site or propagate the failure explicitly.

**Critical sites**:
- Line 3365: `Err(_) => continue` — graph query failure silently skips method nodes; must log.
- Lines 4849-4856: 5 VB regex `.ok()` patterns — if ANY fail, silent zero-out; must log compile failure.
- Lines 9121, 9205: `Regex::new(&pattern).ok()?` — early return on compile failure; must log.
- Line 9526: `Regex::new(...).ok()` — config transform regex disabled silently; must log.

**Acceptance criteria**:
- Every `.ok()` on a `Regex::new()` call either has a `tracing::warn!` on the None branch or is proven infallible (static pattern).
- Every `Err(_) => continue` in migration graph/node traversal logs the skip reason.
- Tests verify that fallback paths emit at least one tracing event.

---

## D3 — JOB1: GC/Checkpoint Race Proof (Medium priority)

**Demand**: Prove with a deterministic concurrency test that GC purge and
in-flight job checkpoint writes do not race to delete live state.

**Acceptance criteria**:
- Test that starts a job, forces a GC tick at the same time, and verifies:
  - Active job's checkpoint is not deleted by GC.
  - Job completes successfully after GC tick.
  - GC counter guard (`active_indexing_count`) is respected.
- At least one test injects GC while `active_indexing_count > 0` and verifies skip.

---

## D4 — NS1: GlobalMutable Concurrent Write Test (Low priority)

**Demand**: Prove that concurrent writes to the same `doc_id` in a GlobalMutable
namespace result in deterministic last-write-wins behavior without data corruption.

**Acceptance criteria**:
- Stress test spawns N concurrent `index_docs` calls for the same pk in a
  `memory_bank` or `insights` namespace.
- After all writers complete, exactly one row exists per pk (no duplicates).
- No errors or panics during concurrent writes.

---

## D5 — DS1: Copy-Forward Hash Failure Semantics (Medium priority)

**Demand**: Document and test the behavior of the copy-forward indexing path when
fingerprint computation fails mid-file.

**Acceptance criteria**:
- Test injects a read/hash failure during `copy_forward_unchanged` (or equivalent).
- Behavior is explicit: either the file is re-indexed from source (fail-open) or
  the error is propagated (fail-closed), documented in code.
- No silent data loss: skipped files must be logged.

---

## D6 — EMB1: Per-Request Timeout (Medium priority, operational)

**Demand**: Remote embedder HTTP calls must have a bounded maximum duration so
that cancellation latency is guaranteed even under network stalls.

**Acceptance criteria**:
- `OllamaEmbedder` and `OpenAIEmbedder` HTTP clients are built with a
  `timeout(Duration)` matching a configurable `embedding_request_timeout_secs`
  config field (default: 30s).
- Cancel latency is bounded to at most `timeout + retry_delay` under any network
  condition.
- Test verifies that a timed-out request returns `Err` within the expected window.

---

## D7 — CANCEL1: Exhaustive Await-Loop Coverage (Medium priority)

**Demand**: Assert that every looped `await` in index/hybrid paths checks the
cancellation token at least once per iteration.

**Acceptance criteria**:
- Static annotation or grep-verified invariant: every `loop { ... .await ... }`
  block in `hybrid.rs`, `ingest.rs`, and `job_service.rs` contains a
  `cancel.is_cancelled()` or `select!` check.
- New test: verify that a tight indexing loop (100-file batch) can be cancelled
  within 2 iterations of the first cancelled check.

---

## Revolving Non-Actionable Items

These will continue to appear in audits but have no in-code fix:

| ID | Reason |
|----|--------|
| SEC1 | Needs adversarial symlink integration test to confirm/refute |
| MCP1 | Requires mechanical handler enumeration script |
| ADP1 (kill-switch) | Deployment/ops concern; no code fix warranted |

---

## Company-Adapted Threshold

The code reaches Company-adapted when D1, D2, and D3 are closed and at least
two of D4, D5, D6 are closed. D7 is a stretch goal.

Current score: **8.5/10**
Target score: **9.2–9.5/10**
