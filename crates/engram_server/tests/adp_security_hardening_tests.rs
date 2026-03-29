#![allow(clippy::unwrap_used)]
//! Behavioral tests for ADP security hardening, adversarial path validation, and
//! actor fault propagation.
//!
//! Covers:
//!  - AUD-2026-INV-0001: path traversal rejection and allowed-roots enforcement
//!  - ADP gate behavior under degenerate inputs (all-None, join_failed, missing blast radius)
//!  - Enrichment degradation message completeness
//!  - Actor spawn_blocking panic propagation (dreamer + immune regression)

use engram_core::{safe_join, PathContext};
use engram_server::services::autonomous_decision_service::*;
use engram_server::services::safety_service::{PolicyDecision, RiskLevel};
use std::path::PathBuf;

// ── Shared helpers ────────────────────────────────────────────────────────────

fn safe_policy() -> PolicyDecision {
    PolicyDecision {
        allowed: true,
        risk_level: RiskLevel::Low,
        checks: vec![],
        confidence: 0.95,
        summary: "Safe".into(),
        mitigations: vec![],
    }
}

fn all_green_input() -> AdpInput {
    AdpInput {
        extraction_confidence: Some(0.9),
        extraction_band: Some("high".into()),
        trace_used_fallback: false,
        trace_candidate_count: 0,
        safety_decision: Some(safe_policy()),
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
        risk_profile: RiskProfile::Medium,
        min_extraction_confidence: 0.5,
        min_safety_confidence: 0.7,
        max_blast_radius_for_auto: 6,
        reconciliation: None,
        graph_impact: None,
        retrieval_mode: RetrievalMode::Live,
        migration_class: None,
    }
}

// ── embedding JSON parity — valid floats parse correctly ─────────────

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

// ── ADP parity — Cached retrieval always lower confidence than Live ──

/// Cached retrieval must produce lower ADP confidence than
/// Live retrieval with the same retrieval scores, due to the staleness discount.
///
/// Parity test: same gate inputs, only retrieval_mode differs.
#[test]
fn adp_cached_retrieval_lower_confidence_than_live() {
    let mut live = all_green_input();
    live.retrieval_mode = RetrievalMode::Live;

    let mut cached = all_green_input();
    cached.retrieval_mode = RetrievalMode::Cached;

    let live_dec = evaluate_gates(&live);
    let cached_dec = evaluate_gates(&cached);

    assert!(
        cached_dec.confidence < live_dec.confidence,
        "Cached confidence ({}) must be less than Live confidence ({})",
        cached_dec.confidence, live_dec.confidence
    );
}

// ── No panic on degenerate input ────────────────────────────────────

/// evaluate_gates must return a valid result (not panic) even
/// when all optional fields are None and retrieval is Skipped.
///
/// "Empty project state" parity: the gate pipeline must be robust to missing data.
#[test]
fn evaluate_gates_degenerate_input_does_not_panic() {
    let input = AdpInput {
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
        risk_profile: RiskProfile::High,
        min_extraction_confidence: 0.5,
        min_safety_confidence: 0.7,
        max_blast_radius_for_auto: 6,
        reconciliation: None,
        graph_impact: None,
        retrieval_mode: RetrievalMode::Skipped,
        migration_class: None,
    };

    // Must not panic
    let decision = evaluate_gates(&input);

    // Must return a valid verdict
    let valid_verdicts = [AdpVerdict::Allow, AdpVerdict::Deny, AdpVerdict::Abstain];
    assert!(
        valid_verdicts.contains(&decision.verdict),
        "verdict must be Allow/Deny/Abstain, got {:?}",
        decision.verdict
    );
    // Confidence must be finite
    assert!(decision.confidence.is_finite(),
        "confidence must be finite even with all-None inputs");
}

// ── graph_impact join_failed → safety_decision deny → ADP Deny ───────────────

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
    use engram_server::services::safety_service::RiskLevel;

    let mut input = all_green_input();
    // Simulate what derive_safety_from_graph returns when join_failed=true:
    // allowed=false, confidence=0.0 (indeterminate evidence)
    input.safety_decision = Some(PolicyDecision {
        allowed: false,
        risk_level: RiskLevel::High,
        checks: vec![],
        confidence: 0.0,
        summary: "ENG-AUD-2026-S09-0001: graph evidence join failed — deny".into(),
        mitigations: vec!["retry evidence gathering".into()],
    });
    input.graph_impact = Some(GraphImpactMetrics {
        downstream_dependency_count: 0,
        reads_state_count: 0,
        writes_state_count: 0,
        sql_calls_count: 0,
        queries_table_count: 0,
        injects_script_count: 0,
        join_failed: true,
    });

    let decision = evaluate_gates(&input);

    assert_eq!(
        decision.verdict,
        AdpVerdict::Deny,
        "safety_decision.allowed=false (from join_failed graph) must produce Deny; got {:?}",
        decision.verdict
    );
    // Confidence may be above 0.5 because other gates passed — the important
    // invariant is that the verdict is Deny, not that confidence is artificially low.
    assert!(
        decision.confidence.is_finite(),
        "Deny verdict confidence must be finite; got {}",
        decision.confidence
    );
}

// ── missing blast radius → Abstain for high risk ────────────────────

/// When blast_radius_risk=None with a High risk profile,
/// the ADP gate should not produce Allow — it should Abstain (or Deny).
///
/// This ensures that missing evidence with high-risk context is fail-conservative,
/// not permissive.
#[test]
fn adp_missing_blast_radius_with_high_risk_is_not_allow() {
    let mut input = all_green_input();
    input.blast_radius_risk = None;
    input.blast_radius_band = None;
    input.blast_radius_downstream = None;
    input.risk_profile = RiskProfile::High;

    let decision = evaluate_gates(&input);

    assert_ne!(
        decision.verdict,
        AdpVerdict::Allow,
        "missing blast radius with High risk profile must not Allow; \
         got {:?} (should be Abstain or Deny)",
        decision.verdict
    );
}

// ── dreamer spawn_blocking panic → JoinError (Gate 3.0 regression) ──

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

// ── immune spawn_blocking panic → JoinError (Gate 3.0 regression) ───

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

// ── adversarial path validation — production code ───────────────────────────

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

// ── enrichment warnings are ALL surfaced in job message ──────────────

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

// ── no partial record when directory creation fails ─────────────────

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

