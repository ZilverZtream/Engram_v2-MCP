# External audit — round 4 (2026-09-02)

Captured verbatim. Snapshot: committed HEAD 8630b6b plus six actively edited
files (retrieval batch 1, sweep in flight — NOT credited as tested/deployed).

## Verdict

Yes, they are making real progress. ask_codebase is materially better for one
important OciusX capability: the canonical TS → API route question improved
from 1/15 to 15/15.

But the "AnswerContract program is complete" claim is rejected. It is
currently a narrow, phrase-sensitive implementation that can still return
Answered for demonstrably incomplete answers. The agent-level evaluation
shows no implementation-quality uplift yet: 3.0 versus 3.0 across 15 stories.

## Per-claim verdict

| Claim | Verdict | Finding |
|---|---|---|
| Phase A: typed AnswerContract | Partial | The struct exists, but entity_type and allowed_evidence are effectively write-only. Contract recognition covers a few literal English forms. |
| Phase B: exact provider coverage | Wrong | Only 3 of the promised 6 fields exist. Many providers report default/unmeasured coverage. Truncation is not rendered in Markdown. The exhaustive provider reports false completeness. |
| Phase C: named-file exhaustive callees | Partial | Verified 15/15 for the exact "Which … does X call?" prompt. Equivalent wording bypasses the lane, basename ambiguity is mishandled, and hard caps remain. |
| Phase D: contract-validated status | Partial | It works only for Callees. It trusts false provider coverage. Other exhaustive shapes are always Partial; facet validation is weak or unconditional. |
| Phase E: exact-set judge | Partial | It catches the earlier two-filename fabrication, but it still judges substring tokens in prose—not typed identities. A fabricated single evidence item can satisfy the entire expected set. |
| Phase F: sealed blind suite | Wrong | The suite is committed as readable plaintext. The seal verifies only file hash and a manually maintained flag; scoring does not automatically retire it. |
| Phase G: agent A/B | Verified, but neutral | The stored arithmetic is 3.0 vs 3.0, 7–6–2, delta 0.00. This demonstrates no coding-quality gain from ask_codebase yet. |
| P1b gate failure degradation | Verified | Both runtime-construction failures now call ctx.degrade. |
| P1c no changed-HEAD request walk | Verified in code | Changed HEAD serves stale data and schedules refresh. |
| P1e project-wide health total | Verified in code | count_docs(project_id) now supplies the total. |
| Derived-resolution adjunct | Not done | The broad "if any sym: exists, discard every non-symbol" behavior remains. |
| Declaration/review-config evidence adjunct | Not done | .d.ts, typings, and .coderabbit.yaml remain globally filtered; allowed_evidence does not lift the exclusion. |

## New/reopened P0s

### P0-1 — Semantically equivalent questions receive radically different answers

The planner only recognizes callees when the question:

- starts with which, what, or who;
- contains does or do;
- contains the literal verb call.

See crates/engram_server/src/services/ask_engine/planner.rs:299 and the
exhaustive-lane gate in crates/engram_server/src/handlers/ask_tools.rs:209.

Live OciusX results:

| Question | Result |
|---|---|
| "Which server API functions does ioMarkerInfowindow.ts call?" | Answered, 15/15 |
| "List every server API function called by ioMarkerInfowindow.ts." | Answered, only 1 relevant API |
| "What APIs are invoked from ioMarkerInfowindow.ts?" | Answered, only 1 relevant API plus junk |
| "Which APIs does ioMarkerInfowindow.ts invoke?" | Answered, only 1 relevant API |

This is the most serious current problem. A brain cannot be correct only when
an agent happens to use the golden prompt template.

Required fix: compile questions into a typed relation query—subject, relation,
direction, target type, cardinality and quantifier. Normalize call, invoke,
request, use, depend on, passive voice, "list", "all", and "every" before
execution. Add paraphrase/metamorphic tests asserting identical answer members
and completeness status.

### P0-2 — "Exhaustive" coverage is fabricated

The exhaustive provider:

- fetches at most 500 contained functions;
- fetches at most 500 neighbours per function/kind;
- silently skips graph failures;
- silently skips dangling target nodes;
- silently limits dispatch implementations to two;
- finally returns available = walked and truncated = false.

See crates/engram_server/src/services/ask_engine/providers.rs:841 through
crates/engram_server/src/services/ask_engine/providers.rs:915.

Status then treats any nonempty callee_set with truncated=false as complete
and returns Answered: crates/engram_server/src/services/ask_engine/status.rs:443.

Worse, the test explicitly asserts that "an exhaustive walk never truncates,"
even though the implementation contains hard caps:
crates/engram_server/tests/ask_lookup_cap_tests.rs:509.

Required fix: a typed CoverageProof containing discovered/processed sources,
edges available/emitted, per-kind cap state, dangling count, errors and
policy. Use cap+1 or exact counts. Any unknown, error, dangling endpoint, or
cap hit must prevent complete Answered.

### P0-3 — The "exact-set" judge is still gameable

The judge finds expected identities by substring-searching each evidence
item's path/content: eval/ask_engine_golden.py:155.

I constructed one fake .vb evidence item containing the two required context
tokens and all 15 API names in its prose. Judge v4 returned:

(True, '')

It does not prove 15 distinct returned identities, does not distinguish
subject from target, and does not calculate unexpected identity members. The
claim "precision/recall over identities, not token predicates" is false.

Required fix: ask_codebase must return structured answer_members, for example:

```json
{
  "target_node_id": "...",
  "display_name": "iopGetProperties",
  "relation": "api_call",
  "source_node_id": "...",
  "path": "...",
  "coverage": "complete"
}
```

The evaluator should compare normalized sets of these identities—not search
snippets.

## New/reopened P1s

### Basename ambiguity still breaks exhaustive queries

qtyManager.ts resolves to two OciusX files. The handler silently executes
only named.first(): crates/engram_server/src/handlers/ask_tools.rs:220.

Live result:

- qtyManager.ts→2
- status Partial, not Ambiguous
- exhaustive provider Empty
- blind expected set recall 0/12

The ambiguity logic groups by canonical name, while file canonicals are just
node names/basenames, so two different paths named qtyManager.ts collapse
conceptually: crates/engram_server/src/services/ask_engine/resolver.rs:28,
crates/engram_server/src/services/ask_engine/status.rs:398.

File identity must be path/node ID, not basename. Either execute and label
every branch or return a real ambiguity requiring path qualification.

### The contract remains partly ornamental

Repo-wide inspection found no consumers of allowed_evidence, and entity_type
does not constrain returned members. Facet validation is also unsafe:

- any graph relation satisfies Caller or Implementation;
- Rationale is unconditionally satisfied.

See crates/engram_server/src/services/ask_engine/status.rs:470.

### Coverage is invisible to normal agent users

Markdown renders providers only as provider(Status,count):
crates/engram_server/src/services/ask_engine/report.rs:253.

coverage_gaps reports failures/timeouts but not provider truncation:
crates/engram_server/src/services/ask_engine/report.rs:31.

Thus even the limited coverage metadata available in JSON is absent from the
default agent-facing response.

### Two promised adjuncts were forgotten

The overly broad derived collapse remains unchanged at
crates/engram_server/src/services/ask_engine/resolver.rs:74.

Global source exclusions remain at
crates/engram_server/src/services/ask_engine/providers.rs:255. The promised
contract-specific override was never implemented.

### Evaluation evidence is not durable

The Phase G verdict file lives under ignored eval/data/: .gitignore:29. The
committed audit evidence directory stops at r69, while documents claim live
r71–r73 evidence.

The Phase G judge is also shown labeled dossier and ask_loop proposals, so
evaluation is not arm-blinded: eval/phase_g_workflow.js:111.

Commit the immutable run manifest, prompts, outputs, snapshot hashes and
verdicts. Randomize opaque arm labels and use repeated or independent judges.

## Current uncommitted work

The developers are sensibly investigating the 16 remaining golden/causal
failures, but I would reject one current approach and constrain another:

- Reject treating a dangling file: ID as valid current-code evidence. The
  proposed fallback explicitly acknowledges "dual-spelling graph nodes,
  Site/-prefix drift" and converts corruption into ordinary evidence:
  crates/engram_server/src/services/ask_engine/providers.rs:450. This remains
  both an ingestion/canonicalization bug and an integrity-repair gap.

- The new symbol-substring provider performs up to six query_nodes calls per
  question, silently caps results, ignores lookup failures and reports
  default coverage: crates/engram_server/src/services/ask_engine/providers.rs:748.
  query_nodes is a project node-table scan: crates/engram_graph/src/store.rs:1474.
  Do not ship this as a universal request-path arm without an indexed name
  lookup, measured latency, error propagation and noise/regression tests.

- The cardinality-one and modality-query changes are plausible targeted
  improvements, but current tests are microfixtures. They need end-to-end
  response tests and paraphrase/collision/cap/error cases.

## Final ruling

They are progressing, and the 15/15 canonical route result is absolutely
real. But ask_codebase is not yet broadly more trustworthy for OciusX agents.
It is better on a rehearsed query shape; it remains unreliable across natural
phrasing, ambiguity and coverage boundaries.

The Phase G result is the honest bottom line: agents used 32/42 answers, yet
implementation quality remained exactly neutral. "Agents liked the answers"
is not the goal; fewer bad implementations is.

I would pause survivor-specific ranking patches and implement, in this order:

1. Typed answer members plus an exact CoverageProof.
2. Fail-closed completeness under caps, dangling edges and provider errors.
3. Semantic/paraphrase-stable contract compilation.
4. Path-aware ambiguity and multi-target execution.
5. Structured identity-set evaluation.
6. Only then resume the remaining survivor grind and repeat a properly
   preserved, arm-blinded A/B.

That would turn the current improvement from a successful special case into
the foundation of the "project brain" you actually want.
