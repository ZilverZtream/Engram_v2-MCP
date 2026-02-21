//! ADP vNext integration tests.
//!
//! Validates the evolved gate pipeline, calibrated confidence aggregation,
//! reconciliation-aware runtime gate, wave-level evaluation, and backward
//! compatibility with v1 request shapes.

use engram_server::services::autonomous_decision_service::*;
use engram_server::services::safety_service::{PolicyDecision, RiskLevel};

// ── Helpers ──────────────────────────────────────────────────────────────────

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

fn unsafe_policy() -> PolicyDecision {
    PolicyDecision {
        allowed: false,
        risk_level: RiskLevel::High,
        checks: vec![],
        confidence: 0.3,
        summary: "Unsafe".into(),
        mitigations: vec!["review required".into()],
    }
}

/// Build a minimal AdpInput where all gates pass with high confidence.
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

// ── Test: backward compat with v1 inputs ─────────────────────────────────────

/// A v1-style input (no reconciliation, no graph_impact, no migration_class)
/// should still produce an Allow verdict when all gates pass.
#[test]
fn backward_compat_v1_input_produces_allow() {
    let input = all_green_input();
    let decision = evaluate_gates(&input);
    assert_eq!(
        decision.verdict,
        AdpVerdict::Allow,
        "v1-style input with all-green gates should Allow"
    );
    assert!(decision.confidence > 0.5, "confidence should be meaningful");
}

// ── Test: reconciliation upgrades runtime gate ───────────────────────────────

/// When reconciliation scores are provided, they should be used instead
/// of the boolean `has_runtime_evidence`.
#[test]
fn reconciliation_scores_upgrade_runtime_gate() {
    let mut input = all_green_input();
    input.require_runtime_evidence = true;
    input.has_runtime_evidence = false; // boolean says no
    input.reconciliation = Some(ReconciliationScores {
        confirmed_ratio: 0.90,
        contradicted_ratio: 0.02,
        confidence_delta: 0.15,
        static_paths_count: 50,
    });
    let decision = evaluate_gates(&input);
    // Reconciliation has high confirmed → runtime gate should pass
    let runtime_gate = decision
        .gate_results
        .iter()
        .find(|g| g.gate_id == "runtime_evidence")
        .expect("runtime_evidence gate should exist");
    assert!(
        runtime_gate.passed,
        "reconciliation with high confirmed ratio should pass runtime gate"
    );
    // Reconciliation confidence: 0.90*0.7 - 0.02*0.5 + 0.15*0.3 = 0.665
    assert!(
        runtime_gate.confidence > 0.6,
        "reconciliation-derived confidence ({}) should exceed threshold 0.6",
        runtime_gate.confidence
    );
}

/// When reconciliation shows high contradictions, runtime gate should fail.
#[test]
fn high_contradictions_fail_runtime_gate() {
    let mut input = all_green_input();
    input.require_runtime_evidence = true;
    input.has_runtime_evidence = true; // boolean says yes, but reconciliation overrides
    input.reconciliation = Some(ReconciliationScores {
        confirmed_ratio: 0.15,
        contradicted_ratio: 0.45,
        confidence_delta: -0.10,
        static_paths_count: 40,
    });
    let decision = evaluate_gates(&input);
    let runtime_gate = decision
        .gate_results
        .iter()
        .find(|g| g.gate_id == "runtime_evidence")
        .expect("runtime_evidence gate should exist");
    assert!(
        !runtime_gate.passed,
        "high contradictions should fail runtime gate even with has_runtime_evidence=true"
    );
}

// ── Test: retrieval mode affects confidence ──────────────────────────────────

/// Cached retrieval results should receive a staleness discount.
#[test]
fn cached_retrieval_gets_staleness_discount() {
    let mut live_input = all_green_input();
    live_input.retrieval_mode = RetrievalMode::Live;

    let mut cached_input = all_green_input();
    cached_input.retrieval_mode = RetrievalMode::Cached;

    let live_decision = evaluate_gates(&live_input);
    let cached_decision = evaluate_gates(&cached_input);

    let live_ret = live_decision
        .gate_results
        .iter()
        .find(|g| g.gate_id == "retrieval_quality")
        .unwrap();
    let cached_ret = cached_decision
        .gate_results
        .iter()
        .find(|g| g.gate_id == "retrieval_quality")
        .unwrap();

    assert!(
        cached_ret.confidence < live_ret.confidence,
        "cached retrieval confidence ({}) should be less than live ({})",
        cached_ret.confidence,
        live_ret.confidence
    );
}

/// Skipped retrieval mode should mark the gate as skipped.
#[test]
fn skipped_retrieval_marks_gate_skipped() {
    let mut input = all_green_input();
    input.retrieval_production_ready = None;
    input.retrieval_ndcg = None;
    input.retrieval_recall = None;
    input.retrieval_mode = RetrievalMode::Skipped;

    let decision = evaluate_gates(&input);
    let ret_gate = decision
        .gate_results
        .iter()
        .find(|g| g.gate_id == "retrieval_quality")
        .unwrap();
    assert!(
        ret_gate.skipped,
        "Skipped retrieval mode → gate should be skipped"
    );
}

// ── Test: calibrated confidence with class adjustments ───────────────────────

/// A `data_access` migration class should yield lower confidence than the
/// same gates with no class (due to -0.05 class adjustment).
#[test]
fn data_access_class_yields_lower_confidence() {
    let base = all_green_input();
    let mut data_access = all_green_input();
    data_access.migration_class = Some("data_access".into());

    let base_decision = evaluate_gates(&base);
    let da_decision = evaluate_gates(&data_access);

    assert!(
        da_decision.confidence < base_decision.confidence,
        "data_access class ({}) should have lower confidence than default ({})",
        da_decision.confidence,
        base_decision.confidence
    );
}

/// A `static_asset` migration class should yield higher confidence.
#[test]
fn static_asset_class_yields_higher_confidence() {
    let base = all_green_input();
    let mut static_input = all_green_input();
    static_input.migration_class = Some("static_asset".into());

    let base_decision = evaluate_gates(&base);
    let static_decision = evaluate_gates(&static_input);

    assert!(
        static_decision.confidence > base_decision.confidence,
        "static_asset class ({}) should have higher confidence than default ({})",
        static_decision.confidence,
        base_decision.confidence
    );
}

// ── Test: interaction penalty ────────────────────────────────────────────────

/// When both safety AND blast radius gates fail, the interaction penalty
/// should reduce overall confidence further.
#[test]
fn safety_blast_interaction_penalty_reduces_confidence() {
    // Build input where safety fails
    let mut input = all_green_input();
    input.safety_decision = Some(unsafe_policy());
    // And blast radius is high
    input.blast_radius_risk = Some(9);
    input.blast_radius_band =
        Some(engram_server::services::blast_radius_service::RiskBand::Critical);
    input.blast_radius_downstream = Some(50);

    let decision = evaluate_gates(&input);
    // Should be Deny (both hard gates failed)
    assert_eq!(decision.verdict, AdpVerdict::Deny);
    // Confidence should be low due to multiple failures + interaction penalty
    assert!(
        decision.confidence < 0.6,
        "interaction penalty should keep confidence below 0.6, got {}",
        decision.confidence
    );
}

// ── Test: wave-level evaluation ──────────────────────────────────────────────

/// A wave where all items pass should produce Allow.
#[test]
fn wave_all_allow_produces_allow() {
    let items: Vec<(String, AdpInput)> = (0..3)
        .map(|i| (format!("file_{i}.cs"), all_green_input()))
        .collect();
    let wave = WaveAdpInput {
        wave_number: 1,
        wave_name: "Wave 1".into(),
        items,
        cross_item_deps: 0,
    };
    let decision = evaluate_wave(&wave);
    assert_eq!(decision.verdict, AdpVerdict::Allow);
    assert_eq!(decision.item_decisions.len(), 3);
    assert!(decision.blocking_items.is_empty());
}

/// A single deny in a wave should veto the entire wave.
#[test]
fn wave_single_deny_vetoes_wave() {
    let mut items: Vec<(String, AdpInput)> = (0..3)
        .map(|i| (format!("file_{i}.cs"), all_green_input()))
        .collect();
    // Make the second item fail safety
    items[1].1.safety_decision = Some(unsafe_policy());
    let wave = WaveAdpInput {
        wave_number: 1,
        wave_name: "Wave 1".into(),
        items,
        cross_item_deps: 0,
    };
    let decision = evaluate_wave(&wave);
    assert_eq!(
        decision.verdict,
        AdpVerdict::Deny,
        "single deny should veto wave"
    );
    assert!(!decision.blocking_items.is_empty());
}

/// More than 3 items with high blast radius should trigger wave Abstain.
#[test]
fn wave_high_blast_count_shifts_to_abstain() {
    let mut items: Vec<(String, AdpInput)> = (0..5)
        .map(|i| (format!("file_{i}.cs"), all_green_input()))
        .collect();
    // Give 4 items high blast radius (> 5)
    for item in items.iter_mut().take(4) {
        item.1.blast_radius_risk = Some(6);
        item.1.blast_radius_band =
            Some(engram_server::services::blast_radius_service::RiskBand::High);
        item.1.blast_radius_downstream = Some(20);
    }
    let wave = WaveAdpInput {
        wave_number: 1,
        wave_name: "Wave 1".into(),
        items,
        cross_item_deps: 0,
    };
    let decision = evaluate_wave(&wave);
    assert_eq!(
        decision.verdict,
        AdpVerdict::Abstain,
        "4+ items with high blast should abstain, got {:?}",
        decision.verdict
    );
}

/// Wave format output should include wave number and verdict.
#[test]
fn wave_format_includes_key_info() {
    let items: Vec<(String, AdpInput)> = (0..2)
        .map(|i| (format!("file_{i}.cs"), all_green_input()))
        .collect();
    let wave = WaveAdpInput {
        wave_number: 3,
        wave_name: "Wave 3 - Data Layer".into(),
        items,
        cross_item_deps: 1,
    };
    let decision = evaluate_wave(&wave);
    let formatted = format_wave_decision(&decision);
    assert!(formatted.contains("Wave 3"), "should mention wave number");
    assert!(
        formatted.to_lowercase().contains("allow")
            || formatted.to_lowercase().contains("deny")
            || formatted.to_lowercase().contains("abstain"),
        "should contain verdict"
    );
}

// ── Test: graph impact metrics influence decisions ───────────────────────────

/// Providing GraphImpactMetrics should not break gate evaluation.
#[test]
fn graph_impact_metrics_are_accepted() {
    let mut input = all_green_input();
    input.graph_impact = Some(GraphImpactMetrics {
        downstream_dependency_count: 10,
        reads_state_count: 2,
        writes_state_count: 1,
        sql_calls_count: 5,
        queries_table_count: 3,
        injects_script_count: 0,
    });
    let decision = evaluate_gates(&input);
    // Should still Allow — graph_impact is informational for the EOE layer,
    // the pure gate pipeline uses the derived fields.
    assert_eq!(decision.verdict, AdpVerdict::Allow);
}

// ── Test: evidence depth enum ────────────────────────────────────────────────

#[test]
fn evidence_depth_from_str_parses_correctly() {
    use engram_server::services::evidence_orchestration::EvidenceDepth;

    assert_eq!(EvidenceDepth::from_str("fast"), EvidenceDepth::Fast);
    assert_eq!(EvidenceDepth::from_str("DEEP"), EvidenceDepth::Deep);
    assert_eq!(EvidenceDepth::from_str("standard"), EvidenceDepth::Standard);
    assert_eq!(
        EvidenceDepth::from_str("unknown"),
        EvidenceDepth::Standard,
        "unknown string should default to Standard"
    );
}
