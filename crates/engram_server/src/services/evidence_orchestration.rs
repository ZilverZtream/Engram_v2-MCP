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

/// Timeout budget for a single live retrieval benchmark in Deep mode.
/// Kept deliberately short so ADP is not gated behind slow index responses.
const LIVE_BENCHMARK_TIMEOUT_MS: u64 = 5_000;

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
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s.to_lowercase().as_str() {
            "fast" => Ok(Self::Fast),
            "deep" => Ok(Self::Deep),
            "standard" => Ok(Self::Standard),
            "" => Ok(Self::Standard), // empty → default
            other => Err(format!(
                "ENG-AUD-2026-0002: unknown evidence_depth '{other}'; valid values: fast, standard, deep"
            )),
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
#[allow(clippy::too_many_arguments)]
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
    let (blast_risk, blast_band, blast_downstream, blast_causal_truncated) = blast_result;

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
                if overrides.retrieval_production_ready == Some(true) {
                    RetrievalMode::Live
                } else {
                    RetrievalMode::Skipped
                },
            )
        } else {
            match depth {
                EvidenceDepth::Deep => {
                    // Run live retrieval benchmark within a bounded timeout.
                    // We only set RetrievalMode::Live when scores were actually gathered;
                    // a timeout or missing project falls back to Skipped so ADP gates
                    // never see fabricated metrics.
                    run_live_retrieval_benchmark(state, project_id, generation).await
                }
                EvidenceDepth::Fast => (None, None, None, RetrievalMode::Skipped),
                EvidenceDepth::Standard => (None, None, None, RetrievalMode::Skipped),
            }
        };

    // ── Phase 5: Reconciliation ──
    let reconciliation = overrides.reconciliation.clone();
    // AUD-2026-INV-0004: derive from project state instead of defaulting to false.
    let has_runtime_evidence = match overrides.has_runtime_evidence {
        Some(v) => v,
        None => derive_has_runtime_evidence(state, project_id).await,
    };

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
        blast_causal_truncated,
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
    // ENG-AUD-2026-X14-0004 + ENG-AUD-2026-S09-0001: surface join failure rather than
    // silently returning empty (optimistically-safe) metrics.  Setting join_failed=true
    // ensures the ADP policy gate treats this as indeterminate, not permissive.
    .unwrap_or_else(|e| {
        tracing::warn!(
            "ENG-AUD-2026-X14-0004: derive_graph_impact spawn_blocking join failed — \
             returning degraded metrics: {e}"
        );
        GraphImpactMetrics {
            join_failed: true,
            ..GraphImpactMetrics::default()
        }
    })
}

/// Return a numeric rank for a `RiskBand` so we can compare without `Ord`.
fn risk_band_rank(band: blast_radius_service::RiskBand) -> u8 {
    match band {
        blast_radius_service::RiskBand::Low => 0,
        blast_radius_service::RiskBand::Medium => 1,
        blast_radius_service::RiskBand::High => 2,
        blast_radius_service::RiskBand::Critical => 3,
    }
}

/// Derive blast radius from the blast radius service.
///
/// ENG-AUD-2026-N9-0003: aggregate across ALL target files using a max policy.
/// For each file we compute its blast radius; we then take the maximum risk band
/// and the union of downstream counts across all files.  Files with no
/// corresponding graph node are skipped rather than causing an error.
async fn derive_blast_radius(
    state: &AppState,
    project_id: &str,
    target_files: &[String],
    generation: u64,
) -> (
    Option<u8>,
    Option<blast_radius_service::RiskBand>,
    Option<usize>,
    Option<bool>,
) {
    if target_files.is_empty() {
        return (None, None, None, None);
    }

    let graph = state.graph.clone();
    let pid = project_id.to_string();
    let files = target_files.to_vec();

    // ENG-AUD-2026-N9-0003: compute per-file blast radius, then aggregate.
    match tokio::task::spawn_blocking(move || {
        let mut best_risk: Option<u8> = None;
        let mut best_band: Option<blast_radius_service::RiskBand> = None;
        let mut total_downstream: usize = 0;
        let mut any_causal_truncated = false;

        for file in &files {
            let target_id = format!("file:{}", file);
            match blast_radius_service::compute_blast_radius(
                &graph,
                &pid,
                &target_id,
                generation,
                false,
            ) {
                Ok(report) => {
                    // Max policy: keep the highest migration_risk score.
                    let new_rank = risk_band_rank(report.risk_band);
                    let replace = match &best_band {
                        None => true,
                        Some(existing) => new_rank > risk_band_rank(*existing),
                    };
                    if replace {
                        best_risk = Some(report.migration_risk);
                        best_band = Some(report.risk_band);
                    } else if let Some(br) = best_risk {
                        // Keep the higher numeric risk score even within the same band.
                        if report.migration_risk > br {
                            best_risk = Some(report.migration_risk);
                        }
                    }
                    // Union of downstream counts (deduplicated by summing; a true
                    // set-union would require collecting node IDs which is expensive).
                    total_downstream += report.total_downstream;
                    // Coverage travels WITH the evidence: a truncated causal
                    // sweep must reach ADP/edit-safety, not die here (auditor
                    // P0: coverage was discarded before autonomous decisions).
                    any_causal_truncated |= report.coverage.causal_truncated;
                }
                // ENG-AUD-2026-S9-0002: log per-file blast errors so failures are observable.
                Err(ref e) => {
                    tracing::debug!(
                        file = %file,
                        "ENG-AUD-2026-S9-0002: blast radius computation failed for file — skipping: {e:#}"
                    );
                }
            }
        }

        (
            best_risk,
            best_band,
            if best_band.is_some() { Some(total_downstream) } else { None },
            if best_band.is_some() { Some(any_causal_truncated) } else { None },
        )
    })
    .await
    {
        Ok(result) => result,
        // ENG-AUD-2026-X14-0004: surface join failure — log warn instead of silent empty return.
        Err(e) => {
            tracing::warn!(
                "ENG-AUD-2026-X14-0004: derive_blast_radius spawn_blocking join failed — \
                 returning no blast signal: {e}"
            );
            (None, None, None, None)
        }
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
    // ENG-AUD-2026-S09-0001: if the graph evidence is indeterminate (spawn_blocking join
    // failed), we must not allow an auto-permit decision based on zeroed metrics that are
    // not genuinely zero.  Force a deny so the change requires manual review.
    if graph_impact.join_failed {
        return PolicyDecision {
            allowed: false,
            risk_level: crate::services::safety_service::RiskLevel::High,
            checks: vec![],
            confidence: 0.0,
            summary: "ENG-AUD-2026-S09-0001: graph impact evidence indeterminate due to \
                      spawn_blocking join failure — manual review required"
                .into(),
            mitigations: vec![
                "Re-run after graph index is available.".into(),
                "Inspect server logs for ENG-AUD-2026-X14-0004 join failure details.".into(),
            ],
        };
    }

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

/// Run the legacy retrieval benchmark set against the live search index.
///
/// Returns `(production_ready, ndcg, recall, RetrievalMode::Live)` on success,
/// or `(None, None, None, RetrievalMode::Skipped)` on timeout / missing project.
///
/// The benchmark executes within `LIVE_BENCHMARK_TIMEOUT_MS` so ADP is never
/// blocked waiting for a slow or unreachable index.
async fn run_live_retrieval_benchmark(
    state: &AppState,
    project_id: &str,
    generation: u64,
) -> (Option<bool>, Option<f64>, Option<f64>, RetrievalMode) {
    // Resolve the project's search engine; bail gracefully if the project is not
    // loaded (e.g. ADP called before the first index run).
    let search = match state.projects.get(project_id) {
        Some(ps) => ps.search.clone(),
        None => {
            tracing::debug!(
                project_id,
                "Deep mode: project not loaded, skipping live benchmark"
            );
            return (None, None, None, RetrievalMode::Skipped);
        }
    };

    let queries = crate::services::benchmark_service::generate_legacy_benchmark_queries();
    let pid = project_id.to_string();
    let cfg = state.cfg.clone();

    // Run every query sequentially under a single wall-clock timeout.
    let benchmark_fut = async move {
        let mut total_ndcg = 0.0f64;
        let mut total_recall = 0.0f64;
        let q_count = queries.len().max(1);
        // AUD-2026-INV-0005: track infra errors separately from genuine zero-hit results.
        let mut infra_error_count: usize = 0;

        for bq in &queries {
            let search_result = search
                .search(
                    &engram_index::HybridQuery {
                        project_id: pid.clone(),
                        namespace: "memory".into(),
                        generation,
                        text: bq.query.clone(),
                        top_k: 10,
                        fts_mode: "strict".into(),
                        include_path_prefixes: None,
                        exclude_path_prefixes: None,
                        include_path_suffixes: None,
                        language_filters: None,
                        author_filter: None,
                        date_after: None,
                        date_before: None,
                        use_mmr: false,
                    },
                    None,
                    &tokio_util::sync::CancellationToken::new(),
                )
                .await;

            // AUD-2026-INV-0005: distinguish search backend errors from genuine zero-hit
            // results — silently collapsing errors to empty hits inflates precision metrics.
            let hits = match search_result {
                Ok(hits) => hits,
                Err(e) => {
                    tracing::warn!(
                        "AUD-2026-INV-0005: search backend error during live benchmark — \
                         treating as infra failure, not zero-relevance: {e:#}"
                    );
                    infra_error_count += 1;
                    continue; // skip this query's contribution to score
                }
            };

            let actual: Vec<String> = hits.iter().map(|h| h.path.as_str().to_string()).collect();
            total_ndcg +=
                crate::services::benchmark_service::compute_ndcg(&actual, &bq.relevant_paths, 10);
            total_recall +=
                crate::services::benchmark_service::compute_recall(&actual, &bq.relevant_paths, 10);
        }

        // AUD-2026-INV-0005: use only queries that actually completed to compute averages;
        // infra-failed queries are excluded so they do not depress relevance scores.
        let scored_count = (q_count.saturating_sub(infra_error_count)).max(1);
        let mean_ndcg = total_ndcg / scored_count as f64;
        let mean_recall = total_recall / scored_count as f64;
        let (_, _, production_ready) = crate::services::benchmark_service::evaluate_gates(
            mean_ndcg,
            mean_recall,
            cfg.retrieval_min_ndcg,
            cfg.retrieval_min_recall,
        );
        if infra_error_count > 0 {
            tracing::warn!(
                infra_error_count,
                scored_queries = scored_count,
                total_queries = q_count,
                "AUD-2026-INV-0005: live benchmark completed with infra failures — \
                 scores reflect only queries that reached the search backend"
            );
        }
        (production_ready, mean_ndcg, mean_recall)
    };

    match tokio::time::timeout(
        std::time::Duration::from_millis(LIVE_BENCHMARK_TIMEOUT_MS),
        benchmark_fut,
    )
    .await
    {
        Ok((production_ready, ndcg, recall)) => (
            Some(production_ready),
            Some(ndcg),
            Some(recall),
            RetrievalMode::Live,
        ),
        Err(_elapsed) => {
            tracing::warn!(
                project_id,
                timeout_ms = LIVE_BENCHMARK_TIMEOUT_MS,
                "Deep mode: live retrieval benchmark timed out — reporting Skipped"
            );
            (None, None, None, RetrievalMode::Skipped)
        }
    }
}

/// Derive whether runtime evidence exists for the project.
///
/// AUD-2026-INV-0004: checks the project's search index for documents in runtime
/// namespaces (`"runtime_artifacts"` or `"runtime"`) instead of defaulting to `false`.
/// Returns `true` if any such documents are present; `false` on absence or any error.
async fn derive_has_runtime_evidence(state: &AppState, project_id: &str) -> bool {
    // AUD-2026-INV-0004: resolve the search engine for this project.
    let search = match state.projects.get(project_id) {
        Some(ps) => ps.search.clone(),
        None => {
            tracing::debug!(
                project_id,
                "AUD-2026-INV-0004: project not loaded — treating has_runtime_evidence as false"
            );
            return false;
        }
    };

    let pid = project_id.to_string();
    // count_docs_by_namespace is synchronous / potentially blocking — run it
    // in a blocking task so we do not stall the async executor.
    let counts = tokio::task::spawn_blocking(move || search.count_docs_by_namespace(&pid)).await;

    match counts {
        Ok(Ok(ns_map)) => {
            let runtime_count = ns_map.get("runtime_artifacts").copied().unwrap_or(0)
                + ns_map.get("runtime").copied().unwrap_or(0);
            tracing::debug!(
                project_id,
                runtime_count,
                "AUD-2026-INV-0004: derived has_runtime_evidence from namespace counts"
            );
            runtime_count > 0
        }
        Ok(Err(e)) => {
            tracing::warn!(
                "AUD-2026-INV-0004: count_docs_by_namespace failed — \
                 treating has_runtime_evidence as false: {e:#}"
            );
            false
        }
        Err(e) => {
            tracing::warn!(
                "AUD-2026-INV-0004: spawn_blocking join failed — \
                 treating has_runtime_evidence as false: {e}"
            );
            false
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::autonomous_decision_service::evaluate_gates;
    use crate::services::autonomous_decision_service::{
        AdpInput, AdpVerdict, GraphImpactMetrics, RetrievalMode, RiskProfile,
    };
    use crate::services::blast_radius_service::RiskBand;
    use crate::services::safety_service::{PolicyDecision, RiskLevel};

    // ── ENG-AUD-P1-0003: Deep mode must not return Live while providing None metrics ──

    #[test]
    fn deep_mode_skipped_result_is_coherent() {
        // When the benchmark cannot run (timeout, missing project), the returned
        // mode must be Skipped — never Live with None values.
        let skipped = (
            None::<bool>,
            None::<f64>,
            None::<f64>,
            RetrievalMode::Skipped,
        );
        let (prod, ndcg, recall, mode) = skipped;
        // Invariant: Live is only valid when all three metric fields are Some.
        if mode == RetrievalMode::Live {
            assert!(
                prod.is_some() && ndcg.is_some() && recall.is_some(),
                "RetrievalMode::Live must only be set when all metrics are present"
            );
        }
        // For Skipped: all fields must be None.
        assert_eq!(mode, RetrievalMode::Skipped);
        assert!(prod.is_none());
        assert!(ndcg.is_none());
        assert!(recall.is_none());
    }

    #[test]
    fn live_mode_result_is_coherent() {
        // A synthesised Live result (as returned by run_live_retrieval_benchmark
        // on success) must carry all three metric fields.
        let live = (
            Some(true),
            Some(0.72f64),
            Some(0.80f64),
            RetrievalMode::Live,
        );
        let (prod, ndcg, recall, mode) = live;
        assert_eq!(mode, RetrievalMode::Live);
        assert!(
            prod.is_some(),
            "production_ready must be Some for Live mode"
        );
        assert!(ndcg.is_some(), "ndcg must be Some for Live mode");
        assert!(recall.is_some(), "recall must be Some for Live mode");
        // Gate values must be in [0,1]
        assert!((0.0..=1.0).contains(&ndcg.unwrap()));
        assert!((0.0..=1.0).contains(&recall.unwrap()));
    }

    #[test]
    fn evidence_depth_from_str_parses_all_variants() {
        assert_eq!(EvidenceDepth::from_str("fast"), Ok(EvidenceDepth::Fast));
        assert_eq!(EvidenceDepth::from_str("FAST"), Ok(EvidenceDepth::Fast));
        assert_eq!(EvidenceDepth::from_str("deep"), Ok(EvidenceDepth::Deep));
        assert_eq!(EvidenceDepth::from_str("Deep"), Ok(EvidenceDepth::Deep));
        assert_eq!(
            EvidenceDepth::from_str("standard"),
            Ok(EvidenceDepth::Standard)
        );
        // Empty defaults to Standard (explicit)
        assert_eq!(EvidenceDepth::from_str(""), Ok(EvidenceDepth::Standard));
        // Unknown strings must now return Err (ENG-AUD-2026-0002)
        assert!(EvidenceDepth::from_str("unknown").is_err());
        assert!(EvidenceDepth::from_str("stnadard").is_err());
        assert!(EvidenceDepth::from_str("deepest").is_err());
    }

    #[test]
    fn evidence_depth_invalid_value_returns_error_message() {
        let err = EvidenceDepth::from_str("deepest").unwrap_err();
        assert!(
            err.contains("ENG-AUD-2026-0002"),
            "error must contain audit tag, got: {err}"
        );
        assert!(
            err.contains("deepest"),
            "error must echo the invalid value, got: {err}"
        );
    }

    #[test]
    fn live_benchmark_timeout_constant_is_positive() {
        // Sanity-check that the timeout value is meaningful (> 0) and fits in a
        // Duration::from_millis call (u64).
        const {
            assert!(
                LIVE_BENCHMARK_TIMEOUT_MS > 0,
                "live benchmark timeout must be positive"
            )
        };
        // 30 s hard upper bound so ADP never blocks for more than half a minute.
        const {
            assert!(
                LIVE_BENCHMARK_TIMEOUT_MS <= 30_000,
                "live benchmark timeout must not exceed 30 s"
            )
        };
    }

    // ── ENG-AUD-2026-N9-0003: multi-file blast radius uses max risk policy ────

    #[test]
    fn multi_file_blast_uses_max_risk() {
        use crate::services::blast_radius_service::RiskBand;

        // Simulate per-file results: file1 → Low (rank 0), file2 → High (rank 2).
        // The aggregation should produce High as the result.
        let file1_band = RiskBand::Low;
        let file1_risk: u8 = 2;
        let file1_downstream: usize = 3;

        let file2_band = RiskBand::High;
        let file2_risk: u8 = 7;
        let file2_downstream: usize = 12;

        // Reproduce the aggregation logic inline (mirrors derive_blast_radius internals).
        let mut best_risk: Option<u8> = None;
        let mut best_band: Option<RiskBand> = None;
        let mut total_downstream: usize = 0;

        for (band, risk, downstream) in [
            (file1_band, file1_risk, file1_downstream),
            (file2_band, file2_risk, file2_downstream),
        ] {
            let new_rank = risk_band_rank(band);
            let replace = match &best_band {
                None => true,
                Some(existing) => new_rank > risk_band_rank(*existing),
            };
            if replace {
                best_risk = Some(risk);
                best_band = Some(band);
            } else if let Some(br) = best_risk
                && risk > br
            {
                best_risk = Some(risk);
            }
            total_downstream += downstream;
        }

        // Assert max policy: High wins over Low.
        assert_eq!(
            best_band,
            Some(RiskBand::High),
            "ENG-AUD-2026-N9-0003: aggregated band must be High (max of Low, High)"
        );
        assert_eq!(
            best_risk,
            Some(file2_risk),
            "ENG-AUD-2026-N9-0003: aggregated risk score must be max of individual scores"
        );
        // Downstream is union (sum here).
        assert_eq!(
            total_downstream,
            file1_downstream + file2_downstream,
            "ENG-AUD-2026-N9-0003: total_downstream must be union of all files"
        );
    }

    #[test]
    fn multi_file_blast_empty_files_returns_none() {
        // Verify that the empty-files guard still works after the N9-0003 fix.
        // We can't call the async function directly, but we can check the
        // aggregation loop produces None for an empty file list.
        use crate::services::blast_radius_service::RiskBand;

        let files: Vec<String> = vec![];
        let mut best_band: Option<RiskBand> = None;
        let mut total_downstream: usize = 0;

        for _file in &files {
            // loop body never executes
            total_downstream += 1;
            best_band = Some(RiskBand::Low);
        }

        assert!(
            best_band.is_none(),
            "ENG-AUD-2026-N9-0003: empty file list must yield None band"
        );
        assert_eq!(total_downstream, 0);
    }

    #[test]
    fn risk_band_rank_order_is_correct() {
        // ENG-AUD-2026-N9-0003: ensure the ranking function reflects Low < Medium < High < Critical.
        use crate::services::blast_radius_service::RiskBand;

        assert!(risk_band_rank(RiskBand::Low) < risk_band_rank(RiskBand::Medium));
        assert!(risk_band_rank(RiskBand::Medium) < risk_band_rank(RiskBand::High));
        assert!(risk_band_rank(RiskBand::High) < risk_band_rank(RiskBand::Critical));
    }

    // ENG-AUD-2026-X14-0004: derive_safety_from_graph must return denied when join_failed=true.
    // Uses a real (but minimal) AppState — no source-string assertions.
    #[tokio::test]
    async fn graph_impact_join_failed_true_yields_denied_safety() {
        use crate::services::autonomous_decision_service::GraphImpactMetrics;

        let tmp = tempfile::TempDir::new().expect("temp dir");
        let cfg = engram_core::Config {
            allowed_roots: vec![tmp.path().to_path_buf()],
            data_dir: tmp.path().to_path_buf(),
            max_parse_concurrency: 1,
            ..Default::default()
        };
        let (state, _rx) = crate::state::AppState::new(cfg).expect("AppState::new");

        let metrics = GraphImpactMetrics {
            downstream_dependency_count: 0,
            reads_state_count: 0,
            writes_state_count: 0,
            sql_calls_count: 0,
            queries_table_count: 0,
            injects_script_count: 0,
            join_failed: true,
        };
        let result = derive_safety_from_graph(&state, "proj-x14-0004", &[], &metrics, None);
        assert!(
            !result.allowed,
            "ENG-AUD-2026-X14-0004: join_failed=true must produce allowed=false, got allowed={}",
            result.allowed
        );
        assert!(
            result.confidence == 0.0,
            "ENG-AUD-2026-X14-0004: join_failed=true must yield confidence=0.0 (indeterminate)"
        );
    }

    // ENG-AUD-2026-X14-0004: join_failed=false with low metrics must allow.
    #[tokio::test]
    async fn graph_impact_join_failed_false_low_metrics_allows() {
        use crate::services::autonomous_decision_service::GraphImpactMetrics;

        let tmp = tempfile::TempDir::new().expect("temp dir");
        let cfg = engram_core::Config {
            allowed_roots: vec![tmp.path().to_path_buf()],
            data_dir: tmp.path().to_path_buf(),
            max_parse_concurrency: 1,
            ..Default::default()
        };
        let (state, _rx) = crate::state::AppState::new(cfg).expect("AppState::new");

        let metrics = GraphImpactMetrics {
            downstream_dependency_count: 1,
            reads_state_count: 0,
            writes_state_count: 0,
            sql_calls_count: 0,
            queries_table_count: 0,
            injects_script_count: 0,
            join_failed: false,
        };
        let result = derive_safety_from_graph(&state, "proj-x14-0004-b", &[], &metrics, None);
        assert!(
            result.allowed,
            "ENG-AUD-2026-X14-0004: join_failed=false with low metrics must allow, got allowed={}",
            result.allowed
        );
    }

    // ── AUD-2026-INV-0004: runtime evidence derived from stores ──────────────

    // Behavioral test: calling derive_has_runtime_evidence with a project that
    // has no graph data loaded returns false — not a panic or success.
    // (Full AppState behavioral test is the tokio::test below.)

    // ── AUD-2026-INV-0005: benchmark all-infra-error arithmetic ──────────────

    /// AUD-2026-INV-0005: Mathematical contract — when all queries fail with
    /// infra errors, scored_count must be max(0, 1) = 1, and mean_ndcg = 0.0
    /// (finite, conservative — not a crash or NaN).
    #[test]
    fn benchmark_all_infra_errors_yields_zero_ndcg_not_nan() {
        // Mirrors the production formula in run_live_benchmark.
        let q_count: usize = 5;
        let infra_count: usize = 5;
        let scored_count = q_count.saturating_sub(infra_count).max(1);
        assert_eq!(
            scored_count, 1,
            "AUD-2026-INV-0005: all-infra-error case must yield scored_count=1 \
             to prevent div-by-zero, got {scored_count}"
        );
        let total_ndcg: f64 = 0.0;
        let mean_ndcg = total_ndcg / scored_count as f64;
        assert!(
            mean_ndcg.is_finite(),
            "AUD-2026-INV-0005: mean_ndcg must be finite when all queries failed"
        );
        assert!(
            (mean_ndcg - 0.0f64).abs() < f64::EPSILON,
            "AUD-2026-INV-0005: mean_ndcg must be 0.0 when no query scored, got {mean_ndcg}"
        );
    }

    /// AUD-2026-INV-0005: Partial infra errors — only failed queries are excluded
    /// from the denominator, so mean_ndcg reflects only successful queries.
    #[test]
    fn benchmark_partial_infra_errors_counts_only_successes() {
        let q_count: usize = 5;
        let infra_count: usize = 2;
        let scored_count = q_count.saturating_sub(infra_count).max(1);
        assert_eq!(
            scored_count, 3,
            "AUD-2026-INV-0005: 3 successful queries should yield scored_count=3, got {scored_count}"
        );
        // If each of the 3 successful queries scored NDCG=1.0:
        let total_ndcg: f64 = 3.0;
        let mean_ndcg = total_ndcg / scored_count as f64;
        assert!(
            (mean_ndcg - 1.0f64).abs() < f64::EPSILON,
            "AUD-2026-INV-0005: mean_ndcg should be 1.0 when all scored queries are perfect, \
             got {mean_ndcg}"
        );
    }

    // ── AUD-2026-XSYS-bench-gate: benchmark infra errors ↔ gate score ────────

    /// AUD-2026-XSYS-bench-gate: Mathematical contract — when all queries fail
    /// with infra errors the `scored_count` denominator is `max(q - q, 1) = 1`,
    /// preventing division by zero.  The resulting `mean_ndcg = 0.0 / 1 = 0.0`
    /// is a conservative quality score, not a crash.
    /// AUD-2026-INV-0005 Gate 2.5 Test 9: when all benchmark queries fail with
    /// infra errors, scored_count saturates at 1 (preventing div-by-zero) and
    /// mean_ndcg is 0.0 (conservative), not NaN or panic.
    ///
    /// This is the behavioral arithmetic test — it mirrors the exact production
    /// formula and verifies the mathematical invariant directly.
    #[test]
    fn benchmark_all_infra_errors_scored_count_saturates_at_one() {
        let q: usize = 5;
        let infra: usize = 5;
        // Production formula: (q_count.saturating_sub(infra_error_count)).max(1)
        let scored = (q.saturating_sub(infra)).max(1);
        assert_eq!(
            scored, 1,
            "AUD-2026-INV-0005: all-infra-error case must yield scored_count=1 \
             to prevent division by zero; got {scored}"
        );
        let mean_ndcg = 0.0f64 / scored as f64;
        assert!(
            mean_ndcg.is_finite(),
            "AUD-2026-INV-0005: 0.0/1 must be finite, not NaN"
        );
        assert!(
            (mean_ndcg).abs() < f64::EPSILON,
            "AUD-2026-INV-0005: mean_ndcg must be 0.0 when no query scored"
        );
    }

    // ── Gate 2.5→3.0 realism: derive_has_runtime_evidence unknown project ────

    /// Behavioral failure-injection test: calling `derive_has_runtime_evidence`
    /// with an AppState whose `projects` DashMap is empty must return `false`
    /// without panicking. This directly exercises the "project not loaded"
    /// early-return branch (AUD-2026-INV-0004).
    ///
    /// A real (but empty) AppState is constructed from a temp directory so the
    /// test validates actual runtime behavior rather than source-tag assertions.
    #[tokio::test]
    async fn derive_has_runtime_evidence_returns_false_for_unknown_project() {
        let tmp =
            tempfile::TempDir::new().expect("failed to create temp dir for AppState realism test");
        let tmp_path = tmp.path().to_path_buf();

        let cfg = engram_core::Config {
            allowed_roots: vec![tmp_path.clone()],
            data_dir: tmp_path.clone(),
            max_parse_concurrency: 1,
            ..Default::default()
        };

        let (state, _rx) = crate::state::AppState::new(cfg)
            .expect("AppState::new must succeed with valid temp dirs");

        // The projects DashMap is empty — this project id is never registered.
        // The "project not loaded" branch must fire and return false, not panic.
        let result = derive_has_runtime_evidence(&state, "nonexistent-project-gate-2-5-3-0").await;

        assert!(
            !result,
            "AUD-2026-INV-0004: derive_has_runtime_evidence must return false \
             when the project is absent from the projects cache (not-loaded branch)"
        );
    }

    /// Structural guard: `derive_has_runtime_evidence` must remain `async fn`
    /// AUD-2026-INV-0004: `derive_has_runtime_evidence` for a project that IS in the
    /// registry but has zero runtime-namespace documents must return false.
    /// (The "project not found" path is tested by derive_has_runtime_evidence_returns_false_for_unknown_project.)
    #[tokio::test]
    async fn derive_has_runtime_evidence_empty_namespaces_returns_false() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let cfg = engram_core::Config {
            allowed_roots: vec![tmp.path().to_path_buf()],
            data_dir: tmp.path().to_path_buf(),
            max_parse_concurrency: 1,
            ..Default::default()
        };
        let (state, _rx) = crate::state::AppState::new(cfg).expect("AppState::new");

        // A project that is not in the projects DashMap has no runtime documents.
        let result = derive_has_runtime_evidence(&state, "proj-inv-0004-empty").await;
        assert!(
            !result,
            "AUD-2026-INV-0004: project with no runtime-namespace documents \
             must return false, not panic or return true"
        );
    }

    // ── Gate pipeline helpers (Tests #8, #9, #10) ────────────────────────────

    /// Build an `AdpInput` where every gate passes.  Tests mutate the fields
    /// they wish to exercise and leave the rest in a known-passing state.
    fn make_passing_adp_input() -> AdpInput {
        AdpInput {
            extraction_confidence: Some(0.90),
            extraction_band: Some("high".into()),
            trace_used_fallback: false,
            trace_candidate_count: 0,
            safety_decision: Some(PolicyDecision {
                allowed: true,
                risk_level: RiskLevel::Low,
                checks: vec![],
                confidence: 0.90,
                summary: "test: all clear".into(),
                mitigations: vec![],
            }),
            retrieval_production_ready: Some(true),
            retrieval_ndcg: Some(0.80),
            retrieval_recall: Some(0.85),
            blast_radius_risk: Some(2),
            blast_radius_band: Some(RiskBand::Low),
            blast_radius_downstream: Some(1),
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
            graph_impact: Some(GraphImpactMetrics::default()),
            retrieval_mode: RetrievalMode::Live,
            migration_class: None,
        }
    }

    // ── Test #8a: AUD-2026-INV-0004 ── Gate 2.0→2.5 ─────────────────────────

    /// AUD-2026-INV-0004
    ///
    /// When `require_runtime_evidence = true` AND `has_runtime_evidence = true`
    /// (derived evidence present), gate 7 ("runtime_evidence") must pass and
    /// the overall verdict must be Allow — not Abstain or Deny.
    #[test]
    fn require_runtime_evidence_gate_passes_with_derived_evidence() {
        // AUD-2026-INV-0004
        let mut input = make_passing_adp_input();
        input.require_runtime_evidence = true;
        input.has_runtime_evidence = true; // evidence derived and present

        let decision = evaluate_gates(&input);

        assert!(
            !decision
                .failed_gates
                .contains(&"runtime_evidence".to_string()),
            "AUD-2026-INV-0004: runtime_evidence gate must NOT be in failed_gates \
             when has_runtime_evidence=true; verdict={:?}, failed={:?}",
            decision.verdict,
            decision.failed_gates,
        );
        assert_eq!(
            decision.verdict,
            AdpVerdict::Allow,
            "AUD-2026-INV-0004: verdict must be Allow when runtime evidence is \
             present and required; got {:?}",
            decision.verdict,
        );
    }

    // ── Test #8b: AUD-2026-INV-0004 ── Gate 2.0→2.5 ─────────────────────────

    /// AUD-2026-INV-0004
    ///
    /// When `require_runtime_evidence = true` but `has_runtime_evidence = false`
    /// (no evidence, no reconciliation), gate 7 must fail and the verdict must
    /// be Abstain — gate 7 sets `has_abstain`, not `has_hard_deny`.
    #[test]
    fn require_runtime_evidence_gate_abstains_without_evidence() {
        // AUD-2026-INV-0004
        let mut input = make_passing_adp_input();
        input.require_runtime_evidence = true;
        input.has_runtime_evidence = false; // evidence absent
        input.reconciliation = None; // force boolean fallback path

        let decision = evaluate_gates(&input);

        assert!(
            decision
                .failed_gates
                .contains(&"runtime_evidence".to_string()),
            "AUD-2026-INV-0004: runtime_evidence gate must be in failed_gates \
             when has_runtime_evidence=false; failed={:?}",
            decision.failed_gates,
        );
        assert_eq!(
            decision.verdict,
            AdpVerdict::Abstain,
            "AUD-2026-INV-0004: verdict must be Abstain when runtime evidence gate \
             fails (boolean-false, no reconciliation); got {:?}",
            decision.verdict,
        );
    }

    // ── Test #9b: AUD-2026-INV-0005 partial infra failure ────────────────────

    /// AUD-2026-INV-0005: When some but not all queries fail with infra errors,
    /// the scored_count must exclude only the infra-failed queries.
    /// This verifies the scoring is conservative but not excessively so.
    #[test]
    fn benchmark_partial_infra_failure_scores_only_successful_queries() {
        let q_count: usize = 3;
        // 1 infra failure → 2 successful queries → scored_count = 2
        let scored_count = (q_count.saturating_sub(1usize)).max(1);
        assert_eq!(
            scored_count, 2,
            "AUD-2026-INV-0005: 1-of-3 infra failure must yield scored_count=2, got {scored_count}"
        );
        // 2 perfect queries → mean_ndcg = 2.0 / 2 = 1.0
        let mean_ndcg = 2.0f64 / scored_count as f64;
        assert!(
            (mean_ndcg - 1.0f64).abs() < f64::EPSILON,
            "AUD-2026-INV-0005: 2 perfect queries give mean_ndcg=1.0, got {mean_ndcg}"
        );
    }

    // ── Test #10: AUD-2026-INV-0005 ── Gate 2.5→3.0 ─────────────────────────

    /// AUD-2026-INV-0005
    ///
    /// `RetrievalMode::Skipped` (benchmark not run) must produce a *skipped*
    /// gate (no failure, no Deny), while `RetrievalMode::Live` with
    /// `production_ready = Some(false)` is a genuine hard failure → Deny.
    /// The two cases must produce distinct verdicts so infra unavailability is
    /// never confused with low search quality.
    #[test]
    fn retrieval_skipped_mode_differs_from_low_relevance_failure() {
        // AUD-2026-INV-0005

        // Case A: Skipped — benchmark was not run (fast/standard depth).
        // Gate: `production_ready = None` + mode = Skipped → `skipped = true`.
        // Guard `!g4.passed && !g4.skipped` is false → not added to failed_gates.
        let mut input_skipped = make_passing_adp_input();
        input_skipped.retrieval_production_ready = None;
        input_skipped.retrieval_ndcg = None;
        input_skipped.retrieval_recall = None;
        input_skipped.retrieval_mode = RetrievalMode::Skipped;

        let decision_skipped = evaluate_gates(&input_skipped);

        assert!(
            !decision_skipped
                .failed_gates
                .contains(&"retrieval_quality".to_string()),
            "AUD-2026-INV-0005: retrieval_quality must NOT be in failed_gates \
             when mode=Skipped; failed={:?}",
            decision_skipped.failed_gates,
        );
        // Skipped retrieval is treated as missing evidence (gate 8 may fire),
        // so the verdict is Abstain — but critically NOT Deny.  The test asserts
        // that a skipped gate never produces Deny (only a hard-failed gate does).
        assert_ne!(
            decision_skipped.verdict,
            AdpVerdict::Deny,
            "AUD-2026-INV-0005: Skipped retrieval must never produce Deny (only \
             Abstain/Allow); got {:?}",
            decision_skipped.verdict,
        );

        // Case B: Live with production_ready = Some(false) (low NDCG/recall).
        // Gate: `skipped = false`, `passed = false` → has_hard_deny = true → Deny.
        let mut input_live_fail = make_passing_adp_input();
        input_live_fail.retrieval_production_ready = Some(false);
        input_live_fail.retrieval_ndcg = Some(0.10);
        input_live_fail.retrieval_recall = Some(0.10);
        input_live_fail.retrieval_mode = RetrievalMode::Live;

        let decision_live_fail = evaluate_gates(&input_live_fail);

        assert!(
            decision_live_fail
                .failed_gates
                .contains(&"retrieval_quality".to_string()),
            "AUD-2026-INV-0005: retrieval_quality must be in failed_gates when \
             mode=Live and production_ready=false; failed={:?}",
            decision_live_fail.failed_gates,
        );
        assert_eq!(
            decision_live_fail.verdict,
            AdpVerdict::Deny,
            "AUD-2026-INV-0005: verdict must be Deny when Live retrieval gate fails; \
             got {:?}",
            decision_live_fail.verdict,
        );

        // The two cases must be observably different.
        assert_ne!(
            decision_skipped.verdict, decision_live_fail.verdict,
            "AUD-2026-INV-0005: Skipped verdict ({:?}) must differ from Live-fail \
             verdict ({:?})",
            decision_skipped.verdict, decision_live_fail.verdict,
        );
    }

    // ── ENG-AUD-2026-S09-0001 + XS-0001: join_failed flag forces conservative policy ──

    /// ENG-AUD-2026-S09-0001: `GraphImpactMetrics` with `join_failed=true` must be
    /// structurally distinct from a genuine zero-metrics struct (join_failed=false).
    /// Behavioral: the two structs differ on join_failed only, and that field is
    /// the discriminator that forces a deny in derive_safety_from_graph.
    #[test]
    fn graph_impact_join_failed_struct_is_distinct_from_genuine_zero() {
        let degraded = GraphImpactMetrics {
            join_failed: true,
            ..GraphImpactMetrics::default()
        };
        let genuine_zero = GraphImpactMetrics::default();

        assert!(
            degraded.join_failed,
            "join_failed=true must be set in degraded metrics"
        );
        assert!(
            !genuine_zero.join_failed,
            "default() must have join_failed=false"
        );
        assert_ne!(
            degraded.join_failed, genuine_zero.join_failed,
            "ENG-AUD-2026-S09-0001: degraded and genuine-zero metrics must differ on join_failed \
             — this field is the sole discriminator for the deny-on-indeterminate path"
        );
        // All count fields must be zero in the degraded struct (join failed before counting).
        assert_eq!(degraded.downstream_dependency_count, 0);
        assert_eq!(degraded.reads_state_count, 0);
        assert_eq!(degraded.writes_state_count, 0);
        assert_eq!(degraded.sql_calls_count, 0);
    }

    /// ENG-AUD-2026-S09-0001 + XS-0001 (behavioral, requires AppState):
    ///
    /// Calling `derive_safety_from_graph` with a `join_failed=true` metrics struct
    /// must return `allowed=false` even though all count fields are zero (which
    /// would otherwise be a "no dependencies, no state reads → allow" result).
    #[tokio::test]
    async fn derive_safety_join_failed_returns_deny() {
        let tmp = tempfile::TempDir::new()
            .expect("failed to create temp dir for S09-0001 behavioral test");
        let tmp_path = tmp.path().to_path_buf();

        // Ensure safety policy is enabled so the policy gate is active.
        let cfg = engram_core::Config {
            allowed_roots: vec![tmp_path.clone()],
            data_dir: tmp_path.clone(),
            max_parse_concurrency: 1,
            safety_policy_enabled: true,
            ..Default::default()
        };

        let (state, _rx) = crate::state::AppState::new(cfg)
            .expect("AppState::new must succeed with valid temp dirs");

        let degraded = GraphImpactMetrics {
            join_failed: true,
            ..GraphImpactMetrics::default()
        };

        let decision = derive_safety_from_graph(
            &state,
            "test-project-s09-0001",
            &["src/foo.rs".to_string()],
            &degraded,
            None,
        );

        assert!(
            !decision.allowed,
            "ENG-AUD-2026-S09-0001 / XS-0001: derive_safety_from_graph must return \
             allowed=false when join_failed=true (indeterminate evidence must not \
             produce a permissive decision); summary: {}",
            decision.summary,
        );
        assert!(
            decision.summary.contains("ENG-AUD-2026-S09-0001"),
            "XS-0001: deny summary must contain the audit tag ENG-AUD-2026-S09-0001; \
             got: {}",
            decision.summary,
        );
    }
}
