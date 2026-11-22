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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub patch_observations: Vec<String>,
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
                "Observed semantic-edge confidence: minimum {}; effective {}; threshold {:.2}; scored {}/{}; coverage {}. Caller scores cannot raise the observed minimum or supply missing graph coverage. {} {}",
                self.minimum_confidence.map_or("unknown".into(), |n| format!("{n:.2}")),
                score.map_or("unknown".into(), |n| format!("{n:.2}")),
                input.min_extraction_confidence,
                self.scored_edges,
                self.edges_checked,
                if self.complete {
                    "complete_within_declared_scope"
                } else {
                    "incomplete"
                },
                self.scope, self.patch_observations.join(" ")
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
    root: &std::path::Path,
    proposed_change: &str,
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
    let patch = super::pre_commit_review_service::parse_unified_diff(proposed_change);
    if !patch.is_empty() {
        result.scope = "For submitted patches, added lines are compared with fingerprint-verified source. An exact match selects the indexed functions containing those lines. Other files retain whole-file graph coverage. This measures outgoing semantic-edge confidence; removed code, runtime behavior and correctness are unassessed.".into();
        for changed in patch.iter().filter(|d| !files.contains(&d.path.replace('\\', "/"))).take(MAX_FILES) {
            result.gap(format!("Submitted patch file outside target_files: {}", changed.path));
        }
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
        let changed = patch.iter().find(|d| d.path.replace('\\', "/") == *file);
        let selected = if let Some(diff) = changed {
            let mut matching = BTreeSet::new();
            if !diff.added_lines.is_empty() && diff.added_lines.len() <= 8192
                && let Ok(text) = crate::planner_source::read(graph, project, root, file)
            {
                let source_lines: Vec<_> = text.lines().collect();
                if diff.added_lines.iter().all(|(line, code)| line.checked_sub(1)
                    .and_then(|i| source_lines.get(i)).is_some_and(|actual| *actual == code.trim_end_matches('\r')))
                {
                    matching.extend(diff.added_lines.iter().filter(|(_,s)| !s.trim().is_empty()).map(|(n,_)| *n));
                }
            }
            if matching.is_empty() {
                result.gap(format!("{file}: submitted additions could not be bound to the selected source; file-wide evidence only"));
                None
            } else {
                let mut owners = graph.functions_for_source_lines(project, file)?;
                owners.retain(|n| n.start_line > 0 && n.end_line >= n.start_line &&
                    matching.range(n.start_line as usize..=n.end_line as usize).next().is_some());
                if matching.iter().all(|line| owners.iter().any(|n| (n.start_line as usize..=n.end_line as usize).contains(line))) {
                    let names = owners.iter().take(6).map(|n| format!("{} at {}", n.name,n.start_line)).collect::<Vec<_>>();
                    result.patch_observations.push(format!("{file}: {} verified added lines (first {}), {} removed lines, {} overlapping indexed functions: {}", matching.len(),matching.first().unwrap(),diff.removed_lines.len(),owners.len(),names.join(", ")));
                    Some(owners)
                } else {
                    result.gap(format!("{file}: {} verified added lines include changes outside indexed function ranges; file-wide evidence only", matching.len()));
                    None
                }
            }
        } else { None };
        let nodes = match selected {
            Some(nodes) => nodes,
            None => graph.query_nodes_in_file(project, None, file, remaining + 1)?,
        };
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
    proposed_change: &str,
) -> ExtractionEvidence {
    let failed = |message: String| {
        ExtractionEvidence { generation, files_requested: files.len(),
        gaps: vec![message], scope: "Selected-source semantic-edge confidence is unavailable; do not infer confidence from structural counts.".into(), ..Default::default() }
    };
    let Some(runtime) = state.projects.get(project) else {
        return failed("project_runtime_unavailable".into());
    };
    let search = runtime.search.clone();
    let root = std::path::PathBuf::from(&runtime.info.directory);
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
    if proposed_change.len() > 2 * 1024 * 1024 { return failed("submitted patch exceeds 2 MiB for extraction evidence".into()); }
    let proposed_change = proposed_change.to_owned();
    match tokio::task::spawn_blocking(move || {
        collect(&graph, &search, &project, &files, generation, source, &root, &proposed_change)
    })
    .await
    {
        Ok(Ok(evidence)) => evidence,
        Ok(Err(error)) => failed(format!("extraction_evidence_unavailable: {error:#}")),
        Err(error) => failed(format!("extraction_evidence_worker_failed: {error}")),
    }
}
