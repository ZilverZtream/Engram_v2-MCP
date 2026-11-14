//! Recompute module-owned Calls from retained source facts. Target deletion or
//! export edits cannot erase the caller evidence needed by the next pass.
use super::*;
use engram_core::typescript_modules::{
    CALL_KEY, CONFIG_KEY, Config, MODULE_KEY, Module, UNBOUND_PREFIX,
};
use std::collections::{BTreeMap, BTreeSet};

const BINDINGS: &str = "typescript_module_bindings_v1";
const PRESERVED: &str = "typescript_preserved_call_edge_v1";
mod config;
mod state;

struct Resolver {
    modules: BTreeMap<String, (Node, Module)>,
    nodes: BTreeMap<String, Node>,
    configs: BTreeMap<String, Option<Config>>,
    state_category: Option<String>,
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |p| p.0)
}

fn join(base: &str, path: &str) -> Option<String> {
    if path.starts_with(['/', '\\']) || path.contains([':', '\0', '\\']) {
        return None;
    }
    let mut parts: Vec<_> = base.split('/').filter(|p| !p.is_empty()).collect();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            _ => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

impl Resolver {
    fn module_path(&self, from: &str, specifier: &str) -> Result<String, &'static str> {
        if specifier.starts_with("./") || specifier.starts_with("../") {
            let base = join(parent(from), specifier).ok_or("module_outside_project")?;
            if self.state_category.is_some() && (from.ends_with(".js") || from.ends_with(".jsx"))
                && (base.ends_with(".js") || base.ends_with(".jsx")) && self.modules.contains_key(&base) {
                return Ok(base);
            }
            return self.module_at(&base);
        }
        self.configured_module(from, specifier)
    }

    fn module_at(&self, base: &str) -> Result<String, &'static str> {
        // TypeScript extension substitution; package manifests and runtime
        // loader semantics are deliberately not inferred from a file name.
        let mut candidates = Vec::new();
        if let Some(stem) = base.strip_suffix(".jsx") {
            // JSX specifiers prefer the TSX sibling; JS specifiers prefer TS.
            // Sharing the order can bind to the wrong same-named export.
            candidates.extend([
                format!("{stem}.tsx"),
                format!("{stem}.ts"),
                format!("{stem}.d.ts"),
            ]);
        } else if let Some(stem) = base.strip_suffix(".js") {
            candidates.extend([
                format!("{stem}.ts"),
                format!("{stem}.tsx"),
                format!("{stem}.d.ts"),
            ]);
        } else if base.ends_with(".ts") || base.ends_with(".tsx") {
            candidates.push(base.to_string());
        } else {
            if !base.is_empty() {
                candidates.extend([
                    format!("{base}.ts"),
                    format!("{base}.tsx"),
                    format!("{base}.d.ts"),
                ]);
            }
            let prefix = if base.is_empty() {
                String::new()
            } else {
                format!("{base}/")
            };
            candidates.extend([
                format!("{prefix}index.ts"),
                format!("{prefix}index.tsx"),
                format!("{prefix}index.d.ts"),
            ]);
        }
        if self.state_category.is_some() {
            if base.ends_with(".js") || base.ends_with(".jsx") {
                candidates.push(base.to_owned());
            } else if !base.ends_with(".ts") && !base.ends_with(".tsx") {
                candidates.extend([format!("{base}.js"), format!("{base}.jsx"),
                    format!("{base}/index.js"), format!("{base}/index.jsx")]);
            }
        }
        candidates
            .into_iter()
            .find(|p| self.modules.contains_key(p))
            .ok_or("module_not_indexed")
    }

    fn export(
        &self,
        path: &str,
        name: &str,
        visiting: &mut BTreeSet<(String, String)>,
        budget: &mut usize,
    ) -> Result<String, &'static str> {
        if *budget == 0 || visiting.len() >= 64 {
            return Err("export_resolution_budget");
        }
        *budget -= 1;
        let key = (path.to_string(), name.to_string());
        if !visiting.insert(key.clone()) {
            return Err("cyclic_export");
        }
        let result = self.export_inner(path, name, visiting, budget);
        visiting.remove(&key);
        result
    }

    fn export_inner(
        &self,
        path: &str,
        name: &str,
        visiting: &mut BTreeSet<(String, String)>,
        budget: &mut usize,
    ) -> Result<String, &'static str> {
        let (_, facts) = self.modules.get(path).ok_or("module_not_indexed")?;
        if !facts.issues.is_empty() {
            return Err("incomplete_module_syntax");
        }
        let direct: Vec<_> = facts
            .exports
            .iter()
            .filter(|e| e.exported == name)
            .collect();
        let mut targets = BTreeSet::new();
        if !direct.is_empty() {
            for export in direct {
                if export.type_only {
                    return Err("type_only_export");
                }
                let target = match export.kind.as_str() {
                    "local" => self.local(path, &export.local, visiting, budget)?,
                    "reexport" => {
                        let module = self.module_path(
                            path,
                            export.module.as_deref().ok_or("missing_export_module")?,
                        )?;
                        self.export(&module, &export.local, visiting, budget)?
                    }
                    _ => return Err("unsupported_export_kind"),
                };
                targets.insert(target);
            }
        } else if name != "default" {
            for export in facts
                .exports
                .iter()
                .filter(|e| e.kind == "star" && !e.type_only)
            {
                let module = self.module_path(
                    path,
                    export.module.as_deref().ok_or("missing_export_module")?,
                )?;
                match self.export(&module, name, visiting, budget) {
                    Ok(target) => {
                        targets.insert(target);
                    }
                    Err("export_not_found") => {}
                    Err(reason) => return Err(reason),
                }
            }
        }
        match targets.len() {
            0 => Err("export_not_found"),
            1 => Ok(targets.into_iter().next().unwrap()),
            _ => Err("ambiguous_export"),
        }
    }

    fn local(
        &self,
        path: &str,
        name: &str,
        visiting: &mut BTreeSet<(String, String)>,
        budget: &mut usize,
    ) -> Result<String, &'static str> {
        let (_, facts) = self.modules.get(path).ok_or("module_not_indexed")?;
        let declarations: Vec<_> = facts
            .declarations
            .iter()
            .filter(|d| d.local == name)
            .collect();
        let imports: Vec<_> = facts.imports.iter().filter(|i| i.local == name).collect();
        if !declarations.is_empty() {
            if let Some(category) = &self.state_category {
                if declarations.len() != 1 || !imports.is_empty() {
                    return Err("ambiguous_state_declaration");
                }
                let categories: &[&str] = match category.as_str() {
                    "ModuleOrSingleton" => &["Module", "Singleton"],
                    "ReactContext" => &["ReactContext"],
                    "Redux" => &["Redux"],
                    _ => return Err("unsupported_state_category"),
                };
                let matches = categories.iter().filter_map(|category| {
                    let id = engram_core::NodeId::state(category, &format!("{path}::{name}")).0;
                    self.nodes.get(&id).filter(|node| node.node_type == "global_state"
                        && node.file_path.as_str() == path).map(|_| id)
                }).collect::<Vec<_>>();
                return if matches.len() == 1 { Ok(matches[0].clone()) }
                    else { Err("not_a_supported_state_declaration") };
            }
            if !imports.is_empty() || declarations.iter().any(|d| !d.callable) {
                return Err("ambiguous_or_noncallable_local");
            }
            let implementations: Vec<_> = declarations.iter().filter(|d| d.has_body).collect();
            if implementations.len() != 1 {
                return Err("missing_or_ambiguous_implementation");
            }
            let declaration = implementations[0];
            let id =
                engram_core::NodeId::symbol("function", None, path, name, declaration.site.line).0;
            let node = self.nodes.get(&id).ok_or("declaration_not_indexed")?;
            if node.node_type != "function"
                || node.file_path.as_str() != path
                || node.start_line != declaration.site.line
            {
                return Err("declaration_identity_mismatch");
            }
            return Ok(id);
        }
        if imports.len() != 1 {
            return Err("missing_or_ambiguous_local");
        }
        let import = imports[0];
        if import.type_only || import.imported == "*" {
            return Err("noncallable_import_export");
        }
        let module = self.module_path(path, &import.module)?;
        self.export(&module, &import.imported, visiting, budget)
    }
}

impl GraphStore {
    pub(super) fn resolve_typescript_modules(&self, project: &str) -> anyhow::Result<usize> {
        let wtx = self.db.begin_write()?;
        let mut resolver = Resolver {
            modules: BTreeMap::new(),
            nodes: BTreeMap::new(),
            configs: BTreeMap::new(),
            state_category: None,
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
                    if node
                        .file_path
                        .as_str()
                        .to_ascii_lowercase()
                        .ends_with(".json")
                    {
                        let facts = node
                            .metadata
                            .as_ref()
                            .and_then(|m| m.get(CONFIG_KEY))
                            .and_then(|v| v.as_str())
                            .and_then(|raw| serde_json::from_str::<Config>(raw).ok())
                            .filter(|c| c.version == 1);
                        resolver
                            .configs
                            .insert(node.file_path.as_str().into(), facts);
                    }
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
                    resolver.module_path(path, &call.module).and_then(|module| {
                        resolver.export(&module, &call.imported, &mut BTreeSet::new(), &mut 4096)
                    })
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
        let mut changed = 0;
        {
            let mut table = wtx.open_table(EDGES)?;
            let mut outgoing = wtx.open_table(ADJ_OUT)?;
            let mut incoming = wtx.open_table(ADJ_IN)?;
            for (key, edge) in &old {
                if let Some(next) = desired.get(key)
                    && bincode::serialize(edge)? == bincode::serialize(next)? {
                    continue;
                }
                if !desired.contains_key(key) { changed += 1; }
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
                if let Some(previous) = old.get(key)
                    && bincode::serialize(previous)? == bytes {
                    continue;
                }
                changed += 1;
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
        wtx.commit()?;
        Ok(changed)
    }
}
