use engram_core::rust_modules::{Dependency, Manifest, Target};
use std::collections::BTreeMap;
use toml_edit::Item;

fn dependencies(item: Option<&Item>) -> BTreeMap<String, Dependency> {
    item.and_then(Item::as_table_like)
        .map(|t| {
            t.iter()
                .map(|(name, item)| {
                    let path = item.get("path").and_then(Item::as_str).map(str::to_string);
                    let workspace = item.get("workspace").and_then(Item::as_bool) == Some(true);
                    let conditional = item.get("optional").and_then(Item::as_bool) == Some(true);
                    (
                        name.replace('-', "_"),
                        Dependency {
                            path,
                            workspace,
                            reason: conditional.then(|| "optional_dependency".into()),
                        },
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}
pub(crate) fn extract(source: &str) -> Manifest {
    let mut facts = Manifest {
        version: 1,
        name: None,
        edition: None,
        inherit_edition: false,
        workspace_edition: None,
        targets: vec![],
        autobins: true,
        dependencies: BTreeMap::new(),
        workspace_dependencies: BTreeMap::new(),
        issues: vec![],
    };
    let document = match source.parse::<toml_edit::DocumentMut>() {
        Ok(value) => value,
        Err(_) => {
            facts.issues.push("invalid_cargo_manifest".into());
            return facts;
        }
    };
    let package = document.get("package");
    facts.name = package
        .and_then(|p| p.get("name"))
        .and_then(Item::as_str)
        .map(str::to_string);
    let edition = package.and_then(|p| p.get("edition"));
    facts.edition = edition.and_then(Item::as_str).map(str::to_string);
    facts.inherit_edition = edition
        .and_then(|e| e.get("workspace"))
        .and_then(Item::as_bool)
        == Some(true);
    facts.workspace_edition = document
        .get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("edition"))
        .and_then(Item::as_str)
        .map(str::to_string);
    facts.dependencies = dependencies(document.get("dependencies"));
    facts.workspace_dependencies = dependencies(
        document
            .get("workspace")
            .and_then(|w| w.get("dependencies")),
    );
    // Target-specific dependencies are not unconditional bindings.
    if let Some(targets) = document.get("target").and_then(Item::as_table_like) {
        for (_, target) in targets.iter() {
            for (name, mut dependency) in dependencies(target.get("dependencies")) {
                dependency.reason = Some("conditional_dependency".into());
                facts.dependencies.entry(name).or_insert(dependency);
            }
        }
    }
    if let Some(name) = &facts.name {
        let library = document.get("lib");
        if library.is_some()
            || package
                .and_then(|p| p.get("autolib"))
                .and_then(Item::as_bool)
                != Some(false)
        {
            facts.targets.push(Target {
                kind: "lib".into(),
                name: library
                    .and_then(|p| p.get("name"))
                    .and_then(Item::as_str)
                    .unwrap_or(name)
                    .replace('-', "_"),
                path: Some(
                    library
                        .and_then(|p| p.get("path"))
                        .and_then(Item::as_str)
                        .unwrap_or("src/lib.rs")
                        .into(),
                ),
            });
        }
        facts.autobins = package
            .and_then(|p| p.get("autobins"))
            .and_then(Item::as_bool)
            != Some(false);
        if facts.autobins {
            facts.targets.push(Target {
                kind: "bin".into(),
                name: name.replace('-', "_"),
                path: Some("src/main.rs".into()),
            });
        }
        if let Some(bins) = document.get("bin").and_then(Item::as_array_of_tables) {
            for bin in bins {
                let path = bin.get("path").and_then(Item::as_str).map(str::to_string);
                let name = bin.get("name").and_then(Item::as_str).unwrap_or("");
                if name.is_empty() && path.is_none() {
                    facts.issues.push("unnamed_binary_target".into());
                    continue;
                }
                facts.targets.push(Target {
                    kind: "bin".into(),
                    name: name.replace('-', "_"),
                    path,
                });
            }
        }
    }
    facts
}
