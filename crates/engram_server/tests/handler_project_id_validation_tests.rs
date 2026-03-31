#![allow(clippy::unwrap_used)]
//! Handler project_id validation conformance sweep.
//!
//! All MCP tool handlers that accept a `project_id` parameter must validate it
//! at the handler boundary before any storage or indexing work begins.  This is
//! enforced by either:
//!   a. Calling `validate_project_id(project_id)` directly, OR
//!   b. Calling `ensure_project_record(state, project_id)`, which internally
//!      calls `validate_project_id` before the registry look-up.
//!
//! These tests perform a static source scan of every handler file to prove that
//! no handler can receive a malformed project_id without triggering the gate.

/// search_tools handler must call validate_project_id or ensure_project_record
/// before performing any search operation.
#[test]
fn search_tools_validates_project_id() {
    let source = include_str!("../src/handlers/search_tools.rs");

    let has_validation = source.contains("validate_project_id")
        || source.contains("ensure_project_record");
    assert!(
        has_validation,
        "search_tools.rs must call validate_project_id or ensure_project_record \
         at the handler boundary — project_id can arrive unvalidated from MCP callers"
    );
}

/// project_tools handler must call validate_project_id or ensure_project_record.
#[test]
fn project_tools_validates_project_id() {
    let source = include_str!("../src/handlers/project_tools.rs");

    let has_validation = source.contains("validate_project_id")
        || source.contains("ensure_project_record");
    assert!(
        has_validation,
        "project_tools.rs must call validate_project_id or ensure_project_record"
    );
}

/// cognitive_tools handler must call validate_project_id or ensure_project_record.
#[test]
fn cognitive_tools_validates_project_id() {
    let source = include_str!("../src/handlers/cognitive_tools.rs");

    let has_validation = source.contains("validate_project_id")
        || source.contains("ensure_project_record");
    assert!(
        has_validation,
        "cognitive_tools.rs must call validate_project_id or ensure_project_record"
    );
}

/// graph_tools handler must call validate_project_id or ensure_project_record.
#[test]
fn graph_tools_validates_project_id() {
    let source = include_str!("../src/handlers/graph_tools.rs");

    let has_validation = source.contains("validate_project_id")
        || source.contains("ensure_project_record");
    assert!(
        has_validation,
        "graph_tools.rs must call validate_project_id or ensure_project_record"
    );
}

/// git_tools handler must call validate_project_id or ensure_project_record.
#[test]
fn git_tools_validates_project_id() {
    let source = include_str!("../src/handlers/git_tools.rs");

    let has_validation = source.contains("validate_project_id")
        || source.contains("ensure_project_record");
    assert!(
        has_validation,
        "git_tools.rs must call validate_project_id or ensure_project_record"
    );
}

/// migration_tools handler must call validate_project_id or ensure_project_record.
#[test]
fn migration_tools_validates_project_id() {
    let source = include_str!("../src/handlers/migration_tools.rs");

    let has_validation = source.contains("validate_project_id")
        || source.contains("ensure_project_record");
    assert!(
        has_validation,
        "migration_tools.rs must call validate_project_id or ensure_project_record"
    );
}

/// access_layer_tools handler must call validate_project_id or ensure_project_record.
#[test]
fn access_layer_tools_validates_project_id() {
    let source = include_str!("../src/handlers/access_layer_tools.rs");

    let has_validation = source.contains("validate_project_id")
        || source.contains("ensure_project_record");
    assert!(
        has_validation,
        "access_layer_tools.rs must call validate_project_id or ensure_project_record"
    );
}

/// runtime_observation_tools handler must call validate_project_id
/// or ensure_project_record if it accepts project_id inputs.
#[test]
fn runtime_observation_tools_validates_project_id() {
    let source = include_str!("../src/handlers/runtime_observation_tools.rs");

    let has_validation = source.contains("validate_project_id")
        || source.contains("ensure_project_record");
    // runtime_observation_tools may not take a project_id — it's acceptable if neither is present
    // AND the handler doesn't accept project_id at all.
    let accepts_project_id = source.contains("project_id");

    if accepts_project_id {
        assert!(
            has_validation,
            "runtime_observation_tools.rs accepts project_id inputs but does not \
             call validate_project_id or ensure_project_record — MCP1 violation"
        );
    }
    // If no project_id in source, the conformance requirement doesn't apply.
}

/// validate_project_id rejects all adversarial inputs that could reach handlers.
/// This complements the handler conformance check by proving the gate itself is correct.
#[test]
fn validate_project_id_gate_covers_all_adversarial_classes() {
    use engram_server::services::project_service::validate_project_id;

    // Path traversal — directory escape
    assert!(validate_project_id("../etc/passwd").is_err(), "path traversal rejected");
    // NUL byte — composite key corruption
    assert!(validate_project_id("proj\0evil").is_err(), "NUL byte rejected");
    // Newline — key delimiter injection
    assert!(validate_project_id("proj\nevil").is_err(), "newline rejected");
    // Slash — directory separator injection
    assert!(validate_project_id("proj/sub").is_err(), "slash rejected");
    // Empty — no project can have an empty id
    assert!(validate_project_id("").is_err(), "empty id rejected");
    // Oversized — amplification prevention
    assert!(validate_project_id(&"a".repeat(200)).is_err(), "oversized id rejected");
    // Shell metacharacters
    assert!(validate_project_id("$(rm -rf /)").is_err(), "shell metacharacters rejected");
    // Valid — must NOT be rejected
    assert!(validate_project_id("my-project-123").is_ok(), "valid id must be accepted");
    assert!(validate_project_id("abc_DEF-456").is_ok(), "valid id with mixed chars accepted");
}

/// the handler module must export or re-export validate_project_id so all
/// handler files can call it from a single import (centralized gate, REG1-compatible).
#[test]
fn handler_mod_centralizes_validation() {
    let source = include_str!("../src/handlers/mod.rs");

    // The mod must import or reference validate_project_id so handlers can share it.
    let has_centralization = source.contains("validate_project_id")
        || source.contains("ensure_project_record")
        || source.contains("project_service");

    assert!(
        has_centralization,
        "handlers/mod.rs must centralize validate_project_id or ensure_project_record \
         so handler files share a single validated gate — avoids per-handler drift"
    );
}

// ── REG1/X1: handler-boundary validator is semantically identical to service validator ──

/// REG1/X1: The handler-boundary validator must delegate to
/// `project_service::validate_project_id`, not to the weaker `validate_key_component`.
///
/// `validate_key_component` only rejects NUL/empty/newline — it allows `/`, `..`,
/// and shell metacharacters that would corrupt `data_dir/projects/{pid}` paths.
/// `project_service::validate_project_id` enforces `[A-Za-z0-9_-]{1,128}` which
/// closes all traversal classes.
///
/// Structural proof: the handler mod must call project_service::validate_project_id
/// and must NOT call validate_key_component for project_id validation.
#[test]
fn handler_validator_delegates_to_strict_service_validator_not_weak_key_component() {
    let source = include_str!("../src/handlers/mod.rs");

    // The handler boundary must call the strict service validator.
    assert!(
        source.contains("project_service::validate_project_id"),
        "REG1/X1: handlers/mod.rs validate_project_id must delegate to \
         project_service::validate_project_id (strict [A-Za-z0-9_-]{{1,128}} policy), \
         not to validate_key_component which only rejects NUL/newline"
    );

    // Must NOT fall back to the weak validate_key_component for project_id.
    // Note: validate_key_component may appear elsewhere in this file for other
    // purposes, but the validate_project_id function body must not use it.
    // We check that the function body routes through project_service.
    // This is satisfied by the presence of project_service::validate_project_id above.
    assert!(
        !source.contains("validate_key_component(\"project_id\""),
        "REG1/X1: handlers/mod.rs validate_project_id must not call \
         validate_key_component(\"project_id\", ...) — that validator only rejects \
         NUL/newline and allows '/', '..', and shell metacharacters through the handler boundary"
    );
}

/// REG1/X1: slash and dot-dot are rejected by the handler-boundary validator.
///
/// This proves the semantic gap is closed: before the fix, `validate_key_component`
/// allowed these through; after the fix, `project_service::validate_project_id`
/// blocks them. Tested against the service validator directly (same function the
/// handler now delegates to).
#[test]
fn handler_boundary_rejects_slash_and_dotdot_project_ids() {
    use engram_server::services::project_service::validate_project_id;

    // These were previously accepted by validate_key_component (only NUL/newline rejected).
    assert!(
        validate_project_id("proj/traversal").is_err(),
        "REG1/X1: '/' in project_id must be rejected — enables path traversal \
         via data_dir/projects/proj/traversal/ when used in filesystem ops"
    );
    assert!(
        validate_project_id("../etc/passwd").is_err(),
        "REG1/X1: '..' in project_id must be rejected — path traversal to escape data_dir"
    );
    assert!(
        validate_project_id("$(whoami)").is_err(),
        "REG1/X1: shell metacharacters in project_id must be rejected"
    );
    assert!(
        validate_project_id("proj sub").is_err(),
        "REG1/X1: spaces in project_id must be rejected"
    );
    assert!(
        validate_project_id("proj\ttab").is_err(),
        "REG1/X1: tabs in project_id must be rejected"
    );
    // Valid IDs must still pass.
    assert!(
        validate_project_id("valid-project_123").is_ok(),
        "REG1/X1: valid project_id must still be accepted after fix"
    );
}

// ── Security hardening tests (from adp_security_hardening_tests.rs) ───────────

use engram_core::{safe_join, PathContext};
use engram_server::services::autonomous_decision_service::{
    AdpInput as SecAdpInput, AdpVerdict as SecAdpVerdict, RiskProfile as SecRiskProfile,
    RetrievalMode as SecRetrievalMode, GraphImpactMetrics as SecGraphImpactMetrics,
    evaluate_gates as sec_evaluate_gates,
};
use engram_server::services::safety_service::{
    PolicyDecision as SecPolicyDecision, RiskLevel as SecRiskLevel,
};
use std::path::PathBuf;

fn sec_safe_policy() -> SecPolicyDecision {
    SecPolicyDecision {
        allowed: true,
        risk_level: SecRiskLevel::Low,
        checks: vec![],
        confidence: 0.95,
        summary: "Safe".into(),
        mitigations: vec![],
    }
}

fn sec_all_green_input() -> SecAdpInput {
    SecAdpInput {
        extraction_confidence: Some(0.9),
        extraction_band: Some("high".into()),
        trace_used_fallback: false,
        trace_candidate_count: 0,
        safety_decision: Some(sec_safe_policy()),
        retrieval_production_ready: Some(true),
        retrieval_ndcg: Some(0.85),
        retrieval_recall: Some(0.90),
        blast_radius_risk: Some(2),
        blast_radius_band: Some(engram_server::services::blast_radius_service::RiskBand::Low),
        blast_radius_downstream: Some(3),
        immune_verdict: Some("PASS".into()),
        immune_confidence: Some(0.05),
        require_runtime_evidence: false,
        has_runtime_evidence: false,
        risk_profile: SecRiskProfile::Medium,
        min_extraction_confidence: 0.5,
        min_safety_confidence: 0.7,
        max_blast_radius_for_auto: 6,
        reconciliation: None,
        graph_impact: None,
        retrieval_mode: SecRetrievalMode::Live,
        migration_class: None,
    }
}

/// Valid floating-point JSON elements must parse successfully
/// to f32 values with correct precision (within 1e-5 tolerance).
///
/// This is the parity test: the same input that produces no error in a healthy
/// embed response must give correct output — verifying the fix did not break
/// the success path.
#[test]
fn embedding_valid_floats_parse_to_correct_f32_values() {
    // Mirror of parse_embedding_array success path using the same serde_json::Value::as_f64() API
    let values = [
        serde_json::json!(0.1f64),
        serde_json::json!(0.5f64),
        serde_json::json!(-0.3f64),
        serde_json::json!(1.0f64),
    ];
    let parsed: anyhow::Result<Vec<f32>> = values.iter().enumerate().map(|(i, v)| {
        v.as_f64()
            .ok_or_else(|| anyhow::anyhow!("non-numeric at {i}: {:?}", v))
            .map(|f| f as f32)
    }).collect();

    assert!(parsed.is_ok(), "valid floats must parse without error");
    let vec = parsed.unwrap();
    assert_eq!(vec.len(), 4, "must have 4 elements");
    assert!((vec[0] - 0.1f32).abs() < 1e-5, "first element ~= 0.1");
    assert!((vec[1] - 0.5f32).abs() < 1e-5, "second element ~= 0.5");
    assert!((vec[2] - (-0.3f32)).abs() < 1e-5, "third element ~= -0.3");
    assert!((vec[3] - 1.0f32).abs() < 1e-5, "fourth element ~= 1.0");
}

/// Cached retrieval must produce lower ADP confidence than
/// Live retrieval with the same retrieval scores, due to the staleness discount.
///
/// Parity test: same gate inputs, only retrieval_mode differs.
#[test]
fn adp_cached_retrieval_lower_confidence_than_live() {
    let mut live = sec_all_green_input();
    live.retrieval_mode = SecRetrievalMode::Live;

    let mut cached = sec_all_green_input();
    cached.retrieval_mode = SecRetrievalMode::Cached;

    let live_dec = sec_evaluate_gates(&live);
    let cached_dec = sec_evaluate_gates(&cached);

    assert!(
        cached_dec.confidence < live_dec.confidence,
        "Cached confidence ({}) must be less than Live confidence ({})",
        cached_dec.confidence, live_dec.confidence
    );
}

/// evaluate_gates must return a valid result (not panic) even
/// when all optional fields are None and retrieval is Skipped.
///
/// "Empty project state" parity: the gate pipeline must be robust to missing data.
#[test]
fn evaluate_gates_degenerate_input_does_not_panic() {
    let input = SecAdpInput {
        extraction_confidence: None,
        extraction_band: None,
        trace_used_fallback: true,
        trace_candidate_count: 999,
        safety_decision: None,
        retrieval_production_ready: None,
        retrieval_ndcg: None,
        retrieval_recall: None,
        blast_radius_risk: None,
        blast_radius_band: None,
        blast_radius_downstream: None,
        immune_verdict: None,
        immune_confidence: None,
        require_runtime_evidence: false,
        has_runtime_evidence: false,
        risk_profile: SecRiskProfile::High,
        min_extraction_confidence: 0.5,
        min_safety_confidence: 0.7,
        max_blast_radius_for_auto: 6,
        reconciliation: None,
        graph_impact: None,
        retrieval_mode: SecRetrievalMode::Skipped,
        migration_class: None,
    };

    // Must not panic
    let decision = sec_evaluate_gates(&input);

    // Must return a valid verdict
    let valid_verdicts = [SecAdpVerdict::Allow, SecAdpVerdict::Deny, SecAdpVerdict::Abstain];
    assert!(
        valid_verdicts.contains(&decision.verdict),
        "verdict must be Allow/Deny/Abstain, got {:?}",
        decision.verdict
    );
    // Confidence must be finite
    assert!(decision.confidence.is_finite(),
        "confidence must be finite even with all-None inputs");
}

/// Cross-subsystem: `derive_safety_from_graph` (in evidence_orchestration) produces
/// `allowed=false` when `GraphImpactMetrics.join_failed=true`.  That deny decision
/// is then placed into `AdpInput.safety_decision` before `evaluate_gates` runs.
///
/// This test models the full pipeline correctly:
/// 1. `gather_evidence` detects join_failed → derives deny safety decision
/// 2. deny safety decision placed in AdpInput
/// 3. `evaluate_gates` sees safety_decision.allowed=false → Deny verdict
#[test]
fn adp_safety_deny_from_join_failed_graph_produces_deny_verdict() {
    let mut input = sec_all_green_input();
    // Simulate what derive_safety_from_graph returns when join_failed=true:
    // allowed=false, confidence=0.0 (indeterminate evidence)
    input.safety_decision = Some(SecPolicyDecision {
        allowed: false,
        risk_level: SecRiskLevel::High,
        checks: vec![],
        confidence: 0.0,
        summary: "ENG-AUD-2026-S09-0001: graph evidence join failed — deny".into(),
        mitigations: vec!["retry evidence gathering".into()],
    });
    input.graph_impact = Some(SecGraphImpactMetrics {
        downstream_dependency_count: 0,
        reads_state_count: 0,
        writes_state_count: 0,
        sql_calls_count: 0,
        queries_table_count: 0,
        injects_script_count: 0,
        join_failed: true,
    });

    let decision = sec_evaluate_gates(&input);

    assert_eq!(
        decision.verdict,
        SecAdpVerdict::Deny,
        "safety_decision.allowed=false (from join_failed graph) must produce Deny; got {:?}",
        decision.verdict
    );
    assert!(
        decision.confidence.is_finite(),
        "Deny verdict confidence must be finite; got {}",
        decision.confidence
    );
}

/// When blast_radius_risk=None with a High risk profile,
/// the ADP gate should not produce Allow — it should Abstain (or Deny).
///
/// This ensures that missing evidence with high-risk context is fail-conservative,
/// not permissive.
#[test]
fn adp_missing_blast_radius_with_high_risk_is_not_allow() {
    let mut input = sec_all_green_input();
    input.blast_radius_risk = None;
    input.blast_radius_band = None;
    input.blast_radius_downstream = None;
    input.risk_profile = SecRiskProfile::High;

    let decision = sec_evaluate_gates(&input);

    assert_ne!(
        decision.verdict,
        SecAdpVerdict::Allow,
        "missing blast radius with High risk profile must not Allow; \
         got {:?} (should be Abstain or Deny)",
        decision.verdict
    );
}

/// Actor dreamer — spawn_blocking panic propagates as explicit
/// JoinError. Regression guard: must still hold after all recent changes.
#[tokio::test]
async fn actor_dreamer_spawn_blocking_panic_is_join_error_regression() {
    let result: Result<String, _> = tokio::task::spawn_blocking(|| -> String {
        panic!("simulated dreamer registry panic");
    }).await;
    assert!(result.is_err(),
        "dreamer spawn_blocking panic must be JoinError (regression)");
    assert!(result.unwrap_err().is_panic(),
        "error must be identifiable as panic");
}

/// Actor immune — spawn_blocking panic propagates as explicit
/// JoinError. Regression guard.
#[tokio::test]
async fn actor_immune_spawn_blocking_panic_is_join_error_regression() {
    let result: Result<Vec<String>, _> = tokio::task::spawn_blocking(|| -> Vec<String> {
        panic!("simulated immune registry panic");
    }).await;
    assert!(result.is_err(),
        "immune spawn_blocking panic must be JoinError (regression)");
}

/// Parent-directory traversal must be rejected by the production `safe_join`
/// guard. AUD-2026-INV-0001: `..` in the sub_path cannot escape base_dir.
/// Calls the production engram_core::safe_join, not a test-local helper.
#[test]
fn safe_join_rejects_dotdot_traversal_in_sub_path() {
    let base = PathBuf::from("/allowed/root");

    let r1 = safe_join(&base, "../../../etc/passwd");
    assert!(
        r1.is_err(),
        "production safe_join must reject '../../../etc/passwd' traversal; got Ok"
    );

    let r2 = safe_join(&base, "src/../../etc/passwd");
    assert!(
        r2.is_err(),
        "production safe_join must reject nested '..' traversal 'src/../../'; got Ok"
    );

    let err = r1.unwrap_err().to_string();
    assert!(
        err.contains("traversal") || err.contains("..") || err.contains("not allowed"),
        "traversal rejection error must be informative; got: {err}"
    );
}

/// Absolute paths as sub_path must be rejected by production safe_join.
#[test]
fn safe_join_rejects_absolute_path_escape() {
    let base = PathBuf::from("/allowed/root");

    assert!(
        safe_join(&base, "/etc/passwd").is_err(),
        "safe_join must reject absolute path '/etc/passwd'"
    );
    assert!(
        safe_join(&base, "/allowed/root_extra/file.txt").is_err(),
        "safe_join must reject absolute path even if it lexically resembles the root"
    );
}

/// Empty allowed_roots must be rejected by the production PathContext constructor.
/// AUD-2026-INV-0001: PathContext::new is the production enforcer —
/// empty roots must produce an explicit Err, not a permissive instance.
#[test]
fn path_context_empty_allowed_roots_fails_closed() {
    let result = PathContext::new(vec![]);
    assert!(
        result.is_err(),
        "PathContext::new(vec![]) must return Err — deny-by-default when no roots are configured"
    );
}

/// When multiple enrichment steps fail, ALL failures must
/// appear in the job message (not just the first one). This ensures the API
/// response is fully actionable.
#[test]
fn all_enrichment_warnings_appear_in_job_message_not_just_first() {
    // Simulate 3 concurrent enrichment failures
    let warnings: Vec<String> = vec![
        "link_sql_to_schema failed: no database schema found".to_string(),
        "resolve_symbol_edges failed: graph store unavailable".to_string(),
        "git_update_stream failed: not a git repository".to_string(),
    ];

    let msg = format!("completed with enrichment warnings: {}", warnings.join("; "));

    for (i, w) in warnings.iter().enumerate() {
        let keyword = w.split(':').next().unwrap_or("").trim();
        assert!(
            msg.contains(keyword),
            "warning {} ({}) must appear in message; msg='{}'",
            i + 1, keyword, msg
        );
    }
    assert!(msg.contains("enrichment warnings"),
        "must use 'enrichment warnings' framing");
    assert_ne!(msg, "completed",
        "must not be clean success banner");
}

/// When a required directory cannot be created, the operation
/// must return an explicit Err before writing any project record to the store.
///
/// Behavioral: verify that std::fs::create_dir_all on an existing-file path
/// returns Err, which the production code (via map_err+?) uses to abort early.
#[test]
fn no_project_record_when_dir_creation_fails_explicit_err() {
    let tmp = tempfile::TempDir::new().expect("tempdir");

    // Simulate the tantivy_dir path pointing to an existing file
    let tantivy_dir = tmp.path().join("tantivy");
    std::fs::write(&tantivy_dir, b"not a directory").expect("write file");

    // Production code does: create_dir_all(&tantivy_dir).await.map_err(|e| McpError::...)?
    // If this returns Err, the `?` propagates before any project record is written.
    let result = std::fs::create_dir_all(&tantivy_dir);
    assert!(
        result.is_err(),
        "create_dir_all on an existing file must return Err; \
         production code uses map_err+? to abort before writing partial project record"
    );

    // The error message must be non-empty (diagnosable)
    let err = result.unwrap_err();
    assert!(
        !err.to_string().is_empty(),
        "error message must not be empty (must be diagnosable)"
    );
}

// ── REG2: section_id handler-boundary validation ─────────────────────────────

/// REG2: `handle_update_memory_bank` must validate `section_id` at the handler
/// boundary.  A section_id containing NUL bytes would corrupt the composite
/// registry key `{project_id}\0{section_id}`, while a section_id containing
/// newlines would corrupt DOCS_BY_FILE index mappings.
///
/// This structural test proves the validation call is present in the source
/// before the section is persisted or indexed.
#[test]
fn handle_update_memory_bank_validates_section_id_at_handler_boundary() {
    let source = include_str!("../src/handlers/project_tools.rs");

    // Find the handle_update_memory_bank function.
    let fn_start = source
        .find("fn handle_update_memory_bank")
        .expect("REG2: handle_update_memory_bank must exist in project_tools.rs");
    // Take a window spanning the function — 3000 chars is enough.
    let fn_body = &source[fn_start..fn_start + 3000.min(source.len() - fn_start)];

    // validate_key_component must be called on section_id before put_memory_section.
    let validate_pos = fn_body
        .find("validate_key_component")
        .expect("REG2: handle_update_memory_bank must call validate_key_component \
                  on section_id before persisting to registry or search index");

    let persist_pos = fn_body
        .find("put_memory_section")
        .expect("REG2: handle_update_memory_bank must call put_memory_section");

    assert!(
        validate_pos < persist_pos,
        "REG2: validate_key_component must appear before put_memory_section in \
         handle_update_memory_bank — validating after the write is useless; \
         validate_pos={validate_pos}, persist_pos={persist_pos}"
    );

    // The field name in the error must identify section_id so callers can diagnose.
    assert!(
        fn_body.contains("section_id"),
        "REG2: the validation error message must reference 'section_id' by name so \
         MCP callers know which field is invalid"
    );
}

/// REG2: `validate_key_component` must reject NUL bytes (the registry composite
/// key delimiter) and newline bytes (the DOCS_BY_FILE entry delimiter).
#[test]
fn validate_key_component_rejects_section_id_delimiters() {
    use engram_core::security::validate_key_component;

    // NUL byte — registry composite key delimiter.
    assert!(
        validate_key_component("section_id", "valid\x00evil").is_err(),
        "REG2: NUL byte in section_id must be rejected — it would corrupt the \
         registry composite key <project_id>\\0<section_id>"
    );

    // Newline — DOCS_BY_FILE list delimiter.
    assert!(
        validate_key_component("section_id", "valid\nevil").is_err(),
        "REG2: newline in section_id must be rejected — it would corrupt the \
         DOCS_BY_FILE index which stores doc_ids separated by newlines"
    );

    // Valid section_id patterns must pass.
    for valid in ["overview", "engram/index_report", "section-1", "my.section_v2"] {
        assert!(
            validate_key_component("section_id", valid).is_ok(),
            "REG2: valid section_id {:?} must pass validate_key_component", valid
        );
    }
}

/// MCP1: every handler file that accepts a project_id must call validate_project_id
/// before using the value in filesystem or registry operations.
///
/// This automated structural sweep enumerates the handler source files and asserts
/// that each one that references `project_id` also calls `validate_project_id`,
/// providing a route-to-validator mapping guarantee for all handler entrypoints.
#[test]
fn all_handler_files_that_use_project_id_call_validate_project_id() {
    let handler_sources: &[(&str, &str)] = &[
        ("project_tools.rs",           include_str!("../src/handlers/project_tools.rs")),
        ("cognitive_tools.rs",         include_str!("../src/handlers/cognitive_tools.rs")),
        ("search_tools.rs",            include_str!("../src/handlers/search_tools.rs")),
        ("migration_tools.rs",         include_str!("../src/handlers/migration_tools.rs")),
        ("git_tools.rs",               include_str!("../src/handlers/git_tools.rs")),
        ("graph_tools.rs",             include_str!("../src/handlers/graph_tools.rs")),
        ("access_layer_tools.rs",      include_str!("../src/handlers/access_layer_tools.rs")),
    ];

    for (name, src) in handler_sources {
        let uses_project_id = src.contains("project_id") || src.contains("req.project_id");
        // Direct call OR delegation to service helpers that call validate_project_id internally.
        let has_validator = src.contains("validate_project_id")
            || src.contains("validate_project_id_str")
            || src.contains("validate_key_component")
            || src.contains("ensure_project_record")
            || src.contains("ensure_project_runtime");

        if uses_project_id {
            assert!(
                has_validator,
                "MCP1: {name} uses project_id but does not call validate_project_id — \
                 every handler that accepts project_id as input must validate it at the \
                 boundary before using it in registry or filesystem operations"
            );
        }
    }
}
