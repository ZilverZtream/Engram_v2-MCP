use crate::utils::now_ms;
use crate::utils::text::contains_word;
use std::collections::{HashMap, HashSet};

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

/// Post-ingest: resolve App_Code global FQN references for legacy WebForms projects.
///
/// In ASP.NET WebForms, any class placed in the `App_Code/` folder is globally available
/// without explicit `Imports` or `using` statements. This function:
///
/// 1. Collects all symbol nodes (class, function) whose file_path is under `App_Code/`.
/// 2. Builds a lookup table: `short_name` → `node_id` (case-insensitive for VB support).
/// 3. Finds all unresolved edges (target_id starts with `::`) in the graph.
/// 4. For each unresolved target, checks if it matches an App_Code symbol.
/// 5. Creates a resolved Dependency edge from the source to the App_Code symbol.
///
/// Returns the number of newly resolved edges.
pub fn resolve_app_code_globals(
    graph: &engram_graph::GraphStore,
    project_id: &str,
    generation: u64,
) -> anyhow::Result<usize> {
    // Step 1: Collect all nodes under App_Code/
    // We check for both "App_Code/" and "app_code/" since path casing varies.
    let all_classes = graph.query_nodes(project_id, Some("class"), None, None, 10_000)?;
    let all_functions = graph.query_nodes(project_id, Some("function"), None, None, 50_000)?;

    let mut app_code_by_name_ci: HashMap<String, String> = HashMap::new();
    let mut app_code_by_name: HashMap<String, String> = HashMap::new();

    let is_app_code_path = |path: &str| -> bool {
        let lower = path.to_lowercase().replace('\\', "/");
        lower.starts_with("app_code/") || lower.contains("/app_code/")
    };

    for node in all_classes.iter().chain(all_functions.iter()) {
        if !is_app_code_path(node.file_path.as_str()) {
            continue;
        }
        // Use the node's name (short name) as the lookup key
        app_code_by_name_ci.insert(node.name.to_lowercase(), node.node_id.clone());
        app_code_by_name.insert(node.name.clone(), node.node_id.clone());

        // Also expose FQN components: if node_id is "sym:class:Namespace.ClassName",
        // register both "ClassName" and the full FQN
        if let Some(fqn) = node
            .metadata
            .as_ref()
            .and_then(|m| m.get("fqn"))
            .and_then(|v| v.as_str())
        {
            if let Some(short) = fqn.split('.').next_back() {
                app_code_by_name_ci.insert(short.to_lowercase(), node.node_id.clone());
                app_code_by_name.insert(short.to_string(), node.node_id.clone());
            }
            // Also register the full FQN for direct lookups
            app_code_by_name_ci.insert(fqn.to_lowercase(), node.node_id.clone());
            app_code_by_name.insert(fqn.to_string(), node.node_id.clone());
        }
    }

    if app_code_by_name.is_empty() {
        return Ok(0);
    }

    tracing::info!(
        "resolve_app_code_globals: found {} App_Code symbols for {}",
        app_code_by_name.len(),
        project_id
    );

    // Step 2: Find all unresolved edges (target_id starts with "::")
    // These are edges where the call resolver couldn't find a target.
    let all_dep_edges = graph.list_edges(project_id, Some(engram_graph::EdgeKind::Dependency))?;

    let mut new_edges: Vec<engram_graph::Edge> = Vec::new();
    let mut resolved_set: HashSet<(String, String)> = HashSet::new();

    for edge in &all_dep_edges {
        if !edge.target_id.starts_with("::") {
            continue;
        }

        let unresolved_name = edge.target_id.trim_start_matches(':');
        if unresolved_name.is_empty() {
            continue;
        }

        // Try exact match first, then case-insensitive
        let resolved_target = app_code_by_name
            .get(unresolved_name)
            .or_else(|| app_code_by_name_ci.get(&unresolved_name.to_lowercase()));

        if let Some(target_id) = resolved_target {
            let pair = (edge.source_id.clone(), target_id.clone());
            if !resolved_set.insert(pair) {
                continue;
            }

            let mut meta = serde_json::Map::new();
            meta.insert(
                "resolved_from".into(),
                serde_json::Value::String("app_code".into()),
            );
            meta.insert(
                "original_target".into(),
                serde_json::Value::String(edge.target_id.clone()),
            );

            new_edges.push(engram_graph::Edge {
                source_id: edge.source_id.clone(),
                target_id: target_id.clone(),
                namespace: engram_core::namespaces::NAMESPACE_MEMORY.into(),
                language: edge.language.clone(),
                edge_kind: engram_graph::EdgeKind::Dependency,
                weight: 1,
                generation,
                metadata: Some(serde_json::Value::Object(meta)),
                updated_at_ms: now_ms(),
            });
        }
    }

    let count = new_edges.len();
    if !new_edges.is_empty() {
        graph.upsert_edges(project_id, &new_edges)?;
        tracing::info!(
            "resolve_app_code_globals: resolved {} edges to App_Code symbols for {}",
            count,
            project_id
        );
    }
    Ok(count)
}

/// Post-ingest: link data-binding field nodes to database column nodes.
///
/// When `<%# Eval("FieldName") %>` is found in markup, the extractor creates a
/// `binding_field:FieldName` virtual node. This function checks if any `db_column`
/// nodes have a matching name and creates `DataBinding` edges to link them.
pub fn link_binding_fields_to_columns(
    graph: &engram_graph::GraphStore,
    project_id: &str,
    generation: u64,
) -> anyhow::Result<usize> {
    let binding_nodes = graph.query_nodes(project_id, Some("binding_field"), None, None, 5000)?;
    if binding_nodes.is_empty() {
        return Ok(0);
    }

    // Build column lookup: column_name_lower → node_id
    let column_node_ids = graph.list_node_ids(project_id, Some("db_column"))?;
    let mut column_by_name: HashMap<String, String> = HashMap::new();
    for cid in &column_node_ids {
        // column:tablename:colname → extract colname
        if let Some((_table_col, col)) = cid.rsplit_once(':') {
            column_by_name.insert(col.to_string(), cid.clone());
        }
    }

    if column_by_name.is_empty() {
        return Ok(0);
    }

    let mut new_edges: Vec<engram_graph::Edge> = Vec::new();
    let mut linked: HashSet<(String, String)> = HashSet::new();

    for bf_node in &binding_nodes {
        let field_lower = bf_node.name.to_lowercase();
        if let Some(col_id) = column_by_name.get(&field_lower) {
            if linked.insert((bf_node.node_id.clone(), col_id.clone())) {
                new_edges.push(engram_graph::Edge {
                    source_id: bf_node.node_id.clone(),
                    target_id: col_id.clone(),
                    namespace: engram_core::namespaces::NAMESPACE_MEMORY.into(),
                    language: "aspx".into(),
                    edge_kind: engram_graph::EdgeKind::DataBinding,
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
            "link_binding_fields_to_columns: created {} DataBinding edges for {}",
            count,
            project_id
        );
    }
    Ok(count)
}
