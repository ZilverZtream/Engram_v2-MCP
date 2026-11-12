use super::*;

#[derive(Default)]
struct Effective {
    base_url: Option<String>,
    // A paths table without baseUrl is relative to its defining config.
    paths: Option<(String, BTreeMap<String, Vec<String>>)>,
}

impl Resolver {
    fn effective_config(
        &self,
        path: &str,
        visiting: &mut BTreeSet<String>,
    ) -> Result<Effective, &'static str> {
        if visiting.len() >= 32 || !visiting.insert(path.into()) {
            return Err("cyclic_or_deep_config");
        }
        let config = self
            .configs
            .get(path)
            .and_then(|c| c.as_ref())
            .ok_or("config_not_indexed")?;
        if !config.issues.is_empty() {
            return Err("unsupported_or_invalid_config");
        }
        let mut effective = if let Some(extends) = &config.extends {
            if !(extends.starts_with("./") || extends.starts_with("../")) {
                return Err("external_config_extends");
            }
            let base = join(parent(path), extends).ok_or("config_outside_project")?;
            let resolved = if self.configs.contains_key(&base) {
                base
            } else {
                format!("{base}.json")
            };
            self.effective_config(&resolved, visiting)?
        } else {
            Effective::default()
        };
        if let Some(base) = &config.base_url {
            effective.base_url = Some(join(parent(path), base).ok_or("base_url_outside_project")?);
        }
        if let Some(paths) = &config.paths {
            effective.paths = Some((parent(path).into(), paths.clone()));
        }
        visiting.remove(path);
        Ok(effective)
    }

    pub(super) fn configured_module(
        &self,
        from: &str,
        specifier: &str,
    ) -> Result<String, &'static str> {
        let mut directory = parent(from);
        let config_path = loop {
            let candidate = if directory.is_empty() {
                "tsconfig.json".into()
            } else {
                format!("{directory}/tsconfig.json")
            };
            if self.configs.contains_key(&candidate) {
                break candidate;
            }
            if directory.is_empty() {
                return Err("external_or_unmapped_module");
            }
            directory = parent(directory);
        };
        let effective = self.effective_config(&config_path, &mut BTreeSet::new())?;
        if let Some((origin, paths)) = &effective.paths {
            // Exact pattern first; otherwise the matching wildcard with the
            // longest literal prefix. Fallback target arrays retain order.
            let selected = if let Some(targets) = paths.get(specifier) {
                Some((targets, ""))
            } else {
                let mut patterns: Vec<_> = paths
                    .iter()
                    .filter_map(|(pattern, targets)| {
                        let (prefix, suffix) = pattern.split_once('*')?;
                        let matched = specifier.strip_prefix(prefix)?.strip_suffix(suffix)?;
                        Some((prefix.len(), targets, matched))
                    })
                    .collect();
                patterns.sort_by(|a, b| b.0.cmp(&a.0));
                // Equal-priority patterns with different mappings cannot be
                // decided by a map's alphabetical iteration order.
                if patterns.len() > 1
                    && patterns[0].0 == patterns[1].0
                    && patterns[0].1 != patterns[1].1
                {
                    return Err("ambiguous_path_pattern");
                }
                patterns
                    .first()
                    .map(|(_, targets, matched)| (*targets, *matched))
            };
            if let Some((targets, matched)) = selected {
                let base = effective.base_url.as_deref().unwrap_or(origin);
                for target in targets {
                    let target = target.replace('*', matched);
                    let path = join(base, &target).ok_or("mapped_module_outside_project")?;
                    if let Ok(module) = self.module_at(&path) {
                        return Ok(module);
                    }
                }
                return Err("mapped_module_not_indexed");
            }
        }
        if let Some(base) = effective.base_url {
            let path = join(&base, specifier).ok_or("module_outside_project")?;
            return self.module_at(&path);
        }
        Err("external_or_unmapped_module")
    }
}
