//! TypeScript compiler outputs declared by the reviewed checkout.
use regex::Regex;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

fn parse_config(text: &str) -> Option<serde_json::Value> {
    static TOKENS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#""(?:\\.|[^"\\])*"|//[^\n]*|(?s:/\*.*?\*/)"#).unwrap());
    static COMMAS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#""(?:\\.|[^"\\])*"|,\s*([}\]])"#).unwrap());
    let clean = TOKENS.replace_all(
        text.trim_start_matches('\u{feff}'),
        |m: &regex::Captures| {
            if m[0].starts_with('/') {
                " ".to_owned()
            } else {
                m[0].to_owned()
            }
        },
    );
    let clean = COMMAS.replace_all(&clean, |m: &regex::Captures| {
        m.get(1).unwrap_or(m.get(0).unwrap()).as_str().to_owned()
    });
    serde_json::from_str(&clean).ok()
}

pub(crate) fn declared<'a>(
    root: &Path,
    paths: impl Iterator<Item = &'a str>,
) -> Vec<(PathBuf, serde_json::Value, String)> {
    let Ok(root) = root.canonicalize() else {
        return Vec::new();
    };
    let mut configs = BTreeSet::new();
    for file in paths.map(Path::new) {
        if matches!(
            file.extension().and_then(|s| s.to_str()),
            Some("ts" | "tsx")
        ) {
            configs.extend(file.ancestors().skip(1).map(|p| p.join("tsconfig.json")));
        } else if file
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.starts_with("tsconfig") && s.ends_with(".json"))
        {
            configs.insert(file.to_path_buf());
        }
    }
    configs
        .into_iter()
        .filter_map(|cfg| {
            let dir = cfg.parent()?;
            let text = engram_core::safe_read_to_string(&root, cfg.to_str()?).ok()?;
            let config = parse_config(&text)?;
            let output = config.get("compilerOptions")?.get("outFile")?.as_str()?;
            let file = root.join(dir).join(output).canonicalize().ok()?;
            let relative = file.strip_prefix(&root).ok()?.to_str()?.replace('\\', "/");
            Some((dir.to_path_buf(), config, relative))
        })
        .collect()
}

/// Positive evidence only. An unlisted script remains eligible as authored JS.
/// The source-generation inventory supplies config names even across bundles.
pub(crate) fn indexed_outputs(
    graph: &engram_graph::GraphStore,
    project: &str,
    root: &Path,
) -> anyhow::Result<BTreeSet<String>> {
    let mut paths = graph.indexed_paths_with_suffix(project, "tsconfig.json", 4096)?;
    paths.extend(graph.indexed_paths_with_suffix(project, ".ts", 8192)?);
    paths.extend(graph.indexed_paths_with_suffix(project, ".tsx", 8192)?);
    // Some projects use distinct named entrypoint configs in one directory.
    paths.extend(
        graph
            .indexed_paths_with_suffix(project, ".json", 8192)?
            .into_iter()
            .filter(|p| {
                Path::new(p)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|n| n.starts_with("tsconfig"))
            }),
    );
    Ok(declared(root, paths.iter().map(String::as_str))
        .into_iter()
        .map(|(_, _, out)| out)
        .collect())
}

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

pub(crate) fn build_outputs(
    root: &Path,
    current: &[String],
    edits: &[(String, String)],
) -> Vec<(String, String)> {
    use std::collections::HashMap;
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
    for (dir, config, bundle) in declared(root, current.iter().map(String::as_str)) {
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
