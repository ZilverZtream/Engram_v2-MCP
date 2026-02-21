//! Autonomous Decision Protocol (ADP v1) — mandatory gate pipeline.
//!
//! Orchestrates a sequence of verification gates that must ALL pass before an
//! autonomous agent is allowed to auto-apply a code change. If any required gate
//! fails or returns insufficient evidence, the verdict is `deny` or `abstain`.
//!
//! Gate sequence (ordered):
//! 1. Extraction confidence gate
//! 2. Trace certainty gate (ambiguity penalty)
//! 3. Safety policy gate
//! 4. Retrieval quality gate
//! 5. Blast radius / risk gate
//! 6. Anti-pattern gate
//! 7. Runtime evidence gate (optional)
//! 8. Abstention rule (insufficient evidence → abstain)

use serde::{Deserialize, Serialize};

use crate::services::blast_radius_service::RiskBand;
use crate::services::safety_service::PolicyDecision;

// ── Types ────────────────────────────────────────────────────────────────────

/// Final verdict from the ADP gate pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdpVerdict {
    /// All gates passed — change may be auto-applied.
    Allow,
    /// At least one gate hard-failed — change must NOT be applied.
    Deny,
    /// Evidence insufficient to decide — agent must gather more data.
    Abstain,
}

impl std::fmt::Display for AdpVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Allow => write!(f, "allow"),
            Self::Deny => write!(f, "deny"),
            Self::Abstain => write!(f, "abstain"),
        }
    }
}

/// Outcome of a single gate evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateResult {
    /// Machine-readable gate identifier.
    pub gate_id: String,
    /// Human-readable gate name.
    pub gate_name: String,
    /// Whether this gate passed.
    pub passed: bool,
    /// Confidence in the gate's own evaluation (0.0–1.0).
    pub confidence: f64,
    /// Human-readable detail of why this gate passed or failed.
    pub detail: String,
    /// If the gate was skipped (e.g., not applicable), this is true.
    pub skipped: bool,
}

/// Complete ADP evaluation result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdpDecision {
    pub verdict: AdpVerdict,
    /// Aggregate confidence across all gates (0.0–1.0).
    pub confidence: f64,
    /// Human-readable reasons for the verdict.
    pub reasons: Vec<String>,
    /// Gate IDs that failed.
    pub failed_gates: Vec<String>,
    /// Next evidence the agent should gather to upgrade from abstain to allow.
    pub required_followups: Vec<String>,
    /// Per-gate detailed results.
    pub gate_results: Vec<GateResult>,
}

/// Risk profile provided by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RiskProfile {
    Low,
    Medium,
    High,
}

impl RiskProfile {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "low" => Self::Low,
            "high" => Self::High,
            _ => Self::Medium,
        }
    }
}

/// Input to the ADP pipeline, built by the tool handler from request + state.
pub struct AdpInput {
    // ── Extraction confidence ──
    /// Extraction confidence score (0.0–1.0). None if not applicable.
    pub extraction_confidence: Option<f64>,
    /// Extraction confidence band string (e.g., "high", "medium", "low").
    pub extraction_band: Option<String>,

    // ── Trace certainty ──
    /// Whether the trace used a fallback/candidate resolution.
    pub trace_used_fallback: bool,
    /// Number of ambiguous candidate matches found during trace.
    pub trace_candidate_count: usize,

    // ── Safety policy ──
    pub safety_decision: Option<PolicyDecision>,

    // ── Retrieval quality ──
    /// Whether retrieval benchmarks pass the configured thresholds.
    pub retrieval_production_ready: Option<bool>,
    pub retrieval_ndcg: Option<f64>,
    pub retrieval_recall: Option<f64>,

    // ── Blast radius ──
    /// Migration risk score (1–10).
    pub blast_radius_risk: Option<u8>,
    pub blast_radius_band: Option<RiskBand>,
    pub blast_radius_downstream: Option<usize>,

    // ── Anti-pattern ──
    /// Immune check verdict: "PASS", "WARN", "BLOCK".
    pub immune_verdict: Option<String>,
    pub immune_confidence: Option<f32>,

    // ── Runtime evidence ──
    pub require_runtime_evidence: bool,
    pub has_runtime_evidence: bool,

    // ── Risk profile ──
    pub risk_profile: RiskProfile,

    // ── Thresholds (from config) ──
    pub min_extraction_confidence: f64,
    pub min_safety_confidence: f64,
    pub max_blast_radius_for_auto: u8,
}

// ── Gate pipeline ────────────────────────────────────────────────────────────

/// Run the full ADP gate pipeline and produce a deterministic verdict.
pub fn evaluate_gates(input: &AdpInput) -> AdpDecision {
    let mut gate_results = Vec::with_capacity(8);
    let mut failed_gates = Vec::new();
    let mut reasons = Vec::new();
    let mut followups = Vec::new();
    let mut has_hard_deny = false;
    let mut has_abstain = false;

    // ── Gate 1: Extraction confidence ──
    let g1 = evaluate_extraction_confidence_gate(input);
    if !g1.passed && !g1.skipped {
        if input.extraction_confidence.is_some() {
            has_hard_deny = true;
            reasons.push(g1.detail.clone());
        } else {
            has_abstain = true;
            followups.push(
                "Run get_extraction_confidence for affected files to provide extraction evidence"
                    .into(),
            );
        }
        failed_gates.push(g1.gate_id.clone());
    }
    gate_results.push(g1);

    // ── Gate 2: Trace certainty ──
    let g2 = evaluate_trace_certainty_gate(input);
    if !g2.passed && !g2.skipped {
        // Ambiguous trace → abstain (never auto-allow with ambiguity)
        has_abstain = true;
        reasons.push(g2.detail.clone());
        failed_gates.push(g2.gate_id.clone());
        followups.push(
            "Disambiguate control mapping: provide explicit control_id or handler_fqn".into(),
        );
    }
    gate_results.push(g2);

    // ── Gate 3: Safety policy ──
    let g3 = evaluate_safety_policy_gate(input);
    if !g3.passed && !g3.skipped {
        if input.safety_decision.is_some() {
            has_hard_deny = true;
            reasons.push(g3.detail.clone());
        } else {
            has_abstain = true;
            followups.push("Run evaluate_safety for the proposed change".into());
        }
        failed_gates.push(g3.gate_id.clone());
    }
    gate_results.push(g3);

    // ── Gate 4: Retrieval quality ──
    let g4 = evaluate_retrieval_quality_gate(input);
    if !g4.passed && !g4.skipped {
        if input.retrieval_production_ready.is_some() {
            has_hard_deny = true;
            reasons.push(g4.detail.clone());
        } else {
            has_abstain = true;
            followups.push("Run benchmark_retrieval to validate search quality".into());
        }
        failed_gates.push(g4.gate_id.clone());
    }
    gate_results.push(g4);

    // ── Gate 5: Blast radius / risk ──
    let g5 = evaluate_blast_radius_gate(input);
    if !g5.passed && !g5.skipped {
        if input.blast_radius_risk.is_some() {
            // High risk profile can tolerate higher blast radius
            if input.risk_profile == RiskProfile::High {
                // Already factored in — this is a hard deny for critical
                has_hard_deny = true;
            } else {
                has_hard_deny = true;
            }
            reasons.push(g5.detail.clone());
        } else {
            has_abstain = true;
            followups.push("Run compute_blast_radius on affected files to assess risk".into());
        }
        failed_gates.push(g5.gate_id.clone());
    }
    gate_results.push(g5);

    // ── Gate 6: Anti-pattern ──
    let g6 = evaluate_anti_pattern_gate(input);
    if !g6.passed && !g6.skipped {
        if input.immune_verdict.is_some() {
            let verdict = input.immune_verdict.as_deref().unwrap_or("");
            if verdict == "BLOCK" {
                has_hard_deny = true;
            } else {
                // WARN — downgrade to abstain for medium/high risk
                if input.risk_profile != RiskProfile::Low {
                    has_abstain = true;
                }
            }
            reasons.push(g6.detail.clone());
        } else {
            has_abstain = true;
            followups.push("Run immune_check on the proposed code change".into());
        }
        failed_gates.push(g6.gate_id.clone());
    }
    gate_results.push(g6);

    // ── Gate 7: Runtime evidence ──
    let g7 = evaluate_runtime_evidence_gate(input);
    if !g7.passed && !g7.skipped {
        has_abstain = true;
        reasons.push(g7.detail.clone());
        failed_gates.push(g7.gate_id.clone());
        followups.push(
            "Deploy instrumentation pack and collect runtime evidence before auto-applying".into(),
        );
    }
    gate_results.push(g7);

    // ── Gate 8: Abstention rule (meta-gate) ──
    let insufficient_evidence = gate_results.iter().filter(|g| g.skipped).count();
    let total_applicable = gate_results.iter().filter(|g| !g.skipped).count();
    let g8_passed = if total_applicable == 0 {
        false // No evidence at all → must abstain
    } else {
        // If more than half of applicable gates lack data, abstain
        insufficient_evidence <= total_applicable / 3
    };
    let g8 = GateResult {
        gate_id: "evidence_sufficiency".into(),
        gate_name: "Evidence Sufficiency".into(),
        passed: g8_passed,
        confidence: if total_applicable > 0 {
            (total_applicable as f64 - insufficient_evidence as f64) / total_applicable as f64
        } else {
            0.0
        },
        detail: format!(
            "{} of {} applicable gates had sufficient evidence",
            total_applicable - insufficient_evidence,
            total_applicable
        ),
        skipped: false,
    };
    if !g8.passed {
        has_abstain = true;
        if !failed_gates.contains(&g8.gate_id) {
            failed_gates.push(g8.gate_id.clone());
        }
    }
    gate_results.push(g8);

    // ── Compute aggregate confidence ──
    let applicable: Vec<&GateResult> = gate_results.iter().filter(|g| !g.skipped).collect();
    let aggregate_confidence = if applicable.is_empty() {
        0.0
    } else {
        applicable.iter().map(|g| g.confidence).sum::<f64>() / applicable.len() as f64
    };

    // ── Determine verdict ──
    let verdict = if has_hard_deny {
        AdpVerdict::Deny
    } else if has_abstain || !failed_gates.is_empty() {
        AdpVerdict::Abstain
    } else {
        AdpVerdict::Allow
    };

    // Ensure followups are populated for abstain
    if verdict == AdpVerdict::Abstain && followups.is_empty() {
        followups.push("Provide additional evidence for failing gates to proceed".into());
    }

    AdpDecision {
        verdict,
        confidence: aggregate_confidence,
        reasons,
        failed_gates,
        required_followups: followups,
        gate_results,
    }
}

// ── Individual gate evaluators ───────────────────────────────────────────────

fn evaluate_extraction_confidence_gate(input: &AdpInput) -> GateResult {
    match input.extraction_confidence {
        Some(score) => {
            let threshold = input.min_extraction_confidence;
            let passed = score >= threshold;
            GateResult {
                gate_id: "extraction_confidence".into(),
                gate_name: "Extraction Confidence".into(),
                passed,
                confidence: score,
                detail: if passed {
                    format!(
                        "Extraction confidence {:.2} >= threshold {:.2} (band: {})",
                        score,
                        threshold,
                        input.extraction_band.as_deref().unwrap_or("unknown")
                    )
                } else {
                    format!(
                        "Extraction confidence {:.2} < threshold {:.2} — extraction unreliable",
                        score, threshold
                    )
                },
                skipped: false,
            }
        }
        None => GateResult {
            gate_id: "extraction_confidence".into(),
            gate_name: "Extraction Confidence".into(),
            passed: false,
            confidence: 0.0,
            detail: "No extraction confidence data provided".into(),
            skipped: true,
        },
    }
}

fn evaluate_trace_certainty_gate(input: &AdpInput) -> GateResult {
    if !input.trace_used_fallback && input.trace_candidate_count <= 1 {
        return GateResult {
            gate_id: "trace_certainty".into(),
            gate_name: "Trace Certainty".into(),
            passed: true,
            confidence: 1.0,
            detail: "Trace resolution was deterministic (no fallback candidates)".into(),
            skipped: false,
        };
    }

    // Fallback was used or multiple candidates found
    let penalty = if input.trace_candidate_count > 1 {
        // Multiple candidates: confidence degrades with more candidates
        1.0 - (input.trace_candidate_count as f64 * 0.2).min(0.8)
    } else {
        0.5 // Single fallback candidate: moderate confidence
    };

    GateResult {
        gate_id: "trace_certainty".into(),
        gate_name: "Trace Certainty".into(),
        passed: false,
        confidence: penalty,
        detail: format!(
            "Trace used fallback candidate resolution ({} candidates). \
             Ambiguous control mapping — cannot auto-apply without explicit disambiguation.",
            input.trace_candidate_count
        ),
        skipped: false,
    }
}

fn evaluate_safety_policy_gate(input: &AdpInput) -> GateResult {
    match &input.safety_decision {
        Some(decision) => {
            let passed = decision.allowed;
            GateResult {
                gate_id: "safety_policy".into(),
                gate_name: "Safety Policy".into(),
                passed,
                confidence: decision.confidence,
                detail: decision.summary.clone(),
                skipped: false,
            }
        }
        None => GateResult {
            gate_id: "safety_policy".into(),
            gate_name: "Safety Policy".into(),
            passed: false,
            confidence: 0.0,
            detail: "Safety evaluation not performed".into(),
            skipped: true,
        },
    }
}

fn evaluate_retrieval_quality_gate(input: &AdpInput) -> GateResult {
    match input.retrieval_production_ready {
        Some(ready) => GateResult {
            gate_id: "retrieval_quality".into(),
            gate_name: "Retrieval Quality".into(),
            passed: ready,
            confidence: if ready {
                ((input.retrieval_ndcg.unwrap_or(0.0) + input.retrieval_recall.unwrap_or(0.0))
                    / 2.0)
                    .min(1.0)
            } else {
                (input.retrieval_ndcg.unwrap_or(0.0) + input.retrieval_recall.unwrap_or(0.0)) / 2.0
            },
            detail: format!(
                "Retrieval NDCG@10={:.2}, Recall@10={:.2}, production_ready={}",
                input.retrieval_ndcg.unwrap_or(0.0),
                input.retrieval_recall.unwrap_or(0.0),
                ready
            ),
            skipped: false,
        },
        None => GateResult {
            gate_id: "retrieval_quality".into(),
            gate_name: "Retrieval Quality".into(),
            passed: false,
            confidence: 0.0,
            detail: "Retrieval benchmark not run".into(),
            skipped: true,
        },
    }
}

fn evaluate_blast_radius_gate(input: &AdpInput) -> GateResult {
    match input.blast_radius_risk {
        Some(risk) => {
            let max_allowed = input.max_blast_radius_for_auto;
            let passed = risk <= max_allowed;
            let band = input
                .blast_radius_band
                .as_ref()
                .map(|b| b.to_string())
                .unwrap_or_else(|| "unknown".into());
            GateResult {
                gate_id: "blast_radius".into(),
                gate_name: "Blast Radius / Risk".into(),
                passed,
                confidence: if passed {
                    1.0 - (risk as f64 / 10.0)
                } else {
                    0.1
                },
                detail: format!(
                    "Migration risk {}/10 ({}) — max allowed for auto-apply: {}/10 (downstream: {})",
                    risk,
                    band,
                    max_allowed,
                    input.blast_radius_downstream.unwrap_or(0)
                ),
                skipped: false,
            }
        }
        None => GateResult {
            gate_id: "blast_radius".into(),
            gate_name: "Blast Radius / Risk".into(),
            passed: false,
            confidence: 0.0,
            detail: "Blast radius not computed".into(),
            skipped: true,
        },
    }
}

fn evaluate_anti_pattern_gate(input: &AdpInput) -> GateResult {
    match &input.immune_verdict {
        Some(verdict) => {
            let passed = verdict == "PASS";
            let conf = input.immune_confidence.unwrap_or(0.0) as f64;
            GateResult {
                gate_id: "anti_pattern".into(),
                gate_name: "Anti-Pattern Guard".into(),
                passed,
                confidence: if passed { 1.0 - conf } else { conf },
                detail: format!("Immune verdict: {} (similarity: {:.2})", verdict, conf),
                skipped: false,
            }
        }
        None => GateResult {
            gate_id: "anti_pattern".into(),
            gate_name: "Anti-Pattern Guard".into(),
            passed: false,
            confidence: 0.0,
            detail: "Anti-pattern check not performed".into(),
            skipped: true,
        },
    }
}

fn evaluate_runtime_evidence_gate(input: &AdpInput) -> GateResult {
    if !input.require_runtime_evidence {
        return GateResult {
            gate_id: "runtime_evidence".into(),
            gate_name: "Runtime Evidence".into(),
            passed: true,
            confidence: 1.0,
            detail: "Runtime evidence not required for this evaluation".into(),
            skipped: true,
        };
    }

    GateResult {
        gate_id: "runtime_evidence".into(),
        gate_name: "Runtime Evidence".into(),
        passed: input.has_runtime_evidence,
        confidence: if input.has_runtime_evidence { 0.9 } else { 0.0 },
        detail: if input.has_runtime_evidence {
            "Runtime evidence available — instrumentation data collected".into()
        } else {
            "Runtime evidence required but not available — deploy instrumentation first".into()
        },
        skipped: false,
    }
}

// ── Formatting ───────────────────────────────────────────────────────────────

/// Format an ADP decision as human-readable text.
pub fn format_decision(decision: &AdpDecision) -> String {
    let mut out = String::with_capacity(2048);

    out.push_str(&format!(
        "# Autonomous Decision Gate Result\n\n\
         **Verdict:** {}\n\
         **Confidence:** {:.2}\n\n",
        decision.verdict, decision.confidence
    ));

    if !decision.reasons.is_empty() {
        out.push_str("## Reasons\n");
        for r in &decision.reasons {
            out.push_str(&format!("- {}\n", r));
        }
        out.push('\n');
    }

    if !decision.failed_gates.is_empty() {
        out.push_str("## Failed Gates\n");
        for g in &decision.failed_gates {
            out.push_str(&format!("- `{}`\n", g));
        }
        out.push('\n');
    }

    if !decision.required_followups.is_empty() {
        out.push_str("## Required Follow-ups\n");
        for f in &decision.required_followups {
            out.push_str(&format!("- {}\n", f));
        }
        out.push('\n');
    }

    out.push_str("## Gate Details\n");
    for g in &decision.gate_results {
        let status = if g.skipped {
            "SKIP"
        } else if g.passed {
            "PASS"
        } else {
            "FAIL"
        };
        out.push_str(&format!(
            "- [{}] **{}** (confidence: {:.2}): {}\n",
            status, g.gate_name, g.confidence, g.detail
        ));
    }

    out
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::safety_service::{self, SafetyEvalRequest};

    fn make_default_input() -> AdpInput {
        AdpInput {
            extraction_confidence: Some(0.85),
            extraction_band: Some("high".into()),
            trace_used_fallback: false,
            trace_candidate_count: 0,
            safety_decision: Some(safety_service::evaluate_safety(
                &SafetyEvalRequest {
                    project_id: "test".into(),
                    affected_files: vec!["a.rs".into()],
                    refactor_type: "rename".into(),
                    impact_node_count: 5,
                    impact_confidence: 0.95,
                    test_coverage: 0.85,
                    anti_pattern_clear: true,
                    downstream_dependents: 3,
                    touches_global_state: false,
                    touches_database: false,
                },
                true,
                0.7,
                0.6,
            )),
            retrieval_production_ready: Some(true),
            retrieval_ndcg: Some(0.75),
            retrieval_recall: Some(0.80),
            blast_radius_risk: Some(3),
            blast_radius_band: Some(RiskBand::Low),
            blast_radius_downstream: Some(5),
            immune_verdict: Some("PASS".into()),
            immune_confidence: Some(0.05),
            require_runtime_evidence: false,
            has_runtime_evidence: false,
            risk_profile: RiskProfile::Medium,
            min_extraction_confidence: 0.5,
            min_safety_confidence: 0.7,
            max_blast_radius_for_auto: 6,
        }
    }

    #[test]
    fn happy_path_all_gates_pass() {
        let input = make_default_input();
        let decision = evaluate_gates(&input);
        assert_eq!(decision.verdict, AdpVerdict::Allow);
        assert!(decision.confidence > 0.5);
        assert!(decision.failed_gates.is_empty());
        assert!(decision.required_followups.is_empty());
    }

    #[test]
    fn deny_on_safety_failure() {
        let mut input = make_default_input();
        input.safety_decision = Some(safety_service::evaluate_safety(
            &SafetyEvalRequest {
                project_id: "test".into(),
                affected_files: vec!["a.rs".into()],
                refactor_type: "rename".into(),
                impact_node_count: 5,
                impact_confidence: 0.2, // Low confidence → blocks
                test_coverage: 0.85,
                anti_pattern_clear: true,
                downstream_dependents: 3,
                touches_global_state: false,
                touches_database: false,
            },
            true,
            0.7,
            0.6,
        ));
        let decision = evaluate_gates(&input);
        assert_eq!(decision.verdict, AdpVerdict::Deny);
        assert!(decision.failed_gates.contains(&"safety_policy".to_string()));
    }

    #[test]
    fn abstain_on_low_confidence() {
        let mut input = make_default_input();
        input.extraction_confidence = None;
        input.retrieval_production_ready = None;
        let decision = evaluate_gates(&input);
        assert_eq!(decision.verdict, AdpVerdict::Abstain);
        assert!(!decision.required_followups.is_empty());
    }

    #[test]
    fn abstain_on_insufficient_runtime_evidence() {
        let mut input = make_default_input();
        input.require_runtime_evidence = true;
        input.has_runtime_evidence = false;
        let decision = evaluate_gates(&input);
        assert_eq!(decision.verdict, AdpVerdict::Abstain);
        assert!(
            decision
                .failed_gates
                .contains(&"runtime_evidence".to_string())
        );
    }

    #[test]
    fn deny_on_ambiguous_ui_control_fallback() {
        let mut input = make_default_input();
        input.trace_used_fallback = true;
        input.trace_candidate_count = 3;
        let decision = evaluate_gates(&input);
        // Ambiguous trace → abstain (never allow)
        assert_ne!(decision.verdict, AdpVerdict::Allow);
        assert!(
            decision
                .failed_gates
                .contains(&"trace_certainty".to_string())
        );
    }

    #[test]
    fn deny_on_retrieval_benchmark_failure() {
        let mut input = make_default_input();
        input.retrieval_production_ready = Some(false);
        input.retrieval_ndcg = Some(0.3);
        input.retrieval_recall = Some(0.4);
        let decision = evaluate_gates(&input);
        assert_eq!(decision.verdict, AdpVerdict::Deny);
        assert!(
            decision
                .failed_gates
                .contains(&"retrieval_quality".to_string())
        );
    }

    #[test]
    fn deny_on_immune_block() {
        let mut input = make_default_input();
        input.immune_verdict = Some("BLOCK".into());
        input.immune_confidence = Some(0.8);
        let decision = evaluate_gates(&input);
        assert_eq!(decision.verdict, AdpVerdict::Deny);
        assert!(decision.failed_gates.contains(&"anti_pattern".to_string()));
    }

    #[test]
    fn deny_on_high_blast_radius() {
        let mut input = make_default_input();
        input.blast_radius_risk = Some(9);
        input.blast_radius_band = Some(RiskBand::Critical);
        let decision = evaluate_gates(&input);
        assert_eq!(decision.verdict, AdpVerdict::Deny);
        assert!(decision.failed_gates.contains(&"blast_radius".to_string()));
    }

    #[test]
    fn machine_readable_reasons_always_present() {
        // Even on allow, gate_results should be present
        let input = make_default_input();
        let decision = evaluate_gates(&input);
        assert!(!decision.gate_results.is_empty());
        // Every gate has an id and detail
        for g in &decision.gate_results {
            assert!(!g.gate_id.is_empty());
            assert!(!g.detail.is_empty());
        }
    }

    #[test]
    fn format_produces_valid_output() {
        let input = make_default_input();
        let decision = evaluate_gates(&input);
        let text = format_decision(&decision);
        assert!(text.contains("Autonomous Decision Gate Result"));
        assert!(text.contains("Verdict:"));
        assert!(text.contains("Gate Details"));
    }

    #[test]
    fn immune_warn_abstains_for_high_risk() {
        let mut input = make_default_input();
        input.immune_verdict = Some("WARN".into());
        input.immune_confidence = Some(0.25);
        input.risk_profile = RiskProfile::High;
        let decision = evaluate_gates(&input);
        // WARN + high risk profile → abstain (not allow)
        assert_ne!(decision.verdict, AdpVerdict::Allow);
    }
}
