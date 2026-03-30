# Engram v2 – Full Audit Prompt

You are performing a complete, single-pass correctness and security audit of **Engram v2**, a Rust MCP server for AI-assisted software analysis located at the root of this repository. Cover every subsystem. Do not split into parts. Do not stop until every subsystem below has been audited, all cross-subsystem interactions have been swept, and the full report is written.

---

## Step 0 – Repository inventory (do this first, record in Section 1)

```
find . -name "Cargo.toml" | sort
git log --name-only -n 8
rg -c "#\[tokio::test\]|#\[test\]" --type rust
rg -l "ENGRAM_TEST_" --type rust
```

Record:
- All crate names and their purpose.
- Total `#[test]` + `#[tokio::test]` count and how many are `#[ignore]`-marked.
- All environment variables that gate live tests and which backends they cover.
- Compile-time feature flags (`vector` in `engram_index`/`engram_server`).
- Last 8 commits: map each changed file to a subsystem.

---

## Step 1 – Audit every subsystem below

Read the actual source files. Do not guess. For each subsystem, trace critical paths end-to-end and record any finding using the finding schema at the bottom.

### Embedder backends — `crates/engram_ml/src/embed.rs`
- Does `Embedder::embed_batch()` accept a `CancellationToken`? If not, can an in-flight HTTP call to Ollama or OpenAI be interrupted after it starts?
- Do all four impls (`ProjectionEmbedder`, `LocalEmbedder`, `OllamaEmbedder`, `OpenAIEmbedder`) satisfy the dimension contract, L2 normalization guarantee, and empty-text behavior identically?
- Does `build_embedder()` fail fast on missing api_key, zero dim, or unknown backend — or does it fail silently later?
- Which tests cover each backend? Are any env-gated? What is exercised in standard CI with no env vars set?

### Vector store — `crates/engram_index/src/vector.rs`
- Read `open_or_create_table()` fully. On dimension mismatch or missing `pk` column: is data loss silent? Is there a pre-flight warning the caller can act on? Is a re-index automatically triggered?
- Are partial batch upserts rolled back on failure, or left partially committed?
- After a table drop-and-recreate, are the DocStore generation counter and the new vector table consistent, or is there a window where searches return degraded results silently?
- Does the `ENG-AUD-2026-0005` fail-closed path cover all code routes that could produce a dimension mismatch (not just the one tested in `dimension_mismatch_fails_closed`)?

### Full-text index and hybrid search — `crates/engram_index/src/hybrid.rs`, `tantivy_index.rs`
- In `regex` FTS mode: is raw user input passed to the Tantivy regex engine without escaping? What happens on a malformed or catastrophically backtracking regex?
- If the `vector` feature is compiled out or the LanceDB table is empty, does hybrid search degrade gracefully (FTS-only) or panic/return misleading scores?
- Is the deterministic secondary sort (S06-001) applied to all search entry points, including `lexical_search`?
- Is the MMR oversampling multiplier bounded to prevent OOM on a corpus smaller than `top_k * multiplier`?

### DocStore — `crates/engram_index/src/docstore.rs`
- In the bincode → JSON fallback deserialization path: can a corrupted record cause a panic or silent data loss?
- In copy-forward logic: if blake3 hashing errors mid-file, does the code fail open (treats file as changed) or fail closed (aborts)?
- Does `KeepLatestOnly` retention actually delete prior-generation chunks, or can orphaned chunks accumulate?

### Registry and key validation — `crates/engram_core/src/registry.rs`
- Does `validate_key_component()` reject every byte used as a composite key delimiter (`\0`, `\n`)? Are there composite key construction sites that bypass this call?
- Are project_id and section_id values sourced directly from MCP tool call arguments validated before they reach registry writes?
- Is the TOCTOU-safe `symlink_metadata()` pattern used consistently at every stat site, or are there remaining `exists()` + `metadata()` pairs?

### Security boundary — `crates/engram_core/src/security.rs`
- Can the 64-level ancestor walk limit be bypassed by a symlink that resolves to a path with more than 64 components?
- Does `safe_join()` correctly reject all Windows path edge cases: drive-letter prefixes, `\\?\` UNC, `\\server\share`, device paths (`\\.\`), and null bytes?
- Does symlink rejection walk every component of the path, not just the final segment?
- Is there any caller that reads or writes files by going directly to `std::fs` without passing through `PathContext`?

### Cancellation propagation — `crates/engram_index/src/hybrid.rs`, `crates/engram_server/src/actors/`
- Are there any `.await` points inside loops that do not check `cancel.is_cancelled()` at the loop boundary? (Check all 249+ `CancellationToken` sites.)
- Do background actors (watcher, dreamer, immune, gc) exit cleanly on cancellation without leaving mutexes held, channels wedged, or Redb transactions open?
- Does the S14-001 stale-cancel tombstone contract hold: once a job is cancelled and a tombstone is written, can a restart incorrectly resume it?

### ADP decision service — `crates/engram_server/src/services/autonomous_decision_service.rs`
- Does `ConfigSnapshot` capture enough to reproduce a verdict deterministically? What is absent (gate implementation version, crate semver, runtime evidence hash)?
- Is the 8-gate evaluation order fixed in code, or can config reorder gates?
- Is the emergency kill-switch sticky across restarts (persisted), or does it reset on process restart?
- What test asserts the ≤ 1% false-allow rate target? Is the threshold verified against a confusion matrix, or just asserted against a handful of scenarios?

### Job orchestration and checkpoint recovery — `crates/engram_server/src/services/job_service.rs`, `lifecycle_service.rs`, `crates/engram_core/src/checkpoint.rs`
- If a job is killed mid-phase and restarted, does it re-enter the same phase cleanly, or can it double-write a phase's output?
- Can a later phase start if an earlier phase did not complete and write its checkpoint?
- Can the GC actor delete a job's Redb state while the job is still running (gc.rs vs. job_service.rs race)?

### Namespace and primary-key construction — `crates/engram_core/src/ids.rs`, `namespaces.rs`
- For `GlobalMutable` namespaces, generation is clamped to `0` in `build_pk()`. Is last-write-wins semantics documented and tested for concurrent writers to the same doc_id?
- Do all 6 namespace policies enforce their retention semantics end-to-end: `KeepLatestOnly`, `KeepLastGenerations(N)`, `KeepForever`?
- Is there any pk construction site that bypasses `build_pk()` and constructs the pk string directly?

### Full project migration service — `crates/engram_server/src/services/full_project_migration_service.rs`
- Audit error propagation: are `?` operators used uniformly, or are some errors swallowed by `.unwrap_or_default()`, broad `_ => {}` arms, or silent logging?
- Is cancellation safe throughout: if the token fires mid-migration, does the project end in a recoverable state or a partially migrated one with no rollback path?
- Is migration progress state written atomically, or can it be observed in a partial state by a concurrent reader?

### Parser extractors — `crates/engram_index/src/asp_classic_extractor.rs`, `vb_extractor.rs`, `js_extractor.rs`, `sql_parser.rs`
- Are there any `.unwrap()` calls on tree-sitter node children that would panic on malformed input?
- Is there unbounded recursion in any extractor that a deeply nested input could exploit?
- Are extracted SQL strings stored as data only, never interpolated into Redb keys or queries?
- Is `#![deny(clippy::unwrap_used)]` enforced in extractor files, or bypassed by `#[allow(...)]` file headers?

### Memory budget and backpressure — `crates/engram_core/src/memory.rs`
- Under concurrent indexing load, can two tasks each allocate up to the soft limit, together exceeding the hard limit before either is rejected?
- Does `AllocationGuard` drop correctly if the task holding it panics (i.e., is deallocation panic-safe)?
- Are there allocation sites that bypass `AllocationGuard` and write directly to Tantivy/LanceDB without budget accounting?

### MCP tool handler dispatch — `crates/engram_server/src/handlers/`, `tools.rs`
- Are all handler input fields validated before reaching storage or search layers, or can a malformed MCP request bypass `validate_key_component()` / `PathContext`?
- Is there a handler that accepts a file path from the MCP caller without routing it through `PathContext`?
- Can a tool call trigger an unbounded allocation (e.g., `top_k` with no upper bound passed to hybrid search)?

---

## Step 2 – Cross-subsystem interaction sweep

After completing Step 1, check these interaction pairs and record any amplifying interactions as **X-findings**:

- Vector table drop-and-recreate (VEC) ↔ DocStore generation counter consistency
- Remote embedder non-interruptible batch (EMB) ↔ memory budget RAII guards held across HTTP await
- Registry key validation ↔ MCP tool input path (end-to-end: MCP → handler → registry)
- ADP gate verdicts ↔ job orchestration enforce path (can a deny verdict be bypassed by a direct job enqueue?)
- GC actor cleanup timing ↔ in-flight job Redb state (read-then-delete race)
- Cancellation token cancellation ↔ checkpointed phase state (partial phase written before cancel token fires)

---

## Step 3 – Provider parity matrix

Populate Section 4 with this table:

| Subsystem | Live-tested in CI | Mocked/unit-tested | Confirmed gaps |
|-----------|------------------|-------------------|----------------|
| ProjectionEmbedder (local) | | | |
| OllamaEmbedder (remote) | | | |
| OpenAIEmbedder (remote) | | | |
| Tantivy FTS (`strict` mode) | | | |
| Tantivy FTS (`loose` mode) | | | |
| Tantivy FTS (`regex` mode) | | | |
| LanceDB vector (`vector` feature on) | | | |
| Hybrid search (vector feature off / FTS-only degraded) | | | |
| Redb DocStore | | | |
| Redb Registry | | | |
| Redb Graph store | | | |

---

## Output format

Write the complete audit as a single document using exactly these sections:

```
# Engram v2 Audit

## Section 1: Audit methodology
## Section 2: Findings by subsystem
## Section 3: Cross-subsystem interaction findings
## Section 4: Provider parity matrix
## Section 5: Recent changes audit coverage
## Section 6: Top critical blockers
## Section 7: Full findings inventory summary
## Section 8: Engram v2 rating
## Section 9: Score movement rules
## Section 10: What to retest after fixes
## Section 11: Findings rejected after test review
## Section 12: Open risks (not yet confirmed or refuted)
```

---

## Finding schema

Every finding in Section 2 must use this exact format:

```
### <ID>
- Severity: **Critical | High | Medium | Low**
- Confidence: **Confirmed | Provisional**
- Backend impact: **<crates / features / backends affected>**
- Subsystem: <subsystem name>
- Summary: <one sentence>
- Evidence: <file path(s) and line numbers>
- Trigger: <what condition causes this>
- Impact: <what goes wrong and for whom>
- Why tests might miss: <why existing tests do not catch this>
- Coverage status: **Covered-Insufficient | Uncovered** (<test file> lines <N–M>)
- Regression risk if fixed naively: Critical | High | Medium | Low
```

For Provisional findings add:
```
- Needed proof to confirm/refute: <specific test or code path to check>
```

---

## Rules

- **Read before writing.** Do not write a finding without reading the triggering code path end-to-end.
- **Reject if already covered.** If a test covers a concern adequately, record the rejection in Section 11 with the contradicting test file and line numbers.
- **No style findings.** Do not report missing docs, clippy style warnings, or naming issues unless they directly cause a correctness or security defect.
- **Rating scale: / 10.0** — consistent with the `engram-validator` output already used in this repo.
- **Finding IDs:** subsystem prefix + number: `EMB1`, `VEC1`, `FTS1`, `DS1`, `REG1`, `SEC1`, `CANCEL1`, `ADP1`, `JOB1`, `NS1`, `MIG1`, `PARSE1`, `MEM1`, `MCP1`. Cross-subsystem: `X1`, `X2`, etc.
