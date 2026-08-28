# Row 1 — Story-to-change scope: `get_change_set` (+ `detect_incomplete_changes`, `find_similar_changes`)

Audit date 2026-08-28. Code at `ask-codebase-brain` (fef66ca). Live evidence
from OciusX project `5a35e8e0-d37a-41b3-a250-a26957e7aedb`, gen 824.
Research pass by an isolated Sonnet agent (4 identical live runs); every
citation re-verified by grep before it was written down.

## 1. Verdict

`get_change_set` is the only capability with demonstrated implementation
uplift (eval: 3.2 vs 2.8, zero losses — `eval/README.md:221`), so it stays
rank 1. The live result confirms the auditor's substance and corrects two
specifics:

- concept extraction for a story with no path/auth cue is literally the
  first three non-stopword words ≥ 4 chars in document order — live:
  `main, reporting, category` — and the cross-language bridge meant to map
  that to the Swedish code vocabulary swallows its own failure and
  contributed nothing;
- ranking is by signal COUNT (any two signals outrank one precise concept
  hit), the co-change/history tier is never capped while the weak tail is
  capped silently at 18 per layer;
- 149 candidate files, cross-domain noise, and the actual target family
  (`rk_redovisningskategorier.sql`, `redovisningskategorier.vb`,
  `productioncodelistmaincategory.aspx(.vb)`, `api-redovisning.vb`,
  `iFalt.dbml`) absent in 4/4 runs;
- 27-32 s per call because the tool embeds an 800-commit git walk plus two
  full 200k-node scans, and drops the walk's own PARTIAL marker at the
  tool boundary.

Corrections to the auditor's report: `productioncodelistcategory` and
`linkcodeandforecastqty` ARE present in the output (both `.aspx`/`.aspx.vb`);
the auditor's cited lines (`:1635`, `:3413`) are a stopword-array entry and a
test fixture respectively — the real sites are below; 46.5 s / 19 s did not
reproduce (27-32 s with a warm co-change cache; no cold run was captured);
`huvudredovisningskategori` occurs nowhere in the codebase, so its absence
is not a miss — the miss is the `redovisningskategori*` FAMILY.

## 2. Verified defects (`handlers/planning_tools.rs` unless stated)

| # | Sev | Defect | Evidence |
|---|---|---|---|
| D1 | P0 | Concept extraction = first 3 acceptable words in document order | `extract_story_concepts` `:1630`; loop `:1786-1795` `for word in story.split(..)` (stopword filter, ≥ 4 chars, take 3); live concepts `main, reporting, category` |
| D2 | P0 | Cross-language KB bridge swallows failure and contributed 0 | `:3788-3792` `spawn_blocking(.. lexical_search(&q)).await.ok().and_then(\|r\| r.ok()).unwrap_or_default()`; the comment `:3763-3770` describes it as the English/Swedish gap closer |
| D3 | P0 | Ranking by signal count; golden tier uncapped, weak tail capped silently | `change_set_tier` `:3003-3016` (`cochange`/`history` ⇒ tier 0/1; any 2 signals ⇒ tier 2; single `concept` ⇒ tier 3); cap loop `:3673-3686` fires only for tier ≥ 2, `tail > 18` per layer, `continue` with no "N omitted"; design stated at `:3489-3491` |
| D4 | P0 | Precision: 149 candidates, cross-domain noise, target family absent | live §3: `user_edit.aspx(.vb)`, `import_networkdesign*`, `visualisering-map.vb`/`vsmap/*`, `taskmanagement/calendarservices`, `trp_main.js`, `roqqtymanager.js`/`qtyManager.ts` present; the 5 family files absent in 4/4 runs |
| D5 | P1 | Interactive call embeds an 800-commit git walk | `:3942-3947` `handle_find_similar_changes(FindSimilarChangesRequest { .. max_commits: 800, top: 8 ..})`; own comment `:3906-3917` "RE-WALKS GIT (slow)" vs detect = "graph-neighbour lookup (cheap)"; live 27-32 s vs `detect_incomplete_changes` 0.75 s |
| D6 | P1 | PARTIAL marker dropped at the tool boundary | `find_similar_changes` emits "PARTIAL: the walk hit its time budget…" `:864-869` (budget `co_change_budget()` `:364-371`, default 20 s); `get_change_set` passes the text through `change_set_paths` `:2689-2699` (path regex) — the caveat never reaches the caller |
| D7 | P1 | Duplicated full scans in one call | `handle_detect_incomplete_changes` called twice `:3954`, `:4044`; each runs `query_nodes(&pid, None, None, None, NODE_SCAN_LIMIT)` `:5495` (200k, `handlers/mod.rs:26`); further full scans for permission gates / shared components; nothing shared in-call |
| D8 | P1 | Vector arm swallowed; probably dead on OciusX | `:3980` `if let Ok(r) = self.handle_vector_search(..)`; live 0/149 candidates carry a `vector`/`vtop` tag in 4/4 runs — no way to tell "timed out" from "contributed nothing" |
| D9 | P1 | Path identity mixed | live 88/149 candidates `site/…`, 61 bare (`modules/dashboard/…`, `app_code/…`) — historical vs current paths (the `HistoryFileRef` item deferred in blast round 5) |
| D10 | P2 | Permission-gates section caps gate types at 10 silently | `:4964` `rows.into_iter().take(10)` |

Already right (keep): deterministic output (4 identical runs); layer
grouping + co-change-first design that produced the eval uplift;
`detect_incomplete_changes` fast (0.75 s) with the honest "No strong
co-change…" phrasing and the house-conventions section (17 rules on the
probe); symmetric-sibling / permission-gates / shared-components sections;
all three request structs `deny_unknown_fields` with every field read; the
eval harness (`eval/`, implementation score) exists and is the gate.

## 3. Live OciusX evidence (2026-08-28, gen 824)

Story: *"As an admin I want to set a main reporting category
(huvudredovisningskategori) on a production code list category so that
time reports roll up to it"*

```
concepts: main, reporting, category
## Candidate files: 149 (Server 85 · Client 27 · Resources 14 · SQL 18 · Markup 4 · Other 1); 165 incl. sections
paths: 88 site/-prefixed, 61 bare
first 15 (Server, in-file order) — all tagged [cochange|concept]:
  site/app_code/ifalt.designer.vb · site/modules/dashboard/dashboard.master · site/app_code/grunddata/code/projekt.vb
  site/app_code/installationsobjekt/api-json/api-installationsobjektprojekt.vb · site/app_code/visualisering/code/visualisering-map.vb
  app_code/api-v2/services/reportingofquantities/interfaces/iroqentryservice.vb
  modules/dashboard/pages/admin/production/import_price_list.aspx.vb · …/productioncodelist.aspx.vb
  …/productioncodelistcategory.aspx.vb · …/productionprojectcodelist_edit.aspx.vb
  modules/dashboard/pages/public/producedq/estimatedvsreportedquantities.aspx.vb
  site/app_code/api-v2/datatransferobjects/reportingofquantities/roqpricelistitemcategory-out.vb · …/roqreportitem-out.vb
  site/app_code/api-v2/services/reportingofquantities/roqentryservice.vb
  modules/dashboard/pages/admin/system/import/import_networkdesign.aspx.vb
present: productioncodelistcategory.aspx(+.vb), linkcodeandforecastqty.aspx(+.vb)
absent (4/4 runs): productioncodelistmaincategory.aspx(+.vb), rk_redovisningskategorier.sql,
  redovisningskategorier.vb, api-redovisning.vb, iFalt.dbml(+.layout), redovisning*.rdl
wall: 31.97 s, 27.11 s (warm co-change cache) · vector tags: 0/149
```

`detect_incomplete_changes {edited_files:[api-installationsobjektprojekt.vb]}` — 0.75 s:
co-change partners (top: `Site/ts/qty/qtyManager.ts`, 4785 co-changes),
"Implemented but never wired (0 callers)" (`api.iopGet:1838`), 17 house
conventions.

What a complete answer contains (repo grep): `productioncodelistmaincategory`
→ `…/productioncodelistmaincategory.aspx(+.vb)`, `Site/App_Code/RouteConfig.vb`,
`…/productioncodelistcategory.aspx`; the `redovisningskategori` family per
row-4 doc §3 (45 files).

## 4. Redesign

### A. Defects to fix now (each gets a failing test first)

| Fix | Mechanism | Closes |
|---|---|---|
| A1 | Typed `TaskSpec` from the story: change kind, entities as NOUN PHRASES (not first-3 words), user action, UI surfaces, security scope, data change. Entities resolved against the graph through the row-4 `ConceptIdentity` (table ↔ entity ↔ designer member ↔ class ↔ page), with the language bridge as a first-class provider whose failure is REPORTED | D1, D2 |
| A2 | Per-file rationale and directness ranking: every candidate carries `why: [EntityMatch(name) \| DirectConsumer(edge) \| Exemplar(pr) \| AtomicFamily(of) \| CoChange{support, confidence, lift, recency}]`; rank by directness class, then score; co-change contributes by confidence/lift with a cap, never by raw count; atomic families (resx/designer/dbml) preserved as units | D3, D4 |
| A3 | Precomputed co-change cohorts (approved-PR units) at index/refresh; `get_change_set` never walks git interactively; `find_similar_changes` stays the explicit deep pass and its PARTIAL marker propagates verbatim | D5, D6 |
| A4 | One graph snapshot per call: the node scan runs once and is shared by the two detect calls and the gates/shared-components sections; per-call work budget reported | D7 |
| A5 | Vector arm status line (`vector: 12 hits \| timed out after N ms \| unavailable`) — never silent | D8 |
| A6 | Canonical path layer (`HistoryFileRef`): every rendered path is the current repo-relative path or labelled `historical:` | D9 |
| A7 | JSON output with per-layer coverage, ambiguity (unresolved entities), omissions (what the caps cut and why), counterevidence; the 18/layer and 10-gate caps become reported omissions | D3, D10 |
| A8 | Tests: extractor on multilingual stories (Swedish family resolved through the identity layer); ranking (`concept+direct` beats two weak signals); PARTIAL propagation; path canonicalisation; single-scan assertion | all |

### B. Redesign that needs evidence first

- **Calibrated probability ranking**: train/validate on the merged-PR
  corpus already in `eval/` (story → PR files). Ship A2's ordinal ranking
  first; calibrate when the harness shows where it fails.
- **Precision vs recall trade-off**: the eval optimises implementation
  score; agents reading 149 files is the cost side. Measure candidate
  count vs score before setting a precision target.

## 5. Acceptance gate

| Gate | Measure | Target |
|---|---|---|
| G1 | Implementation score on the existing eval set | ≥ 3.2 (no regression); precision (candidates ∩ merged-PR files / candidates) reported before/after |
| G2 | The `redovisningskategori` story names the 5 family files | present in every run |
| G3 | Latency | ≤ 5 s warm (today 27-32 s) with cohorts precomputed; cold measured and reported |
| G4 | Every cap/omission reported; PARTIAL propagates | test-enforced |
| G5 | Vector arm status visible | live line present |
| G6 | Paths 100 % canonical | 0 bare/historical paths without label |
| G7 | Sweep green; new tests mutation-checked | |

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
| A8 | |
| G1-G7 | |
