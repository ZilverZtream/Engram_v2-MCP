# P0 — Core Agent Loop Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement TODO.md items 1–8 (P0 tier): line numbers + honest snippets in search, honest semantic degradation, staleness metadata + freshness tool, incremental watcher cost fix, actionable errors, search→graph ID bridge, response footers, and the 114-tool description sweep.

**Architecture:** All changes are additive and keep the existing generation/namespace model intact. Engine-level fields are added to `HybridHit`; handler-level output gains a standard one-line footer; the watcher's per-save cost drops from O(repo bytes) to O(repo stats) by trusting mtime+size; new tools (`get_index_freshness`, `resolve_id`) follow the existing tools.rs → requests.rs → handler → capabilities.rs pattern.

**Tech Stack:** Rust 2024, rmcp 0.3 (`#[tool]` router), Tantivy 0.24, LanceDB, Redb registry.

**Verification commands (Windows, Git Bash):**
- Format: `cargo fmt --all`
- Compile: `cargo check --all-targets` (workspace; ~minutes incremental)
- Targeted tests: `cargo test -p engram_server --test hybrid_search_behavioral_tests`, `cargo test -p engram_index`, `cargo test -p engram_server --test registry_lifecycle_tests`
- Known pre-existing failure (ignore): `utils::files::escaped_meta_chars_are_literal`

**Scoping decisions locked here:**
- Item 2: we do the "degrade honestly" branch (label trigram projection as degraded everywhere; no ranking change, no bundled model — that's P2-adjacent and risky to ranking stability).
- Item 3: full description rewrite + `[WebForms/.NET]` tagging + alias deprecation notes. No dynamic per-project tool gating (depends on P2 #24; MCP tool lists are static per server).
- Item 4: envelope = standard one-line footer + labeled truncation markers on the hot read tools (search_memory, vector_search, get_chunk, find_symbol_references, grep_project). Not all 193 handlers — the rest follow the documented convention later.
- Item 6: watcher stays full-project-trigger (update path is already fingerprint-incremental); the fix is (a) stop re-hashing every file whose mtime+size match (`verify_unchanged_hashes` config, default false = trust stat), (b) configurable debounce. The remaining per-generation copy-forward cost is the git-perf-plan's territory (TODO #36), not P0.

---

### Task 1: HybridHit line numbers + labeled snippet truncation

**Files:**
- Modify: `crates/engram_index/src/hybrid.rs` (HybridHit struct ~line 33; lexical_search ~1786; lexical_search_with_content ~1827; vector_search ~2192; search ~2484; pure_vector_search ~2219)
- Modify: `crates/engram_server/src/handlers/search_tools.rs` (handle_search_memory ~166; handle_vector_search ~246)
- Test: `crates/engram_server/tests/hybrid_search_behavioral_tests.rs`

- [ ] **Step 1.1: Write failing test** — in hybrid_search_behavioral_tests.rs (follow the file's existing engine-construction pattern): index a doc with `start_line: 41, end_line: 60` and >600 chars of content, run `lexical_search`, assert `hit.start_line == 41 && hit.end_line == 60`, assert `hit.snippet_truncated == true` and snippet length ≤ ~520. Second doc with short content asserts `snippet_truncated == false`.
- [ ] **Step 1.2: Run it** — `cargo test -p engram_server --test hybrid_search_behavioral_tests` → FAIL (no such fields).
- [ ] **Step 1.3: Engine changes:**
  - `HybridHit` gains `pub start_line: u32, pub end_line: u32, pub snippet_truncated: bool`.
  - New helper in hybrid.rs:
    ```rust
    /// Cut at a line boundary at or below `max_chars`; falls back to a char
    /// boundary when the first line alone exceeds the budget.
    pub(crate) fn snippet_of(content: &str, max_chars: usize) -> (String, bool) { ... }
    ```
    with `const SNIPPET_MAX_CHARS: usize = 500;`
  - `lexical_search`: read `fields.start_line`/`fields.end_line` (same `get_first(...).as_u64()` pattern as copy_generation_for_paths line ~934), use `snippet_of` instead of `.chars().take(300)`.
  - `lexical_search_with_content`: populate the same fields on its HybridHit.
  - `vector_search` (~2192): `start_line: 0, end_line: 0, snippet_truncated: false` (LanceDB schema has no line columns — do NOT touch the lance schema; recreation drops data).
  - `search()` post-merge, after truncate to top_k: for hits with `start_line == 0 && end_line == 0`, enrich via `self.get_doc_by_pk(&hit.pk)` → fill lines + snippet (`snippet_of`). Bounded by top_k.
  - `pure_vector_search`: same enrichment loop on its final list.
  - Fix every other `HybridHit { ... }` construction the compiler finds (grep `HybridHit {`).
- [ ] **Step 1.4: Handler changes:**
  - `handle_search_memory` per-hit header becomes `#N\ndoc_id/chunk_id/path/lines: S-E/score`. The `include_content` truncation marker becomes `"... [truncated at {limit} chars — call get_chunk(doc_id) for the full chunk]"`. Snippet path appends `" ... [snippet truncated — call get_chunk(doc_id) for the full chunk]"` when `snippet_truncated`.
  - `handle_vector_search`: print `lines=S-E` per hit (fields are now enriched by pure_vector_search).
- [ ] **Step 1.5: Run tests** → PASS; `cargo check --all-targets` clean.
- [ ] **Step 1.6: Commit** — `feat(search): line numbers on every hit + labeled line-boundary snippet truncation`

### Task 2: Honest semantic degradation labeling

**Files:**
- Modify: `crates/engram_index/src/hybrid.rs` (engine has `self.embedding_backend: String`)
- Modify: `crates/engram_server/src/handlers/search_tools.rs`, `crates/engram_server/src/main.rs` (startup warn), capabilities/get_capabilities handler (locate via grep), `crates/engram_server/src/tools.rs` (vector_search/search_memory descriptions get the honest note as part of Task 7)
- Test: `crates/engram_index/src/hybrid.rs` unit test (mod tests)

- [ ] **Step 2.1:** Engine method + unit test first:
  ```rust
  pub enum SemanticQuality { Semantic(&'static str), DegradedTrigram, Off }
  pub fn semantic_quality(&self) -> SemanticQuality {
      match self.embedding_backend.as_str() {
          "ollama" => SemanticQuality::Semantic("ollama"),
          "openai" => SemanticQuality::Semantic("openai"),
          "fts_only" => SemanticQuality::Off,
          _ => SemanticQuality::DegradedTrigram, // local / candle / ""
      }
  }
  ```
  Test maps all five backend strings.
- [ ] **Step 2.2:** `handle_search_memory` (when `req.semantic`) and `handle_vector_search`: when `DegradedTrigram`, prepend one line:
  `NOTE: vector half of this search uses a non-semantic trigram-projection embedder (default install). Lexical quality is unaffected. For true semantic search set embedding_backend=ollama|openai in engram_mcp.yaml.`
  When `Off` in search_memory: `NOTE: vector search disabled (fts_only) — results are lexical-only.`
- [ ] **Step 2.3:** get_capabilities output gains `semantic_search: true|degraded(trigram)|off`. main.rs startup: `tracing::warn!` once when backend is local/empty.
- [ ] **Step 2.4:** `cargo test -p engram_index` + check. Commit — `feat(search): honest semantic-quality labeling for the default trigram embedder`

### Task 3: Staleness metadata, `get_index_freshness`, response footer

**Files:**
- Modify: `crates/engram_server/src/services/ingest_service.rs` (process_ingest_stats — single spot both index & update flow through)
- Create: `crates/engram_server/src/utils/envelope.rs` (+ wire in utils/mod.rs)
- Modify: `crates/engram_server/src/models/requests.rs` (GetIndexFreshnessRequest), `handlers/project_tools.rs` (handler), `tools.rs` (registration), `capabilities.rs` (flag)
- Modify: `handlers/search_tools.rs`, `handlers/grep_tools.rs` (footer application)
- Test: `crates/engram_server/tests/registry_lifecycle_tests.rs` pattern → new asserts; envelope unit test in envelope.rs

- [ ] **Step 3.1:** In `process_ingest_stats`, after success: `reg.set_meta(pid, "last_index_completed_ms", &now_ms.to_string())` and `"last_index_files"` = stats.all_files.len(). (spawn_blocking, same pattern as active_generation writes.)
- [ ] **Step 3.2:** `envelope.rs`:
  ```rust
  /// One-line, token-cheap trailer for read-tool responses.
  pub fn footer(generation: u64, last_index_ms: Option<u64>) -> String {
      // "\n---\n[engram] gen=42 | indexed 3m ago | stale? call get_index_freshness"
      // age rendered as s/m/h/d; "never indexed" when None
  }
  ```
  Unit tests: age rendering boundaries (59s, 61s, 26h, None).
- [ ] **Step 3.3:** `get_index_freshness` tool. Request `{ project_id, check_disk: bool = true }`. Handler returns: active_generation, last_index_completed (age), watcher enabled (`registry.list_watches`), `reindex_required_since_ms`, and when check_disk: count of tracked-extension files with mtime > last_index (iter_files + stat in spawn_blocking), plus an advice line (`fresh` / `run update_project` / `enable watch_project`). Register in tools.rs + capabilities.rs.
- [ ] **Step 3.4:** Append `envelope::footer` to: handle_search_memory, handle_vector_search, handle_get_chunk, handle_find_symbol_references, grep handler. Fetch `last_index_completed_ms` once per request via registry meta read.
- [ ] **Step 3.5:** Tests run; commit — `feat(freshness): last-index metadata, get_index_freshness tool, standard response footer`

### Task 4: Watcher cost — trust stat, configurable debounce

**Files:**
- Modify: `crates/engram_core/src/config.rs` (two fields + defaults)
- Modify: `crates/engram_server/src/services/project_service.rs` (get_incremental_changes ~267)
- Modify: `crates/engram_server/src/actors/watcher.rs` (~line 32)
- Test: unit tests in project_service.rs

- [ ] **Step 4.1:** Config: `watch_debounce_secs: u64` (default 5), `verify_unchanged_hashes: bool` (default false) with serde defaults + doc comments explaining the tradeoff (trust mtime+size like git's stat cache; set true to Blake3-verify every stat-matching file as before).
- [ ] **Step 4.2:** Extract pure decision helper + tests FIRST:
  ```rust
  pub(crate) fn stat_match_is_trustworthy(stored_mtime: u64, stored_size: u64) -> bool {
      stored_mtime != 0 && stored_size != 0
  }
  ```
  In get_incremental_changes: when `stored_mtime == mtime && stored_size == size`:
  - if `!cfg.verify_unchanged_hashes && stat_match_is_trustworthy(..)` → unchanged, NO stream_hash;
  - else keep existing hash-verify branch (incl. Fix 2.3 >100MB semantics).
  Tests: trustworthy yes/no for zero-mtime/zero-size; (behavioral hash-skip is covered by the existing `incremental_no_changes_is_zero.rs` still passing).
- [ ] **Step 4.3:** watcher.rs: `let debounce_duration = Duration::from_secs(state.cfg.watch_debounce_secs.max(1));`
- [ ] **Step 4.4:** `cargo test -p engram_server --test incremental_no_changes_is_zero` + unit tests; commit — `perf(watcher): trust mtime+size for unchanged files (opt-in re-hash), configurable debounce`

### Task 5: Actionable error messages

**Files:**
- Modify: `crates/engram_server/src/error.rs`, `crates/engram_server/src/services/project_service.rs` (validate_project_id, ensure_project_record error), `handlers/search_tools.rs` (no_hits, get_chunk not-found)
- Test: `crates/engram_server/tests/mcp_contract_enforcement_tests.rs` (existing validate_project_id tests must still pass; extend asserts for hint text)

- [ ] **Step 5.1:** `EngramError::ProjectNotFound` display → `"Unknown project_id: '{0}'. Call list_projects to see indexed projects, or index_project to index a new directory."` Check `ensure_project_record` produces ProjectNotFound (not a bare Internal).
- [ ] **Step 5.2:** `no_hits` response becomes:
  ```
  result: no_hits
  hints: try fts_mode="loose" (OR-of-terms) or a broader query; check namespace (default "memory"); call get_index_freshness to verify the index is current.
  ```
  (keep first line verbatim — tests grep for it).
- [ ] **Step 5.3:** get_chunk not-found: append `" doc_ids are generation-scoped; re-run search_memory to get current doc_ids."`
- [ ] **Step 5.4:** Run mcp_contract_enforcement_tests; commit — `feat(errors): recovery hints on the agent-facing error paths`

### Task 6: Search→graph ID bridge (`symbols:` per hit + `resolve_id` tool)

**Files:**
- Modify: `handlers/search_tools.rs` (enrichment in handle_search_memory)
- Create handler fn in `handlers/graph_tools.rs` (resolve_id), modify `models/requests.rs`, `tools.rs`, `capabilities.rs`
- Test: pure overlap helper unit test in search_tools.rs or utils

- [ ] **Step 6.1:** Pure helper + test first: `fn line_ranges_overlap(a: (u32,u32), b: (u32,u32)) -> bool` (inclusive, zero-tolerant).
- [ ] **Step 6.2:** In handle_search_memory after hits final: one spawn_blocking — for each distinct hit path, `graph.query_nodes(pid, None, None, Some(path), 200)`; per hit attach up to 3 nodes whose (start_line,end_line) overlap the chunk range, rendered as `symbols: Name (function) node_id=...; ...`. Skip silently when graph empty.
- [ ] **Step 6.3:** `resolve_id` tool. Request `{ project_id, id, namespace = "memory" }`. Resolution order: (1) `graph.get_node(pid, id)` exact node_id; (2) `resolve_symbol` (store.rs ~1022 — reuse; surfaces ambiguity list!); (3) `search.get_doc_by_doc_id(pid, ns, gen, id)` for doc_id. Output every identity found: node_id, name, type, fqn-ish name, file, lines, plus "use with:" hints (find_symbol_references / get_chunk / compute_blast_radius). Ambiguous → list candidates, say so explicitly.
- [ ] **Step 6.4:** check + targeted tests; commit — `feat(identity): symbols on search hits + resolve_id bridging doc_id/node_id/name`

### Task 7: Tool description sweep (114 tools)

**Files:** `crates/engram_server/src/tools.rs` only (attribute strings).

- [ ] **Step 7.1:** Rewrite every `#[tool(description = ...)]` to the format: *what it returns* + *when to use* + *key default/param* + *related tool*. ≤ ~220 chars each (token cost is per-session for every connected agent). Prefix the WebForms/.NET-legacy-only tools (migration suite, viewstate, postback, VB traps, ASPX, ADO, jQuery, GIS, SP/trigger tools) with `[.NET legacy]`. Mark aliases: `trigger_rem_cycle` → "Alias of dream_project; prefer dream_project." Honest notes on shallow tools (`analyze_error_stack`: "heuristic — builds a search query from the trace").
- [ ] **Step 7.2:** `cargo check -p engram_server` (attribute strings only); spot-check `tools/list` JSON via existing tests if any assert on descriptions.
- [ ] **Step 7.3:** Commit — `docs(tools): rewrite all tool descriptions for agent tool-selection; tag .NET-legacy surface`

### Task 8: Final verification + TODO bookkeeping

- [ ] `cargo fmt --all` → clean diff; `cargo check --all-targets` → 0 errors.
- [ ] Run the touched test suites: hybrid_search_behavioral_tests, registry_lifecycle_tests, mcp_contract_enforcement_tests, incremental_no_changes_is_zero, engram_index unit tests.
- [ ] Tick P0 boxes in TODO.md (items fully done; note scoping deltas inline: #4 partial=hot tools, #6 stat-trust approach).
- [ ] Commit — `chore(p0): mark P0 core-agent-loop items complete in TODO.md`

## Self-review notes
- Spec coverage: items 1→T1, 2→T2, 3→T7, 4→T3 (footer+markers, scoped) + T1 markers, 5→T3, 6→T4, 7→T5, 8→T6. All eight covered.
- Type consistency: `HybridHit.start_line/end_line: u32` matches Tantivy u64→u32 casts used elsewhere; `snippet_of` returns (String, bool) consumed by both lexical paths and enrichment.
- Risk watch: HybridHit is constructed in multiple places (grep before compiling); LanceDB schema intentionally untouched; `no_hits` first line preserved for test compat.
