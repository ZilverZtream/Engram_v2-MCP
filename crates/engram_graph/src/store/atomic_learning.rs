use super::*;

/// Preserve the node-upsert contract inside an already admitted transaction.
pub(super) fn upsert_nodes_in_tx(
    tx: &redb::WriteTransaction,
    project: &str,
    nodes: &[Node],
) -> anyhow::Result<()> {
    if nodes.is_empty() {
        return Ok(());
    }
    let mut table = tx.open_table(NODES)?;
    for node in nodes {
        validate_key_component("node_id", &node.node_id)?;
        let key = format!("{project}\0{}", node.node_id);
        // Absent metadata means no opinion; endpoint writers must not erase
        // source fingerprints or annotations. Refuse unreadable stored data.
        let inherited = if node.metadata.is_none() {
            table
                .get(key.as_str())?
                .map(|value| bincode::deserialize::<Node>(value.value()))
                .transpose()?
                .and_then(|old| old.metadata)
        } else {
            None
        };
        let bytes = if let Some(metadata) = inherited {
            let mut merged = node.clone();
            merged.metadata = Some(metadata);
            bincode::serialize(&merged)?
        } else {
            bincode::serialize(node)?
        };
        table.insert(key.as_str(), bytes.as_slice())?;
    }
    Ok(())
}

pub(super) fn increment_edges_in_tx(
    tx: &redb::WriteTransaction,
    project: &str,
    namespace: &str,
    language: &str,
    generation: u64,
    increments: &[(EdgeKind, String, String, u32)],
) -> anyhow::Result<()> {
    if increments.is_empty() {
        return Ok(());
    }
    let now = now_ms();
    let mut edges = tx.open_table(EDGES)?;
    let mut outgoing = tx.open_table(ADJ_OUT)?;
    let mut incoming = tx.open_table(ADJ_IN)?;
    for (kind, source, target, delta) in increments {
        let key = edge_key(project, kind, source, target);
        // Read the current transaction's value each time: duplicate/reversed
        // pairs and concurrent events must accumulate rather than lose counts.
        let existing = edges
            .get(key.as_str())?
            .map(|value| bincode::deserialize::<Edge>(value.value()))
            .transpose()?;
        let edge = if let Some(mut edge) = existing {
            edge.weight = edge.weight.saturating_add(*delta);
            edge.updated_at_ms = now;
            edge.generation = generation;
            edge
        } else {
            Edge {
                source_id: source.clone(),
                target_id: target.clone(),
                namespace: namespace.into(),
                language: language.into(),
                edge_kind: kind.clone(),
                weight: *delta,
                generation,
                metadata: None,
                updated_at_ms: now,
            }
        };
        let bytes = bincode::serialize(&edge)?;
        edges.insert(key.as_str(), bytes.as_slice())?;
        let value = encode_adj_value(edge.weight, now);
        let out_key = adj_key(project, kind, source);
        outgoing.insert((out_key.as_str(), target.as_str()), value.as_slice())?;
        let in_key = adj_key(project, kind, target);
        incoming.insert((in_key.as_str(), source.as_str()), value.as_slice())?;
    }
    Ok(())
}

impl GraphStore {
    /// Record one complete learning event, including endpoints and both edge
    /// directions/mirrors. A failure preserves the graph as it was before the
    /// event; no partially learned search session becomes visible.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_nodes_and_increment_undirected_edges(
        &self,
        project_id: &str,
        namespace: &str,
        language: &str,
        generation: u64,
        nodes: &[Node],
        pairs: &[(EdgeKind, String, String, u32)],
    ) -> anyhow::Result<()> {
        if nodes.is_empty() && pairs.is_empty() {
            return Ok(());
        }
        validate_key_component("project_id", project_id)?;
        for node in nodes {
            validate_key_component("node_id", &node.node_id)?;
        }
        let mut directed = Vec::with_capacity(pairs.len().saturating_mul(2));
        for (kind, a, b, delta) in pairs {
            validate_key_component("source_id", a)?;
            validate_key_component("target_id", b)?;
            if a != b {
                directed.push((kind.clone(), a.clone(), b.clone(), *delta));
                directed.push((kind.clone(), b.clone(), a.clone(), *delta));
            }
        }
        let tx = self.db.begin_write()?;
        upsert_nodes_in_tx(&tx, project_id, nodes)?;
        increment_edges_in_tx(&tx, project_id, namespace, language, generation, &directed)?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use redb::ReadableTableMetadata;

    fn node(id: &str) -> Node {
        Node {
            node_id: id.into(),
            node_type: "chunk".into(),
            name: id.into(),
            namespace: "memory".into(),
            language: "rust".into(),
            file_path: RelPath::new("source.rs"),
            start_line: 3,
            end_line: 5,
            generation: 7,
            metadata: None,
        }
    }

    fn pair(kind: EdgeKind, a: &str, b: &str, delta: u32) -> (EdgeKind, String, String, u32) {
        (kind, a.into(), b.into(), delta)
    }

    fn assert_mirrors(
        graph: &GraphStore,
        kind: EdgeKind,
        a: &str,
        b: &str,
        forward: u32,
        reverse: u32,
    ) {
        let read = graph.db.begin_read().unwrap();
        let edges = read.open_table(EDGES).unwrap();
        let outgoing = read.open_table(ADJ_OUT).unwrap();
        let incoming = read.open_table(ADJ_IN).unwrap();
        for (source, target, weight) in [(a, b, forward), (b, a, reverse)] {
            let key = edge_key("project", &kind, source, target);
            let stored = edges.get(key.as_str()).unwrap().unwrap();
            let edge: Edge = bincode::deserialize(stored.value()).unwrap();
            assert_eq!(edge.weight, weight);
            let out_key = adj_key("project", &kind, source);
            let in_key = adj_key("project", &kind, target);
            for value in [
                outgoing.get((out_key.as_str(), target)).unwrap().unwrap(),
                incoming.get((in_key.as_str(), source)).unwrap().unwrap(),
            ] {
                assert_eq!(
                    decode_adj_value(value.value()).unwrap(),
                    (weight, edge.updated_at_ms)
                );
            }
        }
    }

    #[test]
    fn concurrent_learning_preserves_every_event_and_never_links_separate_sessions() {
        let temporary = tempfile::tempdir().unwrap();
        let graph = GraphStore::open(&temporary.path().join("graph.redb")).unwrap();
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let graph = &graph;
                scope.spawn(move || {
                    for _ in 0..8 {
                        for (a, b) in [("a", "b"), ("b", "c")] {
                            graph
                                .upsert_nodes_and_increment_undirected_edges(
                                    "project",
                                    "memory",
                                    "text",
                                    7,
                                    &[node(a), node(b)],
                                    &[
                                        pair(EdgeKind::CoOccurrence, a, b, 1),
                                        pair(EdgeKind::CoOccurrence, b, a, 2),
                                        pair(EdgeKind::Dependency, a, b, 1),
                                        pair(EdgeKind::CoOccurrence, a, a, 100),
                                    ],
                                )
                                .unwrap();
                        }
                    }
                });
            }
        });
        for (a, b) in [("a", "b"), ("b", "c")] {
            assert_mirrors(&graph, EdgeKind::CoOccurrence, a, b, 96, 96);
            assert_mirrors(&graph, EdgeKind::Dependency, a, b, 32, 32);
        }
        assert_eq!(graph.count_edges("project").unwrap(), 8);
        assert_eq!(
            graph
                .neighbors("project", EdgeKind::CoOccurrence, "a", 10)
                .unwrap(),
            vec![("b".into(), 96)]
        );
        drop(graph);
        temporary.close().unwrap();
    }

    #[test]
    fn learning_weights_saturate_without_erasing_annotations() {
        let temporary = tempfile::tempdir().unwrap();
        let graph = GraphStore::open(&temporary.path().join("graph.redb")).unwrap();
        let mut original = node("a");
        original.metadata =
            Some(serde_json::json!({"human_note":"keep","file_hash":"fingerprint"}));
        graph.upsert_nodes("project", &[original.clone()]).unwrap();
        graph
            .upsert_nodes_and_increment_undirected_edges(
                "project",
                "memory",
                "text",
                7,
                &[node("a"), node("b")],
                &[pair(EdgeKind::CoOccurrence, "a", "b", u32::MAX - 2)],
            )
            .unwrap();
        let mut edges = graph
            .list_edges_by_kind("project", EdgeKind::CoOccurrence, 10)
            .unwrap();
        for edge in &mut edges {
            edge.metadata = Some(serde_json::json!({"human_note":"edge annotation"}));
            if edge.source_id == "b" {
                edge.weight = 9;
            }
        }
        graph.upsert_edges("project", &edges).unwrap();
        graph
            .upsert_nodes_and_increment_undirected_edges(
                "project",
                "memory",
                "text",
                8,
                &[],
                &[
                    pair(EdgeKind::CoOccurrence, "a", "b", 2),
                    pair(EdgeKind::CoOccurrence, "b", "a", 4),
                ],
            )
            .unwrap();
        assert_mirrors(&graph, EdgeKind::CoOccurrence, "a", "b", u32::MAX, 15);
        assert_eq!(
            graph.get_node("project", "a").unwrap().unwrap().metadata,
            original.metadata
        );
        for edge in graph
            .list_edges_by_kind("project", EdgeKind::CoOccurrence, 10)
            .unwrap()
        {
            assert_eq!(edge.metadata.unwrap()["human_note"], "edge annotation");
            assert_eq!(edge.generation, 8);
        }
        drop(graph);
        temporary.close().unwrap();
    }

    #[test]
    fn unreadable_late_edge_rolls_back_entire_learning_event_after_reopen() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("graph.redb");
        let graph = GraphStore::open(&path).unwrap();
        let mut original = node("existing");
        original.metadata = Some(serde_json::json!({"human_note":"keep"}));
        graph.upsert_nodes("project", &[original.clone()]).unwrap();
        let bad_key = edge_key("project", &EdgeKind::CoOccurrence, "bad", "edge");
        let tx = graph.db.begin_write().unwrap();
        tx.open_table(EDGES)
            .unwrap()
            .insert(bad_key.as_str(), b"unreadable edge".as_slice())
            .unwrap();
        tx.commit().unwrap();
        let mut replacement = node("existing");
        replacement.generation = 99;
        replacement.metadata = Some(serde_json::json!({"human_note":"changed by event"}));
        let error = graph.upsert_nodes_and_increment_undirected_edges(
            "project",
            "memory",
            "text",
            99,
            &[replacement, node("new")],
            &[
                pair(EdgeKind::Dependency, "existing", "new", 1),
                pair(EdgeKind::CoOccurrence, "bad", "edge", 1),
            ],
        );
        assert!(
            error.is_err(),
            "Unreadable stored evidence must not be replaced with a fresh count"
        );
        drop(graph);
        let graph = GraphStore::open(&path).unwrap();
        assert_eq!(
            bincode::serialize(&graph.get_node("project", "existing").unwrap().unwrap()).unwrap(),
            bincode::serialize(&original).unwrap()
        );
        assert!(graph.get_node("project", "new").unwrap().is_none());
        assert_eq!(graph.count_edges("project").unwrap(), 1);
        {
            let read = graph.db.begin_read().unwrap();
            assert_eq!(
                read.open_table(EDGES)
                    .unwrap()
                    .get(bad_key.as_str())
                    .unwrap()
                    .unwrap()
                    .value(),
                b"unreadable edge"
            );
            assert!(read.open_table(ADJ_OUT).unwrap().is_empty().unwrap());
            assert!(read.open_table(ADJ_IN).unwrap().is_empty().unwrap());
        }
        drop(graph);
        temporary.close().unwrap();
    }
}
