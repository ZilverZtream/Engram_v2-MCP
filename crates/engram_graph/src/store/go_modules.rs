//! Resolve Go package functions from indexed imports and module manifests.
use super::*;
use engram_core::go_modules::{
    CALL_KEY, CONFIG_KEY, Call, Config, Import, MODULE_KEY, Module, Replacement, UNBOUND_PREFIX,
};
use std::collections::{BTreeMap, BTreeSet};
const BINDINGS: &str = "go_module_bindings_v1";
const PRESERVED: &str = "go_preserved_call_edge_v1";
struct Resolver {
    modules: BTreeMap<String, (Node, Module)>,
    nodes: BTreeMap<String, Node>,
    configs: BTreeMap<String, Config>,
}
fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |p| p.0)
}
fn join(base: &str, path: &str) -> Result<String, &'static str> {
    if path.starts_with(['/', '\\']) || path.contains([':', '\0']) {
        return Err("module_outside_project");
    }
    let normalized = path.replace('\\', "/");
    let mut parts: Vec<_> = base.split('/').filter(|s| !s.is_empty()).collect();
    for part in normalized.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop().ok_or("module_outside_project")?;
            }
            _ => parts.push(part),
        }
    }
    Ok(parts.join("/"))
}
fn suffix<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    if path == prefix {
        Some("")
    } else {
        path.strip_prefix(prefix)?.strip_prefix('/')
    }
}
impl Resolver {
    fn ancestor_config(&self, directory: &str, name: &str) -> Option<(&String, &Config)> {
        let mut at = directory;
        loop {
            if let Some(found) = self.configs.get_key_value(&join(at, name).ok()?) {
                return Some(found);
            }
            if at.is_empty() {
                return None;
            }
            at = parent(at);
        }
    }
    fn package_dir(&self, root: &str, rest: &str) -> Result<String, &'static str> {
        let directory = join(parent(root), rest)?;
        if self
            .ancestor_config(&directory, "go.mod")
            .map(|(p, _)| p.as_str())
            != Some(root)
        {
            return Err("nested_module_not_selected");
        }
        Ok(directory)
    }
    fn imported_dir(&self, file: &str, path: &str) -> Result<String, &'static str> {
        let (own, config) = self
            .ancestor_config(parent(file), "go.mod")
            .ok_or("module_manifest_not_indexed")?;
        if !config.issues.is_empty() {
            return Err("unsupported_module_config");
        }
        let mut mains = vec![(own, config)];
        let workspace = self.ancestor_config(parent(own), "go.work");
        if let Some((work, facts)) = workspace {
            if !facts.issues.is_empty() {
                return Err("unsupported_workspace_config");
            }
            mains.clear();
            for directory in &facts.uses {
                let target = join(&join(parent(work), directory)?, "go.mod")?;
                let (key, config) = self
                    .configs
                    .get_key_value(&target)
                    .ok_or("workspace_module_not_indexed")?;
                if !config.issues.is_empty() {
                    return Err("unsupported_module_config");
                }
                mains.push((key, config));
            }
            if !mains.iter().any(|(p, _)| *p == own) {
                return Err("module_not_in_workspace");
            }
        }
        let mut direct = vec![];
        for (root, facts) in &mains {
            if let Some(module) = &facts.module {
                if let Some(rest) = suffix(path, module) {
                    direct.push((module.len(), root.as_str(), rest));
                }
            }
        }
        direct.sort();
        if let Some((length, root, rest)) = direct.last() {
            if direct.iter().filter(|(n, _, _)| n == length).count() != 1 {
                return Err("ambiguous_module_identity");
            }
            return self.package_dir(root, rest);
        }
        let mut requirements: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for (_, facts) in &mains {
            for (module, version) in &facts.requires {
                requirements.entry(module).or_default().insert(version);
            }
        }
        let Some((module, versions)) = requirements
            .iter()
            .filter(|(m, _)| suffix(path, m).is_some())
            .max_by_key(|(m, _)| m.len())
        else {
            return Err("external_package_not_indexed");
        };
        let active = |r: &Replacement| {
            r.module == *module
                && r.version
                    .as_deref()
                    .is_none_or(|v| versions.len() == 1 && versions.contains(v))
        };
        let mut replacements: Vec<(&str, &Replacement)> = vec![];
        if let Some((work, facts)) = workspace {
            replacements.extend(
                facts
                    .replaces
                    .iter()
                    .filter(|r| active(r))
                    .map(|r| (work.as_str(), r)),
            );
        }
        if replacements.is_empty() {
            for (root, facts) in &mains {
                replacements.extend(
                    facts
                        .replaces
                        .iter()
                        .filter(|r| active(r))
                        .map(|r| (root.as_str(), r)),
                );
            }
        }
        // A wildcard replacement overrides version-specific selections.
        if replacements.iter().any(|(_, r)| r.version.is_none()) {
            replacements.retain(|(_, r)| r.version.is_none());
        }
        let mut destinations = BTreeSet::new();
        for (origin, replacement) in replacements {
            if replacement.destination_version.is_some()
                || !(replacement.destination.starts_with("./")
                    || replacement.destination.starts_with("../")
                    || replacement.destination == "."
                    || replacement.destination == "..")
            {
                return Err("nonlocal_replacement_not_indexed");
            }
            destinations.insert(join(parent(origin), &replacement.destination)?);
        }
        if destinations.len() != 1 {
            return Err(if destinations.is_empty() {
                "external_package_not_indexed"
            } else {
                "ambiguous_local_replacement"
            });
        }
        let directory = destinations.pop_first().unwrap();
        let root = join(&directory, "go.mod")?;
        let facts = self
            .configs
            .get(&root)
            .ok_or("replacement_manifest_not_indexed")?;
        if !facts.issues.is_empty() || facts.module.as_deref() != Some(*module) {
            return Err("replacement_module_identity_mismatch");
        }
        self.package_dir(&root, suffix(path, module).unwrap())
    }
    fn package_name(&self, directory: &str) -> Result<String, &'static str> {
        let names: BTreeSet<_> = self
            .modules
            .iter()
            .filter(|(file, (_, m))| parent(file) == directory && !m.test_file)
            .map(|(_, (_, m))| m.package.clone())
            .collect();
        match names.len() {
            0 => Err("package_not_indexed"),
            1 => Ok(names.into_iter().next().unwrap()),
            _ => Err("ambiguous_package_name"),
        }
    }
    fn lookup(
        &self,
        directory: &str,
        package: &str,
        name: &str,
        tests: bool,
        external: bool,
    ) -> Result<String, &'static str> {
        if external && !name.chars().next().is_some_and(char::is_uppercase) {
            return Err("unexported_package_binding");
        }
        let mut found = vec![];
        for (file, (_, facts)) in &self.modules {
            if parent(file) != directory || facts.package != package || (facts.test_file && !tests)
            {
                continue;
            }
            if !facts.issues.is_empty() {
                return Err("incomplete_package_syntax");
            }
            for declaration in facts.declarations.iter().filter(|d| d.name == name) {
                found.push((file, facts, declaration));
            }
        }
        if found.len() > 1 {
            return Err("ambiguous_package_binding");
        }
        let (file, facts, declaration) = found.first().ok_or("name_not_found")?;
        if facts.constraint.is_some() {
            return Err("build_constraints_not_evaluated");
        }
        if declaration.kind != "function" {
            return Err("noncallable_package_binding");
        }
        if !declaration.has_body {
            return Err("function_body_not_indexed");
        }
        let id = engram_core::NodeId::symbol("function", None, file, name, declaration.site.line).0;
        let node = self.nodes.get(&id).ok_or("declaration_not_indexed")?;
        if node.node_type != "function"
            || node.file_path.as_str() != file.as_str()
            || node.start_line != declaration.site.line
        {
            return Err("declaration_identity_mismatch");
        }
        Ok(id)
    }
    fn imported_target(
        &self,
        file: &str,
        import: &Import,
        name: &str,
    ) -> Result<String, &'static str> {
        let directory = self.imported_dir(file, &import.path)?;
        // The Go command restricts internal packages to their containing tree.
        let parts: Vec<_> = import.path.split('/').collect();
        if let Some(at) = parts.iter().rposition(|p| *p == "internal") {
            let (root, config) = self
                .ancestor_config(parent(file), "go.mod")
                .ok_or("module_manifest_not_indexed")?;
            let relative = if parent(root).is_empty() {
                parent(file)
            } else {
                suffix(parent(file), parent(root)).ok_or("module_identity_mismatch")?
            };
            let caller = format!(
                "{}/{}",
                config.module.as_deref().ok_or("module_identity_mismatch")?,
                relative
            );
            let allowed = parts[..at].join("/");
            if !allowed.is_empty() && suffix(caller.trim_end_matches('/'), &allowed).is_none() {
                return Err("internal_package_not_visible");
            }
        }
        let package = self.package_name(&directory)?;
        if package == "main" {
            return Err("main_package_not_importable");
        }
        self.lookup(&directory, &package, name, false, true)
    }
    fn resolve_call(&self, file: &str, call: &mut Call) -> Result<String, &'static str> {
        let (_, facts) = self.modules.get(file).ok_or("package_not_indexed")?;
        let imported = call.imported.clone();
        let adopt = |call: &mut Call, import: &Import| {
            call.import_site = import.site.clone();
            call.module = import.path.clone();
            call.path_from_use = true;
        };
        if let Some(qualifier) = call.qualifier.clone() {
            if !matches!(
                self.lookup(
                    parent(file),
                    &facts.package,
                    &qualifier,
                    facts.test_file,
                    false
                ),
                Err("name_not_found")
            ) {
                return Err("shadowed_or_ambiguous_package_qualifier");
            }
            let explicit: Vec<_> = facts
                .imports
                .iter()
                .filter(|i| i.alias.as_deref() == Some(&qualifier))
                .collect();
            if explicit.len() > 1 {
                return Err("ambiguous_import_binding");
            }
            let mut matching = explicit;
            if matching.is_empty() {
                for import in facts.imports.iter().filter(|i| i.alias.is_none()) {
                    if self
                        .imported_dir(file, &import.path)
                        .and_then(|d| self.package_name(&d))
                        .is_ok_and(|n| n == qualifier)
                    {
                        matching.push(import);
                    }
                }
            }
            if matching.len() != 1 {
                return Err(if matching.is_empty() {
                    "unknown_receiver_or_unindexed_import"
                } else {
                    "ambiguous_import_binding"
                });
            }
            let import = matching[0];
            adopt(call, import);
            return self.imported_target(file, import, &imported);
        }
        let local = self.lookup(
            parent(file),
            &facts.package,
            &imported,
            facts.test_file,
            false,
        );
        let mut matches = vec![];
        match local {
            Ok(target) => matches.push((target, None)),
            Err("name_not_found") => {}
            Err(reason) => return Err(reason),
        }
        for import in facts
            .imports
            .iter()
            .filter(|i| i.alias.as_deref() == Some("."))
        {
            match self.imported_target(file, import, &imported) {
                Ok(target) => matches.push((target, Some(import))),
                Err("name_not_found" | "unexported_package_binding") => {}
                Err(reason) => return Err(reason),
            }
        }
        if matches.len() != 1 {
            return Err(if matches.len() > 1 {
                "ambiguous_dot_import"
            } else if matches!(
                imported.as_str(),
                "append"
                    | "cap"
                    | "clear"
                    | "close"
                    | "complex"
                    | "copy"
                    | "delete"
                    | "imag"
                    | "len"
                    | "make"
                    | "max"
                    | "min"
                    | "new"
                    | "panic"
                    | "print"
                    | "println"
                    | "real"
                    | "recover"
            ) {
                "builtin_not_source_call"
            } else {
                "name_not_found"
            });
        }
        let (target, import) = matches.pop().unwrap();
        if let Some(import) = import {
            adopt(call, import);
        }
        Ok(target)
    }
}

impl GraphStore {
    pub(super) fn resolve_go_modules(&self, project: &str) -> anyhow::Result<usize> {
        let wtx = self.db.begin_write()?;
        let mut resolver = Resolver {
            modules: BTreeMap::new(),
            nodes: BTreeMap::new(),
            configs: BTreeMap::new(),
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
                        if let Ok(manifest) = serde_json::from_str::<Config>(raw) {
                            if manifest.version == 1 {
                                resolver
                                    .configs
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
