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
| P1a | Post-publish purge debt: registry writes + induced-failure test | OPEN |
| P1b | Gate runtime-construction fail-open (gates.rs:2832/3050) | OPEN |
| P1c | Co-change changed-HEAD call-time walk | OPEN |
| P1d | Callee traversal error surfacing + cap reporting | OPEN |
| P1e | project_health tantivy_docs_total label | OPEN |


## Grind log (doc-11 item 2 continuation — honest gates per release)

**r54 (cycle 36 padding suppression, commit 158062a; binary REBUILT on the settled
toolchain after the 15:28 build linked mid-VS-update and wedged at serve):**
Health OK (graph in the equality). Causal 13/20 — GATE FAIL vs the ≥14 floor:
six failures are the 0.10–0.30 item-precision boundary class (ox_causal_3 newly
flapped at 0.30) and ox_causal_16's imgHandler citation was evidently satisfied
by index_report padding before cycle 36 removed it (an exposure, not a quality
regression). Golden 15/35 — PASS (+1 vs the 14 baseline). HELD-OUT first run
(never tuned): golden 3/11, causal 1/5 — failure classes: typings cited via
SEARCH arms (the cycle-36 filter covers hop+memory only; cycle-38 candidate),
over-ambiguity on fresh names, abstention miscalibration (hx_missing_1
ambiguous instead of unsupported), genuine retrieval misses. Ranks 5/6 ×2:
api-redovisning.vb pos=31 (one over top-30, cochange signal missing, candidates
159→163) — boundary jitter on a historically borderline file; P0-3 watch item.
Evidence: causal_r54.*, golden_v3_r54.json, holdout_*_r54.json, verify_r54.log.
