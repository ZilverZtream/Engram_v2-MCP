//! Project indexed call targets into identities usable by existing read tools.
use super::{ProviderStatus, fqn_from_node};
use engram_graph::GraphStore;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

const EDGE_LIMIT: usize = 200;
const TARGET_LIMIT: usize = 50;
const SCOPE: &str = "Indexed graph observations only, not an atomic source snapshot, exhaustive source-call analysis or runtime execution. Metadata call hints are unverified and are not canonical target identities. Complete means the bounded indexed lookups completed. Check current source before editing.";

#[derive(Debug, Clone, Serialize)]
pub struct Target {
    pub node_id: String,
    pub fqn: String,
    pub file_path: String,
    pub line_start: u32,
    pub line_end: u32,
    pub line_kind: &'static str,
    pub indexed_generation: u64,
    pub edge_kinds: Vec<&'static str>,
    pub resolve_action: Value,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Inventory {
    pub status: ProviderStatus,
    pub targets: Vec<Target>,
    pub metadata_hints: Vec<String>,
    pub missing_targets: usize,
    pub non_method_targets: usize,
    pub failed_target_lookups: usize,
    pub uninspected_targets: usize,
    pub edge_kinds_truncated: Vec<&'static str>,
    pub issues: Vec<String>,
    pub interpretation: &'static str,
}

pub(super) fn collect(
    graph: &GraphStore,
    project: &str,
    source: &str,
    metadata_hints: Vec<String>,
) -> Inventory {
    let mut result = Inventory {
        metadata_hints,
        interpretation: SCOPE,
        ..Default::default()
    };
    let mut candidates = BTreeMap::<String, (u32, BTreeSet<&'static str>)>::new();
    for kind in crate::handlers::CALLER_EDGE_KINDS {
        let kind_name = kind.as_str();
        match graph.neighbors(project, kind, source, EDGE_LIMIT + 1) {
            Ok(edges) => {
                if edges.len() > EDGE_LIMIT {
                    result.edge_kinds_truncated.push(kind_name);
                }
                for (id, weight) in edges.into_iter().take(EDGE_LIMIT) {
                    let entry = candidates.entry(id).or_default();
                    entry.0 = entry.0.max(weight);
                    entry.1.insert(kind_name);
                }
            }
            Err(error) => result
                .issues
                .push(format!("{kind_name} target lookup failed: {error}")),
        }
    }
    let mut candidates = candidates.into_iter().collect::<Vec<_>>();
    candidates.sort_by(|(a, aw), (b, bw)| bw.0.cmp(&aw.0).then(a.cmp(b)));
    result.uninspected_targets = candidates.len().saturating_sub(TARGET_LIMIT);
    for (id, (_, kinds)) in candidates.into_iter().take(TARGET_LIMIT) {
        match graph.get_node_bounded(project, &id, 256 * 1024) {
            Ok(Some((node, _))) if node.node_type == "function" => result.targets.push(Target {
                fqn: fqn_from_node(&node), file_path: node.file_path.as_str().into(),
                line_start: node.start_line, line_end: node.end_line, line_kind: "declaration",
                indexed_generation: node.generation, edge_kinds: kinds.into_iter().collect(),
                resolve_action: json!({"tool":"resolve_id","arguments":{"project_id":project,"id":id}}),
                node_id: id,
            }),
            Ok(Some(_)) => result.non_method_targets += 1,
            Ok(None) => result.missing_targets += 1,
            Err(error) => {
                result.failed_target_lookups += 1;
                // Keep bounded actionable examples; the count includes all failures.
                if result.issues.len() < 3 { result.issues.push(format!("Target {id} unavailable: {error}")); }
            }
        }
    }
    result.status = if result.uninspected_targets > 0 || !result.edge_kinds_truncated.is_empty() {
        ProviderStatus::Truncated {
            shown: result.targets.len(),
            cap: TARGET_LIMIT,
            known_total: None,
        }
    } else if result.missing_targets + result.non_method_targets + result.failed_target_lookups > 0
        || !result.issues.is_empty()
    {
        ProviderStatus::Partial { reasons: vec!["Some indexed target identities could not be supplied; see omission counts and issues.".into()] }
    } else {
        ProviderStatus::Complete
    };
    result
}

pub(super) fn render_markdown(inventory: &Inventory, text: &mut String) {
    use std::fmt::Write;
    let _ = writeln!(
        text,
        "\n## Indexed callee identities\n\nCoverage: {}. Missing: {}; non-method targets: {}; failed lookups: {}; uninspected: {}.\n\n{}\n",
        serde_json::to_string(&inventory.status).unwrap_or_default(),
        inventory.missing_targets,
        inventory.non_method_targets,
        inventory.failed_target_lookups,
        inventory.uninspected_targets,
        inventory.interpretation
    );
    for target in &inventory.targets {
        let _ = writeln!(
            text,
            "- `{}` — node `{}`, `{}` lines {}–{}, indexed revision {}. Edges: {}.\n  `{}`",
            target.fqn,
            target.node_id,
            target.file_path,
            target.line_start,
            target.line_end,
            target.indexed_generation,
            target.edge_kinds.join(", "),
            target.resolve_action
        );
    }
    if !inventory.metadata_hints.is_empty() {
        let _ = writeln!(
            text,
            "\nUnverified metadata call hints: {}.",
            inventory.metadata_hints.join(", ")
        );
    }
    for issue in &inventory.issues {
        let _ = writeln!(text, "\n- {issue}");
    }
}
