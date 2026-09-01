# External pre-audit — 2026-08-31 (REJECT "7 of 8 closed")

Source: auditor pre-audit delivered by the owner 2026-08-31, after r51 (c8a1b87) was
committed, pushed, built, deployed, and live-tested during the audit.

Final r51 results acknowledged by the auditor:
- Full sweep: 3,721 passed, 0 failed, 188 suites
- OciusX generation: 849; current paths 2,277 / 2,277 / 2,277
- Causal gate: 20/20; main golden: 34/35 — FAIL; reference-story ranks: 6/6
- Deployed binary matches the release binary

## Per-item verdict

| Item | Verdict | Ruling |
|---|---|---|
| 1. GC atomicity | Verified core; P1 residual | Original vector race fixed |
| 2. Per-store path integrity | **Wrong / reopened P0** | Graph and total vector loss can still be ignored |
| 3. Change-set ranking | Verified narrowly | Reference story is dramatically better |
| 4. Answer-correctness golden | **Wrong / reopened P0** | Gate still awards incorrect answers |
| 5. Pre-commit Ok(None) | Verified scoped fix | Runtime-construction fail-open remains |
| 6. No call-time Git walk | Partial | Warm unchanged HEAD avoids walk; changed HEAD still walks |
| 7. Dream default off | Verified | Correct implementation and live behavior |
| P1-3 immutable acceptance | Verified process | Run remains honestly 23/24, not green |
| 8. ImpactEngine/causal | Partial, not complete | Valuable route graph work, but no shared ImpactEngine and r51 golden still fails |

## Reopened P0-1 — Path-set health still has false-healthy states

`graph_paths` is collected (project_tools.rs:2302) but `missing`/`complete` use only
Tantivy and *optional* vectors (project_tools.rs:2343):

```rust
let vectors_present = vector_rows > 0 || !vectors.is_empty();
missing = !tantivy.contains(path) || (vectors_present && !vectors.contains(path));
complete = missing == 0 && cross_store_mismatch == 0;
```

Consequences:
1. Complete graph loss can still report healthy (graph_paths may be zero while Tantivy/vectors are complete).
2. Complete vector loss can report healthy (rows and paths both zero → `vectors_present=false` → vectors excluded).
3. LanceDB errors can report healthy — both vector calls swallow provider errors:
   `count_vectors_in_generation(...).await.unwrap_or(0)` (project_tools.rs:2257) and
   `vector_paths_in_generation(...).await.unwrap_or_default()` (project_tools.rs:2292).
4. Vector extras and graph extras are not included in `extra`.

The vector-loss test deletes only 3 of 10 rows (leaves `vectors_present=true`). No test for:
all vector rows lost; LanceDB read failure; graph-only loss; graph path-set mismatch.

**Required fix**: embeddings expected from configuration, not row presence; LanceDB
enumeration errors → DEGRADED, never an empty store; graph paths in `missing`, `extra`,
and cross-store equality; tests for all-vector-loss, graph-loss, provider-failure.

## Reopened P0-2 — The "answer-correctness" evaluator does not establish correctness

Proven by a live false positive. Golden question "Which TypeScript files under ts/map
render the camera icon on a marker?" was marked correct in the r50 evidence; live re-run
returned Google Maps typings, .coderabbit.yaml, imgHandler.ts, IETypeDefinitions.ts, a
checklist instruction document, and index-report memory — not the actual ts/map renderer.

It passed because: some unrelated evidence ended in `.ts`; the checklist text happened to
contain `ioMarkerInfowindow`; `required_all` searches every path and document body as one
blob (eval/ask_engine_golden.py:87). Precision is weak: relevance tokens include `.ts`/`.vb`
so every file of that extension counts as relevant (ask_engine_golden.py:110); only 7 of 35
rows specify `min_precision`. Allowed unsupported/ambiguous statuses return success before
evidence checks (ask_engine_golden.py:94).

**Required fix** — item-level predicates per expected fact:

```json
{ "required_items": [ { "path_suffix": "ioMarkerMoment.ts", "content_all": ["camera", "marker"] } ] }
```

Also require: item-level precision excluding extension tokens; file-set recall where the
question asks "which files"; exact symbol/route assertions; forbidden irrelevant evidence
classes; correct abstention labels rather than broadly permitting unsupported; held-out
rows not used during implementation.

## Item 8 audit

**The route-resolution work is real and accepted as implemented**: TS `api.ajax("name")`
extraction; VB `Select Case` dispatch metadata; route resolution (store.rs:3128); ambiguous
routes skipped; route edges connected to enclosing client functions; live OciusX 432 API
calls scanned / 313 resolved / 1 ambiguous / 1 unbound; four consecutive causal 20/20 runs;
r51 repeated 20/20 after a complete rebuild.

**But 20/20 does not mean complete causal answering.** ox_causal_20 ("Which server API
functions does ioMarkerInfowindow.ts call?") requires only two tokens; the source contains
at least 16 API calls (iopCheckIfRoqSupported, rvGetCount, iopUpdate, iopDelete,
iopGetProperties, iopGetLog, cfGetAvailableFormsAndSheetsByEntityId, iopGetCoordinates,
getImage, iopGetAvailableImages, …). The live accepted answer returned only two meaningful
routed functions plus noise. 20/20 proves token retrieval, not complete/correct causal answers.

**r51 did not fix ox_multi_4** (34/35, `api-images` not cited). The patch reserves cue slots
inside `callee_evidence`, but final global ranking still applies afterward
(ask_tools.rs:194); an item surviving the provider-local cap is not guaranteed to survive
the final evidence cap. The fixture proves provider-local behavior; OciusX proves the
end-to-end behavior still fails.

**This is not yet the shared ImpactEngine.** The traversal lives inside ask_codebase
(ask_tools.rs:73; providers.rs:610). impact_analysis, blast radius, and ask do not consume
a shared typed engine. No shared ImpactEngine, ChangeSpec, edge-policy matrix, common
traversal result, per-hop/per-edge coverage, SCC handling, or shared structured JSON
contract. Honest name: "TS/VB route graph enrichment plus ask-specific bounded callee
traversal."

## Remaining P1s

- Post-publish purge debt is best-effort: registry writes ignored (project_tools.rs:2089);
  the test manually creates purge_pending rather than inducing a real purge failure.
- ProductIntentGate and CoAddedFamilyGate silently return no findings when runtime
  construction fails (gates.rs:2832, gates.rs:3050).
- Co-change avoids walking only when cached HEAD and depth match; still opens Git
  (planning_tools.rs:584) and walks when HEAD changed (planning_tools.rs:607).
- Callee traversal silently skips graph/query failures; unreported caps (4 seeds, 200/60
  functions, 40 neighbours, 10 wrapper routes, 6 provider items).
- project_health prints tantivy_docs_total 421,293 vs lancedb_vectors 422,249 while
  declaring healthy — Tantivy "total" counts a hardcoded namespace subset;
  check_integrity correctly reported 422,249 for both. Label misleading.

## Final ruling — the honest state

Genuinely closed: the original GC vector race; the OciusX corpus collapse; reference-story
change-set ranking; missing-document handling; Dream default; immutable acceptance
procedure; a valuable TS→VB route-resolution capability.

The program is NOT at 7/8 closed. Remaining:
1. Fix health so every expected store — including graph and an entirely empty vector store — is mandatory.
2. Replace token-presence judging with item-level factual correctness and completeness.
3. Add a held-out causal suite.
4. Fix ox_multi_4 at the final ranking boundary, not only inside the callee provider.
5. Decide whether item 8 means "ask route enrichment" or the actual shared ImpactEngine. It cannot be called both.

## Disposition (to be filled as items close)

| # | Item | Status |
|---|------|--------|
| 1 | Path-set health (graph + empty-vector mandatory, errors → DEGRADED) | fixed@5b2bfee, live r53: Health OK with graph in the equality (2277/2277/2277/2277, missing 0, extra 0, mismatch 0); RED 3/4 tests failed pre-fix (all-vector-loss, LanceDB-failure via freshness, graph-loss) + fts_only guard; sweep 63 3,774/0/197 (evidence: verify_r53) |
| 2 | Item-level evaluator (required_items predicates, precision, recall, held-out) | judge v3 SHIPPED (eval/ask_engine_golden.py + eval/test_golden_judge.py self-test = the auditor's live false positive, RED on v2/GREEN on v3); corpus upgraded: 21 rows to item-level predicates + 2 allow_abstain (shas in --out records); HONEST BASELINE vs live r53: golden 14/35 (40%), causal 14/20 (70%) — evidence golden_v3_r53_baseline.json / causal_v3_r53_baseline.json; held-out rows still owed (item 3) |
| 3 | Held-out causal suite | AUTHORED 2026-08-31 (owner: held-out first): eval/data/ask_golden_holdout_ociusx.jsonl (11 rows) + ask_causal_holdout_ociusx.jsonl (5 rows) — fresh subsystems never used in implementation (fortnox sync, fbinstplan/fiber jobs, vehicle vehQuery, prGetSubProjects, permits, RoQ price list) with item-level predicates; git-ignored, sha16 7048b2ee6fc82c16 / 550241a9e2ef7662; FIRST RUN at the next release verify, never tuned against |
| 4 | ox_multi_4 at the final ranking boundary (owner: collect-and-rank, cycle 35) | fixed@1f0a2de, live r52: golden 35/35, causal 20/20, ranks 6/6 ×2 (evidence: golden_v2_r52, verify_r52) |
| 5 | Item-8 definition decision (ask enrichment vs shared ImpactEngine) | DECIDED 2026-08-31 |

Owner decisions (AskUserQuestion, 2026-08-31):
- **Program order = the auditor's order**: health P0 → evaluator P0 → held-out causal
  suite → P1 ledger.
- **Item 8 renamed honestly**: item 8 = *TS/VB route enrichment + ask-bounded causal
  hop* — closes once ox_multi_4 passes live AND a held-out causal suite exists. The
  shared ImpactEngine (ChangeSpec, edge-policy matrix, per-hop coverage, SCC handling,
  shared JSON contract consumed by ask/impact/blast) is its own queued follow-on
  program, per the blast-radius memory queue.
| P1a | Post-publish purge debt: registry writes + induced-failure test | fixed@2b6b545 + live r67 (2026-09-01 08:27): settle_purge_outcome seam — the record/clear registry write shapes the note and GC nudge, never `let _`; four induced-failure units (purge_settlement_tests, suite #199); live update_project reports `purge: ok` through the seam; sweep79 3801/0/199 |
| P1b | Gate runtime-construction fail-open (gates.rs:2832/3050) | fixed@0551373 + live r73: both sites degrade via ctx.degrade ("project runtime unavailable"); gate-level RED (GateContext direct, note None, runtime unbuildable) observed "degraded notes: []" first |
| P1c | Co-change changed-HEAD call-time walk | fixed@b0c568c + live r73: a changed HEAD serves the stale snapshot (head_advanced) and the caller starts a detached background refresh; RED observed "extended by a git walk (fresh diffs: 1)"; live co-change complete in 326 ms |
| P1d | Callee traversal error surfacing + cap reporting | subsumed@Phase B (b637565, live r69): ArmCoverage examined/available/truncated on every arm outcome incl. timed_out/failed; consumed by Phase D status |
| P1e | project_health tantivy_docs_total label | fixed@ac85a06 + live r73: the label prints count_docs (project-wide) — live tantivy_docs_total 172,145 == lancedb_vectors 172,145 (the 421,293-vs-422,249 subset gap class is dead); RED observed printed 1 vs held 3 |


## Grind log (doc-11 item 2 continuation — honest gates per release)

**r54 (cycle 36 padding suppression, commit 158062a; binary REBUILT on the settled
toolchain after the 15:28 build linked mid-VS-update and wedged at serve):**
Health OK (graph in the equality). Causal 13/20 — GATE FAIL vs the ≥14 floor:
six failures are the 0.10–0.30 item-precision boundary class (ox_causal_3 newly
flapped at 0.30) and ox_causal_16's imgHandler citation was evidently satisfied
by index_report padding before cycle 36 removed it (an exposure, not a quality
regression). Golden 15/35 — PASS (+1 vs the 14 baseline). HELD-OUT first run
(never tuned): golden 3/11, causal 0/5 [CORRECTED 2026-09-01: this line said 1/5; the committed holdout_causal_r54.json says 0/5 — caught by the round-3 audit] — failure classes: typings cited via
SEARCH arms (the cycle-36 filter covers hop+memory only; cycle-38 candidate),
over-ambiguity on fresh names, abstention miscalibration (hx_missing_1
ambiguous instead of unsupported), genuine retrieval misses. Ranks 5/6 ×2:
api-redovisning.vb pos=31 (one over top-30, cochange signal missing, candidates
159→163) — boundary jitter on a historically borderline file; P0-3 watch item.
Evidence: causal_r54.*, golden_v3_r54.json, holdout_*_r54.json, verify_r54.log.

**r55 (cycle 37 hit-centered snippets, commit3an):** numbers UNCHANGED vs r54 —
causal 13/20 (GATE FAIL, the same seven rows byte-identical: six precision-
boundary, one padding-exposure), golden 15/35 PASS, held-out 3/11. The snippet
window fixed VISIBILITY (the judged content now carries the matched region)
but the precision failures are evidence COMPOSITION (7 of 10 items are
wrong-file: typings/config among them) — cycle 38's search-arm exclusion is
the direct lever. OWNER 2026-08-31: floor HELD at 14; r54/r55 stay GATE FAIL
in this log; r56 lands next. Evidence: causal_r55.*, golden_v3_r55.json,
holdout_*_r55.json, verify_r55.log.

**r56 (cycle 38 search-arm declaration/config exclusion, commit 013ff11):**
FLOORS RESTORED — causal 14/20 GATE PASS (the typings removal lifted the flap
row), golden 15/35 GATE PASS, Health OK, ranks 6/6 ×2. Held-out unchanged
(3/11, 1/5). P0-3 watch-item CORRECTED: api-redovisning.vb never carried the
cochange signal — the candidate pool jitters 159–166 per reindex and the file
oscillates pos 26→31→32→27 around the top-30 line; a borderline-robustness
item, not a signal regression. Evidence: causal_r56.*, golden_v3_r56.json,
holdout_*_r56.json, verify_r56.log.

**r57 (cycle 39 system-section second ingress, commit be1d030):** golden
19/35 PASS (+4), causal 14/20 PASS, Health OK. The index_report leak is gone
from every arm; four rows cleared outright, the rest fell to their real next
layer: evidence DILUTION (exact_2/3/4/6, multi_4, rationale_2, compound_1,
bug_2 — 1–3 relevant of 10 items) and right-file-wrong-chunk retrieval
(usage_2/3/4/5, multi_1/2/3, bug_1). Held-out: hx_golden_2 now honestly
ABSTAINS with its padding gone. NEW OWNER CADENCE from here: batched fixes
per release; wipe+reindex only for index-affecting changes. Evidence:
causal_r57.*, golden_v3_r57.json, holdout_*_r57.json, verify_r57.log.

**r58 (batch 1: lookup-cap 2169d4f + corpus token widening; FIRST no-repair
verify under the new cadence):** FLAT — golden 19/35 PASS (identical row set),
causal 14/20 PASS, Health OK on the un-wiped index, ranks 6/6 ×2 (pool 174).
Honest diagnosis: the widened tokens TOOK (multi_4 and rationale_2 moved
0.10→0.30 — one relevant item short of 0.34), but lookup_cap NEVER ENGAGED
live — every exact row still carries 10 items, so the real plans are not
"one clear entity" the way the unit fixture models (multi-word mentions
resolve ambiguously or mint several mentions). Batch 2 corrects the
engagement condition from live plan evidence. The no-repair verify cut the
cycle by ~6 min and lost nothing.

**r59 (batch 2: long-stem minting + resolved-only lookup cap, commit c34d4b2):**
golden 21/35 PASS (net +2: exact_2/exact_4 cleared outright, cap-5 engaged on
exact_6/usage_3 — but multi_1 and bug_2 REGRESSED to Ambiguous: the minted
word "installation" (12 chars, ordinary English, exactly at the threshold)
resolved to four files and tripped the ambiguity verdict; held-out golden
dipped 3→2/11 the same way). Causal 14/20 PASS, Health OK, ranks 6/6 ×2
(pool 172). Batch 3: a speculative-SHAPED mention (bare lowercase, no
separators — recoverable from the text) may help when it resolves uniquely
but never drives Ambiguous; plus per-file-1 under the lookup cap
(exact_6 sits at 2/5 with a same-file pair). Evidence: causal_r59.*,
golden_v3_r59.json, holdout_*_r59.json, verify_r59.log.

**r60 (batch 3: speculative-mention ambiguity veto + lookup per-file-1,
commit 0f978d0):** FLAT — golden 21/35 PASS (+0/−0 row churn), causal 14/20
PASS, Health OK, ranks 6/6 ×2 (pool 172/174). Honest diagnosis: the veto
WORKED as designed — multi_1 moved ambiguous→unsupported and bug_2
ambiguous→partial; both now fail at their real next layer (retrieval /
required-cite). But retain_one_per_path CUT exact_6's measured precision
0.40→0.25: the same-file pair it deduped was the RELEVANT pair — the
bottleneck is irrelevant slot-fillers, not slot spending. Held-out +1
(hx_missing_2 cleared; 3/11 + 1/5); the five held-out ambiguous rows are NOT
speculative-shaped (their mentions carry separators/uppercase) and need a
live probe before any design. Batch-4 survivor classes: (a) required-cite
retrieval misses (bug_1/2, usage_2–5, multi_2/3, causal_20), (b) the
0.30 < 0.34 precision boundary (compound_1, multi_4, rationale_2,
causal_3/12/17/19), (c) lookup precision bars (exact_3 0.20, exact_6 0.25
vs 0.5). Evidence: causal_r60.*, golden_v3_r60.json, holdout_*_r60.json,
verify_r60.log.

**r61 (batch 4 a–e: where-defined lookup + term co-occurrence + anchored
slots + reserve-protection visibility, commit 16afb04):** golden 21/35 PASS
— flat on totals but the TARGETS LANDED: +exact_3 (the where-defined
question now engages the lookup cap and anchors its slots) and +usage_4
(the all-terms item outranks the single-term FK swarm). Two rows paid for
it: −exact_2 (0.40) and −exact_5 (0.20) — the co-occurrence terms list
feeds UNRESOLVED junk mentions ("data-access" resolves to []) into the ≥2
switch, handing the 0.9 directness boost to prose chunks that contain the
junk word. causal 14/20 PASS (causal_13 crept 0.25→0.33 — one filler
short of the bar), held-out unchanged (3/11 + 1/5), Health OK, ranks 6/6
×2 (pool 172). The landing itself surfaced a chain defect: the replay
guard crashed on fmt-wrapped detection strings AFTER stashing, and the
landing script kept going — recovered (stash restored, orphan build
killed), guard made per-hunk idempotent with post-fmt strings, relaunch
dry-run == swept. Batch 5: co-occurrence terms are RESOLVED-only (the
batch-2 lookup_cap lesson, applied to the ranker's term list). Evidence:
causal_r61.*, golden_v3_r61.json, holdout_*_r61.json, verify_r61.log.

**r62 (batch 5: resolved-only co-occurrence terms, commit 6a9754d):** golden
23/35 PASS — the grind's NEW HIGH (+exact_2 reverted as predicted, +multi_4
cleared as a bonus — its 0.30 boundary broke once junk terms stopped
stealing the boost; ZERO losses). exact_5 did not revert (0.20, 2/10 — its
own diagnosis pending). causal 14/20 PASS (identical row set; causal_13 at
0.33 vs 0.34 — one item), held-out unchanged (3/11 + 1/5), Health OK,
ranks 6/6 ×2 (pool 172). Survivor classes for batch 6: (a) required-cite
retrieval misses (bug_1/2, usage_2/3/5, multi_2/3, causal_20), (b) the
0.30/0.33 precision boundary (compound_1, rationale_2, causal_3/12/17/19,
causal_13), (c) lookup precision (exact_5 0.20, exact_6 0.25). Evidence:
causal_r62.*, golden_v3_r62.json, holdout_*_r62.json, verify_r62.log.

**r63 (batch 6: plural-caller cue + derived-state collapse, commit 6a69744):**
causal 15/20 PASS — the FIRST causal gain since the floor was set
(+causal_13: the callers arm now runs on "Which TypeScript files call X?").
golden 23/35 PASS (+exact_5 — the sym-over-state collapse engaged the
lookup cap — but multi_4 flip-flopped back to its 0.30 boundary: the
collapse reshuffled its resolutions and the 10-item breadth returned).
Held-out causal 2/5 (+hx_causal_2 — a genuine GENERALIZATION win, never
tuned against); held-out golden 3/11 unchanged. Health OK, ranks 6/6 ×2
(pool 172). Twelve golden survivors: the 0.30-boundary cluster (multi_4,
rationale_2, compound_1 — each 3/10, one relevant item short), required-cite
retrieval (usage_2/3/5, multi_2, bug_1/2), multi_3's cross-language .rdl
gap, exact_6 (0.25), multi_1 (unsupported). Evidence: causal_r63.*,
golden_v3_r63.json, holdout_*_r63.json, verify_r63.log.

**r64 (batch 7: question-named path scope, commit 11ce724):** FLAT — zero
row churn on all four suites (causal 15/20 PASS — the new floor HELD;
golden 23/35 PASS; held-out 3/11 + 2/5; ranks 6/6 ×2, pool 172). The scope
filter WORKED where it engaged: usage_5 moved answered-with-junk →
UNSUPPORTED — the marker_edit callee junk is gone, but the retrieval arms
never had ts/map files in their top-k, so only weak concept items survived
the scope and the engine honestly abstained. Filtering is not steering:
batch 8 threads the scope INTO the search arms (HybridQuery already
carries include_path_prefixes, unset) so the top-k comes from in-scope
documents. Evidence: causal_r64.*, golden_v3_r64.json, holdout_*_r64.json,
verify_r64.log.

**r65 (batch 8: scope-steered retrieval, commit 506df86):** golden 23/35
PASS, causal 14/20 PASS — the gate held but ox_causal_1 OSCILLATED out
(probe: the dispatch pair IS retrieved — athDeleteByID in caw.ts +
DeleteChangeRequest in api-atahuvud.vb — yet status calibrated Unsupported
at generation 887; the verify's route probes are byte-identical to r64, so
the graph is intact and batch 8's code path is inert with empty scopes:
generation-sensitive status oscillation, the P0-3-adjacent watch class).
The STEERING itself landed: usage_5's evidence is now ENTIRELY under
ts/map including the required ioMarkerInfowindow.ts — remaining gaps are
status calibration (code-only scoped evidence judged inadequate →
Unsupported) and the specific "camera" chunks not ranking. Held-out
3/11 + 2/5 unchanged, Health OK, ranks 6/6 ×2 (pool 174). Golden has been
flat at 23/35 for four releases — the survivors need status-calibration
or deep-retrieval designs; the priority fork goes to the owner. Evidence:
causal_r65.*, golden_v3_r65.json, holdout_*_r65.json, verify_r65.log.

**r66 (batch 9: status calibration — language and role vocabulary, commit
af39a0c, owner-approved):** the grind's STRONGEST release. golden 24/35
PASS — NEW HIGH (+multi_4, zero losses); causal 15/20 PASS (+causal_1
recovered — the "TypeScript"-as-uncovered-premise oscillation class is
dead); HELD-OUT GOLDEN 4/11 (+hx_golden_2 — a pure generalization win,
never tuned against). Both remaining batch-9 targets moved exactly as
designed: usage_5 unsupported→ANSWERED (fails only on the second required
file's camera chunk — retrieval), multi_1 unsupported→PARTIAL (fails only
on the required canuserbulkupdate cite — retrieval). Health OK, ranks
6/6 ×2 (pool 172). The status-calibration class is closed; every
remaining golden survivor is now a RETRIEVAL problem (cross-language
compounds, specific-chunk ranking, .rdl corroboration). Evidence:
causal_r66.*, golden_v3_r66.json, holdout_*_r66.json, verify_r66.log.

**r67 (P1a purge settlement, commit 2b6b545 — first landing under the doc-13
program era):** causal 15/20 PASS, golden 23/35 PASS (ox_multi_4 oscillated
back OUT — the documented 0.30-boundary jitter class; P1a touched no
ask-engine code), held-out 4/11 + 2/5 unchanged, Health OK, ranks 6/6 ×2
(pool 174). P1a LIVE EVIDENCE: `purge: ok` rendered through the
settle_purge_outcome seam in the real update report (verify_r67 08:27:35).
P1a row CLOSED above. Next: doc-13 Phase A (AnswerContract) — chain c50.
Evidence: causal_r67.*, golden_v3_r67.json, holdout_*_r67.json,
verify_r67.log.

**r68 (doc-13 Phase A: typed AnswerContract, commit 0e630f1):** PERFECTLY
FLAT — zero row churn on all four suites (causal 15/20 PASS, golden 23/35
PASS, held-out 4/11 + 2/5, ranks 6/6 ×2, pool 174, Health OK). Correct by
design: the contract ships WITHOUT consumers — five shape units assert it
against the round-3 audit's exact probes; Phases B–D make it load-bearing.
Evidence: causal_r68.*, golden_v3_r68.json, holdout_*_r68.json,
verify_r68.log.

**r69 (doc-13 Phase B: arm coverage on every outcome, commit b637565):**
PERFECTLY FLAT — zero row churn (causal 15/20 PASS, golden 23/35 PASS,
held-out 4/11 + 2/5, ranks 6/6 ×2, pool 174, Health OK). Correct by
design: coverage metadata (examined / available / truncated per arm, in
the outcome, the fold, the report struct and the JSON) changes no ranking
and no status — Phase D consumes it. The invisible-caps P1 now has its
data path. Evidence: causal_r69.*, golden_v3_r69.json, holdout_*_r69.json,
verify_r69.log.

**r70 (doc-13 Phase C: exhaustive named-file callee set, commit 19379d8):**
zero row churn (causal 15/20 PASS, golden 23/35 PASS, held-out 4/11 + 2/5,
ranks 6/6 ×2, pool 172/174, Health OK). The doc-12 P0-2 live probe
("Which server API functions does ioMarkerInfowindow.ts call?") now runs
the arm — callee_set(Hit,79), status Answered — but the REPORT cited only
5/15 routes. Root cause (refs probes + provider counts): the graph has
every route edge and the arm emitted them all; the ranker's MMR per-path
anti-anchoring cap (`per_file < 2`, round-2 P0-4e) collapsed the 12 routes
defined in api-installationsobjektprojekt.vb to exactly 2. Second defect:
r70's widened cap let 73 items through (concept 77) and cost ox_causal_20
its item precision (0.10). Fix C2 lands r71: an exempt-provider lane in
selection (set items are facts, not anchoring bias), a kinds-restricted
walk ("API" questions walk ApiCall only), and the cap reverts to the plain
lookup cap. Evidence: probe_r70_p02.txt, refs_*_diag.txt, causal_r70.*,
golden_v3_r70.json, verify_r70.log.

**r71 (doc-13 C2+D: exempt selection lane + contract-validated status,
commit 3524218):** the doc-12 P0-2 probe now cites the COMPLETE set —
ROUTE RECALL 15/15 vs the source-derived ground truth (audit: 1/15,
r70: 5/15) in a 23-item report (r70 flooded 73), status Answered through
Phase D's contract gate. causal 16/20 (+1: ox_causal_20 correct — the
ApiCall-only walk and the reverted cap healed its precision, 0.10 → pass);
golden 23/35 row-identical, held-out 4/11 + 2/5 unchanged, ranks 6/6 ×2,
pool 172, sweep73 3,813/0/199. Iterations recorded honestly: C2 test
literals (E0308 ×4), Phase-D legacy primary-kind tail (satisfied contract
now returns Answered), Script-modality test model, and the shared-test-file
commit partition (landed as ONE commit). Evidence: probe_r71_p02.txt,
causal_r71.*, golden_v2_r71.log, verify_r71.log.

**r72 (doc-13 Phase E: judge v4 exact-set, commit 86c7cfd, EVAL-ONLY —
r71's binary):** the doc-12 P0-3 construction is dead — the fabricated
two-filename report now FAILS with "set recall 0.00 < 1.0" naming the
missing routes (red_phaseE.py red observed on v3, green gated on v4).
ox_causal_20 carries the fifteen source-derived route identities
(min_recall 1.0) and STAYS correct under them; the misleading
"(gate = 100%)" banner is replaced by the per-row rubric + printed floor
(the GATE-PASS P1). Under v4: causal 16/20 PASS, golden 23/35 PASS (zero
rows lost to the tightening), held-out 4/11 + 2/5 report-only. The
identity-precision half of the doc-13 sketch rides the item-precision
token join (set names now count); a dedicated per-identity precision
metric remains open if a later row needs it. Evidence: causal_r72.*,
golden_v4_r72.json, land_r72.log.

**Phase F: suite sealed (commits 95b69da protocol + e479112 suite):**
ONE new sealed OciusX blind suite — ask_blind2_ociusx.jsonl, 8 rows
(two exhaustive callee sets: qtyManager.ts 12 routes,
installation_edit.ts 8; a 5-file callers set; where-defined; a callers
row; a 4-table query set; a served-by row; a fabricated-premise abstain
row) — every fact source-derived by grep from the OciusX working tree,
never run through the engine before sealing. Seal
sha256=a772866bedb8a014…, state BLIND; scoring goes through
eval/_seal_suite.py verify; first inspection retires it into the
dev/validation pool. The old held-out set is dev/validation and is never
again cited as blind. First blind scoring run (r71 binary, judge v4):
**6/8 correct** — the aggregate line only; per-row failures were captured
to disk unread, so the suite REMAINS BLIND.

**r73 (P1 slices, three commits: 0551373 P1b / b0c568c P1c / ac85a06 P1e;
sweep74 3,817/0/202):** all three landed with observed REDs and live
acceptance — health totals in exact cross-store equality (172,145 ==
172,145), co-change served warm in 326 ms, both fail-open gates degrade
at the seam. Suites flat as designed: causal 16/20 PASS, golden 23/35
PASS (row-identical), held-out 4/11 + 2/5 report-only, ranks 6/6 ×2
(174 candidates), r71 probe regression watch held (23 items, 15/15
routes, Answered). Iterations recorded: the runner's completeness
pre-check masks the site-level fail-open (RED moved to the gate seam);
family_keys drops single-segment parents; two reuse-contract tests now
poll the background refresh. Evidence: verify_r73.log, causal_r73.*,
golden_v2_r73.log.

**Phase G: the agent-level A/B ran (run wf_dbbb2206-fe2, 45 Sonnet
agents, 15 canonical PRs, fresh r73 substrate, 0 agent errors):** arm A =
get_change_set dossier; arm B = dossier + a live ask_codebase loop
(eval/ask_eval.py against each story's leak-free index; 42 asks made,
32 rated useful by the asking agents). All 46 frozen agent trees VERIFIED
read-only with intact mtimes before scoring. RESULT — implementation-
NEUTRAL at n=15: mean impl 3.0 vs 3.0, ask_delta exactly 0.00, wins
dossier 7 / ask_loop 6 / tie 2, per-story deltas −2..+2 (one ask-arm 5/5
merge-equivalent at PR1904; one −2 where the ask arm dropped a permission
gate the merged PR enforces). The trajectory replayed the historical
enrichment pattern (+1.0 at n=4 → 0.00 at n=11 → 0.00 final): early
promise, flat at power. NOTABLE: this is the FIRST enrichment beyond the
file-list dossier that is NOT net-negative (structural map −0.4,
change-pattern −0.27) — the honest ask engine confers confirmation value
(callers/tables/absence checks) without the anchoring cost, but the impl
ceiling remains agent reasoning, not retrieval. Doc-12 item 7 is
DISPOSITIONED with this measurement. Evidence:
eval/data/p2/_phase_g_verdicts.json, workflow journal wf_dbbb2206-fe2.
