# Planning Tools Trio — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:executing-plans. Steps use checkbox syntax.

**Goal:** Three new MCP tools that give an agent the planning context to implement a weak
one-line user story in a legacy codebase (OciusX): `get_concept_footprint`,
`find_similar_changes`, `find_implementation_pattern`.

**Architecture:** One new handler file `crates/engram_server/src/handlers/planning_tools.rs`
containing all three handlers plus pure helpers (path tokenization, similarity scoring).
Pure composition of existing infrastructure: GraphStore (query_nodes = case-insensitive
substring on name/path; neighbors; find_incoming_edges_with_kind), HybridSearchEngine
(lexical_search), GitWalker (walk_commits + files_changed_in_commit), registry project
record (repo directory). No new storage, no schema changes.

**Tech stack:** rmcp #[tool] registration in tools.rs + requests.rs structs + capabilities.rs
flags, spawn_blocking for graph/git work, freshness_footer on outputs.

---

### Task 1: get_concept_footprint
Request `{ project_id, concept, max_per_group = 15 }`.
- ONE unfiltered `query_nodes(pid, None, None, None, 50_000)` scan in spawn_blocking; match
  nodes whose name contains the concept stem (lowercased; also try naive singular by
  trimming trailing 's').
- Group: data (db_table/db_column + stored_proc/sql nodes), state (global_state), UI
  (page/control/ui_container), logic (function/class), frontend (route_handler/web_service).
- For up to 5 anchor tables/state keys: incoming edges (QueriesTable/ReadsColumn/
  ReadsState/WritesState/SqlCalls/DataBinding) → "consumers" list (who reads/writes).
- Lexical layer: `lexical_search` (fts loose, top 50) → distinct files mentioning the
  concept that the graph groups missed → "mentioned in (verify manually)".
- Output: markdown grouped by role, each entry `name — node_id (file:lines)`, group counts,
  total touchpoint count, hint footer (trace_state_usage / get_table_schema /
  find_similar_changes) + freshness_footer.
- Test: integration test indexes a tiny fixture (table + page + codebehind + session key
  referencing "photo") and asserts the footprint lists the table, the page, the state key,
  and a lexical-only file.

### Task 2: find_similar_changes
Request `{ project_id, files: Vec<String>, max_commits = 500, top = 5 }`.
- Resolve repo dir from project record; spawn_blocking: GitWalker::open_repo, walk newest
  `max_commits` commits (reuse walk_commits/revwalk; skip commits with >80 files), collect
  (oid, summary, file list).
- Similarity input-set vs commit-set: Jaccard over token bags built per file from:
  full normalized path, each directory segment, extension, basename split on `_`/`-`/case
  boundaries. Pure helper `path_token_bag(&[String]) -> HashSet<String>` + unit tests.
- Rank, take `top`. Companion analysis across top commits: (a) exact files appearing in
  >= half of the top commits but NOT in input (registration/menu/config files recur
  exactly); (b) (directory, extension) pairs recurring in >= half but absent from input.
- Output: ranked commits (hash, summary, overlap-marked file list) + "Changes like this
  also touched — missing from your set" section + caveats (commits scanned, elapsed).
- Test: temp git repo with 3 feature commits sharing an anatomy (page + admin page +
  menu.xml); query with page-only file set; assert menu.xml + Admin pattern reported.

### Task 3: find_implementation_pattern
Request `{ project_id, pattern_query, max_examples = 3 }`.
- lexical_search (loose, top 30, memory ns); group hits by file; rank files by (hits,
  best score), prefer distinct directories; take max_examples exemplars.
- Per exemplar (spawn_blocking): graph nodes in that file (functions/classes, top 10);
  union of their outgoing SqlCalls/QueriesTable/ReadsState/WritesState targets; file
  node's TemporalCoupling neighbors (top 3); best-matching chunk snippet (snippet_of-style
  cap ~600 chars).
- "Common ingredients": artifacts (tables/sprocs/state keys) appearing in >= 2 exemplars.
- Output: per-exemplar card + common-ingredients + freshness footer.
- Test: fixture with two pages following the same pattern (both call same sproc);
  query the pattern; assert both exemplars + common ingredient listed.

### Task 4: registration + verification
- requests.rs: 3 structs (serde defaults; deny_unknown_fields).
- tools.rs: 3 #[tool] registrations with agent-grade descriptions; capabilities.rs flags.
- cargo check + run new tests + full engram_server sweep; commit per task.

**Decisions locked:** handler-local logic (no new service file); markdown output (JSON
later); v1 similarity is token-bag Jaccard (explainable); git walk bounded at request
time, no persistent mining store (that is the future feature-template index).
