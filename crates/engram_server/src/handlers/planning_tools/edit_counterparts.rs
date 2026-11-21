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
    let mut configs = BTreeSet::new();
    for p in &sources {
        for parent in p.ancestors().skip(1) {
            configs.insert(parent.join("tsconfig.json"));
        }
    }
    configs.extend(
        current
            .iter()
            .filter(|p| {
                Path::new(p)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s.starts_with("tsconfig") && s.ends_with(".json"))
            })
            .map(PathBuf::from),
    );
    let mut refs: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    let mut result = Vec::new();
    for cfg in configs {
        let Some(dir) = cfg.parent() else { continue };
        let Some(text) = cfg
            .to_str()
            .and_then(|p| engram_core::safe_read_to_string(root, p).ok())
        else {
            continue;
        };
        let Some(config) = parse_config(&text) else {
            continue;
        };
        let Some(output) = config["compilerOptions"]["outFile"].as_str() else {
            continue;
        };
        let directory = root.join(dir);
        let Ok(target) = directory.join(output).canonicalize() else {
            continue;
        };
        let Ok(bundle) = target.strip_prefix(root) else {
            continue;
        };
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
            let Ok(local) = p.strip_prefix(dir) else {
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
                result.push(((*src).clone(), bundle.to_string_lossy().replace('\\', "/")));
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
) -> BTreeSet<(String, String, &'static str)> {
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
                    "compiled output of an entry point referencing the edited source",
                ));
            }
        }
        let mut add = |candidate: &str, reason| {
            let key = super::edit_path_key(candidate);
            if !edited.contains(&key)
                && let Some(actual) = indexed.get(&key)
            {
                result.insert((source.clone(), (*actual).clone(), reason));
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
        if let Some(stem) = lower.strip_suffix(".resx") {
            fn family(stem: &str) -> &str {
                static CULTURE: LazyLock<Regex> =
                    LazyLock::new(|| Regex::new(r"(?i)\.[a-z]{2,3}(?:-[a-z0-9]{2,8})*$").unwrap());
                CULTURE.find(stem).map_or(stem, |m| &stem[..m.start()])
            }
            let base = family(stem);
            for file in current {
                if file
                    .to_ascii_lowercase()
                    .strip_suffix(".resx")
                    .is_some_and(|s| family(s) == base)
                {
                    add(file, "sibling locale resource");
                }
            }
        }
    }
    result
}
