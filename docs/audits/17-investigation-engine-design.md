# Root cause of the 0.00 and the investigation-engine design (2026-09-02)

Owner directive: no more retrieval tuning. Analyze the 42 Phase-G asks by
answer correctness, novelty, decision effect and outcome; design a
task-scoped, proof-carrying investigation engine that VERIFIES an
implementation plan; propose typed tools integrated with the get_change_set
dossier and task ledger; define one falsifiable vertical slice. Nothing is
built until this design and its experiment are approved.

## Part 1 — what the 42 asks actually did

Dataset: scratchpad `asks_dataset.json` (42 asks, in-transcript answers,
15 judged outcomes). Taxonomy by decision effect:

**Class 1 — absence proofs (≈11 asks; the winning class).** "Is there an
existing clone helper?" (no → build new, PR won +1); "any tests covering
aktivitet.vb's GetAllByFilter?" (no → scope decision, +2); "does
fl_filelibrary store image dimensions?" (no); "other callers of
CheckIfProjectHasNonSpatialLayer?" (one, confirmed, +1). The engine's honest
empty/Unsupported answers, read as *absence evidence*, changed scope
decisions. Every ask_loop win contains at least one.

**Class 2 — contract enumerations (≈9 asks; the corrective class).** The
columns of `planner_ak_rs` with FKs (+2 story — the one answer that CORRECTED
a plan); the complete set of compiled bundles embedding qtyManager.ts; the
full caller set of a function. When complete, these won; when partial
("returned only the three files I already knew"), they added nothing.

**Class 3 — history/provenance (≈8 asks; the failing class).** "Which files
changed together in that commit", "full file list of PR-1796" — index misses
or absent history in the leak-free snapshot. Agents recovered by reading git
themselves. Near-zero value as shipped.

**Class 4 — flow questions (≈6 asks; noisy).** "How does the marker import
flow duplicate rows", "when a page calls window.open…" — answers noisy or
tangential; agents answered them by reading code.

**Class 5 — confirmations (the remainder).** Correct answers the agent
already believed. 32/42 asks were rated "useful" — but usefulness was mostly
*confirmation*, which is why the aggregate delta is 0.00: confirmations
don't change implementations.

**The decisive negative finding.** The LOSING stories' asks were fine; the
losses came from decisions nobody asked about. PR 1890 (−1): the ask-arm
proposal dropped the permission/project-access gate the merged service
enforces — no ask touched authorization. PR 13 (−2): wrong mechanism choice;
the asks probed callers and history, not the mechanism convention. **A Q&A
engine leaves plan verification to agent initiative, and agents do not know
what they don't know.** The unasked question — "what contract does this
surface impose?" — is exactly what a verifier asks unconditionally.

## Part 2 — the typed architecture (proposal)

Shared foundations (already landed or in flight): `AnswerMember`,
`CoverageProof` (r75), the get_change_set dossier, the graph
(nodes/edges/co-change/history), the task ledger.

Core type:

```rust
/// A claim with its proof. NOTHING is reported without one.
struct ProvenClaim {
    claim: ClaimKind,            // Present{members} | Absent | Convention{rule, holds}
    members: Vec<AnswerMember>,  // typed identities, never prose
    proof: CoverageProof,        // complete() gates every "exhaustive"/"absent" claim
    scope: TaskScope,            // the files/surfaces of THIS task, from dossier+ledger
}
```

### Tool 1 — `enumerate_project_contract`
Input: a surface selector (table | function | file | route | control |
convention-class) + task scope. Output: the COMPLETE membership of that
surface with proof — columns+FKs of a table; all callers of a symbol; all
bundles embedding a source; the resx family of a label class; and
*convention contracts* mined from the surface's siblings (e.g. "12/12
api-v2 controller actions carry Authorize + project-access check" — a
statistical contract with counts, not a guess). Productizes Classes 1–2;
an Absent claim is only utterable with `proof.complete()`.

### Tool 2 — `trace_project_flow`
Input: an entry point and a direction. Output: the hop-by-hop path
(handler → service → domain → SQL / TS event → api call → broker →
implementation) as typed edges with per-hop proof, terminating at sinks or
at an explicit `unknown` hop that forbids completeness. Replaces Class 4's
noisy prose; reuses the route-resolution graph work (the 15/15 machinery)
generalized beyond callees.

### Tool 3 — `verify_implementation_plan` (the product)
Input: a PLAN — `{task_ref, files: [{path, action, intent}], claims: [...]}`
(the shape agents already emit in Phase G) + the dossier + task ledger.
Output: a typed verdict list, each entry a ProvenClaim-backed finding:

- **MissingCompanion**: dossier/family/co-change layers the plan omits
  (resx family, compiled bundle, SQL migration, designer file) — with the
  evidence that makes them companions.
- **ConventionViolation**: the plan touches a surface whose enumerated
  convention contract the plan does not satisfy (PR 1890's missing
  permission gate is caught HERE, unconditionally, no ask needed).
- **StaleAssumption**: a plan claim contradicted by an enumeration (a
  "sole caller" that isn't; an "unused" setting that is read).
- **UnverifiedScope**: plan surfaces where enumeration was incomplete —
  fail-closed honesty instead of silence.

Integration: get_change_set gains `verify` mode (dossier in, plan in,
verdicts out); the task ledger records plan→verdicts→revisions so
completeness is checked against the TASK, not a question.

## Part 3 — the vertical slice (one, falsifiable)

**Slice: `verify_implementation_plan` v0 with exactly two verdict kinds —
MissingCompanion and ConventionViolation — scoped to the auth-convention
contract (api-json/api-v2 handler surfaces) and the existing companion
machinery (resx family, compiled bundles, SQL, designer).** No new graph
capabilities; it composes the dossier, family expansion, and one
convention miner.

### Acceptance test (two gates, agent-level, pre-registered)

- **Gate 1 (replay, no new agents, cheap):** apply the verifier to the 15
  stored Phase-G LOSING-arm proposals. Success = it flags the judge-named
  primary defect in **≥ 8/15** stories (PR 1890's auth gap and the
  resx/bundle omissions are in-scope kinds) with **≤ 5 findings per plan**
  (noise bound — the enriched-dossier history shows flooding anchors
  agents). Both numbers fixed now; misses are misses.
- **Gate 2 (agent A/B, only if Gate 1 passes):** re-run the 15-story A/B —
  arm A dossier-only vs arm B dossier + one mandatory verify pass on the
  draft plan (revise once on findings). Arm-blinded labels, committed
  manifests (doc-16 D7). Success = **mean impl delta ≥ +0.4 and no story
  regresses by more than 1**. If Gate 2 fails, the engine thesis is wrong
  as designed and we stop and rethink with the data.

## Approval gate

Nothing in Part 2–3 is implemented until the owner approves this document
(or edits it). Open questions for the owner: (a) is the two-kind slice the
right first cut, or swap ConventionViolation for StaleAssumption? (b) Gate-1
threshold 8/15 — accept? (c) should `verify` live inside get_change_set or
as a standalone tool from day one?
