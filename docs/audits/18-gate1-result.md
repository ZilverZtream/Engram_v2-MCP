# Gate 1 result — verify_implementation_plan v0 (2026-09-03)

Pre-registered bar (doc 17): the verifier flags the judge-named primary
defect in >= 8/15 losing-arm stories, with 0 plans over the 5-finding budget.

## Result: FAIL

```
hits = 6/15   (bar >= 8)
over_budget = 0/15   (bar 0)  ✓
proof.complete() = true on all 15  ✓
```

Run: `examples/gate1_plan_verify` against the production OciusX graph, the 15
Phase-G losing-arm plans with their actual proposed files and change text.
The 5-finding budget and the CoverageProof both held; the failure is
precision — the verifier did not name the judge's defect often enough.

## Why it failed (two honest causes)

1. **Hub noise ate the budget.** `App_Code/api-json/api-broker.vb` — a
   central dispatcher every handler co-changes with — was emitted as a
   MissingCompanion in almost every plan, often filling 4–5 of the 5 slots.
   It is a graph hub, not a task companion; the co-change signal is real but
   meaningless. This crowded out the genuine companions (resx families,
   compiled bundles) that would have been hits. A hub demotion (degree-based
   or IDF-style) is the obvious fix, BUT — see below — it is not claimed to
   reach 8.

2. **Several misses are out of the slice's scope, structurally.** The most
   common real defect in the losing arms was *wrong family*, not *missing
   family*: the agent touched `text.*.resx` when the correct family was
   `label.*.resx` (PR 1893), or chose the wrong mechanism entirely (PR 1967,
   imp1: a permission-tracing misdiagnosis). The v0 verifier suggests
   siblings of what the plan DID touch; it cannot know the plan touched the
   wrong surface. No amount of hub-demotion catches these — they need a
   different capability (contract/flow tracing, doc-17 tools 1–2).

## What worked

- The honesty layer: every verification carried a complete proof; nothing
  was claimed verified that could not be enumerated.
- The budget: 0/15 plans exceeded 5 findings — the anti-flooding bound the
  Phase-G enrichment history demanded held under real data.
- The convention arm fired correctly where it applied (PR 1937: a genuine
  ConventionViolation matching the judge's permission complaint).

## The honest read

A 2-kind verifier (MissingCompanion + ConventionViolation) is not enough to
move the needle on these 15 stories, because the dominant real defect is
*wrong choice*, not *missing companion*. That is not a tuning miss; it is
the wrong tool for the modal failure. Hub-demotion might lift 6→7 or 8, but
even a perfect companion detector leaves the wrong-family and wrong-mechanism
losses untouched.

Per the pre-registration, the program STOPS here for an owner decision. I am
NOT auto-fixing the hub noise and re-running to manufacture a pass — that is
the teaching-to-the-test pattern this project already rejected.

## Options for the owner

- **A. Accept the stop.** The 2-kind slice is falsified as a needle-mover;
  do not build the tool. Redirect to what measurably works (get_change_set
  quality; the +0.80 came from there).
- **B. One bounded rerun.** Authorize a single hub-demotion fix (degree cap
  on co-change companions) and re-run Gate 1 once, accepting in advance that
  it may still fail and that structural misses remain out of scope.
- **C. Re-scope the slice.** The analysis says the modal defect is
  wrong-choice. Redefine v0 around *StaleAssumption* (flag a plan claim the
  graph contradicts — "sole caller" that isn't, "unused" that's read) or a
  convention-contract check that catches wrong-family, before any A/B.

## Owner decision (2026-09-03): A — accept the stop

The 2-kind verifier is falsified as a needle-mover and will not be built out.
`verify_implementation_plan` v0 (plan_verify.rs), its tests, and the Gate-1
runner are KEPT in-tree as the recorded experiment and its evidence — not
wired into any tool, not on any request path. Effort redirects to
get_change_set, the one component with a measured agent-quality lift (+0.80
impl at n=15). No further ask_codebase or verify hardening.
