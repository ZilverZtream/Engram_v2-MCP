# Row 8 — Security, settings, and durable project laws: `map_guards_and_settings`, `immune_check`, repo rules

Audit date 2026-08-28. Code at `ask-codebase-brain` (fef66ca). Live evidence
from OciusX project `5a35e8e0-d37a-41b3-a250-a26957e7aedb`, gen 824.
Research pass by an isolated Sonnet agent; every citation re-verified by
grep before it was written down.

## 1. Verdict

`map_guards_and_settings` answers "does this function call something
guard-shaped?" and presents the answer as "guarded / UNGUARDED". The
verdict comes from index-time regex metadata (`permission_checks`,
`guard_roles`), a project-wide 200k node scan even when a scope is given,
and a strict binary with no "unknown" state. On the PR-2032 file it says
"29 of 50 functions have permission checks" — and:

- `ioGetCountByCategory:2346` has **no permission check at all** and is
  correctly counted among the 21 unguarded, but is **not shown** because
  the unguarded list is silently cut at 10;
- the four bulk-write endpoints and the PR-2032 sibling are marked GUARDED
  on a **role-level** `CheckRead`/`CheckWrite` while they read a
  client-posted `pr_id` and never call the project's own object-level
  `Check_pr_id` idiom (77 uses project-wide) — the role-vs-object
  distinction the auditor asked for does not exist in the tool;
- `ioUpdateBaseTypeInBulk`'s real guard is `CanUserBulkUpdate()` (a name
  the regex cannot see); the tool credits a conditional secondary
  `CheckWrite` instead — right verdict, wrong evidence;
- the extractor already tags low-fidelity symbols (`extraction_fallback`)
  and this tool never reads the tag.

`immune_check` ignores its `include_content` field, reports `top_k` as
"Matches Found", and its file-specific escalation keys on `immune_`-prefixed
rules — of which OciusX has zero (30 `cr_` rules exist; the memory note
"repo_rules=0" is stale). `list_repo_rules` is honest and uncapped.

## 2. Verified defects (`handlers/planning_tools.rs` = `plan`; `handlers/cognitive_tools.rs` = `cogt`; `engram_index/src/vb_extractor.rs` = `vbx`)

| # | Sev | Defect | Evidence |
|---|---|---|---|
| D1 | P0 | Verdict = regex name-shape metadata, binary, no unknown state | `plan:1864-1874` reads `metadata["permission_checks"]` / `["guard_roles"]`; `plan:1888` `if checks.is_empty() { unguarded } else { guarded }`; metadata written by `RE_VB_GUARD_CALL` `vbx:74-83` (`isinrole`, `checkread/write`, `check_<entity>id`, `authorize…` shapes) attributed to whichever function span contains the line (`cs_extractor.rs:413-419` for C#) |
| D2 | P0 | No role-level vs object-level distinction; no tenant/boundary model | live: 5 endpoints reading client `pr_id` marked GUARDED on `CheckRead`/`CheckWrite` alone (§3); grep for boundary/tenant/ordering/denial concepts in the handler → none; a tenant analyser exists only in the migration service (`full_project_migration_service/analyzers/multi_tenancy.rs:33-58`) and is not wired here |
| D3 | P0 | Unguarded list silently cut at 10; guarded at 20 | `plan:1990` `unguarded.iter().take(10)`, `plan:1996` `guarded.iter().take(20)`; header count honest, names not — live: 11 unguarded functions incl. `ioGetCountByCategory` never printed |
| D4 | P1 | Whole-project scan even with `scope`; no truncation flag | `plan:1841-1842` `query_nodes(&pid, None, None, None, NODE_SCAN_LIMIT).unwrap_or_default()` (200k, `handlers/mod.rs:26`) with `file_path: None`, filtered client-side; no `scan_truncated` (contrast `plan:401`, `settings_tools.rs:82`) |
| D5 | P1 | Unreported caps in the settings half | `plan:1908` `scoped_fn_ids.iter().take(300)` (settings-read enrichment only, not the guard classification); `plan:1909` `neighbors(.., ReadsSetting, .., 20)`; `plan:1928` `settings_tables.iter().take(10)`; `plan:1930` `find_incoming_edges_with_kind(.., 500)`; display caps 20 / 8 with no "N more" |
| D6 | P1 | Graph failures become empty/zero | `plan:1842` `.unwrap_or_default()`; `plan:1909` `if let Ok(neigh)`; `plan:1945` consumer count `.unwrap_or(0)` |
| D7 | P1 | Extraction-fidelity signal dropped | `vbx:1565-1572` writes `extraction_fallback` so "confidence scoring and audits can discount" degraded symbols; `grep -c extraction_fallback plan` → 0 |
| D8 | P1 | Guard helpers with non-shape names are invisible | live: `CanUserBulkUpdate()` (`api-installationsobjektprojekt.vb:857, :979`, defined `:1051`, itself calling `CheckIfAdminOrArbetsledare` + 2× `CheckWrite`) matches no regex; the verdict rides on a conditional secondary guard |
| D9 | P1 | `immune_check`: `include_content` never read; `top_k` shown as total; escalation keyed on a rule prefix the project does not have | `requests.rs:1591-1593` field; only use is `cogt:2963` inside `handle_anti_pattern_guard`; snippets printed unconditionally `cogt:2807-2812`; `cogt:2735` `top_k: req.sanitized_top_k()` → "Matches Found: 10"; `cogt:2775` `rule_id.starts_with("immune_")` — live 0 of 30 rules |
| D10 | P2 | `immune_check` repo-rule cross-ref swallowed | `cogt:2771` / `project_tools.rs:3489` `list_repo_rules(..).unwrap_or_default()` — registry error silently disables escalation |

Already right (keep): `MapGuardsAndSettingsRequest` (`requests.rs:509-515`)
has two fields, both read; the "House auth patterns" section (live: 77 ×
`check_pr_id`, `CheckRead`/`CheckWrite` families) is exactly the sibling
evidence a new endpoint should match; `list_repo_rules` is uncapped
(`registry.rs:370-383`) and `immune_rule_matches_path` (`cogt:62-93`) is
glob-honest and unit-tested; `add_repo_rule` reads every field.

## 3. Live OciusX evidence (2026-08-28, gen 824)

`map_guards_and_settings {scope: "Site/App_Code/installationsobjekt/api-json/api-installationsobjektprojekt.vb"}` — 0.18 s:
`29 of 50 function(s) in scope have permission checks.` Unguarded shown 10,
guarded shown 20.

| Function (line) | Tool | Source truth |
|---|---|---|
| `ioGetIdsFilteredByMarkerCheckListItemStatus` (2205) | GUARDED `checkread` | `:2211 If Not _us.UserAccess.CheckRead(vs_karta_io_objekt)` — role-level only; reads client `pr_id` `:2218`, queries by it `:2224`, **no `Check_pr_id`** (the CodeRabbit/PR-2032 finding) |
| `ioGetCountByCategory` (2346) | **not printed** (in the 11 cut by `take(10)`) | **no permission check**; `pr_id` from `qry.params` `:2350` straight into `GetAllByCheckingTotalProject(pr_id)` `:2359` |
| `ioUpdateBaseTypeInBulk` (851) | GUARDED `checkwrite` | real guard `:857 If Not CanUserBulkUpdate()` (invisible to regex); the credited `CheckWrite` `:894` is conditional on `isUpdatingAR` |
| `iopDeleteInBulk` (973) | **not printed** (beyond `take(20)`) | guarded by `CanUserBulkUpdate()` `:979`; surfaces only as a settings consumer line |
| `iomsBulkUpdate` (2417) | GUARDED `checkwrite` | `:2425` role-level; client `pr_id` `:2432` → `_gd.projekt.GetByID(prId)`; no object scoping |
| `iomsBulkPreCheck` (2699) | GUARDED `checkwrite` | `:2706-2709` role-level; client `pr_id` `:2714`; no object scoping |

`immune_check` (snippet: `.Where(Function(x) x.pr_id = Request("pr_id"))`, same file):
`Matches Found: 10` (= `top_k`), highest similarity 0.016 (warn threshold
0.600), escalated to 🟡 WARNING by match-count ≥ 3 alone; snippets printed
although `include_content` was not set; no `immune_` rule exists so the
file-specific escalation path never engaged.

`list_repo_rules`: 30 rules, all `cr_`-prefixed, extension-wildcard
patterns (`**/*.vb`, `**/*.ts`, `**/*.aspx`, …) from the CR-history
ingestion; no cap note (none needed).

## 4. Redesign

### A. Defects to fix now (each gets a failing test first)

| Fix | Mechanism | Closes |
|---|---|---|
| A1 | Three-state verdict per function: `Guarded{family, level: Role \| Object \| Tenant, unconditional: bool}` / `Unguarded` / `Unknown{reason}` (no body read, `extraction_fallback` set, guard via unresolved helper). Level derived from the matched idiom: `Check_<entity>id`/`Check_pr_id` ⇒ Object; `CheckRead/CheckWrite/IsInRole` ⇒ Role | D1, D2, D7 |
| A2 | Client-input rule: a function that reads a scope key from client input (`qry.params("pr_id")`, `Request(..)`, DTO field named like a house scope key) and has no Object-level guard for that key is reported `ROLE-ONLY — object scope unchecked` even when a Role guard exists. This is the H2/H1 finding class from PR 2032, made mechanical | D2 |
| A3 | Guard helper resolution: a call whose target function's own metadata contains guard checks inherits them (`CanUserBulkUpdate()` ⇒ `CheckIfAdminOrArbetsledare + CheckWrite`) — one hop through `Calls`; conditional vs unconditional recorded from the enclosing `If` at extraction | D8 |
| A4 | Scoped query at the store (`file_path` param), `scan_truncated` reported; every cap in D5 reported; lists never cut without "… and N more (full list in JSON)"; `output_json` added | D3, D4, D5 |
| A5 | Graph failures reported (`settings: FAILED (..)`), never empty/zero | D6 |
| A6 | `immune_check`: honour `include_content`; print `top_k` as a cap ("10 shown (cap) — raise top_k"); escalation on rule KIND (revert-derived) not id prefix; repo-rule lookup failure reported | D9, D10 |
| A7 | Tests: fixture file with role-only, object-level, helper-wrapped, conditional, and unguarded functions (expected verdict table); scope-cut regression (an unguarded function beyond position 10 must be printed or counted with an explicit pointer); `include_content` honoured; `top_k` cap text | all |

### B. Redesign that needs evidence first

- **Typed security policy model** (boundary type API / page handler /
  background job / data helper; tenant + database context; required guard
  family and ORDER; denial-path expectations): A1-A3 give the per-function
  facts; the policy (what is REQUIRED where) should be derived from the
  house patterns section + the CR history (`cr_` rules already name the
  marker authorization model) and validated on the six endpoints above
  before it gates anything.
- **Negative-path tests** (does the endpoint return 403 on the wrong
  project?) belong to `derive_test_matrix`, not this tool.

## 5. Acceptance gate

| Gate | Measure | Target |
|---|---|---|
| G1 | Six-endpoint truth table | `ioGetCountByCategory` printed as UNGUARDED; the five `pr_id` readers flagged ROLE-ONLY; `ioUpdateBaseTypeInBulk`/`iopDeleteInBulk` credited to `CanUserBulkUpdate` |
| G2 | 0 silent list cuts; 0 unreported caps; scope query at the store | grep + tests |
| G3 | `Unknown` verdicts appear for `extraction_fallback` symbols (count reported) | today: none |
| G4 | `immune_check` honours `include_content`; "Matches Found" labelled as capped | |
| G5 | Latency ≤ 1 s per scope (today 0.18 s with a full-project scan; store-scoped should be faster) | |
| G6 | Sweep green; new tests mutation-checked | |

## 6. Disposition table (fill at implementation)

| Item | Disposition |
|---|---|
| A1 | |
| A2 | |
| A3 | |
| A4 | |
| A5 | |
| A6 | |
| A7 | |
| G1-G6 | |
