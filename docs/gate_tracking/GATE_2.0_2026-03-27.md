---
gate: "2.0 → 2.5"
date: "2026-03-27"
eqs_target: 2.5
owner: Dennis
status: tests_passing_pending_ci
---

# Gate 2.0 → 2.5 Exit Criteria

## Open Findings (must all be Fixed or Risk-Accepted before gate advances)

| Finding ID | Subsystem | Summary | Status | Fix Commit | Regression Test |
|------------|-----------|---------|--------|------------|-----------------|
| AUD-2026-INV-0001 | 2 (path validation) | `allowed_roots` empty → fail-closed; add `allow_cwd_default` opt-in | Fixed | e2550be | gate_2_0_tests::empty_allowed_roots_fails_closed |
| AUD-2026-INV-0002 | 3 (repair) | `repair_project` must propagate `process_ingest_stats` + `set_meta` failures | Fixed | e2550be | gate_2_0_tests::repair_set_meta_failure_degrades_result |
| AUD-2026-INV-0003 | 5 (index) | `create_dir_all` `.ok()` → fail early with explicit error | Fixed | e2550be | gate_2_0_tests::index_mkdir_failure_returns_explicit_error |
| AUD-2026-INV-0004 | 9 (ADP) | `gather_evidence` must derive `has_runtime_evidence` from graph/runtime stores | Fixed | e2550be | gate_2_0_tests::adp_gather_evidence_derives_runtime_without_overrides |
| S03-0001 | 3 (enrichment) | Enrichment failures surface as "degraded" status not silent warn | Fixed | (this session) | gate_2_0_tests::enrichment_failure_surfaces_degraded_status |

## Required Tests (8 minimum for Gate 2.0)

| # | Test Name | Location | Status | Type |
|---|-----------|----------|--------|------|
| 1 | `link_sql_failure_surfaces_degraded_status` | handlers::project_tools::inv_tag_tests | **PASS** | behavioral |
| 2 | `resolve_symbol_failure_surfaces_degraded_status` | handlers::project_tools::inv_tag_tests | **PASS** | behavioral |
| 3 | `git_update_failure_does_not_report_clean_success` | handlers::project_tools::inv_tag_tests | **PASS** | behavioral |
| 4 | `graph_impact_join_failed_true_yields_denied_safety` | services::evidence_orchestration::tests | **PASS** | behavioral |
| 5 | `parse_embedding_array_rejects_null_element` | engram_ml::embed::parse_embedding_array_tests | **PASS** | behavioral |
| 6 | `parse_embedding_array_rejects_string_element` | engram_ml::embed::parse_embedding_array_tests | **PASS** | behavioral |
| 7 | `spawn_blocking_join_error_is_propagated_not_swallowed` (immune) | actors::immune::tests | **PASS** | behavioral |
| 8 | `spawn_blocking_join_error_is_propagated_not_swallowed` (dreamer) | actors::dreamer::tests | **PASS** | behavioral |

## Exit Checklist
- [x] All 8 tests exist and pass (verified 2026-03-27 — all behavioral, no include_str! assertions)
- [x] Old behavior documented: status was "done" even with enrichment failures; join errors swallowed as Ok(None)
- [x] No finding in this gate remains Open without fix commit
- [ ] CI green on main (pending push)
- [x] Ledger updated in RISK_ACCEPTANCE.md

## PR / Commit Evidence
- Production fix (S03 enrichment degraded status): this session (determine_job_status helper + Arc<Mutex> warnings)
- New behavioral tests (tests 1-3): handlers::project_tools::inv_tag_tests
- Gate tracking files created: 2026-03-27
- Production code: enrichment failures now produce "degraded" status and surface warning text in job message
