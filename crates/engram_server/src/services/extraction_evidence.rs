//! ADP extraction evidence: producer confidence on observed semantic edges.
//! This is not an estimate of parser recall or proof of runtime behavior.
use crate::{
    AppState,
    services::autonomous_decision_service::{AdpInput, GateResult},
};
use engram_graph::{EdgeKind, GraphStore};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, sync::Arc};

const MAX_FILES: usize = 32;
const MAX_NODES: usize = 512;
const MAX_EDGES: usize = 4096;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeWitness {
    pub source: String,
    pub target: String,
    pub kind: String,
    pub confidence: Option<f64>,
    pub resolution: Option<String>,
    pub issue: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtractionEvidence {
    pub generation: u64,
    pub files_requested: usize,
    pub files_checked: usize,
    pub nodes_checked: usize,
    pub edges_checked: usize,
    pub scored_edges: usize,
    pub unscored_edges: usize,
    pub minimum_confidence: Option<f64>,
    pub complete: bool,
    pub gaps: Vec<String>,
    pub witnesses: Vec<EdgeWitness>,
    pub witnesses_omitted: usize,
    pub scope: String,
}

impl ExtractionEvidence {
    fn gap(&mut self, message: impl Into<String>) {
        self.complete = false;
        let message = message.into();
        if self.gaps.len() < 16 && !self.gaps.contains(&message) {
            self.gaps.push(message);
        }
    }

    pub fn gate(&self, input: &AdpInput) -> GateResult {
        let supplied = input.extraction_confidence;
        let score = match (self.minimum_confidence, supplied) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let below = score.is_some_and(|score| score < input.min_extraction_confidence);
        let known = self.complete && self.scored_edges > 0;
        GateResult {
            gate_id: "extraction_confidence".into(),
            gate_name: "Extraction Confidence".into(),
            passed: known && !below,
            confidence: if known || below {
                score.unwrap_or(0.0)
            } else {
                0.0
            },
            skipped: !known && !below,
            detail: format!(
                "Observed semantic-edge confidence: minimum {:?}; effective {:?}; threshold {:.2}; scored {}/{}; coverage {}. Caller scores cannot raise the observed minimum or supply missing graph coverage. {}",
                self.minimum_confidence,
                score,
                input.min_extraction_confidence,
                self.scored_edges,
                self.edges_checked,
                if self.complete {
                    "complete_within_declared_scope"
                } else {
                    "incomplete"
                },
                self.scope
            ),
            extraction_evidence: Some(self.clone()),
        }
    }
}

fn metadata_score(value: Option<&serde_json::Value>) -> Option<f64> {
    value
        .and_then(|value| value.as_f64().or_else(|| value.as_str()?.parse().ok()))
        .filter(|score| score.is_finite() && (0.0..=1.0).contains(score))
}

/// Use the existing exact-file lookup and source-revision membership. No graph
/// edits or traversal expansion; uncertainty remains visible to the gate.
fn collect(
    graph: &GraphStore,
    search: &engram_index::HybridSearchEngine,
    project: &str,
    files: &[String],
    generation: u64,
    source: Arc<engram_index::SourceSnapshotGuard>,
) -> anyhow::Result<ExtractionEvidence> {
    let membership = search.selected_source_revision_lookup(project, generation, source)?;
    let files = files
        .iter()
        .map(|f| f.replace('\\', "/"))
        .collect::<BTreeSet<_>>();
    let mut result = ExtractionEvidence { generation, files_requested: files.len(), complete: true,
        scope: "Producer-reported confidence for observed outgoing semantic edges of exact target files at the named source selection. Membership edges and historical associations are excluded. This does not measure missing-edge recall, working-tree freshness, runtime behavior or overall edit safety.".into(), ..Default::default() };
    if files.len() > MAX_FILES {
        result.gap("target_file_limit: at most 32 distinct files are inspected");
    }
    for file in files.iter().take(MAX_FILES) {
        let file_node = graph.get_node(project, &format!("file:{file}"))?;
        let Some(file_node) = file_node else {
            result.gap(format!("file_not_indexed: {file}"));
            continue;
        };
        if membership.matches(file, file_node.generation)? != Some(true) {
            result.gap(format!("file_revision_not_selected: {file}"));
            continue;
        }
        result.files_checked += 1;
        let remaining = MAX_NODES.saturating_sub(result.nodes_checked);
        let nodes = graph.query_nodes_in_file(project, None, file, remaining + 1)?;
        if nodes.len() > remaining {
            result.gap("node_limit: at most 512 nodes are inspected");
        }
        for node in nodes.iter().take(remaining) {
            result.nodes_checked += 1;
            if membership.matches(file, node.generation)? != Some(true) {
                result.gap(format!("node_revision_not_selected: {}", node.node_id));
                continue;
            }
            for kind in EdgeKind::ALL.iter().filter(|kind| {
                crate::services::blast_radius_service::is_causal_dependency(kind)
                    || matches!(
                        kind,
                        EdgeKind::UnresolvedStateRead | EdgeKind::UnresolvedStateWrite
                    )
            }) {
                let remaining = MAX_EDGES.saturating_sub(result.edges_checked);
                let (edges, truncated) = graph.outgoing_edges_of_kind(
                    project,
                    kind.clone(),
                    &node.node_id,
                    remaining,
                )?;
                if truncated {
                    result.gap("edge_limit: at most 4096 semantic edges are inspected");
                }
                for edge in edges {
                    result.edges_checked += 1;
                    let metadata = edge.metadata.as_ref();
                    let score = metadata_score(metadata.and_then(|m| m.get("confidence")));
                    let resolution = metadata
                        .and_then(|m| m.get("resolution"))
                        .and_then(|v| v.as_str())
                        .map(str::to_owned);
                    let target = graph.get_node(project, &edge.target_id)?;
                    let mut issue = if edge.generation != node.generation {
                        Some("edge_source_revision_mismatch")
                    } else {
                        None
                    };
                    if target
                        .as_ref()
                        .is_none_or(|target| target.generation > generation)
                    {
                        issue = Some("target_missing_or_newer_than_selection");
                    }
                    if let Some(target) = target
                        .as_ref()
                        .filter(|target| !target.file_path.as_str().is_empty())
                        && membership.matches(target.file_path.as_str(), target.generation)?
                            != Some(true)
                    {
                        issue = Some("target_revision_not_selected");
                    }
                    if score.is_none() {
                        issue = Some("missing_or_invalid_confidence");
                    }
                    if issue.is_some() {
                        result.unscored_edges += 1;
                        result.gap("Some semantic edges lack valid confidence or selected endpoint revisions; inspect witnesses and refresh/recover evidence.");
                    } else {
                        result.scored_edges += 1;
                        let score = score.unwrap();
                        result.minimum_confidence = Some(
                            result
                                .minimum_confidence
                                .map_or(score, |old| old.min(score)),
                        );
                    }
                    result.witnesses.push(EdgeWitness {
                        source: edge.source_id,
                        target: edge.target_id,
                        kind: kind.as_str().into(),
                        confidence: score,
                        resolution,
                        issue: issue.map(str::to_owned),
                    });
                }
            }
        }
    }
    if result.edges_checked == 0 {
        result.gap("no_semantic_edge_evidence: graph density or a missing edge is not extraction confidence");
    }
    result.witnesses.sort_by(|a, b| {
        a.issue
            .is_none()
            .cmp(&b.issue.is_none())
            .then_with(|| {
                a.confidence
                    .unwrap_or(-1.0)
                    .total_cmp(&b.confidence.unwrap_or(-1.0))
            })
            .then_with(|| (&a.source, &a.kind, &a.target).cmp(&(&b.source, &b.kind, &b.target)))
    });
    result.witnesses_omitted = result.witnesses.len().saturating_sub(8);
    result.witnesses.truncate(8);
    Ok(result)
}

pub async fn gather(
    state: &AppState,
    project: &str,
    files: &[String],
    generation: u64,
    selected: Option<Arc<engram_index::SourceSnapshotGuard>>,
) -> ExtractionEvidence {
    let failed = |message: String| {
        ExtractionEvidence { generation, files_requested: files.len(),
        gaps: vec![message], scope: "Selected-source semantic-edge confidence is unavailable; do not infer confidence from structural counts.".into(), ..Default::default() }
    };
    let Some(runtime) = state.projects.get(project) else {
        return failed("project_runtime_unavailable".into());
    };
    let search = runtime.search.clone();
    drop(runtime);
    let source = match selected {
        Some(source) => source,
        None => match crate::services::project_service::pin_source_generation(
            state, &search, project, generation,
        ) {
            Ok(source) => Arc::new(source),
            Err(error) => return failed(format!("source_selection_unavailable: {error:#}")),
        },
    };
    let graph = state.graph.clone();
    let project = project.to_owned();
    let files = files.to_vec();
    match tokio::task::spawn_blocking(move || {
        collect(&graph, &search, &project, &files, generation, source)
    })
    .await
    {
        Ok(Ok(evidence)) => evidence,
        Ok(Err(error)) => failed(format!("extraction_evidence_unavailable: {error:#}")),
        Err(error) => failed(format!("extraction_evidence_worker_failed: {error}")),
    }
}
