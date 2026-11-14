# Engram TODO — Ordered by Value to an AI Agent

Engram's product is *an AI model's working memory of a codebase*. So every item below is
ranked by one question: **how much does this improve the answers an agent gets back, per
token spent, per call made?** Tiers are strictly ordered; items within a tier are ordered
too. File references come from a full-codebase review (2026-06-12); verify line numbers at
implementation time.

Problem descriptions and tool counts from that review are historical context.
Use each item's current delivery notes and linked receipts for implementation and
validation status. Checked or partial items retain their stated limits; remaining
work is not implied complete by an available tool name.

Effort: **S** = hours, **M** = days, **L** = a week+.

---

## P0 — Fix the core agent loop (search → understand → edit)

These degrade or improve *every single interaction*. Nothing below matters until these are right.

- [x] **0. Escape the apostrophe in `escape_tantivy_literal` — NL queries with contractions hard-error** (S) — *original fix22a5668 already shipped; acceptance completed 2026-11-11. The regression now requires matching documents for contractions and possessives through lexical and full hybrid search in loose and strict modes, with native and no-default-features runs passing. Twelve installed public search cases recovered the expected independent source bytes/hash/identity. Production escaping code was unchanged. See eval/p00-apostrophe-delivery-20261111.json.*
  `engram_index/src/hybrid.rs` escapes the apostrophe before passing literals to Tantivy. `engram_server/tests/hybrid_search_behavioral_tests.rs` covers the original failure: a contraction must neither parse-error nor abort the hybrid search before its vector arm runs.

- [x] **1. Return line numbers and untruncated snippets from search** (M) — *done 2026-06-12: HybridHit carries start/end_line + snippet_truncated; vector hits backfilled from Tantivy; truncation markers name get_chunk.*
  `engram_index/src/hybrid.rs`, `handlers/search_tools.rs`. Search hits omit
  `start_line`/`end_line` and truncate snippets at ~300 chars. An agent that can't jump
  straight to the location must spend a second tool call (and re-read the file) for every
  hit. Every result should carry: path, line range, symbol name + kind, full chunk text
  (or an explicit `truncated: true` + a handle to fetch the rest).

- [x] **2. Ship real semantic embeddings by default — or degrade honestly** (M) — *done 2026-06-12 (degrade-honestly branch): SemanticQuality labeling in search/vector/project_health + startup warning; also fixed embedding_backend=ollama being rejected at engine construction. Bundling a real local model remains open (see P2).*
  November 13: the original lexical-ranking fallback was still missing despite
  the disclosure delivery. Source now bypasses projection fusion/MMR; public
  debug, candidate and installed checks pass, including absent identifiers and
  lexical-equivalent pages. Current OciusX uses Ollama and its ranking is
  unaffected; no ranking gain is claimed. The paired deployment also repairs
  captured VB instruction evidence; see
  `eval/ociusx-vb-instruction-evidence-delivery-20261113.json`.
  `engram_core/src/config.rs`, `engram_ml/`. The default `local` backend is a
  non-semantic projection/trigram embedder, so default installs get vector search that
  *looks* like it works but returns noise — worse than no vector search, because hybrid
  fusion dilutes good lexical hits with it. Either bundle/auto-download a small real model
  (e.g. a quantized MiniLM/bge via ONNX or fastembed), or detect the fallback and (a) set
  hybrid weights to ~100% lexical, (b) report `"semantic_search": "degraded"` in every
  search response and in `get_capabilities`.

- [x] **3. Tame the 114-tool surface** (M) — *done 2026-06-12: all descriptions rewritten (returns + when-to-use + related tool), 33 tools tagged [.NET legacy], trigger_rem_cycle marked deprecated alias, ServerInfo.instructions now a which-tool-when guide. Dynamic per-project gating deferred to P2 #24.*
  `engram_server/src/tools.rs`, `capabilities.rs`. 114 tools with terse descriptions is
  beyond what any model selects among reliably; the best tools get lost. Three moves:
  (a) rewrite every description to state *when to use it, what it returns, what to use
  instead* — descriptions are the only marketing a tool gets; (b) consolidate aliases and
  near-duplicates (e.g. `dream_project`/`trigger_rem_cycle`); (c) gate the ~40
  WebForms/migration tools behind project type so a Rust project sees ~50 relevant tools,
  not 114 (see #28).

- [~] **4. Uniform response envelope: token budgets, truncation metadata, pagination** (L) — *partial: deployed 2026-11-11 for search_memory, vector_search and grep_project. Optional max_tokens estimates ceil(UTF-8 complete reply-text bytes / 4), pages whole records and retains warnings/freshness and exact recovery actions. All three support output_json; search/vector now share a versioned envelope with typed identities/recovery, per-field truncation, live-guidance handling and explicit unknown-versus-bounded totals. Search retains requested-page guidance/omissions; vector exposes full_chunk guidance recovery within its fixed top_k/MMR window. Grep preserves coverage/stale-source warnings and continuation within max_results, including previously hidden Markdown matches. Candidate and installed MCP checks recovered independent source and distinct original memory sections exactly; JSON escaping counts toward the reply budget. See eval/p04-search-response-budget-delivery-20261111.json, eval/p04-vector-response-budget-delivery-20261111.json, eval/p04-grep-response-budget-delivery-20261111.json and eval/p04-search-json-delivery-20261111.json. get_chunk and read_memory_bank now also honor root max_tokens for complete text/JSON replies and offer output_json with exact hash-bound recovery. Open citation pages retain whole lines or UTF-8 boundaries; explicit ranges, quote proofs and transformed content remain complete or return a minimum-budget retry. Native candidate/installed checks retained provenance/guidance and reconstructed 114,000-byte project/user sections exactly in pages no larger than 8,192 bytes; see eval/p04-document-response-delivery-20261111.json. search_history text/precedent modes now also offer complete reply budgets, typed JSON and whole-record paging within the selected limit. Exact indexed citations and Git message/file actions are verified against independently authored history; capability, rerank and clipping disclosures remain. See eval/p04-history-response-delivery-20261111.json. prepare_implementation_context now supports complete-reply max_tokens with required policy/coverage/warnings and hash-bound source/context continuation records. Native candidate/installed checks reconstructed original typed contexts from authored method/caller sources and retained policy qualifications; changed evidence refuses. See eval/p04-context-response-delivery-20261111.json. get_page_context also supports complete text/JSON budgets with exact hash-bound recovery of controls, methods and other context entries, retaining provider/composition/house-style qualifications. Candidate/installed checks reconstructed four authored handler bodies and150controls; changed evidence refuses mixed pages. See eval/p04-page-response-delivery-20261111.json. pre_commit_review now also defaults to a40KiB complete reply-text budget, accepts max_tokens, preserves every selected finding in a bounded ledger and provides exact original-report recovery without rerunning gates. Candidate/installed JSON and Markdown checks preserve40findings and decision evidence after source edits; actual OciusX returned6526B with exact recovery of its47369B report. The131s review is not a latency improvement. See eval/ociusx-review-response-budget-delivery-20261113.json. Remaining: budgets and consistent envelopes across other tools and durable cursors; provider-token accounting is not claimed.*
  All 13 handler files (~193 handlers). Responses are heterogeneous markdown/text with
  silent truncation (`graph_tools.rs` hardcoded limits, `access_layer_tools.rs` 30KB+
  contexts, unbounded migration reports). Define one envelope: optional `max_tokens`
  request param honored everywhere, `truncated`/`total_count`/`cursor` fields, and
  `output_json` consistently available. An agent's context is its scarcest resource;
  silent truncation is the worst failure mode because the agent doesn't know it's missing data.

  Database report budgeting installed 2026-11-14: JSON/Markdown reports now
  honor a complete reply budget with counts, warnings and exact recovery;
  other response-budget work remains open. [Delivery](eval/ociusx-ai-context-batch-delivery-20261114.json).

  Change-set caller budgets installed 2026-11-14: `get_change_set.max_tokens`
  controls the complete JSON/Markdown reply and immutable recovery pages.
  Default remains40KiB; mandatory identities survive compaction. [Delivery](eval/ociusx-navigation-policy-batch-delivery-20261114.json).

  Natural-language report budgets installed 2026-11-14: `ask_codebase.max_tokens`
  bounds JSON/Markdown/both replies and exact immutable recovery, default40KiB.
  SQL schema retrieval now includes applicable authored guidance. [Delivery](eval/ociusx-ask-sql-guidance-batch-delivery-20261114.json).

- [x] **5. Staleness metadata on every response** (S) — *done 2026-06-12: last_index_completed_ms recorded by process_ingest_stats; get_index_freshness tool (generation, age, watcher, disk-drift count, advice); footer on hot read tools.*
  `state.rs`, `metrics.rs`, handlers. There is no "index generation age", "last reindex",
  or "commits behind HEAD" signal anywhere. An agent getting stale results has zero way to
  detect it — it just gets wrong answers confidently. Add `index_age`/`generation`/
  `dirty_files_pending` to a standard response footer and a cheap `get_index_freshness` tool.

- [x] **6. True incremental reindex in the file watcher** (M) — *done 2026-06-12 (re-scoped): update path was already fingerprint-incremental; the real cost was re-hashing every stat-matching file. Now trusts mtime+size (opt-out via verify_unchanged_hashes), debounce configurable (watch_debounce_secs). Remaining per-generation copy-forward cost belongs to #36 (git/index perf).*
  `engram_server/src/actors/watcher.rs`. Any file change currently triggers a full
  `update_project`; channel saturation loses file specificity; 5s debounce is hardcoded.
  Reindex only the changed files (the generation model already supports this), keep the
  file list across overflow, make debounce configurable. This is the difference between a
  brain that's always current and one that's minutes behind during active editing.

- [x] **7. Actionable error messages with recovery hints** (M) — *done 2026-06-12: ProjectNotFound, no_hits, get_chunk not-found, find_symbol_references empty — all carry concrete next steps.*
  `error.rs` + all handlers. "doc_id not found" / wrong `project_id` / unindexed project
  errors give no next step. Every error an agent can hit should say what to do:
  "project_id 'X' unknown — call list_projects", "node not found — try resolve_symbol
  with name='Foo'". Agents recover from good errors in one turn; bad errors burn three.

- [x] **8. One identity system across tools** (M) — *done 2026-06-12: search hits carry symbols + node_ids; new resolve_id tool accepts node_id/name/FQN/doc_id and surfaces ambiguity as a candidate list.*
  `models/requests.rs`, `graph_tools.rs`, `search_tools.rs`. Search returns `doc_id`,
  graph tools want `node_id` or `fqn`, with no conversion path. Output of tool A must be
  directly valid input to tool B: include `node_id` + `fqn` in search hits where resolvable,
  accept any of the three ID kinds in graph/access-layer tools, add a `resolve_id` helper.

---

## P1 — Graph accuracy & trust

The graph is the differentiator vs. plain RAG. Wrong edges are worse than missing edges:
they feed blast radius, edit safety, and the ADP gates.

- [~] **9. Add inheritance/interface edges (Implements, Extends)** (M) — *partial 2026-06-12:
  InheritsFrom/Implements edge kinds + C# base-list and VB Inherits/Implements extraction;
  overview lists most-inherited types. JS/TS declared extends and TypeScript
  implements/interface extends installed 2026-11-14; source refresh required for
  older indexed revisions. Three existing OciusX first-party files refreshed.
  [Delivery](eval/ociusx-navigation-policy-batch-delivery-20261114.json). Remaining: other target languages (outside OciusX scope),
  blast-radius polymorphism weight (#15), guard-parity inheritance awareness.*
  `engram_graph/src/store.rs`, `cs_extractor.rs`, `vb_extractor.rs`, `parsing.rs`. There
  are 33 edge kinds but no type hierarchy. An agent can't answer "who implements
  IRepository" or see that editing a base class touches 50 subclasses. This also fixes a
  systematic blast-radius blind spot (#15).

- [x] **10. Clean up stale nodes/edges when files are deleted or change** (S–M) — *done
  2026-06-12: purge_stale_nodes_for_paths runs after every update for re-indexed + deleted
  files, REMAPPING surviving cross-file edges onto same-identity successor nodes. NOTE
  discovered: the scheduled GC's GLOBAL purge_old_generations is unsafe for incrementally
  updated projects (unchanged files keep older generations) — audit gc.rs.*
  `store.rs` (purge_old_generations), `ingest_service.rs`. Deleted files leave orphaned
  symbol nodes and dangling edges until (and sometimes past) GC. Agents then get phantom
  callers in blast radius and "dead" methods that look alive. Purge a file's old symbols
  synchronously on change/delete; add an orphan-detection integrity check.

- [x] **11. Surface ambiguity instead of silently picking the first match** (S) - *done 2026-06-12: resolve_symbol/resolve_id/trace tools already surfaced Ambiguous; the remaining hazard was access-layer candidates.first() on substring matches. New resolve_unique_function: exact-name/FQN preference, dotted-FQN retry via terminal segment, AMBIGUOUS error with candidate list (4 tests). File-scoped first() sites deferred to #13 overloads.*
  `store.rs` (resolve_symbol), `graph_tools.rs`. Multiple classes named `Order` →
  tools silently use the first. Return the candidate list with namespaces and require/let
  the agent disambiguate. Silent wrong-target resolution is how agents edit the wrong class.

- [x] **12. Confidence metadata on edges, threaded into tools and ADP gate 1** (M) - *remaining Gate1 consumption deployed 2026-11-11. Gate1 reads producer confidence on selected-source semantic edges including method nodes, preserves the minimum score and explicit unknown coverage, and returns bounded witnesses in text/JSON and immutable decision reports. Higher caller confidence cannot hide weak or missing evidence. 106 focused checks plus candidate/installed public Rust cases verified local call confidence 0.90, unresolved scope, lower overrides and wave evidence. Prior producer/path/impact consumers remain unchanged; this does not certify parser recall, working-tree freshness or overall edit safety. See eval/p112-adp-confidence-delivery-20261111.json.*
  All extractors, `ingest_service.rs`, `cognitive_tools.rs`. A bare-name App_Code match
  and an FQN-verified call currently look identical. Add `confidence` +
  `resolution_method` to edge metadata; show it in graph/blast-radius output; feed
  `ExtractionConfidence` into the Autonomous Decision Protocol's extraction gate (it's
  computed today but not consumed).

- [x] **12b. resolve_symbol_edges: consult edge-metadata FQN during resolution** (S) — *done 2026-06-12: Step 2b matches the edge's own metadata.fqn against node names + node FQNs before terminal-segment fallback; 2 behavioral tests (ambiguous cross-file handler, fqn-miss fallthrough).*
  `engram_graph/src/store.rs`. The HashMap rewrite of resolve_symbol_edges dropped the
  old implementation's use of the EDGE's `metadata.fqn` (webforms event_wiring edges
  carry the handler FQN there). Restore it as a step before terminal-segment matching —
  it disambiguates shared handler names across pages where same-file tiebreaks can't.
  (Identified during the 2026-06-12 edge-endpoint wiring fix; not needed for the test
  suite but improves precision on real multi-page projects like OciusX.)

- [x] **13. Overload/arity-aware call resolution** (M–L) - *done 2026-06-13: sidecar arity+args, VB fallback arity (depth-aware), tree-sitter arity for C#/Rust/Python (named param children); resolver prefers arity matches with stamped methods. Post-pass arity preference deferred (placeholder edges rarely carry args). Was: core done 2026-06-13: sidecar emits arity on methods + args on call sites; batch resolver prefers arity matches (batch_same_file_arity 0.92, batch_arity_match 0.75) with end-to-end ingest test. Remaining: fallback/C# extractor arity, post-pass arity preference.*
  `parsing.rs`, `cs_extractor.rs`, `vb_extractor.rs`. Calls resolve by bare name to the
  first `MyMethod`, regardless of signature. Store arity (cheap, tree-sitter gives it) and
  prefer arity matches; record `ambiguous_overload` when unsure.

- [x] **14. Path-between / reachability graph queries** (M) - *done 2026-06-12: engram_graph::analysis::find_path (BFS, directed-then-undirected fallback, kind filter, membership-edge exclusion, 4 tests) + find_connection_path tool with #11-style ambiguity errors. Bottleneck/SCC queries remain under #20.*
  `graph_tools.rs`, `graph_service.rs`. Agents can traverse one hop at a time but can't
  ask "how does LoginPage reach the Orders table" or "what's transitively affected if I
  change X". Add `find_paths_between`, `reachable_from` (with depth + edge-kind filters),
  and SCC/bottleneck detection. This turns N agent turns into one call.

- [x] **15. Blast radius: polymorphism weight + better explanations** (S) - *done 2026-06-12: WEIGHT_POLYMORPHISM=0.10 over incoming inherits_from+implements_interface (saturates at 8 subclasses), polymorphism_score in ComplexityBreakdown + report line + fan-out guidance item; weights rebalanced dep 0.35 / handles 0.05.*
  `blast_radius_service.rs`. No weight for inheritance fan-out (depends on #9). Also make
  the 1–10 score decomposable in output: which edges, which files, why.

- [x] **16. File-level Contains edges** (S) - *done 2026-06-12: ingest synthesizes file->symbol Contains edges (metadata containment=file) for all location-based symbols; blast radius skips them in density/handles scoring (regression test).*
  `store.rs`, `blast_radius_service.rs`. Containment stops at class level; blast radius
  re-derives file membership by string-comparing `file_path`. Emit File→Symbol edges and
  drop the workaround; fixes systematic undercounting of file-level impact.

- [x] **16b. rebuild_graph mode: re-extract without re-embedding** (M) — delivered 2026-11-12 through `repair_project(scope="graph_only")`; see `eval/p116b-graph-rebuild-delivery-20261112.json`. Original timing estimates below are not measured results.
  Extractor changes currently require delete+reindex: new project id, full Ollama
  re-embed (~30 min), git history re-run (~50 min). Add an update_project mode (or tool)
  that re-runs extraction + process_ingest_stats from disk for ALL files but leaves
  search/vector stores untouched (content unchanged); scoped purge + successor remap
  already preserve cross-file and temporal edges. Turns extractor iteration from ~80 min
  into ~3 min and keeps the project id stable.

- [x] **16c. Indexing must survive embed-backend failure** (M) — delivered 2026-11-12: source/history provider failures retain lexical and graph output, report unembedded counts in direct/job results, and recover through `vector_only`; see `eval/p116c-embedding-failure-delivery-20261112.json`.
  Observed 2026-06-12: machine contention starved Ollama (3x30s retries exhausted) and
  the whole index job died mid-walk (9.5k/28k chunks, 0 graph nodes) and git-history's
  consumer dropped. Embed failure should degrade that BATCH to fts-only + count it in
  the report ("N chunks unembedded — rerun repair_project vector_only"), never kill the
  job. Graph extraction must complete regardless of vector health. Mitigations applied:
  embedding_request_timeout_secs 30->120 in deployed config; embed cache (in progress)
  makes retries cheap.

- [x] **16d. Temporal increments are not idempotent across partial runs** (S-M) - *done 2026-06-13: delete_edges_of_kind (EDGES + both adjacency mirrors, batched); a fresh walk (no last_git_oid watermark) clears prior TemporalCoupling edges so crash-reruns are idempotent. Store test covers edges+adjacency+other-kind isolation.*
  A history run that dies mid-walk leaves its TemporalCoupling increments behind;
  the rerun re-walks from the last SUCCESS watermark and double-counts (observed:
  api-broker<->map.js 246 -> 1112 after one failed + one clean run). Checkpoint the
  walk watermark periodically (e.g. every edge flush), or rebuild temporal edges
  from scratch per full run instead of incrementing.

- [x] **17. Mark dynamic state/SQL access instead of skipping it** (M) - *done 2026-06-13: state half (unresolved counts -> lower-bound rule in docs, verified live: '37 state accesses use a VARIABLE key'); SQL half: New SqlCommand(var) / CommandText = var emit sql:dynamic:{ident} edges with dynamic=true, concat-head literals marked. Was: state half done 2026-06-13: extractor already emitted unresolved_state_read/write (47+13 on OciusX); now COUNTED in StateSummary.unresolved_accesses and rendered as a mandatory lower-bound rule in state-and-data.md. Remaining: string-built SQL marking in sql_parser, trace_state_usage footer.*
  `state_extractor.rs`, `sql_parser.rs`. `Session[variableName]` and string-built SQL are
  silently dropped. Emit `dynamic: true` unresolved-access nodes/edges so data-flow and
  state-migration reports can say "plus N dynamic accesses we couldn't resolve" — agents
  need to know what the graph *doesn't* know.

- [x] **18. Flag VB Roslyn sidecar fallback in extraction metadata** (S) - *done 2026-06-12: fallback_extract_vb stamps extraction_fallback=true on every symbol; sidecar obj/ now gitignored; sidecar deploy documented in memory.*
  `vb_extractor.rs`. Sidecar timeout silently falls back to tree-sitter; the resulting
  lower-fidelity nodes are indistinguishable. Tag them (`extraction_fallback: true`) so
  confidence (#12) reflects it. Also: `tools/vb_roslyn_sidecar/obj/` is untracked at repo
  root — confirm the sidecar is wired, documented, and built in CI.

- [x] **19. Cross-language (C#/VB/ASPX/JS) interop edges with confidence** (M) - *done 2026-06-13: SymEntry carries declaring-file language; ambiguous bare-name ties prefer the caller's language (batch_same_lang 0.65); cross-language bindings get confidence x0.8 + cross_language=true metadata. End-to-end ingest test (VB caller binds VB over JS twin).*
  `graph_service.rs` (App_Code resolution). Same-language, bare-name matching only.
  Record language-boundary crossings explicitly with lowered confidence.

- [x] **20. Cycle/SCC detection surfaced in impact analysis** (M) - *done 2026-06-12: tarjan_scc over Calls/Dependency/Imports (placeholders + statistical kinds excluded; 3 tests) + find_dependency_cycles tool (named members, binding kinds, migration guidance). Blast-radius integration deferred until a cached SCC index exists.*
  `engram_graph` (analysis), `blast_radius_service.rs`. Tarjan SCC post-ingest; flag
  cycles in blast radius and migration ordering (cycles are exactly where naive
  strangler-fig plans fail).

- [x] **21. Return-type / parameter-type edges** (L)
  `parsing.rs`, extractors. Enables type-boundary reasoning (strangler-fig seams, "what
  flows into this method"). Big, but unlocks the next tier of graph value.

  Delivery: `eval/p121-signature-types-delivery-20261112.json` (source `7f9aa10`; candidate and installed public acceptance passed).

- [x] **22. Lightweight def-use (data-flow) edges** (L)
  New post-ingest pass. Today data-flow is call edges + state/SQL edges only. Even
  method-local def-use with cross-method parameter linking would let `trace_data_flow`
  answer lineage questions agents currently have to read source for.

  Delivery: `eval/p122-def-use-delivery-20261112.json` (candidate and installed public lineage, source-update and storage-preserving graph repair acceptance passed).

---

## P2 — Generalize beyond legacy .NET

Engram's pitch is "a brain for *any* project", but deep extraction (calls, state, UI,
SQL, lifecycle) is WebForms/.NET-only; the other 9 tree-sitter languages get shallow
symbol extraction, and ~56 service files hardcode WebForms concepts. This is the
strategic gap between "niche migration tool" and "every agent's first MCP server."

- [x] **23. Second-pass call resolution for TypeScript, Python, Rust, Go, Java** (L, per language)
  `parsing.rs`. Tree-sitter already extracts calls; add per-language import/module
  resolution so bare calls link to FQNs (TS: import paths + tsconfig paths; Python:
  imports; Rust: `use` + crate paths; Go: packages). Start with TypeScript and Python —
  that's where most agent work happens today. Reuse the existing two-pass FQN
  architecture from the .NET side.

  TypeScript delivered: `eval/p123-typescript-modules-delivery-20261112.json` (candidate and installed module/alias calls, incremental changes and storage-preserving graph repair). Python delivered: `eval/p124-python-modules-delivery-20261112.json` (candidate/installed import calls, incremental changes and storage-preserving graph repair). Rust delivered: `eval/p125-rust-modules-delivery-20261112.json` (candidate/installed use/module calls, Cargo manifest updates and storage-preserving graph repair). Go delivered: `eval/p126-go-modules-delivery-20261112.json` (candidate/installed package calls, manifest/workspace updates and storage-preserving graph repair). Java delivered: `eval/p127-java-modules-delivery-20261112.json` (candidate/installed import and typed-receiver calls, incremental rebinding and storage-preserving graph repair). All five language deliveries reconciled: `eval/p127-todo23-acceptance-20261112.json`; static source scope and explicit limitations retained.

- [x] **24. Project-type detection + capability gating** (M)
  Delivered: `eval/p128-project-capabilities-delivery-20261112.json` (candidate/installed automatic presets, source recovery, capability-filtered discovery, refresh/reopen and explicit overrides).
  `project_service.rs`, `capabilities.rs`. Detect ecosystem at index time (csproj/sln vs
  package.json vs Cargo.toml vs pyproject). Use it to: gate WebForms/migration tools out
  of the visible tool list (#3), pick extractor presets, and tailor
  `get_codebase_overview` output.

- [x] **25. Language fixture corpora beyond WebForms** (M)
  Delivered: `eval/p129-language-corpora-delivery-20261112.json` (four authored source/call oracles, public-handler integration checks, native/no-vector strict local PASS and CI wiring; remote execution status recorded separately).
  `tests/fixtures/` has only 4 fixtures, all `dotnet_webforms_*`. Add small
  ground-truth projects for TS/Python/Rust/Go with expected symbols + call edges, wired
  into CI. Without these, #23 can't be verified and regressions are invisible.

- [x] **26. Audit the 56 WebForms-biased service files for non-.NET behavior** (M)
  Delivered: `eval/p130-service-applicability-delivery-20261112.json` (requested-project/target no-ops, explicit review scope, neutral four-language guidance/style/review, source-preserving slices; candidate and installed MCP acceptance).
  Services hardcode Page_Load/ViewState/Master Pages. Each must either no-op cleanly with
  a clear "not applicable to this project type" message, or generalize (e.g.
  `produce_claude_md`, `pre_commit_review` gates, coding-style analysis should be fully
  language-neutral). Today an agent on a Node repo can call tools that quietly return
  nonsense.

- [~] **27. Generalize state tracking to modern equivalents** (L)
  Browser-storage tracing installed 2026-11-13. `trace_state_usage` accepts
  `LocalStorage` and `SessionStorage` with literal JS/TS getItem/setItem/removeItem
  keys and returns indexed readers/writers with source positions and hashes.
  Seventeen native checks and candidate/installed public MCP acceptance passed.
  After one normal index refresh, actual OciusX `tabbedModalExtended` returned
  the source save/reset/read methods. Shared browser origin/session and execution
  remain unproven; dynamic keys and clear-all effects are unresolved.
  Evidence: [browser storage delivery](eval/ociusx-browser-storage-delivery-20261113.json).
  Broader retained language/framework work remains unfinished and parked.

  Retained baseline: `eval/p131-modern-state-baseline-20261112.json`.
  Earlier language/framework work and its unfinished acceptance remain recorded in
  `eval/p131-modern-state-progress-20261112.json`; this delivery does not close them.
  `state_extractor.rs` knows Session/ViewState/Application. Equivalent value for modern
  stacks: env vars, global singletons, React context/Redux stores, module-level mutable
  state. Same edge kinds, new extractors.

---

## P3 — Agent workflow & orchestration tools

The original AGENT_ENHANCEMENT_SPEC phases 39–45 proposed these workflows. Several
are now implemented and installed; the item notes below identify delivered behavior,
failed acceptance and remaining work. Preserve the original priority order. Skip
proposals that duplicate the existing surface:
`find_hardcoded_secrets` ≈ secret gate, `calculate_change_risk_score` ≈ `check_edit_safety`.

- [~] **28. `ask_codebase` - one natural-language entry point (spec 43-1)** (L)
  Optional cited synthesis installed 2026-11-13. Default requests retain the
  deterministic evidence report; `synthesize: true` uses the configured model
  and checks evidence IDs and exact quotations without upgrading retrieval status.
  Five focused native checks and candidate/installed MCP acceptance passed.
  Same-file member context is now installed in JSON and Markdown definition
  evidence, preserving relevant types/defaults and source spans. Two focused
  native checks and candidate/installed public MCP checks passed. One candidate
  OciusX answer with the installed binary's SHA met all four unchanged criteria,
  including the previously withheld `Nothing` outcome. The model and prompt
  were unchanged; no repeated model tuning was performed. This inspected source
  question does not establish general task usefulness or product readiness.
  Applicable authored guidance now accompanies returned code in JSON and Markdown;
  optional synthesis can cite exact policy text. Installed OciusX map-layer guidance
  matched the independent source exactly. This does not establish model compliance.
  Evidence: [scoped-guidance delivery](eval/ociusx-ask-policy-delivery-20261113.json).
  Opt-in model-assisted routing installed 2026-11-14. Default makes no routing
  model request; rejected/unavailable routing retains the deterministic plan.
  Forty native checks and candidate/installed public checks passed, including
  a source-matched OciusX caller query. Broader task acceptance remains unfinished.
  [Batch delivery](eval/ociusx-ai-readiness-batch-delivery-20261114.json).
  Evidence: [member-context delivery](eval/ociusx-ask-member-context-delivery-20261113.json);
  [retained earlier 3/4 result](eval/ociusx-ask-synthesis-delivery-20261113.json).
  Routes a question to search + graph + access-layer internally and returns a synthesized,
  cited answer. This is the single highest-leverage *new* tool: it collapses the 114-tool
  selection problem for the 80% case and makes Engram useful to agents that never learn
  the full surface. Deterministic router first; optional LLM synthesis later.

- [x] **29. Edit Session Protocol (spec 45: begin/complete_edit_session, detect_incomplete_changes)** (M) - *done 2026-06-13: bookends persist intent (registry meta), begin returns the expectation brief on the PLANNED set, complete reports scope drift (planned-but-unedited) + full completeness on actuals, session consumed on close. Was: engine done 2026-06-13: detect_incomplete_changes — untouched co-change partners (with historical evidence, tail-matched against path-spelling variants) + state keys shared with untouched files; 3-case behavioral test through real graph. Both session bookends are delivered; their expectations remain advisory evidence for the agent.*
  Bookends an agent's edit: snapshot expectations at start, verify coupled files/tests/
  state keys at end. Turns Engram from advisor into a loop-closing safety harness — and
  it composes the already-built check_edit_safety/pre_commit_review machinery.

- [x] **30. `find_implementation_pattern` (spec 42-2)** (M) — *done 2026-06-12: top exemplar
  files for a pattern query with symbols, SQL/state edges, co-change partners, snippet, and
  cross-exemplar common ingredients.*

- [x] **30b. `get_concept_footprint`** — *done 2026-06-12 (from the OciusX weak-user-story
  analysis): every touchpoint of a domain concept grouped by role (data/sql/state/ui/logic/
  endpoints) + anchor consumers + lexical-only files. The "all the places code categories
  is used" catch.*

- [x] **30c. `find_similar_changes`** — *done 2026-06-12: token-bag similarity over recent
  git history; reports recurring companion artifacts (exact files + dir/*.ext shapes)
  missing from a planned file set. The "forgot the admin page / menu entry" catch. v2 ideas:
  persistent commit-shape index instead of request-time walk; cluster into named feature
  templates.*

- [ ] **31. `trace_user_request` (spec 43-2)** (M)
  *Partial delivery 2026-11-12: installed source-cited WebForms URL/action tracing,
  authored handlers, verified calls, literal stored-procedure/table source evidence
  and response excerpts. Added source-bound DBML mapping for explicit local VB
  contexts: actual OciusX geolayers trace now cites dbo.vsl_visualiseringslager
  from iFalt.dbml:281 and the reached LINQ reference at visualisering-lager.vb:58.
  Candidate/installed public cases and actual OciusX source acceptance passed.
  Dynamic bindings, general ORM translation, nested procedure execution, trigger
  effects, deployed database identity and runtime execution remain unresolved.
  Evidence: [original trace](eval/ociusx-request-trace-delivery-20261112.json),
  [DBML mapping delivery](eval/ociusx-request-dbml-mapping-delivery-20261112.json).*
  Conventional VB Web API entry tracing installed 2026-11-13. Literal
  `MapHttpRoute` registrations now lead to source-verified controller/HTTP-method
  candidates and existing call evidence. All 22 native request-trace checks and
  candidate/installed public checks passed. Actual `POST /webhook/vetro/42`
  resolved its registration, `VetroController.HandleIncomingRequest` and the
  source interface call. Concrete DI binding and runtime execution remain unproven.
  Evidence: [Web API entry delivery](eval/ociusx-webapi-request-trace-delivery-20261113.json).
  Explicit VB interface implementation tracing installed 2026-11-13. Existing
  compiler-backed call/Implements mappings now lead to implementation candidates
  and their downstream call/database evidence. Twenty-four native checks and candidate/
  installed public MCP checks passed. Actual OciusX POST /webhook/vetro/42 reaches
  VetroService.HandleIncomingRequest and the source SaveEventsToDB call. These are
  possible source paths; runtime DI selection and execution remain unproven.
  Actual OciusX required max_tokens=32628 (130509-byte response); its default
  output budget was insufficient.
  Evidence: [interface trace delivery](eval/ociusx-interface-request-trace-delivery-20261113.json).
  Default-budget usability fixed 2026-11-14. Oversized traces retain all selected
  paths in a compact node/edge view with exact immutable full-response recovery.
  Actual OciusX POST /webhook/vetro/42 now returns within the default budget;
  every selected path and source location is retained and full recovery matches
  the original response byte for byte. This supersedes the earlier default-budget
  refusal above; runtime binding/database limits remain.
  Evidence: [default trace delivery](eval/ociusx-request-trace-budget-delivery-20261114.json).
  Literal VB `ExecuteCommand` table tracing installed 2026-11-14. Constant
  SQL concatenation and separate format-value arguments now produce source-cited
  read/write table candidates in reached methods. Thirty native and seven checks
  on each candidate/installed public fixture passed; dynamic commands remain
  unresolved and database binding/execution are not claimed.
  Evidence: [inline SQL delivery](eval/ociusx-inline-sql-request-trace-delivery-20261114.json).
  Literal nested procedure tracing installed 2026-11-14: reached SQL procedures now
  lead through source-cited EXEC/EXECUTE references to downstream procedures/tables.
  Exact-name binding, source hashes, SQL batches and bounded traversal are retained.
  Runtime execution and trigger/deployment limits above remain.
  [Batch delivery](eval/ociusx-ai-readiness-batch-delivery-20261114.json).
  Applicable authored guidance now accompanies returned source paths, refreshes
  without reindexing and retains complete-source recovery when navigation and
  policy cannot fit together. [Batch delivery](eval/ociusx-ai-context-batch-delivery-20261114.json).
  URL/route → handler → SPs → tables, end to end. Mostly composes existing edges; huge for
  "where do I start" questions.

- [~] **32. `one_shot_feature_plan` (spec 42-1)** (L)
  Compiles a full pre-edit dossier for a feature request (relevant files, patterns,
  safety, test plan). Build after #28/#30 exist to compose them.
  WebForms/VB.NET implementation installed 2026-11-13 with bounded source evidence,
  proposed edits/DDL, requirement dispositions and at most one contract correction.
  Native and candidate/installed public checks passed. The actual OciusX request
  recognized existing behavior and proposed no edits, but source review found
  capacity/minimum confusion and an unrequested global-setting assumption.
  Semantic acceptance failed; this item remains unfinished. Preserve those findings
  and advance the original backlog instead of repeating planner tuning.
  Evidence: [installed partial result](eval/ociusx-feature-plan-installed-partial-20261113.json).
  Source-dossier component installed 2026-11-14: `dossier_only: true` returns
  bounded source/change-set/pattern context for the calling AI with zero server
  text-generation calls, including when that backend is disabled. 15 native checks and
  candidate/installed public checks passed; actual OciusX returned 16 independently
  verified source excerpts. Retrieval still took 95-110 seconds. Generated-plan
  semantic failures above remain unfinished; this mode does not close item 32.
  Evidence: [source dossier delivery](eval/ociusx-feature-dossier-delivery-20261114.json).
  Planning inventory batching installed 2026-11-14: the same OciusX request returned
  the identical 16 source excerpts and complete pattern output in 53.63s candidate
  and 60.18s installed. Per-file caps and source freshness remain.
  The installed result misses the fixed 60-second acceptance, which remains
  unfinished. Retain the functional improvement and advance the backlog without
  further timing replays. [Partial result](eval/ociusx-planning-node-batch-partial-20261114.json).

- [x] **33. Batch/multi-get endpoints** (M) — *delivered 2026-11-12.*
  `batch_read` composes 1–16 typed `get_method_info`, `resolve_id` and `get_chunk`
  reads with one complete response budget, independent errors and exact recovery
  for whole omitted results. Candidate and installed public MCP checks pass;
  one actual OciusX batch matched three individual responses. No atomic snapshot
  or latency/productivity improvement is claimed.
  Evidence: `eval/ociusx-batch-read-delivery-20261112.json`.

- [x] **34. Next-tool hints in every response** (S?M) ? *remaining applicable specialty tier delivered 2026-11-13.*
  Bounded `next_tools` with reasons and complete arguments where source identity
  permits; OciusX repeated-query findings lead to their actual VB method context.
  Profile filtering and empty/ambiguous/stale-source guidance are verified.
  Existing hot-path/core/closed-area replies remain intact. Errors, multipart/media,
  non-object JSON, oversized results and replies without advisory budget preserve
  original data; hints never replace evidence or execute follow-ups automatically.
  Nine native checks, 13 candidate and 13 installed public calls passed; an actual
  OciusX finding at GisImageLayer.vb:148 led to UpdateLayers with source/state unchanged.
  Evidence: `eval/ociusx-next-tool-hints-delivery-20261113.json`.

- [x] **35. Database Oracle remainder (spec 39: query_schema, detect_n_plus_one, find_missing_indexes)** (M–L)
  For data-heavy projects. `query_schema` first — agents constantly need "what columns
  does this table actually have."

  `query_schema` delivered 2026-11-12: eleven source-backed query modes with
  qualified identities, source citations/freshness, keys/defaults/indexes,
  ambiguity, paging and complete-response budgets. Five native tests and18
  public calls each on candidate/installed binaries pass. A real OciusX table
  lookup verified four columns, two cascading foreign keys, identity/default,
  primary key and unique index without source or generation changes.
  SQL declarations do not establish deployed database state; ALTER/DROP,
  execution and unresolved name binding remain explicit limits.
  `detect_n_plus_one` delivered 2026-11-12: VB loop/WebForms row-binding
  database-call candidates, cross-file helper evidence, LINQ cached-list
  exclusions, source freshness, paging and complete budgets. Three native tests
  and ten candidate/installed public calls each passed. Installed OciusX check
  identified `GisImageLayer.UpdateLayers` query line148 inside loop146. Static
  candidates do not prove runtime cost or execution counts; constructors,
  injected receivers and dynamic dispatch remain unassessed.
  `find_missing_indexes` delivered 2026-11-12: WHERE/JOIN/ORDER BY index
  candidates from SQL sources and indexed VB literals; existing leading/unique/
  filtered keys, citations, static site ranking, quoted DDL, complete budgets
  and stale-evidence withholding. Seven native tests and eleven candidate/
  installed calls each passed. Actual OciusX `rl_pris_view` correctly suppressed
  three redundant base-table suggestions. Dynamic/provider SQL, indexed-view
  target resolution and executed migration state remain explicit limits;
  no measured database speedup or comparative productivity claim.
  Evidence: [index-suggestion delivery](eval/ociusx-index-suggestions-delivery-20261112.json).
  Evidence: [N+1 installed delivery](eval/ociusx-n-plus-one-delivery-20261112.json).
  Evidence: [installed delivery](eval/ociusx-query-schema-delivery-20261112.json).

---

## P4 — Performance & reliability

A slow or flaky brain gets dropped from the loop. These are mostly known issues with
specs already written.

- [~] **36. Implement the 4-task git-history indexing perf plan** (M — already fully specced)
  `docs/superpowers/plans/2026-04-16-index-git-history-perf.md`. Tree-ID precheck, drop
  BTreeSet, bulk IndexWriter, batched channel pipeline. 50–80% projected gain; git
  indexing measured in hours on big repos is an adoption blocker.
  Reconciled 2026-11-12: all four implementation tasks already exist and are
  included in the installed build; do not reimplement. The projected large-repo
  speedup remains unmeasured, so this item stays partial.
  Evidence: [existing implementation](eval/todo36-existing-implementation-20261112.json).

- [x] **37. Indexing progress reporting an agent can poll** (M)
  `project_tools.rs`, `git_tools.rs`, job service. Original requirement: long indexing needs pollable state so agents do not
  assume failure and retry. Persist per-phase progress (files done/total,
  current phase, ETA) and expose via `get_job_status`.
  Delivered 2026-11-12: persistent phase/count/unit/percent/timestamp and phase
  ETA output, unknown values kept explicit, terminal observations preserved.
  Three new behavior and four startup-cleanup checks passed; candidate and
  installed MCP each verified five background source/update/history jobs.
  ETA is phase-only; phase completion is not whole-job completion.
  Evidence: [installed progress delivery](eval/ociusx-indexing-progress-delivery-20261112.json).

- [x] **38. Windows named-pipe transport for the multi-client daemon** (M) - *done 2026-06-13
  (user-reported: only one CLI window could use the MCP at a time — second died on the
  redb exclusive lock). Implemented the Windows named-pipe transport mirroring the Unix
  path: spawn_socket_listener accept-loop (NamedPipeServer, recreate-instance-per-connection),
  serve_pipe_session, run_proxy NamedPipeClient connect + forward_stdio_to_pipe. dispatch
  routes the pipe name through socket_path on Windows. Hardened the proxy connect loop to
  retry ERROR_FILE_NOT_FOUND/PIPE_BUSY for 30s (primary writes metadata BEFORE its slow
  AppState init binds the pipe — a real startup-window race found in testing). Flipped the
  default to multi_client=true (serde default fn, not just the struct impl). Verified live:
  two engram_server processes share one daemon on Windows, both serve list_projects via
  the config-driven path. CI test: named_pipe_transport_round_trips.*

- [~] **39. Vector/Tantivy consistency repair after partial ingest failures** (M) -
  Published-source remainder delivered 2026-11-12: compare the same managed
  source snapshot and memory namespace, including inherited revisions. Detect
  shortage and surplus even when project-wide totals cancel; recheck under the
  existing owned scoped-repair lock before recovery. Unavailable evidence stays
  explicit. Candidate and installed MCP checks plus owned corruption/recovery
  tests passed. Other namespaces retain aggregate thresholds; their generation
  completeness and equal-count wrong-row corruption remain unverified.
  Evidence: [installed source consistency](eval/ociusx-source-integrity-delivery-20261112.json).

- [x] **40. Trigger generation GC after crash recovery** (S) - *done 2026-06-13:
  AppState.gc_nudge (Notify) + third GC select branch; update_project fires it after a
  run that resumed from a checkpoint completes. JOB1 active-count guard still applies.*

- [x] **41. PageRank/centrality cache warming** (S) - *done 2026-06-12: get_or_compute_centrality (cache-or-compute-and-persist) used by blast radius + get_centrality; index_project warms the cache post-resolve; stale EdgeKind exhaustiveness count fixed (40->43).*
  Centrality reranking blocks on first use. Compute post-ingest, persist, refresh lazily.

- [x] **42. Harden `business_logic_service` LLM integration** (M) - delivered
  2026-11-12: safe visible failure reasons, bounded cited source fallback and no
  misplaced output-budget retry on provider outages. Reuses configured backend/model,
  deadlines and provider backoff; preserves malformed-output retry and prevents failed
  analysis being stored as business rules. Fallback is limited lexical evidence,
  not inferred semantics. Native and candidate/installed public checks passed.
  Evidence: [installed LLM degradation](eval/ociusx-llm-degradation-delivery-20261112.json).

- [x] **43. Replace panics on safety-critical service paths with errors** (S–M) - *triaged
  + closed 2026-06-13: full services sweep found ONLY infallible patterns (compile-time
  Regex::new expects in LazyLock statics, group-0 capture expects) plus one max_by_key
  unwrap on a fixed array (now unwrap_or). The prior hardening phases (18/S9/EXH) had
  already eliminated the reachable class; TODO entry was stale.*

- [x] **44. Fix job-cancellation checkpoint marking** (S) - *verified already fixed
  2026-06-13 (ENG-AUD-P1-0005 hardening): result captured at all 3 call sites, failure
  embedded in the job tombstone message, errors logged, persistence test in
  job_service.rs. TODO entry was stale.*

- [x] **45. LanceDB integrity sentinels** (M) - *done 2026-06-13: integrity_service now
  detects MismatchKind::VectorShortfall — when embeddings are expected (backend != fts_only)
  but the LanceDB vector store holds far fewer entries than Tantivy (>5% and >20 absolute),
  embeddings failed for part of the corpus and hybrid recall is silently degraded. Wired to
  the vector_only re-embed repair. The pre-existing "masked vector=0 is invisible" canary
  test now PROVES the fix. 3 new unit tests (flagged / fts_only-suppressed / floored).*

- [x] **46. grep_project freshness cost** (M) - *done 2026-06-13: DocStore.list_fingerprints
  (one range scan, replaces list_tracked_paths + N point reads) + a 10s TTL freshness
  cache keyed (project, namespace) so agent grep bursts don't re-stat the corpus each
  call. Batch-scan test pins project scoping.*

- [x] **47. Symlinked roots + Windows path-separator polish** (S) - *done 2026-06-13:
  from_relative now falls back to canonical-root stripping (root canonicalized once,
  memoized in CANON_ROOT_CACHE) then both-sides-canonical, paying syscalls only on the
  rare symlink-mismatch path; fixes silent file drops under symlinked roots.
  RelPath::to_native_string renders `\` on Windows for user-facing messages. 2 tests
  (canonical-mismatch fallback via canonicalize() prefix, native separator).*

---

## P5 — Quality infrastructure (prove the brain is right)

There is currently no ground-truth evaluation that search or the graph return *correct*
answers — only behavioral tests that they return *something*. For a tool whose whole
value is accuracy, this is the biggest meta-gap.

- [~] **48. Retrieval eval harness with golden query sets** (M) ? *live tier retained; fixture CI tier implemented and locally verified 2026-11-13.*
  `retrieval_fixture_tests` indexes 13 authored WebForms/VB/JS/TS/SQL files and
  checks eight frozen questions through public handlers. All expected files ranked
  first locally (MRR 1.0); fixed-denominator precision@5 is 0.2 with one relevant
  file per question. Missing answers, bad ranks and duplicate-chunk credit fail.
  `benchmark-ci.yml` runs the gate and uploads actual rankings and metrics.
  Hosted execution remains unverified: repository GitHub Actions are disabled.
  Existing live golden coverage and limits are retained; no semantic/productivity claim.
  Evidence: `eval/ociusx-retrieval-fixture-delivery-20261113.json`.

- [x] **49. Graph-accuracy eval: expected call/state/SQL edges per fixture** (M) - *done 2026-06-13: graph_accuracy_eval.rs — mini legacy-app fixtures (VB+enrich, state extractor, JS bridge) with exact expected inventories: arity, guards, settings, ORM, literal+dynamic SQL, spatial counts+sentinel, DOM/postback/AJAX. Tier boundary documented (bare calls = sidecar tier).*
  Extend fixtures with ground-truth edge lists; report extraction recall/precision per
  language per extractor. Gate CI on regressions.

- [x] **50. End-to-end MCP-layer tests for the top 20 tools** (M) - *implemented and locally/candidate/installed verified 2026-11-13; hosted CI remains unverified.*
  `mcp_core_contract_tests` launches the production binary and calls twenty tools over
  stdio MCP, including 36 invalid/unknown project identities, empty index, admission
  limits, source recovery and response budgets. It exposed and retains a regression
  check for explicit empty edit completion; that runtime fix is installed. Existing
  CI is configured to run the test, but repository Actions are disabled. Owned fixtures and server
  processes close on completion. This is not full-scale performance or sidecar proof.
  Evidence: `eval/ociusx-mcp-contract-delivery-20261113.json`.
  ~50 integration test files exist but mostly call services directly. Spin the actual MCP
  server, call tools as a client would (bad project_id, huge repo, empty index) and
  assert response envelope contracts (#4).

- [ ] **51. Finish the EQS gate plan** (per `eqs_gate_requirements.md`) (M)
  Target 4.5+, currently ~2.5–3.0 with all EXH findings closed. The remaining gates
  overlap heavily with #48–50 — do them together.

- [~] **52. Latency budgets in CI** (S) - *search/method fixture gates implemented and locally verified 2026-11-13.*
  Three authored queries each exercise `search_memory` and `get_method_info` over
  production stdio MCP. Every reply must identify the expected source; forty warm
  samples per case gate p95 at fixed 250ms/200ms limits. Local p95 ranges were
  9.62-15.25ms for search and 1.29-2.35ms for method lookup. CI is configured to
  fail on violations, but hosted execution remains unverified while Actions are
  disabled. Blast Radius remains held by the closed-tool instruction. These small
  fixture results do not establish production-scale OciusX latency.
  Evidence: `eval/ociusx-read-latency-delivery-20261113.json`.
  Assert p95 per hot tool (search, get_method_info, blast radius) on the fixture corpus
  so perf regressions surface before users feel them.

---

## P6 — Adoption & packaging

- [~] **53. Prebuilt release binaries + one-command install** (M)
  *Codex installation added 2026-11-13: `-Codex` registers through the native
  CLI while retaining existing settings and refusing duplicate server names.
  Isolated CLI registration, unchanged JSON-client checks and VB retrieval through
  the exact registered command passed. The current live Codex profile was preserved.
  Evidence: `eval/ociusx-codex-first-install-delivery-20261113.json`.*
  *Self-contained Windows ZIP added 2026-11-13: the verified runtime, all 214
  sidecar assets, installer and first-query guide ship together. An extracted copy
  installed without repository scripts or `PackageRoot`; the exact MCP entry
  indexed 13 sample files and retrieved the expected VB source. Public hosting,
  download/bootstrap and macOS/Linux distribution remain unfinished.
  Evidence: `eval/ociusx-windows-release-bundle-delivery-20261113.json`.*
  *Windows first-time installation added 2026-11-13: `scripts/install_windows.ps1`
  consumes a complete existing package, installs the server and full VB sidecar,
  creates local configuration and registers an MCP client connection while
  preserving unrelated entries. Fresh-install acceptance used the exact registered
  command to index 13 VB/WebForms/JS/TS/SQL sample files and retrieve VB source.
  Published releases, download/bootstrap distribution and macOS/Linux installation
  remain unfinished. Evidence: `eval/ociusx-windows-first-install-delivery-20261113.json`.*
  *2026-11-13: no GitHub releases are published. Existing Windows paired build and
  managed-upgrade scripts do not satisfy public cross-platform distribution or
  first-time installation. See `eval/todo53-packaging-state-20261113.json`; retain
  this requirement and the no-new-tags constraint.*
  Today: `cargo build --release` (30+ min, libgit2 headers, Rust toolchain). Add GitHub
  Releases with win/mac/linux binaries and an `npx`/installer wrapper that also writes
  the MCP config block. Most potential users are lost right here.

- [~] **54. Quickstart: clone → indexed → first query in 5 minutes** (S)
  *2026-11-13: `docs/QUICKSTART.md` provides per-OS sample setup, exact index /
  freshness / search / source recovery calls and expected results. Literal Windows
  setup and all four calls passed against the installed runtime in 1.07s, with
  the owned sample removed after server exit. Linux/macOS and GUI setup remain
  unverified; compilation/full-repository indexing are outside the small-sample
  timing. See `eval/ociusx-quickstart-delivery-20261113.json`.*
  README has MCP config samples; tie it together with a copy-paste path per OS, expected
  timings, and "how do I know it worked" (point at #5 freshness tool).

- [x] **55. Large-repo tuning guide** (S) - *delivered 2026-11-13.*
  `docs/TUNING.md` gives source-grounded defaults and conservative starting profiles,
  embedding/history/debounce tradeoffs and memory-accounting limits. Both literal
  config fragments passed installed indexing/freshness/search/metrics checks on an
  owned tiny fixture; no large-repository speedup or optimum is claimed.
  Evidence: `eval/ociusx-tuning-delivery-20261113.json`.
  `tantivy_writer_memory`, embedder choice, git-history depth, watcher debounce — one doc
  page with recommended values by repo size.

- [x] **56. Defaults audit** (S) - *audited and default disclosure corrected 2026-11-13.*
  Retained local projection/degrade-honestly behavior, 5s debounce, 50MB writer
  and 2GiB tracked memory default; no evidence supported arbitrary numeric changes.
  Default search now admits projection ranking effects and gives the lexical-only
  alternative. README no longer implies a learned local model is selected. Five
  native checks plus optimized candidate/installed default-config cases passed.
  No global tuning optimum or ranking-quality improvement is claimed.
  Evidence: `eval/ociusx-defaults-delivery-20261113.json`.
  `config.rs`: default embedder model name (see #2), debounce, memory budgets — make the
  zero-config experience the recommended one.

- [x] **57. Doc drift pass** (S–M) - *reconciled 2026-11-13.*
  Existing generator now passes for all three capability matrices. README,
  TOOL_CONTRACT and ROADMAP distinguish code presence, discovery and acceptance
  and no longer claim universal completeness/offline/safety. Enhancement phases
  39-45 identify available versus unregistered names while preserving design
  requirements, examples and partial semantic/trace limits.
  Evidence: `eval/ociusx-doc-drift-delivery-20261113.json`.
  `docs/ROADMAP.md`, `TOOL_PARITY.md`, `TOOL_CONTRACT.md`, README claims vs. the 114
  actual tools; mark AGENT_ENHANCEMENT_SPEC phases 39–45 as roadmap-not-built (an agent
  reading the spec today would believe tools exist that don't).

- [x] **58. IPC protocol version negotiation** (S) ? *delivered 2026-11-13.*
  Default detached-daemon clients now reject an advertised incompatible wire
  protocol before forwarding data. The installed baseline forwarded 59 bytes
  to the incompatible owned endpoint; the fixed installed binary forwards zero
  and reports both versions with restart guidance. Candidate and installed
  executable fixtures also verify compatible message/response forwarding across
  crate versions and the existing in-process refusal. All fixtures cleaned up.
  Windows execution verified; Unix uses the shared gate but was not executed.
  Wire compatibility is not source/tool identity or automatic version downgrade.
  Evidence: `eval/ociusx-daemon-protocol-delivery-20261113.json`.


---

## P7 — Debt & cleanup

- [ ] **59. Consolidate the 3 overlapping DB-analysis services** (M)
  `database_intelligence_service`, `db_strategy_service`, parts of
  `full_project_migration_service` re-implement SQL parsing/table extraction.
  Partial delivery 2026-11-14: shared qualified object-name recognition and
  basename normalization correct ordinary `dbo.Table` / `[dbo].[Table]` analysis.
  Native statement/SP/report checks and candidate/installed public dependency
  reports pass. Full service consolidation remains open; public scaffold method
  emission remains unverified. See `eval/ociusx-sql-identifiers-delivery-20261114.json`.
  Call-chain reports now share the indexed procedure parser (2026-11-14),
  correctly recognizing ordinary `[dbo].Procedure` declarations. Full consolidation
  remains unfinished. [Batch delivery](eval/ociusx-ai-readiness-batch-delivery-20261114.json).
  Further SQL consolidation installed 2026-11-14: shared statement/body references
  recognize INSERT without INTO, shared parameter parsing retains OUTPUT, and an
  explicit SQL file with at most one procedure avoids unused application-code scans.
  Candidate/installed checks passed on unchanged OciusX SQL in owned fixtures;
  broader consolidation remains unfinished. [Delivery](eval/ociusx-sql-analysis-batch-delivery-20261114.json).

- [x] **60. Remove or document alias tools** (S) ? *verified 2026-11-13: the existing `trigger_rem_cycle` description directs callers to `dream_project`; both use the same request and handler. TOOL_CONTRACT and README document the alias. No new runtime change or tool-selection benefit is claimed. See `eval/ociusx-alias-documentation-reconciliation-20261113.json`.*
  `dream_project`/`trigger_rem_cycle` etc. Every redundant tool worsens #3.

- [x] **61. Shallow tools: fix or relabel** (S?M) ? *named gaps reconciled 2026-11-13.*
  `analyze_error_stack` already parses frames, boosts matching files/functions and
  labels its result heuristic, not a debugger; no new stack-analysis claim.
  `get_instrumentation_pack` now supplies complete manual VB.NET/C# event helpers
  instead of a C# placeholder and VB refusal. Both returned helpers compiled and
  executed locally; four emitted records ingested with expected graph identities.
  Candidate and installed MCP checks verify exact source delivery and the matching
  `ingest_instrumentation_logs(project_id, log_content)` handoff. It does not install
  instrumentation, infer query hashes or prove runtime completeness. Existing
  legacy next-tool filtering remains; the ingestion call is explicit in the text.
  Evidence: `eval/ociusx-instrumentation-pack-delivery-20261113.json`.

- [x] **62. TODO/FIXME sweep through services** (S) - *audited clean 2026-06-13: ZERO
  real stubs in the whole codebase (no todo!()/unimplemented!()/FIXME). integrity_service
  is fully implemented (resolve_auto_repair, check_project_integrity[_with_policy],
  build_integrity_mismatches, run_integrity_checker — no markers). The only service TODOs
  are (a) scaffold_service's intentional markers EMITTED INTO generated migration code for
  humans, and (b) TODO-NN design-rationale comments tied to completed accuracy items —
  load-bearing docs, kept.*

- [x] **63. Repo hygiene** (S) - *done 2026-06-13: sidecar obj/ and .superpowers/ now
  gitignored; removed tracked one-off scratch script fix_defaults.ps1.
  full_project_migration_service.rs + dir is the STANDARD Rust module layout (the .rs
  declares pub mod model / mod analyzers / mod llm_enhancement / mod rendering living in
  the dir) — idiomatic, not ambiguous; left as-is.*

---

- **2026-06-13 iter 15 — accuracy batch verified + perf regression hunted down**:
  warm embed cache cut reindex 8min->4.5min and history 9min->4.6min; calls
  98,977->74,289 (25k phantoms gone). Doc regen ballooned to 30+ min twice; section
  instrumentation + timeline analysis pinned it to the O(n^2) co-change tail-merge
  (650k pairs ~ 1e11 probes). Fixed (sort + 256-pair working set): regen now 11
  SECONDS, co-change window 1.5s. Live docs verified: "37 state accesses use a
  VARIABLE key ... lower bound" (#17), arity + confidence stamps in index (#12/#13),
  blast radius discounts bare-name callers.

- **2026-06-13 iter 16 — accuracy program complete (#11-#19) + true co-change revealed**:
  language-aware binding (#19), C#/fallback arity (#13 done), dynamic SQL on both
  extraction tiers (#17 done, sidecar-path gap caught BY the verification cycle),
  16d crash-idempotent temporal walks. Full cycle now 13 min. Streaming co-change
  over all 1.3M edges revealed the biased-sample lie: true top pairs are
  text.*.resx bundles (2,463) and map.js <-> qtyManager.ts (2,632) — the prior
  246/1112 numbers were key-order sampling artifacts. calls stable at 74,289.

- **2026-06-13 iter 17 — detect_incomplete_changes LIVE (the original ask, closed)**:
  editing map.js alone now reports, with evidence: qtyManager.ts (877 co-changes),
  map.aspx (748), index.vbhtml (656), ioMarkerTypeFilterManager.ts (538), vsMap.ts
  (441) — the 'forgot the other places' detector deployed, tested 3-ways, and
  mandated by the generated workflow + hooks (installed into OciusX). Dynamic SQL
  verified on sidecar path (real ADO hit in copycodelist.aspx.vb).

- **2026-06-13 iter 18 — #29 + #48 shipped and live**: edit-session bookends verified
  on OciusX (begin on map.js returns the 5-partner expectation brief upfront);
  golden-query retrieval scorecard baseline 9/10 hit@5 (RoQ miss = sensitivity
  marker); workflow pack with bookends installed. Engram lifecycle now complete:
  plan_user_story -> begin_edit_session -> check_edit_safety ->
  complete_edit_session -> pre_commit_review.

- **2026-06-13 iter 19 — ask_codebase live + #34 hot-path hints**: all three probed
  intents route correctly on OciusX (impact: CheckIsUserInRole callers enumerated;
  usage: ss_systemsettings refs; explain: clustering search+footprint); eval
  regression 9/10 unchanged. search/references/blast now chain into the session
  workflow via next: hints. P3 surface complete: one front door + full lifecycle.

- **2026-06-13 iter 20 — multi-window MCP fixed (#38) + integrity correctness, all live**:
  user hit single-instance lock contention (only one CLI window worked). Implemented +
  hardened Windows named-pipe daemon transport; default now multi_client=true; verified
  two processes share one daemon on OciusX. Also: integrity false-positive TantivyOrphan
  + overall_healthy masking fixed (live OciusX before/after), VectorShortfall detection
  (#45/#39), grep TTL cache (#46), symlink-robust paths (#47), #62/#63 hygiene. All
  deployed; OciusX check_integrity now genuinely clean (mismatches: []).

## Suggested sequencing

**Week 1–2 (quick wins, mostly S):** #1, #5, #7, #11, #15, #16, #18, #34, #40, #41, #44, #47, #54, #56, #60, #63
**Month 1:** rest of P0 (#2, #3, #4, #6, #8) + #9, #10, #12 — after this, every agent
interaction is materially better and the graph is trustworthy.
**Month 2:** P2 generalization (#23 TS/Python first, #24, #25) + #28 `ask_codebase` + #36 git perf.
**Month 3+:** remaining P1 deep accuracy (L items), P3 workflow tools, P5 eval harness
(start earlier if ranking changes land — don't tune retrieval blind).

- [x] **64. Resolve the unused DOC_BY_ID integrity comparison** (M)
  Original finding, 2026-06-13 live OciusX check: production indexing did not write
  per-doc DOC_BY_ID records, leaving the document-store comparison gated off by
  its zero count. The original alternatives were to populate those records during
  index_docs or remove the unused comparison and document actual coverage.
  Completed using the latter: runtime removal already shipped in `009295e`;
  documentation reconciled 2026-11-13. Integrity compares Tantivy/vector counts
  and reports graph cardinalities; it does not validate graph references or
  document-store rows. Managed source counts use the published source selection;
  matching counts do not prove row identity, vector quality or history completeness.
  See [installed source integrity](eval/ociusx-source-integrity-delivery-20261112.json)
  and [documentation completion](eval/ociusx-integrity-documentation-20261113.json).

---

## Loop log (continuous improvement, validated against OciusX)

- **2026-06-12 iter 1 — LINQ/ORM DAL extraction (VB)**: `enrich_vb_source` now emits
  `queries_table` edges for LINQ-to-SQL DataContext usage (ctx decl/assign tracking,
  member-access table refs, write-method detection, orm/access metadata).
  *Verified: OciusX queries_table 67 → 1,967; ss_systemsettings consumers 0 → 14.*
- **2026-06-12 iter 2 — FQN ancestor-duplication fix**: sidecar `ComposeName` uses the
  FQN already on the types stack; Rust `dedupe_fqn` normalizer as defense-in-depth.
  *Verified: central nodes show `_api2.Logger.LogError` (was `_api2._api2.…`).*
- **2026-06-12 iter 3 — get_gis_inventory tool**: map API usage grouped by
  library.class with call sites/files/modern equivalents + per-file map configs +
  WMS/XYZ/Esri layer inventory, from existing spatial_call/gis_config extraction.
- **2026-06-12 iter 4 — get_gis_inventory verified live**: 489 real call sites with
  per-(library,class) counts matching grep ground truth exactly (Marker 40, Point 73,
  Polygon 13); call-site counts preserved through edge dedup via count metadata.
- **2026-06-12 iter 5 — JS phantom-source fix**: all 12 JS bridge emitters sourced
  edges from file:{basename} (disconnected phantoms). Now the "file" sentinel ->
  ingest substitutes the true rel path. *Verified: full paths in GIS inventory.*
- **2026-06-12 iter 6 — real embeddings**: deployed config flipped to Ollama
  nomic-embed-text (768-dim); 28k OciusX chunks embedded for real semantic search.
- **2026-06-12 iter 7 — git history indexed**: 6,761 commits -> 1,307,942 temporal
  edges, 15 reverts. find_similar_changes verified on real PRs (page+codebehind+
  App_Code companions). co_change_pairs + auth_summary now populate the generated
  CLAUDE.md (verified: api-broker.vb <-> map.js 246 co-changes; 5 exact role
  literals; checkisuserinrole 66x).
- **2026-06-12 iter 8 — scan scoping**: 1.3M temporal edges made full-table scans
  toxic; list_structural_edges + phase-2 per-kind scans skip them.
- **2026-06-12 iter 9 — tools**: find_connection_path (#14), find_dependency_cycles
  (#20), resolve_graph_edges; access-layer first-match hazard fixed (#11); blast
  polymorphism weight (#15); file Contains edges (#16); fallback tagging (#18);
  centrality cache fixed (#41).
- **2026-06-12 iter 10 — vendor gating**: bower_components/node_modules/*.min.js no
  longer feed the graph (font-awesome bare-name phantoms). Reindex died from
  load-starved Ollama (-> 16c queued, timeout 30->120s); recovery chain running.
- **2026-06-12 iter 11 — wedge root-caused + fixed**: both reindex failures were a
  blocking writeln! into the sidecar stdin pipe (child dead, inherited handle keeps
  pipe alive -> 0-CPU hang holding the sidecar mutex). Fixed: liveness check, 2MB
  source cap -> fallback, threaded write with 60s timeout. *Verified: reindex
  completes in ~8 min (old 30-min estimate was build-contention).*
- **2026-06-12 iter 12 — embed cache landed**: CachedEmbedder (redb, model_tag +
  blake3 keys, cross-project) wraps remote embedders; copy-forward and reindex
  re-embeds become cache hits. 2 behavioral tests.
- **2026-06-12 iter 13 — vendor gating verified live**: graph 49,470 nodes (vendor
  symbols gone), GIS 466 clean call sites, path probe routes through app code only
  (font-awesome phantom eliminated). find_dependency_cycles on real data: 31
  components, top = 39-function cycle in map.js. Driver shutdown fixed (stale
  tantivy lock killed follow-on phases).
- **2026-06-12 iter 14 — history failure triple-fixed, FULL BRAIN COMPLETE**: error
  masking removed (consumer root error surfaces) -> revealed LockBusy -> bulk-writer
  retry -> revealed Ollama 400 on oversized history docs (50k diffs / 200k
  anti-pattern vs 2048-token context) -> 8k-char clamp + truncate:true + 16c
  degrade-not-die vectors. *Verified on 9933d04d: 6,761 commits, 1,307,942 temporal
  edges, diagnostic ok, 163,816 vectors = full corpus embedded, history in ~9 min.
  Final state: 49,470 nodes / 1.35M edges / 163,815 docs, all green.*
