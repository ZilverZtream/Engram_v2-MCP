//! Evidence Orchestration Engine (EOE) — ADP vNext.
//!
//! Replaces caller-supplied gate evidence with first-party evidence gathered
//! from the project graph, safety service, blast radius service, and benchmark
//! service.  The pure gate pipeline (`evaluate_gates`) remains deterministic
//! and testable; this module handles the async orchestration that feeds it.

use crate::services::autonomous_decision_service::{
    AdpInput, GraphImpactMetrics, ReconciliationScores, RetrievalMode, RiskProfile,
};
use crate::services::blast_radius_service;
use crate::services::safety_service::{self, PolicyDecision, SafetyEvalRequest};
use crate::state::AppState;
use engram_graph::EdgeKind;
use serde::{Deserialize, Serialize};

// ── Evidence depth ──────────────────────────────────────────────────────────

/// Controls how expensive the evidence gathering is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvidenceDepth {
    /// Fastest: skip retrieval benchmark, use cached blast radius.
    Fast,
    /// Default: run blast radius + safety + extraction derivation.
    Standard,
    /// Full: also run retrieval benchmark (expensive).
    Deep,
}

impl EvidenceDepth {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "fast" => Self::Fast,
            "deep" => Self::Deep,
            _ => Self::Standard,
        }
    }
}

// ── Evidence overrides ──────────────────────────────────────────────────────

/// Optional caller-supplied overrides for backward compatibility.
///
/// If a field is `Some(...)`, the EOE skips the live service call for that gate
/// and uses the provided value instead.
#[derive(Debug, Clone, Default)]
pub struct EvidenceOverrides {
    pub extraction_confidence: Option<f64>,
    pub extraction_type: Option<String>,
    pub trace_used_fallback: Option<bool>,
    pub trace_candidate_count: Option<usize>,
    pub immune_verdict: Option<String>,
    pub immune_confidence: Option<f32>,
    pub has_runtime_evidence: Option<bool>,
    pub reconciliation: Option<ReconciliationScores>,
    pub safety_decision: Option<PolicyDecision>,
    pub retrieval_production_ready: Option<bool>,
    pub retrieval_ndcg: Option<f64>,
    pub retrieval_recall: Option<f64>,
    pub migration_class: Option<String>,
}

// ── Core orchestration ──────────────────────────────────────────────────────

/// Gather all evidence needed for an ADP decision.
///
/// Callers may provide `overrides` to inject pre-computed evidence (backward compat).
/// Any override field that is `Some(...)` will skip the live service call for that gate.
pub async fn gather_evidence(
    state: &AppState,
    project_id: &str,
    target_files: &[String],
    _proposed_change: &str,
    risk_profile: RiskProfile,
    depth: EvidenceDepth,
    overrides: &EvidenceOverrides,
    require_runtime_evidence: bool,
    generation: u64,
) -> Result<AdpInput, anyhow::Error> {
    // ── Phase 1: Parallel evidence gathering ──
    // Run graph impact and blast radius in parallel.

    let graph_impact_fut = derive_graph_impact(state, project_id, target_files);
    let blast_radius_fut = derive_blast_radius(state, project_id, target_files, generation);

    let (graph_impact, blast_result) = tokio::join!(graph_impact_fut, blast_radius_fut);
    let graph_impact = graph_impact;
    let (blast_risk, blast_band, blast_downstream) = blast_result;

    // ── Phase 2: Safety evaluation (uses graph impact) ──
    let safety_decision = if let Some(ref sd) = overrides.safety_decision {
        Some(sd.clone())
    } else if !target_files.is_empty() {
        Some(derive_safety_from_graph(
            state,
            project_id,
            target_files,
            &graph_impact,
            overrides.immune_verdict.as_deref(),
        ))
    } else {
        None
    };

    // ── Phase 3: Extraction confidence ──
    let (extraction_confidence, extraction_band) = if let Some(ec) = overrides.extraction_confidence
    {
        let band = if ec >= 0.8 {
            "high".to_string()
        } else if ec >= 0.5 {
            "medium".to_string()
        } else {
            "low".to_string()
        };
        (Some(ec), Some(band))
    } else {
        derive_extraction_confidence(&graph_impact)
    };

    // ── Phase 4: Retrieval benchmark (only in Deep mode) ──
    let (retrieval_ready, retrieval_ndcg, retrieval_recall, retrieval_mode) =
        if overrides.retrieval_production_ready.is_some() {
            (
                overrides.retrieval_production_ready,
                overrides.retrieval_ndcg,
                overrides.retrieval_recall,
                if overrides.retrieval_production_ready.is_some() {
                    RetrievalMode::Live
                } else {
                    RetrievalMode::Skipped
                },
            )
        } else {
            match depth {
                EvidenceDepth::Deep => {
                    // TODO: Run live benchmark when benchmark_service supports async queries.
                    // For now, signal that retrieval was requested but not available.
                    (None, None, None, RetrievalMode::Live)
                }
                EvidenceDepth::Fast => (None, None, None, RetrievalMode::Skipped),
                EvidenceDepth::Standard => (None, None, None, RetrievalMode::Skipped),
            }
        };

    // ── Phase 5: Reconciliation ──
    let reconciliation = overrides.reconciliation.clone();
    let has_runtime_evidence = overrides.has_runtime_evidence.unwrap_or(false);

    // ── Build AdpInput ──
    Ok(AdpInput {
        extraction_confidence,
        extraction_band,
        trace_used_fallback: overrides.trace_used_fallback.unwrap_or(false),
        trace_candidate_count: overrides.trace_candidate_count.unwrap_or(0),
        safety_decision,
        retrieval_production_ready: retrieval_ready,
        retrieval_ndcg,
        retrieval_recall,
        blast_radius_risk: blast_risk,
        blast_radius_band: blast_band,
        blast_radius_downstream: blast_downstream,
        immune_verdict: overrides.immune_verdict.clone(),
        immune_confidence: overrides.immune_confidence,
        require_runtime_evidence,
        has_runtime_evidence,
        risk_profile,
        min_extraction_confidence: state.cfg.adp_min_extraction_confidence,
        min_safety_confidence: state.cfg.safety_min_confidence,
        max_blast_radius_for_auto: state.cfg.adp_max_blast_radius,
        reconciliation,
        graph_impact: Some(graph_impact),
        retrieval_mode,
        migration_class: overrides.migration_class.clone(),
    })
}

// ── Internal helpers ────────────────────────────────────────────────────────

/// Derive graph impact metrics from the project graph via spawn_blocking.
async fn derive_graph_impact(
    state: &AppState,
    project_id: &str,
    target_files: &[String],
) -> GraphImpactMetrics {
    if target_files.is_empty() {
        return GraphImpactMetrics::default();
    }

    let graph = state.graph.clone();
    let pid = project_id.to_string();
    let files = target_files.to_vec();

    tokio::task::spawn_blocking(move || {
        let mut metrics = GraphImpactMetrics::default();

        for f in &files {
            let target_id = format!("file:{}", f);

            if let Ok(deps) = graph.neighbors(&pid, EdgeKind::Dependency, &target_id, 500) {
                metrics.downstream_dependency_count += deps.len() as u64;
            }
            if let Ok(reads) = graph.neighbors(&pid, EdgeKind::ReadsState, &target_id, 100) {
                metrics.reads_state_count += reads.len();
            }
            if let Ok(writes) = graph.neighbors(&pid, EdgeKind::WritesState, &target_id, 100) {
                metrics.writes_state_count += writes.len();
            }
            if let Ok(sql) = graph.neighbors(&pid, EdgeKind::SqlCalls, &target_id, 100) {
                metrics.sql_calls_count += sql.len();
            }
            if let Ok(qt) = graph.neighbors(&pid, EdgeKind::QueriesTable, &target_id, 100) {
                metrics.queries_table_count += qt.len();
            }
            if let Ok(scripts) = graph.neighbors(&pid, EdgeKind::InjectsScript, &target_id, 100) {
                metrics.injects_script_count += scripts.len();
            }
        }

        metrics
    })
    .await
    .unwrap_or_default()
}

/// Derive blast radius from the blast radius service.
async fn derive_blast_radius(
    state: &AppState,
    project_id: &str,
    target_files: &[String],
    generation: u64,
) -> (
    Option<u8>,
    Option<blast_radius_service::RiskBand>,
    Option<usize>,
) {
    let first_file = match target_files.first() {
        Some(f) => f.clone(),
        None => return (None, None, None),
    };

    let graph = state.graph.clone();
    let pid = project_id.to_string();
    let target_id = format!("file:{}", first_file);

    match tokio::task::spawn_blocking(move || {
        blast_radius_service::compute_blast_radius(&graph, &pid, &target_id, generation, false)
    })
    .await
    {
        Ok(Ok(report)) => (
            Some(report.migration_risk),
            Some(report.risk_band),
            Some(report.total_downstream),
        ),
        _ => (None, None, None),
    }
}

/// Build a safety evaluation from graph-derived impact metrics (replaces text heuristics).
fn derive_safety_from_graph(
    state: &AppState,
    project_id: &str,
    target_files: &[String],
    graph_impact: &GraphImpactMetrics,
    immune_verdict: Option<&str>,
) -> PolicyDecision {
    let touches_global_state =
        graph_impact.reads_state_count > 0 || graph_impact.writes_state_count > 0;
    let touches_database = graph_impact.sql_calls_count > 0 || graph_impact.queries_table_count > 0;

    // Compute impact confidence from structural signals in the graph
    let has_deps = graph_impact.downstream_dependency_count > 0;
    let has_state = touches_global_state;
    let has_sql = touches_database;
    let signals = [has_deps, has_state, has_sql]
        .iter()
        .filter(|&&x| x)
        .count();
    let impact_confidence = match signals {
        0 => 0.5, // No graph data — moderate confidence
        1 => 0.7,
        2 => 0.85,
        _ => 0.95,
    };

    let eval_req = SafetyEvalRequest {
        project_id: project_id.to_string(),
        affected_files: target_files.to_vec(),
        refactor_type: "autonomous_change".into(),
        impact_node_count: graph_impact.downstream_dependency_count,
        impact_confidence,
        test_coverage: -1.0, // Still unknown at ADP time
        anti_pattern_clear: immune_verdict == Some("PASS") || immune_verdict.is_none(),
        downstream_dependents: graph_impact.downstream_dependency_count,
        touches_global_state,
        touches_database,
    };

    safety_service::evaluate_safety(
        &eval_req,
        state.cfg.safety_policy_enabled,
        state.cfg.safety_min_confidence,
        state.cfg.safety_min_coverage,
    )
}

/// Derive extraction confidence from graph structural signals.
///
/// If the graph has relevant edges for a file, we can infer that extraction
/// was successful with higher confidence.
fn derive_extraction_confidence(
    graph_impact: &GraphImpactMetrics,
) -> (Option<f64>, Option<String>) {
    let has_deps = graph_impact.downstream_dependency_count > 0;
    let has_state = graph_impact.reads_state_count > 0 || graph_impact.writes_state_count > 0;
    let has_sql = graph_impact.sql_calls_count > 0 || graph_impact.queries_table_count > 0;
    let has_scripts = graph_impact.injects_script_count > 0;

    let signals = [has_deps, has_state, has_sql, has_scripts]
        .iter()
        .filter(|&&x| x)
        .count();

    if signals == 0 {
        // No graph data — cannot derive confidence
        return (None, None);
    }

    // Each structural signal adds confidence
    let score = match signals {
        1 => 0.55,
        2 => 0.70,
        3 => 0.85,
        _ => 0.95,
    };
    let band = if score >= 0.8 {
        "high".to_string()
    } else if score >= 0.5 {
        "medium".to_string()
    } else {
        "low".to_string()
    };

    (Some(score), Some(band))
}
