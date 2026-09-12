# Final status — ship what works, stop tuning (2026-09-03)

Owner decision after Gate-1 FAIL: get_change_set is at its validated plateau;
consolidate and stop retrieval R&D. This document is the honest closing
ledger.

## What ships and is proven to work

- **get_change_set** — the change-set dossier. The only component with a
  measured agent-quality lift: **+0.80 implementation score at n=15, 11 wins
  / 0 losses** (capstone A/B, `eval/data/p2/_ab15_final_verdicts.json`).
  Page-family recall ~73% held-out / ~77% all-15 after five committed recall
  fixes (greedy path extraction, case-insensitive co-change, prefix-dedup,
  resx indexing, semantic arm). This is the product. It is the default native
  tool; nothing about it changed in the final round.

- **The r75 honesty change** (commit 78f1b44, deployed, live) — typed
  `AnswerMember` + exact `CoverageProof`. ask_codebase now proves
  completeness from counters instead of asserting it; the flagship callee
  answer honestly reports *partial* on one dangling graph edge rather than a
  false *Answered*. This corrected the one round-4 finding that was
  indefensible.

- The round-2/3 remediation ledger, auditor-verified across rounds: the GC
  vector race, corpus-collapse, change-set ranking, missing-document
  handling, Dream default, immutable acceptance run, TS→VB route resolution,
  cross-store health equality, co-change 11.7s→326ms, and the full P1 ledger
  (P1a–P1e).

## What is parked as recorded evidence (NOT shipped, NOT wired)

- **ask_codebase contract program** (doc 13, phases A–F). Landed and live,
  but Phase G measured its marginal agent value at exactly **0.00 (n=15)**.
  Kept because it is correct and harmless; it is not a needle-mover and gets
  no further hardening.

- **verify_implementation_plan v0** (`services/plan_verify.rs`, doc 17). The
  owner-approved experiment to attack the 0.00. Gate 1 (pre-registered) ran
  on the real graph and **FAILED 6/15** (doc 18): a companion+convention
  verifier cannot catch the modal real defect, which is *wrong-family /
  wrong-mechanism*, not *missing-companion*. Kept in-tree as the experiment
  and its evidence; not wired to any tool or request path.

## The conclusion the data forced

Engram's retrieval is strong and its value is real but **bounded and already
captured**: get_change_set converts ~15× recall into +0.80 impl, and two
independent attempts to push implementation quality *above* current recall
(change-pattern diffs, structural method maps) were **both net-negative at
n=15**. The remaining ceiling is the **coding model's execution** of a
multi-layer legacy change — matching the developer's exact client-side edits
and design decisions — which is not an Engram-retrieval problem. Chasing
more retrieval or more answer-engine sophistication does not move the metric
the owner cares about (fewer bad implementations).

Retrieval R&D on this benchmark is therefore closed. get_change_set stands as
the shipped, validated product.

## Operational state

- Deployed binary: r75 (78f1b44). Daemon healthy, generation 948, 2277 paths
  complete, cross-store equal.
- Tree: clean, green (plan_verify 5/5 property tests; last full sweep76
  3,822/0/202).
- Eval evidence committed under `eval/data/p2/` (force-added past .gitignore
  for durability, per done-bar D7).
