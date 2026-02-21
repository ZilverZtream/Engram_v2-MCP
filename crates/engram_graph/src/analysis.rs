use crate::store::{EdgeKind, GraphStore};
use petgraph::graph::NodeIndex;
use petgraph::prelude::DiGraph;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct CentralityMetrics {
    pub pagerank: HashMap<String, f32>,
}

/// Multi-algorithm centrality result for reranking.
#[derive(Debug, Clone)]
pub struct MultiCentrality {
    pub pagerank: HashMap<String, f32>,
    pub in_degree: HashMap<String, u32>,
    pub out_degree: HashMap<String, u32>,
    pub betweenness: HashMap<String, f32>,
}

impl MultiCentrality {
    /// Compute a blended score using configurable weights for each algorithm.
    /// All sub-scores are normalized to [0, 1] before blending.
    pub fn blended_score(
        &self,
        node_id: &str,
        pr_weight: f32,
        degree_weight: f32,
        betweenness_weight: f32,
    ) -> f32 {
        let pr = self.pagerank.get(node_id).copied().unwrap_or(0.0);
        let in_d = self.in_degree.get(node_id).copied().unwrap_or(0) as f32;
        let bt = self.betweenness.get(node_id).copied().unwrap_or(0.0);

        // Normalize: divide by max in each category (avoid /0).
        let pr_max = self
            .pagerank
            .values()
            .copied()
            .fold(f32::MIN, f32::max)
            .max(f32::EPSILON);
        let in_max = self.in_degree.values().copied().max().unwrap_or(1).max(1) as f32;
        let bt_max = self
            .betweenness
            .values()
            .copied()
            .fold(f32::MIN, f32::max)
            .max(f32::EPSILON);

        let total_weight = pr_weight + degree_weight + betweenness_weight;
        if total_weight < f32::EPSILON {
            return 0.0;
        }

        ((pr / pr_max) * pr_weight
            + (in_d / in_max) * degree_weight
            + (bt / bt_max) * betweenness_weight)
            / total_weight
    }
}

/// Edge weight configuration shared between single and multi-centrality.
const EDGE_CONFIGS: &[(EdgeKind, f32)] = &[
    (EdgeKind::Dependency, 1.0),
    (EdgeKind::Imports, 0.5),
    (EdgeKind::Contains, 0.3),
    (EdgeKind::SqlCalls, 0.8),
    (EdgeKind::HasColumn, 0.2),
    (EdgeKind::ForeignKey, 0.4),
    (EdgeKind::QueriesTable, 0.7),
    (EdgeKind::ReadsState, 0.6),
    (EdgeKind::WritesState, 0.7),
    (EdgeKind::SpatialCall, 0.5),
    (EdgeKind::StateAffinity, 0.4),
    (EdgeKind::InjectsScript, 0.7),
];

/// Build the weighted petgraph from the GraphStore.
fn build_weighted_graph(
    store: &GraphStore,
    project_id: &str,
) -> anyhow::Result<(DiGraph<String, f32>, HashMap<String, NodeIndex>)> {
    let mut graph = DiGraph::<String, f32>::new();
    let mut node_to_idx = HashMap::new();

    let node_ids = store.list_node_ids(project_id, None)?;
    for id in node_ids {
        let idx = graph.add_node(id.clone());
        node_to_idx.insert(id, idx);
    }

    for (kind, weight_multiplier) in EDGE_CONFIGS {
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

    Ok((graph, node_to_idx))
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

    let (graph, _node_to_idx) = build_weighted_graph(store, project_id)?;

    // Compute PageRank
    let scores = pagerank(&graph, 0.85, 20);

    let mut pagerank_map = HashMap::new();
    for (idx, score) in scores {
        if let Some(id) = graph.node_weight(idx) {
            pagerank_map.insert(id.clone(), score);
        }
    }

    // Store in cache
    let _ = store.set_cached_centrality(project_id, generation, &pagerank_map);

    Ok(CentralityMetrics {
        pagerank: pagerank_map,
    })
}

/// Compute multi-algorithm centrality: PageRank + degree + betweenness.
///
/// Betweenness uses random-sample approximation (Brandes k-pivot) to stay O(k * (V + E))
/// rather than exact O(V^3). Suitable for graphs with thousands of nodes.
pub fn compute_multi_centrality(
    store: &GraphStore,
    project_id: &str,
    generation: u64,
    betweenness_samples: usize,
) -> anyhow::Result<MultiCentrality> {
    let (graph, node_to_idx) = build_weighted_graph(store, project_id)?;

    // PageRank (with cache)
    let pr_map = if let Ok(Some(cached)) = store.get_cached_centrality(project_id, generation) {
        cached
    } else {
        let scores = pagerank(&graph, 0.85, 20);
        let mut pr = HashMap::new();
        for (idx, score) in &scores {
            if let Some(id) = graph.node_weight(*idx) {
                pr.insert(id.clone(), *score);
            }
        }
        let _ = store.set_cached_centrality(project_id, generation, &pr);
        pr
    };

    // Degree centrality
    let mut in_degree = HashMap::new();
    let mut out_degree = HashMap::new();
    for (node_id, &idx) in &node_to_idx {
        let in_d = graph
            .neighbors_directed(idx, petgraph::Direction::Incoming)
            .count() as u32;
        let out_d = graph
            .neighbors_directed(idx, petgraph::Direction::Outgoing)
            .count() as u32;
        in_degree.insert(node_id.clone(), in_d);
        out_degree.insert(node_id.clone(), out_d);
    }

    // Approximate betweenness centrality (Brandes k-pivot algorithm)
    let betweenness_idx = approximate_betweenness(&graph, betweenness_samples);
    let mut betweenness = HashMap::new();
    for (idx, score) in betweenness_idx {
        if let Some(id) = graph.node_weight(idx) {
            betweenness.insert(id.clone(), score);
        }
    }

    Ok(MultiCentrality {
        pagerank: pr_map,
        in_degree,
        out_degree,
        betweenness,
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

/// Approximate betweenness centrality using Brandes' algorithm with k random pivot nodes.
///
/// For each pivot, runs a BFS from that node and accumulates dependency scores.
/// The scores are scaled by (n / k) to approximate the full computation.
/// Complexity: O(k * (V + E)), where k = betweenness_samples.
fn approximate_betweenness(graph: &DiGraph<String, f32>, k: usize) -> HashMap<NodeIndex, f32> {
    let n = graph.node_count();
    if n == 0 {
        return HashMap::new();
    }

    let mut betweenness: HashMap<NodeIndex, f32> = HashMap::new();
    for node in graph.node_indices() {
        betweenness.insert(node, 0.0);
    }

    // Select k pivot nodes deterministically (evenly spaced by index).
    let pivots: Vec<NodeIndex> = {
        let all_nodes: Vec<NodeIndex> = graph.node_indices().collect();
        if k >= all_nodes.len() {
            all_nodes
        } else {
            let step = all_nodes.len() as f64 / k as f64;
            (0..k)
                .map(|i| all_nodes[(i as f64 * step) as usize])
                .collect()
        }
    };

    let actual_k = pivots.len();

    for &s in &pivots {
        // Brandes single-source BFS
        let mut stack = Vec::new();
        let mut predecessors: HashMap<NodeIndex, Vec<NodeIndex>> = HashMap::new();
        let mut sigma: HashMap<NodeIndex, f64> = HashMap::new();
        let mut dist: HashMap<NodeIndex, i64> = HashMap::new();

        for node in graph.node_indices() {
            sigma.insert(node, 0.0);
            dist.insert(node, -1);
        }
        sigma.insert(s, 1.0);
        dist.insert(s, 0);

        let mut queue = std::collections::VecDeque::new();
        queue.push_back(s);

        while let Some(v) = queue.pop_front() {
            stack.push(v);
            let d_v = dist[&v];

            for w in graph.neighbors(v) {
                // First visit?
                if dist[&w] < 0 {
                    queue.push_back(w);
                    dist.insert(w, d_v + 1);
                }
                // Shortest path via v?
                if dist[&w] == d_v + 1 {
                    *sigma.entry(w).or_insert(0.0) += sigma[&v];
                    predecessors.entry(w).or_default().push(v);
                }
            }
        }

        // Accumulate dependencies
        let mut delta: HashMap<NodeIndex, f64> = HashMap::new();
        for node in graph.node_indices() {
            delta.insert(node, 0.0);
        }

        while let Some(w) = stack.pop() {
            if let Some(preds) = predecessors.get(&w) {
                let sigma_w = sigma.get(&w).copied().unwrap_or(0.0);
                if sigma_w > 0.0 {
                    for &v in preds {
                        let sigma_v = sigma.get(&v).copied().unwrap_or(0.0);
                        let d_w = delta.get(&w).copied().unwrap_or(0.0);
                        *delta.entry(v).or_insert(0.0) += (sigma_v / sigma_w) * (1.0 + d_w);
                    }
                }
            }
            if w != s {
                let d = delta.get(&w).copied().unwrap_or(0.0);
                *betweenness.entry(w).or_insert(0.0) += d as f32;
            }
        }
    }

    // Scale by n/k to approximate full computation
    if actual_k > 0 && actual_k < n {
        let scale = n as f32 / actual_k as f32;
        for val in betweenness.values_mut() {
            *val *= scale;
        }
    }

    betweenness
}
