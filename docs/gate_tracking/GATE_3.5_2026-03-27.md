---
gate: "3.5 → 4.0"
date: "2026-03-27"
eqs_target: 4.0
owner: Dennis
status: blocked_on_gate_3.0
---

# Gate 3.5 → 4.0 Exit Criteria

## Required Work
- Resolve cross-subsystem blockers: repair-state consistency ↔ ADP evidence correctness
- Benchmark infra error signaling ↔ gate interpretation
- Re-run full interaction retest + benchmark/repro checks

## Required Tests (5 additional tests for Gate 3.5, tests 27–31 cumulative)

| # | Test Name | Status |
|---|-----------|--------|
| 27 | `repair_set_meta_failure_no_stale_generation_pointer` | TODO |
| 28 | `adp_evidence_correct_after_enrichment_degraded` | TODO |
| 29 | `benchmark_infra_error_does_not_influence_adp_confidence` | TODO |
| 30 | `full_retest_no_gate_regression_after_s03_fix` | TODO |
| 31 | `recent_change_regression_s09_s12_no_new_failures` | TODO |

## Blocked On
Gates 2.0, 2.5, and 3.0 must be fully closed.
