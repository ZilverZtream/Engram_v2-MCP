//! Structural companions have independent evidence even when Git treats the output as a hub.
use regex::Regex;
use std::collections::{BTreeSet, HashSet};
use std::path::Path;
use std::sync::LazyLock;

// Follow local TypeScript reference directives from explicit compilation roots.
// This also resolves legacy outFile projects whose include list names a single entry point.
fn reference_includes(root: &Path, dir: &Path, config: &serde_json::Value, edited: &Path) -> bool {
    static REFERENCE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?im)^\s*///\s*<reference\s+path\s*=\s*["']([^"']+)["']"#).unwrap()
    });
    let mut pending = Vec::new();
    let mut visited = HashSet::new();
    let Ok(edited) = edited.canonicalize() else {
        return false;
    };
    for key in ["files", "include"] {
        for entry in config[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str())
        {
            if !entry.contains(['*', '?']) {
                pending.push(dir.join(entry));
            }
        }
    }
    while let Some(file) = pending.pop() {
        let Ok(file) = file.canonicalize() else {
            continue;
        };
        if !file.starts_with(root) || !visited.insert(file.clone()) {
            continue;
        }
        if file == edited {
            return true;
        }
        if visited.len() > 512 {
            break;
        }
        let Some(rel) = file.strip_prefix(root).ok().and_then(|p| p.to_str()) else {
            continue;
        };
        let Ok(text) = engram_core::safe_read_to_string(root, rel) else {
            continue;
        };
        if let Some(parent) = file.parent() {
            pending.extend(REFERENCE.captures_iter(&text).map(|m| parent.join(&m[1])));
        }
    }
    false
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
    for (source, path) in edits {
        let lower = path.to_ascii_lowercase();
        let mut add = |candidate: &str, reason| {
            let key = super::edit_path_key(candidate);
            if !edited.contains(&key)
                && let Some(actual) = indexed.get(&key)
            {
                result.insert((source.clone(), (*actual).clone(), reason));
            }
        };
        if lower.ends_with(".ts") || lower.ends_with(".tsx") {
            for dir in Path::new(path).ancestors().skip(1) {
                let cfg = dir.join("tsconfig.json");
                let Some(cfg_str) = cfg.to_str() else {
                    continue;
                };
                let Ok(text) = engram_core::safe_read_to_string(&root, cfg_str) else {
                    continue;
                };
                static OUTPUT: LazyLock<Regex> =
                    LazyLock::new(|| Regex::new(r#""outFile"\s*:\s*"([^"]+)""#).unwrap());
                if let Some(m) = OUTPUT.captures(&text)
                    && let Ok(target) = root.join(dir).join(&m[1]).canonicalize()
                    && let Ok(rel) = target.strip_prefix(&root)
                {
                    let cfg = serde_json::from_str::<serde_json::Value>(&text).unwrap_or_default();
                    let linked = reference_includes(&root, &root.join(dir), &cfg, &root.join(path));
                    let reason = if linked {
                        "compiled output of an entry point referencing the edited source"
                    } else {
                        "output declared by the nearest tsconfig; confirm source inclusion"
                    };
                    add(&rel.to_string_lossy().replace('\\', "/"), reason);
                }
                break;
            }
        }
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
