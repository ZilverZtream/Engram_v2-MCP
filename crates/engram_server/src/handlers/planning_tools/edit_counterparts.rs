//! Structural companions have independent evidence even when Git treats the output as a hub.
use regex::Regex;
use std::collections::{BTreeSet, HashSet};
use std::path::Path;
use std::sync::LazyLock;

fn typescript(path: &str) -> bool {
    path.ends_with(".ts") || path.ends_with(".tsx")
}

fn matches_entry(path: &str, pattern: &str) -> bool {
    let p = pattern.trim_start_matches("./").trim_end_matches('/');
    if p == "**/*" {
        return true;
    }
    if !p.contains(['*', '?']) {
        return path == p
            || path
                .strip_prefix(p)
                .is_some_and(|tail| tail.starts_with('/'));
    }
    crate::utils::files::pattern_match(path, p)
}

fn build_outputs(
    root: &Path,
    current: &[String],
    edits: &[(String, String)],
) -> Vec<(String, String)> {
    use std::collections::HashMap;
    use std::path::PathBuf;
    let targets: HashMap<_, _> = edits
        .iter()
        .filter(|(_, p)| typescript(p))
        .filter_map(|(src, p)| Some((root.join(p).canonicalize().ok()?, src)))
        .collect();
    if targets.is_empty() {
        return Vec::new();
    }
    let sources: Vec<_> = current
        .iter()
        .filter(|p| typescript(p))
        .map(Path::new)
        .collect();
    let mut refs: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    let mut result = Vec::new();
    for (dir, config, bundle) in crate::services::typescript_outputs::declared(root, current.iter().map(String::as_str)) {
        let directory = root.join(&dir);
        let entries = |key: &str| {
            config[key]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
        };
        let mut pending: Vec<_> = entries("files").iter().map(|p| directory.join(p)).collect();
        let mut include = entries("include");
        if config.get("files").is_none() && config.get("include").is_none() {
            include.push("**/*");
        }
        let exclude = entries("exclude");
        for p in &sources {
            let Ok(local) = p.strip_prefix(&dir) else {
                continue;
            };
            let path = local.to_string_lossy().replace('\\', "/");
            if include.iter().any(|pat| matches_entry(&path, pat))
                && !exclude.iter().any(|pat| matches_entry(&path, pat))
            {
                pending.push(root.join(p));
            }
        }
        // Explicit relative roots can start outside the config directory.
        pending.extend(
            include
                .iter()
                .filter(|p| !p.contains(['*', '?']) && typescript(p))
                .map(|p| directory.join(p)),
        );
        let mut visited = HashSet::new();
        while let Some(source) = pending.pop() {
            let Ok(path) = source.canonicalize() else {
                continue;
            };
            if !path.starts_with(root) || !visited.insert(path.clone()) {
                continue;
            }
            if let Some(src) = targets.get(&path) {
                result.push(((*src).clone(), bundle.clone()));
            }
            let dependencies = refs.entry(path.clone()).or_insert_with(|| {
                static REF: LazyLock<Regex> = LazyLock::new(|| {
                    Regex::new(r#"(?m)^\s*///\s*<reference\s+path\s*=\s*["']([^"']+)["']"#).unwrap()
                });
                static IMPORT: LazyLock<Regex> = LazyLock::new(|| {
                    Regex::new(
                        r#"(?m)^\s*(?:import|export)\s+(?:[^\n;]*?\s+from\s*)?["'](\.[^"']+)["']"#,
                    )
                    .unwrap()
                });
                let Some(text) = path
                    .strip_prefix(root)
                    .ok()
                    .and_then(|p| p.to_str())
                    .and_then(|p| engram_core::safe_read_to_string(root, p).ok())
                else {
                    return Vec::new();
                };
                let text = text.trim_start_matches('\u{feff}');
                let Some(parent) = path.parent() else {
                    return Vec::new();
                };
                let mut dependencies: Vec<_> = REF
                    .captures_iter(text)
                    .map(|m| parent.join(&m[1]))
                    .collect();
                for m in IMPORT.captures_iter(text) {
                    let p = parent.join(&m[1]);
                    dependencies.extend(
                        [
                            p.with_extension("ts"),
                            p.with_extension("tsx"),
                            p.join("index.ts"),
                        ]
                        .into_iter()
                        .filter(|p| p.is_file()),
                    );
                }
                dependencies
            });
            pending.extend(dependencies.iter().cloned());
        }
    }
    result
}

pub(super) fn collect(
    root: &Path,
    edits: &[(String, String)],
    current: &[String],
    edited: &HashSet<String>,
) -> BTreeSet<(String, String, String)> {
    let mut result = BTreeSet::new();
    let Ok(root) = root.canonicalize() else {
        return result;
    };
    let indexed: std::collections::HashMap<_, _> = current
        .iter()
        .map(|p| (super::edit_path_key(p), p))
        .collect();
    let outputs = build_outputs(&root, current, edits);
    for (source, path) in edits {
        let lower = path.to_ascii_lowercase();
        // The config and existing on-disk bundle establish the relationship.
        // A large generated file may intentionally be absent from the code index.
        for (_, output) in outputs.iter().filter(|(anchor, _)| anchor == source) {
            if !edited.contains(&super::edit_path_key(output)) {
                result.insert((
                    source.clone(),
                    output.clone(),
                    "compiled output of an entry point referencing the edited source".to_owned(),
                ));
            }
        }
        let mut add = |candidate: &str, reason: &str| {
            let key = super::edit_path_key(candidate);
            if !edited.contains(&key)
                && let Some(actual) = indexed.get(&key)
            {
                result.insert((source.clone(), (*actual).clone(), reason.to_owned()));
            }
        };
        let page = lower
            .strip_suffix(".vb")
            .or_else(|| lower.strip_suffix(".cs"))
            .unwrap_or(&lower);
        if [".aspx", ".ascx", ".master"]
            .iter()
            .any(|ext| page.ends_with(ext))
        {
            for ext in [".designer.vb", ".designer.cs"] {
                add(&format!("{page}{ext}"), "generated WebForms designer");
            }
        }
    }
    super::resource_keys::assess(&root, edits, current, edited, &mut result);
    result
}
