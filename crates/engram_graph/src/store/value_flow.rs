//! Persist authored value dependencies separately from the declaration graph.
use super::*;
use engram_core::value_flow::{FlowProgram, METADATA_KEY};
use serde_json::json;
use std::collections::BTreeMap;

mod local;
#[cfg(test)]
mod tests;
mod view;

fn field<'a>(node: &'a Node, key: &str) -> Option<&'a str> {
    node.metadata.as_ref()?.get(key)?.as_str()
}

fn language(name: &str) -> &str {
    match name {
        "csharp" => "cs",
        "vbnet" => "vb",
        _ => name,
    }
}

fn callable_name(node: &Node) -> &str {
    field(node, "fqn").unwrap_or(&node.name)
}

// No overload/type inference: every authored argument must have one distinct
// formal position. Optional omitted values are left without an incoming edge.
fn argument_mapping(
    call: &local::CallSite,
    target: &local::LocalFlow,
    semantic: bool,
) -> Option<Vec<usize>> {
    let parameters = &target.program.parameters;
    if parameters.iter().any(|p| p.variadic) {
        return None;
    }
    let mut used = std::collections::BTreeSet::new();
    let mut mapping = Vec::new();
    for (position, argument) in call.expression.arguments.iter().enumerate() {
        let named = argument.name.as_ref().and_then(|name| {
            parameters.iter().position(|p| {
                local::canonical(&p.name, &target.owner.language)
                    == local::canonical(name, &target.owner.language)
            })
        });
        if argument.name.is_some() && named.is_none() {
            return None;
        }
        let index = if semantic {
            argument.parameter_index? as usize
        } else {
            named.unwrap_or(position)
        };
        let parameter = parameters.get(index)?;
        if parameter.index as usize != index
            || named.is_some_and(|named| named != index)
            || parameter.by_ref != argument.by_ref
            || !used.insert(index)
        {
            return None;
        }
        mapping.push(index);
    }
    if parameters
        .iter()
        .enumerate()
        .any(|(index, p)| !p.optional && !used.contains(&index))
    {
        return None;
    }
    Some(mapping)
}

fn bind_call(
    call: &local::CallSite,
    caller: &local::LocalFlow,
    flows: &BTreeMap<String, local::LocalFlow>,
    declaration_counts: &BTreeMap<(String, String), usize>,
    files: &BTreeMap<String, Node>,
    calls: &[Edge],
) -> Option<(String, Vec<usize>, &'static str)> {
    let mut candidates = Vec::new();
    for edge in calls.iter().filter(|e| e.source_id == caller.owner.node_id) {
        let Some(target) = flows.get(&edge.target_id) else {
            continue;
        };
        if language(&target.owner.language) != language(&caller.owner.language) {
            continue;
        }
        let semantic = call.expression.target_semantic_id.as_deref();
        let binding = if matches!(caller.owner.language.as_str(), "vb" | "vbnet") {
            if semantic.is_none() || semantic != field(&target.owner, "semantic_member_id") {
                continue;
            }
            let source_file = files.get(caller.owner.file_path.as_str())?;
            let target_file = files.get(target.owner.file_path.as_str())?;
            if !source_call_binding_matches(
                edge,
                &caller.owner,
                source_file,
                &target.owner,
                target_file,
            ) {
                continue;
            }
            let proofs = source_call_bindings(edge).ok().flatten()?;
            if !proofs
                .bindings
                .iter()
                .any(|proof| proof.reference_lines.contains(&call.expression.site.line))
            {
                continue;
            }
            "admitted_compiler_selected_source_call"
        } else if language(&caller.owner.language) == "cs" {
            let written = call.expression.name.as_deref()?;
            let simple = written.strip_prefix("this.").unwrap_or(written);
            // Receiver variables and generic invocations require type analysis
            // beyond this local pass. Do not borrow a name-only Calls target.
            if simple.contains(['.', '<', '>']) {
                continue;
            }
            let caller_scope = callable_name(&caller.owner).rsplit_once('.')?.0;
            let expected = format!("{caller_scope}.{simple}");
            if callable_name(&target.owner) != expected {
                continue;
            }
            if declaration_counts.get(&("cs".to_string(), expected.clone())) != Some(&1) {
                continue;
            }
            "unique_lexical_callable_candidate"
        } else {
            continue;
        };
        if let Some(mapping) = argument_mapping(call, target, semantic.is_some()) {
            candidates.push((target.owner.node_id.clone(), mapping, binding));
        }
    }
    candidates.sort();
    candidates.dedup();
    if candidates.len() == 1 {
        candidates.pop()
    } else {
        None
    }
}

impl GraphStore {
    pub(super) fn resolve_value_flows(&self, project: &str) -> anyhow::Result<usize> {
        let wtx = self.db.begin_write()?;
        let mut declarations = BTreeMap::new();
        let mut files = BTreeMap::new();
        let mut old_nodes = BTreeMap::new();
        let mut old_edges = BTreeMap::new();
        let mut calls: BTreeMap<String, Vec<Edge>> = BTreeMap::new();
        let prefix = format!("{project}\0");
        {
            let table = wtx.open_table(NODES)?;
            for row in table.range(prefix.as_str()..)? {
                let (key, value) = row?;
                if !key.value().starts_with(&prefix) {
                    break;
                }
                let node: Node = bincode::deserialize(value.value())?;
                if node.namespace != engram_core::namespaces::NAMESPACE_MEMORY {
                    continue;
                }
                match node.node_type.as_str() {
                    "function" => {
                        declarations.insert(node.node_id.clone(), node);
                    }
                    "file" => {
                        files.insert(node.file_path.as_str().to_string(), node);
                    }
                    "value_point" if field(&node, "producer") == Some(METADATA_KEY) => {
                        old_nodes.insert(key.value().to_string(), node);
                    }
                    _ => {}
                }
            }
            let table = wtx.open_table(EDGES)?;
            for kind in [EdgeKind::Calls, EdgeKind::ValueFlow] {
                let prefix = format!("{project}\0{}\0", kind.as_str());
                for row in table.range(prefix.as_str()..)? {
                    let (key, value) = row?;
                    if !key.value().starts_with(&prefix) {
                        break;
                    }
                    let edge: Edge = bincode::deserialize(value.value())?;
                    if kind == EdgeKind::Calls {
                        if edge.namespace == engram_core::namespaces::NAMESPACE_MEMORY {
                            calls.entry(edge.source_id.clone()).or_default().push(edge);
                        }
                    } else {
                        old_edges.insert(key.value().to_string(), edge);
                    }
                }
            }
        }
        let mut flows = BTreeMap::new();
        let mut declaration_counts = BTreeMap::new();
        for node in declarations.values() {
            *declaration_counts
                .entry((
                    language(&node.language).to_string(),
                    callable_name(node).to_string(),
                ))
                .or_insert(0) += 1;
        }
        for owner in declarations.values() {
            let Some(raw) = field(owner, METADATA_KEY) else {
                continue;
            };
            let program = serde_json::from_str::<FlowProgram>(raw).ok().filter(|p| {
                p.version == 1
                    && language(&p.language) == language(&owner.language)
                    && p.parameters.len() <= 4096
            });
            let program = program.unwrap_or_else(|| FlowProgram {
                language: owner.language.clone(),
                issues: vec![
                    "Invalid or unsupported authored value syntax; lineage unavailable.".into(),
                ],
                ..Default::default()
            });
            flows.insert(
                owner.node_id.clone(),
                local::LocalFlow::build(owner.clone(), program),
            );
        }
        // Decide every link against the same declaration/call snapshot before
        // mutating a flow. Each cross-method edge carries its invocation ID.
        let mut links = Vec::new();
        for (id, flow) in &flows {
            for (index, call) in flow.calls.iter().enumerate() {
                links.push((
                    id.clone(),
                    index,
                    bind_call(
                        call,
                        flow,
                        &flows,
                        &declaration_counts,
                        &files,
                        calls.get(id).map(Vec::as_slice).unwrap_or(&[]),
                    ),
                ));
            }
        }
        for (owner, index, binding) in links {
            let target = binding.as_ref().map(|(id, _, _)| {
                let target = &flows[id];
                (target.parameters.clone(), target.returns.clone())
            });
            let flow = flows.get_mut(&owner).expect("owner retained in snapshot");
            let call = &flow.calls[index];
            let result = call.result.clone();
            let arguments = call.arguments.clone();
            let (callee, evidence) = if let Some((target_id, mapping, binding)) = binding {
                let (parameters, returns) = target.expect("bound target retained");
                for (argument, parameter) in arguments.iter().zip(mapping) {
                    flow.link(
                        argument,
                        &parameters[parameter],
                        "argument_parameter",
                        Some(&result),
                        Some(binding),
                    );
                }
                for returned in returns {
                    flow.link(
                        &returned,
                        &result,
                        "return_value",
                        Some(&result),
                        Some(binding),
                    );
                }
                (Some(target_id), binding)
            } else {
                flow.issue("Call target or argument mapping is unbound; no argument-to-result dependency is inferred.");
                (None, "unbound")
            };
            if let Some(node) = flow.nodes.iter_mut().find(|node| node.node_id == result) {
                let metadata = node.metadata.as_mut().expect("value metadata");
                metadata["callee_id"] = json!(callee);
                metadata["binding"] = json!(evidence);
            }
        }
        let mut nodes = BTreeMap::new();
        let mut edges = BTreeMap::new();
        for mut flow in flows.into_values() {
            flow.finish();
            for node in flow.nodes {
                nodes.insert(format!("{project}\0{}", node.node_id), node);
            }
            for mut edge in flow.edges {
                let key = edge_key(project, &edge.edge_kind, &edge.source_id, &edge.target_id);
                edge.updated_at_ms = old_edges
                    .get(&key)
                    .map_or_else(now_ms, |old| old.updated_at_ms);
                edges.insert(key, edge);
            }
        }
        fn same<T: Serialize>(a: &BTreeMap<String, T>, b: &BTreeMap<String, T>) -> bool {
            a.len() == b.len()
                && a.iter().all(|(key, value)| {
                    b.get(key).is_some_and(|old| {
                        bincode::serialize(value).ok() == bincode::serialize(old).ok()
                    })
                })
        }
        if same(&nodes, &old_nodes) && same(&edges, &old_edges) {
            return Ok(0);
        }
        {
            let mut table = wtx.open_table(NODES)?;
            for key in old_nodes.keys() {
                table.remove(key.as_str())?;
            }
            for (key, node) in &nodes {
                let bytes = bincode::serialize(node)?;
                table.insert(key.as_str(), bytes.as_slice())?;
            }
            let mut table = wtx.open_table(EDGES)?;
            let mut outgoing = wtx.open_table(ADJ_OUT)?;
            let mut incoming = wtx.open_table(ADJ_IN)?;
            for (key, edge) in &old_edges {
                table.remove(key.as_str())?;
                outgoing.remove((
                    adj_key(project, &edge.edge_kind, &edge.source_id).as_str(),
                    edge.target_id.as_str(),
                ))?;
                incoming.remove((
                    adj_key(project, &edge.edge_kind, &edge.target_id).as_str(),
                    edge.source_id.as_str(),
                ))?;
            }
            for (key, edge) in &edges {
                let bytes = bincode::serialize(edge)?;
                table.insert(key.as_str(), bytes.as_slice())?;
                let bytes = encode_adj_value(edge.weight, edge.updated_at_ms);
                outgoing.insert(
                    (
                        adj_key(project, &edge.edge_kind, &edge.source_id).as_str(),
                        edge.target_id.as_str(),
                    ),
                    bytes.as_slice(),
                )?;
                incoming.insert(
                    (
                        adj_key(project, &edge.edge_kind, &edge.target_id).as_str(),
                        edge.source_id.as_str(),
                    ),
                    bytes.as_slice(),
                )?;
            }
        }
        let changed = nodes.len()
            + edges.len()
            + old_nodes.keys().filter(|k| !nodes.contains_key(*k)).count()
            + old_edges.keys().filter(|k| !edges.contains_key(*k)).count();
        wtx.commit()?;
        Ok(changed)
    }
}
