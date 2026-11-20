//! Source observations for literal settings keys, not physical declarations.
use engram_graph::{GraphStore, Node};
use serde_json::Value;
use std::{collections::BTreeSet, path::Path};

pub(super) fn field<'a>(node: &'a Node, key: &str) -> &'a str {
    node.metadata
        .as_ref()
        .and_then(|m| m.get(key))
        .and_then(Value::as_str)
        .unwrap_or("")
}

pub(super) fn axis_label(node: &Node) -> String {
    format!(
        "{} (literal key; getter {}; indexed call line {}; store binding/value unverified)",
        node.name,
        field(node, "literal_setting_getters"),
        node.start_line
    )
}

pub(super) fn lookup(
    graph: &GraphStore,
    pid: &str,
    root: &Path,
    query: &str,
) -> Result<Option<String>, String> {
    let nodes = graph
        .query_nodes(pid, Some("setting_key"), Some(query), None, 5001)
        .map_err(|e| format!("Literal setting lookup failed: {e}"))?;
    if nodes.len() > 5000 {
        return Err("Literal setting scan exceeded 5000 observations; narrow the indexed project before resolving a key".into());
    }
    let candidates: Vec<_> = nodes
        .into_iter()
        .filter(|n| n.name == query || field(n, "literal_setting_key") == query)
        .collect();
    if candidates.is_empty() {
        return Ok(None);
    }
    let names: BTreeSet<_> = candidates.iter().map(|n| &n.name).collect();
    if names.len() != 1 {
        return Err(format!(
            "AMBIGUOUS literal key: use an exact catalog label. Candidates: {}",
            names
                .into_iter()
                .take(10)
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(Some(render_observations(graph, pid, root, &candidates, false)?))
}

pub(super) fn render_observations(
    graph: &GraphStore,
    pid: &str,
    root: &Path,
    candidates: &[Node],
    published_source_read: bool,
) -> Result<String, String> {
    let mut out = format!(
        "# Literal setting: `{}`\n\n{} indexed call-site observation(s). These are name-shaped typed getter calls, not web.config declarations, compiler/store binding, current values or proof of execution. Observations with the same label do not prove one physical backing store.\n",
        candidates[0].name,
        candidates.len()
    );
    for node in candidates.iter().take(50) {
        let hash = graph
            .get_node(pid, &format!("file:{}", node.file_path))
            .map_err(|e| format!("File evidence lookup failed: {e}"))?
            .filter(|file| file.generation == node.generation)
            .and_then(|file| {
                file.metadata.and_then(|m| {
                    m.get("file_hash")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
            });
        let source = (|| -> anyhow::Result<Vec<u8>> {
            let path = engram_core::safe_join(root, node.file_path.as_str())?;
            anyhow::ensure!(
                std::fs::metadata(&path)?.len() <= 2 * 1024 * 1024,
                "source exceeds per-file lookup bound"
            );
            use std::io::Read;
            let mut bytes = Vec::new();
            std::fs::File::open(path)?
                .take(2 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)?;
            anyhow::ensure!(
                bytes.len() <= 2 * 1024 * 1024,
                "source exceeds per-file lookup bound"
            );
            Ok(bytes)
        })();
        let current = source
            .ok()
            .filter(|bytes| hash.as_deref() == Some(blake3::hash(bytes).to_hex().as_str()));
        out.push_str(&format!(
            "\n- {}:{} | getter `{}` | indexed generation {} | indexed raw BLAKE3 {}\n",
            node.file_path,
            node.start_line,
            field(node, "literal_setting_getters"),
            node.generation,
            hash.as_deref().unwrap_or("unavailable")
        ));
        if let Some(bytes) = current {
            let source = String::from_utf8_lossy(&bytes);
            if let Some(line) = source
                .lines()
                .nth(node.start_line.saturating_sub(1) as usize)
            {
                let excerpt: String = line.chars().take(800).collect();
                out.push_str(&format!(
                    "  Source status: current at observation; call-site excerpt: {excerpt}\n"
                ));
                if published_source_read && node.start_line > 0 && let Some(hash) = &hash {
                    let action = serde_json::json!({
                        "tool": "get_full_method_body",
                        "arguments": {
                            "project_id": pid,
                            "file_path": node.file_path.as_str(),
                            "line_start": node.start_line,
                            "line_end": node.start_line,
                            "expected_file_blake3": hash,
                            "context_lines": 0,
                            "output_json": true,
                        },
                    });
                    out.push_str(&format!("  Source read: {action}\n"));
                }
            }
        } else {
            out.push_str("  Source status: STALE_OR_UNAVAILABLE; current call-site excerpt withheld. Refresh the index and inspect the full source before relying on this indexed observation.\n");
        }
    }
    if candidates.len() > 50 {
        out.push_str(
            "\nINCOMPLETE: call-site output capped at50; more indexed observations exist.\n",
        );
    }
    out.push_str("\n## Test guidance\nInspect the getter and consuming conditions before choosing cases. GetText/GetInt/etc. are observed names, not verified return types; do not assume every setting is Boolean. Consider valid, missing and boundary values only where the actual task/source supports them. Independent source observations are not one atomic snapshot; no inferred business rule or test execution is supplied.\n");
    Ok(out)
}
