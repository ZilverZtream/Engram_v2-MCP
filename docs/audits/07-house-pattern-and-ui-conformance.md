# Row 5 — House implementation pattern + UI conformance: `find_implementation_pattern`, `analyze_file_coding_style`, `get_ui_conformance` (M0 A/B)

Audit date 2026-08-28. Code at `ask-codebase-brain` (fef66ca). Live evidence
from OciusX project `5a35e8e0-d37a-41b3-a250-a26957e7aedb`, gen 824.
Research pass by an isolated Sonnet agent; every citation re-verified by
grep before it was written down.

## 1. Verdict

The auditor is right: `find_implementation_pattern` is lexical retrieval
wearing a pattern-tool name. It builds a `HybridQuery` and then calls
`lexical_search` only, hard-codes `top_k: 30`, ranks files by FTS hit count
then score, and its "common ingredients" are four data/state edge kinds
counted across exemplars. Live, an "admin page with a GridView and a save
button" query returned a TypeScript quantity manager as exemplar #1, no
common ingredients at all, and nothing about control flow — the
`Page_Load`/`IsPostBack` text in its output is a verbatim FTS snippet, not a
derived description. The idioms that actually make an OciusX admin page
(cascading bind chain, session-restore try/catch, feature-flag column
visibility, master-page header wiring) are all present in the top exemplar
and none are surfaced.

`analyze_file_coding_style` is more useful than the auditor's table
suggests (the VB static analyser produces real house rules) but it prints a
**hardcoded `Confidence: 1.00`** while discarding the engine's computed
confidence, and the VB analyser is gated to files with fewer than five
commits of history — the files with the most history get the least
specific guide.

`get_ui_conformance` does not exist. The design spec's M0 A/B is cheap and
its inputs are on disk (the 1908 and 1937 replay corpora); the
hand-authored icon-label contract that M0 needs has not been written.

## 2. Verified defects (`handlers/planning_tools.rs` = `plan`; `handlers/cognitive_tools.rs` = `cogt`; `services/cognitive_service.rs` = `cogs`; `engram_ml/src/mimicry.rs` = `mim`)

| # | Sev | Defect | Evidence |
|---|---|---|---|
| D1 | P0 | Lexical-only retrieval behind a "pattern" contract | `plan:936-940` `HybridQuery { namespace: "memory", .. top_k: 30, fts_mode: "loose" }`; `plan:949` `spawn_blocking(move \|\| engine.lexical_search(&q))` — no vector/hybrid path in `plan:918-1114` |
| D2 | P0 | Ranking by hit count, then FTS score | `plan:983-988` `b.1.0.cmp(&a.1.0).then(b.1.1.partial_cmp(&a.1.1)..)` |
| D3 | P0 | "Common ingredients" = four data/state edge kinds, count ≥ 2; no control flow, idioms, UI structure, error behaviour, design-family invariants | `plan:1038-1041` `SqlCalls, QueriesTable, ReadsState, WritesState` on `function` nodes; `plan:1100` `filter(\|(_, c)\| **c >= 2)`; `plan:1103` section title; `Calls`/`ContainsUi`/`UiLayoutNeighbor`/`DataBinding`/`TriggersPostback` never queried in the handler |
| D4 | P1 | Eight unreported caps | lexical hits 30 (`plan:939`); `query_nodes(.., 200)` `plan:1026`; `neighbors(.., 10)` per function × kind `plan:1043`; symbols listed 10; `TemporalCoupling` neighbours 3 `plan:1057`; data edges 12; snippet 600 chars; ingredients shown 10 — none appear in the output |
| D5 | P1 | Provider failures swallowed | `plan:1026` `.unwrap_or_default()`; `plan:1043` `if let Ok(neigh)`; `plan:1057-1058` `.unwrap_or_default()`; `plan:1067` `.await.unwrap_or_default()` — a panic in the blocking closure silently empties symbols/data/co-change for EVERY exemplar |
| D6 | P0 | `analyze_file_coding_style` prints a constant confidence | `cogt:2698` `out.push_str("Confidence: 1.00\n\n")`; the engine computes one (`mim:88` `pub confidence: f32`, rendered at `mim:103`) and only `.bullets` is taken at `cogs:148` |
| D7 | P1 | VB static analyser gated to thin history | `cogs:156-158` `const SHALLOW_HISTORY_THRESHOLD: usize = 5; if diffs.len() < SHALLOW_HISTORY_THRESHOLD ..` — files with ≥ 5 touching commits get only the generic mimicry bullets (`mim:251` `bullets.truncate(8)`) |
| D8 | P2 | Silent failures in coding style | `cogs:83-88` git-diff collection `.unwrap_or_else(\|_\| Ok(Vec::new())).unwrap_or_default()`; `cogs:97-106` file read `.ok()` chain; `cogt:2712-2715` `let _ = registry.set_meta(..)` cache write discarded |
| D9 | — | `get_ui_conformance` absent; M0 not run | spec `docs/superpowers/specs/2026-08-17-ui-conformance-design.md:165-170` "Draft for review"; no `*conformance*` artifact in Engram, OciusX or `C:\playwright`; replay inputs exist: `eval/data/p2/pr1908.json` ("upload map marker icons in a safe way"), `eval/data/p2/pr1937.json` ("camera icon doesn't go back to dimmed") + dossiers/diffs/ctx |

Already right (keep): both request structs `deny_unknown_fields`, all
fields read; `max_examples` clamp 1..=10 (`plan:930`); the VB static
analyser's rules (naming casing, `Is Nothing` guards, `Using`,
`Try/Catch` vs `On Error`, `Handles`, `SafeRedirect … Return`, XML-doc
counts, `Optional db As …Context = Nothing` idiom — `cogs:428-560`) are
exactly the house-style facts an agent needs; `diff_limit` is reported
("Analysed N commits."); the co-change line per exemplar.

## 3. Live OciusX evidence (2026-08-28, gen 824)

`find_implementation_pattern("admin page that lists and edits categories with a GridView and a save button")` — 0.40 s:

```
## Exemplar #1: Site/ts/qty/qtyManager.ts (5 match(es), score 55.90)          ← TypeScript, not a page
## Exemplar #2: Site/modules/dashboard/pages/admin/system/project/project.aspx.vb (3, 63.20)
   symbols: system_project_project (class); BindModuleDropDown; BindTypeDropDown; GridView1_RowDataBound;
            GridView1_PageIndexChanged; Page_Load; btnClear_Click; btnSok_Click; ddlType_SelectedIndexChanged;
            linqSource_Selecting            ← alphabetical, no call order, no _rv.* link
   data/state: [reads_state] Session:admin_modules_grunddata_projekt_ddlModule … [writes_state] Session:grunddata_projekt_GridView1
   snippet (line 5): Partial Class system_project_project … If Page.IsPostBack Then Return   ← raw FTS snippet
## Exemplar #3: Site/App_Code/redovisning/code/redovisningskategorier.vb (3, 58.57)
(no "Common ingredients" section — nothing reached the ≥2 threshold)
```

Idioms present in `project.aspx.vb:1-80` that the output does not describe:
`Page_Load` early-return on postback; `Try/Catch` around session-state
restore ("Silently ignore session restore errors"); GridView page-index
restore from `Session(..)`; feature-flag column visibility
(`SystemSettingStore.EnabledModules.FinancialResults OrElse …`); master-page
header `Master.PageHeader = Resources.label.Project`; cascading bind chain
`Page_Load → BindTypeDropDown() → BindModuleDropDown()`; "highlight last
modified row" in `GridView1_RowDataBound`.

`analyze_file_coding_style("…/admin/production/productioncodelistcategory.aspx.vb")` — 11.6 s (LLM round trip):

```
Confidence: 1.00                                   ← hardcoded (D6)
Method naming: camelCase (9/14 methods) … Guard pattern: `If x Is Nothing Then …` (7) …
Event wiring: `Handles` clauses (8) … `SafeRedirect(...)` MUST be followed by `Return` …
Documentation: XML doc comments on public API — 31 lines.
### LLM Analysis … PascalCase public members, `_camelCase` private fields, camelCase locals …
Analysed 1 commits.                                ← thin history ⇒ VB analyser ran (D7)
```

## 4. Redesign

### A. Defects to fix now (each gets a failing test first)

| Fix | Mechanism | Closes |
|---|---|---|
| A1 | Query the pattern with the real hybrid engine (lexical + vector, the same fusion `search_memory` uses), restricted to the exemplar KINDS the query implies (page code-behind for "admin page", domain class for "helper", …) — a TS file cannot be exemplar #1 for a WebForms page query | D1 |
| A2 | Rank by structural fit, not hit count: exemplar score = query relevance × structural-match (has the handler shapes / controls / data edges the query names) × recency of last merged change; the sort is testable | D2 |
| A3 | Derive the exemplar's SHAPE from the graph, not FTS text: ordered handler list with `Calls` edges (`Page_Load → BindX → _rv.y.Method`), controls (`ContainsUi`/`DataBinding`), postbacks (`TriggersPostback`), error idiom (Try/Catch presence per handler from the extractor), session/state edges; "common ingredients" become common SHAPES across exemplars (≥ 2 of 3 share `btnSave_Click → validate → _rv.* → rebind`) | D3 |
| A4 | Every cap in D4 reported (`hits 30/≥30`, `symbols 10 of 27`, …); provider failures reported per exemplar; the closure panic path returns an error | D4, D5 |
| A5 | `analyze_file_coding_style` prints the engine's computed confidence and the evidence basis ("N commits · file read · VB analyser ran/skipped") | D6 |
| A6 | Run the VB static analyser ALWAYS (it reads the file, history is irrelevant to it); merge with mimicry bullets; keep the 8-bullet cap but report it | D7 |
| A7 | Silent failures in D8 become reported lines; cache-write failure logged | D8 |
| A8 | Tests: fixture repo with two WebForms pages sharing a shape and one decoy TS file; ranking test (page beats TS for a page query); shape-derivation test; confidence-not-constant test; VB-analyser-runs-with-deep-history test | all |

### B. Redesign that needs evidence first — the M0 A/B (do this FIRST in this row)

Per the spec's own kill-switch: hand-author one conformance contract for
the icon-label family, inject it into the `/story` dry-run replays of PR
1908 and PR 1937 (inputs exist in `eval/data/p2/`), Sonnet arms only per
the eval rule, compare implementation score against the canonical table
(1908 = 100, 1937 = 85.7). If the score does not move, the Catalog is not
built. If it moves, `get_ui_conformance` M1 follows and A3's shape model is
its extractor.

## 5. Acceptance gate

| Gate | Measure | Target |
|---|---|---|
| G1 | For 5 OciusX pattern queries with a known best exemplar (chosen from merged PRs), the best exemplar is in the top 3 and no wrong-language file is | today: TS file #1 for a page query |
| G2 | Each exemplar output names an ordered handler chain with ≥ 1 `_rv.*`/DAL call resolved | today: 0 |
| G3 | 0 unreported caps; 0 silent provider failures | grep + tests |
| G4 | `Confidence:` varies with evidence (two files with different history/readability print different values) | today: constant 1.00 |
| G5 | M0 A/B executed and recorded (score delta on 1908/1937) | not run today |
| G6 | Latency: `find_implementation_pattern` ≤ 2 s (today 0.40 s; hybrid + shape adds work) | |
| G7 | Sweep green; new tests mutation-checked | |

## 6. Disposition table (fill at implementation)

| Item | Disposition |
|---|---|
| A1 | **fixed (slice 1)** — `infer_pattern_kind` (page / class / script / any from the query's words) + `kind_matches` on the path; applied only when ≥ 1 candidate matches, otherwise reported as NOT applied. Still lexical retrieval underneath (the hybrid/vector fusion is not needed for kind correctness; revisit if G1 fails live) |
| A2 | **fixed (slice 1)** — rank = kind match → structural score (handlers ×3, chains with calls ×2, controls ≤ 10, data ≤ 6) → lexical hits → score; directory diversity kept |
| A3 | **fixed (slice 1)** — per exemplar: ordered handler chains through `Calls` edges (depth ≤ 3) complemented by bare in-class calls read from the handler body (the VB extractor emits `Calls` for qualified calls only); controls `ID (Type)` from the sibling markup; data/state edges; co-changes. Common shapes = chains shared by ≥ 2 exemplars with class qualifiers dropped |
| A4 | **fixed (slice 1)** — `PatternCoverage`: lexical hits/cap/status (cap+1, 200), files, kind filter, candidates/cap 15, exemplar/handlers/controls/data/chain caps, `failures[]`; `output_json` |
| A5 | **fixed (slice 2)** — the handler prints the mimicry engine's computed `confidence` (`StyleAnalysisResult.confidence`) and a `Basis:` line (N commits (limit L) · file read · VB/static analyser ran (K rules) · mimicry M bullets · LLM yes/no) |
| A6 | **fixed (slice 2)** — the static analyser runs whenever the file is readable (the `< 5 commits` gate is gone); `StyleBasis.vb_analyser_ran` / `static_bullets` say so |
| A7 | **fixed (slice 2)** — git-history and file-read failures are `basis.failures[]` lines (the guide is still produced from what could be read); a cache-write failure is logged and printed "(not cached: …)" |
| A8 | **fixed (slices 1+2)** — `tests/implementation_pattern_tests.rs` x3 (real mini WebForms project via index_project) + `tests/coding_style_basis_tests.rs` x3 (real git repo, 6 commits: VB analyser ran + basis; printed confidence = engine's; non-git project reports the git failure and still gets the static guide). Unit x4 |
| M0 A/B | **blocked on user opt-in (2026-08-28)** — the replay needs either the OciusX `/story` dry-run orchestrator (registered domain agents + two human gates, run from an OciusX session) or the Engram Phase-2 A/B (in-session agents via the Workflow tool, which requires explicit opt-in). Inputs are ready (`eval/data/p2/pr1908.json`, `pr1937.json` + dossiers). Ask: hand-author the icon-label contract, then run both replays twice (with/without) — ~1 h of Sonnet agent time |
| G1 | **4/5 live** (§7) — the miss ("user control with a dropdown …" ⇒ `any`) is fixed in slice 3 (control-side page words) |
| G2 | **met (live)** — ordered chains with resolved DAL calls on the real pages (`btnSave_Click → … _cf.Form.Create | _cf.Form.Update`, `btnCopy_Click → _pp.mal.CopyForecastQtyByPrID | _rv.malredovisningsartiklar.…`) |
| G3 | **met by construction** (every cap in `PatternCoverage`; failures listed) |
| G4 | **met (test); live served the OLD cache** — the deployed handler returned the previous binary's cached "Confidence: 1.00" (key = file + HEAD oid). Slice 3 versions the cache key (`style_guide:v2:…`, test RED first); live check after its deploy |
| G5 | M0 A/B — blocked on user opt-in (see M0 row) |
| G6 | **met** — 0.85–1.06 s per query incl. the JSON-RPC round trip (target ≤ 2 s) |
| G7 | sweep13 (116 suites) |

## 7. Live evidence — slices 1+2 (2026-08-29 00:17 deploy, commits 95ac7d8 + 451d5d0, OciusX gen 828)

`find_implementation_pattern`, five queries with a known exemplar kind, `output_json`:

| query (kind) | inferred | filter | exemplar #1 | chains | lexical | ms |
|---|---|---|---|---|---|---|
| admin page … GridView … save button (page) | page | applied | `…/admin/system/customform/form_edit.aspx.vb` | 31 | truncated 200/200 | 998 |
| helper class … installationsobjekt … data context (class) | class | applied | `App_Code/installationsobjekt/api-json/api-installationsobjektprojekt.vb` | 0 | truncated | 983 |
| typescript module … quantities … marker dialog (script) | script | applied | `Site/ts/qty/qtyManager.ts` | 0 | truncated | 847 |
| user control with a dropdown … search button (page) | **any** | not applied | `…/projectplanner/planner.aspx` (markup) | 31 | truncated | 1060 |
| api endpoint that validates pr_id … json (class) | class | applied | `App_Code/redovisning/api-json/api-redovisning.vb` | 0 | truncated | 895 |

4/5 kind-correct, 0 wrong-language exemplars, every lexical page reported
`truncated 200/200` (the cap fills on every query — honest), 0 provider
failures. Chains are real house shapes:
`btnBack_Click → sharedfunc.SafeRedirect → .CompleteRequest | …Response.Redirect`,
`btnSave_Click → GetAccessLevel | Integer.TryParse | ShowHideCustomExports | … | _cf.Form.Create | _cf.Form.Update`.
`common_shapes` was empty for every query — three different pages rarely
share an exact chain; the key normalisation helps only within a family.

`analyze_file_coding_style` on `productioncodelistcategory.aspx.vb`:
returned `Confidence: 1.00 / Analysed 1 commits.` — the previous binary's
cached text (cache key = file + HEAD oid). Slice 3 versions the key.
