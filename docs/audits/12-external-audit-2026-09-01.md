# External audit — 2026-09-01 (round 3, delivered by the owner; audited HEAD e468b43, deployed r66, live generation 889)

> Captured verbatim from the auditor's report. This document GOVERNS alongside the
> doc-11 disposition table once the owner sets the direction (AskUserQuestion sent
> 2026-09-01). The in-flight P1a chain (purge settlement, sweep 79) predates this
> audit and completes under the doc-11 P1 ledger.

Engram is measurably better on OciusX than it was at r51.

But ask_codebase is not yet trustworthy enough to act as the project's "brain." It is becoming a useful evidence finder, especially for exact symbols and known API routes, but it can still present incomplete evidence as a confident, complete answer. That is precisely the failure mode most likely to mislead an implementation agent.

Current practical rating:

- Index integrity: strong.
- Exact symbol/route questions: useful.
- Feature-planning support through get_change_set: still Engram's strongest proven capability.
- Broad natural-language questions: improving, but noisy.
- Exhaustive "which callers/files/functions?" questions: unsafe.
- Replacement for reading/searching the source: no.
- Advisory starting point: yes.

I audited committed HEAD e468b43, verified that the deployed r66 executable matches the release binary, and queried the live OciusX generation 889.

## Progress that is genuine

### 1. Index health: verified fixed

This is now a meaningful integrity invariant. Tantivy, vectors, and graph file paths must agree; an expected-but-empty vector store is corruption, graph loss is corruption, and provider errors produce DEGRADED, not false health.

The implementation is sound at crates/engram_server/src/handlers/project_tools.rs:2354, with destructive-store-loss tests at crates/engram_server/tests/index_integrity_pathset_tests.rs:177.

Live OciusX:

```
Health: OK
generation: 889
expected/tantivy/vectors/graph: 2277/2277/2277/2277
missing: 0
cross-store mismatch: 0
semantic_search: true
```

Verdict: verified and materially better.

### 2. Noise reduction and path-scoped retrieval: real improvement

The camera query previously returned typings, CodeRabbit configuration, and unrelated files. It now returns evidence entirely under Site/modules/dashboard/ts/map.

The following changes are useful and reasonably generic:

- hit-centred snippets;
- removal of Engram's own index-report padding;
- exclusion of declarations/review configuration from ordinary source answers;
- directory scope steering retrieval, not merely filtering afterwards;
- recognition that a .ts result satisfies a TypeScript requirement;
- deterministic evidence ordering;
- plural-caller recognition.

That is real progress, even though the resulting camera answer remains incomplete.

### 3. Exact graph-resolvable questions can be excellent

Live:

> Which VB function handles the vehQuery API and which TS client calls it?

Engram correctly returned:

- api.vehQuery in api-visualisering-vehicle.vb;
- vehicleManager.ts;
- vehicleMarkerInfowindow.ts;
- the compiled map.js caller.

Likewise, "Where is Check_pr_id defined and who calls it?" correctly found the definition and several real callers.

For this class of query, Engram is already useful.

### 4. get_change_set remains stable

The OciusX reference-story rank probes remain 6/6 across repeated r66 runs. That is still only one reference story, but it is the one capability with demonstrated agent-level implementation uplift in the existing A/B evaluation. The documented best dossier recipe improved implementation results to 3.2 versus 2.8 without Engram, with no losses in that run. See eval/README.md:221.

Verdict: useful and narrowly verified, but not broadly calibrated.

## The benchmark truth

The old 35/35 and 20/20 results should no longer be cited. The stronger v3 evaluator correctly exposed those as inflated.

Current figures:

| Suite | Honest baseline | r66 |
|---|---|---|
| Main golden | 14/35 | 24/35 |
| Causal | 14/20 | 15/20 |
| Held-out general | 3/11 | 4/11 |
| Held-out causal | 0/5 | 2/5 |
| Combined held-out | 3/16 | 6/16 |

The fairest primary-set comparison is actually 19/35→24/35, because the main corpus has had a stable SHA only since r58. The earlier 14→24 headline compares different corpus versions.

The combined held-out gain from 19% to 38% is modest but real. Absolute quality is still low.

Also, the held-out set is no longer truly blind. Its failures have been inspected after every release and were explicitly used to identify future fixes in docs/audits/11-external-preaudit-2026-08-31.md:165. It is now a validation/development set. A new sealed suite is required for credible generalization claims.

## New P0 findings

### P0-1 — "Answered" still does not mean the question was answered

Live camera query:

> Which TypeScript files under ts/map render the camera icon on a marker?

Engram reported:

```
status: Answered — direct, adequately-authoritative evidence found
```

But:

- it found only one of the two actual files;
- it missed ioMarkerMoment.ts;
- most displayed snippets did not establish camera rendering;
- it recommended opening ioMarker.ts, which was not the direct answer.

The correct camera implementations are visible in source at Site/modules/dashboard/ts/map/vsMap/iomarker/ioMarkerInfowindow.ts:572 and Site/modules/dashboard/ts/map/vsMap/iomarker/ioMarkerMoment.ts:249 (OciusX working tree).

Why this happens:

- a resolved entity appearing anywhere is considered adequate support at crates/engram_server/src/services/ask_engine/status.rs:264;
- Answered then requires only some evidence of the generic primary kind at crates/engram_server/src/services/ask_engine/status.rs:424;
- there is no question-specific answer contract or completeness requirement.

This contradicts the advertised "HONEST status" at crates/engram_server/src/tools.rs:300.

### P0-2 — Set-valued causal questions are catastrophically incomplete

Live:

> Which server API functions does ioMarkerInfowindow.ts call?

Engram returned one valid callee—cfGetAvailableFormsAndSheetsByEntityId—plus unrelated evidence.

The source contains fifteen named AJAX routes and a getImage wrapper call, beginning at Site/modules/dashboard/ts/map/vsMap/iomarker/ioMarkerInfowindow.ts:233 (OciusX working tree).

The implementation structurally cannot provide a complete answer:

- the planner leaves this as generic Explain, despite its own comment acknowledging callee questions at crates/engram_server/src/services/ask_engine/planner.rs:115;
- the handler requests only three callee items at crates/engram_server/src/handlers/ask_tools.rs:165;
- named files raise that to only six;
- callees are deduplicated to one item per implementation file at crates/engram_server/src/services/ask_engine/providers.rs:742.

Many OciusX API functions live in the same VB file. "One item per file" therefore destroys exactly the function-level cardinality the question requests.

### P0-3 — The causal evaluator still accepts factually empty answers

The v3 judge successfully fixes cross-item token laundering for the camera case. Its self-test is green.

However, ox_causal_20 requires only these names:

```
["ioMarkerInfowindow", "api-installationsobjektprojekt"]
```

It does not require any of the fifteen API function names. See the row at eval/data/ask_causal_ociusx.jsonl:20.

I constructed an evidence report containing only:

- the TS filename;
- the VB filename;
- the text "no API functions listed."

The current judge returned:

```
(True, "")
```

So even v3 can award this question without answering it.

The evaluator logic is better, but its ground truth still lacks question-shaped semantics: exact set recall, function identities, route-to-implementation mapping, and coverage.

### P0-4 — Evaluation inputs are not reproducible

All four OciusX corpora are under the ignored eval/data/ directory at .gitignore:31. The committed evidence records their hashes, but a fresh checkout cannot reproduce the evaluations or inspect historical expectation changes.

The main corpus changed SHA twice during the reported 14→24 progression.

For a quality program this important, corpora, schemas, expected facts, and revisions must be immutable and version-controlled. Secrets and generated responses can remain ignored; golden inputs cannot.

## Important P1s

- Caller/callee caps are invisible. Check_pr_id appears in roughly 40 source files, but the answer shows seven callers and says Answered, without saying the provider stopped at 25 and the final evidence cap removed more. Coverage reporting only handles failed/empty modalities, not truncation, at crates/engram_server/src/services/ask_engine/report.rs:31.
- collapse_derived_resolutions is too broad. If any symbol resolution exists, it removes every non-symbol resolution—not merely proven derived-state twins. That can silently choose a function over a legitimate table/file ambiguity.
- Globally excluding .d.ts and .coderabbit.yaml creates false negatives when the user explicitly asks about declarations or review configuration.
- The displayed GATE PASS is a no-regression floor, not the printed 100% correctness gate. The evaluator substitutes min_correct at eval/ask_engine_golden.py:310. Calling 24/35 an acceptance pass is misleading.
- The documentation says held-out causal started at 1/5; the committed r54 result and verification log say 0/5.
- The previously identified pre-commit runtime fail-open paths remain at crates/engram_server/src/services/pre_commit_review_service/gates.rs:2832 and crates/engram_server/src/services/pre_commit_review_service/gates.rs:3050.
- The shared ImpactEngine, ChangeSpec, policy matrix, SCC handling, and shared coverage model still do not exist. The team now acknowledges that honestly rather than calling the ask-specific hop an ImpactEngine.

## What I would do next

Stop the heuristic "golden grind" temporarily. The remaining problem is architectural.

1. Introduce a typed AnswerContract:
   - direction: callers versus callees;
   - requested entity type: function/file/table/route;
   - cardinality: one, top-K, or exhaustive set;
   - required facets: definition, caller, implementation, rationale;
   - allowed evidence classes;
   - completeness requirement.

2. Make every provider return:

```
items
examined_count
available_count
truncated
missing_or_dangling_count
errors
policy_used
```

3. For a named-file callee question, bypass semantic ranking initially:

```
named file
  → contained functions + file-level calls
  → every direct ApiCall edge
  → route name
  → broker dispatch
  → implementation
```

Group the result by route. Do not deduplicate functions merely because they share a file.

4. Compute status after validating the answer contract:
   - Answered: all required facets satisfied and exhaustive traversal complete;
   - Partial: valid evidence, but capped, unresolved, or incomplete;
   - Unsupported: no adequate evidence;
   - Failed: provider/integrity failure.

5. Replace token predicates with exact factual sets for callers/callees and "which files" questions. Score precision and recall over identities, not over incidental strings.

6. Freeze a new blind OciusX suite and add at least one unrelated large project. Once its results have been examined, retire it into the development suite and rotate a new blind set.

7. Run agent-level implementation A/Bs. Retrieval correctness is only a proxy; the actual objective is fewer wrong files, missed conventions, regressions, and incomplete implementations.

## Owner-supplied additional corpora candidates

Other legacy-like projects on this dev machine, offered for blind suites:

- `C:\Users\Dennis\Desktop\EyeImage_old`
- `C:\Optitec RS`
