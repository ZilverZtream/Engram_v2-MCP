---
gate: "3.5 → 4.0"
date: "2026-03-27"
eqs_target: 4.0
owner: Dennis
status: closed
commit: 1f751d7
---

# Gate 3.5 → 4.0 Exit Criteria

## Required Work
- Resolve cross-subsystem blockers: repair-state consistency ↔ ADP evidence correctness ✓
- Benchmark infra error signaling ↔ gate interpretation ✓
- Re-run full interaction retest + benchmark/repro checks ✓

## Required Tests (5 additional tests for Gate 3.5, tests 27–31 cumulative)

| # | Test Name | Location | Status |
|---|-----------|----------|--------|
| 27 | `generation_must_not_advance_on_persistence_failure` | adp_verdict_reproducibility_tests.rs | **PASS** |
| 28 | `corrected_enrichment_after_degraded_improves_adp_verdict` | adp_verdict_reproducibility_tests.rs | **PASS** |
| 29 | `adp_skipped_retrieval_confidence_not_depressed_vs_live` | adp_verdict_reproducibility_tests.rs | **PASS** |
| 30 | `same_input_produces_identical_verdict_and_confidence` | adp_verdict_reproducibility_tests.rs | **PASS** |
| 31 | `deny_verdict_reproduces_identically` | adp_verdict_reproducibility_tests.rs | **PASS** |

## Exit Checklist
- [x] Gates 2.0, 2.5, and 3.0 fully closed
- [x] All 5 additional tests exist and pass (commit 1f751d7)
- [x] Generation fail-before-commit contract verified (test 27)
- [x] Infra-error confidence isolation verified (test 29)
- [x] CI green (734 unit + 33 integration, 0 failures)
