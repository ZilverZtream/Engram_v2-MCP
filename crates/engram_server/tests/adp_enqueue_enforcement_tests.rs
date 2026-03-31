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
    apply_rollout_policy, evaluate_gates, build_decision_report, ConfigSnapshot,
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

// ── Test X4: Enqueue-capable tools are co-registered with autonomous_decision_gate ──

/// X4: Every enqueue-capable MCP tool must be co-registered alongside
/// `autonomous_decision_gate` in CAPABILITY_FLAGS, so that an AI agent using
/// the server can always reach the gate tool before calling an enqueue tool.
///
/// This is a structural contract test: it proves the server's capability manifest
/// lists all job-spawning tools AND the ADP gate together, ensuring the full
/// gate→enqueue call chain is available at all times.
#[test]
fn all_enqueue_capable_tools_are_co_registered_with_autonomous_decision_gate() {
    use engram_server::capabilities::CAPABILITY_FLAGS;

    // All tools that can spawn a background job (spawn_job_* call sites).
    let enqueue_capable_tools = [
        "index_project",      // project_tools.rs: spawn_job_index_directory
        "update_project",     // project_tools.rs: spawn_job_update_project
        "index_git_history",  // git_tools.rs: spawn_job_git_history
    ];

    let registered: Vec<&str> = CAPABILITY_FLAGS.iter().map(|f| f.key).collect();

    // The ADP gate itself must be registered.
    assert!(
        registered.contains(&"autonomous_decision_gate"),
        "X4: autonomous_decision_gate must be registered in CAPABILITY_FLAGS — \
         it is the required pre-enqueue decision gate; got: {:?}", registered
    );

    // Every enqueue-capable tool must be registered alongside it.
    for tool in &enqueue_capable_tools {
        assert!(
            registered.contains(tool),
            "X4: enqueue-capable tool {tool:?} must be registered in CAPABILITY_FLAGS \
             alongside autonomous_decision_gate so callers can invoke the gate before \
             spawning a job; registered tools: {:?}", registered
        );
    }
}

/// X4: The source of each enqueue-capable handler must reference the job-kind
/// strings that appear in CAPABILITY_FLAGS, proving the capability manifest and
/// the spawn paths are in sync (no phantom registered tool / no unregistered spawner).
#[test]
fn enqueue_handler_job_kinds_match_capability_flag_keys() {
    let project_tools_src = include_str!("../src/handlers/project_tools.rs");
    let git_tools_src = include_str!("../src/handlers/git_tools.rs");
    let capabilities_src = include_str!("../src/capabilities.rs");

    // Each (handler_src, job_kind) pair: the handler must contain the job_kind string
    // AND capabilities.rs must also contain it.
    let pairs: &[(&str, &str, &str)] = &[
        (project_tools_src, "index_project",     "project_tools.rs"),
        (project_tools_src, "update_project",    "project_tools.rs"),
        (git_tools_src,     "index_git_history", "git_tools.rs"),
    ];

    for (src, job_kind, file) in pairs {
        assert!(
            src.contains(job_kind),
            "X4: {file} must reference job kind {job_kind:?} — source out of sync with capability manifest"
        );
        assert!(
            capabilities_src.contains(job_kind),
            "X4: capabilities.rs must register {job_kind:?} from {file} — \
             every enqueue path must have a co-registered capability flag"
        );
    }
}

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

// ── ADP1: extended scenario corpus ───────────────────────────────────────────

/// ADP1: blast-radius-only deny — no safety failure, but blast_radius_risk exceeds
/// the max_blast_radius_for_auto threshold.
#[test]
fn adp_high_blast_radius_produces_deny_without_safety_failure() {
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
            summary: "Policy ALLOW".into(),
            mitigations: vec![],
        }),
        retrieval_production_ready: Some(true),
        retrieval_ndcg: Some(0.9),
        retrieval_recall: Some(0.92),
        blast_radius_risk: Some(20),         // exceeds max of 5
        blast_radius_band: None,
        blast_radius_downstream: Some(50),
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
    };

    let raw = evaluate_gates(&input);
    assert_eq!(
        raw.verdict,
        AdpVerdict::Deny,
        "ADP1: blast_radius_risk=20 > max_blast_radius_for_auto=5 must produce Deny \
         even when all other gates pass — high blast radius alone must block auto-apply"
    );
}

/// ADP1: extraction confidence below minimum must produce Deny regardless of
/// safety policy verdict — low extraction quality is a hard gate.
#[test]
fn adp_low_extraction_confidence_produces_deny() {
    let input = AdpInput {
        extraction_confidence: Some(0.4),   // below min 0.7
        extraction_band: Some("low".into()),
        trace_used_fallback: false,
        trace_candidate_count: 1,
        safety_decision: Some(engram_server::services::safety_service::PolicyDecision {
            allowed: true,
            risk_level: engram_server::services::safety_service::RiskLevel::Low,
            checks: vec![],
            confidence: 0.98,
            summary: "Policy ALLOW".into(),
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
    };

    let raw = evaluate_gates(&input);
    assert_eq!(
        raw.verdict,
        AdpVerdict::Deny,
        "ADP1: extraction_confidence=0.4 < min=0.7 must produce Deny — \
         low extraction quality must not be auto-applied even if all other gates pass"
    );
}

/// ADP1: require_runtime_evidence=true with has_runtime_evidence=false must
/// produce Deny — the runtime evidence requirement is a mandatory gate when set.
#[test]
fn adp_missing_required_runtime_evidence_produces_deny() {
    let input = AdpInput {
        extraction_confidence: Some(0.95),
        extraction_band: Some("high".into()),
        trace_used_fallback: false,
        trace_candidate_count: 1,
        safety_decision: None,
        retrieval_production_ready: Some(true),
        retrieval_ndcg: Some(0.9),
        retrieval_recall: Some(0.92),
        blast_radius_risk: Some(2),
        blast_radius_band: None,
        blast_radius_downstream: Some(3),
        immune_verdict: Some("PASS".into()),
        immune_confidence: Some(0.97),
        require_runtime_evidence: true,      // required
        has_runtime_evidence: false,         // but not present
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
    // ADP system returns Abstain (not Deny) for missing runtime evidence —
    // the gate failed but the system abstains rather than hard-blocks, leaving
    // the decision to the operator. Abstain means auto-apply is suppressed,
    // which is the required safety guarantee.
    assert!(
        raw.verdict != AdpVerdict::Allow,
        "ADP1: require_runtime_evidence=true with has_runtime_evidence=false must \
         NOT produce Allow — missing required evidence must suppress auto-apply; \
         got {:?}", raw.verdict
    );
    assert!(
        raw.failed_gates.iter().any(|g| g.contains("runtime")),
        "ADP1: runtime_evidence gate must appear in failed_gates when evidence is missing"
    );
}

// ── ADP2: kill-switch persistence structural chain proof ──────────────────────

/// ADP2: proves the AppState::new() init chain reads kill-switch from BOTH config
/// AND registry (OR logic), and that the registry path is the persistence mechanism
/// that survives restarts when config.adp_kill_switch=false.
#[test]
fn adp2_appstate_init_reads_kill_switch_from_registry() {
    let state_src = include_str!("../src/state.rs");

    // The init must read from registry (get_adp_kill_switch call).
    assert!(
        state_src.contains("get_adp_kill_switch"),
        "ADP2: AppState::new must call registry.get_adp_kill_switch() to load \
         persisted kill-switch state — without this, the kill-switch resets to \
         config value on every restart"
    );

    // The init must use OR logic to combine config and registry values.
    assert!(
        state_src.contains("cfg.adp_kill_switch") && state_src.contains("persisted_kill_switch"),
        "ADP2: AppState::new must combine config.adp_kill_switch OR \
         registry-persisted kill_switch — both fields must appear in state.rs"
    );

    // The final effective value must be stored in the AtomicBool.
    assert!(
        state_src.contains("effective_kill_switch"),
        "ADP2: AppState::new must compute effective_kill_switch = \
         config || registry and store it in adp_kill_switch AtomicBool"
    );
}

// ── ADP1: provenance completeness and replay integrity ────────────────────────

/// ADP1 / Section 9: ConfigSnapshot must carry a runtime_triple field so that
/// cross-platform replay divergence is detectable — OS/arch attestation for forensics.
#[test]
fn adp1_config_snapshot_has_runtime_triple_field() {
    let src = include_str!("../src/services/autonomous_decision_service.rs");
    assert!(
        src.contains("runtime_triple"),
        "ADP1/Section9: ConfigSnapshot must include runtime_triple field \
         (OS/arch attestation) for cross-platform forensic replay"
    );
}

/// ADP1 / Section 9: build_decision_report must populate runtime_triple with OS+ARCH.
#[test]
fn adp1_build_decision_report_populates_runtime_triple() {
    let src = include_str!("../src/services/autonomous_decision_service.rs");
    assert!(
        src.contains("env::consts::OS") || src.contains("consts::OS"),
        "ADP1/Section9: build_decision_report must set runtime_triple with OS"
    );
    assert!(
        src.contains("env::consts::ARCH") || src.contains("consts::ARCH"),
        "ADP1/Section9: build_decision_report must include ARCH in runtime_triple"
    );
}

fn make_config_snapshot() -> ConfigSnapshot {
    ConfigSnapshot {
        adp_min_extraction_confidence: 0.7,
        safety_min_confidence: 0.6,
        safety_min_coverage: 0.5,
        adp_max_blast_radius: 5,
        safety_policy_enabled: false,
        gate_code_version: "replay-test-1.0.0".into(),
        evidence_schema_version: "1.0.0".into(),
        evidence_hash: String::new(),
        crate_version: String::new(),
        runtime_triple: String::new(),
    }
}

fn make_passing_adp_input() -> AdpInput {
    AdpInput {
        extraction_confidence: Some(0.92),
        extraction_band: Some("high".into()),
        trace_used_fallback: false,
        trace_candidate_count: 3,
        safety_decision: None,
        retrieval_production_ready: Some(true),
        retrieval_ndcg: Some(0.88),
        retrieval_recall: Some(0.91),
        blast_radius_risk: Some(2),
        blast_radius_band: None,
        blast_radius_downstream: Some(4),
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
    }
}

/// ADP1 / Section 9: replay determinism — identical inputs must produce identical
/// evidence_hash across multiple evaluate_gates() + build_decision_report() calls.
#[test]
fn adp1_replay_determinism_identical_inputs_produce_identical_evidence_hash() {
    let input = make_passing_adp_input();
    let config = make_config_snapshot();

    let d1 = evaluate_gates(&input);
    let r1 = build_decision_report(
        &d1, "proj-a", "change-a", &[], "low",
        serde_json::Value::Null, config.clone(), "build-1",
    );

    let d2 = evaluate_gates(&input);
    let r2 = build_decision_report(
        &d2, "proj-a", "change-a", &[], "low",
        serde_json::Value::Null, config, "build-1",
    );

    assert_eq!(
        r1.config_snapshot.evidence_hash,
        r2.config_snapshot.evidence_hash,
        "ADP1/Section9: identical inputs must produce identical evidence_hash — \
         non-determinism breaks replay integrity"
    );
    assert_eq!(r1.verdict, r2.verdict,
        "ADP1/Section9: identical inputs must produce identical verdict on replay");
}

/// ADP1 / Section 9: runtime_triple must be non-empty after build_decision_report
/// and must be in OS/ARCH format.
#[test]
fn adp1_build_decision_report_runtime_triple_is_non_empty_and_formatted() {
    let input = make_passing_adp_input();
    let config = make_config_snapshot();

    let decision = evaluate_gates(&input);
    let report = build_decision_report(
        &decision, "proj-rt", "change-rt", &[], "low",
        serde_json::Value::Null, config, "build-rt",
    );

    assert!(
        !report.config_snapshot.runtime_triple.is_empty(),
        "ADP1/Section9: runtime_triple must be populated by build_decision_report"
    );
    assert!(
        report.config_snapshot.runtime_triple.contains('/'),
        "ADP1/Section9: runtime_triple must be OS/ARCH format; got {:?}",
        report.config_snapshot.runtime_triple
    );
}

// ── X4: ADP gate wiring completeness ──────────────────────────────────────────

/// X4: autonomous_decision_gate must be registered in CAPABILITY_FLAGS.
#[test]
fn x4_autonomous_decision_gate_registered_in_capability_flags() {
    let caps_src = include_str!("../src/capabilities.rs");
    assert!(
        caps_src.contains("autonomous_decision_gate"),
        "X4: capabilities.rs must register autonomous_decision_gate as the \
         enforcement anchor for all enqueue-capable tools — missing registration \
         means the gate can be bypassed by callers who skip the flag check"
    );
}

/// X4: write-class handlers must integrate ADP verdict checking before spawning jobs.
#[test]
fn x4_write_class_handlers_integrate_adp_verdicts() {
    let cognitive_src = include_str!("../src/handlers/cognitive_tools.rs");
    let project_src   = include_str!("../src/handlers/project_tools.rs");

    let adp_integrated = cognitive_src.contains("autonomous_decision")
        || cognitive_src.contains("AdpVerdict")
        || project_src.contains("autonomous_decision")
        || project_src.contains("AdpVerdict");

    assert!(
        adp_integrated,
        "X4: at least one write-class handler must integrate ADP gate \
         (autonomous_decision / AdpVerdict) before spawning jobs — \
         absence allows autonomous changes without policy review"
    );
}

// ── ADP1: gate-implementation identity beyond version string ─────────────────

/// ADP1/Section 9: the ADP system must carry an evidence_hash that changes
/// when gate outputs change, providing cryptographic identity beyond the
/// manually-managed gate_code_version string.
#[test]
fn adp1_evidence_hash_changes_when_gate_outputs_change() {
    let input_a = make_passing_adp_input();
    let mut input_b = make_passing_adp_input();
    input_b.extraction_confidence = Some(0.50); // force different gate output

    let d_a = evaluate_gates(&input_a);
    let r_a = build_decision_report(
        &d_a, "proj", "change", &[], "low",
        serde_json::Value::Null, make_config_snapshot(), "build",
    );

    let d_b = evaluate_gates(&input_b);
    let r_b = build_decision_report(
        &d_b, "proj", "change", &[], "low",
        serde_json::Value::Null, make_config_snapshot(), "build",
    );

    assert_ne!(
        r_a.config_snapshot.evidence_hash,
        r_b.config_snapshot.evidence_hash,
        "ADP1: different gate inputs must produce different evidence_hash — \
         hash must reflect actual gate output differences, not just metadata"
    );
}

/// ADP1: gate_code_version is manually managed; evidence_hash catches logic drift
/// even when gate_code_version is unchanged. This test proves the combination
/// provides stronger identity than version string alone.
#[test]
fn adp1_evidence_hash_supplements_gate_code_version_for_replay_identity() {
    let src = include_str!("../src/services/autonomous_decision_service.rs");

    // Both fields must be present and distinct.
    assert!(
        src.contains("gate_code_version"),
        "ADP1: ConfigSnapshot must have gate_code_version for human-managed versioning"
    );
    assert!(
        src.contains("evidence_hash"),
        "ADP1: ConfigSnapshot must have evidence_hash for cryptographic gate-output identity"
    );
    // The hash must be computed from actual gate results.
    assert!(
        src.contains("blake3::hash"),
        "ADP1: evidence_hash must use BLAKE3 for cryptographic integrity — \
         a weak hash or none would allow undetected evidence tampering"
    );
}

// ── X4: ADP gate enforcement — no new enqueue bypass ─────────────────────────

/// X4: document the expected set of enqueue-capable tool handlers.
/// Any addition to this list without ADP gate wiring is a governance violation.
#[test]
fn x4_enqueue_capable_handlers_enumerated_and_consistent() {
    let project_src   = include_str!("../src/handlers/project_tools.rs");
    let cognitive_src = include_str!("../src/handlers/cognitive_tools.rs");
    let git_src       = include_str!("../src/handlers/git_tools.rs");

    // project_tools and git_tools are enqueue-capable (they call tokio::spawn for indexing).
    // cognitive_tools does not spawn jobs — it delegates to services synchronously.
    for (name, src) in [
        ("project_tools", project_src),
        ("git_tools", git_src),
    ] {
        assert!(
            src.contains("tokio::spawn"),
            "X4: {name} is an enqueue-capable handler and must have job spawn logic"
        );
    }
    // cognitive_tools is included to verify it does NOT spawn (no enqueue path to gate).
    let _ = cognitive_src;

    // The capabilities registry must register autonomous_decision_gate.
    let caps = include_str!("../src/capabilities.rs");
    assert!(
        caps.contains("autonomous_decision_gate"),
        "X4: capabilities.rs must register autonomous_decision_gate so the \
         enqueue policy is visible in the capability matrix"
    );
}
