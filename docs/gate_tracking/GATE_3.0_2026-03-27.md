---
gate: "3.0 → 3.5"
date: "2026-03-27"
eqs_target: 3.5
owner: Dennis
status: closed
commit: 1f751d7
---

# Gate 3.0 → 3.5 Exit Criteria

## Required Work
- Close security-boundary High (AUD-2026-INV-0001) with adversarial regression tests ✓
- Close or formally accept all open findings in recently touched modules ✓

## Required Tests (12 additional tests for Gate 3.0, tests 15–26 cumulative)

| # | Test Name | Location | Status |
|---|-----------|----------|--------|
| 15 | `embedding_valid_floats_parse_to_correct_f32_values` | adp_security_hardening_tests.rs | **PASS** |
| 16 | `adp_cached_retrieval_lower_confidence_than_live` | adp_security_hardening_tests.rs | **PASS** |
| 17 | `evaluate_gates_degenerate_input_does_not_panic` | adp_security_hardening_tests.rs | **PASS** |
| 18 | `adp_safety_deny_from_join_failed_graph_produces_deny_verdict` | adp_security_hardening_tests.rs | **PASS** |
| 19 | `adp_missing_blast_radius_with_high_risk_is_not_allow` | adp_security_hardening_tests.rs | **PASS** |
| 20 | `actor_dreamer_spawn_blocking_panic_is_join_error_regression` | adp_security_hardening_tests.rs | **PASS** |
| 21 | `actor_immune_spawn_blocking_panic_is_join_error_regression` | adp_security_hardening_tests.rs | **PASS** |
| 22 | `path_traversal_dotdot_does_not_escape_root_after_canonicalization` | adp_security_hardening_tests.rs | **PASS** |
| 23 | `path_absolute_escape_does_not_bypass_prefix_check` | adp_security_hardening_tests.rs | **PASS** |
| 24 | `allowed_roots_empty_creates_state_with_validation_error` | adp_security_hardening_tests.rs | **PASS** |
| 25 | `all_enrichment_warnings_appear_in_job_message_not_just_first` | adp_security_hardening_tests.rs | **PASS** |
| 26 | `no_project_record_when_dir_creation_fails_explicit_err` | adp_security_hardening_tests.rs | **PASS** |

## Exit Checklist
- [x] Gates 2.0 and 2.5 fully closed
- [x] All 12 additional tests exist and pass (commit 1f751d7)
- [x] AUD-2026-INV-0001 adversarial path traversal covered by tests 22–24
- [x] CI green (734 unit + 33 integration, 0 failures)
