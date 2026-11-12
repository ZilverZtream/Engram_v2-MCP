//! Replace an extracted source graph without touching search or history storage.
use super::*;
use std::collections::{BTreeMap, HashSet};

#[derive(Debug, Default, Serialize)]
pub struct SourceGraphReplacement {
    pub removed_nodes: usize,
    pub removed_edges: usize,
    pub remapped_edges: usize,
    /// Retained historical/learned edges whose retired endpoint has no unique
    /// successor. Their original IDs and evidence remain stored, not guessed.
    pub retained_edges_with_retired_endpoints: usize,
}

fn extracted_node(node: &Node) -> bool {
    node.namespace == engram_core::namespaces::NAMESPACE_MEMORY
        && !matches!(node.node_type.as_str(), "chunk" | "search_document")
        && extracted_address(&node.node_id)
}

fn extracted_address(id: &str) -> bool {
    [
        "file:",
        "sym:",
        "page:",
        "control:",
        "gis_config:",
        "table:",
        "column:",
        "state:",
        "binding_field:",
        "ui_container:",
        "bundle:",
        "route:",
        "sql:",
    ]
    .iter()
    .any(|prefix| id.starts_with(prefix))
}

fn extracted_kind(kind: &EdgeKind) -> bool {
    matches!(
        kind,
        EdgeKind::Calls
            | EdgeKind::Dependency
            | EdgeKind::Contains
            | EdgeKind::Imports
            | EdgeKind::SqlCalls
            | EdgeKind::HasColumn
            | EdgeKind::ForeignKey
            | EdgeKind::QueriesTable
            | EdgeKind::ReadsState
            | EdgeKind::WritesState
            | EdgeKind::DataBinding
            | EdgeKind::RegistersControl
            | EdgeKind::IncludesFile
            | EdgeKind::UnresolvedStateRead
            | EdgeKind::UnresolvedStateWrite
            | EdgeKind::ExposesWebService
            | EdgeKind::ExposesHttpHandler
            | EdgeKind::ExposesWcfService
            | EdgeKind::ContainsUi
            | EdgeKind::UiLayoutNeighbor
            | EdgeKind::ReadsColumn
            | EdgeKind::RegistersModule
            | EdgeKind::RegistersHandler
            | EdgeKind::ManipulatesDom
            | EdgeKind::TriggersPostback
            | EdgeKind::ApiCall
            | EdgeKind::ParameterBinding
            | EdgeKind::InjectsScript
            | EdgeKind::CallsStoredProcedure
            | EdgeKind::StoredProcReadsTable
            | EdgeKind::StoredProcWritesTable
            | EdgeKind::FillsRegion
            | EdgeKind::ReadsSetting
            | EdgeKind::InheritsFrom
            | EdgeKind::Implements
            | EdgeKind::TestOracle
            | EdgeKind::ReturnsType
            | EdgeKind::ParameterType
            | EdgeKind::TypeDefinition
            | EdgeKind::SpatialCall
            | EdgeKind::StateAffinity
    )
}

fn identity(node: &Node) -> (String, String, String) {
    (
        node.file_path.as_str().into(),
        node.node_type.clone(),
        node.name.clone(),
    )
}

impl GraphStore {
    /// The caller holds project mutation admission, prepares/resolves the full
    /// extraction off to the side, and verifies source bytes before this call.
    /// Compare all original file rows inside the write transaction, then replace
    /// extracted rows and adjacency together. Concurrent learning is read here,
    /// not overwritten from a stale pre-extraction graph snapshot.
    pub fn replace_source_graph(
        &self,
        project: &str,
        expected_files: &[Node],
        replacements: &[Node],
        replacement_edges: &[Edge],
    ) -> anyhow::Result<SourceGraphReplacement> {
        validate_key_component("project_id", project)?;
        let mut expected = BTreeMap::new();
        for file in expected_files {
            anyhow::ensure!(
                extracted_node(file)
                    && file.node_type == "file"
                    && file.node_id == engram_core::ids::NodeId::file(file.file_path.as_str()).0,
                "Graph rebuild requires canonical source file evidence"
            );
            anyhow::ensure!(
                expected
                    .insert(file.node_id.clone(), bincode::serialize(file)?)
                    .is_none(),
                "Duplicate source file evidence"
            );
        }
        let mut new_nodes = HashMap::new();
        let mut new_identities: HashMap<_, Vec<&Node>> = HashMap::new();
        let mut new_files = HashSet::new();
        for node in replacements {
            validate_key_component("node_id", &node.node_id)?;
            anyhow::ensure!(
                extracted_node(node),
                "Replacement contains a non-source node"
            );
            anyhow::ensure!(
                new_nodes.insert(node.node_id.as_str(), node).is_none(),
                "Duplicate replacement node; resolve the staged graph first"
            );
            new_identities.entry(identity(node)).or_default().push(node);
            if node.node_type == "file" {
                let old: Node =
                    bincode::deserialize(expected.get(&node.node_id).ok_or_else(|| {
                        anyhow::anyhow!("Rebuild changed the selected source file set")
                    })?)?;
                let hash = |n: &Node| {
                    n.metadata
                        .as_ref()
                        .and_then(|m| m.get("file_hash"))
                        .and_then(|v| v.as_str())
                        .map(str::to_owned)
                };
                anyhow::ensure!(
                    node.generation == old.generation
                        && node.file_path == old.file_path
                        && hash(node).is_some()
                        && hash(node) == hash(&old),
                    "Rebuild changed source content or revision"
                );
                new_files.insert(node.node_id.clone());
            }
        }
        anyhow::ensure!(
            new_files.len() == expected.len(),
            "Rebuild omitted selected source files"
        );
        let mut new_edges = BTreeMap::new();
        for edge in replacement_edges {
            validate_key_component("source_id", &edge.source_id)?;
            validate_key_component("target_id", &edge.target_id)?;
            anyhow::ensure!(
                edge.namespace == engram_core::namespaces::NAMESPACE_MEMORY
                    && extracted_kind(&edge.edge_kind),
                "Replacement contains a non-source edge"
            );
            let key = edge_key(project, &edge.edge_kind, &edge.source_id, &edge.target_id);
            anyhow::ensure!(
                new_edges.insert(key, edge.clone()).is_none(),
                "Duplicate replacement edge; resolve the staged graph first"
            );
        }
        let tx = self.db.begin_write()?;
        let mut report = SourceGraphReplacement::default();
        {
            let prefix = format!("{project}\0");
            let mut nodes = tx.open_table(NODES)?;
            let mut old_source = HashMap::new();
            let mut retained_node_ids = HashSet::new();
            let mut old_identities: HashMap<_, usize> = HashMap::new();
            let mut actual_files = BTreeMap::new();
            for row in nodes.range(prefix.as_str()..)? {
                let (key, value) = row?;
                if !key.value().starts_with(&prefix) {
                    break;
                }
                let node: Node = bincode::deserialize(value.value())?;
                if extracted_node(&node) {
                    if node.node_type == "file" {
                        actual_files.insert(node.node_id.clone(), value.value().to_vec());
                    }
                    *old_identities.entry(identity(&node)).or_default() += 1;
                    old_source.insert(node.node_id.clone(), node);
                } else {
                    retained_node_ids.insert(node.node_id);
                }
            }
            anyhow::ensure!(
                actual_files == expected,
                "Source graph changed during extraction; retry graph rebuild"
            );
            anyhow::ensure!(
                new_nodes.keys().all(|id| !retained_node_ids.contains(*id)),
                "Replacement would overwrite a retained non-source node"
            );
            let remap = |id: &str| -> (String, bool) {
                if new_nodes.contains_key(id) {
                    return (id.into(), false);
                }
                let Some(old) = old_source.get(id) else {
                    return (id.into(), false);
                };
                let key = identity(old);
                if old_identities.get(&key) == Some(&1)
                    && let Some(candidates) = new_identities.get(&key)
                    && candidates.len() == 1
                {
                    return (candidates[0].node_id.clone(), false);
                }
                (id.into(), true)
            };
            let mut edges = tx.open_table(EDGES)?;
            let mut outgoing = tx.open_table(ADJ_OUT)?;
            let mut incoming = tx.open_table(ADJ_IN)?;
            let mut remove_edges = Vec::new();
            // Only replaced/remapped rows are copied. Large temporal histories
            // with unchanged endpoints remain in place throughout the rebuild.
            for row in edges.range(prefix.as_str()..)? {
                let (key, value) = row?;
                if !key.value().starts_with(&prefix) {
                    break;
                }
                let edge: Edge = bincode::deserialize(value.value())?;
                let source_derived = edge.namespace == engram_core::namespaces::NAMESPACE_MEMORY
                    && extracted_kind(&edge.edge_kind)
                    && extracted_address(&edge.source_id)
                    // Learning's file-to-document links remain learning even
                    // when a damaged graph is missing the document node.
                    && (edge.edge_kind != EdgeKind::Dependency
                        || extracted_address(&edge.target_id)
                        || edge.target_id.starts_with("::"))
                    && !retained_node_ids.contains(&edge.source_id)
                    && !retained_node_ids.contains(&edge.target_id);
                if source_derived {
                    remove_edges.push(edge);
                    report.removed_edges += 1;
                    continue;
                }
                let (source, missing_source) = remap(&edge.source_id);
                let (target, missing_target) = remap(&edge.target_id);
                if missing_source || missing_target {
                    report.retained_edges_with_retired_endpoints += 1;
                }
                if source != edge.source_id || target != edge.target_id {
                    let mut rebound = edge.clone();
                    rebound.source_id = source;
                    rebound.target_id = target;
                    let new_key = edge_key(
                        project,
                        &rebound.edge_kind,
                        &rebound.source_id,
                        &rebound.target_id,
                    );
                    anyhow::ensure!(
                        new_edges.insert(new_key, rebound).is_none(),
                        "Successor remap collides with another relationship; graph unchanged"
                    );
                    remove_edges.push(edge);
                    report.remapped_edges += 1;
                }
            }
            for id in old_source.keys() {
                if !new_nodes.contains_key(id.as_str()) {
                    nodes.remove(format!("{prefix}{id}").as_str())?;
                    report.removed_nodes += 1;
                }
            }
            // Fresh extraction is authoritative, including absent metadata.
            for node in replacements {
                let bytes = bincode::serialize(node)?;
                nodes.insert(
                    format!("{prefix}{}", node.node_id).as_str(),
                    bytes.as_slice(),
                )?;
            }
            for edge in remove_edges {
                edges.remove(
                    edge_key(project, &edge.edge_kind, &edge.source_id, &edge.target_id).as_str(),
                )?;
                outgoing.remove((
                    adj_key(project, &edge.edge_kind, &edge.source_id).as_str(),
                    edge.target_id.as_str(),
                ))?;
                incoming.remove((
                    adj_key(project, &edge.edge_kind, &edge.target_id).as_str(),
                    edge.source_id.as_str(),
                ))?;
            }
            for (key, edge) in new_edges {
                // Unchanged historical rows stay in the table. Check here,
                // after removals, rather than copying their entire key set.
                // Any failure rolls back nodes, edges and both mirrors.
                anyhow::ensure!(
                    edges.get(key.as_str())?.is_none(),
                    "Replacement collides with retained evidence; graph unchanged"
                );
                let bytes = bincode::serialize(&edge)?;
                edges.insert(key.as_str(), bytes.as_slice())?;
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
            // Source generation stays the same, so generation comparison alone
            // cannot invalidate this derived ranking.
            tx.open_table(CENTRALITY)?
                .remove(format!("{prefix}current_v1").as_str())?;
        }
        tx.commit()?;
        Ok(report)
    }
}

#[cfg(test)]
mod tests;
