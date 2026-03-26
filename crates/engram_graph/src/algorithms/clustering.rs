use crate::store::{EdgeKind, GraphStore};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone)]
pub struct Cluster {
    pub node_ids: Vec<String>,
    pub score: u64,
}

impl Cluster {
    pub fn fingerprint(&self) -> String {
        // Sort in-place on a clone for deterministic hashing.
        // Use with_capacity to hint the join buffer size.
        let mut sorted = self.node_ids.clone();
        sorted.sort_unstable();
        let estimated_len: usize = sorted.iter().map(|s| s.len() + 1).sum();
        let mut data = String::with_capacity(estimated_len);
        for (i, s) in sorted.iter().enumerate() {
            if i > 0 {
                data.push('\0');
            }
            data.push_str(s);
        }
        blake3::hash(data.as_bytes()).to_string()
    }
}

/// Robust cluster finder for "REM-style dreaming" using Label Propagation.
///
/// Label Propagation is a solid approximation for community detection (like Louvain/Leiden)
/// that works well on large sparse graphs. It is O(E) and naturally finds dense regions.
pub fn find_cooccurrence_clusters(
    store: &GraphStore,
    project_id: &str,
    min_edge_weight: u32,
    min_size: usize,
    max_clusters: usize,
) -> anyhow::Result<Vec<Cluster>> {
    let nodes = store.list_node_ids(project_id, Some("chunk"))?;
    if nodes.is_empty() {
        return Ok(Vec::new());
    }

    // 1. Initialize each node with a unique label
    let mut labels: HashMap<String, String> =
        nodes.iter().map(|id| (id.clone(), id.clone())).collect();

    // 2. Iterate label propagation (limited rounds for performance/convergence)
    let iterations = 5;
    for _ in 0..iterations {
        let mut changed = false;

        // We'll shuffle nodes or just iterate.
        // In a real implementation we'd shuffle to avoid bias.
        for node_id in &nodes {
            let neighbors = store.neighbors(project_id, EdgeKind::CoOccurrence, node_id, 128)?;
            if neighbors.is_empty() {
                continue;
            }

            // Count frequencies of neighbor labels, weighted by edge weight
            let mut label_weights: HashMap<String, u64> = HashMap::new();
            for (neigh_id, weight) in neighbors {
                if weight < min_edge_weight {
                    continue;
                }
                if let Some(l) = labels.get(&neigh_id) {
                    *label_weights.entry(l.clone()).or_insert(0) += weight as u64;
                }
            }

            // Find most frequent/heaviest label
            if let Some(best_label) = label_weights
                .into_iter()
                .max_by_key(|&(_, w)| w)
                .map(|(l, _)| l)
                && let Some(current) = labels.get(node_id)
                && *current != best_label
            {
                labels.insert(node_id.clone(), best_label);
                changed = true;
            }
        }

        if !changed {
            break;
        }
    }

    // 3. Group nodes by labels
    let mut community_map: HashMap<String, Vec<String>> = HashMap::new();
    for (node_id, label) in labels {
        community_map.entry(label).or_default().push(node_id);
    }

    // 4. Convert to Cluster objects and score.
    // Build a HashSet per cluster for O(1) membership checks during scoring
    // (was O(N) Vec::contains, quadratic for large clusters).
    let mut clusters = Vec::new();
    for members in community_map.into_values() {
        if members.len() < min_size {
            continue;
        }

        // Skip clusters that already have an insight connected.
        if store.cluster_has_insight(project_id, &members, 16)? {
            continue;
        }

        // Score = internal density or sum of weights.
        // HashSet lookup is O(1) vs O(N) for Vec::contains.
        let member_set: HashSet<&str> = members.iter().map(|s| s.as_str()).collect();
        let mut score: u64 = 0;
        for m in &members {
            if let Ok(neigh) = store.neighbors(project_id, EdgeKind::CoOccurrence, m, 128) {
                for (target, w) in neigh {
                    if member_set.contains(target.as_str()) {
                        score += w as u64;
                    }
                }
            }
        }

        clusters.push(Cluster {
            node_ids: members,
            score,
        });
    }

    clusters.sort_by(|a, b| b.score.cmp(&a.score));
    if clusters.len() > max_clusters {
        clusters.truncate(max_clusters);
    }
    Ok(clusters)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Edge, EdgeKind, GraphStore, Node};
    use engram_core::RelPath;
    use tempfile::tempdir;

    fn make_chunk_node(id: &str) -> Node {
        Node {
            node_id: id.to_string(),
            node_type: "chunk".to_string(),
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

    fn make_cooccurrence_edge(src: &str, tgt: &str, weight: u32) -> Edge {
        Edge {
            source_id: src.to_string(),
            target_id: tgt.to_string(),
            namespace: "test".to_string(),
            language: "rust".to_string(),
            edge_kind: EdgeKind::CoOccurrence,
            weight,
            generation: 1,
            metadata: None,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn empty_graph_returns_no_clusters() {
        let dir = tempdir().unwrap();
        let store = GraphStore::open(&dir.path().join("graph.db")).unwrap();
        let result = find_cooccurrence_clusters(&store, "proj", 1, 2, 100).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn disconnected_nodes_no_clusters_below_min_size() {
        let dir = tempdir().unwrap();
        let store = GraphStore::open(&dir.path().join("graph.db")).unwrap();
        // Three isolated nodes — no edges, so label propagation leaves each as its own community.
        // With min_size=2, all single-node communities are filtered out.
        store
            .upsert_nodes("proj", &[make_chunk_node("a"), make_chunk_node("b"), make_chunk_node("c")])
            .unwrap();
        let result = find_cooccurrence_clusters(&store, "proj", 1, 2, 100).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn two_connected_nodes_form_cluster() {
        let dir = tempdir().unwrap();
        let store = GraphStore::open(&dir.path().join("graph.db")).unwrap();
        store
            .upsert_nodes("proj", &[make_chunk_node("a"), make_chunk_node("b")])
            .unwrap();
        // Undirected CoOccurrence: insert both directions so each node sees the other as a neighbor.
        store
            .upsert_edges(
                "proj",
                &[
                    make_cooccurrence_edge("a", "b", 5),
                    make_cooccurrence_edge("b", "a", 5),
                ],
            )
            .unwrap();
        let result = find_cooccurrence_clusters(&store, "proj", 1, 2, 100).unwrap();
        assert_eq!(result.len(), 1);
        let cluster = &result[0];
        assert_eq!(cluster.node_ids.len(), 2);
        assert!(cluster.node_ids.contains(&"a".to_string()));
        assert!(cluster.node_ids.contains(&"b".to_string()));
    }

    #[test]
    fn cluster_fingerprint_stable() {
        let c = Cluster {
            node_ids: vec!["node_a".to_string(), "node_b".to_string()],
            score: 10,
        };
        let fp1 = c.fingerprint();
        let fp2 = c.fingerprint();
        assert_eq!(fp1, fp2);
    }

    #[test]
    fn cluster_fingerprint_order_independent() {
        let c1 = Cluster {
            node_ids: vec!["a".to_string(), "b".to_string()],
            score: 0,
        };
        let c2 = Cluster {
            node_ids: vec!["b".to_string(), "a".to_string()],
            score: 0,
        };
        assert_eq!(c1.fingerprint(), c2.fingerprint());
    }

    #[test]
    fn cluster_fingerprint_differs_for_different_sets() {
        let c1 = Cluster {
            node_ids: vec!["a".to_string(), "b".to_string()],
            score: 0,
        };
        let c2 = Cluster {
            node_ids: vec!["a".to_string(), "c".to_string()],
            score: 0,
        };
        assert_ne!(c1.fingerprint(), c2.fingerprint());
    }

    #[test]
    fn min_size_filters_small_clusters() {
        let dir = tempdir().unwrap();
        let store = GraphStore::open(&dir.path().join("graph.db")).unwrap();
        // Two nodes connected — would form a cluster of size 2.
        store
            .upsert_nodes("proj", &[make_chunk_node("a"), make_chunk_node("b")])
            .unwrap();
        store
            .upsert_edges(
                "proj",
                &[
                    make_cooccurrence_edge("a", "b", 5),
                    make_cooccurrence_edge("b", "a", 5),
                ],
            )
            .unwrap();
        // min_size=3 should filter the 2-node cluster out.
        let result = find_cooccurrence_clusters(&store, "proj", 1, 3, 100).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn min_edge_weight_filters_weak_connections() {
        let dir = tempdir().unwrap();
        let store = GraphStore::open(&dir.path().join("graph.db")).unwrap();
        store
            .upsert_nodes("proj", &[make_chunk_node("a"), make_chunk_node("b")])
            .unwrap();
        // Edge weight=1, min_edge_weight=5 — the edge is skipped, nodes stay isolated.
        store
            .upsert_edges(
                "proj",
                &[
                    make_cooccurrence_edge("a", "b", 1),
                    make_cooccurrence_edge("b", "a", 1),
                ],
            )
            .unwrap();
        let result = find_cooccurrence_clusters(&store, "proj", 5, 2, 100).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn max_clusters_truncates() {
        let dir = tempdir().unwrap();
        let store = GraphStore::open(&dir.path().join("graph.db")).unwrap();
        // Build 5 isolated pairs, each forming a 2-node cluster.
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        for i in 0..5u32 {
            let a = format!("a{i}");
            let b = format!("b{i}");
            nodes.push(make_chunk_node(&a));
            nodes.push(make_chunk_node(&b));
            edges.push(make_cooccurrence_edge(&a, &b, 10));
            edges.push(make_cooccurrence_edge(&b, &a, 10));
        }
        store.upsert_nodes("proj", &nodes).unwrap();
        store.upsert_edges("proj", &edges).unwrap();
        let result = find_cooccurrence_clusters(&store, "proj", 1, 2, 3).unwrap();
        assert!(result.len() <= 3);
    }

    #[test]
    fn clusters_returned_by_score_descending() {
        let dir = tempdir().unwrap();
        let store = GraphStore::open(&dir.path().join("graph.db")).unwrap();
        // Pair 1: weight 100, Pair 2: weight 1 — pair 1 should have higher score.
        store
            .upsert_nodes(
                "proj",
                &[
                    make_chunk_node("a1"),
                    make_chunk_node("b1"),
                    make_chunk_node("a2"),
                    make_chunk_node("b2"),
                ],
            )
            .unwrap();
        store
            .upsert_edges(
                "proj",
                &[
                    make_cooccurrence_edge("a1", "b1", 100),
                    make_cooccurrence_edge("b1", "a1", 100),
                    make_cooccurrence_edge("a2", "b2", 1),
                    make_cooccurrence_edge("b2", "a2", 1),
                ],
            )
            .unwrap();
        let result = find_cooccurrence_clusters(&store, "proj", 1, 2, 100).unwrap();
        // There should be 2 clusters; the first should have higher or equal score.
        assert!(result.len() >= 2);
        assert!(result[0].score >= result[1].score);
    }
}
