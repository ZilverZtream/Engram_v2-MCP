use super::integrity_service::{IntegrityMismatch, MismatchKind};
use engram_index::HybridSearchEngine;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamespaceCounts {
    pub tantivy_doc_count: u64,
    pub vector_doc_count: Option<u64>,
    /// Memory uses the separate published source comparison instead.
    pub compared: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamespaceIntegrity {
    pub coverage: String,
    pub tantivy_doc_count: u64,
    pub vector_doc_count: Option<u64>,
    pub namespaces: BTreeMap<String, NamespaceCounts>,
    pub unattributed_vector_rows: Option<u64>,
    /// Retained non-memory counts only; not proof of row identity or quality.
    pub counts_match: Option<bool>,
}

pub async fn observe(
    search: &HybridSearchEngine,
    project_id: &str,
    embeddings_expected: bool,
) -> anyhow::Result<NamespaceIntegrity> {
    let lexical = search.clone();
    let project = project_id.to_owned();
    let counts =
        tokio::task::spawn_blocking(move || lexical.count_all_namespace_docs(&project)).await??;
    let names: BTreeSet<String> = counts
        .keys()
        .cloned()
        .chain(
            engram_core::namespaces::KNOWN_NAMESPACES
                .iter()
                .map(|name| (*name).to_owned()),
        )
        .collect();
    let vectors = if embeddings_expected {
        search
            .count_namespace_vectors(project_id, names.iter().cloned().collect())
            .await?
    } else {
        None
    };
    let classified = vectors
        .as_ref()
        .map(|vectors| vectors.by_namespace.values().sum::<usize>());
    if let (Some(classified), Some(vectors)) = (classified, &vectors) {
        anyhow::ensure!(
            classified <= vectors.total,
            "Vector namespace counts changed during observation; retry integrity check"
        );
    }
    let mut report = NamespaceIntegrity {
        coverage: if !embeddings_expected {
            "embeddings_disabled"
        } else if vectors.is_some() {
            "retained_non_memory_namespace_counts_only"
        } else {
            "vector_support_unavailable"
        }
        .into(),
        tantivy_doc_count: counts.values().sum::<usize>() as u64,
        vector_doc_count: vectors.as_ref().map(|counts| counts.total as u64),
        namespaces: BTreeMap::new(),
        unattributed_vector_rows: vectors
            .as_ref()
            .map(|counts| (counts.total - classified.unwrap_or(0)) as u64),
        counts_match: vectors.as_ref().map(|_| true),
    };
    for name in names {
        let lexical = counts.get(&name).copied().unwrap_or(0) as u64;
        let vector = vectors
            .as_ref()
            .map(|counts| counts.by_namespace.get(&name).copied().unwrap_or(0) as u64);
        if lexical == 0 && vector.unwrap_or(0) == 0 {
            continue;
        }
        let compared = name != "memory" && vector.is_some();
        if compared && vector != Some(lexical) {
            report.counts_match = Some(false);
        }
        report.namespaces.insert(
            name,
            NamespaceCounts {
                tantivy_doc_count: lexical,
                vector_doc_count: vector,
                compared,
            },
        );
    }
    if report
        .unattributed_vector_rows
        .is_some_and(|count| count > 0)
    {
        report.counts_match = Some(false);
    }
    Ok(report)
}

/// Keep one signal per direction. A single all-namespace rebuild repairs both
/// directions; the owned recheck prevents the second signal repeating work.
pub fn include_mismatches(mismatches: &mut Vec<IntegrityMismatch>, report: &NamespaceIntegrity) {
    for (namespace, counts) in &report.namespaces {
        if !counts.compared {
            continue;
        }
        let Some(actual) = counts.vector_doc_count else {
            continue;
        };
        let expected = counts.tantivy_doc_count;
        if expected == actual {
            continue;
        }
        include(
            mismatches,
            expected,
            actual,
            format!(
                "Retained namespace {namespace:?} has {expected} Tantivy docs and {actual} vectors (all retained generations)"
            ),
        );
    }
    if let Some(extra) = report.unattributed_vector_rows.filter(|extra| *extra > 0) {
        include(
            mismatches,
            0,
            extra,
            format!(
                "Vector table has {extra} rows outside discovered lexical and known namespaces; namespace identity unavailable"
            ),
        );
    }
}

fn include(
    mismatches: &mut Vec<IntegrityMismatch>,
    expected: u64,
    actual: u64,
    description: String,
) {
    let kind = if actual < expected {
        MismatchKind::VectorShortfall
    } else {
        MismatchKind::VectorOrphan
    };
    if let Some(existing) = mismatches.iter_mut().find(|mismatch| mismatch.kind == kind) {
        existing.description.push_str("; ");
        existing.description.push_str(&description);
    } else {
        mismatches.push(IntegrityMismatch {
            kind,
            description,
            expected,
            actual,
        });
    }
}
