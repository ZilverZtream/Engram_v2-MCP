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
