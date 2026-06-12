# Guards/Settings Intelligence + Story Planner — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:executing-plans.

**Goal (5 items):** (1) settings-read + permission-check extraction (C#/VB, generic
name-shape patterns validated against OciusX), (2) `map_guards_and_settings` tool,
(3) `guard_parity` pre-commit gate, (4) `plan_user_story` composite brief,
(5) output upgrades for compute_blast_radius + find_symbol_references.

**Generic-by-design:** detection uses pure name-shape regex families — `AppSettings["K"]`
/ `AppSettings("K")` / `My.Settings.K` reads; guard calls shaped like
`(?i)(is*admin*|isinrole|isuserinrole|check*access*|check*permission*|has*permission*|
has*access*|hasrole*|require*(role|admin)*|authorize*|demand*)\(`; settings TABLES =
db_table nodes whose name matches `(?i)(setting|config|option|param|preference)`.
OciusX survey confirms coverage (ConfigurationManager.AppSettings ×67, IsUserInRole ×26,
IsInRole ×8, CheckAccessLevelByAccessObject, IsContactableAdmin, ss_systemsettings,
EmailSettings) with zero app-specific names in code.

**Key wiring facts:**
- web.config appSettings already become symbols kind `app_setting` (config_extractor) and
  are NOT in ingest's NON_SYMBOL_KINDS — so a `reads_setting` edge with
  `target_kind: Some("app_setting")`, `target_name: <Key>`, no line resolves to the real
  config node via the batch resolver, or becomes `::Key` (unresolved = not in web.config).
- New EdgeKind::ReadsSetting: variant + as_str/parse "reads_setting" + ALL + ingest
  raw-kind mapping.
- Guard hits annotate the enclosing function symbol metadata: `permission_checks`
  (deduped, `;`-joined call names) + `guard_roles` (captured string literals from
  IsInRole/IsUserInRole-style args).

### Task A — extraction
- engram_graph/store.rs: EdgeKind::ReadsSetting.
- cs_extractor: RE_CS_APPSETTINGS `AppSettings\s*\[\s*"([^"]+)"`; guard family regex +
  role-literal capture; emit reads_setting edges (source = enclosing method fqn via
  method_ranges); post-loop annotate function symbols (range-contains) with metadata.
- vb_extractor fallback: `AppSettings\s*\(\s*"([^"]+)"`, `My\.Settings\.(\w+)`; same
  guard family; edges from current_method; annotate symbols post-loop by fqn.
- ingest_service: "reads_setting" → ReadsSetting.
- Unit tests in both extractors.

### Task B — map_guards_and_settings (planning_tools.rs)
Request { project_id, scope: Option<String> }. One node scan: in-scope functions w/
permission_checks metadata (guarded/total parity), project-wide guard-name counts
("house auth patterns"), roles seen, app_setting nodes + per-function ReadsSetting
neighbors in scope, settings-shaped db_tables + incoming consumer counts. next-hints →
map_auth_config, trace_state_usage. Integration test in planning_tools_test.rs.

### Task C — guard_parity gate (gates.rs)
Trigger per changed .cs/.vb/.asmx file: added_content contains a new endpoint/handler
(WebMethod attribute, `_Click(` handler, `Handles ... .Click`) AND no guard-family match
in added_content AND the on-disk file's other content has ≥1 guard match → WARNING
finding listing the sibling guards (and the file's settings reads as evidence). Register
in all_gates(). Test in pre_commit_review_gate_tests.rs following its harness.

### Task D — plan_user_story (planning_tools.rs)
Request { project_id, story, concepts: Option<Vec<String>> }. Deterministic: stopword-
filtered concept extraction (≤3) → per-concept trimmed footprint section; trimmed
pattern-exemplar section (query = filtered story); trimmed guards/settings section
(project-wide); then a concrete checklist with prefilled next tool calls
(find_similar_changes, check_edit_safety, pre_commit_review). Sub-outputs reused by
calling the sibling handlers and trimming to a line budget. Integration test: the
photos story over the footprint fixture.

### Task E — output upgrades
- handle_compute_blast_radius (cognitive_tools.rs:2573): name top incoming/downstream
  contributors with file:line + fqn (bounded get_node enrichment), keep score breakdown.
- handle_find_symbol_references: resolve shown edge endpoints to `Name (file:line)`
  instead of bare node_ids (bounded to displayed items, inside existing blocking hop).
- Keep all existing tests green (golden/source-scan tests may pin formats — adjust only
  if they fail for rendering-additive reasons).

### Task F — registration, sweep, commits
requests.rs structs, tools.rs descriptions (honest: ".NET idiom coverage today"),
capabilities.rs, TODO.md, full two-package sweep, commit per task.
