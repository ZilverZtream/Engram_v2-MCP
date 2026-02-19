use crate::utils::now_ms;
use crate::utils::text::contains_word;

/// Post-ingest: link SQL nodes (stored_proc, inline_sql) to db_table nodes via QueriesTable edges.
///
/// 1. Collect all `db_table` node names into a lookup set.
/// 2. Collect all `stored_proc` and `inline_sql` nodes.
/// 3. For each SQL node, check if its name/metadata references any known table.
/// 4. Create `QueriesTable` edges from SQL node -> table node.
pub fn link_sql_to_schema(
    graph: &engram_graph::GraphStore,
    project_id: &str,
    generation: u64,
) -> anyhow::Result<usize> {
    use std::collections::HashSet;

    let table_node_ids = graph.list_node_ids(project_id, Some("db_table"))?;
    if table_node_ids.is_empty() {
        return Ok(0);
    }

    let mut table_name_to_id: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for tid in &table_node_ids {
        if let Some(name) = tid.strip_prefix("table:") {
            table_name_to_id.insert(name.to_string(), tid.clone());
        }
    }

    if table_name_to_id.is_empty() {
        return Ok(0);
    }

    let mut sql_nodes: Vec<engram_graph::Node> = Vec::new();
    sql_nodes.extend(graph.query_nodes(project_id, Some("stored_proc"), None, None, 5000)?);
    sql_nodes.extend(graph.query_nodes(project_id, Some("inline_sql"), None, None, 5000)?);

    if sql_nodes.is_empty() {
        return Ok(0);
    }

    let mut new_edges: Vec<engram_graph::Edge> = Vec::new();
    let mut linked: HashSet<(String, String)> = HashSet::new();

    for sql_node in &sql_nodes {
        let sql_name_lower = sql_node.name.to_lowercase();
        let metadata_str = sql_node
            .metadata
            .as_ref()
            .map(|m| m.to_string().to_lowercase())
            .unwrap_or_default();

        for (table_name, table_id) in &table_name_to_id {
            let matches = contains_word(&sql_name_lower, table_name)
                || contains_word(&metadata_str, table_name);

            if matches && linked.insert((sql_node.node_id.clone(), table_id.clone())) {
                new_edges.push(engram_graph::Edge {
                    source_id: sql_node.node_id.clone(),
                    target_id: table_id.clone(),
                    namespace: engram_core::namespaces::NAMESPACE_MEMORY.into(),
                    language: "sql".into(),
                    edge_kind: engram_graph::EdgeKind::QueriesTable,
                    weight: 1,
                    generation,
                    metadata: None,
                    updated_at_ms: now_ms(),
                });
            }
        }
    }

    let count = new_edges.len();
    if !new_edges.is_empty() {
        graph.upsert_edges(project_id, &new_edges)?;
        tracing::info!(
            "link_sql_to_schema: created {} QueriesTable edges for {}",
            count,
            project_id
        );
    }
    Ok(count)
}
