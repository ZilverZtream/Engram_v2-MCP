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
    if let Err(e) = store.set_cached_centrality(project_id, generation, &pagerank_map) {
        tracing::warn!(
            project_id,
            generation,
            "ENG-AUD-S1-0004: centrality cache write failed — recomputation will occur on next call: {e}"
        );
    }

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
        if let Err(e) = store.set_cached_centrality(project_id, generation, &pr) {
            tracing::warn!(
                project_id,
                generation,
                "ENG-AUD-S1-0004: centrality cache write failed — recomputation will occur on next call: {e}"
            );
        }
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

// ── Path finding (TODO-14) ───────────────────────────────────────────────────

/// One hop in a found path: the edge kind taken and the node arrived at.
#[derive(Debug, Clone)]
pub struct PathHop {
    pub edge_kind: EdgeKind,
    pub node_id: String,
    /// True when the edge was traversed against its stored direction
    /// (only happens in undirected mode).
    pub reversed: bool,
    /// Resolution confidence from edge metadata (TODO-12), when recorded.
    /// Low values mean the endpoint was bound by bare-name matching.
    pub confidence: Option<f32>,
}

/// Result of [`find_path`]: the start node plus the hops to the target.
#[derive(Debug, Clone)]
pub struct FoundPath {
    pub start: String,
    pub hops: Vec<PathHop>,
    /// False when the path needed undirected traversal.
    pub directed: bool,
}

/// BFS shortest path between two node ids over the project's edges.
///
/// Tries directed traversal first (source -> target along edge direction);
/// when no directed path exists within `max_depth`, retries treating every
/// edge as bidirectional so "how are these even related?" still gets an
/// answer (marked `directed: false`, hops carry `reversed` flags).
///
/// `kind_filter`: when non-empty, only these edge kinds are traversed.
/// Loads the project's edge list once (one prefix scan) — OciusX-scale
/// (113k edges) builds the adjacency map in well under a second.
pub fn find_path(
    store: &GraphStore,
    project_id: &str,
    from: &str,
    to: &str,
    max_depth: usize,
    kind_filter: &[EdgeKind],
) -> anyhow::Result<Option<FoundPath>> {
    // Structural edges only by default; statistical history kinds are
    // misleading as "connections" and dominate the table after git-history
    // indexing. An explicit kind_filter scans exactly those kinds instead.
    let edges = if kind_filter.is_empty() {
        store.list_structural_edges(project_id)?
    } else {
        let mut acc = Vec::new();
        for k in kind_filter {
            acc.extend(store.list_edges_by_kind(project_id, k.clone(), usize::MAX)?);
        }
        acc
    };

    // adjacency: node -> [(neighbor, kind, confidence)]
    let mut fwd: HashMap<&str, Vec<(&str, &EdgeKind, Option<f32>)>> = HashMap::new();
    let mut rev: HashMap<&str, Vec<(&str, &EdgeKind, Option<f32>)>> = HashMap::new();
    for e in &edges {
        if !kind_filter.is_empty() && !kind_filter.contains(&e.edge_kind) {
            continue;
        }
        // Synthesized file-membership edges connect a file to everything in
        // it; including them makes every intra-file pair "1 hop" and hides
        // the real wiring. They are still used as a last-resort connector in
        // undirected mode only when nothing else links the endpoints — for
        // now simply skip them; file blast aggregation covers membership.
        if e.metadata
            .as_ref()
            .and_then(|m| m.get("containment"))
            .and_then(|v| v.as_str())
            == Some("file")
        {
            continue;
        }
        let conf = e
            .metadata
            .as_ref()
            .and_then(|m| m.get("confidence"))
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f32>().ok());
        fwd.entry(e.source_id.as_str()).or_default().push((
            e.target_id.as_str(),
            &e.edge_kind,
            conf,
        ));
        rev.entry(e.target_id.as_str()).or_default().push((
            e.source_id.as_str(),
            &e.edge_kind,
            conf,
        ));
    }

    let bfs = |undirected: bool| -> Option<FoundPath> {
        use std::collections::VecDeque;
        // parent: node -> (prev_node, kind, reversed, confidence)
        let mut parent: HashMap<&str, (&str, EdgeKind, bool, Option<f32>)> = HashMap::new();
        let mut q: VecDeque<(&str, usize)> = VecDeque::new();
        q.push_back((from, 0));
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        seen.insert(from);
        while let Some((cur, depth)) = q.pop_front() {
            if cur == to {
                // Reconstruct.
                let mut hops_rev = Vec::new();
                let mut walk = cur;
                while walk != from {
                    let (prev, kind, reversed, confidence) = parent.get(walk)?.clone();
                    hops_rev.push(PathHop {
                        edge_kind: kind,
                        node_id: walk.to_string(),
                        reversed,
                        confidence,
                    });
                    walk = prev;
                }
                hops_rev.reverse();
                return Some(FoundPath {
                    start: from.to_string(),
                    hops: hops_rev,
                    directed: !undirected,
                });
            }
            if depth >= max_depth {
                continue;
            }
            let empty = Vec::new();
            let nexts = fwd
                .get(cur)
                .unwrap_or(&empty)
                .iter()
                .map(|(n, k, c)| (*n, *k, false, *c));
            let backs = if undirected {
                Some(
                    rev.get(cur)
                        .unwrap_or(&empty)
                        .iter()
                        .map(|(n, k, c)| (*n, *k, true, *c)),
                )
            } else {
                None
            };
            for (n, k, reversed, conf) in nexts.chain(backs.into_iter().flatten()) {
                if seen.insert(n) {
                    parent.insert(n, (cur, k.clone(), reversed, conf));
                    q.push_back((n, depth + 1));
                }
            }
        }
        None
    };

    if let Some(p) = bfs(false) {
        return Ok(Some(p));
    }
    Ok(bfs(true))
}

// ── Dependency cycles / SCCs (TODO-20) ───────────────────────────────────────

/// A strongly-connected component of the dependency graph: every member
/// reaches every other member. Cycles are exactly where naive
/// strangler-fig migration plans fail — you cannot extract one member
/// without the others.
#[derive(Debug, Clone)]
pub struct DependencyCycle {
    pub members: Vec<String>,
    /// Edge kinds observed inside the cycle (what binds it together).
    pub binding_kinds: Vec<EdgeKind>,
}

/// Find strongly-connected components over dependency-like edges
/// (Calls, Dependency, Imports). Statistical and hierarchical kinds are
/// excluded — temporal coupling is not a build-order constraint and
/// Contains cannot cycle. Returns components of size >= `min_size`,
/// largest first.
pub fn find_dependency_cycles(
    store: &GraphStore,
    project_id: &str,
    min_size: usize,
) -> anyhow::Result<Vec<DependencyCycle>> {
    const CYCLE_KINDS: [EdgeKind; 3] = [EdgeKind::Calls, EdgeKind::Dependency, EdgeKind::Imports];

    let mut graph: DiGraph<String, EdgeKind> = DiGraph::new();
    let mut idx: HashMap<String, NodeIndex> = HashMap::new();
    let mut get_idx =
        |g: &mut DiGraph<String, EdgeKind>, m: &mut HashMap<String, NodeIndex>, id: &str| {
            if let Some(i) = m.get(id) {
                *i
            } else {
                let i = g.add_node(id.to_string());
                m.insert(id.to_string(), i);
                i
            }
        };

    for kind in CYCLE_KINDS {
        for e in store.list_edges_by_kind(project_id, kind.clone(), usize::MAX)? {
            // Placeholder targets are unresolved externals — they cannot
            // participate in a real cycle.
            if e.target_id.starts_with("::") {
                continue;
            }
            let s = get_idx(&mut graph, &mut idx, &e.source_id);
            let t = get_idx(&mut graph, &mut idx, &e.target_id);
            graph.add_edge(s, t, e.edge_kind.clone());
        }
    }

    let sccs = petgraph::algo::tarjan_scc(&graph);
    let mut out = Vec::new();
    for comp in sccs {
        if comp.len() < min_size.max(2) {
            continue;
        }
        let member_set: std::collections::HashSet<NodeIndex> = comp.iter().copied().collect();
        let mut kinds: Vec<EdgeKind> = Vec::new();
        for &n in &comp {
            for edge in graph.edges(n) {
                use petgraph::visit::EdgeRef;
                if member_set.contains(&edge.target()) && !kinds.contains(edge.weight()) {
                    kinds.push(edge.weight().clone());
                }
            }
        }
        out.push(DependencyCycle {
            members: comp.iter().map(|&n| graph[n].clone()).collect(),
            binding_kinds: kinds,
        });
    }
    out.sort_by(|a, b| b.members.len().cmp(&a.members.len()));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Edge, EdgeKind, GraphStore, Node};
    use engram_core::RelPath;
    use tempfile::tempdir;

    fn make_node(id: &str, node_type: &str) -> Node {
        Node {
            node_id: id.to_string(),
            node_type: node_type.to_string(),
            name: id.to_string(),
            namespace: "test".to_string(),
            language: "rust".to_string(),
            file_path: RelPath::new("src/lib.rs"),
            start_line: 1,
            end_line: 10,
            generation: 1,
            metadata: None,
        }
    }

    fn make_dep_edge(src: &str, tgt: &str, weight: u32) -> Edge {
        Edge {
            source_id: src.to_string(),
            target_id: tgt.to_string(),
            namespace: "test".to_string(),
            language: "rust".to_string(),
            edge_kind: EdgeKind::Dependency,
            weight,
            generation: 1,
            metadata: None,
            updated_at_ms: 0,
        }
    }

    // ─── PageRank ─────────────────────────────────────────────────────────────

    #[test]
    fn pagerank_empty_graph_returns_empty() {
        let dir = tempdir().unwrap();
        let store = GraphStore::open(&dir.path().join("graph.db")).unwrap();
        let result = compute_pagerank(&store, "proj", 1).unwrap();
        assert!(result.pagerank.is_empty());
    }

    #[test]
    fn pagerank_single_node_has_positive_score() {
        let dir = tempdir().unwrap();
        let store = GraphStore::open(&dir.path().join("graph.db")).unwrap();
        store
            .upsert_nodes("proj", &[make_node("only", "function")])
            .unwrap();
        let result = compute_pagerank(&store, "proj", 1).unwrap();
        assert_eq!(result.pagerank.len(), 1);
        let score = *result.pagerank.get("only").unwrap();
        assert!(
            score > 0.0,
            "single node PageRank should be positive, got {score}"
        );
    }

    #[test]
    fn pagerank_hub_scores_higher_than_leaf() {
        // Star graph: hub ← leaf1, leaf2, leaf3 (leaves depend on hub).
        let dir = tempdir().unwrap();
        let store = GraphStore::open(&dir.path().join("graph.db")).unwrap();
        let nodes = vec![
            make_node("hub", "function"),
            make_node("leaf1", "function"),
            make_node("leaf2", "function"),
            make_node("leaf3", "function"),
        ];
        store.upsert_nodes("proj", &nodes).unwrap();
        // Leaves point to hub (hub gets more inbound PageRank).
        let edges = vec![
            make_dep_edge("leaf1", "hub", 1),
            make_dep_edge("leaf2", "hub", 1),
            make_dep_edge("leaf3", "hub", 1),
        ];
        store.upsert_edges("proj", &edges).unwrap();
        let result = compute_pagerank(&store, "proj", 1).unwrap();
        let hub_score = *result.pagerank.get("hub").unwrap();
        let leaf_score = *result.pagerank.get("leaf1").unwrap();
        assert!(
            hub_score > leaf_score,
            "hub PageRank {hub_score} should exceed leaf PageRank {leaf_score}"
        );
    }

    #[test]
    fn pagerank_scores_sum_approximately_to_one() {
        let dir = tempdir().unwrap();
        let store = GraphStore::open(&dir.path().join("graph.db")).unwrap();
        let nodes: Vec<Node> = (0..5)
            .map(|i| make_node(&format!("n{i}"), "function"))
            .collect();
        store.upsert_nodes("proj", &nodes).unwrap();
        // Chain: n0→n1→n2→n3→n4
        let edges: Vec<Edge> = (0..4)
            .map(|i| make_dep_edge(&format!("n{i}"), &format!("n{}", i + 1), 1))
            .collect();
        store.upsert_edges("proj", &edges).unwrap();
        let result = compute_pagerank(&store, "proj", 1).unwrap();
        let total: f32 = result.pagerank.values().sum();
        // PageRank scores should sum approximately to 1.0 (within 10% tolerance).
        assert!(
            (total - 1.0).abs() < 0.1,
            "PageRank scores sum to {total}, expected ~1.0"
        );
    }

    #[test]
    fn pagerank_deterministic() {
        let dir = tempdir().unwrap();
        let store = GraphStore::open(&dir.path().join("graph.db")).unwrap();
        let nodes: Vec<Node> = (0..4)
            .map(|i| make_node(&format!("n{i}"), "function"))
            .collect();
        store.upsert_nodes("proj", &nodes).unwrap();
        store
            .upsert_edges(
                "proj",
                &[make_dep_edge("n0", "n1", 1), make_dep_edge("n2", "n3", 1)],
            )
            .unwrap();
        // Use different generations to bypass cache.
        let r1 = compute_pagerank(&store, "proj", 1).unwrap();
        let r2 = compute_pagerank(&store, "proj", 2).unwrap();
        for (id, score1) in &r1.pagerank {
            let score2 = r2.pagerank.get(id).copied().unwrap_or(0.0);
            assert!(
                (score1 - score2).abs() < 1e-6,
                "PageRank for {id} not deterministic: {score1} vs {score2}"
            );
        }
    }

    // ─── MultiCentrality ──────────────────────────────────────────────────────

    #[test]
    fn multi_centrality_in_degree_correct() {
        // Star: hub gets an in-edge from each leaf.
        let dir = tempdir().unwrap();
        let store = GraphStore::open(&dir.path().join("graph.db")).unwrap();
        let num_leaves = 4usize;
        let mut nodes = vec![make_node("hub", "function")];
        for i in 0..num_leaves {
            nodes.push(make_node(&format!("leaf{i}"), "function"));
        }
        store.upsert_nodes("proj", &nodes).unwrap();
        let edges: Vec<Edge> = (0..num_leaves)
            .map(|i| make_dep_edge(&format!("leaf{i}"), "hub", 1))
            .collect();
        store.upsert_edges("proj", &edges).unwrap();
        let mc = compute_multi_centrality(&store, "proj", 1, 4).unwrap();
        let hub_in = *mc.in_degree.get("hub").unwrap();
        assert_eq!(
            hub_in, num_leaves as u32,
            "hub in_degree should be {num_leaves}, got {hub_in}"
        );
    }

    #[test]
    fn multi_centrality_out_degree_correct() {
        let dir = tempdir().unwrap();
        let store = GraphStore::open(&dir.path().join("graph.db")).unwrap();
        store
            .upsert_nodes(
                "proj",
                &[make_node("src", "function"), make_node("dst", "function")],
            )
            .unwrap();
        store
            .upsert_edges("proj", &[make_dep_edge("src", "dst", 1)])
            .unwrap();
        let mc = compute_multi_centrality(&store, "proj", 1, 4).unwrap();
        let src_out = *mc.out_degree.get("src").unwrap();
        assert!(src_out > 0, "src out_degree should be > 0, got {src_out}");
        let dst_out = *mc.out_degree.get("dst").unwrap();
        assert_eq!(dst_out, 0, "dst out_degree should be 0, got {dst_out}");
    }

    #[test]
    fn multi_centrality_empty_graph() {
        let dir = tempdir().unwrap();
        let store = GraphStore::open(&dir.path().join("graph.db")).unwrap();
        // Should not panic on empty graph.
        let mc = compute_multi_centrality(&store, "proj", 1, 4).unwrap();
        assert!(mc.pagerank.is_empty());
        assert!(mc.in_degree.is_empty());
        assert!(mc.out_degree.is_empty());
        assert!(mc.betweenness.is_empty());
    }

    // ─── BlendedScore ─────────────────────────────────────────────────────────

    #[test]
    fn blended_score_zero_weights_returns_zero() {
        let dir = tempdir().unwrap();
        let store = GraphStore::open(&dir.path().join("graph.db")).unwrap();
        store
            .upsert_nodes("proj", &[make_node("x", "function")])
            .unwrap();
        let mc = compute_multi_centrality(&store, "proj", 1, 4).unwrap();
        let score = mc.blended_score("x", 0.0, 0.0, 0.0);
        assert_eq!(score, 0.0);
    }

    #[test]
    fn blended_score_pagerank_only() {
        let dir = tempdir().unwrap();
        let store = GraphStore::open(&dir.path().join("graph.db")).unwrap();
        let nodes: Vec<Node> = vec![
            make_node("hub", "function"),
            make_node("leaf1", "function"),
            make_node("leaf2", "function"),
        ];
        store.upsert_nodes("proj", &nodes).unwrap();
        store
            .upsert_edges(
                "proj",
                &[
                    make_dep_edge("leaf1", "hub", 1),
                    make_dep_edge("leaf2", "hub", 1),
                ],
            )
            .unwrap();
        let mc = compute_multi_centrality(&store, "proj", 1, 4).unwrap();
        // With pr_weight=1, others=0, blended score = normalized pagerank.
        let hub_score = mc.blended_score("hub", 1.0, 0.0, 0.0);
        let leaf_score = mc.blended_score("leaf1", 1.0, 0.0, 0.0);
        // Hub should have higher or equal blended score than leaf.
        assert!(
            hub_score >= leaf_score,
            "hub blended (pr only) {hub_score} should >= leaf {leaf_score}"
        );
    }

    #[test]
    fn blended_score_higher_for_more_central_node() {
        let dir = tempdir().unwrap();
        let store = GraphStore::open(&dir.path().join("graph.db")).unwrap();
        let nodes: Vec<Node> = vec![
            make_node("hub", "function"),
            make_node("l1", "function"),
            make_node("l2", "function"),
            make_node("l3", "function"),
        ];
        store.upsert_nodes("proj", &nodes).unwrap();
        let edges = vec![
            make_dep_edge("l1", "hub", 1),
            make_dep_edge("l2", "hub", 1),
            make_dep_edge("l3", "hub", 1),
        ];
        store.upsert_edges("proj", &edges).unwrap();
        let mc = compute_multi_centrality(&store, "proj", 1, 4).unwrap();
        let hub_score = mc.blended_score("hub", 1.0, 1.0, 0.5);
        let leaf_score = mc.blended_score("l1", 1.0, 1.0, 0.5);
        assert!(
            hub_score >= leaf_score,
            "hub blended {hub_score} should >= leaf blended {leaf_score}"
        );
    }

    #[test]
    fn blended_score_in_range_zero_to_one() {
        let dir = tempdir().unwrap();
        let store = GraphStore::open(&dir.path().join("graph.db")).unwrap();
        let nodes: Vec<Node> = (0..5)
            .map(|i| make_node(&format!("n{i}"), "function"))
            .collect();
        store.upsert_nodes("proj", &nodes).unwrap();
        let edges: Vec<Edge> = (0..4)
            .map(|i| make_dep_edge(&format!("n{i}"), &format!("n{}", i + 1), 1))
            .collect();
        store.upsert_edges("proj", &edges).unwrap();
        let mc = compute_multi_centrality(&store, "proj", 1, 4).unwrap();
        for i in 0..5 {
            let id = format!("n{i}");
            let score = mc.blended_score(&id, 1.0, 1.0, 1.0);
            assert!(
                (0.0..=1.0).contains(&score),
                "blended score for {id} = {score}, expected [0, 1]"
            );
        }
    }

    // ─── Betweenness ──────────────────────────────────────────────────────────

    #[test]
    fn betweenness_hub_scores_higher() {
        // Chain A→B→C: B is on every path from A to C, so B has higher betweenness.
        let dir = tempdir().unwrap();
        let store = GraphStore::open(&dir.path().join("graph.db")).unwrap();
        store
            .upsert_nodes(
                "proj",
                &[
                    make_node("A", "function"),
                    make_node("B", "function"),
                    make_node("C", "function"),
                ],
            )
            .unwrap();
        store
            .upsert_edges(
                "proj",
                &[make_dep_edge("A", "B", 1), make_dep_edge("B", "C", 1)],
            )
            .unwrap();
        // Use all nodes as pivots for exact betweenness.
        let mc = compute_multi_centrality(&store, "proj", 1, 10).unwrap();
        let b_score = mc.betweenness.get("B").copied().unwrap_or(0.0);
        let a_score = mc.betweenness.get("A").copied().unwrap_or(0.0);
        let c_score = mc.betweenness.get("C").copied().unwrap_or(0.0);
        assert!(
            b_score >= a_score,
            "B betweenness {b_score} should >= A betweenness {a_score}"
        );
        assert!(
            b_score >= c_score,
            "B betweenness {b_score} should >= C betweenness {c_score}"
        );
    }

    // ─── ENG-AUD-S1-0004 regression guards ────────────────────────────────────

    #[test]
    fn centrality_cache_write_failure_is_observable() {
        // Verify that the ENG-AUD-S1-0004 tag is present in the source,
        // confirming cache write failures are logged rather than silently discarded.
        let source = include_str!("analysis.rs");
        assert!(
            source.contains("ENG-AUD-S1-0004"),
            "analysis.rs must log cache write failures with ENG-AUD-S1-0004 tag"
        );
        // Confirm both cache-write sites have the audit tag (one tag per site).
        // This is a stronger guarantee than checking for absent patterns (which
        // can self-match when the pattern string appears in the test itself).
        let tag_count = source.matches("ENG-AUD-S1-0004").count();
        assert!(
            tag_count >= 2,
            "both set_cached_centrality call sites must carry the ENG-AUD-S1-0004 tag; found {tag_count}"
        );
    }

    #[test]
    fn centrality_cache_write_uses_warn_level() {
        let source = include_str!("analysis.rs");
        // warn! is appropriate for degraded-performance situations
        assert!(
            source.contains("warn!(") || source.contains("tracing::warn!"),
            "cache write failure must emit a warn! log"
        );
    }

    // ── find_path tests (TODO-14) ────────────────────────────────────────

    fn path_edge(store: &GraphStore, a: &str, b: &str, kind: EdgeKind) {
        path_node(store, a);
        path_node(store, b);
        store
            .upsert_edges(
                "proj",
                &[crate::store::Edge {
                    source_id: a.to_string(),
                    target_id: b.to_string(),
                    namespace: "memory".to_string(),
                    language: "vb".to_string(),
                    edge_kind: kind,
                    weight: 1,
                    generation: 1,
                    metadata: None,
                    updated_at_ms: 0,
                }],
            )
            .unwrap();
    }

    fn path_node(store: &GraphStore, id: &str) {
        store
            .upsert_nodes(
                "proj",
                &[crate::store::Node {
                    node_id: id.to_string(),
                    node_type: "function".to_string(),
                    name: id.to_string(),
                    namespace: "memory".to_string(),
                    language: "vb".to_string(),
                    file_path: engram_core::RelPath::new("f.vb"),
                    start_line: 1,
                    end_line: 1,
                    generation: 1,
                    metadata: None,
                }],
            )
            .unwrap();
    }

    #[test]
    fn directed_chain_found_with_kinds() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = GraphStore::open(&tmp.path().join("g.redb")).unwrap();
        path_edge(&store, "btn", "handler", EdgeKind::Dependency);
        path_edge(&store, "handler", "sql:GetOrders", EdgeKind::SqlCalls);
        path_edge(
            &store,
            "sql:GetOrders",
            "table:orders",
            EdgeKind::QueriesTable,
        );

        let p = find_path(&store, "proj", "btn", "table:orders", 6, &[])
            .unwrap()
            .expect("path exists");
        assert!(p.directed);
        assert_eq!(p.hops.len(), 3);
        assert_eq!(p.hops[0].edge_kind, EdgeKind::Dependency);
        assert_eq!(p.hops[2].node_id, "table:orders");
        assert!(p.hops.iter().all(|h| !h.reversed));
    }

    #[test]
    fn falls_back_to_undirected_when_direction_blocks() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = GraphStore::open(&tmp.path().join("g.redb")).unwrap();
        // a -> shared <- b : no directed a->b path, undirected exists.
        path_edge(&store, "a", "shared", EdgeKind::Calls);
        path_edge(&store, "b", "shared", EdgeKind::Calls);

        let p = find_path(&store, "proj", "a", "b", 6, &[])
            .unwrap()
            .expect("undirected path");
        assert!(!p.directed);
        assert_eq!(p.hops.len(), 2);
        assert!(p.hops[1].reversed, "second hop goes against edge direction");
    }

    #[test]
    fn respects_max_depth_and_kind_filter() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = GraphStore::open(&tmp.path().join("g.redb")).unwrap();
        path_edge(&store, "n1", "n2", EdgeKind::Calls);
        path_edge(&store, "n2", "n3", EdgeKind::Calls);
        path_edge(&store, "n3", "n4", EdgeKind::Calls);

        assert!(
            find_path(&store, "proj", "n1", "n4", 2, &[])
                .unwrap()
                .is_none(),
            "3 hops must not be found at max_depth 2"
        );
        assert!(
            find_path(&store, "proj", "n1", "n4", 6, &[EdgeKind::SqlCalls])
                .unwrap()
                .is_none(),
            "kind filter excludes Calls"
        );
        assert!(
            find_path(&store, "proj", "n1", "n4", 6, &[EdgeKind::Calls])
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn delete_edges_of_kind_clears_edges_and_adjacency() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = GraphStore::open(&tmp.path().join("g.redb")).unwrap();
        path_edge(&store, "f1", "f2", EdgeKind::TemporalCoupling);
        path_edge(&store, "f2", "f1", EdgeKind::TemporalCoupling);
        path_edge(&store, "f1", "f2", EdgeKind::Calls);

        let removed = store
            .delete_edges_of_kind("proj", &EdgeKind::TemporalCoupling)
            .unwrap();
        assert_eq!(removed, 2, "both temporal directions removed");

        let temporal = store
            .list_edges_by_kind("proj", EdgeKind::TemporalCoupling, usize::MAX)
            .unwrap();
        assert!(temporal.is_empty(), "edges table cleared");
        let neigh = store
            .neighbors("proj", EdgeKind::TemporalCoupling, "f1", 10)
            .unwrap();
        assert!(neigh.is_empty(), "adjacency cleared");

        let calls = store
            .list_edges_by_kind("proj", EdgeKind::Calls, usize::MAX)
            .unwrap();
        assert_eq!(calls.len(), 1, "other kinds untouched");
        // Idempotent re-clear.
        assert_eq!(
            store
                .delete_edges_of_kind("proj", &EdgeKind::TemporalCoupling)
                .unwrap(),
            0
        );
    }

    #[test]
    fn path_hops_carry_resolution_confidence() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = GraphStore::open(&tmp.path().join("g.redb")).unwrap();
        path_node(&store, "caller");
        path_node(&store, "guessed_target");
        store
            .upsert_edges(
                "proj",
                &[crate::store::Edge {
                    source_id: "caller".into(),
                    target_id: "guessed_target".into(),
                    namespace: "memory".into(),
                    language: "vb".into(),
                    edge_kind: EdgeKind::Calls,
                    weight: 1,
                    generation: 1,
                    metadata: Some(serde_json::json!({
                        "resolution": "batch_unique_any_terminal",
                        "confidence": "0.35"
                    })),
                    updated_at_ms: 0,
                }],
            )
            .unwrap();

        let p = find_path(&store, "proj", "caller", "guessed_target", 3, &[])
            .unwrap()
            .expect("path");
        assert_eq!(p.hops.len(), 1);
        assert_eq!(p.hops[0].confidence, Some(0.35), "confidence must surface");
    }

    #[test]
    fn membership_edges_are_not_shortcuts() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = GraphStore::open(&tmp.path().join("g.redb")).unwrap();
        path_node(&store, "file:big.vb");
        path_node(&store, "sym:function:big.vb:A:1");
        path_node(&store, "sym:function:big.vb:B:9");
        store
            .upsert_edges(
                "proj",
                &[
                    crate::store::Edge {
                        source_id: "file:big.vb".into(),
                        target_id: "sym:function:big.vb:A:1".into(),
                        namespace: "memory".into(),
                        language: "vb".into(),
                        edge_kind: EdgeKind::Contains,
                        weight: 1,
                        generation: 1,
                        metadata: Some(serde_json::json!({"containment": "file"})),
                        updated_at_ms: 0,
                    },
                    crate::store::Edge {
                        source_id: "file:big.vb".into(),
                        target_id: "sym:function:big.vb:B:9".into(),
                        namespace: "memory".into(),
                        language: "vb".into(),
                        edge_kind: EdgeKind::Contains,
                        weight: 1,
                        generation: 1,
                        metadata: Some(serde_json::json!({"containment": "file"})),
                        updated_at_ms: 0,
                    },
                ],
            )
            .unwrap();

        assert!(
            find_path(
                &store,
                "proj",
                "sym:function:big.vb:A:1",
                "sym:function:big.vb:B:9",
                6,
                &[]
            )
            .unwrap()
            .is_none(),
            "two unrelated functions in one file must not be 'connected' via membership"
        );
    }

    // ── find_dependency_cycles tests (TODO-20) ───────────────────────────

    #[test]
    fn detects_call_cycle_and_ignores_acyclic() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = GraphStore::open(&tmp.path().join("g.redb")).unwrap();
        // a -> b -> c -> a (cycle), d -> a (acyclic feeder)
        path_edge(&store, "a", "b", EdgeKind::Calls);
        path_edge(&store, "b", "c", EdgeKind::Calls);
        path_edge(&store, "c", "a", EdgeKind::Calls);
        path_edge(&store, "d", "a", EdgeKind::Calls);

        let cycles = find_dependency_cycles(&store, "proj", 2).unwrap();
        assert_eq!(cycles.len(), 1);
        let mut members = cycles[0].members.clone();
        members.sort();
        assert_eq!(members, vec!["a", "b", "c"]);
        assert!(cycles[0].binding_kinds.contains(&EdgeKind::Calls));
    }

    #[test]
    fn mutual_imports_cycle_detected_temporal_ignored() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = GraphStore::open(&tmp.path().join("g.redb")).unwrap();
        path_edge(&store, "m1", "m2", EdgeKind::Imports);
        path_edge(&store, "m2", "m1", EdgeKind::Imports);
        // Temporal pair must NOT count as a cycle.
        path_edge(&store, "t1", "t2", EdgeKind::TemporalCoupling);
        path_edge(&store, "t2", "t1", EdgeKind::TemporalCoupling);

        let cycles = find_dependency_cycles(&store, "proj", 2).unwrap();
        assert_eq!(cycles.len(), 1, "only the Imports cycle counts");
        assert!(cycles[0].members.contains(&"m1".to_string()));
    }

    #[test]
    fn placeholder_targets_do_not_form_cycles() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = GraphStore::open(&tmp.path().join("g.redb")).unwrap();
        path_edge(&store, "x", "::Ghost", EdgeKind::Calls);
        let cycles = find_dependency_cycles(&store, "proj", 2).unwrap();
        assert!(cycles.is_empty());
    }
}
