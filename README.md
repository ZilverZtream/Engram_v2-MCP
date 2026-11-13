# Engram MCP

A graph-aware codebase context server for AI coding agents. Engram is a
[Model Context Protocol](https://modelcontextprotocol.io) server, written in
Rust, that gives any MCP-capable agent (Claude Desktop, Claude Code, Codex,
your own SDK client) a persistent, evidence-backed picture of a real
codebase: a typed knowledge graph, full-text and semantic search, git
temporal intelligence, a revert-derived anti-pattern index, per-file coding
conventions, a deterministic pre-commit review, a CLAUDE.md generator, a diff
narrator, and an Autonomous Decision Protocol for agents that want to edit
without asking every time.

- **Offline operation is available.** Core retrieval and review can run without
  LLM generation. Configured embedding providers can make network requests, and
  optional generation/ranking features can use an LLM. The quickstart uses
  `fts_only` and `llm_backend: none` for an offline source-query baseline.
- **Language-aware.** Tree-sitter parsers for Rust, Python, Go, Java, C,
  C++, C#, VB.NET, TypeScript, and JavaScript; custom extractors for
  ASP.NET WebForms (ASPX / ASCX / Master), Classic ASP, T-SQL, SSRS,
  Crystal Reports, Google Maps, and Esri ArcGIS JS.
- **Project-scoped workflows.** Project tools select an index with `project_id`.
  Runtime discovery has no project ID, and knowledge recall can include explicitly
  shared user memory; inspect the returned namespace and provenance.
- **Generation-based recovery.** Generation-based indexing, Blake3
  fingerprints, Redb-backed durable checkpoints, integrity sentinels with
  auto-repair, and a multi-client dispatcher that lets several agents
  share one daemon. Inspect reported recovery requirements and operation status.

---

## What Engram gives an agent

| Agent failure mode | Engram response |
|---|---|
| Invents method signatures | Indexed declarations and relationships, queryable by name or FQN with extraction and freshness limits |
| Touches a high-impact file blindly | `compute_blast_radius` returns a 1–10 score with incoming / outgoing / downstream counts |
| Re-introduces a reverted pattern | Detected reverted changes supply anti-pattern evidence; `immune_check` scores new code against the indexed evidence |
| Changes a file but not its coupled partner | Temporal coupling surfaces files that change together in git history |
| Ships a regression | `pre_commit_review` runs 19 deterministic gates over the staged diff, with secret redaction and CI-stable finding IDs |
| Adds a public endpoint that skips the admin check | `guard_parity` gate + `map_guards_and_settings` compare new code against the sibling guards and settings that gate the area |
| Misses implementation or companion files | `plan_user_story` → `get_concept_footprint` → `find_similar_changes` return candidates with evidence and coverage limits |
| Gets lost in a 5,000-file legacy codebase | `get_codebase_overview`, `generate_migration_blueprint`, `analyze_full_project_migration` |
| Writes project-context guidance from scratch every session | `produce_claude_md` generates a `CLAUDE.md` and `.claude/rules/*` from indexed signals |
| Can't explain the diff it just produced | `explain_change` narrates a diff as commit message, PR description, or changelog |
| Edits autonomously with no audit trail | Autonomous Decision Protocol — 8-gate verdict pipeline with progressive rollout and a kill-switch |

---

## Capabilities

Project workflows use `project_id` to select a registered project, and individual
tools document their prerequisites and filesystem scope. Discovery and
runtime-wide operations, such as `list_projects` and `get_metrics`, do not require
a project ID.

The sections below group review, planning, context and operational capabilities.
Their order is not a measured ranking of agent benefit. The
[recorded OciusX task results](eval/ociusx-current-task-evaluation-20261112.json)
describe the measured outcomes and limits of the November 12 regression run.

### Pre-commit review (`pre_commit_review`)

Nineteen deterministic gates run over a unified diff (raw text, `staged`,
`unstaged`, `head`, or a `.patch` path) and emit severity-ranked findings
with concrete evidence and fix suggestions. No LLM reranking is used; configured
embedding search can contact its provider. Runtime depends on diff size and
available index coverage.

| Gate | What it catches |
|---|---|
| `immune` | Modifications to files flagged by prior reverts; CRITICAL when paired with destructive patterns |
| `repo_rules` | Applicable repository rule evidence and mechanically detectable violations |
| `blast_radius` | High-impact files where a change ripples outward |
| `style` | Per-file convention violations (naming, `Using` blocks, `Try/Catch`, `SafeRedirect`, etc.) derived from the file's current code |
| `temporal` | Strongly-coupled partner files missing from the diff |
| `state` | Session / ViewState / Application / Cache keys touched by the diff, with their other readers and writers |
| `audit` | Database mutations missing the project's established audit-log convention |
| `antipattern` | Added code that resembles indexed anti-patterns (reverted diffs) via hybrid search |
| `new_file` | New files that break the folder's extension / prefix conventions; ASPX pages without codebehind |
| `test_coverage` | Non-test files changed without their coupled test file |
| `secret_leakage` | Hardcoded AWS / GitHub / OpenAI / Anthropic / JWT / connection-string secrets — redacted in output |
| `guard_parity` | Missing guards compared with relevant sibling code |
| `unwired` | Added code that appears disconnected from its consumers |
| `product_intent` | Changes that conflict with retrieved product-intent evidence |
| `sync_contract` | Synchronization-contract mismatches |
| `co_added_family` | Missing members of historically co-added file families |
| `complexity_budget` | Added complexity requiring review |
| `added_conventions` | Conventions around newly added declarations |
| `ui_house_style` | Advisory differences from nearby application markup |

Every finding carries a stable ID (for run-to-run tracking in CI), specific
line numbers, diff-context snippets, evidence lines (graph data, revert
hashes, coupling weights, convention statistics), an actionable fix
suggestion, and a `Next` tool recommendation. Output is either rendered
Markdown with a single 🟢 / 🟡 / 🔴 verdict badge, or a stable JSON payload
(`output_json: true`) for CI pipelines.

The default review also retrieves bounded prior-review context from changed paths
and identifiers. JSON includes `prior_review_context`; Markdown presents the same
evidence separately from findings. Retrieval examines at most 24 indexed candidates
and returns at most eight items within a 16 KiB compact-JSON context budget. Exact
file references, basename matches and shared review/thread identifiers describe
relevance, not an exception's applicability. Indexed discussions may already be
clipped; missing source URLs, unassociated events and budget omissions are explicit.

Budget-omitted indexed summaries retain a `recovery` call to `get_chunk` in citation
mode, bound to the exact document ID and observed raw-content hash. Follow its
UTF-8 byte continuations to recover the stored summary; a changed document is
rejected instead of silently substituted. This does not reconstruct discussion
missing from a legacy summary. Standalone decision omissions retain their existing
event-history recovery. Recovery metadata shares the same 16 KiB context budget.

Workflow status and LLM dismissal are not accepted policy decisions. Earlier
`wontFix` patterns no longer reduce anti-pattern finding severity. Existing imported
events remain intact, and a later explicit decision survives a rescan. Imported
authors, URLs and external verification declarations are not authenticated; matching
an attested commit to HEAD does not verify changed worktree code. This context is a
current observation, not a historical replay; requested cutoff/revision binding
remains unfinished. Ordinary review also discovers explicitly recorded decisions
whose current rationale or finding ID mentions a complete changed relative path,
without requiring an indexed review thread. Discovery inspects at most 64 identity
records, reads at most 64 KiB per event array and 256 KiB in total, and shares the
eight-item/16 KiB response limits. Invalid records, exhausted limits and omissions
are explicit. Older hashed rows contain no recoverable review ID: they remain
readable by their original ID but cannot be discovered until their unchanged
events are recorded again with that exact ID. Relevance establishes no authority,
exception scope or absence of other rulings. Complete recorded events can be
recovered with `get_review_decisions`. New verdict imports also retain full supplied
discussion captures: exact original JSONL lines, or canonical complete provider
thread JSON before system/blank-comment filtering. Search summaries retain hash-bound
recovery references; `get_review_decisions` with `discussion: {}` lists capture
references and bounded catalog continuations. A thread/hash selector returns bounded
UTF-8 pages of a deterministic recovery JSON bundle. Assemble pages and verify
`recovery_blake3`, then hash the exact `capture_metadata_json` string against the
selected `capture_blake3`. Decode that metadata and verify `raw_utf8` against its
`raw_utf8_blake3` and byte length. Small provenance captures reference immutable
raw bodies by content hash: changing another JSONL line never duplicates an
unchanged thread body. Old capture/body hashes remain recoverable after reimport
changes a summary. Continuations bind the recovery-document hash as well.
This preserves supplied evidence, not authenticated people, accepted scope, original
HTTP wire bytes, unreturned provider history or an as-of decision. Legacy uncaptured
summaries cannot reconstruct omitted discussions.

Imports refuse before summary publication beyond 256 KiB per thread, 4 MiB per
encoded capture batch or 1024 input rows. The project retains at most 4096 total capture-metadata/raw-body objects
and 128 MiB of logical keys, values and link headers across both tiers, transactionally accounted; no automatic
deletion or quota increase occurs. Exact duplicate captures consume no additional
quota; unchanged raw payloads consume no additional body storage when provenance changes. Provider fetches additionally bound HTTP bodies, aggregate bytes, pages and
elapsed waiting. Captures are stored before indexing; if later indexing fails,
retained evidence and its quota remain visible through the discussion catalog,
and publication is reported as unconfirmed rather than rolled back. Input/read
reservations are logical estimates, not a hard process-memory or RSS ceiling.

### Immune system (revert-derived anti-patterns)

Every reverted commit in the git history is treated as a lesson. The
immune actor harvests each revert, extracts the pattern that was undone,
and indexes it as an anti-pattern with the reverting commit hash attached
as evidence.

| Tool | What it does |
|---|---|
| `analyze_reverts` | Walks git history, detects reverts, generates `immune_*` repo rules |
| `immune_check` | Scores proposed code against the anti-pattern index (hybrid FTS + vector) and returns matched rules with revert hashes |
| `anti_pattern_guard` | Pattern matching with remediation guidance and the originating commit |
| `dedicated_antipattern_index` | Maintenance — keeps the anti-pattern index fresh |

For example, `immune_check` can flag proposed code resembling a reverted
`DeleteAllOnSubmit` change and cite the indexed revert. `pre_commit_review` can
report that evidence as a finding. Commit blocking requires the client or CI
workflow to enforce the result; the tool does not intercept Git commits on its own.

### Autonomous Decision Protocol

Eight-gate mandatory pipeline for autonomous edits:

1. Extraction confidence — do we actually understand the method?
2. Trace certainty — are the call / state traces verified?
3. Safety policy — does the policy allow this class of edit?
4. Retrieval quality — did search return relevant context?
5. Blast radius — is the change contained?
6. Anti-pattern clearance — does it avoid indexed reverts?
7. Runtime evidence — does observed behaviour support the edit?
8. Evidence sufficiency — is there enough signal to commit?

Three verdicts: `allow` / `deny` / `abstain`. Progressive rollout —
`shadow` → `advisory` → `guarded` → `autonomous` — with an emergency
kill-switch that forces every verdict to `deny`. Every decision is
written to an immutable JSON audit trail with inputs, gate outputs, and
final verdict for offline replay.

The default `shadow` rollout records decisions without enforcing them. A tool's
presence or an advisory result does not itself authorize or prove an edit safe.

This is the surface an agent framework drives when it wants to edit code
without a human in every loop.

### Knowledge graph (~40 edge kinds)

Typed graph across seven concern domains:

- **Code structure** — `Calls`, `Contains`, `Imports`, `Dependency`
- **Data access** — `QueriesTable`, `HasColumn`, `ForeignKey`,
  `CallsStoredProcedure`, `StoredProcReadsTable`
- **State flow** — `ReadsState`, `WritesState`, `StateAffinity`
- **UI wiring** — `ContainsUi`, `DataBinding`, `TriggersPostback`,
  `ManipulatesDom`, `FillsRegion`
- **Web surface** — `ExposesWebService`, `ExposesHttpHandler`,
  `ExposesWcfService`
- **Change coupling** — `TemporalCoupling`, `CoOccurrence`
- **Runtime evidence** — `ObservedRuntimeControl`, `ObservedRuntimeSql`

Query by type / name / file. BFS traversal up to 12 hops with edge-kind
filters. `compute_blast_radius` returns an advisory 1–10 score alongside
revision-admitted dependency evidence; `impact_analysis` shows potential
downstream exposure when a file or symbol changes. Neither simulates the
proposed edit or proves runtime breakage. `detect_design_patterns` surfaces
Repository / Factory / Singleton / Observer structures. Runtime-evidence
edges let you cross-check static extraction against actual observed
behaviour.

### Legacy modernization engine

End-to-end migration tooling for ASP.NET WebForms and Classic ASP
codebases:

- Control mapping catalog (WebForms → Blazor / React / Angular)
- Scaffold generation with real business logic pulled from graph edges
  (not invented)
- Database-strategy advisor; per-key state-migration recommendations
- Characterization test generation (NUnit / xUnit / MSTest)
- Strangler-fig infrastructure (YARP + feature flags + Polly)
- Runtime instrumentation code generation
- Topologically-ordered migration waves via Kahn sort
- `analyze_full_project_migration` — a single call that produces a
  comprehensive per-file migration report with a cross-cutting summary,
  suitable for handing to a team or a migration agent

### Git temporal intelligence

- `index_git_history` — libgit2-backed commit-graph indexing
- `analyze_temporal_couplings` — files that frequently change together
- `search_history` — structured search over commit messages and diffs
- The immune system (above) is built on top of this layer

Temporal coupling is the quiet superpower: it surfaces files that
aren't related by any import or call edge but are consistently changed
in the same commit — the kind of hidden coupling that turns a one-file
edit into a four-file regression.

History precedent search reports its actual baseline: semantic, degraded
trigram, or lexical fallback when vectors are disabled or absent from the build.
Text-mode history search reports the configured retrieval capability, including
lexical-only and degraded trigram modes; it does not claim that both lexical
and vector results contributed to each response.
The optional `rerank_timeout_ms` sets the LLM ordering budget (0 skips it;
default and maximum 90000 ms). It does not bound baseline retrieval. Unavailable,
failed or timed-out reranking preserves the baseline and historical scope.
Change-card retention separates document embedding profiles and is limited to
eight partitions and an estimated 32 MiB; this is not a process memory limit.
Change-card metadata also survives restart in the existing registry database,
independently of embedding profiles. It is scoped by repository, commit and
extraction implementation, with an 8192-row / 32 MiB logical storage budget,
64 KiB records and bounded batches. Cache hits perform no writes; new records
use insertion-order eviction. Uncacheable metadata and optional cache failures
fall back to Git extraction. Every request still traverses the requested
first-parent line; unavailable ancestors or root trees are reported as errors.
Cached immutable metadata does not establish that every historical subtree or
blob remains available for later source recovery. These budgets do not bound
the complete registry file or process memory.

### Hybrid search

Full-text (Tantivy) + vector (LanceDB) with Reciprocal Rank Fusion,
Maximal Marginal Relevance diversity, namespace / path / language
filters, and content preview. Standalone vector search with 3×
oversampling. Multi-language structured stacktrace parser with
frame-boosted search. Graph-aware search that expands hits along
configurable edge kinds so you get the method PLUS its callers, callees,
or temporal-coupling partners in one call.

### Per-method access layer

Precise per-method context retrieval so an agent can touch one method
without re-reading an entire file:

| Tool | Purpose |
|---|---|
| `get_method_info` | Signature, surrounding class, edges |
| `get_full_method_body` | Indexed method or explicit range, with source fingerprint status |
| `get_method_edit_context` | Source + immediate call / caller neighbourhood |
| `get_page_context` · `prepare_implementation_context` | ASPX-aware context for WebForms edits |
| `trace_user_request` | URL/page → authored handler candidates → indexed calls and database relationships |
| `batch_read` | Up to 16 method, identity and stored-chunk reads under one response budget |
| `query_schema` | Qualified, source-cited SQL table, column, key, index, view, trigger and procedure queries |
| `one_shot_feature_plan` | Source-cited WebForms/VB.NET feature proposal with existing-behavior findings, proposed edits and unresolved requirements |
| `detect_n_plus_one` | Source-cited candidates for database calls repeated by VB loops or WebForms row binding |
| `find_missing_indexes` | Review-only SQL index candidates from cited queries and source-declared index coverage |
| `validate_generated_code` · `validate_sql_fragment` | Pre-commit validation |
| `find_tests_for_method` · `find_dead_methods` · `check_edit_safety` | Discovery and safety checks |

Specialty-tool replies can include `next_tools` entries with a tool name, a reason,
and optional complete `arguments` copied from the request or cited result. For
example, a repeated-query finding can lead directly to its enclosing method's
edit context. Missing method identities lead to a source excerpt instead.
`next_tools_status` explains empty, unavailable, ambiguous or partial-source
outcomes. Suggestions are filtered to the active tool profile; backend and
project-data requirements still apply. Follow-ups are advisory and are not run
automatically.

Guidance yields to complete evidence and recovery data when a response budget is
tight. If even a guidance status cannot fit, the original result is returned
unchanged. Core replies, existing guidance, errors, non-object JSON, multipart or
media results, and text above 1 MiB retain their existing formats. Absence of a
hint does not establish that a task is complete or an edit is safe.

`complete_edit_session` distinguishes an omitted `edited_files` field from an
explicit empty list. Omitting it checks the session's planned files. Passing `[]`
declares no edits: every planned file is reported as unedited, and completeness
analysis is explicitly skipped. Completing a session never certifies the diff.

`trace_user_request` accepts an `entry_point` such as `POST /buy/42` and an
optional `action` such as `click btnBuy` or `load page`. It resolves literal
`MapPageRoute` registrations and physical pages from selected project sources,
preserves competing registrations, and follows authored WebForms bindings.
Expanded methods require matching source fingerprints and selected physical
revisions. Source excerpts retain guards and response expressions, with exact
source-recovery requests when an excerpt is incomplete.
Reached methods can also link literal `SqlCommand` stored-procedure candidates
to matching qualified SQL definitions and their lexical table references.
Database identity, dynamic commands and ORM translation remain unresolved.

The result is static navigation, not an observed execution trace. Dynamic routes,
optional route defaults, rewrites, inherited/dynamic event bindings and missing
database-trigger bindings remain explicit coverage limits. `include_db: false`
omits database relationships; `include_triggers: false` skips trigger coverage.
The complete JSON response defaults to a 16,384-token byte-estimate budget;
smaller budgets that cannot contain it fail with the required size.

`one_shot_feature_plan` accepts `project_id`, `feature_description`,
`target_stack: "webforms_vbnet"`, and `detail_level` (`Sketch`, `Detailed`, or
`ReadyToCode`). Optional `area_hint` and `model_after` help locate the relevant
area and existing pattern. It covers the WebForms/VB.NET application's JS/TS
and SQL files; other target stacks are not implemented. The tool is available
in the full catalog and is not added to the default core preset.

The configured model proposes requirements, reconciles them with admitted source,
and returns proposed files, DDL, ordered steps, validation checks and explicit
`planned`, `already_present`, or `unresolved` dispositions. Existing behavior can
justify a plan with no edits. `ReadyToCode` adds proposed code templates; it does
not authorize edits or establish runtime correctness. The tool never writes
application files or executes SQL.

A usable response is labeled `DRAFT_REQUIRES_SOURCE_AND_REQUIREMENT_REVIEW`.
Plans are limited to 12 new/modified files and 48 steps. Invalid model contracts
may receive one correction attempt using the same evidence and validation rules:
two normal generation requests, at most three with correction, plus the configured
provider's transport retries. The rejected proposal remains separately labeled
untrusted data. Provider failures, unresolved contract errors and source changes
return `INCOMPLETE` with no usable plan. Review citations, coverage, requirement
dispositions and semantic claims before implementing any proposal.

`batch_read` accepts `requests` containing a unique `request_id` and a `request`
with `tool` (`get_method_info`, `resolve_id` or `get_chunk`) and its ordinary
`arguments`. Results preserve input order and each underlying result or error.
`complete` describes delivery; inspect each row's `status` for read failures.
The shared `max_tokens` budget defaults to 8192 and is capped at 65536, estimated
as complete JSON UTF-8 bytes divided by four, rounded up. A result that does not
fit is omitted whole, with its ID, size and exact single-tool `recovery` call;
source text, citations and applicable guidance are never cut inside that result.
A budget too small for all IDs and recovery calls fails before executing reads.
The batch uses the existing read handlers sequentially and is not an atomic
snapshot. Each read and later recovery retains its own freshness semantics.

`query_schema` accepts `project_id` and a tagged `query_type`, for example
`{"kind":"table","name":"dbo.Orders"}` or
`{"kind":"column","table":"dbo.Orders","column":"CustomerID"}`.
Other kinds are `all_tables`, `tables_with_column` (`column_name_pattern`, a
literal substring), `tables_that_reference_foreign_key` (`table`, `column`),
`triggers_on_table`, `views_sourced_from`, `indexes_on_table`, `nullable_columns`,
`computed_columns` (each with `table`), and `stored_proc` (`sp_name`).
Results include a summary, structured declarations and source citations. Names
retain their schema; ambiguous or duplicate table definitions require resolution.
Use `limit` (1–100, default 25), `offset`, and returned `next_page` for paging.
The complete JSON budget defaults to 16384 estimated tokens, maximum 65536;
insufficient budgets fail instead of cutting a declaration or coverage warning.

Only selected indexed SQL files whose current contents match their fingerprint
and physical revision contribute evidence. Changed, unreadable and unsupported
sources remain visible as coverage gaps. This is a SQL Server source inventory:
it does not execute migrations or establish the deployed database schema.
Missing nullability stays unknown; `nullable_columns` returns explicit `NULL`.
Procedure/view references are lexical candidates, and unqualified relationships
do not establish a default schema. Table queries also return bounded, freshly
verified indexed code-reference leads with their qualified-binding limitation.

`detect_n_plus_one` accepts `project_id` and an optional selected VB `file_path`.
Without a file, use `max_files` (1–16, default 8), `file_offset`, and the returned
`next_page`. Findings connect loop or row-binding locations to database calls,
including bounded indexed helper paths. They include heuristic severity and
JOIN/batch guidance. Actual query counts, row counts and runtime cost remain
unknown. Selected source revisions and current file fingerprints must match.

The paired VB sidecar recognizes method-body loops, explicit `Handles` and
authored paired markup row bindings, ADO.NET command declarations and declared
DataContext/DbContext query materialization. Cached collections and deferred
lambdas are excluded. Constructors/accessors, dynamic dispatch, injected
receiver parameters and translated SQL remain unassessed. Coverage gaps and
truncation are explicit; zero findings do not establish absence of N+1 queries.
The complete JSON budget defaults to 16384 estimated tokens, maximum 65536;
an insufficient budget fails intact. Analysis does not run SQL or edit sources.

`find_missing_indexes` accepts `project_id`, optional selected SQL/VB `file_path`
to narrow query evidence, and `limit` (1–50, default 10), `offset`, `max_tokens`.
Schema comparison still uses all admitted SQL declarations. The tool extracts
WHERE/JOIN/ORDER BY patterns from SQL files and indexed static VB SQL calls,
compares leading keys, unique equality lookups, INCLUDE columns and supported
filters, and ranks candidates by distinct source sites. Returned SQL is a review
suggestion; no database connection, index creation or migration occurs.

Results cite queries and table/index declarations, include optional covering and
filtered alternatives, and warn about unknown selectivity and write cost. Inline
SQL citations identify their containing indexed method. Unsupported or ambiguous
expressions remain coverage gaps; unavailable schema sources withhold DDL.
Procedure signatures and view header options are omitted for body parsing and
are not validated. LINQ-generated SQL, assembled query fragments, dynamic SQL,
runtime call counts, deployed schema and actual optimizer benefit are unassessed.

Analysis is bounded to 2048 query sources/scopes, depth 12, 512 candidate patterns,
5000 indexed SQL-call edges, 256 inline-source files and 64 MiB source reads.
At most 16 example sites per candidate and 20 covered patterns are returned;
counts and truncation remain explicit. The complete JSON budget defaults to
16384 estimated tokens, maximum 65536, and fails intact when insufficient.
DDL uses supported key types within 16 columns/1700 bytes; engine compatibility
is unverified. Validate candidates using actual plans and the
[SQL Server index design guidance](https://learn.microsoft.com/en-us/sql/relational-databases/sql-server-index-design-guide).

### Incremental indexing and memory

Unchanged refreshes preserve the current source generation and skip source
copying, parsing, and graph rebuilding. Git history still refreshes independently.
Watcher notifications are filtered against project extensions, Git ignore rules,
and Engram's own storage before queueing work. Actual source changes stage a
complete logical view that references unchanged physical revisions, preserving
their chunk bodies instead of copying them into every generation. A held source
snapshot lease protects its referenced revisions during collection. Publication
checks the exact stored head, a persistent revision and the source directory/type;
changing a head away and back cannot authorize stale work. Initial registration
is visible with generation zero until its complete source view is published.

Missing source directories, failed traversal and unreadable ignore rules stop
updates instead of becoming file deletions. Full and graph repairs complete
this scan before clearing existing graph data or history markers. Configured
file and byte limits are checked while collecting source paths. A successful
scan of an empty source set still permits genuine deletions; the scan observes
the live filesystem and does not freeze concurrent filesystem changes.

Changing a project's source directory/type invalidates inherited stat-cache
trust. Matching size and modification time then require content-hash verification
before reuse. An identical replacement can retain the same generation and physical
chunks; a conditional identity attestation records the verification. This does
not establish a hard process-memory cap or complete sustained-workload validation.
Database page caches have explicit per-store limits: 16 MiB for metadata,
32 MiB per document store, and 128 MiB each for the shared graph and embedding
stores. Each native vector database connection separately shares a 64 MiB index
cache and a 32 MiB metadata cache across its tables and readers. These are
weighted retained-cache budgets; eviction can require additional reads. Active
queries, held readers, multiple connections and total process memory require
separate sustained-workload measurements.

`get_memory_budget` reports tracked allocation reservations, which do not include
all native allocations or process memory. Supply an optional `project_id` to
inspect an already cached project's source views, reader references, vector
routes, approximate native cache item count, and ranking work. Inspection does
not open a project, refresh its cache lifetime, or request native cache
maintenance. Counts are bounded, non-atomic observations; busy components and
omitted entries are reported explicitly. Reference counts describe owners, not
unique requests, and cache item counts do not measure bytes.

`incremental_indexing_gc` preserves selected source revisions and active readers
while removing stale generations. Its default `compact_vectors=true` also
combines small native vector fragments and reports the completed fragment
counts. It then attempts separate history maintenance for verified, owned flat
vector tables. Active native readers, uncertain backend completion, tags, unknown files,
unsupported metadata, or inspection limits defer pruning and preserve history.
Successful pruning reports old versions and bytes removed separately from
compaction counts. Compaction rewrites live vector data; these operations do not
establish a memory plateau or a physical SSD write limit. Set
`compact_vectors=false` to request generation cleanup alone. Native compaction
uses one worker and 1,024-row scan batches; these settings are not a total memory
or write-volume limit. Builds without vectors report that compaction was skipped.
Known default cleanup settings on existing Engram-owned tables migrate during a
write or compaction so history removal follows these checks. Custom cleanup
settings are preserved and reported for review.

### Scoped repository instructions

Reviewed JSON guidelines imported with `ingest_quality_gates` may declare one
`path_scope` or `file_pattern` string per rule. This preserves the existing
repository-rule pattern behavior: `*` and `?` match across separators, and bare
patterns use substring matching. GitHub `applyTo` lists, exclusions, and other
unsupported scope declarations are rejected rather than converted to global
mandates. Use complete Markdown policies for applicability review when those
qualifications cannot be represented by a single repository-rule pattern.
The JSON array accepts `rule`, `why`, `category`, `severity`, `languages`,
`evidence_count`, `bad_example`, `good_example`, and one of the two scope fields.
Unknown fields reject the entire document before any corpus replacement.

Imported records retain their project-relative source and source hash (4 MiB
maximum input). Reimporting authored `copilot`/`rules` policy replaces its
receipt-owned documents and promotions, including removed or reordered rows and
an empty JSON array. Historical finding exports append unless `replace_source`
is true. Exact ownership checks reject manually modified records before mutation.
Interrupted imports withhold managed mandates until a retry finishes. Rules and
their publication receipt commit atomically; obsolete search cleanup is retryable.

Review gates, planning and rule injection verify relevant managed policy hashes
once per request (64 sources, 4 MiB per source, 8 MiB total). Changed, missing or
unverifiable policy is withheld with incomplete coverage; retained evidence stays
recoverable and labelled. Historical exports do not need to remain on disk.
The root-guidance generator cites managed policies separately. Accepted manual
rules retain their complete text, exceptions and exact legacy scope condition in
`.claude/rules/engram-repository-policy.md`; this conditional catalog does not
provide native path-filtered loading. Complete original rows, including learned
evidence, unknown legacy authority and catalog overflow, remain recoverable in
the response and `.engram/instruction-generation.json`. Delivery distinguishes
preview, written, unchanged owned output, human conflicts and partial failures.
Optional `use_llm` ranks existing evidence IDs for review; it cannot rewrite
authored rules or infer their acceptance. Distillation remains for review findings.

`clear_existing` replaces the whole searchable corpus, not every registry rule;
other sources disclose missing searchable coverage until reimported. Legacy
promotions without receipts are not automatically retired. Ambiguous basename-only
records require ownership review. Legacy enforcement remains explicitly unverified;
this does not migrate every previously imported rule or generated instruction file.
Rules with missing or unrecognized provenance retain their enforcement and carry
an ownership warning. The warning counts the returned rule set; an unfiltered
count does not mean every rule applies to every change. Explicit manual provenance
retains its authority regardless of the rule ID.

Source verification accounts for receipt JSON and authored source bytes in one
8 MiB request budget, with at most 64 origins. Exhausted or unavailable evidence
is disclosed and managed constraints remain unverified. Inventory uses the same
verification order as normal consumers; its separate source-fingerprint passes
are additional bounded observations, not a whole-request I/O or atomicity claim.

For exact legacy-ownership evidence, call `list_repo_rules` with
`ownership_inventory: true` and, optionally, up to sixteen project-relative
`source_paths` candidates. The JSON response retains exact raw row text and
hashes, decoded multiline rules, scope, priorities and dates; it includes every
source-receipt entry, including malformed and pending claims. Existing manual
provenance, conflicting claims and unknown ownership remain visible. Candidate
file names, matching text and file existence do not establish ownership.

The inventory reports current unfiltered registry enforcement separately from
ownership and captures bounded raw source fingerprints. Its registry snapshot
hash binds the exact project, existing identity epoch, corpus revision, rows
and all receipt bytes in one read transaction; source fingerprints remain
separate filesystem observations. Missing or invalid identity is reported,
never initialized or repaired by inspection. No native project runtime opens,
no rule or policy file changes, and no adoption occurs. Raw-inventory or
response overflow fails explicitly rather than returning a partial inventory.
Inventory never supplies the ownership decision. To review an explicit adoption,
use `ingest_quality_gates` with its optional `adoption` request instead of normal
ingestion. First discover the source's parser entry IDs:

```json
{
  "project_id": "PROJECT_ID",
  "source_path": "policies/rules.json",
  "source_type": "rules",
  "adoption": {
    "mode": "preview",
    "expected_source_blake3": "64_HEX_SOURCE_HASH",
    "bindings": [],
    "decision_ref": "REFERENCE_TO_EXPLICIT_OWNER_REVIEW"
  }
}
```

Discovery returns up to 128 complete normalized entries per page; use
`source_entry_offset` for the returned next offset. Select explicit mappings:

```json
"bindings": [{
  "rule_id": "EXISTING_REGISTRY_ID",
  "expected_raw_blake3": "64_HEX_RAW_ROW_HASH_FROM_INVENTORY",
  "source_rule_id": "EXACT_ENTRY_ID_FROM_DISCOVERY",
  "decision": "adopt_unverified"
}]
```

An existing manual rule requires `transfer_manual` instead. A selected preview
returns `status: "ready_for_explicit_apply"` with exact original row text,
complete source projections and a plan hash, or `status: "blocked"` with a
concrete reason and `adoption_performed: false`. A blocked preview can be a
successful MCP response (`isError: false`); inspect its payload status before
offering an apply operation. Preview does not create identity or open a native
runtime. Establish missing identity explicitly before recollecting and reviewing.
To apply, send the same selected request with `mode: "apply"` and
`expected_plan_blake3` from that preview. Stale rows, claims, source bytes or
project identity invalidate the preview token; recollect and review before
applying. The caller's `decision_ref` records a decision reference and does not
authenticate human authority or establish policy acceptance.

Apply atomically changes provenance and records the source receipt, parser aliases
and immutable audit. IDs, text, scope, priority, dates and opaque row fields are
preserved. It writes no authored files and publishes no search documents; import
the source separately to provide searchable evidence. Filesystem verification
is separate from the Registry transaction; postcommit source drift is disclosed.
Ordinary reimport preserves adopted rows only while their complete reviewed source
projections match. Changed/missing entries or conflicting resolved IDs stop before
pending receipts or search writes. For intentional parser-ID changes,
`ingest_quality_gates` accepts a separate `alias_review` preview/apply request,
mutually exclusive with adoption and normal ingestion. Rebinding requires an
unchanged complete policy projection and preserves the exact existing rule row;
retirement removes only the exactly reviewed owned row and alias. Explicit alias
swaps are atomic. Apply checks the complete ownership snapshot, records an audit
and leaves the source receipt pending until normal reimport. Caller `decision_ref`
is unauthenticated and does not establish policy acceptance. Ownership is never
inferred from positional IDs, matching words or a deleted source entry.

### Maintaining instructions after code changes

Use the existing `pre_push_audit` tool with `instruction_maintenance` instead of
its code-retrieval inputs:

```json
{
  "project_id": "myapp",
  "instruction_maintenance": {
    "instruction_paths": [".github/instructions/area.instructions.md"],
    "evidence_paths": ["Site/Area/Helper.vb", ".github/instructions/shared.md"],
    "base_commit": "<full 40-character before commit ID>"
  }
}
```

The response supplies complete current and committed-before instructions,
committed-before/disk-after evidence, exact hashes and `packet_blake3`.
`instruction_manifest` follows CommonMark inline and reference-style links
(`[label][rule]`, `[rule][]` and `[rule]`) into a separate dependency collection.
Definitions belong to their own document and historical/current version;
forward definitions work and the first matching definition wins. Unused
definitions and fenced examples do not create dependencies. Packet version 4
retains exact end-exclusive UTF-8 byte ranges for link uses and chosen
definitions, raw scope frontmatter and changed dependencies per instruction.
Previously collected proposal hashes require recollection. Line-ending-only
differences are reported separately while raw hashes remain exact. An unchanged
change list cannot hide incomplete reference coverage. Linked guidance is
evidence, never an additional edit target or accepted policy. Historical links
use their requested Git tree. Exact-content deleted/added matches are move
candidates; duplicate sources or destinations remain ambiguous and missing
original references remain visible.

Limits are eight instructions, sixteen explicit evidence paths and sixteen
automatic dependencies, discovery depth three from any selected root with shared
collected nodes, 256 edges, 128 KiB per snapshot
and 768 KiB of complete input across both versions. Explicit inputs take
precedence. Oversized explicit inputs fail; automatic omissions, unavailable
files and unresolved explicit reference labels are named without truncating
guidance or treating unread content as proven absence. HTML, wiki and image
references remain explicit coverage gaps; image assets are not opened as
guidance. Malformed Markdown and ordinary undefined bracketed prose are not
interpreted as links. Fragments are unverified; no
exhaustive repository discovery or code-symbol resolution is claimed.

Inline-code file paths are also reference candidates. The manifest checks
document-relative and repository-relative paths within the same collection
budgets. Conflicting existing paths retain both complete bodies and an explicit
ambiguity; a missing or unreadable alternative cannot silently establish a
binding. Glob patterns are reported without expanding or scanning directories.

Symbol observations use only the captured code snapshots. They record inline
identifiers and ordinary text matching collected declaration candidates, with
source/version hashes, locations, candidate paths and excerpt hashes. A single
observed candidate is not a certified global binding. Per-instruction comparisons
distinguish changed excerpts, unchanged observed bytes, ambiguous matches and
unresolved current references. Excerpt equality does not establish unchanged
behavior. VB uses source-only regex declaration-line candidates without starting
Roslyn; body coverage and proof of absence remain unavailable. Candidate location
changes do not prove a rename. Whole-packet stale checks still cover every
selected input, separately from the per-instruction impact report.

`project_health` accepts the same optional `instruction_maintenance` input
without `draft`. Its separate reference observation does not establish policy
correctness or change the storage-health verdict. Inspection covers only the
selected instructions and bounded linked dependencies; it runs on request,
without background scans or authored-file writes.

`update_project` accepts that observation input when `wait=true`. It appends
the observation after the source-update result, including on a no-op refresh.
Unavailable or stale instruction evidence does not undo a completed source
update. The observation is a separate worktree read, not an atomic snapshot of
the newly indexed source and policy. Submit proposals through `pre_push_audit`.

The drafting agent can resubmit the same request with `draft`, containing
`expected_packet_blake3` and one `decisions` entry per instruction. Each entry
has an outcome (`change`, `no_change`, `conflict` or `needs_evidence`), rationale
and exact before/after citations (`path`, `version`, `blake3`, `quote`; null hash
and quote cite proven absence). Changes use unique original `old_text` anchors,
`new_text` and a semantic `reason` in `edits`. Declare accepted exception text in
`preserved_fragments` and list missing policy intent in `unresolved_decisions`.
Engram returns the complete proposed file and diff, refusing stale dependencies,
fabricated citations, ambiguous or overlapping edits and removed declared
fragments. An unrelated commit does not invalidate unchanged dependencies.

These are mechanical checks. Fragment presence does not prove preservation of
an exception's meaning, and citations do not prove the proposed interpretation.
Use the existing independent review and human approval workflow before editing
authored files. A code change violating valid policy may be an implementation
defect; it does not authorize weakening the rule. This tool neither applies
proposals nor adopts legacy ownership. Revalidate the exact packet/proposal
before any subsequent edit.

For bounded, read-only offline inspection without starting an index or server,
the same collector is available through `cargo run -p engram_server --locked
--no-default-features --example instruction_maintenance -- <project-root>
<project-id> <request.json>`. The JSON file contains the nested maintenance
request shown above, without the outer `project_id`/`instruction_maintenance`.

New area guidance uses the same workflow. Supply `new_instruction_targets`
with `instruction_path` (Markdown), `provenance_path` (JSON), and `area_scope`
(project-relative `applyTo` patterns). Existing and new instruction targets
together are limited to eight. `instruction_paths` may be empty for a new-only
draft; complete relevant code and authored/shared guidance belong in
`evidence_paths`. Both output paths must be absent at the exact base commit and
on current disk; this operation never recreates a previously authored file.

For automatic evidence gathering, add `area_context` to the nested maintenance
request and omit `evidence_paths` (or supply explicit seed paths):

```json
"area_context": {
  "scope": "src/payments",
  "max_files": 4,
  "exclude_paths": ["src/payments/generated"]
}
```

This indexed workflow is available through `pre_push_audit`, with a managed
source snapshot. It captures complete area files, bounded direct consumer and
companion candidates, house-example candidates, applicable shared guidance and
linked documents. Scope is a literal file or directory boundary; `max_files`
is 1..8. Exclusions also apply to linked dependencies. Missing or capped evidence
is named, and graph declarations/relations are withheld when either endpoint's
selected file fingerprint cannot be verified against complete current source.
Imported review dispositions remain advisory evidence with recovery references;
they never become policy acceptance. No exhaustive discovery or validation run
is implied. The explicit-path offline collector remains available.

After collecting the packet, submit `draft.new_area_files`: each declared target
gets claims with an ID, section, observed/authored/proposed classification, text,
rationale and exact citations, plus shared-rule citations and unresolved
decisions. Sections cover purpose, invariants, house examples, companions and
exceptions; automatic area packets also require entry-point, consumer and
validation-path evidence or explicit gaps. The response contains linked Markdown and provenance proposals,
creation diffs and a hash binding the pair. Missing evidence is named;
`needs_policy_review` means only that mechanical checks passed. A verified
ruling quote or a workflow status establishes no accepted exception. Neither
proposed file is written, and the existing unique-anchor edit flow is unchanged.
Provenance includes complete cited snapshots deduplicated by path and version,
so a reviewer can independently resolve each quote and hash. Instruction text
is capped at 128 KiB and complete provenance at 768 KiB; oversized artifacts are
rejected rather than truncated.

`get_change_set`, `prepare_implementation_context`, and `pre_commit_review`
include applicable repository guidance in both JSON and Markdown.
`search_memory` includes live guidance for the source paths returned on its
current page. Its advertised `full_chunk` call requests `inject_rules: true`;
its separate `citation_chunk` call recovers exact stored source without injected
guidance. Instruction edits take effect without refreshing the source index;
`get_chunk` still checks source freshness before returning editable content.
Discovery reads current root and target-ancestor guidance
(`AGENTS.md`, `CLAUDE.md`, `INSTRUCTIONS.md`), conventional policy directories,
and indexed policy paths, so new topic files need no reindex.
Planning retains selected concept-source identities directly, including root
files and paths containing spaces, and reports incomplete discovery separately.

Topic selection respects `applyTo`, `paths`, and `globs`, including comma-separated
patterns and YAML lists. Sources carry BLAKE3 hashes and applicability reasons.
Documents are included whole or explicitly omitted with a source-read lead;
exceptions are never silently cut off. Invalid or unsupported scope and discovery
limits report incomplete coverage. Matching selects documents; it does not prove
that arbitrary prose rules were enforced. Historical change-set requests withhold
live guidance and explicitly report that historical policy was not evaluated.
Rejected source target paths also report incomplete coverage, while valid paths
for new files retain their applicable guidance.

Change-set response text is bounded to 40 KiB. An oversized result returns
`response_delivery` with mandatory row/checkpoint identities and a copy-ready
`get_change_set` recovery call using `response_id` and `response_offset`.
The deferred manifest retains view omission counts and candidate omission totals
grouped by reason in both output formats.
Concatenate the returned `content` pages in byte-offset order and verify their
BLAKE3 response ID to recover the original JSON or Markdown without rerunning
planning. Recovery preserves the original view's caps; request JSON with
`detail: "full"` to inspect candidate evidence omitted from a compact or Markdown
view. These project-scoped snapshots expire after 24 hours and are limited
to 32 per project; unavailable snapshots require fresh planning and reconciliation.

### Cognitive features (LLM-optional)

Everything in the sections above runs without an LLM. These extras use
one when available:

- **REM Dreaming** — clusters co-occurrence patterns into `Insight`
  nodes. Runs on a schedule or on demand.
- **Style mimicry** — per-file coding conventions extracted from the
  file's current state (no LLM needed for the core extraction).
- **Business-logic comprehension** — local-Ollama method-level analysis
  with a deterministic fallback, validation gate, and hallucination
  detection.
- **Migration boundary suggestion** — LLM + deterministic boundary
  detection with cross-cluster dependency analysis.

### Project memory and diff artefacts

Nice-to-haves rather than core capabilities, grouped here so they
don't clutter the top:

- **`produce_claude_md`** — generate `CLAUDE.md` + `.claude/rules/*`
  from indexed signals. Rules bucketed into three confidence tiers
  (**Hard** — live reverts and security invariants; **Strong** —
  team-enforced conventions; **Observed** — pattern observations,
  advisory). Convention files use an `Applies to / Mandatory / Strong /
  Observed / Avoid` template with explicit sample-scoping. Merge modes:
  `splice`, `optimize`, `replace`. Optional `use_llm: true` polish pass
  is registry-cached so reruns against the same inputs cost zero tokens.
- **`explain_change`** — narrates a diff as commit message, PR
  description, or changelog entry. Deterministic pass with optional LLM
  polish; cached by diff hash.

### Memory bank and repo rules

Per-project persistent notes support recall across sessions. `get_chunk` adds
matching registry rules when `inject_rules: true` is requested; the default is
false. For actual source documents in the `memory` namespace, that option also
resolves live, task-scoped repository guidance. Exact citation requests reject
rule injection and logical slicing. A rule's presence is evidence, not proof
that its prose was enforced or its ownership was established.

### Multi-client daemon

Auto-daemon dispatcher so several MCP clients can share one Engram
process. First client spawns the daemon; subsequent clients attach as
proxies; the daemon lingers past the last disconnect so repeated short
sessions don't pay the warm-up cost every time. File-based lock,
race-free session counting, startup retry.

### Exclusive maintenance stdio

For controlled maintenance after normal clients and the shared primary have
closed, launch `engram_server.exe --maintenance-stdio` over MCP stdio. Maintain
that exclusion until the maintenance process exits and releases its handles.
The mode does not attach to or launch a daemon or proxy, and overrides configured
or environment-enabled multi-client preferences. Combining it with `--daemon`,
`--daemon-launcher`, `--proxy` or explicit `--multi-client` is rejected before
application state opens.

Normal configuration, logging and shared-store initialization still apply; this
is not read-only acquisition. Startup skips orphan-job cleanup, watcher restore,
warm-up, dreamer, GC, immune and integrity actors. Initialization and project
listing do not warm registered project search runtimes. Explicit MCP tools keep
their normal schemas, store writes and requested-job behavior; use synchronous
requests for controlled work and wait for any explicitly requested jobs before
closing the session. Selecting this mode does not authorize a repair or establish
data compatibility, freshness or safe downgrade.

### Upgrading legacy indexes

An older project's numeric source generation alone does not establish that its
source snapshot is complete. Source tools refuse unproven indexes and request
an explicit, non-wiping full repair. A project appearing in `list_projects` or
`project_info` does not establish migration readiness.

Perform conversion with normal clients closed, using exclusive maintenance
stdio. If an old native vector table has no embedding identity, first call
`repair_project` with `scope: "vector_only"`. This re-embeds stored documents
across retained namespaces using the configured provider; it can be substantial
work. It preserves the published source generation and unknown old canonical
tables. Then call `repair_project` with `scope: "full"`,
`wipe_and_reindex: false`, `max_commits: 0`, and `index_antipatterns: false`.
The full repair publishes current source without starting a Git-history crawl.
Check source retrieval and retained knowledge before ending maintenance.

Conversion is per project. Converting one project does not convert others;
unconverted source indexes and vector stores can remain unavailable to the new
runtime. Do not treat file preservation as proof that their tools are usable.
Returning to an older executable also requires compatibility evidence for the
current data state; restoring its executable alone is not a data rollback.

### Observability and integrity

Lock-free metrics (job latency, queue depth, index drift, memory
pressure, checkpoint recovery, extraction confidence, safety decisions).
Cross-store integrity sentinels with 5 % tolerance and auto-repair.
Crash-safe job orchestration with resume-from-failure. Retrieval quality
benchmarking (NDCG@10, Recall@10, MRR) with configurable pass / fail
thresholds.

---

## Languages and ecosystems

| Stack | Parser | Extra capabilities |
|---|---|---|
| Rust, Python, Go, Java, C, C++ | Tree-sitter | AST extraction, call graphs, imports |
| TypeScript, JavaScript (+ JSX / TSX) | Tree-sitter | Framework detection (React, Vue, Angular, RxJS, Node, Express, Jest), jQuery inventory, cross-layer AJAX tracing, transpiled-TS fingerprinting |
| C# | Tree-sitter | LINQ / async / records / pattern-matching detection, ASP.NET WebForms codebehind tracing |
| VB.NET | Tree-sitter | `Handles` clauses, `WithEvents`, `On Error` resolution, `Module` vs `Class`, `Optional` context-injection convention, `SafeRedirect` pattern, `ReDim Preserve` |
| ASP.NET WebForms (.aspx, .ascx, .master) | Custom | Control ID extraction, event wiring, master-page `FillsRegion`, `UpdatePanel` regions, ViewState lifecycle, deep layout extraction |
| Classic ASP (.asp) | Custom | COM objects, ADO connection / recordset, Session / Application / Request / Response, SSI includes, `Server.Transfer` |
| SQL (T-SQL) | Custom | `CREATE TABLE / VIEW / PROCEDURE` parsing, column / PK / FK / CHECK extraction, stored-procedure call chains, trigger detection |
| Google Maps / Esri ArcGIS JS | Custom | 80+ widget classes, migration complexity assessment |
| SSRS (.rdlc / .rdl), Crystal Reports | Custom | Parameter, dataset, subreport, table-reference extraction |

Unknown extensions fall through to a generic text chunker + full-text
indexing path.

---

## Install

For an exact small-source index, freshness check and first query, follow the
[quickstart](docs/QUICKSTART.md). It includes an existing-OciusX connection path
and isolated Windows/Linux/macOS sample configuration commands.

### Prerequisites

- Stable Rust toolchain, as exercised in CI (edition 2024)
- On Linux / macOS: `libgit2` and `cmake` development packages
- Optional: [Ollama](https://ollama.ai) for local LLM / embedding backends

### Build

```bash
git clone https://github.com/ZilverZtream/Engram_v2-MCP.git Engram-MCP_v2
cd Engram-MCP_v2
cargo build --release
```

For a Windows x64 release with the VB Roslyn parser, use a stable .NET 9 SDK and:

```powershell
.\scripts\build_windows_release.ps1
```

This reuses the checkout's shared Cargo target directory and writes
`release/engram_server.exe`, the complete self-contained `release/sidecar/`
directory, and a SHA-256 asset manifest. Deploy the executable and companion
directory together. The companion includes its .NET runtime; users do not need
an SDK. `-SidecarOnly` builds the companion for extraction verification.
`ENGRAM_VB_SIDECAR_PATH` overrides discovery, including legacy adjacent executable
locations. Trusted VB indexing requires a matching companion that attests
`vb-ingestion-parser-v6`; discovering an older binary does not establish
compatibility. A present, unsupported companion makes trusted ingestion refuse
before publishing source output. With no companion, Engram records the Rust
fallback explicitly. Installing a companion does not certify earlier fallback
output; extraction-identity changes can require reindexing. Follow the repair or
reindex requirement reported by Engram.

Trusted VB compilation admits at most 32 MiB of aggregate raw source and 10 MiB
per file before reading source bodies. These limits are separate from the
per-request extraction cap: a peer can remain available for compilation bindings
while its own extraction uses an explicitly recorded source-limit fallback.
Retained compilation admission estimates 32 times the source bytes plus 8 KiB
per tree, with separate refresh scratch reservations. These are allocation
estimates, not a hard Roslyn process-memory limit; the shared memory budget can
refuse a compilation below the source limits.

The binary lands at `target/release/engram_server`.

### Configure

Minimal `engram_mcp.yaml`:

```yaml
allowed_roots:
  - /home/user/projects
data_dir: /home/user/.engram-data

embedding_backend: local       # default: degraded trigram projection, not a learned model
advertise_all_tools: true       # default: full profile, filtered by project capabilities
```

Omitting `embedding_backend` selects `local`; `embedding_model` is unset and is
ignored by the `local`/`candle` projection implementations. Default hybrid search
labels this fallback `degraded_trigram`. Projection matches can affect the combined
ranking; request `semantic: false` for lexical-only retrieval, or configure a real
embedding provider for semantic search. The lexical-only setup in the quickstart
is an explicit choice, not the global default.

Other operational defaults are five seconds of watcher debounce, 50,000,000
bytes per Tantivy writer, two concurrent jobs, and parse concurrency capped at
16 available workers. `memory_budget_bytes: 0` selects a 2 GiB tracked-allocation
budget, not a resident-memory cap. The [tuning guide](docs/TUNING.md) explains
when to test explicit overrides. Existing configuration values take precedence;
these defaults do not enable watching or supply the required allowed roots.

Set `advertise_all_tools: false` only for clients requiring the limited core
tool list. Tools omitted from `tools/list` may be unavailable to client agents;
The full profile includes `list_advanced_tools` as a catalog; it is not a
substitute for protocol discovery.
Restart the daemon and reconnect clients after changing this setting.
The core tier has up to 32 tools and includes `list_projects`, `project_info`, `get_chunk`,
`get_full_method_body` and `read_memory_bank` for project discovery, stored
citations and current-source recovery. Detailed `project_health` diagnostics
remain in the full profile; Core retains `get_index_freshness` for index freshness. Dedicated blast
radius queries, general graph-path discovery, inferred coding-style analysis
and similar-change searches are available in the full profile, alongside
instruction-file generation and pre-push audits. Core clients can poll and cancel
their indexing jobs with `get_job_status` and `cancel_job`. Normal review evidence
can be recovered through Core's advertised `get_review_decisions`, including
complete retained discussion input and immutable decision events. Core omits the
optional advanced catalog to keep its 32-tool ceiling.

`validate_generated_code` defaults to checking native code edits. Set
`include_migration_advice: true` when you also want VB-to-C# translation and
ASP.NET Core migration cautions. Blocking task/lock checks run in both modes.

`analyze_business_logic` preserves separate method overloads. When analyzing a
single overloaded name, pass its one-based declaration `line`; file analysis
returns that selector as `overload_line`. Empty VB methods receive a deterministic
summary. VB extraction preserves Windows line endings and one-based source
references, and successful file reanalysis removes obsolete method summaries.
Interface and abstract declarations have no executable body and are excluded.
Incomplete model JSON receives one retry with a larger response budget;
its deadline fits within the HTTP transport ceiling. If that also fails,
the original diagnostic remains available.

`grep_project` includes a `doc_id` for indexed matches in both JSON and Markdown,
so they can be passed directly to `get_chunk`. Working-tree-only matches have no
indexed identifier and explicitly point callers to the file instead.

Point the server at it via environment variable:

```bash
export ENGRAM_CONFIG_PATH=/path/to/engram_mcp.yaml
./target/release/engram_server
```

The server runs over **stdio**. All stdout is reserved for MCP protocol
messages; logs go to stderr. Enable debug logs with `RUST_LOG=debug`.

---

## Connect

### Claude Desktop

```json
{
  "mcpServers": {
    "engram": {
      "command": "/path/to/engram_server",
      "env": { "ENGRAM_CONFIG_PATH": "/path/to/engram_mcp.yaml" }
    }
  }
}
```

### Claude Code

```json
{
  "mcpServers": {
    "engram": {
      "type": "stdio",
      "command": "/path/to/engram_server",
      "env": { "ENGRAM_CONFIG_PATH": "/path/to/engram_mcp.yaml" }
    }
  }
}
```

### First session

The [verified-query walkthrough](docs/QUICKSTART.md#index-check-search) gives
literal tool arguments and expected source/freshness results. Source builds and
full-application indexing are outside its five-minute small-sample target.

```
Index /home/user/myapp as project "myapp".
Wait for indexing to complete. Then give me a codebase overview.
```

Omit `project_type` or use `auto` to select an indexing preset from eligible
manifests and source files. Mixed projects, or projects whose assets do not fit
a narrower preset, use `general`. Selection happens once at registration;
refreshing preserves that preset. Explicit types and aliases remain supported,
including `rust`, `python`, `typescript`, `csharp` and `dotnetwebformscs`.

`project_info` and the overview summary report the preset, detected ecosystems,
path evidence and coverage from the published source index. These observations
respect the preset and ignore rules; they do not evaluate builds or establish
working-tree freshness. Run `update_project` to reflect disk changes.

WebForms and migration tools appear when a registered project's indexed files
or explicit profile support them. Discovery combines capabilities across all
registered projects and applies the configured full/core tier. An ordinary
`.csproj` or `.sln` does not by itself establish WebForms support. Unknown legacy
or unavailable source coverage conservatively retains specialized discovery.
Re-list tools after background indexing finishes.

After any set of staged changes:

```
Run pre_commit_review on the staged diff for project "myapp".
```

---

## Tool reference

The available tool list depends on the configured profile and registered project
capabilities. Use MCP `tools/list` for exact names and argument schemas in the
current connection; runtime discovery tools do not require `project_id`.
The groups below are a navigation aid. See [`docs/TOOL_CONTRACT.md`](docs/TOOL_CONTRACT.md)
for workflow contracts and limitations; design examples in the enhancement
specification are not a substitute for the runtime schema.

<details>
<summary><b>Project lifecycle</b> — index, update, list, info, health, repair, delete, watch</summary>

| Tool | Purpose |
|---|---|
| `index_project` | Initial indexing of a local directory |
| `update_project` | Incremental re-index of changed files |
| `list_projects` · `project_info` · `project_health` | Inspection |
| `repair_project` | Targeted repair (`full` / `graph_only` / `tantivy_only` / `vector_only`) |
| `delete_project` | Delete a project and its stored data |
| `watch_project` · `unwatch_project` | Automatic re-index on filesystem changes |

Watcher filters apply within the selected project root and respect Git ignore
rules and Engram's runtime-storage boundary. Nested runtime storage is excluded
from source collection, source budgets, and explicit reindex selection. Source
roots must lie outside the runtime-storage tree.

`unwatch_project` durably disables all saved watches for the project while
preserving their configurations. It removes queued work and requests cooperative
cancellation of a running watcher update.

</details>

<details>
<summary><b>Search</b> — hybrid, vector, graph, symbol, error-stack</summary>

| Tool | Purpose |
|---|---|
| `search_memory` | Hybrid FTS + vector with RRF, MMR, filters |
| `vector_search` | Pure semantic vector search with oversampling + MMR |
| `get_chunk` | Fetch content for a chunk ID |
| `graph_search` | Hybrid text + symbol match with edge expansion |
| `find_symbol_references` | Incoming / outgoing edges for a symbol across all edge kinds |
| `get_codebase_overview` | Scoped CodeMap with diverse current file representatives, declaration identities and executable source/context/impact follow-ups; retained context is opt-in |
| `analyze_error_stack` | Multi-language structured stacktrace parser with frame-boosted search |

</details>

<details>
<summary><b>Knowledge graph</b> — query, traverse, impact, blast radius, patterns</summary>

| Tool | Purpose |
|---|---|
| `query_graph_nodes` · `find_references` · `traverse_graph` | Core graph operations |
| `impact_analysis` | Compact revision-admitted dependency graph, shortest paths, alternate connections and named recovery frontiers; historical companions are separate |
| `ast_dependency_graph` | BFS with edge-kind filters, up to 12 hops; tree or JSON output |
| `compute_blast_radius` | Advisory one-hop score plus a separate bounded revision-admitted downstream path walk; no safe-change or complete-impact guarantee |
| `detect_design_patterns` | Structural pattern detection (Repository, Factory, Singleton…) |


CodeMap supports literal project-relative `scope` and `exclude_paths`, plus
`offset`/`limit` for file navigation. Start with diverse project representatives,
follow an area's supplied exploration call, then open a returned source range or
method context. The default preview covers eight files; exact-file responses
provide `next_symbols` calls with `symbol_offset` for later declarations within
the bounded discovery sample. Declarations with usable indexed line bounds carry
one executable source-range read. For richer context and compact impact actions,
pass the exact listed `node_id`
as `symbol_node_id`, retain the same exact-file scope and exclusions, and set both
offsets to zero. This returns only that observed declaration; an unmatched identity
does not establish absence. Compare generation and coverage across these fresh
observations. Generic role hints rank file representatives before
area size; the separate area inventory remains ordered by selected-file count.
Directory groups and filename priorities are navigation
heuristics; they do not establish runtime architecture. Memory/history context
requires `include_retained_context: true` and remains separate from current source.

For a smaller first read, both impact tools accept `detail: "summary"`. This
optional view fits 12 KiB and gives the target, observed counts, immediate source
actions, named missing/depth leads and explicit omissions. Its copyable
`graph_continuation` expands through `impact_analysis` with `detail: "graph"`;
this is a fresh observation, not an atomic replay. Summary skips the separate
history and unverified one-hop advisory workers. Neither view predicts which
consumers will actually break. Omitting `detail` preserves the existing graph view.

Summary actions label source ranges separately from declaration navigation. A
sample incoming connection names both endpoints and records the indexed edge
generation and any recorded source line without requiring the graph registries.
Missing declarations retain unverified name/file hints. Coverage counters count
observations and may repeat an identity; zero unknown observations does not
resolve missing-node or depth gaps. Source-range reads verify returned current
bytes, not complete-method boundaries or invocation locations.

Impact witnesses prefer a recorded call/reference position in `source_line` when
available and preserve the older position separately as `extractor_anchor_line`.
`source_position_kind` distinguishes `call_reference_syntax_metadata` from the
weaker `extractor_anchor_metadata`; neither changes the caller's identity or
declaration bounds. `reference_lines` contains at most eight sorted, distinct
samples from bounded retained metadata. `reference_lines_truncated` flags
additional omitted or uninspected positions; false does not establish an
exhaustive call list. Delegate and property references can also supply these
positions. Follow `recorded_position_read` and inspect current source before
describing a sample as an invocation; the returned hash does not prove compiler
binding or runtime execution.

When a weaker extractor anchor contradicts the source node's known range,
`source_position_scope` is `unattributed_extractor_anchor`. Its raw number is
retained, but `recorded_position_read` is null; use the separate source-node
inspection instead. Engram does not guess another file for that anchor. Without
known node bounds, a legacy position remains an `unverified_file_anchor` read
candidate. Explicit call/reference positions retain their separate inspection
even outside a declaration range.

WebForms controls and layout locations use one-based source lines. A per-file
position contract makes older markup re-extract even when its bytes are unchanged;
the global source format and VB parser identity are unaffected. Page/file directive
locations do not delimit file-wide references.

WebForms contract version 2 also retains the distinction between an event's
control location and its producing file/class. Markup-only refresh recovers VB
`Handles` from exact admitted stored method signatures, without reading the
unchanged codebehind body. Producer edits retire obsolete wiring; changing the
page's backing class or deleting its control removes the old binding. Missing
signature/ownership evidence or conflicting producers can refuse an update;
they do not justify guessing another handler. Shared markup/Handles support for
the same graph key remains an explicit limitation. These indexed relationships
remain static evidence, with producer revisions checked against the held source
selection.

For an explicitly bounded refresh, `update_project` accepts `only_paths` with
`wait: true` and 1–4096 exact eligible relative paths. It re-extracts that set and
inherits outside source revisions without checking their current bytes, additions
or deletions. Git/global enrichment is skipped; referenced extractor companions
may still be read. This mode requires a verified existing source identity and
refuses foreign unpublished source revisions. It cannot be combined with
`reindex_paths`, instruction maintenance or anti-pattern refresh. Omitting it
retains the normal project-wide update; `reindex_paths` adds forced paths to that
normal change scan and does not restrict its scope.

For a source file without usable indexed declaration bounds, CodeMap and admitted
file-impact targets offer a current-file excerpt action. Call
`get_full_method_body` with `file_path` and no `line_end`; `line_start` defaults to
one. This mode reads at most 4 MiB and returns at most 120 complete lines within
32 KiB of rendered tool text, including JSON escaping and freshness metadata.
It returns actual bounds, total lines, the complete-file BLAKE3 hash, and a
copyable `file_excerpt.next_page` action. Pages are fresh reads: compare hashes
before combining them. An oversized first line fails explicitly. Excerpts omit
surrounding context and cannot expand callers; they establish neither method
boundaries nor indexed-generation freshness. Explicit-end ranges and unique-FQN
method retrieval retain their existing validation.

Both impact tools return the same revision-admitted dependency graph. `max_depth` defaults
to six (valid range 1..8); one requests direct callers only. Each displayed path
retains its intermediate source locations and edge witnesses. Deeper paths are
shown first under the response budget, with explicit display omissions. A file
query names the contained declaration where each causal path starts; containment
itself is not presented as a causal hop. Older live-graph counts and the one-hop
risk score remain separately labelled advisory evidence. Schema version 2 stores
file/node identities once and connects them with local references. Additional
verified connections preserve alternate routes that shortest paths alone hide.
An initial inspection section exposes the first observed immediate consumer and
one executable recovery before the full graph. The graph's `omitted_defaults`
describes compact optional fields; an edge's source revision comes from its
source node's physical generation, which can differ from the published HEAD.
Missing or bounded branches have named frontiers and copyable recovery calls;
identity, edge and path omissions are counted separately. Source-range recovery
checks current bytes and does not establish a runtime callsite or method boundary.

An unchanged caller can keep an older physical file revision when its callee
moves. Internally bound caller-origin evidence preserves that relationship across
refreshes; removing the call invalidates it. Older or conflicting bindings without
sufficient provenance remain unknown. These tools do not simulate the proposed
change, compile the project, verify current disk contents or prove runtime safety.

Overview file percentages count selected file identities. Chunk-language
percentages count retrieval units, whose sizes vary with boundaries and overlap.
Neither is a byte/LOC share or a measure of authored business code. Indexing
respects repository ignore rules; tracked vendor and generated files can remain.

</details>

<details>
<summary><b>Git and temporal intelligence</b></summary>

| Tool | Purpose |
|---|---|
| `index_git_history` · `ingest_zip_history` | Commit history indexing |
| `search_history` | Search commit messages / diffs with structured metadata |
| `analyze_temporal_couplings` | Files that frequently change together |
| `analyze_reverts` | Detect reverts, generate anti-pattern rules, index reverted diffs |

</details>

<details>
<summary><b>Review and safety</b></summary>

| Tool | Purpose |
|---|---|
| `pre_commit_review` | Nineteen-gate deterministic diff review with prior-review evidence and CI-stable finding IDs |
| `immune_check` | Score code against the anti-pattern index (hybrid FTS + vector, configurable thresholds) |
| `anti_pattern_guard` | Pattern matching with revert-commit extraction and remediation guidance |
| `autonomous_decision_gate` | 8-gate mandatory pipeline for automated edits; returns `allow` / `deny` / `abstain` |
| `evaluate_safety` | Safety-policy check for a proposed edit |
| `check_edit_safety` | Per-method green / yellow / red verdict |

</details>

<details>
<summary><b>Project-memory generation</b></summary>

| Tool | Purpose |
|---|---|
| `produce_claude_md` | Generate `CLAUDE.md` + `.claude/rules/*` with three-tier confidence bucketing and structured convention files |
| `explain_change` | Turn a diff into a commit message, PR description, or changelog entry |

</details>

<details>
<summary><b>Database and schema</b></summary>

| Tool | Purpose |
|---|---|
| `get_table_schema` | DDL, columns, FK relationships, code references |
| `trace_state_usage` | Readers / writers of Session / ViewState / Application / Cache |
| `trace_ui_event` · `trace_ui_action` | ASPX page → control → code-behind → SQL tracing |
| `get_instrumentation_pack` · `generate_instrumentation_code` | Runtime tracing code generation |
| `get_sp_details` · `list_triggers` | Stored procedure deep analysis, trigger detection |

</details>

<details>
<summary><b>Migration (ASP.NET WebForms → modern)</b></summary>

| Tool | Purpose |
|---|---|
| `analyze_full_project_migration` | Single call that produces a comprehensive per-file migration report |
| `generate_migration_blueprint` | BFS context compiled into a 9-section migration dossier |
| `generate_migration_plan` | Dependency-ordered waves with seams, contract tests, adapters, rollback playbooks |
| `generate_migration_scaffold` | Blazor / React / Angular component generation with real business logic |
| `generate_strangler_fig_config` | YARP reverse proxy + feature flags + Polly resilience |
| `generate_characterization_tests` | NUnit / xUnit / MSTest characterization tests |
| `suggest_state_migration` · `suggest_migration_order` | Per-key state advice; Kahn-sorted migration waves |
| `map_validation_controls` · `map_auth_config` · `map_page_lifecycle` · `map_ajax_regions` | Per-concern migration mapping |
| `analyze_viewstate_deps` · `trace_data_flow` · `get_migration_dossier` · `check_migration_coverage` | Deep per-file analysis |
| `update_migration_status` · `get_migration_progress` | Redb-backed migration tracker |

</details>

<details>
<summary><b>Cognitive and reasoning</b></summary>

| Tool | Purpose |
|---|---|
| `dream_project` · `trigger_rem_cycle` | Cluster co-occurrence into Insight nodes |
| `analyze_file_coding_style` | Per-file convention extraction from git history |
| `analyze_business_logic` · `query_business_logic` | LLM-powered method-level analysis with validation gate |
| `suggest_migration_boundaries` | LLM + deterministic boundary suggestion with cross-cluster dependency analysis |

</details>

<details>
<summary><b>Access layer (per-method)</b></summary>

| Tool | Purpose |
|---|---|
| `get_method_info` · `get_full_method_body` · `get_method_edit_context` | Precise method-context retrieval |
| `get_page_context` · `prepare_implementation_context` | ASPX page / implementation context |
| `trace_user_request` | Source-cited URL/action navigation with call, database and response evidence |
| `validate_generated_code` · `validate_sql_fragment` | Pre-commit code and SQL validation |
| `find_tests_for_method` · `find_dead_methods` · `check_edit_safety` | Discovery and safety checks |

</details>

<details>
<summary><b>Memory bank and repo rules</b></summary>

| Tool | Purpose |
|---|---|
| `update_memory_bank` · `list_memory_bank` · `read_memory_bank` · `delete_memory_bank` | Persistent agent notes |
| `add_repo_rule` · `list_repo_rules` · `delete_repo_rule` | Manage file-pattern rules; chunk injection is opt-in, and ownership inventory is read-only |

</details>

<details>
<summary><b>Observability and operations</b></summary>

| Tool | Purpose |
|---|---|
| `get_metrics` | Server-wide metrics (job latency, queue depth, drift, memory, safety) |
| `check_integrity` | Cross-store consistency check with optional auto-repair |
| `get_memory_budget` · `get_checkpoint_status` | Tracked allocation accounting, optional cached-project retention observations, and recovery status |
| `benchmark_retrieval` | NDCG@10 / Recall@10 / MRR retrieval quality gate |
| `get_extraction_confidence` | Signal-weighted extraction confidence scoring |
| `incremental_indexing_gc` · `dedicated_antipattern_index` | Index hygiene |
| `get_job_status` · `list_jobs` · `cancel_job` | Background job control |
| `export_capture_pack` | Export a zip for offline agentic usage |

`get_job_status` appends a `progress` JSON object for background source indexing,
updates and Git-history jobs. Counts and ETA describe the current observed phase,
not the entire job; flush, graph and publication work can have unknown duration.
A phase at 100% is not job completion. Null values mean unobserved or unknown.
Terminal and cancelled jobs retain their last phase observation without a live
ETA. Older jobs and other job types can report phase coverage as unavailable.

</details>

---

## Architecture

### Storage layers

Indexing a project produces five persistent stores:

```
┌─────────────────────────────────────────────────────────┐
│  Full-text index (Tantivy)                              │
│    camelCase / snake_case tokenisation, namespace-aware │
├─────────────────────────────────────────────────────────┤
│  Vector index (LanceDB)                                 │
│    local trigram / Ollama / OpenAI — RRF + MMR fusion   │
├─────────────────────────────────────────────────────────┤
│  Knowledge graph (Redb)                                 │
│    ~40 edge kinds, O(degree) adjacency, PageRank        │
├─────────────────────────────────────────────────────────┤
│  Git intelligence (libgit2)                             │
│    commit graph, revert detection, temporal coupling    │
├─────────────────────────────────────────────────────────┤
│  Registry (Redb)                                        │
│    projects, jobs, memory bank, repo rules, meta        │
└─────────────────────────────────────────────────────────┘
```

### Crates

| Crate | Purpose |
|---|---|
| `engram_core` | Core types, config, security boundary, Redb registry |
| `engram_index` | Tantivy FTS + LanceDB vectors + DocStore + hybrid search |
| `engram_graph` | Redb-backed graph store + BFS / PageRank algorithms |
| `engram_git` | libgit2 walker, temporal coupling, revert detection |
| `engram_ml` | Dreaming engine, embedders, style mimicry, immune system |
| `engram_server` | MCP server (rmcp), tool handlers, background actors, multi-client dispatcher |

### Background actors

- **Dreamer** — co-occurrence clustering into Insight generation.
- **Immune actor** — git-revert harvesting into anti-pattern indexing.
- **Watcher** — directory monitoring for incremental re-index.
- **GC scheduler** — orphaned-job cleanup.
- **Integrity sentinel** — cross-store consistency checks with
  configurable auto-repair.
- **Memory-budget watcher** — per-subsystem soft / hard limits with CAS
  allocation and backpressure.
- **Multi-client dispatcher** — auto-daemon, file-based lock, proxy
  forwarding, race-free session counting.

---

## Configuration

See the [large-repository tuning guide](docs/TUNING.md) for starting profiles,
writer memory, embedding choices, history work and watcher-delay tradeoffs.

Full YAML:

```yaml
# Required
allowed_roots:
  - /home/user/projects
data_dir: /home/user/.engram-data

# Embedding backend
embedding_backend: local              # default projection; alternatives include fts_only, ollama, openai
# Provider-specific options only when selecting the matching backend:
# embedding_model: nomic-embed-text    # example for Ollama, not the local projection backend
# ollama_url: http://localhost:11434
# openai_api_key: sk-...
# openai_api_base: https://api.openai.com/v1

# LLM backend for cognitive features (optional — everything except
# `dream`, `analyze_business_logic`, `suggest_migration_boundaries`,
# and optional evidence-ID ranking on `produce_claude_md`
# works without it)
llm_backend: none                     # "none" | "ollama" | "openai" | "openrouter"
llm_provider: openrouter
llm_model: llama3.2
llm_ollama_url: http://localhost:11434
llm_openai_api_key: sk-...
llm_http_referer: https://your-app.example

# Limits
max_project_files: 100000
max_project_bytes: 5368709120         # 5 GiB
max_chunks_per_file: 2000
max_concurrent_jobs: 2
max_commits_per_watch: 50

# Safety and Autonomous Decision Protocol
safety_policy_enabled: true
safety_min_confidence: 0.7
safety_min_coverage: 0.6
adp_enabled: true
adp_min_extraction_confidence: 0.5
adp_max_blast_radius: 6
adp_rollout_phase: shadow             # shadow | advisory | guarded | autonomous
adp_kill_switch: false                # emergency kill-switch — forces every verdict to deny
```

### Project types

| Type string | Languages | Key extra extensions |
|---|---|---|
| `rust` | Rust | `rs, toml` |
| `python` | Python | `py, ipynb` |
| `typescript` | TS / JS | `ts, tsx, js, jsx` |
| `java` | Java | `java, xml, gradle` |
| `go` | Go | `go, mod` |
| `dotnetwebformscs` | C# ASP.NET WebForms | `cs, aspx, ascx, master, config, sln, csproj, sql, rdlc` |
| `dotnetwebformsvb` | VB.NET ASP.NET WebForms | `vb, aspx, ascx, master, config, sln, vbproj, sql, rdlc` |

Any unknown type falls back to a broad default covering common source and
config extensions.

---

## Example agent prompts

**Orientation**

```
Give me a codebase overview for project "myapp". Then run pre_commit_review
on the staged diff.
```

**Safe refactor**

```
Before I refactor UserRepository.cs in project "myapp":
1. Run impact_analysis for this file
2. Run compute_blast_radius
3. Find all temporal couplings so I know which files to change together
4. Show me the top 5 reverted commits that touched this file
```

**Pre-commit review (JSON for CI)**

```
Run pre_commit_review on the unstaged diff for project "myapp" with
output_json: true, then summarise the CRITICAL and WARNING findings.
```

**Generate project memory**

```
Run produce_claude_md on project "myapp" with write_to_disk: true and
merge_mode: "optimize". Show me the summary notes.
```

**Narrate a diff**

```
Run explain_change on the staged diff for project "myapp" — I need a PR
description.
```

**Legacy modernization**

```
Run analyze_full_project_migration on project "myapp-legacy". Share the
cross-cutting summary and the top 3 files by migration risk.
```

**Anti-pattern guard**

```
Here's code I'm about to commit. Run immune_check on it against project
"myapp" with file_path="Site/App_Code/dal/orders.vb":

[paste code]
```

---

## Security model

- **Path confinement.** All file access is validated against
  `allowed_roots`. Paths that escape via symlinks, `..`, or canonicalisation
  tricks are rejected.
- **Project-ID validation.** Restricted to `[A-Za-z0-9_-]{1,128}` at the
  handler boundary.
- **Single-writer semantics.** Per-project mutexes serialise concurrent
  index updates.
- **No shell execution.** Every git operation uses libgit2 directly; no
  subprocess is spawned.
- **Secrets redacted in output.** `pre_commit_review` fingerprints matched
  secrets; the raw value is never echoed back in findings.
- **Fail-closed on integrity.** The FTS watcher is fail-closed (no silent
  downgrades); integrity sentinels detect cross-store drift and can
  auto-repair.

---

## Documentation

- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — system design and component interactions
- [`docs/DATA_MODEL.md`](docs/DATA_MODEL.md) — storage-layer schemas
- [`docs/TOOL_CONTRACT.md`](docs/TOOL_CONTRACT.md) — full tool parameter reference
- [`docs/COGNITIVE_PIPELINES.md`](docs/COGNITIVE_PIPELINES.md) — dreaming, immune system, style analysis
- [`docs/GENERATION_MODEL.md`](docs/GENERATION_MODEL.md) — append-only generation semantics
- [`docs/DEVELOPER_SPEC.md`](docs/DEVELOPER_SPEC.md) — contributing and internals
- [`docs/ROADMAP.md`](docs/ROADMAP.md) — planned features

---

## License

Licensed under **PolyForm Noncommercial License 1.0.0**
(`PolyForm-Noncommercial-1.0.0`).

- Noncommercial use — personal, hobby, learning, research — is allowed under
  PolyForm Noncommercial 1.0.0.
- Commercial, business, or for-profit use requires a separate written
  commercial license.
- A company using this code — internally, in product development, SaaS,
  services, or any revenue-generating work — must obtain a commercial
  license.

See [`LICENSE`](LICENSE) for the full terms and
[`COMMERCIAL-LICENSE.md`](COMMERCIAL-LICENSE.md) for the commercial-use
notice.
