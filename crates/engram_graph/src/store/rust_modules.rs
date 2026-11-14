//! Resolve Rust imports against declared source modules and Cargo identities.
use super::*;
use engram_core::rust_modules::{
    Binding, CALL_KEY, CONFIG_KEY, Call, MODULE_KEY, Manifest, Module, UNBOUND_PREFIX,
};
use std::collections::{BTreeMap, BTreeSet};
mod contexts;
const BINDINGS: &str = "rust_module_bindings_v1";
const PRESERVED: &str = "rust_preserved_call_edge_v1";
type Key = (usize, Vec<String>);
#[derive(Clone)]
struct Root {
    file: String,
    manifest: Option<String>,
    name: String,
    kind: String,
    edition: String,
    reason: Option<&'static str>,
}
#[derive(Clone)]
struct Context {
    key: Key,
    file: String,
    inline: Vec<String>,
    directory: String,
    reason: Option<&'static str>,
}
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Found {
    Module(Key),
    Function(String),
}
struct Resolver {
    modules: BTreeMap<String, (Node, Module)>,
    nodes: BTreeMap<String, Node>,
    manifests: BTreeMap<String, Manifest>,
    roots: Vec<Root>,
    contexts: BTreeMap<Key, Context>,
    origins: BTreeMap<(String, Vec<String>), Vec<Key>>,
}
fn binding_reason(reason: &str) -> &'static str {
    match reason {
        "conditional_compilation" => "conditional_compilation",
        "unexpanded_attribute" => "unexpanded_attribute",
        "unsupported_module_path_attribute" => "unsupported_module_path_attribute",
        _ => "unsupported_binding",
    }
}
impl Resolver {
    fn declaration(&self, file: &str, name: &str, line: u32) -> Result<String, &'static str> {
        let id = engram_core::NodeId::symbol("function", None, file, name, line).0;
        let node = self.nodes.get(&id).ok_or("declaration_not_indexed")?;
        if node.file_path.as_str() != file
            || node.start_line != line
            || node.node_type != "function"
        {
            return Err("declaration_identity_mismatch");
        }
        Ok(id)
    }
    fn resolve_call(&self, file: &str, call: &Call) -> Result<String, &'static str> {
        if let Some((name, line)) = &call.local_target {
            return self.declaration(file, name, *line);
        }
        let origins = self
            .origins
            .get(&(file.into(), call.context.clone()))
            .ok_or("crate_context_not_indexed")?;
        let mut outcomes = BTreeSet::new();
        let mut failure = None;
        for origin in origins {
            let context = self
                .contexts
                .get(origin)
                .ok_or("module_context_not_indexed")?;
            let mut path = if call.module.is_empty() {
                vec![]
            } else {
                call.module
                    .split("::")
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            };
            path.push(call.imported.clone());
            let mut visiting = BTreeSet::new();
            let mut budget = 4096;
            let result = if call.glob_paths.is_empty() {
                self.resolve(
                    context,
                    &path,
                    context,
                    call.path_from_use,
                    &mut visiting,
                    &mut budget,
                )
            } else {
                let mut targets = BTreeSet::new();
                let mut error = None;
                for glob in &call.glob_paths {
                    match self
                        .resolve(context, glob, context, true, &mut visiting, &mut budget)
                        .and_then(|found| {
                            self.follow(found, &path, context, &mut visiting, &mut budget)
                        }) {
                        Ok(found) => {
                            targets.insert(found);
                        }
                        Err("name_not_found" | "private_binding") => {}
                        Err(reason) => error = Some(reason),
                    }
                }
                if let Some(error) = error {
                    Err(error)
                } else {
                    unique(targets)
                }
            };
            match result {
                Ok(Found::Function(target)) => {
                    outcomes.insert(target);
                }
                Ok(_) => failure = Some("module_not_callable"),
                Err(reason) => failure = Some(reason),
            }
        }
        if outcomes.is_empty() {
            return Err(failure.unwrap_or("name_not_found"));
        }
        if outcomes.len() != 1 || failure.is_some() {
            return Err("ambiguous_crate_context");
        }
        Ok(outcomes.into_iter().next().unwrap())
    }
    fn visible(&self, binding: &Binding, context: &Context, requester: &Context) -> bool {
        let visibility = binding.visibility.as_str();
        if visibility == "pub" {
            return true;
        }
        if context.key.0 != requester.key.0 {
            return false;
        }
        if visibility == "pub(crate)" {
            return true;
        }
        let mut scope = context.key.1.clone();
        if visibility == "pub(super)" {
            scope.pop();
        } else if let Some(path) = visibility
            .strip_prefix("pub(in")
            .and_then(|p| p.strip_suffix(')'))
        {
            let mut parts = path.split("::");
            match parts.next() {
                Some("crate") => scope.clear(),
                Some("self") => {}
                Some("super") => {
                    scope.pop();
                }
                _ => return false,
            }
            scope.extend(parts.map(str::to_string));
        } else if !matches!(visibility, "" | "pub(self)") {
            return false;
        }
        requester.key.1.starts_with(&scope)
    }
    fn follow(
        &self,
        found: Found,
        path: &[String],
        requester: &Context,
        visiting: &mut BTreeSet<(Key, Vec<String>, bool)>,
        budget: &mut usize,
    ) -> Result<Found, &'static str> {
        if path.is_empty() {
            return Ok(found);
        }
        match found {
            Found::Module(key) => self.resolve(
                self.contexts
                    .get(&key)
                    .ok_or("module_context_not_indexed")?,
                path,
                requester,
                false,
                visiting,
                budget,
            ),
            Found::Function(_) => Err("unsupported_associated_item"),
        }
    }
    fn resolve(
        &self,
        context: &Context,
        path: &[String],
        requester: &Context,
        from_use: bool,
        visiting: &mut BTreeSet<(Key, Vec<String>, bool)>,
        budget: &mut usize,
    ) -> Result<Found, &'static str> {
        if *budget == 0 || visiting.len() >= 64 {
            return Err("import_resolution_budget");
        }
        *budget -= 1;
        if let Some(reason) = context.reason {
            return Err(reason);
        }
        if path.is_empty() {
            return Ok(Found::Module(context.key.clone()));
        }
        let key = (context.key.clone(), path.to_vec(), from_use);
        if !visiting.insert(key.clone()) {
            return Err("cyclic_import");
        }
        let result = self.resolve_inner(context, path, requester, from_use, visiting, budget);
        visiting.remove(&key);
        result
    }
    fn resolve_inner(
        &self,
        context: &Context,
        path: &[String],
        requester: &Context,
        from_use: bool,
        visiting: &mut BTreeSet<(Key, Vec<String>, bool)>,
        budget: &mut usize,
    ) -> Result<Found, &'static str> {
        let edition = &self.roots[context.key.0].edition;
        let mut location = context.key.clone();
        let mut consumed = 0;
        match path[0].as_str() {
            "crate" => {
                location.1.clear();
                consumed = 1;
            }
            "self" => consumed = 1,
            "super" => {
                while path.get(consumed).is_some_and(|p| p == "super") {
                    location.1.pop().ok_or("super_outside_crate")?;
                    consumed += 1;
                }
            }
            "" => {
                if edition == "2015" {
                    location.1.clear();
                    consumed = 1;
                } else {
                    let name = path.get(1).ok_or("empty_extern_path")?;
                    let root = self.dependency(context, name)?;
                    return self.follow(
                        Found::Module(root),
                        &path[2..],
                        requester,
                        visiting,
                        budget,
                    );
                }
            }
            _ => {
                if from_use && edition == "2015" {
                    location.1.clear();
                }
            }
        }
        let context = self
            .contexts
            .get(&location)
            .ok_or("module_context_not_indexed")?;
        if let Some(reason) = context.reason {
            return Err(reason);
        }
        let path = &path[consumed..];
        if path.is_empty() {
            return Ok(Found::Module(location));
        }
        let (_, facts) = self
            .modules
            .get(&context.file)
            .ok_or("module_not_indexed")?;
        let direct: Vec<_> = facts
            .bindings
            .iter()
            .filter(|b| b.scope == 0 && b.context == context.inline && b.name == path[0])
            .collect();
        if direct.len() > 1 {
            return Err("ambiguous_binding");
        }
        if let Some(binding) = direct.first() {
            if !self.visible(binding, context, requester) {
                return Err("private_binding");
            }
            if let Some(reason) = &binding.reason {
                return Err(binding_reason(reason));
            }
            let found = match binding.kind.as_str() {
                "function" => Found::Function(self.declaration(
                    &context.file,
                    &binding.symbol,
                    binding.site.line,
                )?),
                "module" => {
                    let mut key = context.key.clone();
                    key.1.push(binding.name.clone());
                    Found::Module(key)
                }
                "use" => self.resolve(context, &binding.path, context, true, visiting, budget)?,
                _ => return Err("unsupported_associated_item"),
            };
            return self.follow(found, &path[1..], requester, visiting, budget);
        }
        let mut candidates = BTreeSet::new();
        for glob in facts
            .bindings
            .iter()
            .filter(|b| b.scope == 0 && b.context == context.inline && b.kind == "glob")
        {
            if !self.visible(glob, context, requester) {
                continue;
            }
            if let Some(reason) = &glob.reason {
                return Err(binding_reason(reason));
            }
            let found = self.resolve(context, &glob.path, context, true, visiting, budget)?;
            match self.follow(found, path, context, visiting, budget) {
                Ok(found) => {
                    candidates.insert(found);
                }
                Err("name_not_found" | "private_binding") => {}
                Err(reason) => return Err(reason),
            }
        }
        if !candidates.is_empty() {
            return unique(candidates);
        }
        if edition != "2015" {
            if let Ok(root) = self.dependency(context, &path[0]) {
                return self.follow(Found::Module(root), &path[1..], requester, visiting, budget);
            }
        }
        Err("name_not_found")
    }
}
fn unique(mut candidates: BTreeSet<Found>) -> Result<Found, &'static str> {
    match candidates.len() {
        0 => Err("name_not_found"),
        1 => Ok(candidates.pop_first().unwrap()),
        _ => Err("ambiguous_glob"),
    }
}

impl GraphStore {
    pub(super) fn resolve_rust_modules(&self, project: &str) -> anyhow::Result<usize> {
        let wtx = self.db.begin_write()?;
        let mut resolver = Resolver {
            modules: BTreeMap::new(),
            nodes: BTreeMap::new(),
            manifests: BTreeMap::new(),
            roots: vec![],
            contexts: BTreeMap::new(),
            origins: BTreeMap::new(),
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
                        .and_then(|m| m.get(CONFIG_KEY))
                        .and_then(|v| v.as_str())
                    {
                        if let Ok(manifest) = serde_json::from_str::<Manifest>(raw) {
                            if manifest.version == 1 {
                                resolver
                                    .manifests
                                    .insert(node.file_path.as_str().into(), manifest);
                            }
                        }
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
        resolver.build_contexts();
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
