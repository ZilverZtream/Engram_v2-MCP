# AnswerContract program plan (doc 13) — owner-approved 2026-09-01

Governs after the in-flight doc-11 P1a lands. Source: the round-3 audit
(doc 12, "What I would do next", owner-selected). The heuristic golden
grind is STOPPED; the remaining problem is architectural. Discipline per
slice is unchanged: RED → GREEN → sweep → land → live → disposition →
memory; chain template v2; batched landings.

## Phase A — the typed AnswerContract (doc 12 item 1)

`plan.rs` gains `AnswerContract`, derived by the planner beside intents:

- `direction`: Callers | Callees | None
- `entity_type`: Function | File | Table | Route | Any
- `cardinality`: One | TopK(n) | ExhaustiveSet
- `required_facets`: Definition | Caller | Implementation | Rationale (set)
- `allowed_evidence`: evidence-class allowlist
- `completeness_required`: bool (ExhaustiveSet implies true)

Planner rules are SHAPE-generic ("Which … does X call?" → Callees +
ExhaustiveSet; "under <dir> … which files" → File set; "where is X
defined" → One + Definition facet). Unit REDs per shape; the plan
cleanliness guards keep their exact asserts.

## Phase B — provider coverage metadata (doc 12 item 2; subsumes doc-11 P1d and the invisible-caps P1)

`ProviderOutcome` grows: `examined_count`, `available_count`, `truncated`,
`missing_or_dangling_count`, `errors`, `policy_used`. Every arm fills them
(search arms: top_k vs total hits; graph arms: edges walked vs present;
callee hop: caps hit). `report.rs` renders truncation per provider —
"showing 7 of ~40 callers (provider cap 25, evidence cap 10)" — instead of
only failed/empty modalities.

## Phase C — named-file callee traversal (doc 12 item 3; the P0-2 fix)

For `direction=Callees, cardinality=ExhaustiveSet` on a named file, bypass
semantic ranking first: named file → contained functions + file-level
calls → EVERY direct ApiCall edge → route name → broker dispatch →
implementation. Group by route. NO one-per-file dedup — function-level
cardinality is the answer. The evidence cap does not truncate an
exhaustive contract; the report lists the full set (grouped, compact).
Live acceptance: the doc-12 probe ("Which server API functions does
ioMarkerInfowindow.ts call?") returns the full route set (15 + getImage
wrapper) with per-route implementations.

## Phase D — contract-validated status (doc 12 item 4; the P0-1 fix)

`assess_status` takes the contract's validation result:

- Answered: all required facets satisfied AND (if completeness_required)
  the traversal reported `truncated=false` everywhere it walked;
- Partial: valid evidence but capped/unresolved/incomplete;
- Unsupported / Failed: unchanged semantics.

The camera-class lie ("Answered" on 1-of-2 files) becomes Partial with the
uncovered facet named. status.rs:264's resolved-entity-anywhere adequacy
is subordinated to the contract.

## Phase E — exact-set judges (doc 12 item 5; the P0-3 fix)

Judge v4: rows may declare `expected_set` (identities: function names,
files, routes) with `scoring: {precision, recall}` over IDENTITIES, not
token predicates. ox_causal_20-class rows get the full 15-route set as
ground truth (source-verified). The misleading "gate = 100%" banner is
replaced by the truthful printed gate ("no-regression floor ≥ N" when
min_correct substitutes) — the auditor's GATE-PASS P1.

## Phase F — sealed blind suite protocol (doc 12 item 6, owner-scoped)

One NEW sealed OciusX blind suite (owner declined extra projects for now):
authored from source ground truth, sha-committed ENCRYPTED-or-content-only
at authoring time, never inspected until a scoring milestone; on first
inspection it retires into the dev/validation pool and a fresh set is
sealed. The current held-out set is a dev/validation set and is never
again cited as blind.

## Phase G — agent-level A/Bs (doc 12 item 7)

After C–E land: re-run the eval/README dossier A/B recipe with ask_codebase
in the loop; success metric = implementation-level (wrong files, missed
conventions, regressions), not retrieval proxies.

## Folded-in doc-11 P1 remainders

- P1b gate fail-open (gates.rs:2832/3050) — standalone slice, unchanged.
- P1c co-change changed-HEAD walk — standalone slice, unchanged.
- P1d callee error surfacing — SUBSUMED by Phase B.
- P1e health tantivy_docs_total label — standalone small slice.
- collapse_derived_resolutions too broad (doc 12 P1) — Phase A/D adjunct:
  collapse only proven derived twins (state: sharing the symbol's file).
- .d.ts/.coderabbit global exclusion (doc 12 P1) — contract's
  allowed_evidence lifts the exclusion when the question names
  declarations/review config.

Order: A → B → C → D → E (each its own RED/GREEN/landing batch; A+B may
share a landing), then F, then G, with P1b/P1c/P1e slotted between
landings as small slices.
