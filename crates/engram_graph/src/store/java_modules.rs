//! Bind Java calls to indexed package/type declarations without terminal-name guesses.
use super::*;
use engram_core::java_modules::{
    CALL_KEY, Call, Import, MODULE_KEY, Method, Module, Type, UNBOUND_PREFIX,
};
use std::collections::{BTreeMap, BTreeSet};
const BINDINGS: &str = "java_module_bindings_v1";
const PRESERVED: &str = "java_preserved_call_edge_v1";
struct Resolver {
    modules: BTreeMap<String, (Node, Module)>,
    nodes: BTreeMap<String, Node>,
    types: BTreeMap<String, Vec<(String, usize)>>,
}
type TypeRef<'a> = (&'a str, &'a Module, &'a Type);
fn qualify(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.into()
    } else {
        format!("{prefix}.{name}")
    }
}
fn adopt(call: &mut Call, import: &Import) {
    call.module = import.path.clone();
    call.import_site = import.site.clone();
    call.path_from_use = true;
}
impl Resolver {
    fn named_root(&self, file: &str) -> Option<&str> {
        self.modules
            .iter()
            .filter(|(p, (_, m))| {
                m.named_module
                    && (p
                        .rsplit_once('/')
                        .map_or(true, |(dir, _)| file.starts_with(&format!("{dir}/"))))
            })
            .max_by_key(|(p, _)| p.len())
            .map(|(p, _)| p.as_str())
    }
    fn type_ref(&self, name: &str) -> Result<TypeRef<'_>, &'static str> {
        let choices = self.types.get(name).ok_or("type_not_indexed")?;
        if choices.len() != 1 {
            return Err("ambiguous_type_identity");
        }
        let (path, index) = &choices[0];
        let module = &self.modules[path].1;
        Ok((path, module, &module.types[*index]))
    }
    fn nest(&self, name: &str) -> String {
        let mut at = name.to_string();
        let mut visited = BTreeSet::new();
        while visited.insert(at.clone()) {
            match self.type_ref(&at) {
                Ok((_, _, ty)) => match &ty.owner {
                    Some(owner) => at = owner.clone(),
                    None => break,
                },
                Err(_) => break,
            }
        }
        at
    }
    fn access(
        &self,
        caller: &str,
        owner: &str,
        source_package: &str,
        target_package: &str,
        visibility: &str,
    ) -> bool {
        visibility == "public"
            || (visibility == "private" && self.nest(caller) == self.nest(owner))
            || (visibility != "private" && source_package == target_package)
    }
    fn visible_type(
        &self,
        file: &str,
        call: &Call,
        name: &str,
    ) -> Result<TypeRef<'_>, &'static str> {
        let result = self.type_ref(name)?;
        let (target, module, ty) = result;
        if self.named_root(file) != self.named_root(target)
            && (self.named_root(file).is_some() || self.named_root(target).is_some())
        {
            return Err("module_visibility_not_evaluated");
        }
        if !module.issues.is_empty() {
            return Err("unsupported_target_source");
        }
        if ty.scoped {
            return Err("local_type_not_resolved");
        }
        let source = &self.modules[file].1;
        let mut current = Some(ty);
        let mut seen = BTreeSet::new();
        while let Some(t) = current {
            if !seen.insert(t.canonical.as_str()) {
                return Err("cyclic_type_owner");
            }
            if !self.access(
                &call.owner,
                &t.canonical,
                &source.package,
                &module.package,
                &t.access,
            ) {
                return Err("type_not_accessible");
            }
            current = match &t.owner {
                Some(o) => Some(self.type_ref(o)?.2),
                None => None,
            };
        }
        Ok(result)
    }
    fn type_name(
        &self,
        file: &str,
        call: &Call,
        name: &str,
    ) -> Result<(String, Option<Import>), &'static str> {
        let facts = &self.modules[file].1;
        let (head, tail) = name
            .split_once('.')
            .map_or((name, None), |(a, b)| (a, Some(b)));
        // Lexically enclosing types and their members precede imports.
        let mut current = Some(call.owner.clone());
        while let Some(owner) = current {
            let direct = qualify(&owner, head);
            let target = if owner.rsplit('.').next() == Some(head) {
                Some(owner.clone())
            } else if self.types.contains_key(&direct) {
                Some(direct)
            } else {
                None
            };
            if let Some(target) = target {
                return Ok((tail.map_or(target.clone(), |t| qualify(&target, t)), None));
            }
            current = self
                .type_ref(&owner)
                .ok()
                .and_then(|(_, _, t)| t.owner.clone());
        }
        let local: Vec<_> = facts
            .types
            .iter()
            .filter(|t| t.owner.is_none() && t.name == head)
            .collect();
        if !local.is_empty() {
            if local.len() != 1 {
                return Err("ambiguous_type_identity");
            };
            let n = &local[0].canonical;
            return Ok((tail.map_or(n.clone(), |t| qualify(n, t)), None));
        }
        let single: Vec<_> = facts
            .imports
            .iter()
            .filter(|i| !i.wildcard && i.path.rsplit('.').next() == Some(head))
            .collect();
        if !single.is_empty() {
            let names: BTreeSet<_> = single.iter().map(|i| i.path.as_str()).collect();
            if names.len() != 1 {
                return Err("ambiguous_type_import");
            };
            let i = single[0];
            return Ok((
                tail.map_or(i.path.clone(), |t| qualify(&i.path, t)),
                Some(i.clone()),
            ));
        }
        let package = qualify(&facts.package, head);
        if self.types.contains_key(&package) {
            return Ok((tail.map_or(package.clone(), |t| qualify(&package, t)), None));
        }
        let mut demand: BTreeMap<String, Option<Import>> = BTreeMap::new();
        for i in facts.imports.iter().filter(|i| i.wildcard) {
            // An explicit, entirely unindexed on-demand scope may also supply
            // this name. A candidate in another scope does not disprove that.
            if !self.types.contains_key(&i.path)
                && !self.modules.values().any(|(_, m)| m.package == i.path)
            {
                return Err("on_demand_import_not_indexed");
            }
            let n = qualify(&i.path, head);
            if self.types.contains_key(&n) {
                match self.visible_type(file, call, &n) {
                    Ok(_) => {
                        demand.insert(tail.map_or(n.clone(), |t| qualify(&n, t)), Some(i.clone()));
                    }
                    Err("type_not_accessible") => {}
                    Err(e) => return Err(e),
                }
            }
        }
        let standard = qualify("java.lang", head);
        if self.types.contains_key(&standard) {
            demand.insert(
                tail.map_or(standard.clone(), |t| qualify(&standard, t)),
                None,
            );
        }
        if demand.len() > 1 {
            return Err("ambiguous_type_import");
        }
        if let Some(found) = demand.into_iter().next() {
            return Ok(found);
        }
        if tail.is_some() && self.types.contains_key(name) {
            return Ok((name.into(), None));
        }
        Err("type_not_indexed")
    }
    fn method(
        &self,
        file: &str,
        call: &Call,
        owner: &str,
        static_only: bool,
    ) -> Result<String, &'static str> {
        let (path, module, ty) = self.visible_type(file, call, owner)?;
        if ty.inherited {
            return Err("inherited_methods_not_resolved");
        }
        let named: Vec<&Method> = module
            .methods
            .iter()
            .filter(|m| m.owner == owner && m.name == call.imported)
            .collect();
        if named.is_empty() {
            return Err("method_not_found");
        }
        let mut candidates: Vec<_> = named
            .iter()
            .copied()
            .filter(|m| {
                if m.variadic {
                    call.arity >= m.arity.saturating_sub(1)
                } else {
                    call.arity == m.arity
                }
            })
            .collect();
        if candidates.is_empty() {
            return Err("method_arity_mismatch");
        }
        // Without argument typing, a varargs candidate can also accept an array
        // in the fixed-arity phase. Never silently prefer a same-arity overload.
        if candidates.len() != 1 {
            return Err("ambiguous_method_overload");
        }
        let method = candidates.pop().unwrap();
        if static_only && !method.is_static {
            return Err("instance_method_requires_receiver");
        }
        if !self.access(
            &call.owner,
            owner,
            &self.modules[file].1.package,
            &module.package,
            &method.access,
        ) {
            return Err("method_not_accessible");
        }
        let id =
            engram_core::NodeId::symbol("function", None, path, &method.name, method.site.line).0;
        if module
            .methods
            .iter()
            .filter(|m| m.name == method.name && m.site.line == method.site.line)
            .count()
            != 1
        {
            return Err("declaration_identity_collision");
        }
        let Some(node) = self.nodes.get(&id) else {
            return Err("target_declaration_not_indexed");
        };
        if node.file_path.as_str() != path
            || node.start_line != method.site.line
            || node.name != method.name
        {
            return Err("target_declaration_not_indexed");
        }
        Ok(id)
    }
    fn resolve_call(&self, file: &str, call: &mut Call) -> Result<String, &'static str> {
        let facts = &self.modules[file].1;
        if !facts.issues.is_empty() {
            return Err("unsupported_source_facts");
        }
        if call.owner.is_empty() {
            return Err("implicit_owner_not_resolved");
        }
        if matches!(call.invocation.as_str(), "type" | "instance") {
            let spelling = call
                .receiver_type
                .as_deref()
                .ok_or("receiver_type_not_resolved")?;
            let (owner, import) = self.type_name(file, call, spelling)?;
            if let Some(i) = import {
                adopt(call, &i)
            } else {
                call.module = owner.clone();
            }
            return self.method(file, call, &owner, call.invocation == "type");
        }
        if call.invocation == "this" {
            if call.static_context {
                return Err("instance_receiver_in_static_context");
            }
            return self.method(file, call, &call.owner, false);
        }
        let mut owner = Some(call.owner.clone());
        while let Some(name) = owner {
            let (_, module, ty) = self.type_ref(&name)?;
            if module
                .methods
                .iter()
                .any(|m| m.owner == name && m.name == call.imported)
                || ty.inherited
            {
                return self.method(file, call, &name, call.static_context);
            }
            owner = ty.owner.clone();
        }
        let explicit: Vec<_> = facts
            .imports
            .iter()
            .filter(|i| {
                i.is_static
                    && !i.wildcard
                    && i.path.rsplit('.').next() == Some(call.imported.as_str())
            })
            .collect();
        let imports = if explicit.is_empty() {
            facts
                .imports
                .iter()
                .filter(|i| i.is_static && i.wildcard)
                .collect()
        } else {
            explicit
        };
        if imports.len() == 1 {
            adopt(call, imports[0]);
        }
        let mut targets: BTreeMap<String, Import> = BTreeMap::new();
        let mut failure = "method_not_found";
        for import in imports {
            let owner = if import.wildcard {
                import.path.as_str()
            } else {
                import
                    .path
                    .rsplit_once('.')
                    .map(|p| p.0)
                    .ok_or("invalid_static_import")?
            };
            match self.method(file, call, owner, true) {
                Ok(target) => {
                    targets.insert(target, import.clone());
                }
                Err(
                    e @ ("method_not_found"
                    | "method_arity_mismatch"
                    | "method_not_accessible"
                    | "instance_method_requires_receiver"),
                ) => failure = e,
                Err(e) => return Err(e),
            }
        }
        if targets.len() > 1 {
            return Err("ambiguous_static_import");
        }
        if let Some((target, import)) = targets.into_iter().next() {
            adopt(call, &import);
            Ok(target)
        } else {
            Err(failure)
        }
    }
}

impl GraphStore {
    pub(super) fn resolve_java_modules(&self, project: &str) -> anyhow::Result<usize> {
        let wtx = self.db.begin_write()?;
        let mut resolver = Resolver {
            modules: BTreeMap::new(),
            nodes: BTreeMap::new(),
            types: BTreeMap::new(),
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
        for (path, (_, facts)) in &resolver.modules {
            for (index, ty) in facts.types.iter().enumerate() {
                // Local/anonymous types have lexical identity, not a globally
                // importable canonical name. Their scope is retained by the
                // extractor; publishing them here would obscure other methods.
                if ty.scoped {
                    continue;
                }
                resolver
                    .types
                    .entry(ty.canonical.clone())
                    .or_default()
                    .push((path.clone(), index));
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
                let mut call = owned_call.call.clone();
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
                    resolver.resolve_call(path, &mut call)
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
