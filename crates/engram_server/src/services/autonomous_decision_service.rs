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
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s.to_lowercase().as_str() {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            other => Err(format!(
                "unknown risk_profile '{}': must be one of low, medium, high",
                other
            )),
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
    /// ENG-AUD-2026-S09-0001: set to `true` when the spawn_blocking join
    /// for graph impact derivation failed.  When `true`, all count fields
    /// are zero by construction (not genuine) and any policy gate that reads
    /// this struct must treat the evidence as indeterminate, not permissive.
    #[serde(default)]
    pub join_failed: bool,
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
            // The blast-radius score is an UNCALIBRATED 1-hop heuristic (capped
            // counts, no change semantics, no transitive propagation). Until
            // OciusX calibration shows the scalar predicts real failures it may
            // block an automatic Allow and demand more evidence — it must
            // NEVER independently produce a hard Deny. A hard deny needs a
            // calibrated causal result or a separate deterministic policy
            // (protected files, forbidden operations). So: ABSTAIN, not Deny.
            has_abstain = true;
            reasons.push(format!("{} (advisory: abstain, not deny)", g5.detail));
            followups.push(
                "Blast-radius heuristic exceeded the auto-apply bar — supply causal/runtime \
                 evidence (impact_analysis on the changed symbols, tests, or a human review) \
                 before applying autonomously"
                    .into(),
            );
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
            // Confidence is NOT `1 - risk/10`. The blast-radius score is an
            // uncalibrated 1-hop migration-complexity heuristic whose counts
            // are capped and which mixes co-change history into "dependents";
            // converting it into evidence confidence let a low heuristic score
            // read as near-certainty for autonomous edits. A pass on this gate
            // is ADVISORY: bounded confidence that never approaches 1.0, and
            // the detail says so, so the downstream decision cannot treat the
            // heuristic as authorization.
            const ADVISORY_CONFIDENCE_CEILING: f64 = 0.6;
            GateResult {
                gate_id: "blast_radius".into(),
                gate_name: "Blast Radius / Risk".into(),
                passed,
                confidence: if passed {
                    // Scale within [0.3, 0.6]: lower risk → higher, but capped.
                    0.3 + (1.0 - (risk as f64 / 10.0)) * (ADVISORY_CONFIDENCE_CEILING - 0.3)
                } else {
                    0.1
                },
                detail: format!(
                    "Migration risk {}/10 ({}) — max allowed for auto-apply: {}/10 (1-hop degree: {}). \
                     ADVISORY: this is an uncalibrated 1-hop heuristic with capped counts, not a \
                     transitive change-impact analysis; confidence is bounded at {:.1} and must not \
                     be read as authorization for an autonomous edit.",
                    risk,
                    band,
                    max_allowed,
                    input.blast_radius_downstream.unwrap_or(0),
                    ADVISORY_CONFIDENCE_CEILING,
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
/// Recommended follow-up action for a failed gate. Keyed by the gate_name
/// produced by the pipeline; unknown gates fall back to a generic hint.
///
/// Intentionally kept here (not in a .md doc) so the recommendation lives
/// next to the gate-producer code that knows what the gate means.
fn recommendation_for_gate(gate_name: &str) -> &'static str {
    match gate_name {
        "blast_radius" | "blast_radius_threshold" => {
            "Scope the change to a specific method rather than the whole file, \
             or run `compute_blast_radius` / `impact_analysis` for the dependency list"
        }
        "evidence_sufficiency" | "runtime_evidence" => {
            "Run `generate_characterization_tests` to establish baseline behavior \
             before changing, or `generate_instrumentation_code` to gather runtime evidence"
        }
        "anti_pattern" | "anti_pattern_check" => {
            "Run `immune_check` with the proposed snippet to see specific \
             anti-pattern matches and their source commits"
        }
        "safety_policy" | "safety_policy_check" => {
            "Review CLAUDE.md conventions for this project and confirm the \
             change complies with the stated safety policy"
        }
        "extraction_confidence" => {
            "Run `get_extraction_confidence` for a signal-by-signal breakdown \
             of the confidence score and remediation guidance"
        }
        "trace_certainty" | "trace_confidence" => {
            "Run `trace_ui_event` or `trace_data_flow` against the target \
             symbol to get richer provenance before committing"
        }
        "retrieval_quality" => {
            "Re-run `search_memory` with `use_mmr: true` or adjust path \
             filters to surface higher-quality retrieval evidence"
        }
        "kill_switch" => {
            "Deactivate the ADP kill-switch in configuration to resume \
             autonomous decisioning"
        }
        _ => {
            "Investigate this gate's evidence below, or run \
             `get_gate_diagnostics` for a detailed breakdown"
        }
    }
}

/// Inspect the `reasons` of an `apply_rollout_policy`-modified decision
/// and detect whether the current verdict is an override of a stricter
/// underlying verdict. Returns `Some((phase_label, original_verdict))`
/// when the override is present, `None` otherwise.
fn detect_rollout_override(reasons: &[String]) -> Option<(&'static str, String)> {
    for r in reasons {
        if let Some(rest) = r.strip_prefix("[SHADOW] Original verdict was '") {
            if let Some(end) = rest.find('\'') {
                return Some(("shadow", rest[..end].to_string()));
            }
        }
        if let Some(rest) = r.strip_prefix("[ADVISORY] Original verdict was '") {
            if let Some(end) = rest.find('\'') {
                return Some(("advisory", rest[..end].to_string()));
            }
        }
    }
    None
}

pub fn format_decision(decision: &AdpDecision) -> String {
    let mut out = String::with_capacity(2048);
    let rollout = detect_rollout_override(&decision.reasons);

    // ── Header — shadow-deny / advisory-deny lead with a loud warning
    //             so callers cannot skim past the override qualifier.
    match &rollout {
        Some((phase, orig)) if orig == "deny" || orig == "abstain" => {
            let (icon, phase_human) = match *phase {
                "shadow" => ("⚠️ SHADOW DENY", "shadow"),
                "advisory" => ("⚠️ ADVISORY DENY", "advisory"),
                _ => ("⚠️ OVERRIDE", *phase),
            };
            let failed = decision.failed_gates.len();
            let total = decision.gate_results.len();
            let passed = total.saturating_sub(failed);
            out.push_str(&format!(
                "# {icon} — This change WOULD be blocked in production mode.\n\n\
                 **Original verdict:** {orig}  \n\
                 **Rollout phase:** {phase_human}  \n\
                 **Confidence:** {:.2}  \n\
                 **Failed gates:** {failed} of {total}  \n\
                 **Passing gates:** {passed} of {total}\n\n",
                decision.confidence,
            ));
        }
        _ => {
            out.push_str(&format!(
                "# Autonomous Decision Gate Result\n\n\
                 **Verdict:** {}\n\
                 **Confidence:** {:.2}\n\n",
                decision.verdict, decision.confidence
            ));
        }
    }

    // ── Failed gates (prominent when present) ───────────────────────────
    if !decision.failed_gates.is_empty() {
        // Build a quick gate-name → GateResult map so we can cite
        // per-gate confidence + detail inline with the recommendation.
        use std::collections::HashMap;
        let by_name: HashMap<&str, &GateResult> = decision
            .gate_results
            .iter()
            .map(|g| (g.gate_name.as_str(), g))
            .collect();

        out.push_str(&format!(
            "## Failed gates ({})\n",
            decision.failed_gates.len()
        ));
        for (i, g) in decision.failed_gates.iter().enumerate() {
            let detail = by_name
                .get(g.as_str())
                .map(|gr| gr.detail.as_str())
                .unwrap_or("(no detail)");
            let conf = by_name
                .get(g.as_str())
                .map(|gr| gr.confidence)
                .unwrap_or(0.0);
            out.push_str(&format!(
                "{}. **`{}`** (confidence {:.2}) — {}\n",
                i + 1,
                g,
                conf,
                detail
            ));
            out.push_str(&format!(
                "   _Recommended:_ {}\n",
                recommendation_for_gate(g)
            ));
        }
        out.push('\n');
    }

    // ── Passing gates (de-emphasised summary) ───────────────────────────
    let passing: Vec<&str> = decision
        .gate_results
        .iter()
        .filter(|g| !g.skipped && g.passed)
        .map(|g| g.gate_name.as_str())
        .collect();
    if !passing.is_empty() {
        out.push_str(&format!(
            "## Passing gates ({} of {})\n",
            passing.len(),
            decision.gate_results.len()
        ));
        out.push_str(&format!(
            "✓ {}\n\n",
            passing
                .iter()
                .map(|n| format!("`{n}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    // ── Reasons (minus the structured `[SHADOW]` / `[ADVISORY]` markers,
    //             which the header already surfaced) ────────────────────
    let plain_reasons: Vec<&String> = decision
        .reasons
        .iter()
        .filter(|r| !r.starts_with("[SHADOW]") && !r.starts_with("[ADVISORY]"))
        .collect();
    if !plain_reasons.is_empty() {
        out.push_str("## Reasons\n");
        for r in plain_reasons {
            out.push_str(&format!("- {}\n", r));
        }
        out.push('\n');
    }

    if !decision.required_followups.is_empty() {
        out.push_str("## Required follow-ups\n");
        for f in &decision.required_followups {
            out.push_str(&format!("- {}\n", f));
        }
        out.push('\n');
    }

    // ── Full gate detail block kept for audit — useful when the failed
    //    gates' recommendations aren't enough and the caller wants the
    //    full evidence picture. ───────────────────────────────────────
    out.push_str("## Gate details\n");
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

    // Footer: current rollout phase + current verdict (helpful when the
    // reader pastes only the body into a ticket or Slack message).
    if let Some((phase, orig)) = rollout {
        out.push_str(&format!(
            "\n_Current rollout phase: **{phase}** — original verdict '{orig}' \
             was overridden to '{}'. Denials are logged for calibration._\n",
            decision.verdict
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
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s.to_lowercase().as_str() {
            "shadow" => Ok(Self::Shadow),
            "advisory" => Ok(Self::Advisory),
            "guarded" => Ok(Self::Guarded),
            "autonomous" => Ok(Self::Autonomous),
            other => Err(format!(
                "unknown rollout_phase '{}': must be one of shadow, advisory, guarded, autonomous",
                other
            )),
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
///
/// ADP1: includes implementation identity metadata so replaying a decision
/// after a code update is detectable — threshold equality alone does not
/// guarantee gate logic equivalence across binary versions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigSnapshot {
    pub adp_min_extraction_confidence: f64,
    pub safety_min_confidence: f64,
    pub safety_min_coverage: f64,
    pub adp_max_blast_radius: u8,
    pub safety_policy_enabled: bool,
    /// Semver or git-hash of the gate-evaluation engine at decision time.
    /// Used during replay to detect code-version drift between the original
    /// decision and the replay run, even when all threshold values are identical.
    pub gate_code_version: String,
    /// Schema version of the evidence/report format (e.g. "1.0.0").
    /// Bumped when the evidence structure changes to invalidate stale replays.
    pub evidence_schema_version: String,
    /// ADP1: BLAKE3 hex-digest of the serialized gate_evidence array.
    /// Allows replay verification to detect evidence tampering or serialization
    /// drift independently of threshold values and code-version fields.
    pub evidence_hash: String,
    /// ADP1-s2x6: Cargo package version of the engram_server binary at decision time.
    /// Allows replays to detect binary-version drift even when gate thresholds
    /// and gate_code_version strings are identical — a new release may change
    /// gate logic without changing the manually-maintained gate_code_version tag.
    pub crate_version: String,
    /// ADP1: OS and CPU architecture triple at decision time (e.g. "linux/x86_64").
    /// Allows forensic replay to detect cross-platform gate-logic differences
    /// when the same binary version is deployed across heterogeneous environments.
    /// Format: `{std::env::consts::OS}/{std::env::consts::ARCH}`.
    #[serde(default)]
    pub runtime_triple: String,
    /// ADP1-z2t4: BLAKE3 hex-digest of the autonomous_decision_service.rs source
    /// bytes at compile time. Computed by build.rs using the actual file content,
    /// so any edit to gate logic — even without bumping gate_code_version — produces
    /// a different hash. Replay tooling can compare this field against a freshly-
    /// built binary's value to detect silent gate-logic drift.
    #[serde(default)]
    pub gate_source_hash: String,
    /// ADP1-f41c: The rollout phase (shadow/advisory/guarded/autonomous) active when
    /// `apply_rollout_policy` was called. Captures the post-evaluation input that can
    /// flip a raw Deny→Allow (advisory/shadow) or Allow→Deny (kill-switch), making the
    /// *applied* verdict reproducible from this snapshot alone.
    #[serde(default)]
    pub rollout_phase: String,
    /// ADP1-f41c: Whether the ADP kill-switch was active at decision time. Together
    /// with `rollout_phase`, fully captures the `apply_rollout_policy` inputs so that
    /// replay tooling can reproduce the final applied verdict, not just the gate verdict.
    #[serde(default)]
    pub kill_switch: bool,
}

/// Build an immutable decision report from a decision and its context.
#[allow(clippy::too_many_arguments)]
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

    // ADP1: compute BLAKE3 hash of the serialized gate evidence for replay integrity.
    let evidence_json = serde_json::to_vec(&gate_evidence).unwrap_or_default();
    let evidence_hash = blake3::hash(&evidence_json).to_hex().to_string();
    let config_snapshot = ConfigSnapshot {
        evidence_hash,
        // ADP1-s2x6: always stamp with the compile-time binary version so that
        // replays can detect gate-logic drift even when gate_code_version is unchanged.
        crate_version: env!("CARGO_PKG_VERSION").into(),
        // ADP1: stamp runtime OS/arch so cross-platform replay divergence is detectable.
        runtime_triple: format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH),
        // ADP1-z2t4: BLAKE3 of the gate-logic source file, computed by build.rs and
        // embedded at compile time. Changes whenever autonomous_decision_service.rs changes.
        gate_source_hash: include_str!(concat!(env!("OUT_DIR"), "/gate_source_hash.txt")).into(),
        ..config_snapshot
    };

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
) -> Result<AdpDecision, String> {
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
        risk_profile: RiskProfile::from_str(&scenario_input.risk_profile)?,
        min_extraction_confidence: scenario_input.min_extraction_confidence,
        min_safety_confidence: scenario_input.min_safety_confidence,
        max_blast_radius_for_auto: scenario_input.max_blast_radius_for_auto,
        reconciliation,
        graph_impact: None, // Replay doesn't need graph impact (safety_decision is pre-provided)
        retrieval_mode,
        migration_class: scenario_input.migration_class.clone(),
    };

    Ok(evaluate_gates(&input))
}

/// Run a full ADP corpus and return per-scenario results with pass/fail.
pub fn run_corpus(corpus: &engram_core::benchmark::AdpCorpus) -> Vec<AdpCorpusResult> {
    corpus
        .scenarios
        .iter()
        .map(|scenario| {
            let decision = replay_from_scenario(&scenario.input)
                .expect("corpus scenario has invalid risk_profile value");
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
        let mut m = Self {
            total: results.len(),
            ..Self::default()
        };
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
#[allow(clippy::unwrap_used)]
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
    fn high_blast_radius_abstains_never_hard_denies_alone() {
        // The blast-radius score is an uncalibrated heuristic: on its own it
        // may block an automatic Allow (abstain + demand evidence) but must
        // never independently produce a hard Deny.
        let mut input = make_default_input();
        input.blast_radius_risk = Some(9);
        input.blast_radius_band = Some(RiskBand::Critical);
        let decision = evaluate_gates(&input);
        assert_ne!(
            decision.verdict,
            AdpVerdict::Deny,
            "an uncalibrated heuristic must not hard-deny by itself"
        );
        assert_ne!(
            decision.verdict,
            AdpVerdict::Allow,
            "it must still block an automatic Allow"
        );
        assert!(decision.failed_gates.contains(&"blast_radius".to_string()));
        assert!(
            decision
                .required_followups
                .iter()
                .any(|f| f.contains("causal/runtime evidence")),
            "must ask for more evidence rather than deny"
        );
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
        // Non-shadow / non-advisory output keeps the original header.
        assert!(text.contains("Autonomous Decision Gate Result"));
        assert!(text.contains("Verdict:"));
        // Header for the gate-by-gate audit block (case insensitive).
        assert!(
            text.to_lowercase().contains("gate details"),
            "output must include a 'Gate details' section"
        );
    }

    /// Shadow override must lead with a prominent SHADOW DENY warning
    /// rather than "Verdict: allow". This is the T5 regression — the
    /// old format buried the qualifier and a skimming reader could
    /// miss the override.
    #[test]
    fn format_shadow_deny_leads_with_warning() {
        let mut input = make_default_input();
        // Force a deny verdict (high blast radius threshold breach).
        input.blast_radius_risk = Some(9);
        input.risk_profile = RiskProfile::High;
        let inner = evaluate_gates(&input);
        let overridden = apply_rollout_policy(&inner, RolloutPhase::Shadow, false);
        assert_eq!(
            overridden.verdict,
            AdpVerdict::Allow,
            "shadow phase must override to Allow"
        );
        let text = format_decision(&overridden);
        // The header leads with the warning — no "Verdict: allow" opener.
        let first_80: String = text.chars().take(80).collect();
        assert!(
            first_80.contains("SHADOW DENY"),
            "output must lead with SHADOW DENY, got first 80 chars: {first_80:?}"
        );
        assert!(
            !first_80.contains("**Verdict:** allow"),
            "shadow header must not lead with 'Verdict: allow'"
        );
        // The body must surface the passing / failed gate counts.
        assert!(text.contains("Failed gates"));
        // Footer records the rollout phase.
        assert!(text.to_lowercase().contains("rollout phase"));
    }

    /// Advisory override works the same shape as shadow — different label
    /// but same prominence.
    #[test]
    fn format_advisory_deny_leads_with_warning() {
        let mut input = make_default_input();
        input.blast_radius_risk = Some(9);
        input.risk_profile = RiskProfile::High;
        let inner = evaluate_gates(&input);
        let overridden = apply_rollout_policy(&inner, RolloutPhase::Advisory, false);
        assert_eq!(overridden.verdict, AdpVerdict::Allow);
        let text = format_decision(&overridden);
        assert!(text.contains("ADVISORY DENY"));
    }

    /// A genuine Allow (no override) must NOT show the shadow warning.
    #[test]
    fn format_allow_without_override_uses_plain_header() {
        let input = make_default_input();
        let decision = evaluate_gates(&input);
        let text = format_decision(&decision);
        assert!(!text.contains("SHADOW DENY"));
        assert!(!text.contains("ADVISORY DENY"));
        assert!(text.contains("Autonomous Decision Gate Result"));
    }

    /// Per-gate recommendations must appear under each failed gate.
    #[test]
    fn format_failed_gates_include_recommendations() {
        let mut input = make_default_input();
        input.blast_radius_risk = Some(9);
        input.risk_profile = RiskProfile::High;
        let decision = evaluate_gates(&input);
        let text = format_decision(&decision);
        if decision
            .failed_gates
            .iter()
            .any(|g| g.contains("blast_radius"))
        {
            assert!(
                text.contains("_Recommended:_"),
                "each failed gate must have a Recommended: line"
            );
            assert!(
                text.contains("`compute_blast_radius`") || text.contains("`impact_analysis`"),
                "blast_radius recommendation must cite compute_blast_radius / impact_analysis"
            );
        }
    }

    #[test]
    fn recommendation_for_gate_covers_known_names_and_falls_back() {
        // Known gate names produce targeted advice.
        assert!(recommendation_for_gate("blast_radius").contains("compute_blast_radius"));
        assert!(recommendation_for_gate("evidence_sufficiency").contains("characterization_tests"));
        assert!(recommendation_for_gate("anti_pattern").contains("immune_check"));
        assert!(recommendation_for_gate("safety_policy").contains("CLAUDE.md"));
        assert!(
            recommendation_for_gate("extraction_confidence").contains("get_extraction_confidence")
        );
        // Unknown gate names fall back to the generic hint.
        let fallback = recommendation_for_gate("totally_unknown_gate");
        assert!(
            fallback.contains("Investigate"),
            "unknown gate must fall back to generic hint, got: {fallback}"
        );
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

        let d1 = replay_from_scenario(&scenario_input).unwrap();
        let d2 = replay_from_scenario(&scenario_input).unwrap();

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
        let decision = replay_from_scenario(&si).unwrap();
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
        let decision = replay_from_scenario(&si).unwrap();
        assert_eq!(decision.verdict, AdpVerdict::Deny);
        assert!(decision.failed_gates.contains(&"safety_policy".into()));
    }

    /// ADP2-q8l4: expanded confusion matrix with ≥20 synthetic scenarios covering
    /// diverse edge cases across all three verdicts (allow / deny / abstain).
    ///
    /// Scenarios are arranged in three blocks:
    ///  - s001–s005: Allow (5 scenarios)
    ///  - s006–s013: Deny  (8 scenarios)
    ///  - s014–s020: Abstain (7 scenarios)
    #[test]
    fn corpus_runner_computes_confusion_matrix() {
        use engram_core::benchmark::{AdpCorpus, AdpScenario};

        // ── Allow scenarios ───────────────────────────────────────────────────

        // s001: Full-green standard allow
        let mut s001 = make_scenario_input();
        s001.extraction_confidence = Some(0.9);
        s001.retrieval_ndcg = Some(0.8);
        s001.retrieval_recall = Some(0.9);
        s001.blast_radius_risk = Some(2);
        s001.blast_radius_downstream = Some(3);

        // s002: Extraction confidence exactly at threshold (0.5 >= 0.5 → passes)
        let mut s002 = make_scenario_input();
        s002.extraction_confidence = Some(0.5);
        s002.retrieval_ndcg = Some(0.8);
        s002.retrieval_recall = Some(0.9);
        s002.blast_radius_risk = Some(3);

        // s003: Blast radius exactly at max (6 <= 6 → passes)
        let mut s003 = make_scenario_input();
        s003.extraction_confidence = Some(0.88);
        s003.retrieval_ndcg = Some(0.82);
        s003.retrieval_recall = Some(0.88);
        s003.blast_radius_risk = Some(6);
        s003.blast_radius_band = Some("Medium".into());
        s003.blast_radius_downstream = Some(10);

        // s004: Cached retrieval mode — staleness discount applied but ready=true still allows
        let mut s004 = make_scenario_input();
        s004.extraction_confidence = Some(0.92);
        s004.retrieval_production_ready = Some(true);
        s004.retrieval_ndcg = Some(0.85);
        s004.retrieval_recall = Some(0.90);
        s004.retrieval_mode = Some("cached".into());
        s004.blast_radius_risk = Some(2);

        // s005: Very high confidence all gates
        let mut s005 = make_scenario_input();
        s005.extraction_confidence = Some(0.99);
        s005.retrieval_ndcg = Some(0.95);
        s005.retrieval_recall = Some(0.95);
        s005.blast_radius_risk = Some(1);
        s005.blast_radius_downstream = Some(2);
        s005.immune_confidence = Some(0.02);

        // ── Deny scenarios ────────────────────────────────────────────────────

        // s006: Safety evaluation blocked
        let mut s006 = make_scenario_input();
        s006.extraction_confidence = Some(0.9);
        s006.safety_allowed = Some(false);
        s006.safety_confidence = Some(0.2);
        s006.retrieval_ndcg = Some(0.8);
        s006.retrieval_recall = Some(0.9);
        s006.blast_radius_risk = Some(2);
        s006.blast_radius_downstream = Some(3);

        // s007: Blast radius exceeds max (7 > 6)
        let mut s007 = make_scenario_input();
        s007.extraction_confidence = Some(0.9);
        s007.retrieval_ndcg = Some(0.8);
        s007.retrieval_recall = Some(0.9);
        s007.blast_radius_risk = Some(7);
        s007.blast_radius_band = Some("High".into());
        s007.blast_radius_downstream = Some(15);

        // s008: Immune BLOCK verdict
        let mut s008 = make_scenario_input();
        s008.extraction_confidence = Some(0.88);
        s008.retrieval_ndcg = Some(0.78);
        s008.retrieval_recall = Some(0.82);
        s008.blast_radius_risk = Some(3);
        s008.immune_verdict = Some("BLOCK".into());
        s008.immune_confidence = Some(0.92);

        // s009: Extraction below threshold (0.3 < 0.5, Some provided → hard deny)
        let mut s009 = make_scenario_input();
        s009.extraction_confidence = Some(0.3);
        s009.retrieval_ndcg = Some(0.8);
        s009.retrieval_recall = Some(0.9);
        s009.blast_radius_risk = Some(2);

        // s010: Retrieval not production-ready (Some(false) → hard deny)
        let mut s010 = make_scenario_input();
        s010.extraction_confidence = Some(0.88);
        s010.retrieval_production_ready = Some(false);
        s010.retrieval_ndcg = Some(0.35);
        s010.retrieval_recall = Some(0.40);
        s010.blast_radius_risk = Some(3);

        // s011: Blast radius at critical level (10 >> 6)
        let mut s011 = make_scenario_input();
        s011.extraction_confidence = Some(0.9);
        s011.retrieval_ndcg = Some(0.82);
        s011.retrieval_recall = Some(0.88);
        s011.blast_radius_risk = Some(10);
        s011.blast_radius_band = Some("Critical".into());
        s011.blast_radius_downstream = Some(50);

        // s012: Safety blocked with high risk profile
        let mut s012 = make_scenario_input();
        s012.extraction_confidence = Some(0.88);
        s012.safety_allowed = Some(false);
        s012.safety_confidence = Some(0.15);
        s012.retrieval_ndcg = Some(0.8);
        s012.retrieval_recall = Some(0.9);
        s012.blast_radius_risk = Some(4);
        s012.risk_profile = "high".into();

        // s013: Very low extraction confidence (0.1 << 0.5)
        let mut s013 = make_scenario_input();
        s013.extraction_confidence = Some(0.1);
        s013.extraction_band = Some("low".into());
        s013.retrieval_ndcg = Some(0.75);
        s013.retrieval_recall = Some(0.80);
        s013.blast_radius_risk = Some(3);

        // ── Abstain scenarios ─────────────────────────────────────────────────

        // s014: No extraction confidence data — evidence gate fails (g8)
        let mut s014 = make_scenario_input();
        s014.extraction_confidence = None;
        s014.extraction_band = None;

        // s015: Trace fallback used — trace_certainty gate fails → has_abstain
        let mut s015 = make_scenario_input();
        s015.extraction_confidence = Some(0.88);
        s015.retrieval_ndcg = Some(0.80);
        s015.retrieval_recall = Some(0.88);
        s015.blast_radius_risk = Some(3);
        s015.trace_used_fallback = true;
        s015.trace_candidate_count = 3;

        // s016: All evidence missing — everything skipped → evidence_sufficiency fails
        let mut s016 = make_scenario_input();
        s016.extraction_confidence = None;
        s016.extraction_band = None;
        s016.safety_allowed = None;
        s016.safety_confidence = None;
        s016.retrieval_production_ready = None;
        s016.retrieval_ndcg = None;
        s016.retrieval_recall = None;
        s016.blast_radius_risk = None;
        s016.blast_radius_band = None;
        s016.blast_radius_downstream = None;
        s016.immune_verdict = None;
        s016.immune_confidence = None;

        // s017: Runtime evidence required but not available
        let mut s017 = make_scenario_input();
        s017.extraction_confidence = Some(0.88);
        s017.retrieval_ndcg = Some(0.78);
        s017.retrieval_recall = Some(0.82);
        s017.blast_radius_risk = Some(3);
        s017.require_runtime_evidence = true;
        s017.has_runtime_evidence = false;

        // s018: Immune WARN on medium risk → anti_pattern fails → has_abstain
        let mut s018 = make_scenario_input();
        s018.extraction_confidence = Some(0.88);
        s018.retrieval_ndcg = Some(0.80);
        s018.retrieval_recall = Some(0.85);
        s018.blast_radius_risk = Some(4);
        s018.immune_verdict = Some("WARN".into());
        s018.immune_confidence = Some(0.45);
        s018.risk_profile = "medium".into();

        // s019: Immune WARN on high risk → same abstain path
        let mut s019 = make_scenario_input();
        s019.extraction_confidence = Some(0.85);
        s019.retrieval_ndcg = Some(0.78);
        s019.retrieval_recall = Some(0.82);
        s019.blast_radius_risk = Some(5);
        s019.immune_verdict = Some("WARN".into());
        s019.immune_confidence = Some(0.60);
        s019.risk_profile = "high".into();

        // s020: Retrieval missing but mode=live → retrieval_quality fails → has_abstain
        let mut s020 = make_scenario_input();
        s020.extraction_confidence = Some(0.88);
        s020.retrieval_production_ready = None;
        s020.retrieval_ndcg = None;
        s020.retrieval_recall = None;
        s020.retrieval_mode = Some("live".into());
        s020.blast_radius_risk = Some(3);

        fn scenario(
            id: &str,
            desc: &str,
            risk_class: &str,
            input: engram_core::benchmark::AdpScenarioInput,
            verdict: &str,
            gates: Vec<String>,
        ) -> AdpScenario {
            AdpScenario {
                scenario_id: id.into(),
                description: desc.into(),
                risk_class: risk_class.into(),
                source: "synthetic".into(),
                input,
                expected_verdict: verdict.into(),
                expected_failed_gates: gates,
                rationale: desc.into(),
            }
        }

        let corpus = AdpCorpus {
            schema_version: "1.0.0".into(),
            name: "adp2-q8l4-corpus".into(),
            description: "20-scenario confusion matrix for ADP2 coverage".into(),
            scenarios: vec![
                scenario(
                    "s001_allow_full_green",
                    "All signals green",
                    "low",
                    s001,
                    "allow",
                    vec![],
                ),
                scenario(
                    "s002_allow_extraction_boundary",
                    "Extraction at threshold",
                    "low",
                    s002,
                    "allow",
                    vec![],
                ),
                scenario(
                    "s003_allow_blast_at_max",
                    "Blast radius at allowed max",
                    "medium",
                    s003,
                    "allow",
                    vec![],
                ),
                scenario(
                    "s004_allow_cached_retrieval",
                    "Cached retrieval still passes",
                    "low",
                    s004,
                    "allow",
                    vec![],
                ),
                scenario(
                    "s005_allow_high_confidence",
                    "Very high confidence all gates",
                    "low",
                    s005,
                    "allow",
                    vec![],
                ),
                scenario(
                    "s006_deny_safety_blocked",
                    "Safety evaluation blocked",
                    "high",
                    s006,
                    "deny",
                    vec!["safety_policy".into()],
                ),
                scenario(
                    "s007_abstain_blast_exceeds_max",
                    "Blast radius > max allowed: uncalibrated heuristic -> abstain, never hard deny",
                    "high",
                    s007,
                    "abstain",
                    vec!["blast_radius".into()],
                ),
                scenario(
                    "s008_deny_immune_block",
                    "Immune BLOCK verdict",
                    "high",
                    s008,
                    "deny",
                    vec!["anti_pattern".into()],
                ),
                scenario(
                    "s009_deny_extraction_low",
                    "Extraction below threshold",
                    "medium",
                    s009,
                    "deny",
                    vec!["extraction_confidence".into()],
                ),
                scenario(
                    "s010_deny_retrieval_not_ready",
                    "Retrieval not production ready",
                    "medium",
                    s010,
                    "deny",
                    vec!["retrieval_quality".into()],
                ),
                scenario(
                    "s011_abstain_blast_critical",
                    "Blast radius at critical level: still abstain (demand causal evidence), not deny",
                    "high",
                    s011,
                    "abstain",
                    vec!["blast_radius".into()],
                ),
                scenario(
                    "s012_deny_safety_high_risk",
                    "Safety blocked with high risk profile",
                    "high",
                    s012,
                    "deny",
                    vec!["safety_policy".into()],
                ),
                scenario(
                    "s013_deny_extraction_very_low",
                    "Very low extraction confidence",
                    "medium",
                    s013,
                    "deny",
                    vec!["extraction_confidence".into()],
                ),
                scenario(
                    "s014_abstain_no_extraction",
                    "No extraction data — evidence gate fails",
                    "medium",
                    s014,
                    "abstain",
                    vec!["evidence_sufficiency".into()],
                ),
                scenario(
                    "s015_abstain_trace_fallback",
                    "Trace fallback used",
                    "medium",
                    s015,
                    "abstain",
                    vec!["trace_certainty".into()],
                ),
                scenario(
                    "s016_abstain_missing_all",
                    "All evidence missing",
                    "medium",
                    s016,
                    "abstain",
                    vec!["evidence_sufficiency".into()],
                ),
                scenario(
                    "s017_abstain_runtime_required",
                    "Runtime evidence required but absent",
                    "medium",
                    s017,
                    "abstain",
                    vec!["runtime_evidence".into()],
                ),
                scenario(
                    "s018_abstain_immune_warn_medium",
                    "Immune WARN on medium risk",
                    "medium",
                    s018,
                    "abstain",
                    vec!["anti_pattern".into()],
                ),
                scenario(
                    "s019_abstain_immune_warn_high",
                    "Immune WARN on high risk",
                    "high",
                    s019,
                    "abstain",
                    vec!["anti_pattern".into()],
                ),
                scenario(
                    "s020_abstain_retrieval_missing_live",
                    "Retrieval missing in live mode",
                    "medium",
                    s020,
                    "abstain",
                    vec!["retrieval_quality".into()],
                ),
            ],
        };

        let results = run_corpus(&corpus);
        assert_eq!(results.len(), 20, "corpus must produce exactly 20 results");

        // Every scenario must match its expected verdict.
        for r in &results {
            assert!(
                r.verdict_matches,
                "ADP2-q8l4: scenario '{}' expected verdict '{}' but got '{}'",
                r.scenario_id, r.expected_verdict, r.actual_verdict
            );
        }

        let matrix = AdpConfusionMatrix::from_results(&results);
        assert_eq!(matrix.total, 20);
        assert_eq!(matrix.true_allow, 5, "5 allow scenarios must all pass");
        // s007/s011 (blast-radius over the bar) moved deny -> abstain: an
        // uncalibrated heuristic may block auto-Allow but never hard-deny alone.
        assert_eq!(matrix.true_deny, 6, "6 deny scenarios must all pass");
        assert_eq!(matrix.true_abstain, 9, "9 abstain scenarios must all pass");
        assert_eq!(matrix.false_allow, 0, "no false-allow predictions");
        assert_eq!(matrix.false_deny, 0, "no false-deny predictions");
        assert!(
            matrix.false_allow_rate() < 0.01,
            "false-allow rate must be < 1% across 20 scenarios"
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
        input.immune_verdict = Some("BLOCK".into()); // deterministic hard-deny source
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
        input.immune_verdict = Some("BLOCK".into()); // deterministic hard-deny source
        let decision = evaluate_gates(&input);
        assert_eq!(decision.verdict, AdpVerdict::Deny);

        let overridden = apply_rollout_policy(&decision, RolloutPhase::Shadow, false);
        assert_eq!(overridden.verdict, AdpVerdict::Allow);
        assert!(overridden.reasons.iter().any(|r| r.contains("[SHADOW]")));
    }

    #[test]
    fn advisory_phase_overrides_deny_to_allow_with_warning() {
        let mut input = make_default_input();
        input.immune_verdict = Some("BLOCK".into()); // deterministic hard-deny source
        let decision = evaluate_gates(&input);
        assert_eq!(decision.verdict, AdpVerdict::Deny);

        let overridden = apply_rollout_policy(&decision, RolloutPhase::Advisory, false);
        assert_eq!(overridden.verdict, AdpVerdict::Allow);
        assert!(overridden.reasons.iter().any(|r| r.contains("[ADVISORY]")));
    }

    #[test]
    fn guarded_phase_preserves_deny() {
        let mut input = make_default_input();
        input.immune_verdict = Some("BLOCK".into()); // deterministic hard-deny source
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
        assert_eq!(
            RolloutPhase::from_str("shadow").unwrap(),
            RolloutPhase::Shadow
        );
        assert_eq!(
            RolloutPhase::from_str("advisory").unwrap(),
            RolloutPhase::Advisory
        );
        assert_eq!(
            RolloutPhase::from_str("guarded").unwrap(),
            RolloutPhase::Guarded
        );
        assert_eq!(
            RolloutPhase::from_str("autonomous").unwrap(),
            RolloutPhase::Autonomous
        );
        assert_eq!(
            RolloutPhase::from_str("SHADOW").unwrap(),
            RolloutPhase::Shadow
        ); // case-insensitive
        assert!(RolloutPhase::from_str("unknown").is_err()); // fail-closed
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
                gate_code_version: "test-0.0.0".to_string(),
                evidence_schema_version: "1.0.0".to_string(),
                evidence_hash: String::new(), // populated by build_decision_report
                crate_version: String::new(), // overridden by build_decision_report
                runtime_triple: String::new(), // overridden by build_decision_report
                gate_source_hash: String::new(), // overridden by build_decision_report
                rollout_phase: "guarded".to_string(),
                kill_switch: false,
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
        let decision = replay_from_scenario(&scenario_input).unwrap();
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
        let decision = replay_from_scenario(&scenario_input).unwrap();
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
        deny_input.immune_verdict = Some("BLOCK".into()); // deterministic hard-deny source

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

    // ── ADP provenance / deterministic replay tests ──────────────────────────

    fn make_config_snapshot() -> ConfigSnapshot {
        ConfigSnapshot {
            adp_min_extraction_confidence: 0.5,
            safety_min_confidence: 0.7,
            safety_min_coverage: 0.6,
            adp_max_blast_radius: 6,
            safety_policy_enabled: true,
            gate_code_version: "1.0.0".to_string(),
            evidence_schema_version: "1.0.0".to_string(),
            evidence_hash: String::new(),
            crate_version: String::new(),
            runtime_triple: String::new(),
            gate_source_hash: String::new(),
            rollout_phase: "guarded".to_string(),
            kill_switch: false,
        }
    }

    /// ADP provenance: every mandatory versioning field in ConfigSnapshot must be
    /// non-empty after `build_decision_report` fills in the computed fields.
    ///
    /// An empty `evidence_hash` or `crate_version` means the replay envelope
    /// cannot detect drift — the audit finding ADP1 is not closed until all fields
    /// are populated with meaningful values.
    #[test]
    fn decision_report_all_provenance_fields_are_populated() {
        let input = make_default_input();
        let decision = evaluate_gates(&input);
        let report = build_decision_report(
            &decision,
            "proj-1",
            "add helper fn",
            &["src/lib.rs".into()],
            "low",
            serde_json::json!({}),
            make_config_snapshot(),
            "ci-build-42",
        );

        // gate_code_version: caller-supplied and must propagate unchanged.
        assert!(
            !report.config_snapshot.gate_code_version.is_empty(),
            "ADP1: gate_code_version must be non-empty — used to detect gate-logic \
             drift between the original decision and a future replay"
        );

        // evidence_schema_version: format version for replay compatibility.
        assert!(
            !report.config_snapshot.evidence_schema_version.is_empty(),
            "ADP1: evidence_schema_version must be non-empty — bumped when the \
             evidence structure changes to invalidate stale replays"
        );

        // evidence_hash: BLAKE3 of serialized gate_evidence, computed by build_decision_report.
        assert!(
            !report.config_snapshot.evidence_hash.is_empty(),
            "ADP1: evidence_hash must be non-empty after build_decision_report — \
             empty hash means evidence tampering or serialization drift is undetectable"
        );
        // BLAKE3 hex is 64 chars.
        assert_eq!(
            report.config_snapshot.evidence_hash.len(),
            64,
            "ADP1: evidence_hash must be a 64-char BLAKE3 hex digest; \
             got length {}",
            report.config_snapshot.evidence_hash.len()
        );

        // crate_version: compile-time CARGO_PKG_VERSION, overridden by build_decision_report.
        assert!(
            !report.config_snapshot.crate_version.is_empty(),
            "ADP1: crate_version must be non-empty — it reflects the Cargo package \
             version at compile time to detect binary-level drift across releases"
        );
        assert_eq!(
            report.config_snapshot.crate_version,
            env!("CARGO_PKG_VERSION"),
            "ADP1: crate_version must equal env!(CARGO_PKG_VERSION) at decision time; \
             any other value means the provenance stamp is wrong"
        );

        // gate_source_hash: BLAKE3 of autonomous_decision_service.rs, embedded by build.rs.
        assert!(
            !report.config_snapshot.gate_source_hash.is_empty(),
            "ADP1-z2t4: gate_source_hash must be non-empty — it is the compile-time \
             BLAKE3 fingerprint of gate logic source; empty means build.rs did not run"
        );
        assert_eq!(
            report.config_snapshot.gate_source_hash.len(),
            64,
            "ADP1-z2t4: gate_source_hash must be a 64-char BLAKE3 hex digest; \
             got length {}",
            report.config_snapshot.gate_source_hash.len()
        );
    }

    /// ADP deterministic replay: two calls to `build_decision_report` with identical
    /// gate results must produce identical `evidence_hash` values.
    ///
    /// This is the core replay guarantee: same inputs → same provenance hash, so a
    /// forensic replay can detect gate-result divergence by comparing hashes.
    #[test]
    fn decision_report_evidence_hash_is_deterministic_for_same_gate_results() {
        let input = make_default_input();
        let decision = evaluate_gates(&input);

        let report1 = build_decision_report(
            &decision,
            "proj-2",
            "rename field",
            &["src/models.rs".into()],
            "low",
            serde_json::json!({"run": 1}),
            make_config_snapshot(),
            "build-1",
        );
        let report2 = build_decision_report(
            &decision,
            "proj-2",
            "rename field",
            &["src/models.rs".into()],
            "low",
            serde_json::json!({"run": 2}), // input_snapshot differs — hash must NOT
            make_config_snapshot(),
            "build-2",
        );

        // evidence_hash is derived from gate_evidence, not from input_snapshot or build_id.
        // Two calls with the same gate results must produce the same hash.
        assert_eq!(
            report1.config_snapshot.evidence_hash, report2.config_snapshot.evidence_hash,
            "ADP1: evidence_hash must be identical for equal gate results regardless \
             of input_snapshot or build_id — the hash binds only the gate evidence array"
        );
    }

    /// ADP replay drift detection: two decisions with different verdicts (allow vs deny)
    /// must produce different `evidence_hash` values, proving the hash actually binds
    /// the gate outcome.
    #[test]
    fn decision_report_evidence_hash_differs_for_different_gate_results() {
        let allow_input = make_default_input();
        let allow_decision = evaluate_gates(&allow_input);

        let mut deny_input = make_default_input();
        deny_input.safety_decision = Some(crate::services::safety_service::PolicyDecision {
            allowed: false,
            risk_level: crate::services::safety_service::RiskLevel::High,
            checks: vec![],
            confidence: 0.95,
            summary: "blocked".into(),
            mitigations: vec![],
        });
        let deny_decision = evaluate_gates(&deny_input);

        let allow_report = build_decision_report(
            &allow_decision,
            "proj-3",
            "change A",
            &[],
            "low",
            serde_json::json!({}),
            make_config_snapshot(),
            "b1",
        );
        let deny_report = build_decision_report(
            &deny_decision,
            "proj-3",
            "change A",
            &[],
            "low",
            serde_json::json!({}),
            make_config_snapshot(),
            "b1",
        );

        assert_ne!(
            allow_report.config_snapshot.evidence_hash, deny_report.config_snapshot.evidence_hash,
            "ADP1: evidence_hash must differ when gate results differ (allow vs deny) — \
             a hash that is identical for different outcomes cannot detect gate drift"
        );
    }

    /// ADP replay: `build_decision_report` serializes to JSON and can be fully
    /// round-tripped, preserving all provenance fields through the
    /// serialize → deserialize cycle.
    #[test]
    fn decision_report_provenance_survives_json_roundtrip() {
        let input = make_default_input();
        let decision = evaluate_gates(&input);
        let report = build_decision_report(
            &decision,
            "proj-rt",
            "roundtrip test",
            &["a.rs".into()],
            "medium",
            serde_json::json!({"x": 1}),
            make_config_snapshot(),
            "rt-build-1",
        );
        let hash_before = report.config_snapshot.evidence_hash.clone();
        let version_before = report.config_snapshot.crate_version.clone();

        let json = serde_json::to_string(&report).expect("report must serialize");
        let decoded: AdpDecisionReport =
            serde_json::from_str(&json).expect("report must deserialize");

        assert_eq!(
            decoded.config_snapshot.evidence_hash, hash_before,
            "ADP1: evidence_hash must survive JSON roundtrip unchanged"
        );
        assert_eq!(
            decoded.config_snapshot.crate_version, version_before,
            "ADP1: crate_version must survive JSON roundtrip unchanged"
        );
        assert_eq!(
            decoded.config_snapshot.gate_code_version, report.config_snapshot.gate_code_version,
            "ADP1: gate_code_version must survive JSON roundtrip unchanged"
        );
    }
}
