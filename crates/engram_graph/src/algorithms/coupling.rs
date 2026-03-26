use crate::store::{EdgeKind, GraphStore};

#[derive(Debug, Clone)]
pub struct Coupling {
    pub file_node_id: String,
    pub neighbor_node_id: String,
    pub weight: u32,
}

pub fn top_project_couplings(
    store: &GraphStore,
    project_id: &str,
    limit: usize,
) -> anyhow::Result<Vec<Coupling>> {
    let all_ids = store.list_node_ids(project_id, Some("file"))?;
    let mut all_couplings = Vec::new();

    for id in all_ids {
        let neigh = store.neighbors(project_id, EdgeKind::TemporalCoupling, &id, limit)?;
        for (n, w) in neigh {
            // To avoid double counting (a<->b and b<->a), we can enforce an order.
            if id < n {
                all_couplings.push(Coupling {
                    file_node_id: id.clone(),
                    neighbor_node_id: n,
                    weight: w,
                });
            }
        }
    }

    all_couplings.sort_by(|a, b| b.weight.cmp(&a.weight));
    if all_couplings.len() > limit {
        all_couplings.truncate(limit);
    }
    Ok(all_couplings)
}

pub fn file_temporal_couplings(
    store: &GraphStore,
    project_id: &str,
    file_node_id: &str,
    min_frequency: u32,
    limit: usize,
) -> anyhow::Result<Vec<Coupling>> {
    let neigh = store.neighbors(project_id, EdgeKind::TemporalCoupling, file_node_id, limit)?;
    let mut couplings = Vec::new();
    for (n, w) in neigh {
        if w >= min_frequency {
            couplings.push(Coupling {
                file_node_id: file_node_id.to_string(),
                neighbor_node_id: n,
                weight: w,
            });
        }
    }
    Ok(couplings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Edge, EdgeKind, GraphStore, Node};
    use engram_core::RelPath;
    use tempfile::tempdir;

    fn make_file_node(id: &str) -> Node {
        Node {
            node_id: id.to_string(),
            node_type: "file".to_string(),
            name: id.to_string(),
            namespace: "test".to_string(),
            language: "csharp".to_string(),
            file_path: RelPath::new(id),
            start_line: 1,
            end_line: 50,
            generation: 1,
            metadata: None,
        }
    }

    fn make_temporal_edge(src: &str, tgt: &str, weight: u32) -> Edge {
        Edge {
            source_id: src.to_string(),
            target_id: tgt.to_string(),
            namespace: "test".to_string(),
            language: "csharp".to_string(),
            edge_kind: EdgeKind::TemporalCoupling,
            weight,
            generation: 1,
            metadata: None,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn empty_graph_no_couplings() {
        let dir = tempdir().unwrap();
        let store = GraphStore::open(&dir.path().join("graph.db")).unwrap();
        let result = top_project_couplings(&store, "proj", 100).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn two_files_with_coupling_detected() {
        let dir = tempdir().unwrap();
        let store = GraphStore::open(&dir.path().join("graph.db")).unwrap();
        store
            .upsert_nodes("proj", &[make_file_node("file_a"), make_file_node("file_b")])
            .unwrap();
        // The dedup logic keeps only the pair where src < tgt lexicographically.
        store
            .upsert_edges(
                "proj",
                &[
                    make_temporal_edge("file_a", "file_b", 10),
                    make_temporal_edge("file_b", "file_a", 10),
                ],
            )
            .unwrap();
        let result = top_project_couplings(&store, "proj", 100).unwrap();
        assert_eq!(result.len(), 1);
        let c = &result[0];
        assert!(
            (c.file_node_id == "file_a" && c.neighbor_node_id == "file_b")
                || (c.file_node_id == "file_b" && c.neighbor_node_id == "file_a")
        );
    }

    #[test]
    fn min_frequency_filters_weak_coupling() {
        let dir = tempdir().unwrap();
        let store = GraphStore::open(&dir.path().join("graph.db")).unwrap();
        store
            .upsert_nodes("proj", &[make_file_node("a"), make_file_node("b")])
            .unwrap();
        store
            .upsert_edges("proj", &[make_temporal_edge("a", "b", 2)])
            .unwrap();
        // min_frequency=5 should exclude weight=2 edge.
        let result = file_temporal_couplings(&store, "proj", "a", 5, 100).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn top_project_couplings_limit_respected() {
        let dir = tempdir().unwrap();
        let store = GraphStore::open(&dir.path().join("graph.db")).unwrap();
        // Build 6 unique file pairs, each with a TemporalCoupling edge.
        let files: Vec<String> = (0..6).map(|i| format!("file_{i:02}")).collect();
        let nodes: Vec<Node> = files.iter().map(|f| make_file_node(f)).collect();
        store.upsert_nodes("proj", &nodes).unwrap();
        // Pair up files: (0,1), (2,3), (4,5) — 3 pairs where src < tgt.
        let edges: Vec<Edge> = (0..3)
            .flat_map(|i| {
                let a = &files[i * 2];
                let b = &files[i * 2 + 1];
                vec![
                    make_temporal_edge(a, b, 10),
                    make_temporal_edge(b, a, 10),
                ]
            })
            .collect();
        store.upsert_edges("proj", &edges).unwrap();
        let result = top_project_couplings(&store, "proj", 2).unwrap();
        assert!(result.len() <= 2);
    }

    #[test]
    fn no_double_counting_symmetric_pairs() {
        let dir = tempdir().unwrap();
        let store = GraphStore::open(&dir.path().join("graph.db")).unwrap();
        store
            .upsert_nodes("proj", &[make_file_node("aaa"), make_file_node("zzz")])
            .unwrap();
        // Both directions of the same logical pair.
        store
            .upsert_edges(
                "proj",
                &[
                    make_temporal_edge("aaa", "zzz", 7),
                    make_temporal_edge("zzz", "aaa", 7),
                ],
            )
            .unwrap();
        let result = top_project_couplings(&store, "proj", 100).unwrap();
        // Should only have 1 entry, not 2.
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn file_temporal_couplings_single_file() {
        let dir = tempdir().unwrap();
        let store = GraphStore::open(&dir.path().join("graph.db")).unwrap();
        store
            .upsert_nodes(
                "proj",
                &[
                    make_file_node("root"),
                    make_file_node("dep1"),
                    make_file_node("dep2"),
                ],
            )
            .unwrap();
        store
            .upsert_edges(
                "proj",
                &[
                    make_temporal_edge("root", "dep1", 15),
                    make_temporal_edge("root", "dep2", 8),
                ],
            )
            .unwrap();
        let result = file_temporal_couplings(&store, "proj", "root", 1, 100).unwrap();
        assert_eq!(result.len(), 2);
        assert!(result.iter().all(|c| c.file_node_id == "root"));
    }

    #[test]
    fn couplings_sorted_by_weight_descending() {
        let dir = tempdir().unwrap();
        let store = GraphStore::open(&dir.path().join("graph.db")).unwrap();
        store
            .upsert_nodes(
                "proj",
                &[
                    make_file_node("src"),
                    make_file_node("tgt1"),
                    make_file_node("tgt2"),
                    make_file_node("tgt3"),
                ],
            )
            .unwrap();
        store
            .upsert_edges(
                "proj",
                &[
                    make_temporal_edge("src", "tgt1", 3),
                    make_temporal_edge("src", "tgt2", 30),
                    make_temporal_edge("src", "tgt3", 10),
                ],
            )
            .unwrap();
        let result = file_temporal_couplings(&store, "proj", "src", 1, 100).unwrap();
        assert_eq!(result.len(), 3);
        // Neighbors are sorted by weight descending by GraphStore::neighbors.
        assert!(result[0].weight >= result[1].weight);
        assert!(result[1].weight >= result[2].weight);
    }
}
