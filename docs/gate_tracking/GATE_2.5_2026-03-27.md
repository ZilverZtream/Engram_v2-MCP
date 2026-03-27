---
gate: "2.5 → 3.0"
date: "2026-03-27"
eqs_target: 3.0
owner: Dennis
status: blocked_on_gate_2.0
---

# Gate 2.5 → 3.0 Exit Criteria

## Open Findings

| Finding ID | Subsystem | Summary | Status | Fix Commit | Regression Test |
|------------|-----------|---------|--------|------------|-----------------|
| AUD-2026-INV-0005 | 9 (benchmark) | `unwrap_or_default` → explicit infra-error channel; distinguish backend failure from zero-hit | Open | — | gate_2_5_tests::benchmark_backend_error_produces_degraded_status |
| AUD-2026-INV-0006 | 13 (watcher) | `blocking_send` → non-blocking/timeout send with overflow telemetry | Open | — | gate_2_5_tests::watcher_overflow_emits_telemetry |

## Required Tests (6 additional tests for Gate 2.5, tests 9–14 cumulative)

| # | Test Name | Location | Status | Type |
|---|-----------|----------|--------|------|
| 9  | `benchmark_backend_error_produces_degraded_status` | gate_2_5_tests.rs | TODO | behavioral |
| 10 | `adp_gate_eval_infra_error_distinct_from_low_relevance` | gate_2_5_tests.rs | TODO | behavioral |
| 11 | `watcher_channel_saturation_does_not_block_callback` | gate_2_5_tests.rs | TODO | behavioral |
| 12 | `watcher_overflow_emits_explicit_overflow_telemetry` | gate_2_5_tests.rs | TODO | behavioral |
| 13 | `retry_exhaustion_surfaces_explicit_failure_not_empty_result` | gate_2_5_tests.rs | TODO | behavioral |
| 14 | `post_index_enrichment_degraded_does_not_emit_success_banner` | gate_2_5_tests.rs | TODO | behavioral |

## Blocked On
Gate 2.0 must be fully closed before this gate is evaluated.

## Exit Checklist
- [ ] Gate 2.0 fully closed
- [ ] All 6 additional tests exist and pass
- [ ] AUD-2026-INV-0005 and AUD-2026-INV-0006 fixed with old-fail/new-pass tests
- [ ] CI green
- [ ] Ledger updated
