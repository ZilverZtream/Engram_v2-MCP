//! Source evidence for modern state, using the existing graph edge and file receipt.
use engram_graph::{EdgeKind, GraphStore, Node};

pub(super) fn is_modern(category: &str) -> bool {
    matches!(
        category,
        "Environment" | "Module" | "Singleton" | "ReactContext" | "Redux" | "LocalStorage" | "SessionStorage"
    )
}

pub(super) fn evidence(
    graph: &GraphStore,
    project: &str,
    source: &Node,
    target: &str,
    kind: EdgeKind,
) -> Result<String, String> {
    let edges = graph
        .get_edges_by_endpoints(project, &[(source.node_id.clone(), kind, target.into())])
        .map_err(|error| format!("State evidence lookup failed: {error}"))?;
    let Some(metadata) = edges.first().and_then(|edge| edge.metadata.as_ref()) else {
        return Ok("  Indexed access evidence unavailable.\n".into());
    };
    let file = graph
        .get_node(
            project,
            &engram_core::NodeId::file(source.file_path.as_str()).0,
        )
        .map_err(|error| format!("State coverage lookup failed: {error}"))?;
    let coverage = file
        .as_ref()
        .and_then(|file| file.metadata.as_ref())
        .and_then(|metadata| metadata.get(engram_index::source_contract::MODERN_STATE_KEY))
        .and_then(|value| value.as_str())
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
    let coverage = coverage.unwrap_or(serde_json::Value::Null);
    let issues = coverage["issues"]
        .as_array()
        .map(|issues| issues.iter().take(8).cloned().collect::<Vec<_>>());
    let omitted = coverage["issues"]
        .as_array()
        .map_or(0, |issues| issues.len().saturating_sub(8));
    let detail = serde_json::json!({
        "file": source.file_path.as_str(),
        "access_line": metadata["state_access_line"],
        "access_column": metadata["state_access_column"],
        "access_byte": metadata["state_access_byte"],
        "access_end_byte": metadata["state_access_end_byte"],
        "evidence": metadata["state_evidence"],
        "import_bindings": metadata[engram_core::modern_state::BINDING_KEY].as_array().map(|bindings| bindings.iter().take(8).collect::<Vec<_>>()),
        "additional_import_bindings": metadata[engram_core::modern_state::BINDING_KEY].as_array().map_or(0, |bindings| bindings.len().saturating_sub(8)),
        "indexed_source_hash": coverage["source_hash"],
        "coverage": coverage["status"],
        "issues": issues,
        "additional_issues": omitted
    });
    Ok(format!("  Indexed access: {detail}\n"))
}
