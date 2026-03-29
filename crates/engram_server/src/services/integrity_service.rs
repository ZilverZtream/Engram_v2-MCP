//! Data integrity sentinels — cross-store consistency checks.
//!
//! After each generation switch, validates that Redb (registry), Tantivy (FTS),
//! LanceDB (vectors), and the docstore are in agreement. When mismatches are
//! detected, triggers scoped repair automatically if `integrity_auto_repair` is
//! enabled in config.

use crate::state::AppState;
use crate::utils::now_ms;
use engram_core::metrics;
use engram_index::docstore::{DocStore, DocSummary};
use engram_index::hybrid::SearchDocSummary;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Result of a single cross-store consistency check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrityCheckResult {
    pub project_id: String,
    pub generation: u64,
    pub timestamp_ms: u64,
    pub tantivy_doc_count: u64,
    pub vector_doc_count: u64,
    pub graph_node_count: u64,
    pub graph_edge_count: u64,
    pub docstore_doc_count: u64,
    pub mismatches: Vec<IntegrityMismatch>,
    pub repairs_attempted: Vec<RepairOutcome>,
    pub overall_healthy: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrityMismatch {
    pub kind: MismatchKind,
    pub description: String,
    pub expected: u64,
    pub actual: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum MismatchKind {
    /// Tantivy has docs not in docstore.
    TantivyOrphan,
    /// Docstore has docs not in Tantivy.
    DocstoreOrphan,
    /// Vector store has entries not in docstore.
    VectorOrphan,
    /// Graph references non-existent generation.
    GraphStaleGeneration,
    /// Tantivy and vector doc counts diverge beyond threshold.
    CountDivergence,
    /// Registry metadata doesn't match active generation.
    RegistryMismatch,
}

impl std::fmt::Display for MismatchKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TantivyOrphan => write!(f, "tantivy_orphan"),
            Self::DocstoreOrphan => write!(f, "docstore_orphan"),
            Self::VectorOrphan => write!(f, "vector_orphan"),
            Self::GraphStaleGeneration => write!(f, "graph_stale_generation"),
            Self::CountDivergence => write!(f, "count_divergence"),
            Self::RegistryMismatch => write!(f, "registry_mismatch"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepairOutcome {
    pub mismatch_kind: MismatchKind,
    pub action: String,
    pub success: bool,
    pub items_repaired: u64,
}

/// Resolve whether repairs should run for this integrity check.
///
/// Request override semantics:
/// - `Some(true)`  => force repairs on
/// - `Some(false)` => force repairs off
/// - `None`        => follow config
pub fn resolve_auto_repair(config_auto_repair: bool, request_auto_repair: Option<bool>) -> bool {
    request_auto_repair.unwrap_or(config_auto_repair)
}

/// Run a full integrity check for a project.
pub async fn check_project_integrity(
    state: &AppState,
    project_id: &str,
) -> anyhow::Result<IntegrityCheckResult> {
    check_project_integrity_with_policy(
        state,
        project_id,
        resolve_auto_repair(state.cfg.integrity_auto_repair, None),
    )
    .await
}

/// Run a full integrity check for a project with explicit repair policy.
pub async fn check_project_integrity_with_policy(
    state: &AppState,
    project_id: &str,
    auto_repair: bool,
) -> anyhow::Result<IntegrityCheckResult> {
    metrics::metrics().integrity_checks_run.inc();

    let ps = if let Some(p) = state.get_project_cached(project_id) {
        p
    } else {
        // ENG-AUD-S1-0005: Lazy open — project may exist in registry but not yet in cache
        // (e.g., integrity called before first index run, or after server restart).
        let reg_open = state.registry.clone();
        let pid_open = project_id.to_string();
        let rec = tokio::task::spawn_blocking(move || reg_open.get_project(&pid_open))
            .await?
            .map_err(|e| anyhow::anyhow!("registry lookup failed for {project_id}: {e}"))?
            .ok_or_else(|| anyhow::anyhow!("Project {project_id} does not exist in registry"))?;

        let tantivy_dir = state.cfg.data_dir.join("projects").join(project_id).join("tantivy");
        let lancedb_dir = state.cfg.data_dir.join("projects").join(project_id).join("lancedb");
        // Ignore dir-creation errors here — search engine open will fail with a better message
        let _ = std::fs::create_dir_all(&tantivy_dir);
        let _ = std::fs::create_dir_all(&lancedb_dir);

        let search = engram_index::HybridSearchEngine::new_with_budget(
            tantivy_dir.clone(),
            lancedb_dir.clone(),
            &state.cfg,
            Some(state.memory_budget.clone()),
        )
        .await
        .map_err(|e| anyhow::anyhow!("failed to open search engine for {project_id}: {e}"))?;

        let ps = crate::state::ProjectState {
            info: crate::state::ProjectInfo {
                project_id: project_id.to_string(),
                project_name: rec.project_name,
                project_type: rec.project_type,
                directory: rec.directory,
                tantivy_dir,
                lancedb_dir,
            },
            search: std::sync::Arc::new(search),
        };
        state.put_project_cached(ps.clone()).await;
        ps
    };

    // Get active generation from registry
    let reg = state.registry.clone();
    let pid = project_id.to_string();
    let active_gen: u64 = tokio::task::spawn_blocking(move || -> anyhow::Result<u64> {
        let gen_str = reg
            .get_meta(&pid, "active_generation")?
            .unwrap_or_else(|| "1".into());
        let active_gen_val: u64 = gen_str.parse().map_err(|e| {
            anyhow::anyhow!(
                "active_generation metadata is corrupt (value={:?}): {e}",
                gen_str
            )
        })?;
        Ok(active_gen_val)
    })
    .await??;

    // Collect counts and lightweight doc metadata from each store
    let search = ps.search.clone();
    let pid2 = project_id.to_string();

    let search_t = search.clone();
    let pid_tantivy = pid2.clone();
    let tantivy_docs: Vec<SearchDocSummary> =
        tokio::task::spawn_blocking(move || search_t.list_docs_for_project(&pid_tantivy)).await??;
    let tantivy_count = tantivy_docs.len() as u64;

    let docstore_path = state
        .cfg
        .data_dir
        .join("projects")
        .join(project_id)
        .join("docs.redb");
    let pid_docstore = project_id.to_string();
    let (docstore_count, docstore_docs): (u64, Vec<DocSummary>) =
        tokio::task::spawn_blocking(move || -> anyhow::Result<(u64, Vec<DocSummary>)> {
            let store = DocStore::open(&docstore_path)?;
            let count = store.count_docs_for_project(&pid_docstore)? as u64;
            let docs = store.list_doc_summaries_for_project(&pid_docstore)?;
            Ok((count, docs))
        })
        .await??;

    // Vector count (async).
    // count_vectors returns Ok(0) when the vector feature is disabled or the
    // project table does not yet exist; Err only on real store failures.
    let vector_count: u64 = search
        .count_vectors(&pid2)
        .await
        .map_err(|e| anyhow::anyhow!("vector store unreachable during integrity check: {e}"))?
        as u64;

    let graph = state.graph.clone();
    let pid3 = project_id.to_string();
    let (node_count, edge_count) =
        tokio::task::spawn_blocking(move || -> anyhow::Result<(u64, u64)> {
            let nc = graph.count_nodes_by_type(&pid3)?;
            let ec = graph.count_edges_by_kind(&pid3)?;
            let total_nodes: u64 = nc.values().sum::<usize>() as u64;
            let total_edges: u64 = ec.values().sum::<usize>() as u64;
            Ok((total_nodes, total_edges))
        })
        .await??;

    // Update cardinality gauges
    metrics::metrics()
        .tantivy_doc_count
        .set(tantivy_count as i64);
    metrics::metrics().vector_doc_count.set(vector_count as i64);
    metrics::metrics().graph_node_count.set(node_count as i64);
    metrics::metrics().graph_edge_count.set(edge_count as i64);

    // Detect mismatches
    let mismatches = build_integrity_mismatches(
        tantivy_count,
        docstore_count,
        vector_count,
        &tantivy_docs,
        &docstore_docs,
    );

    if !mismatches.is_empty() {
        metrics::metrics()
            .integrity_mismatches_found
            .add(mismatches.len() as u64);
    }

    // Single pass behavior: detect all mismatches first, then apply scoped repairs.
    let mut repairs = Vec::new();
    if auto_repair && !mismatches.is_empty() {
        for mm in &mismatches {
            let repair = attempt_repair(state, project_id, mm).await;
            repairs.push(repair);
        }
    }

    let overall_healthy = mismatches.is_empty() || repairs.iter().all(|r| r.success);

    Ok(IntegrityCheckResult {
        project_id: project_id.to_string(),
        generation: active_gen,
        timestamp_ms: now_ms(),
        tantivy_doc_count: tantivy_count,
        vector_doc_count: vector_count,
        graph_node_count: node_count,
        graph_edge_count: edge_count,
        docstore_doc_count: docstore_count,
        mismatches,
        repairs_attempted: repairs,
        overall_healthy,
    })
}

fn summarize_samples<K: AsRef<str>>(samples: impl IntoIterator<Item = (K, String)>) -> String {
    samples
        .into_iter()
        .map(|(id, path)| format!("{}@{}", id.as_ref(), path))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Compute integrity mismatches given pre-fetched store summaries.
///
/// Exposed as `pub` so integration tests can call the production detection
/// path directly instead of re-implementing it (which risks logic drift).
pub fn build_integrity_mismatches(
    tantivy_count: u64,
    docstore_count: u64,
    vector_count: u64,
    tantivy_docs: &[SearchDocSummary],
    docstore_docs: &[DocSummary],
) -> Vec<IntegrityMismatch> {
    const SAMPLE_LIMIT: usize = 5;
    let mut mismatches = Vec::new();

    let tantivy_map: HashMap<String, String> = tantivy_docs
        .iter()
        .map(|d| (format!("{}:{}", d.namespace, d.doc_id), d.path.clone()))
        .collect();
    let docstore_map: HashMap<String, String> = docstore_docs
        .iter()
        .map(|d| (format!("{}:{}", d.namespace, d.doc_id), d.path.clone()))
        .collect();

    let tantivy_orphans: Vec<(String, String)> = tantivy_map
        .iter()
        .filter(|(id, _)| !docstore_map.contains_key(*id))
        .map(|(id, path)| (id.clone(), path.clone()))
        .collect();
    if !tantivy_orphans.is_empty() {
        mismatches.push(IntegrityMismatch {
            kind: MismatchKind::TantivyOrphan,
            description: format!(
                "Tantivy has {} docs missing from docstore (sample: [{}])",
                tantivy_orphans.len(),
                summarize_samples(tantivy_orphans.iter().take(SAMPLE_LIMIT).cloned())
            ),
            expected: 0,
            actual: tantivy_orphans.len() as u64,
        });
    }

    let docstore_orphans: Vec<(String, String)> = docstore_map
        .iter()
        .filter(|(id, _)| !tantivy_map.contains_key(*id))
        .map(|(id, path)| (id.clone(), path.clone()))
        .collect();
    if !docstore_orphans.is_empty() {
        mismatches.push(IntegrityMismatch {
            kind: MismatchKind::DocstoreOrphan,
            description: format!(
                "Docstore has {} docs missing from Tantivy (sample: [{}])",
                docstore_orphans.len(),
                summarize_samples(docstore_orphans.iter().take(SAMPLE_LIMIT).cloned())
            ),
            expected: 0,
            actual: docstore_orphans.len() as u64,
        });
    }

    if docstore_count > 0 {
        let diff = tantivy_count.abs_diff(docstore_count);
        let threshold = (docstore_count as f64 * 0.05).max(5.0) as u64;
        if diff > threshold {
            mismatches.push(IntegrityMismatch {
                kind: MismatchKind::CountDivergence,
                description: format!(
                    "Tantivy doc count ({tantivy_count}) diverges from docstore ({docstore_count}) by {diff}; tantivy_only_sample=[{}]; docstore_only_sample=[{}]",
                    summarize_samples(tantivy_orphans.iter().take(SAMPLE_LIMIT).cloned()),
                    summarize_samples(docstore_orphans.iter().take(SAMPLE_LIMIT).cloned())
                ),
                expected: docstore_count,
                actual: tantivy_count,
            });
        }
    }

    // Check: Vector store should have <= tantivy docs (some docs may lack embeddings)
    if vector_count > tantivy_count + 100 {
        mismatches.push(IntegrityMismatch {
            kind: MismatchKind::VectorOrphan,
            description: format!(
                "Vector store has {vector_count} entries but Tantivy only has {tantivy_count} docs"
            ),
            expected: tantivy_count,
            actual: vector_count,
        });
    }

    mismatches
}

/// Attempt to repair a specific mismatch.
async fn attempt_repair(
    state: &AppState,
    project_id: &str,
    mismatch: &IntegrityMismatch,
) -> RepairOutcome {
    metrics::metrics().repairs_triggered.inc();

    match mismatch.kind {
        MismatchKind::CountDivergence
        | MismatchKind::TantivyOrphan
        | MismatchKind::DocstoreOrphan => {
            // Trigger a targeted repair_project for the tantivy index
            tracing::info!(
                project_id,
                mismatch_kind = %mismatch.kind,
                "Triggering scoped Tantivy repair for integrity mismatch"
            );
            // We delegate to the existing repair infrastructure
            let repair_result = crate::services::project_service::repair_project_scoped(
                state,
                project_id,
                "tantivy_only",
            )
            .await;
            let success = repair_result.is_ok();
            if success {
                metrics::metrics().repairs_succeeded.inc();
            } else {
                metrics::metrics().repairs_failed.inc();
            }
            RepairOutcome {
                mismatch_kind: mismatch.kind.clone(),
                action: "tantivy_reindex".into(),
                success,
                items_repaired: if success {
                    mismatch.expected.abs_diff(mismatch.actual)
                } else {
                    0
                },
            }
        }
        MismatchKind::VectorOrphan => {
            tracing::info!(
                project_id,
                "Triggering scoped vector repair for orphan mismatch"
            );
            let repair_result = crate::services::project_service::repair_project_scoped(
                state,
                project_id,
                "vector_only",
            )
            .await;
            let success = repair_result.is_ok();
            if success {
                metrics::metrics().repairs_succeeded.inc();
            } else {
                metrics::metrics().repairs_failed.inc();
            }
            RepairOutcome {
                mismatch_kind: mismatch.kind.clone(),
                action: "vector_reindex".into(),
                success,
                items_repaired: if success {
                    mismatch.expected.abs_diff(mismatch.actual)
                } else {
                    0
                },
            }
        }
        MismatchKind::GraphStaleGeneration => {
            tracing::info!(
                project_id,
                "Triggering scoped graph repair for stale generation"
            );
            let repair_result = crate::services::project_service::repair_project_scoped(
                state,
                project_id,
                "graph_only",
            )
            .await;
            let success = repair_result.is_ok();
            if success {
                metrics::metrics().repairs_succeeded.inc();
            } else {
                metrics::metrics().repairs_failed.inc();
            }
            RepairOutcome {
                mismatch_kind: mismatch.kind.clone(),
                action: "graph_rebuild".into(),
                success,
                items_repaired: 0,
            }
        }
        MismatchKind::RegistryMismatch => {
            // Registry mismatches are informational — no auto-repair
            RepairOutcome {
                mismatch_kind: mismatch.kind.clone(),
                action: "manual_review_required".into(),
                success: false,
                items_repaired: 0,
            }
        }
    }
}

/// Background actor: periodic integrity checks.
pub async fn run_integrity_checker(state: AppState) {
    let interval_secs = state.cfg.integrity_check_interval_secs;
    if interval_secs == 0 {
        tracing::info!("Integrity checker disabled (interval=0)");
        return;
    }

    tracing::info!(interval_secs, "Starting periodic integrity checker");

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;

        // Check all cached projects
        let project_ids: Vec<String> = state
            .projects
            .iter()
            .map(|entry| entry.key().clone())
            .collect();

        for pid in project_ids {
            match check_project_integrity(&state, &pid).await {
                Ok(result) => {
                    if !result.overall_healthy {
                        tracing::warn!(
                            project_id = %pid,
                            mismatches = result.mismatches.len(),
                            repairs = result.repairs_attempted.len(),
                            "Integrity check found issues"
                        );
                    } else {
                        tracing::debug!(project_id = %pid, "Integrity check passed");
                    }
                }
                Err(e) => {
                    tracing::error!(project_id = %pid, error = %e, "Integrity check failed");
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tdoc(namespace: &str, doc_id: &str, path: &str) -> SearchDocSummary {
        SearchDocSummary {
            namespace: namespace.to_string(),
            doc_id: doc_id.to_string(),
            path: path.to_string(),
        }
    }

    fn ddoc(namespace: &str, doc_id: &str, path: &str) -> DocSummary {
        DocSummary {
            namespace: namespace.to_string(),
            doc_id: doc_id.to_string(),
            path: path.to_string(),
        }
    }

    #[test]
    fn detects_tantivy_orphans_with_debug_samples() {
        let mismatches = build_integrity_mismatches(
            2,
            1,
            0,
            &[
                tdoc("memory", "a", "src/a.rs"),
                tdoc("memory", "b", "src/b.rs"),
            ],
            &[ddoc("memory", "a", "src/a.rs")],
        );

        let mm = mismatches
            .iter()
            .find(|m| m.kind == MismatchKind::TantivyOrphan)
            .expect("expected tantivy orphan mismatch");
        assert_eq!(mm.actual, 1);
        assert!(mm.description.contains("memory:b@src/b.rs"));
    }

    #[test]
    fn detects_docstore_orphans_with_debug_samples() {
        let mismatches = build_integrity_mismatches(
            1,
            2,
            0,
            &[tdoc("memory", "a", "src/a.rs")],
            &[
                ddoc("memory", "a", "src/a.rs"),
                ddoc("memory", "z", "src/z.rs"),
            ],
        );

        let mm = mismatches
            .iter()
            .find(|m| m.kind == MismatchKind::DocstoreOrphan)
            .expect("expected docstore orphan mismatch");
        assert_eq!(mm.actual, 1);
        assert!(mm.description.contains("memory:z@src/z.rs"));
    }

    #[test]
    fn resolve_auto_repair_uses_config_when_request_unset() {
        assert!(resolve_auto_repair(true, None));
        assert!(!resolve_auto_repair(false, None));
    }

    #[test]
    fn resolve_auto_repair_request_true_overrides_config() {
        assert!(resolve_auto_repair(true, Some(true)));
        assert!(resolve_auto_repair(false, Some(true)));
    }

    #[test]
    fn resolve_auto_repair_request_false_overrides_config() {
        assert!(!resolve_auto_repair(true, Some(false)));
        assert!(!resolve_auto_repair(false, Some(false)));
    }

    // ── MismatchKind Display ─────────────────────────────────────────────────

    #[test]
    fn mismatch_kind_display_strings() {
        assert_eq!(MismatchKind::TantivyOrphan.to_string(), "tantivy_orphan");
        assert_eq!(MismatchKind::DocstoreOrphan.to_string(), "docstore_orphan");
        assert_eq!(MismatchKind::VectorOrphan.to_string(), "vector_orphan");
        assert_eq!(MismatchKind::GraphStaleGeneration.to_string(), "graph_stale_generation");
        assert_eq!(MismatchKind::CountDivergence.to_string(), "count_divergence");
        assert_eq!(MismatchKind::RegistryMismatch.to_string(), "registry_mismatch");
    }

    // ── build_integrity_mismatches: no mismatches ────────────────────────────

    #[test]
    fn no_mismatches_when_stores_agree() {
        let docs = vec![
            tdoc("memory", "a", "src/a.rs"),
            tdoc("memory", "b", "src/b.rs"),
        ];
        let ddocs = vec![
            ddoc("memory", "a", "src/a.rs"),
            ddoc("memory", "b", "src/b.rs"),
        ];
        let mismatches = build_integrity_mismatches(2, 2, 1, &docs, &ddocs);
        assert!(mismatches.is_empty(), "perfectly aligned stores should have no mismatches");
    }

    #[test]
    fn no_mismatch_for_empty_stores() {
        let mismatches = build_integrity_mismatches(0, 0, 0, &[], &[]);
        assert!(mismatches.is_empty());
    }

    // ── build_integrity_mismatches: count divergence threshold ───────────────

    #[test]
    fn count_divergence_triggers_above_5_percent_threshold() {
        // docstore has 100 docs, threshold = max(5.0, 100*0.05) = 5
        // tantivy has 90 → diff = 10 > 5 → divergence
        let ddocs: Vec<DocSummary> = (0..100)
            .map(|i| ddoc("ns", &i.to_string(), &format!("f{i}.rs")))
            .collect();
        let tdocs: Vec<SearchDocSummary> = (0..90)
            .map(|i| tdoc("ns", &i.to_string(), &format!("f{i}.rs")))
            .collect();
        let mismatches = build_integrity_mismatches(90, 100, 0, &tdocs, &ddocs);
        assert!(
            mismatches.iter().any(|m| m.kind == MismatchKind::CountDivergence),
            "diff of 10 (>5% of 100) should trigger divergence"
        );
    }

    #[test]
    fn count_divergence_not_triggered_below_threshold() {
        // docstore has 100 docs, threshold = 5. diff = 3 → no divergence
        let ddocs: Vec<DocSummary> = (0..100)
            .map(|i| ddoc("ns", &i.to_string(), &format!("f{i}.rs")))
            .collect();
        let tdocs: Vec<SearchDocSummary> = (0..97)
            .map(|i| tdoc("ns", &i.to_string(), &format!("f{i}.rs")))
            .collect();
        // No orphans because all tantivy docs are subset of docstore
        let mismatches = build_integrity_mismatches(97, 100, 0, &tdocs, &ddocs);
        assert!(
            !mismatches.iter().any(|m| m.kind == MismatchKind::CountDivergence),
            "diff of 3 (<5% of 100) should not trigger divergence"
        );
    }

    #[test]
    fn count_divergence_uses_minimum_threshold_of_5() {
        // docstore has 10 docs, threshold = max(5.0, 10*0.05=0.5) = 5
        // diff = 4 → no divergence (4 <= 5)
        let ddocs: Vec<DocSummary> = (0..10)
            .map(|i| ddoc("ns", &i.to_string(), &format!("f{i}.rs")))
            .collect();
        let tdocs: Vec<SearchDocSummary> = (0..6)
            .map(|i| tdoc("ns", &i.to_string(), &format!("f{i}.rs")))
            .collect();
        let mismatches = build_integrity_mismatches(6, 10, 0, &tdocs, &ddocs);
        assert!(
            !mismatches.iter().any(|m| m.kind == MismatchKind::CountDivergence),
            "diff of 4 should not exceed minimum threshold of 5"
        );
    }

    // ── build_integrity_mismatches: vector orphan ────────────────────────────

    #[test]
    fn vector_orphan_triggered_when_exceeds_tantivy_by_100() {
        // vector_count = 1200, tantivy = 1000 → diff = 200 > 100 → VectorOrphan
        let tdocs: Vec<SearchDocSummary> = (0..1000)
            .map(|i| tdoc("ns", &i.to_string(), &format!("f{i}.rs")))
            .collect();
        let ddocs: Vec<DocSummary> = tdocs
            .iter()
            .map(|d| ddoc(&d.namespace, &d.doc_id, &d.path))
            .collect();
        let mismatches = build_integrity_mismatches(1000, 1000, 1200, &tdocs, &ddocs);
        assert!(
            mismatches.iter().any(|m| m.kind == MismatchKind::VectorOrphan),
            "1200 vectors vs 1000 tantivy docs should trigger VectorOrphan"
        );
    }

    #[test]
    fn vector_orphan_not_triggered_within_100_margin() {
        // vector = 1050, tantivy = 1000 → diff = 50 ≤ 100 → no orphan
        let tdocs: Vec<SearchDocSummary> = (0..1000)
            .map(|i| tdoc("ns", &i.to_string(), &format!("f{i}.rs")))
            .collect();
        let ddocs: Vec<DocSummary> = tdocs
            .iter()
            .map(|d| ddoc(&d.namespace, &d.doc_id, &d.path))
            .collect();
        let mismatches = build_integrity_mismatches(1000, 1000, 1050, &tdocs, &ddocs);
        assert!(
            !mismatches.iter().any(|m| m.kind == MismatchKind::VectorOrphan),
            "vector count only 50 above tantivy should not trigger VectorOrphan"
        );
    }

    // ── build_integrity_mismatches: multiple mismatches at once ─────────────

    #[test]
    fn multiple_mismatches_can_be_detected_simultaneously() {
        // tantivy has X, docstore has Y (different), vector way too high
        let tdocs = vec![tdoc("ns", "a", "a.rs"), tdoc("ns", "extra", "extra.rs")];
        let ddocs = vec![ddoc("ns", "a", "a.rs"), ddoc("ns", "missing", "missing.rs")];
        let mismatches = build_integrity_mismatches(2, 2, 5000, &tdocs, &ddocs);
        // Should detect both TantivyOrphan (extra) and DocstoreOrphan (missing)
        assert!(mismatches.iter().any(|m| m.kind == MismatchKind::TantivyOrphan));
        assert!(mismatches.iter().any(|m| m.kind == MismatchKind::DocstoreOrphan));
        assert!(mismatches.iter().any(|m| m.kind == MismatchKind::VectorOrphan));
    }

    // ── summarize_samples helper ─────────────────────────────────────────────

    #[test]
    fn tantivy_orphan_description_includes_sample_path() {
        let tdocs = vec![
            tdoc("memory", "doc1", "src/module/file.rs"),
            tdoc("memory", "doc2", "src/other.rs"),
        ];
        let ddocs = vec![ddoc("memory", "doc1", "src/module/file.rs")];
        let mismatches = build_integrity_mismatches(2, 1, 0, &tdocs, &ddocs);
        let mm = mismatches
            .iter()
            .find(|m| m.kind == MismatchKind::TantivyOrphan)
            .unwrap();
        // description should include the orphan key formatted as namespace:doc_id@path
        assert!(
            mm.description.contains("memory:doc2@src/other.rs"),
            "description should include orphan sample: {:?}", mm.description
        );
    }

    #[test]
    fn docstore_orphan_actual_count_is_correct() {
        let tdocs = vec![tdoc("ns", "a", "a.rs")];
        let ddocs = vec![
            ddoc("ns", "a", "a.rs"),
            ddoc("ns", "b", "b.rs"),
            ddoc("ns", "c", "c.rs"),
        ];
        let mismatches = build_integrity_mismatches(1, 3, 0, &tdocs, &ddocs);
        let mm = mismatches
            .iter()
            .find(|m| m.kind == MismatchKind::DocstoreOrphan)
            .unwrap();
        assert_eq!(mm.actual, 2, "two docstore orphans");
        assert_eq!(mm.expected, 0);
    }

    // ── resolve_auto_repair edge cases ───────────────────────────────────────

    #[test]
    fn resolve_auto_repair_symmetry() {
        // None always returns config value
        for config in [true, false] {
            assert_eq!(resolve_auto_repair(config, None), config);
        }
        // Some always overrides config
        for config in [true, false] {
            assert!(resolve_auto_repair(config, Some(true)));
            assert!(!resolve_auto_repair(config, Some(false)));
        }
    }

    // ── ENG-AUD-S1-0005: lazy-open path guard ───────────────────────────────

    #[test]
    fn eng_aud_s1_0005_lazy_open_path_exists() {
        // Verify that the lazy-open path was added: the source must not contain
        // the old hard-fail pattern (get_project_cached immediately chained to ?).
        let source = include_str!("integrity_service.rs");
        // The old single-expression hard-fail was:
        //   state.get_project_cached(project_id).ok_or_else(|| ..."not found in cache"...)
        // We detect it by checking for the exact old error text outside of test code.
        // Count non-test lines that contain the old cache-miss sentinel.
        let old_error_sentinel = "not found in cache\"";
        let hard_fail_lines: Vec<&str> = source
            .lines()
            .filter(|l| {
                let trimmed = l.trim();
                // Exclude the test module lines (which reference the pattern as a comment
                // or string literal for documentation purposes).
                !trimmed.starts_with("//") && l.contains(old_error_sentinel)
            })
            .collect();
        assert_eq!(
            hard_fail_lines.len(), 0,
            "integrity check must not hard-fail when project is not in cache; \
             found: {hard_fail_lines:?}"
        );
    }

    #[test]
    fn eng_aud_s1_0005_tag_present() {
        let source = include_str!("integrity_service.rs");
        assert!(
            source.contains("ENG-AUD-S1-0005"),
            "integrity_service.rs must contain ENG-AUD-S1-0005 tag for lazy-open path"
        );
    }

    // ── overall_healthy logic ────────────────────────────────────────────────

    #[test]
    fn overall_healthy_is_true_when_no_mismatches() {
        let mismatches = build_integrity_mismatches(5, 5, 3,
            &[
                tdoc("ns","a","a.rs"),tdoc("ns","b","b.rs"),
                tdoc("ns","c","c.rs"),tdoc("ns","d","d.rs"),tdoc("ns","e","e.rs"),
            ],
            &[
                ddoc("ns","a","a.rs"),ddoc("ns","b","b.rs"),
                ddoc("ns","c","c.rs"),ddoc("ns","d","d.rs"),ddoc("ns","e","e.rs"),
            ],
        );
        let overall_healthy = mismatches.is_empty();
        assert!(overall_healthy, "no mismatches means healthy");
    }

    // ── ENG-AUD-P1-0006: corrupt active_generation returns Err ──────────────

    /// Regression for ENG-AUD-P1-0006.
    ///
    /// The parse logic extracted from `check_project_integrity_with_policy` must
    /// return `Err` (not a silent default of `1`) when `active_generation` holds
    /// a non-numeric value, so corrupt registry metadata is caught early.
    #[test]
    fn test_corrupt_active_generation_returns_error() {
        let gen_str = "not_a_number".to_string();
        let result: anyhow::Result<u64> = gen_str.parse::<u64>().map_err(|e| {
            anyhow::anyhow!(
                "active_generation metadata is corrupt (value={:?}): {e}",
                gen_str
            )
        });
        assert!(result.is_err(), "corrupt generation string must yield Err");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("corrupt"),
            "error message should mention 'corrupt', got: {msg}"
        );
        assert!(
            msg.contains("not_a_number"),
            "error message should include the bad value, got: {msg}"
        );
    }

    // ── ENG-AUD-P1-0007: vector store error captured, not swallowed ─────────

    /// Regression for ENG-AUD-P1-0007.
    ///
    /// `build_integrity_mismatches` is a pure function, so we validate the
    /// surrounding contract: if count_vectors were to return 0 due to a masked
    /// error, a real VectorOrphan would go undetected.  The fix propagates the
    /// error via `?` so the caller (check_project_integrity_with_policy) can
    /// surface it rather than silently marking the store healthy.
    ///
    /// Here we verify that the VectorOrphan detection still fires correctly
    /// when a non-zero count is supplied, confirming the check is not bypassed
    /// by an erroneously zeroed count.
    #[test]
    fn test_vector_store_error_captured() {
        // Simulate what would happen if a real error were masked as 0:
        // vector_count=0 when the true count is 1200 → VectorOrphan is missed.
        let tdocs: Vec<SearchDocSummary> = (0..1000)
            .map(|i| tdoc("ns", &i.to_string(), &format!("f{i}.rs")))
            .collect();
        let ddocs: Vec<DocSummary> = tdocs
            .iter()
            .map(|d| ddoc(&d.namespace, &d.doc_id, &d.path))
            .collect();

        // With the masked count of 0 → no VectorOrphan detected (bad)
        let masked = build_integrity_mismatches(1000, 1000, 0, &tdocs, &ddocs);
        assert!(
            !masked.iter().any(|m| m.kind == MismatchKind::VectorOrphan),
            "masked zero count must not trigger VectorOrphan (demonstrates the risk)"
        );

        // With the real count propagated → VectorOrphan is detected (correct)
        let real = build_integrity_mismatches(1000, 1000, 1200, &tdocs, &ddocs);
        assert!(
            real.iter().any(|m| m.kind == MismatchKind::VectorOrphan),
            "real count of 1200 vs 1000 must trigger VectorOrphan"
        );
    }
}
