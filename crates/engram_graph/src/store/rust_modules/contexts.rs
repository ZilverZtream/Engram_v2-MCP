use super::*;

pub(super) fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |p| p.0)
}
pub(super) fn join(base: &str, path: &str) -> Option<String> {
    if path.starts_with(['/', '\\']) || path.contains([':', '\0']) {
        return None;
    }
    let path = path.replace('\\', "/");
    let mut parts: Vec<_> = base.split('/').filter(|s| !s.is_empty()).collect();
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
    pub(super) fn workspace(&self, manifest: &str) -> Option<(&String, &Manifest)> {
        let mut directory = parent(manifest);
        loop {
            let path = join(directory, "Cargo.toml")?;
            if let Some((key, value)) = self.manifests.get_key_value(&path) {
                if value.workspace_edition.is_some() || !value.workspace_dependencies.is_empty() {
                    return Some((key, value));
                }
            }
            if directory.is_empty() {
                return None;
            }
            directory = parent(directory);
        }
    }
    pub(super) fn build_contexts(&mut self) {
        let mut roots = Vec::new();
        for (path, manifest) in &self.manifests {
            if manifest.name.is_none() || !manifest.issues.is_empty() {
                continue;
            }
            let edition = if manifest.inherit_edition {
                self.workspace(path)
                    .and_then(|(_, m)| m.workspace_edition.clone())
                    .unwrap_or_default()
            } else {
                manifest.edition.clone().unwrap_or_else(|| "2015".into())
            };
            let reason = (!matches!(edition.as_str(), "2015" | "2018" | "2021" | "2024"))
                .then_some("unsupported_or_missing_edition");
            let mut targets = manifest.targets.clone();
            if manifest.autobins {
                let prefix = join(parent(path), "src/bin/").unwrap() + "/";
                for file in self.modules.keys() {
                    if let Some(rest) = file.strip_prefix(&prefix) {
                        let name = if !rest.contains('/') {
                            rest.strip_suffix(".rs")
                        } else {
                            rest.strip_suffix("/main.rs").filter(|s| !s.contains('/'))
                        };
                        if let Some(name) = name {
                            targets.push(engram_core::rust_modules::Target {
                                name: name.into(),
                                kind: "bin".into(),
                                path: Some(format!("src/bin/{rest}")),
                            });
                        }
                    }
                }
            }
            for target in targets {
                let candidates = target.path.map(|p| vec![p]).unwrap_or_else(|| {
                    vec![
                        format!("src/bin/{}.rs", target.name),
                        format!("src/bin/{}/main.rs", target.name),
                    ]
                });
                for candidate in candidates {
                    let Some(file) = join(parent(path), &candidate) else {
                        continue;
                    };
                    if self.modules.contains_key(&file) {
                        roots.push(Root {
                            file,
                            manifest: Some(path.clone()),
                            name: target.name.clone(),
                            kind: target.kind.clone(),
                            edition: edition.clone(),
                            reason,
                        });
                    }
                }
            }
        }
        // Standalone trees still have explicit conventional crate roots. A
        // Cargo-owned source never gets an invented competing root.
        for file in self.modules.keys() {
            if !matches!(file.rsplit('/').next(), Some("lib.rs" | "main.rs")) {
                continue;
            }
            let owned = self.manifests.keys().any(|m| {
                let p = parent(m);
                p.is_empty() || file.starts_with(&format!("{p}/"))
            });
            if !owned {
                roots.push(Root {
                    file: file.clone(),
                    manifest: None,
                    name: String::new(),
                    kind: "standalone".into(),
                    edition: "2021".into(),
                    reason: None,
                });
            }
        }
        roots.sort_by(|a, b| (&a.file, &a.name).cmp(&(&b.file, &b.name)));
        roots.dedup_by(|a, b| a.file == b.file && a.manifest == b.manifest && a.name == b.name);
        self.roots = roots;
        for root in 0..self.roots.len() {
            let file = self.roots[root].file.clone();
            self.add_context(
                Context {
                    key: (root, vec![]),
                    directory: parent(&file).into(),
                    file,
                    inline: vec![],
                    reason: self.roots[root].reason,
                },
                &mut BTreeSet::new(),
            );
        }
    }
    fn add_context(
        &mut self,
        mut context: Context,
        visiting: &mut BTreeSet<(String, Vec<String>)>,
    ) {
        if let Some(existing) = self.contexts.get_mut(&context.key) {
            existing.reason = Some("ambiguous_module_declaration");
            return;
        }
        let source = (context.file.clone(), context.inline.clone());
        if visiting.len() >= 64 || self.contexts.len() >= 8192 {
            context.reason = Some("module_context_budget");
        }
        if visiting.contains(&source) {
            context.reason = Some("cyclic_module_source");
        }
        self.origins
            .entry(source.clone())
            .or_default()
            .push(context.key.clone());
        self.contexts.insert(context.key.clone(), context.clone());
        if context.reason.is_some() {
            return;
        }
        let Some((_, facts)) = self.modules.get(&context.file) else {
            return;
        };
        if !facts.issues.is_empty() {
            self.contexts.get_mut(&context.key).unwrap().reason = Some("incomplete_module_syntax");
            return;
        }
        let modules: Vec<_> = facts
            .bindings
            .iter()
            .filter(|b| b.scope == 0 && b.context == context.inline && b.kind == "module")
            .cloned()
            .collect();
        visiting.insert(source.clone());
        for binding in modules {
            let mut logical = context.key.1.clone();
            logical.push(binding.name.clone());
            let mut next = context.clone();
            next.key = (context.key.0, logical);
            next.reason = binding.reason.as_deref().map(binding_reason);
            if binding.inline {
                next.inline.push(binding.name.clone());
                let base = if context.inline.is_empty() {
                    parent(&context.file)
                } else {
                    &context.directory
                };
                let directory = if let Some(attr) = &binding.path_attribute {
                    join(base, attr)
                } else {
                    join(&context.directory, &binding.name)
                };
                if let Some(directory) = directory {
                    next.directory = directory;
                } else {
                    next.reason = Some("module_outside_project");
                }
            } else {
                let candidates = if let Some(attr) = &binding.path_attribute {
                    let base = if context.inline.is_empty() {
                        parent(&context.file)
                    } else {
                        &context.directory
                    };
                    join(base, attr).into_iter().collect::<Vec<_>>()
                } else {
                    [
                        format!("{}.rs", binding.name),
                        format!("{}/mod.rs", binding.name),
                    ]
                    .into_iter()
                    .filter_map(|p| join(&context.directory, &p))
                    .collect()
                };
                let present: Vec<_> = candidates
                    .iter()
                    .filter(|p| self.modules.contains_key(*p))
                    .collect();
                if present.len() != 1 {
                    next.reason = Some(if present.is_empty() {
                        "module_not_indexed"
                    } else {
                        "ambiguous_module_source"
                    });
                }
                next.file = present
                    .first()
                    .map(|p| (*p).clone())
                    .or_else(|| candidates.first().cloned())
                    .unwrap_or_default();
                next.inline.clear();
                next.directory = if next.file.ends_with("/mod.rs") {
                    parent(&next.file).into()
                } else {
                    next.file.strip_suffix(".rs").unwrap_or(&next.file).into()
                };
            }
            self.add_context(next, visiting);
        }
        visiting.remove(&source);
    }
    pub(super) fn dependency(&self, context: &Context, name: &str) -> Result<Key, &'static str> {
        let root = &self.roots[context.key.0];
        let manifest_path = root.manifest.as_ref().ok_or("external_crate_not_indexed")?;
        let manifest = self
            .manifests
            .get(manifest_path)
            .ok_or("cargo_manifest_not_indexed")?;
        if root.kind != "lib" {
            let own: Vec<_> = self
                .roots
                .iter()
                .enumerate()
                .filter(|(_, r)| r.manifest == root.manifest && r.kind == "lib" && r.name == name)
                .collect();
            if own.len() == 1 {
                return Ok((own[0].0, vec![]));
            }
        }
        let mut dependency = manifest
            .dependencies
            .get(name)
            .ok_or("external_crate_not_indexed")?;
        let mut origin = manifest_path;
        if dependency.reason.is_some() {
            return Err("conditional_or_optional_dependency");
        }
        if dependency.workspace {
            let (path, workspace) = self
                .workspace(manifest_path)
                .ok_or("workspace_dependency_not_indexed")?;
            dependency = workspace
                .workspace_dependencies
                .get(name)
                .ok_or("workspace_dependency_not_indexed")?;
            origin = path;
        }
        if dependency.reason.is_some() {
            return Err("conditional_or_optional_dependency");
        }
        let destination = dependency
            .path
            .as_deref()
            .ok_or("external_crate_not_indexed")?;
        let directory = join(parent(origin), destination).ok_or("dependency_outside_project")?;
        let target = join(&directory, "Cargo.toml").unwrap();
        let candidates: Vec<_> = self
            .roots
            .iter()
            .enumerate()
            .filter(|(_, r)| r.kind == "lib" && r.manifest.as_deref() == Some(&target))
            .collect();
        if candidates.len() != 1 {
            return Err("missing_or_ambiguous_dependency_root");
        }
        Ok((candidates[0].0, vec![]))
    }
}
