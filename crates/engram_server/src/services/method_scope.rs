//! Caller-selected declaration identities; source admission stays with consumers.
use crate::models::MethodScope;
use engram_graph::Node;
pub(crate) fn validate(scopes: &[MethodScope], files: &[String]) -> anyhow::Result<()> {
    anyhow::ensure!(
        (1..=16).contains(&scopes.len()),
        "methods must contain1..16 exact declarations"
    );
    let mut seen = std::collections::BTreeSet::new();
    for scope in scopes {
        anyhow::ensure!(
            scope.line_start > 0 && files.contains(&scope.file_path),
            "Each method needs a positive line_start and an exact file_path in files"
        );
        anyhow::ensure!(
            seen.insert((&scope.file_path, scope.line_start)),
            "Duplicate method declaration scope"
        );
    }
    Ok(())
}
pub(crate) fn declarations<'a>(
    nodes: &'a [Node],
    file: &str,
    scopes: &[MethodScope],
) -> anyhow::Result<Vec<&'a Node>> {
    let mut selected = Vec::new();
    for scope in scopes.iter().filter(|scope| scope.file_path == file) {
        let candidates: Vec<_> = nodes
            .iter()
            .filter(|n| n.node_type == "function" && n.start_line == scope.line_start)
            .collect();
        anyhow::ensure!(
            candidates.len() == 1 && candidates[0].end_line >= scope.line_start,
            "{file}:{} does not identify one indexed method declaration; inspect get_method_info and source freshness",
            scope.line_start
        );
        selected.push(candidates[0]);
    }
    Ok(selected)
}
