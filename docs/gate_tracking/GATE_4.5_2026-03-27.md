---
gate: "4.5 → 5.0"
date: "2026-03-27"
eqs_target: 5.0
owner: Dennis
status: blocked_on_gate_4.0
---

# Gate 4.5 → 5.0 Exit Criteria

## Required Work
- ADP mutation tests (deliberate wrong verdicts, check detection)
- Enrichment canary (known good project, assert confidence band)
- Embed failover canary (primary down → secondary, assert non-zero result)
- Cross-subsystem chaos (watcher + dreamer + immune concurrent faults)

## Required Tests (8 additional tests for Gate 4.5, tests 35–42 cumulative)

| # | Test Name | Status |
|---|-----------|--------|
| 35 | `adp_mutation_wrong_verdict_detected` | TODO |
| 36 | `adp_mutation_injected_high_confidence_deny_overrides_allow` | TODO |
| 37 | `enrichment_canary_known_good_project_confidence_band` | TODO |
| 38 | `embed_failover_primary_down_secondary_returns_result` | TODO |
| 39 | `embed_failover_both_down_returns_explicit_infra_error` | TODO |
| 40 | `chaos_watcher_dreamer_immune_concurrent_faults` | TODO |
| 41 | `chaos_all_actors_panic_no_orphan_jobs_remain_running` | TODO |
| 42 | `full_42_test_suite_no_regression_on_clean_build` | TODO |

## Blocked On
Gates 2.0, 2.5, 3.0, 3.5, and 4.0 must be fully closed.
