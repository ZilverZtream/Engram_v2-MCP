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

/// Run a full integrity check for a project.
pub async fn check_project_integrity(
    state: &AppState,
    project_id: &str,
) -> anyhow::Result<IntegrityCheckResult> {
    metrics::metrics().integrity_checks_run.inc();

    let ps = state
        .get_project_cached(project_id)
        .ok_or_else(|| anyhow::anyhow!("Project {project_id} not found in cache"))?;

    // Get active generation from registry
    let reg = state.registry.clone();
    let pid = project_id.to_string();
    let active_gen: u64 = tokio::task::spawn_blocking(move || -> anyhow::Result<u64> {
        let gen_str = reg
            .get_meta(&pid, "active_generation")?
            .unwrap_or_else(|| "1".into());
        Ok(gen_str.parse().unwrap_or(1))
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

    // Vector count (async)
    let vector_count: u64 = search.count_vectors(&pid2).await.unwrap_or(0) as u64;

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

    // Attempt auto-repair if configured
    let mut repairs = Vec::new();
    if state.cfg.integrity_auto_repair && !mismatches.is_empty() {
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

fn build_integrity_mismatches(
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

#[cfg(test)]
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
