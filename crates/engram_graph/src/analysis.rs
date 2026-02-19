use crate::store::{EdgeKind, GraphStore};
use petgraph::graph::NodeIndex;
use petgraph::prelude::DiGraph;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct CentralityMetrics {
    pub pagerank: HashMap<String, f32>,
}

/// Compute PageRank for all nodes in a project.
///
/// This materializes the project's dependency graph in memory using petgraph.
pub fn compute_pagerank(
    store: &GraphStore,
    project_id: &str,
    generation: u64,
) -> anyhow::Result<CentralityMetrics> {
    // 0. Check cache
    if let Ok(Some(cached)) = store.get_cached_centrality(project_id, generation) {
        return Ok(CentralityMetrics { pagerank: cached });
    }

    let mut graph = DiGraph::<String, f32>::new();
    let mut node_to_idx = HashMap::new();

    // 1. Load all nodes
    let node_ids = store.list_node_ids(project_id, None)?;
    for id in node_ids {
        let idx = graph.add_node(id.clone());
        node_to_idx.insert(id, idx);
    }

    // 2. Load edges from multiple kinds with different weights for centrality.
    // Dependency (calls, event_wiring): weight 1.0 — direct code dependencies
    // Imports: weight 0.5 — indicates file importance but weaker signal
    // Contains: weight 0.3 — structural containment (class contains method)
    // SqlCalls: weight 0.8 — SQL usage is a strong signal in WebForms apps
    let edge_configs: &[(EdgeKind, f32)] = &[
        (EdgeKind::Dependency, 1.0),
        (EdgeKind::Imports, 0.5),
        (EdgeKind::Contains, 0.3),
        (EdgeKind::SqlCalls, 0.8),
    ];
    for (kind, weight_multiplier) in edge_configs {
        let edges = store.list_edges(project_id, Some(kind.clone()))?;
        for edge in edges {
            if let (Some(&src), Some(&dst)) = (
                node_to_idx.get(&edge.source_id),
                node_to_idx.get(&edge.target_id),
            ) {
                graph.add_edge(src, dst, edge.weight as f32 * weight_multiplier);
            }
        }
    }

    // 3. Compute PageRank
    // We'll use a simple iterative implementation as petgraph doesn't have it built-in
    // (rustworkx-core might, but let's keep it simple first).
    let scores = pagerank(&graph, 0.85, 20);

    let mut pagerank_map = HashMap::new();
    for (idx, score) in scores {
        if let Some(id) = graph.node_weight(idx) {
            pagerank_map.insert(id.clone(), score);
        }
    }

    // 4. Store in cache
    let _ = store.set_cached_centrality(project_id, generation, &pagerank_map);

    Ok(CentralityMetrics {
        pagerank: pagerank_map,
    })
}

fn pagerank(
    graph: &DiGraph<String, f32>,
    damping: f32,
    iterations: usize,
) -> HashMap<NodeIndex, f32> {
    let node_count = graph.node_count();
    if node_count == 0 {
        return HashMap::new();
    }

    let mut scores = HashMap::new();
    let initial_score = 1.0 / node_count as f32;
    for node in graph.node_indices() {
        scores.insert(node, initial_score);
    }

    for _ in 0..iterations {
        let mut next_scores = HashMap::new();
        let mut sink_score = 0.0;

        for node in graph.node_indices() {
            let out_degree = graph.neighbors(node).count();
            if out_degree == 0 {
                sink_score += scores[&node];
            } else {
                let contribution = scores[&node] / out_degree as f32;
                for neighbor in graph.neighbors(node) {
                    *next_scores.entry(neighbor).or_insert(0.0) += contribution;
                }
            }
        }

        let base_score = (1.0 - damping + damping * sink_score) / node_count as f32;
        for node in graph.node_indices() {
            let score = base_score + damping * next_scores.get(&node).unwrap_or(&0.0);
            scores.insert(node, score);
        }
    }

    scores
}
