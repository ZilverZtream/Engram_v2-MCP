//! Signature references survive independently of candidate declarations.
//! Binding is type-only and qualification-preserving; it is not a compiler proof.
use super::*;
use std::collections::BTreeMap;

fn normalized(name: &str, language: &str) -> String {
    let name = name
        .trim_start_matches("global::")
        .trim_start_matches("Global.")
        .replace("::", ".");
    if matches!(language, "vb" | "vbnet") {
        name.to_lowercase()
    } else {
        name
    }
}

impl GraphStore {
    pub(super) fn resolve_signature_types(&self, project: &str) -> anyhow::Result<usize> {
        let prefix = format!("{project}\0");
        let mut references = Vec::new();
        let mut declarations: HashMap<(String, String), Vec<Node>> = HashMap::new();
        let mut old = BTreeMap::new();
        let wtx = self.db.begin_write()?;
        {
            let table = wtx.open_table(NODES)?;
            for entry in table.range(prefix.as_str()..)? {
                let (key, value) = entry?;
                if !key.value().starts_with(&prefix) {
                    break;
                }
                let node: Node = bincode::deserialize(value.value())?;
                if node.namespace != engram_core::namespaces::NAMESPACE_MEMORY {
                    continue;
                }
                if node.node_type == "type_reference" {
                    references.push(node);
                    continue;
                }
                if !matches!(
                    node.node_type.as_str(),
                    "class" | "struct" | "interface" | "enum" | "record" | "type" | "type_alias"
                ) {
                    continue;
                }
                let name = node
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("fqn"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(&node.name);
                declarations
                    .entry((node.language.clone(), normalized(name, &node.language)))
                    .or_default()
                    .push(node);
            }
            let table = wtx.open_table(EDGES)?;
            let edge_prefix = format!("{project}\0type_definition\0");
            for entry in table.range(edge_prefix.as_str()..)? {
                let (key, value) = entry?;
                if !key.value().starts_with(&edge_prefix) {
                    break;
                }
                let edge: Edge = bincode::deserialize(value.value())?;
                old.insert(key.value().to_string(), edge);
            }
        }
        let mut wanted = BTreeMap::new();
        for reference in references {
            let Some(metadata) = reference.metadata.as_ref() else {
                continue;
            };
            if metadata.get("type_reference_kind").and_then(|v| v.as_str()) != Some("named") {
                continue;
            }
            let raw = normalized(&reference.name, &reference.language);
            let owner = metadata
                .get("type_reference_owner")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let owner = normalized(owner, &reference.language);
            let mut names = Vec::new();
            let mut scope = owner.rsplit_once('.').map(|(scope, _)| scope);
            // Lexical containing scopes first, then the written qualified name.
            // Never discard a qualifier or prefer a function with a matching name.
            while let Some(current) = scope {
                names.push(format!("{current}.{raw}"));
                scope = current.rsplit_once('.').map(|(scope, _)| scope);
            }
            names.push(raw);
            for name in names {
                let Some(candidates) =
                    declarations.get(&(reference.language.clone(), name.clone()))
                else {
                    continue;
                };
                // A duplicate/ambiguous type in the nearest scope cannot borrow
                // another declaration from an outer scope.
                if candidates.len() != 1 {
                    break;
                }
                let target = &candidates[0];
                let edge = Edge {
                    source_id: reference.node_id.clone(),
                    target_id: target.node_id.clone(),
                    namespace: engram_core::namespaces::NAMESPACE_MEMORY.into(),
                    language: reference.language.clone(),
                    edge_kind: EdgeKind::TypeDefinition,
                    weight: 1,
                    generation: reference.generation,
                    metadata: Some(serde_json::json!({
                        "resolution": "unique_declared_type_candidate", "matched_name": name,
                        "type_binding_proven": false, "confidence": "0.75",
                        "source_generation": reference.generation, "target_generation": target.generation,
                    })),
                    updated_at_ms: 0,
                };
                wanted.insert(
                    edge_key(project, &edge.edge_kind, &edge.source_id, &edge.target_id),
                    edge,
                );
                break;
            }
        }
        // Keep timestamps stable on unchanged evidence, avoiding a write on an
        // idempotent resolver pass. Changes replace adjacency and edge together.
        for (key, edge) in &mut wanted {
            edge.updated_at_ms = old
                .get(key)
                .map_or_else(now_ms, |previous| previous.updated_at_ms);
        }
        let unchanged = old.len() == wanted.len()
            && wanted.iter().all(|(key, edge)| {
                old.get(key).is_some_and(|previous| {
                    bincode::serialize(previous).ok() == bincode::serialize(edge).ok()
                })
            });
        if unchanged {
            return Ok(0);
        }
        {
            let mut edges = wtx.open_table(EDGES)?;
            let mut outgoing = wtx.open_table(ADJ_OUT)?;
            let mut incoming = wtx.open_table(ADJ_IN)?;
            for (key, edge) in &old {
                edges.remove(key.as_str())?;
                outgoing.remove((
                    adj_key(project, &edge.edge_kind, &edge.source_id).as_str(),
                    edge.target_id.as_str(),
                ))?;
                incoming.remove((
                    adj_key(project, &edge.edge_kind, &edge.target_id).as_str(),
                    edge.source_id.as_str(),
                ))?;
            }
            for (key, edge) in &wanted {
                let value = bincode::serialize(edge)?;
                edges.insert(key.as_str(), value.as_slice())?;
                let value = encode_adj_value(edge.weight, edge.updated_at_ms);
                outgoing.insert(
                    (
                        adj_key(project, &edge.edge_kind, &edge.source_id).as_str(),
                        edge.target_id.as_str(),
                    ),
                    value.as_slice(),
                )?;
                incoming.insert(
                    (
                        adj_key(project, &edge.edge_kind, &edge.target_id).as_str(),
                        edge.source_id.as_str(),
                    ),
                    value.as_slice(),
                )?;
            }
        }
        wtx.commit()?;
        Ok(wanted.len() + old.keys().filter(|key| !wanted.contains_key(*key)).count())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn node(id: &str, kind: &str, name: &str, path: &str) -> Node {
        Node {
            node_id: id.into(),
            node_type: kind.into(),
            name: name.into(),
            namespace: "memory".into(),
            language: "cs".into(),
            file_path: path.into(),
            start_line: 1,
            end_line: 1,
            generation: 1,
            metadata: None,
        }
    }
    fn reference(id: &str, name: &str, kind: &str) -> Node {
        let mut result = node(id, "type_reference", name, "Service.cs");
        result.metadata = Some(
            serde_json::json!({"type_reference_kind":kind,"type_reference_owner":"Contracts.Service.Run"}),
        );
        result
    }
    #[test]
    fn signature_references_do_not_crowd_out_or_impersonate_declarations() {
        let temp = tempfile::tempdir().unwrap();
        let graph = GraphStore::open(&temp.path().join("graph.redb")).unwrap();
        let mut nodes: Vec<_> = (0..150)
            .map(|i| reference(&format!("sym:a{i:03}"), "Request", "named"))
            .collect();
        nodes.push(node("sym:zreal", "class", "Request", "Request.cs"));
        nodes.push(node("sym:caller", "function", "Run", "Service.cs"));
        graph.upsert_nodes("p", &nodes).unwrap();
        let found = graph
            .query_nodes_by_symbol_name("p", "Request", None, 5)
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].node_id, "sym:zreal");
        match graph.resolve_symbol("p", "Request", None, None).unwrap() {
            ResolveResult::Unique(node) => assert_eq!(node.node_id, "sym:zreal"),
            other => panic!("References cannot become declaration candidates: {other:?}"),
        }
        match graph.resolve_symbol("p", "sym:a000", None, None).unwrap() {
            ResolveResult::Unique(node) => assert_eq!(node.node_type, "type_reference"),
            other => panic!("Direct reference lookup must remain available: {other:?}"),
        }
        assert_eq!(
            graph
                .query_nodes("p", Some("type_reference"), Some("Request"), None, 200)
                .unwrap()
                .len(),
            150
        );
        graph
            .upsert_edges(
                "p",
                &[Edge {
                    source_id: "sym:caller".into(),
                    target_id: "::Request".into(),
                    namespace: "memory".into(),
                    language: "cs".into(),
                    edge_kind: EdgeKind::Calls,
                    weight: 1,
                    generation: 1,
                    metadata: None,
                    updated_at_ms: 1,
                }],
            )
            .unwrap();
        graph.resolve_symbol_edges("p").unwrap();
        let calls = graph.list_edges("p", Some(EdgeKind::Calls)).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].target_id, "sym:zreal");
    }
    #[test]
    fn signature_type_candidates_preserve_qualification_and_do_not_bind_other_kinds() {
        let temp = tempfile::tempdir().unwrap();
        let graph = GraphStore::open(&temp.path().join("graph.redb")).unwrap();
        graph
            .upsert_nodes(
                "p",
                &[
                    node("decl", "class", "Contracts.Request", "Request.cs"),
                    node("wrong", "function", "Contracts.Response", "Functions.cs"),
                    node("other", "class", "Other.External", "Other.cs"),
                    reference("request", "Request", "named"),
                    reference("response", "Response", "named"),
                    reference("external", "Missing.External", "named"),
                    reference("generic", "Request", "type_parameter"),
                ],
            )
            .unwrap();
        assert_eq!(graph.resolve_signature_types("p").unwrap(), 1);
        let edges = graph.edges_touching("p", "request", 10).unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_id, "decl");
        assert_eq!(
            edges[0].metadata.as_ref().unwrap()["type_binding_proven"],
            false
        );
        assert_eq!(graph.resolve_signature_types("p").unwrap(), 0);
        for id in ["response", "external", "generic"] {
            assert!(graph.edges_touching("p", id, 10).unwrap().is_empty());
        }
    }
    #[test]
    fn new_ambiguity_retracts_binding_but_keeps_authored_reference() {
        let temp = tempfile::tempdir().unwrap();
        let graph = GraphStore::open(&temp.path().join("graph.redb")).unwrap();
        graph
            .upsert_nodes(
                "p",
                &[
                    reference("request", "Request", "named"),
                    node("decl", "class", "Contracts.Request", "Request.cs"),
                ],
            )
            .unwrap();
        assert_eq!(graph.resolve_signature_types("p").unwrap(), 1);
        graph
            .upsert_nodes(
                "p",
                &[node(
                    "duplicate",
                    "class",
                    "Contracts.Request",
                    "Duplicate.cs",
                )],
            )
            .unwrap();
        assert_eq!(graph.resolve_signature_types("p").unwrap(), 1);
        assert!(graph.edges_touching("p", "request", 10).unwrap().is_empty());
        assert!(graph.get_node("p", "request").unwrap().is_some());
    }
}
