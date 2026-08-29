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

## 6. Disposition table (implementation in slices — slice 1 landed 2026-08-28)

| Item | Disposition |
|---|---|
| A1 | **built, gate FAILED, shipped OPT-IN** — `extract_story_concept_candidates` (recipe + parenthesized glosses + noun phrases) and `resolve_story_concepts` (index-corroborated; compound suffix split `huvudredovisningskategori → redovisningskategori`) exist and are always REPORTED (`coverage.concept_candidates`, markdown line); retrieving on them is `expand_concepts=true` only — on the 5-PR gate they inflated the weak tier past the tail cap: recall 89.2% → 86.5%, precision 5.1% → 4.8% (§7) |
| A2 | **fixed (neutral on the gate)** — `change_set_tier` ranks by evidence directness (golden > concept+associative > concept alone > associative-only; two weak signals no longer outrank an entity match); 5-PR gate: recall 33/37 → 33/37, precision 5.1% → 5.0%, candidates 649 → 655 — no regression, no gain; kept for the principle, measured honestly |
| A3 | **open** — per-arm timings (slice 1) show the co-change arm at 3-6 s of a 9.7 s call; the rest is the post-render sections. Cohort precompute deferred: the gate shows the real problem is precision (5%, ~130 candidates), not latency |
| A4 | **fixed (slice 2)** — one `NodeSnapshot` (Arc<Vec<Node>>) per call shared by every `detect_incomplete_changes` pass via `detect_incomplete_changes_with`; `coverage.node_scans` reports the count (test `one_call_performs_one_full_node_scan`) |
| A5 | **fixed (slice 1)** — vector arm reports `failed — vector search unavailable: …` / `complete (N hits)`; every arm has an `ArmCoverage` (status, hits, ms, note); the `if let Ok` swallows are gone |
| A6 | **fixed (slice 1)** — candidates rendered as the INDEXED path (case + `Site/` restored) via `list_file_node_metadata`; unindexed paths kept and labelled `historical` (markdown suffix + JSON flag); test `candidate_paths_are_the_indexed_canonical_forms` |
| A7 | **fixed (slice 1)** — `output_json` payload {concepts, files[{path, layer, tier, signals, why, historical}], coverage, omissions}; tail-cap cuts reported (markdown note + `omissions[]` with layer/reason) instead of silent `continue`; PARTIAL from `find_similar_changes` propagates as `cochange.status = truncated` with the walk's own line as the note; one shared `change_set_rows` feeds both renderers |
| A8 | **partly (slice 1)** — omission reporting + coverage + canonical-path tests in place (`tests/change_set_evidence_tests.rs` x4, unit `change_set_rows_tests` mutation-checked); extractor/ranking/PARTIAL-under-budget tests come with their slices |
| D10 | **GREEN locally 2026-08-29 04:35 (slice 3, sweep21 pending)** — the gate scan moves before the JSON early-return (output_json never carried the section); `output_json.permission_gates = {cap, total, shown, listed, omitted}`; markdown states "... and N more gate type(s)" and the gate-definition-file cut; `tests/change_set_gate_cap_tests.rs` (2) RED first — observed 2026-08-29 04:27: JSON carries no `permission_gates` at all; the markdown lists ten and states no cut |
| G1-G7 | slice 4 gate (eval) + live evidence per slice in §7 |

## 7. Live evidence after deploy — slice 1 (2026-08-28, binary 18:52, OciusX healed graph 2,274 / 18,144)

Same story as §3, `output_json: true`, wall **9.7 s** (§3: 27-32 s — the
warm co-change cache and the healed graph account for most of it; the arms
now report their own cost):

```
concepts: main, reporting, category
files: 157 | omissions: 20 (reported, with layer + reason) | historical: 1 (labelled)
paths: 137 Site/-prefixed canonical; 1 non-canonical = the historical one
  (modules/dashboard/pages/admin/system/projectplanner/resources.aspx.vb — no longer in the index)
coverage:
  concept   complete (103 hits,   287 ms)
  history   complete (  5 hits,   289 ms)
  cochange  complete ( 87 hits, 3 210 ms)   ← the only slow arm; PARTIAL would propagate here
  vector    complete ( 21 hits,   347 ms)   ← alive today (the §3 "0/149 vector tags" reading was
                                              a dead arm at the time, now visible either way)
  kb_bridge complete (  0 hits,    12 ms)   ← honest zero: nothing bridged
  family    complete (150 hits)
layers: Server 85 · Client 27 · Resources 21 · Data 18 · Markup 5 · Other 1
why (sample):
  Site/App_Code/RouteConfig.vb          -> history search hit; co-changed with the seed files
  Site/App_Code/iFalt.designer.vb       -> co-changed with the seed files; matches concept 'main'
  Site/modules/dashboard/dashboard.master -> co-changed; presentation-layer co-change partner
target family: Site/App_Code/redovisning/code/redovisningskategorier.vb PRESENT (absent in 4/4 §3 runs)
omissions (sample): IRoqPriceListItemCategoryService.vb, ITimeCodeCategoryService.vb, copycodelistcategory.aspx
```

Markdown run: `## Coverage` section present with the six arm lines, the
omission count, and the historical-path suffix on the one unindexed file.

What this slice did NOT change (and the numbers show): concept extraction is
still `main, reporting, category` (A1), the candidate count is still ~150
(A2), the co-change arm still walks git in-call (A3). Those are slices 3-4
with the eval gate.

## 7b. Slices 2 and 4 — gate results (2026-08-28)

Gate = `eval/_recall_subset.py` on dossiers regenerated through the deployed
binary (`eval/phase2_prep.py --pr N --reuse`, daemon stopped) for PRs 1908,
1913, 1937, 1933, 1967 — the same `covered()` rule as `_recall_sweep.py`,
plus precision. The harness parses the RENDERED candidate list, so files
cut by the 18-per-layer tail cap cost recall.

| variant | recall | precision | candidates (sum) |
|---|---|---|---|
| baseline (slice 2 binary) | 33/37 = 89.2 % | 33/649 = 5.1 % | 649 |
| A1 + A2 (concept expansion on) | 32/37 = **86.5 %** | 4.8 % | 662 |
| A2 only (shipped default) | 33/37 = 89.2 % | 5.0 % | 655 |

The A1 loss on PR 1913 was `app_code/users-security/code/aspnetusers.vb`,
a weak-signal candidate pushed past the tail cap once the extra concepts
added ~75 concept hits (live: 103 → 179). Decision: expansion is opt-in
(`expand_concepts`), always reported; A2 stays (neutral).

Live (gated binary, 20:08): `node scans: 1`; advisory line
`entity candidates … redovisningskategori, production code list, code list category`
— the compound split reaches the code's vocabulary without touching
retrieval; concept 103 hits / 492 ms, co-change 87 / 6 178 ms (cold cache
this run), vector 21 / 699 ms.

What the gate says about the remaining work: precision (5 %, ~130
candidates) is the actual defect and neither more concepts nor re-tiering
moves it; it needs the weak-tier policy revisited against the
implementation-score A/B (in-session Opus/Sonnet agents via the Workflow
tool — requires the user's opt-in), not another retrieval knob.
