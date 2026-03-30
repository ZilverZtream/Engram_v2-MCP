# Engram v2 Audit — R5

## Section 1: Audit methodology
- Performed repository inventory commands exactly as requested:
  - `find . -name "Cargo.toml" | sort`
  - `git log --name-only -n 8`
  - `rg -c "#\[tokio::test\]|#\[test\]" --type rust`
  - `rg -l "ENGRAM_TEST_" --type rust`
- Read source paths directly for all subsystems listed in scope (embedder/vector/hybrid/docstore/registry/security/cancellation/ADP/jobs/namespaces/migration/parsers/memory/handlers).
- Counted tests and ignore markers with a direct Rust source scan.

Inventory summary:
- Crates:
  - `engram_core`: shared types, security, registry/checkpoint/memory/namespace policies.
  - `engram_git`: git ingestion/history support.
  - `engram_graph`: graph store + algorithms.
  - `engram_index`: parsing, chunking, Tantivy FTS, LanceDB vector, DocStore.
  - `engram_ml`: embedders (projection/local/ollama/openai).
  - `engram_server`: MCP handlers, actors, orchestration services.
- Compile-time features:
  - `engram_index`: `default = ["vector"]`, `vector` enables LanceDB/Arrow dependencies.
  - `engram_server`: `default = ["vector"]`, `vector = ["engram_index/vector"]`.
- Tests:
  - Total `#[test]` + `#[tokio::test]`: **2232**.
  - `#[ignore]`-marked: **0** detected in Rust sources.
- Env vars gating live tests:
  - `ENGRAM_TEST_OLLAMA_URL`, `ENGRAM_TEST_OLLAMA_MODEL`, `ENGRAM_TEST_OLLAMA_DIM` (Ollama live embedder tests).
  - `ENGRAM_TEST_OPENAI_KEY`, `ENGRAM_TEST_OPENAI_BASE_URL`, `ENGRAM_TEST_OPENAI_MODEL`, `ENGRAM_TEST_OPENAI_DIM` (OpenAI live embedder tests).
  - `ENGRAM_TEST_LOG` (test logging verbosity only).
- Last 8 commits were mapped in Section 5.

## Section 2: Findings by subsystem
### EMB1
- Severity: **Medium**
- Confidence: **Confirmed**
- Summary: Cancellation is cooperative only; in-flight HTTP requests cannot be aborted at socket level.
- Coverage status: **Covered-Insufficient**
- Revolving: Yes — architectural limitation of reqwest; requires per-request timeout config.

### EMB2
- Severity: **Low**
- Confidence: **Confirmed**
- Summary: Dimension/normalization/empty-text parity enforced; `build_embedder()` fails fast.
- Coverage status: **Covered-Insufficient** (env-gated live tests)

### VEC1
- Severity: **High**
- Confidence: **Confirmed**
- Summary: `open_or_create_table()` drop-and-recreate is destructive; no automatic reindex performed in vector layer; recovery must be mandatory and observable.
- Coverage status: **Covered-Insufficient**
- Fix needed: Couple recreate path to mandatory reindex tracking in registry.

### VEC2
- Severity: **Low**
- Confidence: **Confirmed**
- Summary: `merge_insert` now provides atomic upserts; dependent on LanceDB transaction guarantees.
- Coverage status: **Covered-Insufficient**

### FTS1
- Severity: **Medium**
- Confidence: **Confirmed**
- Summary: Regex mode passes user patterns to Tantivy with only length cap; expensive-but-short patterns still possible.
- Coverage status: **Covered-Insufficient**

### FTS2
- Severity: **Low**
- Confidence: **Confirmed**
- Summary: Deterministic secondary sort and MMR oversampling caps implemented.
- Coverage status: **Covered-Insufficient**

### DS1
- Severity: **Medium**
- Confidence: **Provisional**
- Summary: Deserialization is fail-closed, but cross-phase copy-forward behavior on mid-file hashing errors not proven.
- Fix needed: Trace copy_forward_unchanged + fingerprint error paths end-to-end.

### REG1
- Severity: **Low**
- Confidence: **Confirmed**
- Summary: Registry write paths consistently validate key components.
- Coverage status: **Covered-Insufficient**

### SEC1
- Severity: **Medium**
- Confidence: **Provisional**
- Summary: `safe_join()` strong, but 64-level ancestor walk for symlink-expanded realpaths partially proven.
- Revolving: Yes — needs adversarial symlink integration test.

### CANCEL1
- Severity: **Medium**
- Confidence: **Provisional**
- Summary: Core actors use shutdown tokens; exhaustive "all await-in-loops checked" not proven.
- Revolving: Partially — requires static lint or instrumentation sweep.

### ADP1
- Severity: **Low**
- Confidence: **Confirmed**
- Summary: Gate order fixed, ConfigSnapshot captures identity fields, kill-switch remains runtime-config driven.

### JOB1
- Severity: **Medium**
- Confidence: **Provisional**
- Summary: GC checks active_indexing_count; full race-proofing between GC deletion and in-flight checkpoint phases not demonstrated.
- Fix needed: Deterministic concurrency test with forced interleavings.

### NS1
- Severity: **Low**
- Confidence: **Provisional**
- Summary: GlobalMutable generation clamped to 0; concurrent writer last-write-wins not formally validated.
- Fix needed: Stress test with concurrent writes on identical pk.

### MIG1
- Severity: **Medium**
- Confidence: **Confirmed**
- Summary: Silent fallbacks (`.ok()` on regex compile, `Err(_) => continue` without logging) degrade evidence quality without hard failure or operator visibility.
- Critical sites:
  - Line 3365: `Err(_) => continue` with no warn — silent graph query failure skip
  - Lines 4849-4856: 5 VB regex patterns compiled with `.ok()` — if any fail, analysis silently zeroes out
  - Lines 9121, 9205: `Regex::new(&pattern).ok()?` — early return on regex compile failure, no log
  - Line 9526: `Regex::new(...).ok()` — config transform regex silently disabled
- Fix needed: Convert to explicit error logging at each fallback site.

### PARSE1
- Severity: **Low**
- Confidence: **Confirmed**
- Summary: Production extractors largely avoid panicking unwraps; `#![deny(clippy::unwrap_used)]` enforced at crate level.

### MEM1
- Severity: **Low**
- Confidence: **Confirmed**
- Summary: CAS-based `try_allocate` removes transient overcommit window; AllocationGuard is panic-safe.

### MCP1
- Severity: **Medium**
- Confidence: **Provisional**
- Summary: Many handlers clamp cardinalities and route paths through safe_join; full end-to-end coverage unproven.
- Revolving: Yes — requires mechanical handler enumeration.

## Section 3: Cross-subsystem interaction findings
- X1: Vector recreate ↔ DocStore generation: fail-closed present; full reindex auto-orchestration still external.
- X2: Remote embedder cancel ↔ memory guard: AllocationGuard held until HTTP await completes.
- X3: Registry validation ↔ MCP input: layered protection; registry is defensive.
- X4: ADP verdict ↔ job enqueue: no direct bypass confirmed.
- X5: GC cleanup ↔ in-flight job state: active_indexing_count guard lowers race risk.
- X6: Cancellation ↔ checkpoint phase: tombstone logic handles stale/notfound; residual risk in mid-phase writes.

## Section 4: Provider parity matrix
| Subsystem | Live-tested in CI | Confirmed gaps |
|-----------|------------------|----------------|
| ProjectionEmbedder | No | None major |
| OllamaEmbedder | Env-gated | In-flight HTTP cancel cooperative only |
| OpenAIEmbedder | Env-gated | Same |
| Tantivy FTS (all modes) | Yes | Regex complexity space only length-bounded |
| LanceDB vector | Yes (default feature) | Recreate path requires external full reindex |
| Hybrid search (FTS-only) | Partial | Feature-off parity not deeply stress-tested |
| Redb DocStore | Yes | copy-forward hash error semantics not fully proven |
| Redb Registry | Yes | Low |
| Redb Graph store | Yes | Low-medium race tests recommended |

## Section 5: Recent changes audit coverage
- 327930e: MEM CAS, VEC atomic merge_insert, watcher shutdown token, integration tests.
- d2a9ae4: VEC/X reindex signaling, FTS regex cap, ADP evidence hash, DS corrupt-bytes, EMB normalization.
- 804344f: EMB cancellable embed, VEC fail-closed recreate, MCP sanitization.
- fea0860: broad remediation across EMB/FTS/ADP/MIG/CANCEL/JOB/MEM/SEC/PARSE.
- ce58bd8: R1 hardening in registry/security/graph/index/server tests.
- 60abbdc: stale-cancel tombstone tests (S14-001).
- b78a7cd: deterministic lexical sort extension (S06-001).
- 757b325: mixed namespace rejection, timeout fail-closed, stale cancel tombstone.

## Section 6: Top critical blockers
1. **VEC1 operational**: Recreate must trigger mandatory tracked reindex; observable degraded-mode state until complete.
2. **MIG1 data quality**: Silent fallback sites in migration emit degraded outputs without hard failure or warning.
3. **CANCEL/GC race assurance**: Deterministic race tests for GC + checkpoint + active job interleavings.

## Section 7: Full findings inventory summary
- Confirmed defects/risks: EMB1, VEC1, FTS1, MIG1.
- Confirmed hardening states with residual caveats: EMB2, VEC2, FTS2, REG1, ADP1, PARSE1, MEM1.
- Provisional (needs deeper proof): DS1, SEC1, CANCEL1, JOB1, NS1, MCP1.

## Section 8: Rating
**8.5 / 10.0**

## Section 9: Score movement rules
- +0.3 to +0.6 if VEC recreate path coupled to mandatory reindex completion tracking.
- +0.2 to +0.4 if migration silent fallbacks converted to explicit typed errors.
- +0.2 to +0.4 if deterministic concurrency tests prove GC/checkpoint/cancel races closed.
- -0.3 to -0.8 for any regression reintroducing silent vector/docstore divergence or unsafe path handling.

## Section 10: What to retest after fixes
1. Force vector schema mismatch; verify automatic full reindex reaches parity before search marked healthy.
2. Fault-inject file read/hash failures in copy-forward; verify deterministic fail-open/fail-closed policy.
3. Stress cancellation during remote embedding with network delay; verify bounded cancel latency.
4. Run race harness: job phases + GC purge + cancellation tombstone concurrently.
5. Run MCP fuzzing for cardinality/path fields across all tools.

## Section 11: Findings rejected after test review
- "Unknown backend silently falls back to local" — contradicted by explicit `build_embedder` error tests.
- "Lexical search lacks deterministic tie-break" — contradicted by S06 implementation and behavioral tests.
- "Dimension mismatch only soft-fails" — contradicted by fail-closed tags/tests in vector path.
