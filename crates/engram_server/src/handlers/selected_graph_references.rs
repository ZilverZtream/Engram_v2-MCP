//! Direct graph navigation limited to one published source selection.
use crate::models::{Direction, FindReferencesRequest};
use engram_graph::{EdgeKind, GraphStore, Node};
use engram_index::hybrid::SelectedSourceRevisionLookup;
use serde_json::{Value, json};

const EDGE_LIMIT: usize = 100;

pub(super) fn read(
    graph: &GraphStore,
    selected: &SelectedSourceRevisionLookup,
    req: FindReferencesRequest,
    kind: EdgeKind,
    generation: u64,
) -> anyhow::Result<String> {
    let project = &req.project_id;
    let mut edges = Vec::new();
    let mut truncated = false;
    let mut unavailable_edges = 0;
    if matches!(req.direction, Direction::In | Direction::Both) {
        let (incoming, more) =
            graph.incoming_edge_prefix(project, kind.clone(), &req.node_id, EDGE_LIMIT)?;
        let keys: Vec<_> = incoming
            .into_iter()
            .map(|(source, kind, _)| (source, kind, req.node_id.clone()))
            .collect();
        edges = graph.get_edges_by_endpoints(project, &keys)?;
        unavailable_edges = keys.len().saturating_sub(edges.len());
        truncated = more;
    }
    if matches!(req.direction, Direction::Out | Direction::Both) {
        let (outgoing, more) =
            graph.outgoing_edges_of_kind(project, kind.clone(), &req.node_id, EDGE_LIMIT)?;
        edges.extend(outgoing);
        truncated |= more;
    }

    let ids: Vec<_> = std::iter::once(req.node_id.clone())
        .chain(
            edges
                .iter()
                .flat_map(|edge| [edge.source_id.clone(), edge.target_id.clone()]),
        )
        .collect();
    let nodes = graph.get_nodes(project, &ids)?;
    let endpoint = |id: &str| {
        nodes
            .get(id)
            .and_then(Option::as_ref)
            .filter(|node| node.namespace == engram_core::namespaces::NAMESPACE_MEMORY)
    };
    let mut withheld_nodes = [0; 2];
    let anchor = endpoint(&req.node_id);
    let anchor_admitted = super::admit_graph_source(Some(selected), anchor, &mut withheld_nodes);
    let mut revision_edges = 0;
    let mut rows = Vec::new();
    for edge in edges {
        let source = endpoint(&edge.source_id);
        let target = endpoint(&edge.target_id);
        let source_admitted =
            super::admit_graph_source(Some(selected), source, &mut withheld_nodes);
        let target_admitted =
            super::admit_graph_source(Some(selected), target, &mut withheld_nodes);
        if let (Some(source), Some(target), true, true) =
            (source, target, source_admitted, target_admitted)
        {
            if edge.namespace != engram_core::namespaces::NAMESPACE_MEMORY
                || edge.generation != source.generation
            {
                revision_edges += 1;
                continue;
            }
            rows.push(json!({
                "kind":edge.edge_kind.as_str(), "source":identity(source), "target":identity(target),
                "edge_source_revision":edge.generation, "weight":edge.weight,
            }));
        }
    }
    let report = json!({
        "project_id":project, "active_generation":generation, "edge_kind":kind.as_str(),
        "direction":req.direction, "anchor_id":req.node_id,
        "anchor":anchor.filter(|_| anchor_admitted).map(identity),
        "edges":rows, "truncated":truncated, "edge_limit_per_direction":EDGE_LIMIT,
        "withheld":{
            "unselected_endpoint_observations":withheld_nodes[0],
            "unavailable_endpoint_observations":withheld_nodes[1],
            "source_revision_mismatch_edges":revision_edges,
            "unavailable_edges":unavailable_edges
        },
        "coverage":"Candidates discovered in the live graph are bounded before checking both endpoint file revisions and the edge source revision. Withheld observations can repeat. Missing or overwritten graph records cannot be reconstructed. This evidence does not establish an atomic graph snapshot, matching disk bytes or runtime calls.",
        "next_tools":[{"tool":"get_index_freshness","arguments":{"project_id":project}}]
    });
    Ok(serde_json::to_string(&report)?)
}

fn identity(node: &Node) -> Value {
    json!({
        "node_id":node.node_id, "name":node.name, "file_path":node.file_path,
        "line":node.start_line, "physical_revision":node.generation
    })
}
