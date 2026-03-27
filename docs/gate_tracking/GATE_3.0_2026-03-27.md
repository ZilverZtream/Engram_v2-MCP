---
gate: "3.0 → 3.5"
date: "2026-03-27"
eqs_target: 3.5
owner: Dennis
status: blocked_on_gate_2.5
---

# Gate 3.0 → 3.5 Exit Criteria

## Required Work
- Close security-boundary High (AUD-2026-INV-0001) with adversarial regression tests
- Close or formally accept all open findings in recently touched modules with owner+due date

## Required Tests (12 additional tests for Gate 3.0, tests 15–26 cumulative)

| # | Test Name | Status |
|---|-----------|--------|
| 15 | `embedding_parity_openai_vs_ollama_shape` | TODO |
| 16 | `adp_parity_live_vs_cached_retrieval_confidence` | TODO |
| 17 | `search_returns_empty_on_unindexed_project` | TODO |
| 18 | `graph_impact_join_failed_propagates_to_adp_deny` | TODO |
| 19 | `migration_score_degrades_on_missing_blast_radius` | TODO |
| 20 | `actor_dreamer_panic_does_not_leave_job_in_running_state` | TODO |
| 21 | `actor_immune_panic_does_not_leave_job_in_running_state` | TODO |
| 22 | `adversarial_path_traversal_blocked_by_allowed_roots` | TODO |
| 23 | `adversarial_symlink_escape_blocked_by_allowed_roots` | TODO |
| 24 | `allowed_roots_empty_always_fails_closed_adversarial` | TODO |
| 25 | `enrichment_failure_not_reported_as_success_in_job_api` | TODO |
| 26 | `index_dir_create_failure_no_partial_record_created` | TODO |

## Blocked On
Gates 2.0 and 2.5 must be fully closed.
