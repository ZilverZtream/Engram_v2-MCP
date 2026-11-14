//! Request-local function inventories for planning's selected files.
//! Reuses the existing streaming graph projection; no cache or index changes.
use engram_graph::{GraphStore, Node};
use std::collections::BTreeMap;

#[derive(Clone, Copy)]
pub(super) enum LimitCounts {
    /// Pattern shapes historically cap all file nodes before selecting functions.
    AllNodes,
    /// Source method evidence caps only function nodes.
    Functions,
}

pub(super) fn collect(
    graph: &GraphStore,
    project: &str,
    paths: &[String],
    limit_per_file: usize,
    counts: LimitCounts,
) -> anyhow::Result<BTreeMap<String, Vec<Node>>> {
    let mut files: BTreeMap<_, (String, usize, Vec<Node>)> = paths
        .iter()
        .map(|path| {
            let normalized = path.replace('\\', "/");
            (
                normalized.clone(),
                (normalized.to_lowercase(), 0, Vec::new()),
            )
        })
        .collect();
    if files.is_empty() {
        return Ok(BTreeMap::new());
    }
    graph.map_nodes(project, |mut node| {
        if matches!(counts, LimitCounts::Functions) && node.node_type != "function" {
            return;
        }
        // Preserve query_nodes' case-insensitive substring candidates. A page
        // query also supplies its code-behind; source method admission retains
        // its existing exact-file check after this candidate budget.
        let path = node
            .file_path
            .as_str()
            .replace('\\', "/")
            .to_ascii_lowercase();
        node.metadata = None;
        for (query, seen, functions) in files.values_mut() {
            if *seen >= limit_per_file || !path.contains(query.as_str()) {
                continue;
            }
            *seen += 1;
            if node.node_type == "function" {
                functions.push(node.clone());
            }
        }
    })?;
    Ok(files
        .into_iter()
        .map(|(path, (_, _, functions))| (path, functions))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::RelPath;

    fn node(id: &str, kind: &str, path: &str) -> Node {
        Node {
            node_id: id.into(),
            node_type: kind.into(),
            name: id.into(),
            namespace: "memory".into(),
            language: "vbnet".into(),
            file_path: RelPath::new(path),
            start_line: 3,
            end_line: 5,
            generation: 7,
            metadata: None,
        }
    }

    #[test]
    fn file_functions_preserve_all_node_budget_before_pattern_function_selection() {
        let temp = tempfile::tempdir().unwrap();
        let graph = GraphStore::open(&temp.path().join("graph.redb")).unwrap();
        graph
            .upsert_nodes(
                "project",
                &[
                    node("00-class", "class", "A.vb"),
                    node("01-first", "function", "A.vb"),
                    node("02-second", "function", "A.vb"),
                    node("03-foreign", "function", "Copy/A.vb"),
                ],
            )
            .unwrap();
        let rows = collect(
            &graph,
            "project",
            &["A.vb".into()],
            2,
            LimitCounts::AllNodes,
        )
        .unwrap();
        assert_eq!(
            rows["A.vb"]
                .iter()
                .map(|n| n.node_id.as_str())
                .collect::<Vec<_>>(),
            vec!["01-first"]
        );
    }

    #[test]
    fn file_functions_keep_independent_function_budgets_and_stored_order() {
        let temp = tempfile::tempdir().unwrap();
        let graph = GraphStore::open(&temp.path().join("graph.redb")).unwrap();
        graph
            .upsert_nodes(
                "project",
                &[
                    node("00-class", "class", "A.vb"),
                    node("01-a", "function", "A.vb"),
                    node("02-b", "function", "A.vb"),
                    node("03-c", "function", "B.vb"),
                    node("04-d", "function", "B.vb"),
                    node("05-e", "function", "B.vb"),
                    node("06-foreign", "function", "Copy/A.vb"),
                ],
            )
            .unwrap();
        let rows = collect(
            &graph,
            "project",
            &["A.vb".into(), "B.vb".into(), "Empty.vb".into()],
            2,
            LimitCounts::Functions,
        )
        .unwrap();
        assert_eq!(
            rows["A.vb"]
                .iter()
                .map(|n| n.node_id.as_str())
                .collect::<Vec<_>>(),
            vec!["01-a", "02-b"]
        );
        assert_eq!(
            rows["B.vb"]
                .iter()
                .map(|n| n.node_id.as_str())
                .collect::<Vec<_>>(),
            vec!["03-c", "04-d"]
        );
        assert!(rows["Empty.vb"].is_empty());
        assert!(
            rows.values()
                .flatten()
                .all(|n| n.generation == 7 && n.start_line == 3)
        );
    }
    #[test]
    fn file_functions_preserve_page_and_codebehind_candidates() {
        let temp = tempfile::tempdir().unwrap();
        let graph = GraphStore::open(&temp.path().join("graph.redb")).unwrap();
        graph
            .upsert_nodes(
                "project",
                &[
                    node("00-page", "page", "Page.aspx"),
                    node("10-script", "function", "Page.aspx"),
                    node("20-handler", "function", "Page.aspx.vb"),
                ],
            )
            .unwrap();
        let rows = collect(
            &graph,
            "project",
            &["Page.aspx".into(), "Page.aspx.vb".into()],
            500,
            LimitCounts::AllNodes,
        )
        .unwrap();
        assert_eq!(
            rows["Page.aspx"]
                .iter()
                .map(|n| n.node_id.as_str())
                .collect::<Vec<_>>(),
            vec!["10-script", "20-handler"]
        );
        assert_eq!(
            rows["Page.aspx.vb"]
                .iter()
                .map(|n| n.node_id.as_str())
                .collect::<Vec<_>>(),
            vec!["20-handler"]
        );
    }
}
