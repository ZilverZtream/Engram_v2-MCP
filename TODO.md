# Engram TODO — Ordered by Value to an AI Agent

Engram's product is *an AI model's working memory of a codebase*. So every item below is
ranked by one question: **how much does this improve the answers an agent gets back, per
token spent, per call made?** Tiers are strictly ordered; items within a tier are ordered
too. File references come from a full-codebase review (2026-06-12); verify line numbers at
implementation time.

Effort: **S** = hours, **M** = days, **L** = a week+.

---

## P0 — Fix the core agent loop (search → understand → edit)

These degrade or improve *every single interaction*. Nothing below matters until these are right.

- [x] **1. Return line numbers and untruncated snippets from search** (M) — *done 2026-06-12: HybridHit carries start/end_line + snippet_truncated; vector hits backfilled from Tantivy; truncation markers name get_chunk.*
  `engram_index/src/hybrid.rs`, `handlers/search_tools.rs`. Search hits omit
  `start_line`/`end_line` and truncate snippets at ~300 chars. An agent that can't jump
  straight to the location must spend a second tool call (and re-read the file) for every
  hit. Every result should carry: path, line range, symbol name + kind, full chunk text
  (or an explicit `truncated: true` + a handle to fetch the rest).

- [x] **2. Ship real semantic embeddings by default — or degrade honestly** (M) — *done 2026-06-12 (degrade-honestly branch): SemanticQuality labeling in search/vector/project_health + startup warning; also fixed embedding_backend=ollama being rejected at engine construction. Bundling a real local model remains open (see P2).*
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

- [~] **4. Uniform response envelope: token budgets, truncation metadata, pagination** (L) — *partial 2026-06-12: utils::envelope footer + labeled truncation markers on the hot read tools (search_memory, vector_search, get_chunk, find_symbol_references, grep_project). Remaining: max_tokens request param, cursors, and rollout to the other ~180 handlers.*
  All 13 handler files (~193 handlers). Responses are heterogeneous markdown/text with
  silent truncation (`graph_tools.rs` hardcoded limits, `access_layer_tools.rs` 30KB+
  contexts, unbounded migration reports). Define one envelope: optional `max_tokens`
  request param honored everywhere, `truncated`/`total_count`/`cursor` fields, and
  `output_json` consistently available. An agent's context is its scarcest resource;
  silent truncation is the worst failure mode because the agent doesn't know it's missing data.

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
  overview lists most-inherited types. Remaining: tree-sitter languages (TS/Python/Rust/Java),
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

- [~] **12. Confidence metadata on edges, threaded into tools and ADP gate 1** (M) - *partial 2026-06-13: ingest target-binding stamps resolution+confidence (exact_same_file 0.98 ... batch_unique_any_terminal 0.35); find_path/find_connection_path surface LOW CONFIDENCE warnings on weak hops. Remaining: blast-radius weighting, resolve_symbol_edges step stamping, ADP gate 1 consumption.*
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

- [ ] **13. Overload/arity-aware call resolution** (M–L)
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

- [ ] **16b. rebuild_graph mode: re-extract without re-embedding** (M)
  Extractor changes currently require delete+reindex: new project id, full Ollama
  re-embed (~30 min), git history re-run (~50 min). Add an update_project mode (or tool)
  that re-runs extraction + process_ingest_stats from disk for ALL files but leaves
  search/vector stores untouched (content unchanged); scoped purge + successor remap
  already preserve cross-file and temporal edges. Turns extractor iteration from ~80 min
  into ~3 min and keeps the project id stable.

- [ ] **16c. Indexing must survive embed-backend failure** (M)
  Observed 2026-06-12: machine contention starved Ollama (3x30s retries exhausted) and
  the whole index job died mid-walk (9.5k/28k chunks, 0 graph nodes) and git-history's
  consumer dropped. Embed failure should degrade that BATCH to fts-only + count it in
  the report ("N chunks unembedded — rerun repair_project vector_only"), never kill the
  job. Graph extraction must complete regardless of vector health. Mitigations applied:
  embedding_request_timeout_secs 30->120 in deployed config; embed cache (in progress)
  makes retries cheap.

- [ ] **17. Mark dynamic state/SQL access instead of skipping it** (M)
  `state_extractor.rs`, `sql_parser.rs`. `Session[variableName]` and string-built SQL are
  silently dropped. Emit `dynamic: true` unresolved-access nodes/edges so data-flow and
  state-migration reports can say "plus N dynamic accesses we couldn't resolve" — agents
  need to know what the graph *doesn't* know.

- [x] **18. Flag VB Roslyn sidecar fallback in extraction metadata** (S) - *done 2026-06-12: fallback_extract_vb stamps extraction_fallback=true on every symbol; sidecar obj/ now gitignored; sidecar deploy documented in memory.*
  `vb_extractor.rs`. Sidecar timeout silently falls back to tree-sitter; the resulting
  lower-fidelity nodes are indistinguishable. Tag them (`extraction_fallback: true`) so
  confidence (#12) reflects it. Also: `tools/vb_roslyn_sidecar/obj/` is untracked at repo
  root — confirm the sidecar is wired, documented, and built in CI.

- [ ] **19. Cross-language (C#↔VB↔ASPX) interop edges with confidence** (M)
  `graph_service.rs` (App_Code resolution). Same-language, bare-name matching only.
  Record language-boundary crossings explicitly with lowered confidence.

- [x] **20. Cycle/SCC detection surfaced in impact analysis** (M) - *done 2026-06-12: tarjan_scc over Calls/Dependency/Imports (placeholders + statistical kinds excluded; 3 tests) + find_dependency_cycles tool (named members, binding kinds, migration guidance). Blast-radius integration deferred until a cached SCC index exists.*
  `engram_graph` (analysis), `blast_radius_service.rs`. Tarjan SCC post-ingest; flag
  cycles in blast radius and migration ordering (cycles are exactly where naive
  strangler-fig plans fail).

- [ ] **21. Return-type / parameter-type edges** (L)
  `parsing.rs`, extractors. Enables type-boundary reasoning (strangler-fig seams, "what
  flows into this method"). Big, but unlocks the next tier of graph value.

- [ ] **22. Lightweight def-use (data-flow) edges** (L)
  New post-ingest pass. Today data-flow is call edges + state/SQL edges only. Even
  method-local def-use with cross-method parameter linking would let `trace_data_flow`
  answer lineage questions agents currently have to read source for.

---

## P2 — Generalize beyond legacy .NET

Engram's pitch is "a brain for *any* project", but deep extraction (calls, state, UI,
SQL, lifecycle) is WebForms/.NET-only; the other 9 tree-sitter languages get shallow
symbol extraction, and ~56 service files hardcode WebForms concepts. This is the
strategic gap between "niche migration tool" and "every agent's first MCP server."

- [ ] **23. Second-pass call resolution for TypeScript, Python, Rust, Go, Java** (L, per language)
  `parsing.rs`. Tree-sitter already extracts calls; add per-language import/module
  resolution so bare calls link to FQNs (TS: import paths + tsconfig paths; Python:
  imports; Rust: `use` + crate paths; Go: packages). Start with TypeScript and Python —
  that's where most agent work happens today. Reuse the existing two-pass FQN
  architecture from the .NET side.

- [ ] **24. Project-type detection + capability gating** (M)
  `project_service.rs`, `capabilities.rs`. Detect ecosystem at index time (csproj/sln vs
  package.json vs Cargo.toml vs pyproject). Use it to: gate WebForms/migration tools out
  of the visible tool list (#3), pick extractor presets, and tailor
  `get_codebase_overview` output.

- [ ] **25. Language fixture corpora beyond WebForms** (M)
  `tests/fixtures/` has only 4 fixtures, all `dotnet_webforms_*`. Add small
  ground-truth projects for TS/Python/Rust/Go with expected symbols + call edges, wired
  into CI. Without these, #23 can't be verified and regressions are invisible.

- [ ] **26. Audit the 56 WebForms-biased service files for non-.NET behavior** (M)
  Services hardcode Page_Load/ViewState/Master Pages. Each must either no-op cleanly with
  a clear "not applicable to this project type" message, or generalize (e.g.
  `produce_claude_md`, `pre_commit_review` gates, coding-style analysis should be fully
  language-neutral). Today an agent on a Node repo can call tools that quietly return
  nonsense.

- [ ] **27. Generalize state tracking to modern equivalents** (L)
  `state_extractor.rs` knows Session/ViewState/Application. Equivalent value for modern
  stacks: env vars, global singletons, React context/Redux stores, module-level mutable
  state. Same edge kinds, new extractors.

---

## P3 — Agent workflow & orchestration tools

The unbuilt AGENT_ENHANCEMENT_SPEC phases 39–45 (33 tools, zero implemented). Not all are
worth building — these are, in this order. (Skip ones that duplicate existing surface:
`find_hardcoded_secrets` ≈ secret gate, `calculate_change_risk_score` ≈ `check_edit_safety`.)

- [ ] **28. `ask_codebase` — one natural-language entry point (spec 43-1)** (L)
  Routes a question to search + graph + access-layer internally and returns a synthesized,
  cited answer. This is the single highest-leverage *new* tool: it collapses the 114-tool
  selection problem for the 80% case and makes Engram useful to agents that never learn
  the full surface. Deterministic router first; optional LLM synthesis later.

- [ ] **29. Edit Session Protocol (spec 45: begin/complete_edit_session, detect_incomplete_changes)** (M)
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
  URL/route → handler → SPs → tables, end to end. Mostly composes existing edges; huge for
  "where do I start" questions.

- [ ] **32. `one_shot_feature_plan` (spec 42-1)** (L)
  Compiles a full pre-edit dossier for a feature request (relevant files, patterns,
  safety, test plan). Build after #28/#30 exist to compose them.

- [ ] **33. Batch/multi-get endpoints** (M)
  `access_layer_tools.rs`. `get_method_info` × 10 = 10 round trips. Accept arrays in the
  hot read tools (method info, node lookups, doc fetches) with a shared token budget.

- [ ] **34. Next-tool hints in every response** (S–M)
  Some tools have `Next:` recommendations; most don't. Standardize a `next_tools` field —
  this is how agents discover the long tail of the surface organically.

- [ ] **35. Database Oracle remainder (spec 39: query_schema, detect_n_plus_one, find_missing_indexes)** (M–L)
  For data-heavy projects. `query_schema` first — agents constantly need "what columns
  does this table actually have."

---

## P4 — Performance & reliability

A slow or flaky brain gets dropped from the loop. These are mostly known issues with
specs already written.

- [ ] **36. Implement the 4-task git-history indexing perf plan** (M — already fully specced)
  `docs/superpowers/plans/2026-04-16-index-git-history-perf.md`. Tree-ID precheck, drop
  BTreeSet, bulk IndexWriter, batched channel pipeline. 50–80% projected gain; git
  indexing measured in hours on big repos is an adoption blocker.

- [ ] **37. Indexing progress reporting an agent can poll** (M)
  `project_tools.rs`, `git_tools.rs`, job service. Long indexing returns nothing until
  done; agents assume failure and retry. Persist per-phase progress (files done/total,
  current phase, ETA) and expose via `get_job_status`.

- [ ] **38. Windows named-pipe transport for the multi-client daemon** (M)
  `multi_client.rs`. Unix-socket only; on Windows (where this repo lives!) Claude
  Desktop + Claude Code can't share a daemon.

- [ ] **39. Vector/Tantivy consistency repair after partial ingest failures** (M)
  `hybrid.rs`, `ingest.rs`. Partial failure can leave FTS and vector stores divergent,
  silently skewing hybrid results. Detect (count sentinels per generation) and re-converge.

- [ ] **40. Trigger generation GC after crash recovery** (S)
  GC is schedule-only; crash-resume loops accumulate stale generations/segments until
  disk and latency suffer.

- [x] **41. PageRank/centrality cache warming** (S) - *done 2026-06-12: get_or_compute_centrality (cache-or-compute-and-persist) used by blast radius + get_centrality; index_project warms the cache post-resolve; stale EdgeKind exhaustiveness count fixed (40->43).*
  Centrality reranking blocks on first use. Compute post-ingest, persist, refresh lazily.

- [ ] **42. Harden `business_logic_service` LLM integration** (M)
  Silent failure when Ollama is down, no timeout/backoff, post-hoc validation only.
  Degrade visibly ("LLM unavailable — returning deterministic summary only") and make
  backend/model explicitly configurable.

- [ ] **43. Replace panics on safety-critical service paths with errors** (S–M)
  Services layer review found `panic!`/`unwrap` on paths reachable from tool calls. A
  panic in a tool call looks like a dead server to the client.

- [ ] **44. Fix job-cancellation checkpoint marking** (S)
  `job_service.rs`. Failed `mark_checkpoint_cancelled` is ignored → cancelled jobs get
  resumed by the next agent.

- [ ] **45. LanceDB integrity sentinels** (M)
  `integrity_service.rs` covers Tantivy+Redb only; vector-store corruption is invisible.

- [ ] **46. grep_project freshness cost** (M)
  `grep.rs` freshness check is O(files); cache fingerprints per generation.

- [ ] **47. Symlinked roots + Windows path-separator polish** (S)
  `engram_core/src/paths.rs`. `strip_prefix` fails under symlinked roots; error messages
  show `/` on Windows.

---

## P5 — Quality infrastructure (prove the brain is right)

There is currently no ground-truth evaluation that search or the graph return *correct*
answers — only behavioral tests that they return *something*. For a tool whose whole
value is accuracy, this is the biggest meta-gap.

- [ ] **48. Retrieval eval harness with golden query sets** (M)
  Per fixture project: N queries ("where is login validated?") with expected files/symbols;
  measure precision@k / MRR in `benchmark-ci.yml`. Without this, every ranking change
  (#2, P0 items) is flying blind.

- [ ] **49. Graph-accuracy eval: expected call/state/SQL edges per fixture** (M)
  Extend fixtures with ground-truth edge lists; report extraction recall/precision per
  language per extractor. Gate CI on regressions.

- [ ] **50. End-to-end MCP-layer tests for the top 20 tools** (M)
  ~50 integration test files exist but mostly call services directly. Spin the actual MCP
  server, call tools as a client would (bad project_id, huge repo, empty index) and
  assert response envelope contracts (#4).

- [ ] **51. Finish the EQS gate plan** (per `eqs_gate_requirements.md`) (M)
  Target 4.5+, currently ~2.5–3.0 with all EXH findings closed. The remaining gates
  overlap heavily with #48–50 — do them together.

- [ ] **52. Latency budgets in CI** (S)
  Assert p95 per hot tool (search, get_method_info, blast radius) on the fixture corpus
  so perf regressions surface before users feel them.

---

## P6 — Adoption & packaging

- [ ] **53. Prebuilt release binaries + one-command install** (M)
  Today: `cargo build --release` (30+ min, libgit2 headers, Rust toolchain). Add GitHub
  Releases with win/mac/linux binaries and an `npx`/installer wrapper that also writes
  the MCP config block. Most potential users are lost right here.

- [ ] **54. Quickstart: clone → indexed → first query in 5 minutes** (S)
  README has MCP config samples; tie it together with a copy-paste path per OS, expected
  timings, and "how do I know it worked" (point at #5 freshness tool).

- [ ] **55. Large-repo tuning guide** (S)
  `tantivy_writer_memory`, embedder choice, git-history depth, watcher debounce — one doc
  page with recommended values by repo size.

- [ ] **56. Defaults audit** (S)
  `config.rs`: default embedder model name (see #2), debounce, memory budgets — make the
  zero-config experience the recommended one.

- [ ] **57. Doc drift pass** (S–M)
  `docs/ROADMAP.md`, `TOOL_PARITY.md`, `TOOL_CONTRACT.md`, README claims vs. the 114
  actual tools; mark AGENT_ENHANCEMENT_SPEC phases 39–45 as roadmap-not-built (an agent
  reading the spec today would believe tools exist that don't).

- [ ] **58. IPC protocol version negotiation** (S)
  `multi_client.rs` has PROTOCOL_VERSION but no mismatch handling for mixed-version
  daemon/proxy.

---

## P7 — Debt & cleanup

- [ ] **59. Consolidate the 3 overlapping DB-analysis services** (M)
  `database_intelligence_service`, `db_strategy_service`, parts of
  `full_project_migration_service` re-implement SQL parsing/table extraction.

- [ ] **60. Remove or document alias tools** (S)
  `dream_project`/`trigger_rem_cycle` etc. Every redundant tool worsens #3.

- [ ] **61. Shallow tools: fix or relabel** (S–M)
  `analyze_error_stack` (just builds a search query), `get_instrumentation_pack` —
  either deepen them or make descriptions honest so agents don't budget tokens on them.

- [ ] **62. TODO/FIXME sweep through services** (S)
  Services review found clusters of TODOs and partially-wired repair logic
  (`integrity_service` repair paths). Triage: fix, ticket, or delete.

- [ ] **63. Repo hygiene** (S)
  Commit or ignore `tools/vb_roslyn_sidecar/obj/`, decide fate of `.superpowers/` and
  stray `fix_defaults.ps1`; `full_project_migration_service` exists as both file and
  directory module — pick one.

---

## Suggested sequencing

**Week 1–2 (quick wins, mostly S):** #1, #5, #7, #11, #15, #16, #18, #34, #40, #41, #44, #47, #54, #56, #60, #63
**Month 1:** rest of P0 (#2, #3, #4, #6, #8) + #9, #10, #12 — after this, every agent
interaction is materially better and the graph is trustworthy.
**Month 2:** P2 generalization (#23 TS/Python first, #24, #25) + #28 `ask_codebase` + #36 git perf.
**Month 3+:** remaining P1 deep accuracy (L items), P3 workflow tools, P5 eval harness
(start earlier if ranking changes land — don't tune retrieval blind).

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
