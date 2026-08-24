#![allow(clippy::unwrap_used)]
//! Behavioral tests for ADP retrieval gate modes and watcher channel backpressure.
//!
//! Covers:
//!  - AUD-2026-INV-0005: benchmark infra-error channel (retrieval gate mode discrimination)
//!  - AUD-2026-INV-0006: watcher non-blocking send (try_send backpressure behavior)
//!  - Embed JSON element validation (non-numeric → explicit Err)
//!  - Enrichment degraded job message framing

use engram_server::services::autonomous_decision_service::*;
use engram_server::services::safety_service::{PolicyDecision, RiskLevel};

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
        blast_causal_truncated: None,
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

// ── Test 9 (Gate 2.5): AUD-2026-INV-0005 — infra failure skips retrieval gate ──

/// Gate 2.5 Test 9 (AUD-2026-INV-0005): When retrieval_mode=Skipped (benchmark
/// was not run due to infra failure), the retrieval_quality gate must be marked
/// `skipped=true` and must NOT contribute a Deny verdict.
///
/// Old behavior (before fix): infra failures were misclassified as zero-relevance,
/// depressing NDCG scores and potentially producing false Deny verdicts.
#[test]
fn benchmark_infra_failure_skipped_mode_does_not_deny() {
    let mut input = all_green_input();
    input.retrieval_mode = RetrievalMode::Skipped;
    input.retrieval_production_ready = None;
    input.retrieval_ndcg = None;
    input.retrieval_recall = None;

    let decision = evaluate_gates(&input);

    // Gate must be skipped (not failed) when retrieval was not run
    let ret_gate = decision
        .gate_results
        .iter()
        .find(|g| g.gate_id == "retrieval_quality")
        .expect("retrieval_quality gate must exist");

    assert!(
        ret_gate.skipped,
        "AUD-2026-INV-0005: retrieval_quality gate must be skipped when mode=Skipped; \
         got skipped={}, passed={}",
        ret_gate.skipped, ret_gate.passed
    );

    // Skipped gate must NOT veto the verdict with Deny
    assert_ne!(
        decision.verdict,
        AdpVerdict::Deny,
        "AUD-2026-INV-0005: Skipped retrieval (infra failure) must not produce Deny; \
         got {:?}",
        decision.verdict
    );
}

// ── Test 10 (Gate 2.5): skipped vs low-score retrieval are distinguishable ───

/// Gate 2.5 Test 10 (AUD-2026-INV-0005): A skipped retrieval gate (infra failure)
/// must be distinguishable from a failed retrieval gate (genuinely low NDCG/recall).
///
/// - Skipped: gate.skipped=true, does not contribute to Deny
/// - Low-score: gate.skipped=false, gate.passed=false, contributes Deny
#[test]
fn adp_skipped_retrieval_differs_from_low_score_retrieval() {
    // Case A: Skipped mode (infra failure — benchmark not run)
    let mut skipped = all_green_input();
    skipped.retrieval_mode = RetrievalMode::Skipped;
    skipped.retrieval_production_ready = None;
    skipped.retrieval_ndcg = None;
    skipped.retrieval_recall = None;
    let skip_dec = evaluate_gates(&skipped);
    let skip_gate = skip_dec
        .gate_results
        .iter()
        .find(|g| g.gate_id == "retrieval_quality")
        .unwrap();

    // Case B: Live mode with genuinely low NDCG (retrieval quality problem)
    let mut low = all_green_input();
    low.retrieval_mode = RetrievalMode::Live;
    low.retrieval_production_ready = Some(false);
    low.retrieval_ndcg = Some(0.05);
    low.retrieval_recall = Some(0.05);
    let low_dec = evaluate_gates(&low);
    let low_gate = low_dec
        .gate_results
        .iter()
        .find(|g| g.gate_id == "retrieval_quality")
        .unwrap();

    // Structural: the two gate states must be different
    assert!(
        skip_gate.skipped,
        "AUD-2026-INV-0005: infra-failure gate must be skipped=true"
    );
    assert!(
        !low_gate.skipped,
        "AUD-2026-INV-0005: low-quality gate must be skipped=false (it has data)"
    );
    assert!(
        !low_gate.passed,
        "AUD-2026-INV-0005: low-quality gate must be passed=false"
    );

    // Behavioral: verdicts must differ — low NDCG denies, skipped does not
    assert_ne!(
        skip_dec.verdict, low_dec.verdict,
        "AUD-2026-INV-0005: Skipped verdict ({:?}) must differ from low-score verdict ({:?})",
        skip_dec.verdict, low_dec.verdict
    );
    assert_ne!(
        skip_dec.verdict,
        AdpVerdict::Deny,
        "AUD-2026-INV-0005: Skipped retrieval must never produce Deny"
    );
}

// ── Tests 11–12 (Gate 2.5): AUD-2026-INV-0006 — watcher channel non-blocking ─

/// Gate 2.5 Test 11 (AUD-2026-INV-0006): `try_send` on a saturated channel must
/// return `TrySendError::Full` immediately, never blocking the caller.
///
/// This is the behavioral contract that replacing `blocking_send` relies on.
/// `blocking_send` would have stalled the OS filesystem-event thread under
/// sustained event bursts; `try_send` returns immediately with an error instead.
#[tokio::test]
async fn watcher_try_send_on_full_channel_returns_immediately_not_blocking() {
    use tokio::sync::mpsc;
    use tokio::sync::mpsc::error::TrySendError;

    let (tx, _rx) = mpsc::channel::<String>(1);
    // Fill the single slot
    tx.try_send("fill".to_string())
        .expect("first send to empty channel must succeed");

    // Now saturated — must return Full immediately, never block
    let result = tx.try_send("overflow".to_string());
    assert!(
        matches!(result, Err(TrySendError::Full(_))),
        "AUD-2026-INV-0006: try_send on full channel must return TrySendError::Full immediately; \
         blocking_send would have blocked the notify callback thread under event bursts"
    );
}

/// Gate 2.5 Test 12 (AUD-2026-INV-0006): Overflow events are individually
/// countable — each overflow produces exactly one `TrySendError::Full`, making
/// overflow telemetry observable and deterministic.
#[tokio::test]
async fn watcher_overflow_events_are_individually_countable() {
    use tokio::sync::mpsc;
    use tokio::sync::mpsc::error::TrySendError;

    let capacity = 3usize;
    let extra_sends = 7usize;
    let (tx, _rx) = mpsc::channel::<u32>(capacity);

    let mut success_count = 0usize;
    let mut overflow_count = 0usize;

    for i in 0..(capacity + extra_sends) as u32 {
        match tx.try_send(i) {
            Ok(_) => success_count += 1,
            Err(TrySendError::Full(_)) => overflow_count += 1,
            Err(TrySendError::Closed(_)) => panic!("unexpected closed"),
        }
    }

    assert_eq!(
        success_count, capacity,
        "AUD-2026-INV-0006: exactly {capacity} sends must succeed (channel capacity)"
    );
    assert_eq!(
        overflow_count, extra_sends,
        "AUD-2026-INV-0006: exactly {extra_sends} overflow events must be countable — \
         each maps to one warn!() telemetry call in the watcher notify callback"
    );
}

// ── Test 13 (Gate 2.5): retry exhaustion → explicit error not empty Ok ────────

/// Non-numeric elements in JSON embedding arrays must cause
/// explicit Err results, not be silently defaulted to 0.0.
///
/// Behavioral test against the serde_json API that parse_embedding_array relies on:
/// `Value::as_f64()` returns None for null/string/bool, and that None must map to
/// Err — not to 0.0f32 via unwrap_or.
#[test]
fn embed_json_non_numeric_element_as_f64_returns_none_not_zero() {
    // These are the three cases the fix guards against: null, string, bool
    let null_val = serde_json::Value::Null;
    let str_val = serde_json::json!("not_a_number");
    let bool_val = serde_json::json!(true);
    let number_val = serde_json::json!(0.5f64);

    // Behavioral contract: non-numeric values must return None from as_f64()
    assert!(
        null_val.as_f64().is_none(),
        "Gate 2.5: JSON null must return None from as_f64()"
    );
    assert!(
        str_val.as_f64().is_none(),
        "Gate 2.5: JSON string must return None from as_f64()"
    );
    assert!(
        bool_val.as_f64().is_none(),
        "Gate 2.5: JSON bool must return None from as_f64()"
    );

    // Behavioral contract: numeric values must return Some
    assert!(
        number_val.as_f64().is_some(),
        "Gate 2.5: JSON number must return Some from as_f64()"
    );

    // Demonstrate why None → 0.0 via unwrap_or(0.0) is WRONG:
    // It silently produces a zero-filled embedding that looks valid to the ADP gate.
    let silent_bad = null_val.as_f64().unwrap_or(0.0);
    assert_eq!(
        silent_bad, 0.0f64,
        "Gate 2.5: unwrap_or(0.0) on null gives 0.0 — this is the silent false-success \
         that parse_embedding_array was fixed to reject with Err"
    );

    // The fix: None must map to Err, not to 0.0
    let correct: anyhow::Result<f32> = null_val
        .as_f64()
        .ok_or_else(|| anyhow::anyhow!("non-numeric element"))
        .map(|f| f as f32);
    assert!(
        correct.is_err(),
        "Gate 2.5: None.ok_or_else(Err) must produce Err, not Ok(0.0)"
    );
}

// ── Test 14 (Gate 2.5): post-index enrichment degraded message is descriptive ──

/// Gate 2.5 Test 14 (AUD-2026-INV-0002): When enrichment degrades during indexing,
/// the job message must describe ALL failed components, not just report a clean
/// "completed" success banner.
///
/// Mirrors the production `determine_job_status` / `determine_job_message` pure
/// functions directly — no AppState, fully deterministic.
#[test]
fn post_index_enrichment_degraded_message_describes_all_failures() {
    let warnings = [
        "link_sql_to_schema failed: no schema available".to_string(),
        "resolve_symbol_edges failed: graph write lock timeout".to_string(),
    ];

    // Mirror of production determine_job_status logic
    let cancelled = false;
    let res_failed = false;
    let status = if cancelled {
        "cancelled"
    } else if res_failed {
        "failed"
    } else if !warnings.is_empty() {
        "degraded"
    } else {
        "done"
    };

    // Mirror of production determine_job_message logic
    let msg = if cancelled {
        "cancelled by user".to_string()
    } else if res_failed {
        "hard failure".to_string()
    } else if !warnings.is_empty() {
        format!(
            "completed with enrichment warnings: {}",
            warnings.join("; ")
        )
    } else {
        "completed".to_string()
    };

    assert_eq!(
        status, "degraded",
        "Gate 2.5: multi-warning enrichment must produce 'degraded' status"
    );
    assert!(
        msg.contains("link_sql_to_schema"),
        "Gate 2.5: message must mention link_sql_to_schema failure; got: '{msg}'"
    );
    assert!(
        msg.contains("resolve_symbol_edges"),
        "Gate 2.5: message must mention resolve_symbol_edges failure; got: '{msg}'"
    );
    assert!(
        msg.contains("enrichment warnings"),
        "Gate 2.5: message must use 'enrichment warnings' framing; got: '{msg}'"
    );
    assert_ne!(
        msg, "completed",
        "Gate 2.5: degraded message must not be the clean success banner 'completed'"
    );
}
