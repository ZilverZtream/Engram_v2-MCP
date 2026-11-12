//! State references share the existing ES module/export/config resolver.
//! Retained file observations survive deletion of resolved target nodes.
use super::*;
use engram_core::modern_state::{BINDING_KEY, KEY, Reference, VERSION};

impl GraphStore {
    pub(in crate::store) fn resolve_modern_state_imports(
        &self,
        project: &str,
    ) -> anyhow::Result<usize> {
        let mut resolver = Resolver {
            modules: BTreeMap::new(),
            nodes: BTreeMap::new(),
            configs: BTreeMap::new(),
            state_category: Some("ModuleOrSingleton".into()),
        };
        let mut references = Vec::new();
        let mut previous = Vec::new();
        {
            let tx = self.db.begin_read()?;
            let nodes = tx.open_table(NODES)?;
            let prefix = format!("{project}\0");
            for row in nodes.range(prefix.as_str()..)? {
                let (key, value) = row?;
                if !key.value().starts_with(&prefix) {
                    break;
                }
                let node: Node = bincode::deserialize(value.value())?;
                if node.namespace != engram_core::namespaces::NAMESPACE_MEMORY {
                    continue;
                }
                if node.node_type == "file" {
                    if node.file_path.as_str().ends_with(".json") {
                        let config = node
                            .metadata
                            .as_ref()
                            .and_then(|m| m.get(CONFIG_KEY))
                            .and_then(|v| v.as_str())
                            .and_then(|raw| serde_json::from_str::<Config>(raw).ok())
                            .filter(|config| config.version == 1);
                        resolver
                            .configs
                            .insert(node.file_path.as_str().into(), config);
                    }
                    if let Some(coverage) = node
                        .metadata
                        .as_ref()
                        .and_then(|m| m.get(KEY))
                        .and_then(|v| v.as_str())
                        .and_then(|raw| serde_json::from_str::<JsonValue>(raw).ok())
                        .filter(|facts| facts["version"] == VERSION)
                    {
                        if let Ok(module) =
                            serde_json::from_value::<Module>(coverage["script_module"].clone())
                        {
                            resolver
                                .modules
                                .insert(node.file_path.as_str().into(), (node.clone(), module));
                            let retained = serde_json::from_value::<Vec<Reference>>(
                                coverage["references"].clone(),
                            )?;
                            references.push((
                                node.clone(),
                                coverage["source_hash"].clone(),
                                retained,
                            ));
                        }
                    }
                }
                resolver.nodes.insert(node.node_id.clone(), node);
            }
            let edges = tx.open_table(EDGES)?;
            for row in edges.range(prefix.as_str()..)? {
                let (key, value) = row?;
                if !key.value().starts_with(&prefix) {
                    break;
                }
                let edge: Edge = bincode::deserialize(value.value())?;
                if edge.namespace == engram_core::namespaces::NAMESPACE_MEMORY && edge
                    .metadata
                    .as_ref()
                    .is_some_and(|m| m.get(BINDING_KEY).is_some())
                {
                    previous.push(edge);
                }
            }
        }
        let mut desired: BTreeMap<String, Edge> = BTreeMap::new();
        let mut targets: BTreeMap<String, Node> = BTreeMap::new();
        for (file, hash, references) in references {
            for reference in references {
                let path = file.file_path.as_str();
                let source = if reference.source_kind == "file" {
                    Some(file.node_id.clone())
                } else if let Some(node) = resolver.nodes.get(
                    &engram_core::NodeId::symbol(
                        &reference.source_kind,
                        None,
                        path,
                        &reference.source_name,
                        reference.source_line,
                    )
                    .0,
                ) {
                    Some(node.node_id.clone())
                } else {
                    let candidates = resolver
                        .nodes
                        .values()
                        .filter(|node| {
                            node.file_path.as_str() == path
                                && node.node_type == reference.source_kind
                                && node.start_line == reference.source_line
                                && (node.name == reference.source_name
                                    || node
                                        .metadata
                                        .as_ref()
                                        .and_then(|m| m.get("fqn"))
                                        .and_then(|v| v.as_str())
                                        == Some(reference.source_name.as_str()))
                        })
                        .collect::<Vec<_>>();
                    (candidates.len() == 1).then(|| candidates[0].node_id.clone())
                };
                let Some(source) = source else {
                    continue;
                };
                resolver.state_category = Some(
                    reference
                        .category
                        .clone()
                        .unwrap_or_else(|| "ModuleOrSingleton".into()),
                );
                let bound = resolver
                    .module_path(path, &reference.import.module)
                    .and_then(|module| {
                        resolver.export(
                            &module,
                            &reference.imported,
                            &mut BTreeSet::new(),
                            &mut 512,
                        )
                    })
                    .and_then(|target| match reference.unbound_reason.as_deref() {
                        Some(reason) => Err(reason),
                        None => Ok(target),
                    });
                // An indexed function/class or a different framework category
                // is not ordinary mutable module state.
                if matches!(bound, Err("not_a_supported_state_declaration")) {
                    continue;
                }
                let (target, reason) = match bound {
                    Ok(root) => {
                        let declaration = &resolver.nodes[&root];
                        let mut parts = root.splitn(3, ':');
                        parts.next();
                        let category = parts.next().unwrap();
                        let key = parts.next().unwrap();
                        let category = if category == "Module" && !reference.suffix.is_empty() {
                            "Singleton"
                        } else {
                            category
                        };
                        let key = format!("{key}{}", reference.suffix);
                        let target = engram_core::NodeId::state(category, &key).0;
                        if !resolver.nodes.contains_key(&target) {
                            let mut node = declaration.clone();
                            node.node_id = target.clone();
                            node.name = format!("{category}:{key}");
                            node.start_line = 0;
                            node.end_line = 0;
                            node.metadata = Some(
                                serde_json::json!({"state_type":category,"state_key":key,"modern_state_import_target":true}),
                            );
                            targets.insert(target.clone(), node);
                        }
                        (target, None)
                    }
                    Err(reason) => (
                        format!(
                            "::modern_state:{path}:{}:{}",
                            reference.site.offset, reference.imported
                        ),
                        Some(reason),
                    ),
                };
                for (enabled, kind) in [
                    (
                        reference.reads,
                        if reason.is_none() {
                            EdgeKind::ReadsState
                        } else {
                            EdgeKind::UnresolvedStateRead
                        },
                    ),
                    (
                        reference.writes,
                        if reason.is_none() {
                            EdgeKind::WritesState
                        } else {
                            EdgeKind::UnresolvedStateWrite
                        },
                    ),
                ] {
                    if !enabled {
                        continue;
                    }
                    let key = edge_key(project, &kind, &source, &target);
                    let evidence = serde_json::json!({"reference":reference,"reason":reason,"source_hash":hash});
                    let edge = desired.entry(key).or_insert_with(||Edge {source_id:source.clone(),target_id:target.clone(),
                        namespace:engram_core::namespaces::NAMESPACE_MEMORY.into(),language:file.language.clone(),edge_kind:kind,
                        weight:0,generation:file.generation,updated_at_ms:now_ms(),
                        metadata:Some(serde_json::json!({BINDING_KEY:[],"state_evidence":"static_import_v1",
                            "state_access_line":reference.site.line.to_string(),"state_access_column":reference.site.column.to_string(),
                            "state_access_byte":reference.site.offset.to_string(),"state_access_end_byte":reference.site.end_offset.to_string()}))});
                    edge.weight += 1;
                    edge.metadata.as_mut().unwrap()[BINDING_KEY]
                        .as_array_mut()
                        .unwrap()
                        .push(evidence);
                }
            }
        }
        self.upsert_nodes(project, &targets.into_values().collect::<Vec<_>>())?;
        let desired = desired.into_values().collect::<Vec<_>>();
        self.replace_edge_snapshot(project, &previous, &desired)?;
        // Only virtual property nodes created by this resolver are disposable.
        // Keep any node still referenced by another producer, in either direction.
        let desired_targets = desired
            .iter()
            .map(|edge| edge.target_id.as_str())
            .collect::<BTreeSet<_>>();
        let mut retired = resolver
            .nodes
            .values()
            .filter(|node| {
                !desired_targets.contains(node.node_id.as_str())
                    && node
                        .metadata
                        .as_ref()
                        .is_some_and(|metadata| metadata["modern_state_import_target"] == true)
            })
            .map(|node| node.node_id.clone())
            .collect::<BTreeSet<_>>();
        if !retired.is_empty() {
            let tx = self.db.begin_write()?;
            {
                let edges = tx.open_table(EDGES)?;
                let prefix = format!("{project}\0");
                for row in edges.range(prefix.as_str()..)? {
                    let (key, value) = row?;
                    if !key.value().starts_with(&prefix) || retired.is_empty() {
                        break;
                    }
                    let edge: Edge = bincode::deserialize(value.value())?;
                    retired.remove(&edge.source_id);
                    retired.remove(&edge.target_id);
                }
                let mut nodes = tx.open_table(NODES)?;
                for id in retired {
                    let key = format!("{project}\0{id}");
                    let owned = nodes
                        .get(key.as_str())?
                        .map(|raw| bincode::deserialize::<Node>(raw.value()))
                        .transpose()?
                        .and_then(|node| node.metadata)
                        .is_some_and(|metadata| metadata["modern_state_import_target"] == true);
                    if owned {
                        nodes.remove(key.as_str())?;
                    }
                }
            }
            tx.commit()?;
        }
        Ok(desired.len())
    }
}
