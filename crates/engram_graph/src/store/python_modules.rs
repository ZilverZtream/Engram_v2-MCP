//! Python imports resolve only against retained indexed source identities.
use super::*;
use engram_core::python_modules::{CALL_KEY, Call, MODULE_KEY, Module, UNBOUND_PREFIX};
use std::collections::{BTreeMap, BTreeSet};
const BINDINGS: &str = "python_module_bindings_v1";
const PRESERVED: &str = "python_preserved_call_edge_v1";
struct Resolver {
    modules: BTreeMap<String, (Node, Module)>,
    nodes: BTreeMap<String, Node>,
}
fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |p| p.0)
}
fn child(base: &str, name: &str) -> String {
    if base.is_empty() {
        name.into()
    } else {
        format!("{base}/{name}")
    }
}
impl Resolver {
    fn module_at(&self, base: &str) -> Result<String, &'static str> {
        let package = child(base, "__init__.py");
        if self.modules.contains_key(&package) {
            return Ok(package);
        }
        let file = format!("{base}.py");
        if self.modules.contains_key(&file) {
            return Ok(file);
        }
        if self
            .modules
            .keys()
            .any(|p| p.starts_with(&format!("{base}/")))
        {
            return Ok(base.into());
        }
        Err("module_not_indexed")
    }
    fn absolute(&self, root: &str, parts: &[&str]) -> Result<String, &'static str> {
        let mut base = root.to_string();
        for (i, part) in parts.iter().enumerate() {
            if part.is_empty() || !part.chars().all(|c| c == '_' || c.is_alphanumeric()) {
                return Err("invalid_module_name");
            }
            base = child(&base, part);
            let found = self.module_at(&base)?;
            if i + 1 == parts.len() {
                return Ok(found);
            }
            if found.ends_with(".py") && !found.ends_with("/__init__.py") {
                return Err("module_not_package");
            }
        }
        self.module_at(&base)
    }
    fn module_path(&self, from: &str, module: &str) -> Result<String, &'static str> {
        let dots = module.chars().take_while(|c| *c == '.').count();
        let rest = &module[dots..];
        if dots > 0 {
            let mut base = parent(from);
            if base.is_empty() {
                return Err("relative_import_without_package");
            }
            for _ in 1..dots {
                base = parent(base);
                if base.is_empty() || base == "src" {
                    return Err("relative_import_outside_package");
                }
            }
            return if rest.is_empty() {
                self.module_at(base)
            } else {
                self.absolute(base, &rest.split('.').collect::<Vec<_>>())
            };
        }
        let parts: Vec<_> = module.split('.').collect();
        let mut candidates = BTreeSet::new();
        // Both layouts are source candidates, not guesses at a runtime path
        // ordering. Conflicting root/src identities stay explicitly ambiguous.
        for root in ["", "src"] {
            if let Ok(found) = self.absolute(root, &parts) {
                candidates.insert(found);
            }
        }
        match candidates.len() {
            0 => Err("module_not_indexed"),
            1 => Ok(candidates.into_iter().next().unwrap()),
            _ => Err("ambiguous_module_root"),
        }
    }
    fn resolve_call(&self, from: &str, call: &Call) -> Result<String, &'static str> {
        if call.module == "<local>" {
            let mut parts = call.imported.splitn(3, ':');
            let kind = parts.next().ok_or("invalid_local_identity")?;
            let line = parts
                .next()
                .and_then(|s| s.parse::<u32>().ok())
                .ok_or("invalid_local_identity")?;
            return self.declaration(
                from,
                parts.next().ok_or("invalid_local_identity")?,
                kind,
                line,
            );
        }
        let path = self.module_path(from, &call.module)?;
        self.attribute(&path, &call.imported, &mut BTreeSet::new(), &mut 4096)
    }
    fn declaration(
        &self,
        path: &str,
        name: &str,
        kind: &str,
        line: u32,
    ) -> Result<String, &'static str> {
        let id = engram_core::NodeId::symbol(kind, None, path, name, line).0;
        let node = self.nodes.get(&id).ok_or("declaration_not_indexed")?;
        if node.node_type != kind || node.file_path.as_str() != path || node.start_line != line {
            return Err("declaration_identity_mismatch");
        }
        Ok(id)
    }
    fn attribute(
        &self,
        path: &str,
        name: &str,
        visiting: &mut BTreeSet<(String, String)>,
        budget: &mut usize,
    ) -> Result<String, &'static str> {
        if *budget == 0 || visiting.len() >= 64 {
            return Err("import_resolution_budget");
        }
        *budget -= 1;
        let key = (path.to_string(), name.to_string());
        if !visiting.insert(key.clone()) {
            return Err("cyclic_import");
        }
        let result = self.attribute_inner(path, name, visiting, budget);
        visiting.remove(&key);
        result
    }
    fn attribute_inner(
        &self,
        path: &str,
        name: &str,
        visiting: &mut BTreeSet<(String, String)>,
        budget: &mut usize,
    ) -> Result<String, &'static str> {
        let (head, tail) = name.split_once('.').unwrap_or((name, ""));
        if let Some((_, facts)) = self.modules.get(path) {
            if !facts.issues.is_empty() {
                return Err("incomplete_module_syntax");
            }
            let bindings: Vec<_> = facts
                .bindings
                .iter()
                .filter(|b| b.scope == 0 && b.local == head)
                .collect();
            if bindings.len() > 1 {
                return Err("ambiguous_or_shadowed_binding");
            }
            if let Some(binding) = bindings.first() {
                if let Some(reason) = &binding.reason {
                    return Err(match reason.as_str() {
                        "conditional_import" => "conditional_import",
                        "decorated_callable" => "decorated_callable",
                        _ => "conditional_or_unsupported_binding",
                    });
                }
                if binding.kind == "import" {
                    let module = self.module_path(path, &binding.module)?;
                    let imported = if binding.imported == "*" {
                        let suffix = name
                            .strip_prefix(&binding.access)
                            .ok_or("unsupported_module_attribute")?
                            .trim_start_matches('.');
                        if suffix.is_empty() {
                            return Err("module_not_callable");
                        }
                        suffix.to_string()
                    } else if tail.is_empty() {
                        binding.imported.clone()
                    } else {
                        format!("{}.{tail}", binding.imported)
                    };
                    return self.attribute(&module, &imported, visiting, budget);
                }
                if matches!(binding.kind.as_str(), "function" | "class") && tail.is_empty() {
                    return self.declaration(
                        path,
                        &binding.local,
                        &binding.kind,
                        binding.site.line,
                    );
                }
                return Err("shadowed_or_nonmodule_binding");
            }
            if facts
                .bindings
                .iter()
                .any(|b| b.scope == 0 && b.kind == "wildcard")
            {
                return Err("wildcard_import");
            }
        }
        // `from pkg import child; child.save()` can name a real submodule,
        // including namespace packages without an __init__.py.
        if !tail.is_empty() && (!path.ends_with(".py") || path.ends_with("/__init__.py")) {
            let base = if path.ends_with("/__init__.py") {
                parent(path)
            } else {
                path
            };
            let module = self.module_at(&child(base, head))?;
            return self.attribute(&module, tail, visiting, budget);
        }
        Err("name_not_exported")
    }
}

impl GraphStore {
    pub(super) fn resolve_python_modules(&self, project: &str) -> anyhow::Result<usize> {
        let wtx = self.db.begin_write()?;
        let mut resolver = Resolver {
            modules: BTreeMap::new(),
            nodes: BTreeMap::new(),
        };
        let mut previous = BTreeMap::new();
        {
            let table = wtx.open_table(NODES)?;
            let prefix = format!("{project}\0");
            for row in table.range(prefix.as_str()..)? {
                let (key, value) = row?;
                if !key.value().starts_with(&prefix) {
                    break;
                }
                let node: Node = bincode::deserialize(value.value())?;
                if node.namespace != engram_core::namespaces::NAMESPACE_MEMORY {
                    continue;
                }
                if node.node_type == "file" {
                    if let Some(raw) = node
                        .metadata
                        .as_ref()
                        .and_then(|m| m.get(MODULE_KEY))
                        .and_then(|v| v.as_str())
                    {
                        if let Ok(facts) = serde_json::from_str::<Module>(raw) {
                            if facts.version == 1 {
                                resolver
                                    .modules
                                    .insert(node.file_path.as_str().into(), (node.clone(), facts));
                            }
                        }
                    }
                }
                resolver.nodes.insert(node.node_id.clone(), node);
            }
            let table = wtx.open_table(EDGES)?;
            let prefix = format!("{project}\0calls\0");
            for row in table.range(prefix.as_str()..)? {
                let (key, value) = row?;
                if !key.value().starts_with(&prefix) {
                    break;
                }
                let edge: Edge = bincode::deserialize(value.value())?;
                if edge.namespace == engram_core::namespaces::NAMESPACE_MEMORY {
                    previous.insert(key.value().to_string(), edge);
                }
            }
        }
        let owned = |edge: &Edge| {
            edge.metadata
                .as_ref()
                .is_some_and(|m| m.get(CALL_KEY).is_some() || m.get(BINDINGS).is_some())
        };
        let old: BTreeMap<_, _> = previous
            .iter()
            .filter(|(_, e)| owned(e))
            .map(|(k, e)| (k.clone(), e.clone()))
            .collect();
        let mut desired: BTreeMap<String, Edge> = BTreeMap::new();
        let mut ordinary: BTreeMap<String, Edge> = previous
            .iter()
            .filter(|(_, e)| !owned(e))
            .map(|(k, e)| (k.clone(), e.clone()))
            .collect();
        for (key, edge) in &old {
            if let Some(raw) = edge
                .metadata
                .as_ref()
                .and_then(|m| m.get(PRESERVED))
                .and_then(|v| v.as_str())
            {
                let original: Edge = serde_json::from_str(raw)?;
                anyhow::ensure!(
                    edge_key(
                        project,
                        &original.edge_kind,
                        &original.source_id,
                        &original.target_id
                    ) == *key
                        && !owned(&original),
                    "Invalid preserved ordinary call evidence"
                );
                ordinary.insert(key.clone(), original.clone());
                desired.insert(key.clone(), original);
            }
        }
        for (path, (file, facts)) in &resolver.modules {
            for owned_call in &facts.calls {
                let call = &owned_call.call;
                let source = if owned_call.source_kind == "file" {
                    file.node_id.clone()
                } else {
                    engram_core::NodeId::symbol(
                        &owned_call.source_kind,
                        None,
                        path,
                        &owned_call.source_name,
                        owned_call.source_line,
                    )
                    .0
                };
                // Class-qualified FQNs are producer hints; canonical nodes use
                // their short declaration name. Locate by file/kind/line/FQN.
                let source = if resolver.nodes.contains_key(&source) {
                    source
                } else {
                    let candidates: Vec<_> = resolver
                        .nodes
                        .values()
                        .filter(|n| {
                            n.file_path.as_str() == path
                                && n.node_type == owned_call.source_kind
                                && n.start_line == owned_call.source_line
                                && n.metadata
                                    .as_ref()
                                    .and_then(|m| m.get("fqn"))
                                    .and_then(|v| v.as_str())
                                    == Some(&owned_call.source_name)
                        })
                        .collect();
                    if candidates.len() != 1 {
                        continue;
                    }
                    candidates[0].node_id.clone()
                };
                let resolution = if let Some(reason) = &call.unbound_reason {
                    Err(reason.as_str())
                } else {
                    resolver.resolve_call(path, call)
                };
                let (target, reason) = match resolution {
                    Ok(target) => (target, None),
                    Err(reason) => (
                        format!("{UNBOUND_PREFIX}{path}/{}", call.site.offset),
                        Some(reason),
                    ),
                };
                let key = edge_key(project, &EdgeKind::Calls, &source, &target);
                let evidence = serde_json::json!({"source_file": path, "call": call,
                    "binding": if reason.is_none() {"syntax_module_export"} else {"unbound"}, "reason": reason,
                    "target_file": resolver.nodes.get(&target).map(|n| n.file_path.as_str()),
                    "target_line": resolver.nodes.get(&target).map(|n| n.start_line)});
                // One graph key can contain both a local call and self-import
                // evidence. Save and restore the independent ordinary edge
                // when the imported export later disappears.
                let edge = desired.entry(key.clone()).or_insert_with(|| ordinary.get(&key).cloned().unwrap_or_else(|| Edge {
                    source_id: source, target_id: target, namespace: file.namespace.clone(), language: file.language.clone(),
                    edge_kind: EdgeKind::Calls, weight: 1, generation: file.generation,
                    updated_at_ms: previous.get(&key).map_or_else(now_ms, |e| e.updated_at_ms),
                    metadata: Some(serde_json::json!({"resolution": if reason.is_none() {"syntax_module_export"} else {"unbound"},
                        "src_line": owned_call.source_line.to_string(),
                        "call_site_line": call.site.line.to_string(), "call_site_lines": [], BINDINGS: []})),
                }));
                let metadata = edge.metadata.get_or_insert_with(|| serde_json::json!({}));
                if let Some(original) = ordinary.get(&key) {
                    metadata[PRESERVED] = serde_json::json!(serde_json::to_string(original)?);
                }
                if metadata.get(BINDINGS).is_none() {
                    metadata[BINDINGS] = serde_json::json!([]);
                }
                if !metadata
                    .get("call_site_lines")
                    .is_some_and(|v| v.is_array())
                {
                    metadata["call_site_lines"] = serde_json::json!([]);
                }
                metadata[BINDINGS].as_array_mut().unwrap().push(evidence);
                let lines = metadata["call_site_lines"].as_array_mut().unwrap();
                let line = serde_json::json!(call.site.line);
                if !lines.contains(&line) {
                    lines.push(line);
                }
            }
        }
        if old.len() == desired.len()
            && desired.iter().all(|(k, e)| {
                old.get(k)
                    .is_some_and(|o| bincode::serialize(e).ok() == bincode::serialize(o).ok())
            })
        {
            return Ok(0);
        }
        {
            let mut table = wtx.open_table(EDGES)?;
            let mut outgoing = wtx.open_table(ADJ_OUT)?;
            let mut incoming = wtx.open_table(ADJ_IN)?;
            for (key, edge) in &old {
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
            for (key, edge) in &desired {
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
        let changed = desired.len() + old.keys().filter(|k| !desired.contains_key(*k)).count();
        wtx.commit()?;
        Ok(changed)
    }
}
