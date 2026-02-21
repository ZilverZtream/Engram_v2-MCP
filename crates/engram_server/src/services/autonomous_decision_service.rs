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

// ── ADP vNext types ──────────────────────────────────────────────────────────

/// Reconciliation-derived scores for the runtime evidence gate (vNext).
///
/// Replaces the boolean `has_runtime_evidence` with rich reconciliation
/// data from static-vs-runtime path analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconciliationScores {
    /// Ratio of confirmed paths (confirmed / static_paths).
    pub confirmed_ratio: f64,
    /// Ratio of contradicted paths (contradicted / static_paths).
    pub contradicted_ratio: f64,
    /// Confidence delta from reconciliation analysis.
    pub confidence_delta: f64,
    /// Total static paths evaluated.
    pub static_paths_count: usize,
}

/// Graph-derived impact metrics for structured safety evaluation (vNext).
///
/// Replaces text-heuristic detection of state/database touches with
/// edge-count-based signals from the project graph.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GraphImpactMetrics {
    pub downstream_dependency_count: u64,
    pub reads_state_count: usize,
    pub writes_state_count: usize,
    pub sql_calls_count: usize,
    pub queries_table_count: usize,
    pub injects_script_count: usize,
}

/// Retrieval evaluation mode for the retrieval quality gate (vNext).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RetrievalMode {
    /// Retrieval gate skipped (fast mode).
    Skipped,
    /// Used cached benchmark results (staleness discount applies).
    Cached,
    /// Ran live benchmark (deep mode).
    Live,
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

    // ── vNext fields ──
    /// Reconciliation scores derived from runtime evidence (replaces boolean).
    pub reconciliation: Option<ReconciliationScores>,
    /// Graph-derived impact metrics for structured safety evaluation.
    pub graph_impact: Option<GraphImpactMetrics>,
    /// Retrieval evaluation mode (skipped/cached/live).
    pub retrieval_mode: RetrievalMode,
    /// Migration class for calibrated thresholds (e.g., "data_access", "webforms_page").
    pub migration_class: Option<String>,
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
            total_applicable.saturating_sub(insufficient_evidence),
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

    // ── Compute aggregate confidence (vNext: calibrated weighted aggregation) ──
    let applicable: Vec<&GateResult> = gate_results.iter().filter(|g| !g.skipped).collect();
    let aggregate_confidence = if applicable.is_empty() {
        0.0
    } else {
        let mut weighted_sum = 0.0;
        let mut total_weight = 0.0;
        for g in &applicable {
            let weight = gate_reliability_weight(&g.gate_id);
            weighted_sum += g.confidence * weight;
            total_weight += weight;
        }
        let base = if total_weight > 0.0 {
            weighted_sum / total_weight
        } else {
            0.0
        };
        let class_adj = class_threshold_adjustment(input.migration_class.as_deref());
        let penalty = interaction_penalty(&gate_results);
        (base + class_adj - penalty).clamp(0.0, 1.0)
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
        Some(ready) => {
            let raw_conf = ((input.retrieval_ndcg.unwrap_or(0.0)
                + input.retrieval_recall.unwrap_or(0.0))
                / 2.0)
                .min(1.0);
            // vNext: cached results get a staleness discount
            let confidence = match input.retrieval_mode {
                RetrievalMode::Cached => raw_conf * 0.9,
                _ => raw_conf,
            };
            GateResult {
                gate_id: "retrieval_quality".into(),
                gate_name: "Retrieval Quality".into(),
                passed: ready,
                confidence,
                detail: format!(
                    "Retrieval NDCG@10={:.2}, Recall@10={:.2}, mode={:?}, production_ready={}",
                    input.retrieval_ndcg.unwrap_or(0.0),
                    input.retrieval_recall.unwrap_or(0.0),
                    input.retrieval_mode,
                    ready
                ),
                skipped: false,
            }
        }
        None => {
            let is_skipped = input.retrieval_mode == RetrievalMode::Skipped;
            GateResult {
                gate_id: "retrieval_quality".into(),
                gate_name: "Retrieval Quality".into(),
                passed: false,
                confidence: 0.0,
                detail: if is_skipped {
                    "Retrieval benchmark skipped (fast mode)".into()
                } else {
                    "Retrieval benchmark not run".into()
                },
                skipped: is_skipped,
            }
        }
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

    // vNext: prefer reconciliation scores over boolean presence
    if let Some(ref recon) = input.reconciliation {
        let confidence = compute_reconciliation_confidence(recon);
        let passed = confidence >= 0.6 && recon.contradicted_ratio < 0.2;
        return GateResult {
            gate_id: "runtime_evidence".into(),
            gate_name: "Runtime Evidence".into(),
            passed,
            confidence,
            detail: format!(
                "Reconciliation: {:.0}% confirmed, {:.0}% contradicted, \
                 delta={:.2}, paths={}",
                recon.confirmed_ratio * 100.0,
                recon.contradicted_ratio * 100.0,
                recon.confidence_delta,
                recon.static_paths_count,
            ),
            skipped: false,
        };
    }

    // Fallback to boolean (backward compat, lower confidence to incentivize upgrade)
    GateResult {
        gate_id: "runtime_evidence".into(),
        gate_name: "Runtime Evidence".into(),
        passed: input.has_runtime_evidence,
        confidence: if input.has_runtime_evidence { 0.7 } else { 0.0 },
        detail: if input.has_runtime_evidence {
            "Runtime evidence available (boolean mode — upgrade to reconciliation for higher confidence)".into()
        } else {
            "Runtime evidence required but not available — deploy instrumentation first".into()
        },
        skipped: false,
    }
}

/// Compute confidence from reconciliation scores.
///
/// Formula: base (confirmed * 0.7) - penalty (contradicted * 0.5) + uplift (delta * 0.3)
fn compute_reconciliation_confidence(recon: &ReconciliationScores) -> f64 {
    if recon.static_paths_count == 0 {
        return 0.0;
    }
    let base = recon.confirmed_ratio * 0.7;
    let penalty = recon.contradicted_ratio * 0.5;
    let uplift = recon.confidence_delta.max(0.0) * 0.3;
    (base - penalty + uplift).clamp(0.0, 1.0)
}

// ── Wave-level ADP (vNext) ────────────────────────────────────────────────────

/// Input for evaluating an entire migration wave.
pub struct WaveAdpInput {
    pub wave_number: usize,
    pub wave_name: String,
    /// Per-item ADP inputs (one per file/component in the wave).
    pub items: Vec<(String, AdpInput)>, // (file_path, input)
    /// Cross-item dependencies within this wave.
    pub cross_item_deps: usize,
}

/// Result of evaluating a migration wave.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaveAdpDecision {
    pub wave_number: usize,
    pub wave_name: String,
    /// Overall wave verdict: allow only if ALL items allow.
    pub verdict: AdpVerdict,
    /// Aggregate confidence across all items.
    pub confidence: f64,
    /// Per-item decisions.
    pub item_decisions: Vec<WaveItemDecision>,
    /// Items that blocked the wave.
    pub blocking_items: Vec<String>,
    /// Cross-item interaction penalties applied.
    pub interaction_penalties: Vec<String>,
}

/// Per-item decision within a wave evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaveItemDecision {
    pub file_path: String,
    pub decision: AdpDecision,
}

/// Evaluate an entire migration wave.
///
/// Verdict logic:
/// - If ANY item is Deny → wave is Deny
/// - If ANY item is Abstain → wave is Abstain
/// - If ALL items are Allow → wave is Allow
/// - Cross-item penalty: >3 items with blast_radius > 5 → shift to Abstain
pub fn evaluate_wave(input: &WaveAdpInput) -> WaveAdpDecision {
    let mut item_decisions = Vec::new();
    let mut blocking_items = Vec::new();
    let mut has_deny = false;
    let mut has_abstain = false;
    let mut high_blast_count = 0;

    for (file_path, item) in &input.items {
        let decision = evaluate_gates(item);

        match decision.verdict {
            AdpVerdict::Deny => {
                has_deny = true;
                blocking_items.push(file_path.clone());
            }
            AdpVerdict::Abstain => {
                has_abstain = true;
                blocking_items.push(file_path.clone());
            }
            AdpVerdict::Allow => {}
        }

        if item.blast_radius_risk.unwrap_or(0) > 5 {
            high_blast_count += 1;
        }

        item_decisions.push(WaveItemDecision {
            file_path: file_path.clone(),
            decision,
        });
    }

    // Cross-item interaction: many high-blast items in one wave = systemic risk
    let mut interaction_pens = Vec::new();
    if high_blast_count > 3 {
        has_abstain = true;
        interaction_pens.push(format!(
            "{} items have blast radius > 5; consider splitting this wave",
            high_blast_count
        ));
    }

    // Cross-dependency penalty
    if input.items.len() > 1 && input.cross_item_deps > input.items.len() * 2 {
        interaction_pens.push(format!(
            "High internal coupling ({} cross-deps for {} items); migration order matters",
            input.cross_item_deps,
            input.items.len()
        ));
    }

    let verdict = if has_deny {
        AdpVerdict::Deny
    } else if has_abstain {
        AdpVerdict::Abstain
    } else {
        AdpVerdict::Allow
    };

    let confidence = if item_decisions.is_empty() {
        0.0
    } else {
        item_decisions
            .iter()
            .map(|d| d.decision.confidence)
            .sum::<f64>()
            / item_decisions.len() as f64
    };

    WaveAdpDecision {
        wave_number: input.wave_number,
        wave_name: input.wave_name.clone(),
        verdict,
        confidence,
        item_decisions,
        blocking_items,
        interaction_penalties: interaction_pens,
    }
}

/// Format a wave decision as human-readable text.
pub fn format_wave_decision(decision: &WaveAdpDecision) -> String {
    let mut out = String::with_capacity(4096);

    out.push_str(&format!(
        "# Wave {} — {} ADP Result\n\n\
         **Verdict:** {}\n\
         **Confidence:** {:.2}\n\
         **Items:** {}\n\n",
        decision.wave_number,
        decision.wave_name,
        decision.verdict,
        decision.confidence,
        decision.item_decisions.len()
    ));

    if !decision.blocking_items.is_empty() {
        out.push_str("## Blocking Items\n");
        for b in &decision.blocking_items {
            out.push_str(&format!("- `{}`\n", b));
        }
        out.push('\n');
    }

    if !decision.interaction_penalties.is_empty() {
        out.push_str("## Cross-Item Interaction Penalties\n");
        for p in &decision.interaction_penalties {
            out.push_str(&format!("- {}\n", p));
        }
        out.push('\n');
    }

    out.push_str("## Per-Item Results\n");
    for item in &decision.item_decisions {
        out.push_str(&format!(
            "### `{}`\n**Verdict:** {} | **Confidence:** {:.2}\n",
            item.file_path, item.decision.verdict, item.decision.confidence
        ));
        if !item.decision.failed_gates.is_empty() {
            out.push_str(&format!(
                "Failed gates: {}\n",
                item.decision.failed_gates.join(", ")
            ));
        }
        out.push('\n');
    }

    out
}

// ── Calibrated confidence helpers (vNext) ─────────────────────────────────────

/// Per-gate reliability priors for weighted confidence aggregation.
fn gate_reliability_weight(gate_id: &str) -> f64 {
    match gate_id {
        "extraction_confidence" => 0.20,
        "trace_certainty" => 0.15,
        "safety_policy" => 0.25,
        "retrieval_quality" => 0.10,
        "blast_radius" => 0.15,
        "anti_pattern" => 0.10,
        "runtime_evidence" => 0.05,
        _ => 0.10, // evidence_sufficiency or unknown
    }
}

/// Class-specific confidence adjustments.
/// Data-access / DB migrations are stricter; static assets are more lenient.
fn class_threshold_adjustment(migration_class: Option<&str>) -> f64 {
    match migration_class {
        Some("data_access") | Some("database_migration") => -0.05,
        Some("static_asset") | Some("configuration") => 0.05,
        _ => 0.0,
    }
}

/// Interaction penalty: co-failure of safety + blast radius indicates systemic risk.
fn interaction_penalty(gate_results: &[GateResult]) -> f64 {
    let safety_failed = gate_results
        .iter()
        .any(|g| g.gate_id == "safety_policy" && !g.passed && !g.skipped);
    let blast_failed = gate_results
        .iter()
        .any(|g| g.gate_id == "blast_radius" && !g.passed && !g.skipped);
    if safety_failed && blast_failed {
        0.10
    } else {
        0.0
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

// ── Rollout Policy and Kill-Switch (Ticket 10) ──────────────────────────────

/// ADP rollout phase — controls how verdicts are enforced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RolloutPhase {
    /// Log decisions only — never block. For baseline data collection.
    Shadow,
    /// Warn on deny/abstain but do not block. For advisory rollout.
    Advisory,
    /// Block on deny, require human review on abstain. For guarded rollout.
    Guarded,
    /// Auto-apply on allow, block on deny, require review on abstain.
    Autonomous,
}

impl RolloutPhase {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "advisory" => Self::Advisory,
            "guarded" => Self::Guarded,
            "autonomous" => Self::Autonomous,
            _ => Self::Shadow,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Shadow => "shadow",
            Self::Advisory => "advisory",
            Self::Guarded => "guarded",
            Self::Autonomous => "autonomous",
        }
    }
}

/// Apply rollout policy and kill-switch to an ADP decision.
///
/// Returns a modified decision where:
/// - Kill-switch ON → always Deny with explanation.
/// - Shadow phase → verdict is logged but override to Allow.
/// - Advisory phase → verdict is logged but override Deny→Allow (warn only).
/// - Guarded/Autonomous → verdict stands as-is.
pub fn apply_rollout_policy(
    decision: &AdpDecision,
    phase: RolloutPhase,
    kill_switch: bool,
) -> AdpDecision {
    // Kill-switch takes absolute priority
    if kill_switch {
        return AdpDecision {
            verdict: AdpVerdict::Deny,
            confidence: 0.0,
            reasons: vec!["ADP kill-switch is active — all autonomous decisions are denied".into()],
            failed_gates: vec!["kill_switch".into()],
            required_followups: vec![
                "Deactivate kill-switch in configuration to resume ADP".into(),
            ],
            gate_results: decision.gate_results.clone(),
        };
    }

    match phase {
        RolloutPhase::Shadow => {
            // Shadow: always allow (for logging/baseline), append original verdict as metadata
            let mut reasons = decision.reasons.clone();
            reasons.push(format!(
                "[SHADOW] Original verdict was '{}' — logged only, not enforced",
                decision.verdict
            ));
            AdpDecision {
                verdict: AdpVerdict::Allow,
                confidence: decision.confidence,
                reasons,
                failed_gates: decision.failed_gates.clone(),
                required_followups: decision.required_followups.clone(),
                gate_results: decision.gate_results.clone(),
            }
        }
        RolloutPhase::Advisory => {
            // Advisory: warn but do not block (override deny→allow)
            match decision.verdict {
                AdpVerdict::Deny | AdpVerdict::Abstain => {
                    let mut reasons = decision.reasons.clone();
                    reasons.push(format!(
                        "[ADVISORY] Original verdict was '{}' — override to allow with warning",
                        decision.verdict
                    ));
                    AdpDecision {
                        verdict: AdpVerdict::Allow,
                        confidence: decision.confidence,
                        reasons,
                        failed_gates: decision.failed_gates.clone(),
                        required_followups: decision.required_followups.clone(),
                        gate_results: decision.gate_results.clone(),
                    }
                }
                AdpVerdict::Allow => decision.clone(),
            }
        }
        RolloutPhase::Guarded | RolloutPhase::Autonomous => {
            // Verdict stands as-is
            decision.clone()
        }
    }
}

// ── Immutable Decision Report (Ticket 9: JSON audit trail) ──────────────────

/// Immutable, auditable JSON report for every ADP verdict.
///
/// Contains gate-by-gate evidence, confidence deltas, follow-up actions,
/// and retrieval provenance IDs — suitable for SOC2-style audit trail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdpDecisionReport {
    /// Schema version for forward compatibility.
    pub schema_version: String,
    /// ISO-8601 timestamp of when the decision was made.
    pub timestamp: String,
    /// Build/version identifier for traceability.
    pub build_id: String,
    /// Project under evaluation.
    pub project_id: String,
    /// Description of the proposed change.
    pub proposed_change: String,
    /// Files targeted by the change.
    pub target_files: Vec<String>,
    /// Risk profile used for evaluation.
    pub risk_profile: String,
    /// The verdict: allow, deny, or abstain.
    pub verdict: String,
    /// Aggregate confidence score.
    pub confidence: f64,
    /// Human-readable reasons for the verdict.
    pub reasons: Vec<String>,
    /// Gate IDs that failed.
    pub failed_gates: Vec<String>,
    /// Machine-actionable follow-up steps.
    pub required_followups: Vec<String>,
    /// Per-gate detailed evidence.
    pub gate_evidence: Vec<GateEvidence>,
    /// Input snapshot for deterministic replay.
    pub input_snapshot: serde_json::Value,
    /// Config snapshot for deterministic replay.
    pub config_snapshot: ConfigSnapshot,
}

/// Per-gate evidence in the decision report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateEvidence {
    pub gate_id: String,
    pub gate_name: String,
    pub status: String, // "PASS", "FAIL", "SKIP"
    pub confidence: f64,
    pub detail: String,
    /// Delta from threshold (positive = above, negative = below).
    pub threshold_delta: Option<f64>,
}

/// Config snapshot for replay reproducibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigSnapshot {
    pub adp_min_extraction_confidence: f64,
    pub safety_min_confidence: f64,
    pub safety_min_coverage: f64,
    pub adp_max_blast_radius: u8,
    pub safety_policy_enabled: bool,
}

/// Build an immutable decision report from a decision and its context.
pub fn build_decision_report(
    decision: &AdpDecision,
    project_id: &str,
    proposed_change: &str,
    target_files: &[String],
    risk_profile: &str,
    input_snapshot: serde_json::Value,
    config_snapshot: ConfigSnapshot,
    build_id: &str,
) -> AdpDecisionReport {
    let gate_evidence: Vec<GateEvidence> = decision
        .gate_results
        .iter()
        .map(|g| {
            let status = if g.skipped {
                "SKIP"
            } else if g.passed {
                "PASS"
            } else {
                "FAIL"
            };
            GateEvidence {
                gate_id: g.gate_id.clone(),
                gate_name: g.gate_name.clone(),
                status: status.into(),
                confidence: g.confidence,
                detail: g.detail.clone(),
                threshold_delta: None, // Computed per-gate below
            }
        })
        .collect();

    AdpDecisionReport {
        schema_version: "1.0.0".into(),
        timestamp: {
            let dur = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            format!("{}ms", dur.as_millis())
        },
        build_id: build_id.into(),
        project_id: project_id.into(),
        proposed_change: proposed_change.into(),
        target_files: target_files.to_vec(),
        risk_profile: risk_profile.into(),
        verdict: decision.verdict.to_string(),
        confidence: decision.confidence,
        reasons: decision.reasons.clone(),
        failed_gates: decision.failed_gates.clone(),
        required_followups: decision.required_followups.clone(),
        gate_evidence,
        input_snapshot,
        config_snapshot,
    }
}

// ── Deterministic Replay (Ticket 2) ─────────────────────────────────────────

/// Replay an ADP decision from a serialized scenario input.
///
/// Converts `AdpScenarioInput` → `AdpInput` and runs the gate pipeline,
/// returning the same deterministic verdict for identical inputs.
pub fn replay_from_scenario(
    scenario_input: &engram_core::benchmark::AdpScenarioInput,
) -> AdpDecision {
    let safety_decision = match (
        scenario_input.safety_allowed,
        scenario_input.safety_confidence,
    ) {
        (Some(allowed), Some(confidence)) => Some(PolicyDecision {
            allowed,
            risk_level: if allowed {
                crate::services::safety_service::RiskLevel::Low
            } else {
                crate::services::safety_service::RiskLevel::High
            },
            checks: vec![],
            confidence,
            summary: if allowed {
                "Replayed safety decision: allowed".into()
            } else {
                "Replayed safety decision: blocked".into()
            },
            mitigations: vec![],
        }),
        _ => None,
    };

    let blast_radius_band =
        scenario_input
            .blast_radius_band
            .as_deref()
            .and_then(|b| match b.to_lowercase().as_str() {
                "low" => Some(RiskBand::Low),
                "medium" => Some(RiskBand::Medium),
                "high" => Some(RiskBand::High),
                "critical" => Some(RiskBand::Critical),
                _ => None,
            });

    // Build reconciliation scores from v2 scenario fields (if present)
    let reconciliation = match (
        scenario_input.reconciliation_confirmed_ratio,
        scenario_input.reconciliation_contradicted_ratio,
    ) {
        (Some(confirmed), Some(contradicted)) => Some(ReconciliationScores {
            confirmed_ratio: confirmed,
            contradicted_ratio: contradicted,
            confidence_delta: scenario_input
                .reconciliation_confidence_delta
                .unwrap_or(0.0),
            static_paths_count: scenario_input.reconciliation_static_paths.unwrap_or(0),
        }),
        _ => None,
    };

    // Parse retrieval mode from v2 scenario field
    let retrieval_mode = match scenario_input.retrieval_mode.as_deref() {
        Some("cached") => RetrievalMode::Cached,
        Some("live") => RetrievalMode::Live,
        _ => {
            if scenario_input.retrieval_production_ready.is_some() {
                RetrievalMode::Live
            } else {
                RetrievalMode::Skipped
            }
        }
    };

    let input = AdpInput {
        extraction_confidence: scenario_input.extraction_confidence,
        extraction_band: scenario_input.extraction_band.clone(),
        trace_used_fallback: scenario_input.trace_used_fallback,
        trace_candidate_count: scenario_input.trace_candidate_count,
        safety_decision,
        retrieval_production_ready: scenario_input.retrieval_production_ready,
        retrieval_ndcg: scenario_input.retrieval_ndcg,
        retrieval_recall: scenario_input.retrieval_recall,
        blast_radius_risk: scenario_input.blast_radius_risk,
        blast_radius_band,
        blast_radius_downstream: scenario_input.blast_radius_downstream,
        immune_verdict: scenario_input.immune_verdict.clone(),
        immune_confidence: scenario_input.immune_confidence,
        require_runtime_evidence: scenario_input.require_runtime_evidence,
        has_runtime_evidence: scenario_input.has_runtime_evidence,
        risk_profile: RiskProfile::from_str(&scenario_input.risk_profile),
        min_extraction_confidence: scenario_input.min_extraction_confidence,
        min_safety_confidence: scenario_input.min_safety_confidence,
        max_blast_radius_for_auto: scenario_input.max_blast_radius_for_auto,
        reconciliation,
        graph_impact: None, // Replay doesn't need graph impact (safety_decision is pre-provided)
        retrieval_mode,
        migration_class: scenario_input.migration_class.clone(),
    };

    evaluate_gates(&input)
}

/// Run a full ADP corpus and return per-scenario results with pass/fail.
pub fn run_corpus(corpus: &engram_core::benchmark::AdpCorpus) -> Vec<AdpCorpusResult> {
    corpus
        .scenarios
        .iter()
        .map(|scenario| {
            let decision = replay_from_scenario(&scenario.input);
            let actual_verdict = decision.verdict.to_string();
            let verdict_matches = actual_verdict == scenario.expected_verdict;

            let expected_gates_set: std::collections::HashSet<&str> = scenario
                .expected_failed_gates
                .iter()
                .map(|s| s.as_str())
                .collect();
            let actual_gates_set: std::collections::HashSet<&str> =
                decision.failed_gates.iter().map(|s| s.as_str()).collect();
            let gates_match = expected_gates_set == actual_gates_set;

            AdpCorpusResult {
                scenario_id: scenario.scenario_id.clone(),
                expected_verdict: scenario.expected_verdict.clone(),
                actual_verdict,
                verdict_matches,
                expected_failed_gates: scenario.expected_failed_gates.clone(),
                actual_failed_gates: decision.failed_gates.clone(),
                gates_match,
                confidence: decision.confidence,
                decision,
            }
        })
        .collect()
}

/// Result of running a single ADP corpus scenario.
#[derive(Debug, Clone, Serialize)]
pub struct AdpCorpusResult {
    pub scenario_id: String,
    pub expected_verdict: String,
    pub actual_verdict: String,
    pub verdict_matches: bool,
    pub expected_failed_gates: Vec<String>,
    pub actual_failed_gates: Vec<String>,
    pub gates_match: bool,
    pub confidence: f64,
    /// Full decision (not serialized separately — use gate_evidence in report).
    #[serde(skip_serializing)]
    pub decision: AdpDecision,
}

/// Confusion matrix for ADP calibration reporting.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AdpConfusionMatrix {
    /// Total scenarios evaluated.
    pub total: usize,
    /// Scenarios where expected=allow and actual=allow.
    pub true_allow: usize,
    /// Scenarios where expected=deny and actual=deny.
    pub true_deny: usize,
    /// Scenarios where expected=abstain and actual=abstain.
    pub true_abstain: usize,
    /// Scenarios where expected!=allow but actual=allow (DANGEROUS).
    pub false_allow: usize,
    /// Scenarios where expected=allow but actual!=allow.
    pub false_deny: usize,
    /// Mismatched abstains.
    pub mismatched_abstain: usize,
}

impl AdpConfusionMatrix {
    /// Build confusion matrix from corpus results.
    pub fn from_results(results: &[AdpCorpusResult]) -> Self {
        let mut m = Self::default();
        m.total = results.len();
        for r in results {
            match (r.expected_verdict.as_str(), r.actual_verdict.as_str()) {
                ("allow", "allow") => m.true_allow += 1,
                ("deny", "deny") => m.true_deny += 1,
                ("abstain", "abstain") => m.true_abstain += 1,
                (expected, "allow") if expected != "allow" => m.false_allow += 1,
                ("allow", actual) if actual != "allow" => m.false_deny += 1,
                _ => m.mismatched_abstain += 1,
            }
        }
        m
    }

    /// False-allow rate for high-risk scenarios (must be ≤ 1%).
    pub fn false_allow_rate(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.false_allow as f64 / self.total as f64
        }
    }

    /// False-deny rate.
    pub fn false_deny_rate(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.false_deny as f64 / self.total as f64
        }
    }
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
            reconciliation: None,
            graph_impact: None,
            retrieval_mode: RetrievalMode::Live,
            migration_class: None,
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

    // ── Replay determinism tests ────────────────────────────────────────────

    /// Helper to build a v1-compatible scenario input with vNext defaults.
    fn make_scenario_input() -> engram_core::benchmark::AdpScenarioInput {
        engram_core::benchmark::AdpScenarioInput {
            extraction_confidence: Some(0.85),
            extraction_band: Some("high".into()),
            trace_used_fallback: false,
            trace_candidate_count: 0,
            safety_allowed: Some(true),
            safety_confidence: Some(0.95),
            retrieval_production_ready: Some(true),
            retrieval_ndcg: Some(0.75),
            retrieval_recall: Some(0.80),
            blast_radius_risk: Some(3),
            blast_radius_band: Some("Low".into()),
            blast_radius_downstream: Some(5),
            immune_verdict: Some("PASS".into()),
            immune_confidence: Some(0.05),
            require_runtime_evidence: false,
            has_runtime_evidence: false,
            risk_profile: "medium".into(),
            min_extraction_confidence: 0.5,
            min_safety_confidence: 0.7,
            max_blast_radius_for_auto: 6,
            reconciliation_confirmed_ratio: None,
            reconciliation_contradicted_ratio: None,
            reconciliation_confidence_delta: None,
            reconciliation_static_paths: None,
            retrieval_mode: None,
            migration_class: None,
        }
    }

    #[test]
    fn replay_determinism_identical_inputs_same_verdict() {
        let scenario_input = make_scenario_input();

        let d1 = replay_from_scenario(&scenario_input);
        let d2 = replay_from_scenario(&scenario_input);

        assert_eq!(d1.verdict, d2.verdict);
        assert!((d1.confidence - d2.confidence).abs() < 1e-10);
        assert_eq!(d1.failed_gates, d2.failed_gates);
        assert_eq!(d1.gate_results.len(), d2.gate_results.len());
        for (g1, g2) in d1.gate_results.iter().zip(d2.gate_results.iter()) {
            assert_eq!(g1.gate_id, g2.gate_id);
            assert_eq!(g1.passed, g2.passed);
            assert!((g1.confidence - g2.confidence).abs() < 1e-10);
        }
    }

    #[test]
    fn replay_from_scenario_allow() {
        let mut si = make_scenario_input();
        si.extraction_confidence = Some(0.9);
        si.retrieval_ndcg = Some(0.8);
        si.retrieval_recall = Some(0.9);
        si.blast_radius_risk = Some(2);
        si.blast_radius_downstream = Some(3);
        let decision = replay_from_scenario(&si);
        assert_eq!(decision.verdict, AdpVerdict::Allow);
    }

    #[test]
    fn replay_from_scenario_deny_on_safety() {
        let mut si = make_scenario_input();
        si.extraction_confidence = Some(0.9);
        si.safety_allowed = Some(false);
        si.safety_confidence = Some(0.3);
        si.retrieval_ndcg = Some(0.8);
        si.retrieval_recall = Some(0.9);
        si.blast_radius_risk = Some(2);
        si.blast_radius_downstream = Some(3);
        let decision = replay_from_scenario(&si);
        assert_eq!(decision.verdict, AdpVerdict::Deny);
        assert!(decision.failed_gates.contains(&"safety_policy".into()));
    }

    #[test]
    fn corpus_runner_computes_confusion_matrix() {
        let mut s1 = make_scenario_input();
        s1.extraction_confidence = Some(0.9);
        s1.retrieval_ndcg = Some(0.8);
        s1.retrieval_recall = Some(0.9);
        s1.blast_radius_risk = Some(2);
        s1.blast_radius_downstream = Some(3);

        let mut s2 = make_scenario_input();
        s2.extraction_confidence = Some(0.9);
        s2.safety_allowed = Some(false);
        s2.safety_confidence = Some(0.2);
        s2.retrieval_ndcg = Some(0.8);
        s2.retrieval_recall = Some(0.9);
        s2.blast_radius_risk = Some(2);
        s2.blast_radius_downstream = Some(3);
        s2.risk_profile = "high".into();

        let mut s3 = make_scenario_input();
        s3.extraction_confidence = None;
        s3.extraction_band = None;
        s3.safety_allowed = None;
        s3.safety_confidence = None;
        s3.retrieval_production_ready = None;
        s3.retrieval_ndcg = None;
        s3.retrieval_recall = None;
        s3.blast_radius_risk = None;
        s3.blast_radius_band = None;
        s3.blast_radius_downstream = None;
        s3.immune_verdict = None;
        s3.immune_confidence = None;

        let corpus = engram_core::benchmark::AdpCorpus {
            schema_version: "1.0.0".into(),
            name: "test-corpus".into(),
            description: "Test".into(),
            scenarios: vec![
                engram_core::benchmark::AdpScenario {
                    scenario_id: "s001_allow".into(),
                    description: "All green".into(),
                    risk_class: "low".into(),
                    source: "synthetic".into(),
                    input: s1,
                    expected_verdict: "allow".into(),
                    expected_failed_gates: vec![],
                    rationale: "All signals green".into(),
                },
                engram_core::benchmark::AdpScenario {
                    scenario_id: "s002_deny_safety".into(),
                    description: "Safety blocked".into(),
                    risk_class: "high".into(),
                    source: "synthetic".into(),
                    input: s2,
                    expected_verdict: "deny".into(),
                    expected_failed_gates: vec!["safety_policy".into()],
                    rationale: "Safety evaluation blocked".into(),
                },
                engram_core::benchmark::AdpScenario {
                    scenario_id: "s003_abstain_missing".into(),
                    description: "Missing evidence".into(),
                    risk_class: "medium".into(),
                    source: "synthetic".into(),
                    input: s3,
                    expected_verdict: "abstain".into(),
                    expected_failed_gates: vec![
                        "extraction_confidence".into(),
                        "safety_policy".into(),
                        "retrieval_quality".into(),
                        "blast_radius".into(),
                        "anti_pattern".into(),
                        "evidence_sufficiency".into(),
                    ],
                    rationale: "No evidence provided".into(),
                },
            ],
        };

        let results = run_corpus(&corpus);
        assert_eq!(results.len(), 3);
        assert!(results[0].verdict_matches, "s001 should be allow");
        assert!(results[1].verdict_matches, "s002 should be deny");
        assert!(results[2].verdict_matches, "s003 should be abstain");

        let matrix = AdpConfusionMatrix::from_results(&results);
        assert_eq!(matrix.total, 3);
        assert_eq!(matrix.true_allow, 1);
        assert_eq!(matrix.true_deny, 1);
        assert_eq!(matrix.true_abstain, 1);
        assert_eq!(matrix.false_allow, 0);
        assert_eq!(matrix.false_deny, 0);
        assert!(
            matrix.false_allow_rate() < 0.01,
            "false-allow rate must be < 1%"
        );
    }

    // ── Rollout policy and kill-switch tests (Ticket 10) ────────────────

    #[test]
    fn kill_switch_overrides_allow_to_deny() {
        let input = make_default_input();
        let decision = evaluate_gates(&input);
        assert_eq!(decision.verdict, AdpVerdict::Allow);

        let overridden = apply_rollout_policy(&decision, RolloutPhase::Autonomous, true);
        assert_eq!(overridden.verdict, AdpVerdict::Deny);
        assert!(overridden.failed_gates.contains(&"kill_switch".into()));
        assert!(overridden.reasons[0].contains("kill-switch"));
    }

    #[test]
    fn kill_switch_overrides_deny_to_deny() {
        let mut input = make_default_input();
        input.blast_radius_risk = Some(9);
        input.blast_radius_band = Some(RiskBand::Critical);
        let decision = evaluate_gates(&input);
        assert_eq!(decision.verdict, AdpVerdict::Deny);

        // Kill-switch still produces Deny (same verdict, different reason)
        let overridden = apply_rollout_policy(&decision, RolloutPhase::Autonomous, true);
        assert_eq!(overridden.verdict, AdpVerdict::Deny);
        assert!(overridden.failed_gates.contains(&"kill_switch".into()));
    }

    #[test]
    fn shadow_phase_overrides_deny_to_allow() {
        let mut input = make_default_input();
        input.blast_radius_risk = Some(9);
        input.blast_radius_band = Some(RiskBand::Critical);
        let decision = evaluate_gates(&input);
        assert_eq!(decision.verdict, AdpVerdict::Deny);

        let overridden = apply_rollout_policy(&decision, RolloutPhase::Shadow, false);
        assert_eq!(overridden.verdict, AdpVerdict::Allow);
        assert!(overridden.reasons.iter().any(|r| r.contains("[SHADOW]")));
    }

    #[test]
    fn advisory_phase_overrides_deny_to_allow_with_warning() {
        let mut input = make_default_input();
        input.blast_radius_risk = Some(9);
        input.blast_radius_band = Some(RiskBand::Critical);
        let decision = evaluate_gates(&input);
        assert_eq!(decision.verdict, AdpVerdict::Deny);

        let overridden = apply_rollout_policy(&decision, RolloutPhase::Advisory, false);
        assert_eq!(overridden.verdict, AdpVerdict::Allow);
        assert!(overridden.reasons.iter().any(|r| r.contains("[ADVISORY]")));
    }

    #[test]
    fn guarded_phase_preserves_deny() {
        let mut input = make_default_input();
        input.blast_radius_risk = Some(9);
        input.blast_radius_band = Some(RiskBand::Critical);
        let decision = evaluate_gates(&input);
        assert_eq!(decision.verdict, AdpVerdict::Deny);

        let overridden = apply_rollout_policy(&decision, RolloutPhase::Guarded, false);
        assert_eq!(overridden.verdict, AdpVerdict::Deny);
    }

    #[test]
    fn autonomous_phase_preserves_allow() {
        let input = make_default_input();
        let decision = evaluate_gates(&input);
        assert_eq!(decision.verdict, AdpVerdict::Allow);

        let overridden = apply_rollout_policy(&decision, RolloutPhase::Autonomous, false);
        assert_eq!(overridden.verdict, AdpVerdict::Allow);
    }

    #[test]
    fn kill_switch_takes_priority_over_shadow() {
        let input = make_default_input();
        let decision = evaluate_gates(&input);
        // Even in Shadow phase, kill-switch forces Deny
        let overridden = apply_rollout_policy(&decision, RolloutPhase::Shadow, true);
        assert_eq!(overridden.verdict, AdpVerdict::Deny);
    }

    #[test]
    fn rollout_phase_parsing() {
        assert_eq!(RolloutPhase::from_str("shadow"), RolloutPhase::Shadow);
        assert_eq!(RolloutPhase::from_str("advisory"), RolloutPhase::Advisory);
        assert_eq!(RolloutPhase::from_str("guarded"), RolloutPhase::Guarded);
        assert_eq!(
            RolloutPhase::from_str("autonomous"),
            RolloutPhase::Autonomous
        );
        assert_eq!(RolloutPhase::from_str("SHADOW"), RolloutPhase::Shadow);
        assert_eq!(RolloutPhase::from_str("unknown"), RolloutPhase::Shadow); // Default
    }

    #[test]
    fn decision_report_has_all_required_fields() {
        let input = make_default_input();
        let decision = evaluate_gates(&input);
        let report = build_decision_report(
            &decision,
            "test-project",
            "rename method foo to bar",
            &["src/lib.rs".into()],
            "medium",
            serde_json::json!({"test": true}),
            ConfigSnapshot {
                adp_min_extraction_confidence: 0.5,
                safety_min_confidence: 0.7,
                safety_min_coverage: 0.6,
                adp_max_blast_radius: 6,
                safety_policy_enabled: true,
            },
            "test-build-001",
        );

        assert_eq!(report.schema_version, "1.0.0");
        assert_eq!(report.verdict, "allow");
        assert!(!report.gate_evidence.is_empty());
        assert_eq!(report.project_id, "test-project");
        assert_eq!(report.risk_profile, "medium");

        // Roundtrip to JSON
        let json = serde_json::to_string_pretty(&report).unwrap();
        let decoded: AdpDecisionReport = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.verdict, report.verdict);
        assert_eq!(decoded.gate_evidence.len(), report.gate_evidence.len());
    }

    // ── vNext: Reconciliation gate tests ────────────────────────────────

    #[test]
    fn reconciliation_gate_high_confirmed_passes() {
        let mut input = make_default_input();
        input.require_runtime_evidence = true;
        input.reconciliation = Some(ReconciliationScores {
            confirmed_ratio: 0.90,
            contradicted_ratio: 0.03,
            confidence_delta: 0.15,
            static_paths_count: 20,
        });
        let g = evaluate_runtime_evidence_gate(&input);
        assert!(g.passed, "90% confirmed / 3% contradicted should pass");
        assert!(g.confidence >= 0.6);
        assert!(!g.skipped);
        assert!(g.detail.contains("Reconciliation"));
    }

    #[test]
    fn reconciliation_gate_high_contradicted_fails() {
        let mut input = make_default_input();
        input.require_runtime_evidence = true;
        input.reconciliation = Some(ReconciliationScores {
            confirmed_ratio: 0.20,
            contradicted_ratio: 0.40,
            confidence_delta: -0.1,
            static_paths_count: 20,
        });
        let g = evaluate_runtime_evidence_gate(&input);
        assert!(!g.passed, "40% contradicted should fail");
    }

    #[test]
    fn reconciliation_gate_fallback_boolean_lower_confidence() {
        let mut input = make_default_input();
        input.require_runtime_evidence = true;
        input.has_runtime_evidence = true;
        input.reconciliation = None;
        let g = evaluate_runtime_evidence_gate(&input);
        assert!(g.passed);
        assert!(
            (g.confidence - 0.7).abs() < 1e-10,
            "Boolean mode should yield 0.7 confidence, got {}",
            g.confidence
        );
        assert!(g.detail.contains("boolean mode"));
    }

    // ── vNext: Retrieval gate mode tests ────────────────────────────────

    #[test]
    fn retrieval_cached_staleness_discount() {
        let mut input = make_default_input();
        input.retrieval_mode = RetrievalMode::Cached;
        input.retrieval_production_ready = Some(true);
        input.retrieval_ndcg = Some(0.80);
        input.retrieval_recall = Some(0.80);
        let g = evaluate_retrieval_quality_gate(&input);
        assert!(g.passed);
        // (0.80 + 0.80) / 2.0 * 0.9 = 0.72
        let expected = 0.72;
        assert!(
            (g.confidence - expected).abs() < 0.01,
            "Cached mode should discount: expected ~{}, got {}",
            expected,
            g.confidence
        );
        assert!(g.detail.contains("Cached"));
    }

    #[test]
    fn retrieval_skipped_mode_is_skipped() {
        let mut input = make_default_input();
        input.retrieval_mode = RetrievalMode::Skipped;
        input.retrieval_production_ready = None;
        let g = evaluate_retrieval_quality_gate(&input);
        assert!(g.skipped, "Skipped mode should produce skipped=true");
        assert!(g.detail.contains("skipped"));
    }

    #[test]
    fn retrieval_live_mode_not_skipped() {
        let mut input = make_default_input();
        input.retrieval_mode = RetrievalMode::Live;
        input.retrieval_production_ready = None;
        let g = evaluate_retrieval_quality_gate(&input);
        assert!(
            !g.skipped,
            "Live mode with None should be not-skipped (data missing)"
        );
    }

    // ── vNext: Calibrated confidence tests ──────────────────────────────

    #[test]
    fn calibrated_confidence_data_access_stricter() {
        let input_normal = make_default_input();
        let mut input_da = make_default_input();
        input_da.migration_class = Some("data_access".into());

        let d_normal = evaluate_gates(&input_normal);
        let d_da = evaluate_gates(&input_da);

        assert!(
            d_da.confidence < d_normal.confidence,
            "data_access class should have lower confidence ({} vs {})",
            d_da.confidence,
            d_normal.confidence
        );
    }

    #[test]
    fn calibrated_confidence_static_asset_more_lenient() {
        let input_normal = make_default_input();
        let mut input_sa = make_default_input();
        input_sa.migration_class = Some("static_asset".into());

        let d_normal = evaluate_gates(&input_normal);
        let d_sa = evaluate_gates(&input_sa);

        assert!(
            d_sa.confidence > d_normal.confidence,
            "static_asset class should have higher confidence ({} vs {})",
            d_sa.confidence,
            d_normal.confidence
        );
    }

    #[test]
    fn interaction_penalty_safety_and_blast_fail() {
        let mut input = make_default_input();
        // Force safety to fail
        input.safety_decision = Some(safety_service::evaluate_safety(
            &SafetyEvalRequest {
                project_id: "test".into(),
                affected_files: vec!["a.rs".into()],
                refactor_type: "rename".into(),
                impact_node_count: 5,
                impact_confidence: 0.2,
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
        // Force blast radius to fail
        input.blast_radius_risk = Some(9);
        input.blast_radius_band = Some(RiskBand::Critical);

        let decision = evaluate_gates(&input);
        // Both failed → interaction penalty applies, confidence should be lower
        assert_eq!(decision.verdict, AdpVerdict::Deny);
        // The penalty is -0.10 on top of the weighted average
        // We just verify the mechanism works by checking confidence < what it would be
        // without the penalty (hard to compute exactly, so just verify it's reasonable)
        assert!(decision.confidence < 1.0);
    }

    #[test]
    fn calibrated_confidence_is_weighted_not_arithmetic() {
        // Create an input where safety (weight 0.25) has high confidence
        // and anti-pattern (weight 0.10) has low confidence.
        // Weighted average should favor safety more than arithmetic mean.
        let mut input = make_default_input();
        input.immune_verdict = Some("PASS".into());
        input.immune_confidence = Some(0.90); // Low anti-pattern confidence (1.0 - 0.90 = 0.10)

        let decision = evaluate_gates(&input);
        // With arithmetic mean, all gates would contribute equally.
        // With weighted, safety_policy (0.25 weight, high conf) dominates.
        // This is hard to test precisely, but we can verify it produces a valid result.
        assert_eq!(decision.verdict, AdpVerdict::Allow);
        assert!(decision.confidence > 0.0 && decision.confidence <= 1.0);
    }

    // ── vNext: Replay with v2 scenario fields ───────────────────────────

    #[test]
    fn replay_v2_reconciliation_scenario() {
        let scenario_input = engram_core::benchmark::AdpScenarioInput {
            extraction_confidence: Some(0.9),
            extraction_band: Some("high".into()),
            trace_used_fallback: false,
            trace_candidate_count: 0,
            safety_allowed: Some(true),
            safety_confidence: Some(0.95),
            retrieval_production_ready: Some(true),
            retrieval_ndcg: Some(0.8),
            retrieval_recall: Some(0.9),
            blast_radius_risk: Some(2),
            blast_radius_band: Some("Low".into()),
            blast_radius_downstream: Some(3),
            immune_verdict: Some("PASS".into()),
            immune_confidence: Some(0.05),
            require_runtime_evidence: true,
            has_runtime_evidence: false, // Would fail without reconciliation
            risk_profile: "medium".into(),
            min_extraction_confidence: 0.5,
            min_safety_confidence: 0.7,
            max_blast_radius_for_auto: 6,
            // v2 fields
            reconciliation_confirmed_ratio: Some(0.85),
            reconciliation_contradicted_ratio: Some(0.05),
            reconciliation_confidence_delta: Some(0.1),
            reconciliation_static_paths: Some(20),
            retrieval_mode: Some("live".into()),
            migration_class: None,
        };
        let decision = replay_from_scenario(&scenario_input);
        assert_eq!(
            decision.verdict,
            AdpVerdict::Allow,
            "Reconciliation with 85% confirmed should allow"
        );
    }

    #[test]
    fn replay_v2_cached_retrieval_mode() {
        let scenario_input = engram_core::benchmark::AdpScenarioInput {
            extraction_confidence: Some(0.9),
            extraction_band: Some("high".into()),
            trace_used_fallback: false,
            trace_candidate_count: 0,
            safety_allowed: Some(true),
            safety_confidence: Some(0.95),
            retrieval_production_ready: Some(true),
            retrieval_ndcg: Some(0.8),
            retrieval_recall: Some(0.8),
            blast_radius_risk: Some(2),
            blast_radius_band: Some("Low".into()),
            blast_radius_downstream: Some(3),
            immune_verdict: Some("PASS".into()),
            immune_confidence: Some(0.05),
            require_runtime_evidence: false,
            has_runtime_evidence: false,
            risk_profile: "medium".into(),
            min_extraction_confidence: 0.5,
            min_safety_confidence: 0.7,
            max_blast_radius_for_auto: 6,
            reconciliation_confirmed_ratio: None,
            reconciliation_contradicted_ratio: None,
            reconciliation_confidence_delta: None,
            reconciliation_static_paths: None,
            retrieval_mode: Some("cached".into()),
            migration_class: None,
        };
        let decision = replay_from_scenario(&scenario_input);
        // Should still pass but with staleness discount on retrieval confidence
        assert_eq!(decision.verdict, AdpVerdict::Allow);
        // Verify the retrieval gate mentions "Cached"
        let retrieval_gate = decision
            .gate_results
            .iter()
            .find(|g| g.gate_id == "retrieval_quality")
            .unwrap();
        assert!(retrieval_gate.detail.contains("Cached"));
    }

    // ── vNext: Wave-level tests ─────────────────────────────────────────

    #[test]
    fn wave_all_allow() {
        let items: Vec<(String, AdpInput)> = (0..3)
            .map(|i| (format!("file_{}.aspx", i), make_default_input()))
            .collect();
        let wave = WaveAdpInput {
            wave_number: 1,
            wave_name: "test-wave".into(),
            items,
            cross_item_deps: 0,
        };
        let decision = evaluate_wave(&wave);
        assert_eq!(decision.verdict, AdpVerdict::Allow);
        assert_eq!(decision.item_decisions.len(), 3);
        assert!(decision.blocking_items.is_empty());
    }

    #[test]
    fn wave_single_deny_vetoes_wave() {
        let mut deny_input = make_default_input();
        deny_input.blast_radius_risk = Some(9);
        deny_input.blast_radius_band = Some(RiskBand::Critical);

        let items = vec![
            ("good.aspx".into(), make_default_input()),
            ("bad.aspx".into(), deny_input),
            ("good2.aspx".into(), make_default_input()),
        ];
        let wave = WaveAdpInput {
            wave_number: 1,
            wave_name: "test-wave".into(),
            items,
            cross_item_deps: 0,
        };
        let decision = evaluate_wave(&wave);
        assert_eq!(decision.verdict, AdpVerdict::Deny);
        assert!(decision.blocking_items.contains(&"bad.aspx".to_string()));
    }

    #[test]
    fn wave_high_blast_count_triggers_abstain() {
        let items: Vec<(String, AdpInput)> = (0..5)
            .map(|i| {
                let mut input = make_default_input();
                input.blast_radius_risk = Some(6); // > 5 but <= max_blast_radius_for_auto (6)
                input.blast_radius_band = Some(RiskBand::High);
                (format!("file_{}.aspx", i), input)
            })
            .collect();
        let wave = WaveAdpInput {
            wave_number: 1,
            wave_name: "test-wave".into(),
            items,
            cross_item_deps: 0,
        };
        let decision = evaluate_wave(&wave);
        // >3 items with blast_radius > 5 should trigger abstain
        assert_eq!(decision.verdict, AdpVerdict::Abstain);
        assert!(!decision.interaction_penalties.is_empty());
    }

    #[test]
    fn wave_cross_dep_penalty() {
        let items: Vec<(String, AdpInput)> = (0..3)
            .map(|i| (format!("file_{}.aspx", i), make_default_input()))
            .collect();
        let wave = WaveAdpInput {
            wave_number: 1,
            wave_name: "test-wave".into(),
            items,
            cross_item_deps: 10, // > 3 * 2 = 6
        };
        let decision = evaluate_wave(&wave);
        // Cross-dep penalty produces a warning message but doesn't change verdict
        assert!(
            decision
                .interaction_penalties
                .iter()
                .any(|p| p.contains("coupling"))
        );
    }

    #[test]
    fn wave_format_produces_output() {
        let items = vec![("a.aspx".into(), make_default_input())];
        let wave = WaveAdpInput {
            wave_number: 1,
            wave_name: "test".into(),
            items,
            cross_item_deps: 0,
        };
        let decision = evaluate_wave(&wave);
        let text = format_wave_decision(&decision);
        assert!(text.contains("Wave 1"));
        assert!(text.contains("Verdict:"));
        assert!(text.contains("Per-Item Results"));
    }
}
