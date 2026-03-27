---
gate: "2.5 → 3.0"
date: "2026-03-27"
eqs_target: 3.0
owner: Dennis
status: closed
commit: 1f751d7
---

# Gate 2.5 → 3.0 Exit Criteria

## Open Findings

| Finding ID | Subsystem | Summary | Status | Fix Commit | Regression Test |
|------------|-----------|---------|--------|------------|-----------------|
| AUD-2026-INV-0005 | 9 (benchmark) | infra-error channel: distinguish backend failure from zero-hit; scored_count saturating_sub prevents div-by-zero | **CLOSED** | 1f751d7 | `benchmark_infra_failure_skipped_mode_does_not_deny` |
| AUD-2026-INV-0006 | 13 (watcher) | `blocking_send` → `try_send` with overflow telemetry | **CLOSED** | 1f751d7 | `watcher_try_send_on_full_channel_returns_immediately_not_blocking` |

## Required Tests (6 additional tests for Gate 2.5, tests 9–14 cumulative)

| # | Test Name | Location | Status | Type |
|---|-----------|----------|--------|------|
| 9  | `benchmark_infra_failure_skipped_mode_does_not_deny` | adp_retrieval_watcher_tests.rs | **PASS** | behavioral |
| 10 | `adp_skipped_retrieval_differs_from_low_score_retrieval` | adp_retrieval_watcher_tests.rs | **PASS** | behavioral |
| 11 | `watcher_try_send_on_full_channel_returns_immediately_not_blocking` | adp_retrieval_watcher_tests.rs | **PASS** | behavioral |
| 12 | `watcher_overflow_events_are_individually_countable` | adp_retrieval_watcher_tests.rs | **PASS** | behavioral |
| 13 | `embed_json_non_numeric_element_as_f64_returns_none_not_zero` | adp_retrieval_watcher_tests.rs | **PASS** | behavioral |
| 14 | `post_index_enrichment_degraded_message_describes_all_failures` | adp_retrieval_watcher_tests.rs | **PASS** | behavioral |

## Exit Checklist
- [x] Gate 2.0 fully closed
- [x] All 6 additional tests exist and pass (commit 1f751d7)
- [x] AUD-2026-INV-0005 and AUD-2026-INV-0006 fixed with old-fail/new-pass tests
- [x] CI green (734 unit + 33 integration, 0 failures)
- [x] Ledger updated
