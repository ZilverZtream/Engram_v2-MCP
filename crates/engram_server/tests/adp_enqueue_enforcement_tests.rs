#![allow(clippy::unwrap_used)]
//! JOB1/ADP1 — end-to-end ADP deny→enqueue enforcement tests.
//!
//! Proves that:
//! 1. `evaluate_gates` + `apply_rollout_policy` correctly produces Deny verdicts
//!    for inputs that violate gate conditions.
//! 2. The kill-switch forces Deny regardless of per-gate outcomes.
//! 3. Phase-based enforcement (Guarded/Autonomous) blocks Deny verdicts while
//!    Advisory/Shadow override them to Allow.
//! 4. A Deny verdict from the full pipeline is unambiguous — no caller can
//!    mistake it for Allow — demonstrating that the enforcement contract is
//!    complete at the policy layer even though job creation tools are
//!    user/agent-triggered.

use engram_server::services::autonomous_decision_service::{
    AdpInput, AdpVerdict, RiskProfile, RolloutPhase,
    apply_rollout_policy, evaluate_gates,
};
use engram_server::services::safety_service::PolicyDecision;

/// Build a minimal AdpInput whose gates will all abstain (no evidence supplied).
fn abstain_input() -> AdpInput {
    AdpInput {
        extraction_confidence: None,
        extraction_band: None,
        trace_used_fallback: false,
        trace_candidate_count: 0,
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
        risk_profile: RiskProfile::Low,
        min_extraction_confidence: 0.7,
        min_safety_confidence: 0.6,
        max_blast_radius_for_auto: 5,
        reconciliation: None,
        graph_impact: None,
        retrieval_mode: engram_server::services::autonomous_decision_service::RetrievalMode::Skipped,
        migration_class: None,
    }
}

/// Build a deny-triggering AdpInput: safety policy explicitly fails.
fn deny_input() -> AdpInput {
    AdpInput {
        extraction_confidence: Some(0.9),
        extraction_band: Some("high".into()),
        trace_used_fallback: false,
        trace_candidate_count: 1,
        safety_decision: Some(PolicyDecision {
            allowed: false,
            risk_level: engram_server::services::safety_service::RiskLevel::Critical,
            checks: vec![],
            confidence: 0.95,
            summary: "Policy BLOCK: destructive schema change".into(),
            mitigations: vec![],
        }),
        retrieval_production_ready: Some(true),
        retrieval_ndcg: Some(0.85),
        retrieval_recall: Some(0.90),
        blast_radius_risk: Some(3),
        blast_radius_band: None,
        blast_radius_downstream: Some(5),
        immune_verdict: Some("PASS".into()),
        immune_confidence: Some(0.92),
        require_runtime_evidence: false,
        has_runtime_evidence: false,
        risk_profile: RiskProfile::Low,
        min_extraction_confidence: 0.7,
        min_safety_confidence: 0.6,
        max_blast_radius_for_auto: 5,
        reconciliation: None,
        graph_impact: None,
        retrieval_mode: engram_server::services::autonomous_decision_service::RetrievalMode::Skipped,
        migration_class: None,
    }
}

/// Build an allow-producing AdpInput: all evidence present and passing.
fn allow_input() -> AdpInput {
    AdpInput {
        extraction_confidence: Some(0.95),
        extraction_band: Some("high".into()),
        trace_used_fallback: false,
        trace_candidate_count: 1,
        safety_decision: Some(PolicyDecision {
            allowed: true,
            risk_level: engram_server::services::safety_service::RiskLevel::Low,
            checks: vec![],
            confidence: 0.98,
            summary: "Policy ALLOW: low-risk isolated change".into(),
            mitigations: vec![],
        }),
        retrieval_production_ready: Some(true),
        retrieval_ndcg: Some(0.9),
        retrieval_recall: Some(0.92),
        blast_radius_risk: Some(2),
        blast_radius_band: None,
        blast_radius_downstream: Some(3),
        immune_verdict: Some("PASS".into()),
        immune_confidence: Some(0.97),
        require_runtime_evidence: false,
        has_runtime_evidence: false,
        risk_profile: RiskProfile::Low,
        min_extraction_confidence: 0.7,
        min_safety_confidence: 0.6,
        max_blast_radius_for_auto: 5,
        reconciliation: None,
        graph_impact: None,
        retrieval_mode: engram_server::services::autonomous_decision_service::RetrievalMode::Skipped,
        migration_class: None,
    }
}

// ── Test 1: safety policy hard-deny propagates through full pipeline ──────────

/// JOB1/ADP1: A safety-BLOCK verdict must propagate as AdpVerdict::Deny through
/// the full evaluate_gates → apply_rollout_policy pipeline in Guarded mode.
/// No job creation path can reach an autonomous "allow" from these inputs.
#[test]
fn adp_safety_deny_propagates_through_full_pipeline_guarded() {
    let input = deny_input();
    let raw = evaluate_gates(&input);
    assert_eq!(
        raw.verdict,
        AdpVerdict::Deny,
        "JOB1: safety BLOCK must produce Deny from evaluate_gates"
    );

    let enforced = apply_rollout_policy(&raw, RolloutPhase::Guarded, false);
    assert_eq!(
        enforced.verdict,
        AdpVerdict::Deny,
        "JOB1: Deny must survive apply_rollout_policy in Guarded phase"
    );
    assert!(
        !enforced.reasons.is_empty(),
        "JOB1: Deny verdict must carry at least one reason"
    );
}

/// JOB1/ADP1: Same deny pipeline in Autonomous mode also produces Deny.
#[test]
fn adp_safety_deny_propagates_in_autonomous_mode() {
    let input = deny_input();
    let raw = evaluate_gates(&input);
    let enforced = apply_rollout_policy(&raw, RolloutPhase::Autonomous, false);
    assert_eq!(
        enforced.verdict,
        AdpVerdict::Deny,
        "JOB1: Deny must survive apply_rollout_policy in Autonomous phase"
    );
}

// ── Test 2: kill-switch forces Deny regardless of per-gate outcomes ───────────

/// JOB1/ADP1: Kill-switch ON must override an Allow verdict to Deny.
/// This proves no autonomous job can be created while kill-switch is active.
#[test]
fn adp_kill_switch_overrides_allow_to_deny() {
    let input = allow_input();
    let raw = evaluate_gates(&input);
    // Without kill-switch, this input should allow.
    let normal = apply_rollout_policy(&raw, RolloutPhase::Autonomous, false);
    assert_eq!(
        normal.verdict,
        AdpVerdict::Allow,
        "JOB1: precondition — allow-input must produce Allow without kill-switch"
    );

    // With kill-switch, must be Deny.
    let blocked = apply_rollout_policy(&raw, RolloutPhase::Autonomous, true);
    assert_eq!(
        blocked.verdict,
        AdpVerdict::Deny,
        "JOB1: kill-switch must override Allow → Deny"
    );
    assert!(
        blocked.reasons.iter().any(|r| r.contains("kill-switch")),
        "JOB1: kill-switch Deny reason must mention kill-switch"
    );
}

/// JOB1/ADP1: Kill-switch ON must override a Deny verdict too (already Deny,
/// but the reason should be kill-switch, not the original gate failure).
#[test]
fn adp_kill_switch_overrides_deny_reason() {
    let input = deny_input();
    let raw = evaluate_gates(&input);
    let blocked = apply_rollout_policy(&raw, RolloutPhase::Guarded, true);
    assert_eq!(blocked.verdict, AdpVerdict::Deny);
    assert!(
        blocked.reasons.iter().any(|r| r.contains("kill-switch")),
        "JOB1: kill-switch must be cited in Deny reasons"
    );
}

// ── Test 3: Advisory/Shadow phases override Deny → Allow ─────────────────────

/// JOB1/ADP1: In Advisory phase, Deny is overridden to Allow with a warning tag.
/// This is the expected behavior for non-blocking rollout stages.
#[test]
fn adp_advisory_phase_overrides_deny_to_allow() {
    let input = deny_input();
    let raw = evaluate_gates(&input);
    assert_eq!(raw.verdict, AdpVerdict::Deny, "precondition");

    let advisory = apply_rollout_policy(&raw, RolloutPhase::Advisory, false);
    assert_eq!(
        advisory.verdict,
        AdpVerdict::Allow,
        "JOB1: Advisory phase must override Deny → Allow"
    );
    assert!(
        advisory.reasons.iter().any(|r| r.contains("[ADVISORY]")),
        "JOB1: Advisory override must be tagged in reasons"
    );
}

/// JOB1/ADP1: In Shadow phase, Deny is overridden to Allow with a shadow tag.
#[test]
fn adp_shadow_phase_overrides_deny_to_allow() {
    let input = deny_input();
    let raw = evaluate_gates(&input);
    let shadow = apply_rollout_policy(&raw, RolloutPhase::Shadow, false);
    assert_eq!(
        shadow.verdict,
        AdpVerdict::Allow,
        "JOB1: Shadow phase must override Deny → Allow"
    );
    assert!(
        shadow.reasons.iter().any(|r| r.contains("[SHADOW]")),
        "JOB1: Shadow override must be tagged in reasons"
    );
}

// ── Test 4: Allow verdict passes through Guarded/Autonomous unmodified ────────

/// JOB1/ADP1: An Allow verdict in Guarded mode passes through unchanged.
/// Proves the guard does not block valid autonomous actions.
#[test]
fn adp_allow_passes_through_guarded_mode() {
    let input = allow_input();
    let raw = evaluate_gates(&input);
    let enforced = apply_rollout_policy(&raw, RolloutPhase::Guarded, false);
    assert_eq!(
        enforced.verdict,
        AdpVerdict::Allow,
        "JOB1: Allow verdict must pass through Guarded phase unmodified"
    );
}

// ── Test 5: Abstain inputs produce non-Allow in Guarded mode ─────────────────

/// JOB1/ADP1: When no evidence is supplied, the verdict is Abstain or Deny,
/// never Allow — proving incomplete evidence cannot trigger autonomous execution.
#[test]
fn adp_abstain_inputs_never_produce_allow_in_guarded_mode() {
    let input = abstain_input();
    let raw = evaluate_gates(&input);
    // Abstain or Deny — either is fine, just not Allow.
    assert_ne!(
        raw.verdict,
        AdpVerdict::Allow,
        "JOB1: zero-evidence input must not produce Allow"
    );

    let enforced = apply_rollout_policy(&raw, RolloutPhase::Guarded, false);
    assert_ne!(
        enforced.verdict,
        AdpVerdict::Allow,
        "JOB1: zero-evidence input must not produce Allow after policy application in Guarded mode"
    );
}

// ── Test 6: Verdict is unambiguous — Deny is distinguishable from Allow ───────

/// JOB1/ADP1: The Deny verdict renders to a distinct string from Allow.
/// Any caller that text-matches or enum-matches the verdict cannot confuse them.
#[test]
fn adp_deny_verdict_is_unambiguous() {
    let deny = AdpVerdict::Deny;
    let allow = AdpVerdict::Allow;
    assert_ne!(
        format!("{deny}"),
        format!("{allow}"),
        "JOB1: Deny and Allow must render as distinct strings"
    );
    assert_ne!(deny, allow, "JOB1: Deny and Allow must be distinct enum variants");
}

// ── Test 7: Individual gate hard-deny paths ───────────────────────────────────

/// Blast radius exceeding max_blast_radius_for_auto must produce Deny in Guarded mode.
/// Gate 5 is a hard-deny path: any change with blast_radius_risk > threshold is
/// blocked unconditionally. Proves this gate cannot be bypassed by other passing gates.
#[test]
fn blast_radius_above_threshold_produces_deny_in_guarded_mode() {
    let input = AdpInput {
        extraction_confidence: Some(0.95),
        extraction_band: Some("high".into()),
        trace_used_fallback: false,
        trace_candidate_count: 1,
        safety_decision: Some(engram_server::services::safety_service::PolicyDecision {
            allowed: true,
            risk_level: engram_server::services::safety_service::RiskLevel::Low,
            checks: vec![],
            confidence: 0.98,
            summary: "ALLOW".into(),
            mitigations: vec![],
        }),
        retrieval_production_ready: Some(true),
        retrieval_ndcg: Some(0.9),
        retrieval_recall: Some(0.9),
        // blast_radius_risk=9 exceeds max_blast_radius_for_auto=5 → gate 5 hard-deny
        blast_radius_risk: Some(9),
        blast_radius_band: None,
        blast_radius_downstream: Some(20),
        immune_verdict: Some("PASS".into()),
        immune_confidence: Some(0.95),
        require_runtime_evidence: false,
        has_runtime_evidence: false,
        risk_profile: RiskProfile::Low,
        min_extraction_confidence: 0.7,
        min_safety_confidence: 0.6,
        max_blast_radius_for_auto: 5,
        reconciliation: None,
        graph_impact: None,
        retrieval_mode: engram_server::services::autonomous_decision_service::RetrievalMode::Skipped,
        migration_class: None,
    };

    let raw = evaluate_gates(&input);
    assert_eq!(
        raw.verdict,
        AdpVerdict::Deny,
        "Gate 5: blast_radius_risk=9 > max=5 must produce Deny; \
         an over-blast-radius change cannot auto-proceed regardless of other gates"
    );

    let enforced = apply_rollout_policy(&raw, RolloutPhase::Guarded, false);
    assert_eq!(
        enforced.verdict,
        AdpVerdict::Deny,
        "Gate 5 Deny must survive apply_rollout_policy in Guarded mode"
    );
    assert!(
        enforced.failed_gates.iter().any(|g| g.contains("blast")),
        "blast_radius gate must appear in failed_gates; got: {:?}",
        enforced.failed_gates
    );
}

/// Extraction confidence below threshold must produce Deny in Guarded mode.
/// Gate 1 is a hard-deny path when confidence evidence is present but insufficient.
/// Proves that a change cannot proceed when evidence quality is below threshold.
#[test]
fn low_extraction_confidence_produces_deny_in_guarded_mode() {
    let input = AdpInput {
        // confidence=0.4 < min_extraction_confidence=0.7 → gate 1 hard-deny
        extraction_confidence: Some(0.4),
        extraction_band: Some("low".into()),
        trace_used_fallback: false,
        trace_candidate_count: 1,
        safety_decision: Some(engram_server::services::safety_service::PolicyDecision {
            allowed: true,
            risk_level: engram_server::services::safety_service::RiskLevel::Low,
            checks: vec![],
            confidence: 0.98,
            summary: "ALLOW".into(),
            mitigations: vec![],
        }),
        retrieval_production_ready: Some(true),
        retrieval_ndcg: Some(0.9),
        retrieval_recall: Some(0.9),
        blast_radius_risk: Some(2),
        blast_radius_band: None,
        blast_radius_downstream: Some(3),
        immune_verdict: Some("PASS".into()),
        immune_confidence: Some(0.95),
        require_runtime_evidence: false,
        has_runtime_evidence: false,
        risk_profile: RiskProfile::Low,
        min_extraction_confidence: 0.7,
        min_safety_confidence: 0.6,
        max_blast_radius_for_auto: 5,
        reconciliation: None,
        graph_impact: None,
        retrieval_mode: engram_server::services::autonomous_decision_service::RetrievalMode::Skipped,
        migration_class: None,
    };

    let raw = evaluate_gates(&input);
    assert_ne!(
        raw.verdict,
        AdpVerdict::Allow,
        "Gate 1: extraction_confidence=0.4 < threshold=0.7 must not produce Allow"
    );

    let enforced = apply_rollout_policy(&raw, RolloutPhase::Guarded, false);
    assert_ne!(
        enforced.verdict,
        AdpVerdict::Allow,
        "Low extraction confidence must not be allow-able in Guarded mode"
    );
}

/// BLOCK immune verdict must produce Deny in Guarded mode.
/// Gate 6 is a hard-deny path when the anti-pattern check returns BLOCK.
/// Proves that immune-blocked changes cannot auto-proceed regardless of other gates.
#[test]
fn immune_block_verdict_produces_deny_in_guarded_mode() {
    let input = AdpInput {
        extraction_confidence: Some(0.95),
        extraction_band: Some("high".into()),
        trace_used_fallback: false,
        trace_candidate_count: 1,
        safety_decision: Some(engram_server::services::safety_service::PolicyDecision {
            allowed: true,
            risk_level: engram_server::services::safety_service::RiskLevel::Low,
            checks: vec![],
            confidence: 0.98,
            summary: "ALLOW".into(),
            mitigations: vec![],
        }),
        retrieval_production_ready: Some(true),
        retrieval_ndcg: Some(0.9),
        retrieval_recall: Some(0.9),
        blast_radius_risk: Some(2),
        blast_radius_band: None,
        blast_radius_downstream: Some(3),
        // BLOCK verdict → gate 6 hard-deny
        immune_verdict: Some("BLOCK".into()),
        immune_confidence: Some(0.90),
        require_runtime_evidence: false,
        has_runtime_evidence: false,
        risk_profile: RiskProfile::Low,
        min_extraction_confidence: 0.7,
        min_safety_confidence: 0.6,
        max_blast_radius_for_auto: 5,
        reconciliation: None,
        graph_impact: None,
        retrieval_mode: engram_server::services::autonomous_decision_service::RetrievalMode::Skipped,
        migration_class: None,
    };

    let raw = evaluate_gates(&input);
    assert_eq!(
        raw.verdict,
        AdpVerdict::Deny,
        "Gate 6: immune_verdict=BLOCK must produce Deny; \
         an anti-pattern blocked change must not auto-proceed"
    );

    let enforced = apply_rollout_policy(&raw, RolloutPhase::Guarded, false);
    assert_eq!(
        enforced.verdict,
        AdpVerdict::Deny,
        "BLOCK immune verdict Deny must survive Guarded mode policy application"
    );
    assert!(
        enforced.failed_gates.iter().any(|g| g.contains("anti_pattern") || g.contains("immune")),
        "anti_pattern gate must appear in failed_gates; got: {:?}",
        enforced.failed_gates
    );
}

// ── Test 8: Wave-level deny propagation ───────────────────────────────────────

/// A wave containing one deny-producing item must produce a wave-level Deny.
/// Proves that evaluate_wave propagates any item Deny to the overall wave verdict —
/// there is no way for a single deny-blocked file to be "outvoted" by other Allow items.
#[test]
fn wave_with_one_deny_item_produces_wave_deny() {
    use engram_server::services::autonomous_decision_service::{WaveAdpInput, evaluate_wave};

    let wave_input = WaveAdpInput {
        wave_number: 1,
        wave_name: "wave-1-mixed".into(),
        items: vec![
            ("file_a.cs".into(), allow_input()),
            ("file_b.cs".into(), deny_input()), // safety BLOCK → Deny
            ("file_c.cs".into(), allow_input()),
        ],
        cross_item_deps: 0,
    };

    let wave_decision = evaluate_wave(&wave_input);

    assert_eq!(
        wave_decision.verdict,
        AdpVerdict::Deny,
        "evaluate_wave: one deny-producing item must block the entire wave; \
         a single unsafe file must not be auto-applied even if all others are safe"
    );
    assert!(
        wave_decision.blocking_items.contains(&"file_b.cs".to_string()),
        "blocking_items must identify the deny-producing file; \
         got: {:?}",
        wave_decision.blocking_items
    );
}
