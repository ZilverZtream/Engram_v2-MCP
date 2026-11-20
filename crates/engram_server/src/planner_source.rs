//! File-backed planner witnesses are admitted only when their bytes match the indexed file.
use engram_graph::GraphStore;
use std::{io::Read, path::Path};

pub(crate) fn read(
    graph: &GraphStore,
    project: &str,
    root: &Path,
    path: &str,
) -> anyhow::Result<String> {
    let node = graph
        .get_node(project, &format!("file:{path}"))?
        .ok_or_else(|| anyhow::anyhow!("Missing indexed file: {path}"))?;
    let expected = node
        .metadata
        .as_ref()
        .and_then(|m| m["file_hash"].as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing file fingerprint: {path}"))?;
    let mut bytes = Vec::new();
    const MAX: u64 = 4 * 1024 * 1024;
    std::fs::File::open(engram_core::safe_join(root, path)?)?
        .take(MAX + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= MAX as usize && blake3::hash(&bytes).to_hex().as_str() == expected,
        "File exceeds the planner read limit or differs from its indexed fingerprint: {path}"
    );
    Ok(String::from_utf8(bytes)?)
}
