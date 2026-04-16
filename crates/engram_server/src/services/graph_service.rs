use crate::utils::now_ms;
use crate::utils::text::contains_word;
use engram_graph::store::ResolveResult;
use std::collections::{HashMap, HashSet, VecDeque};

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
    let app_code_function_kinds = ["function", "method", "sub", "procedure"];
    fn strip_line_suffix(s: &str) -> &str {
        // If the string ends in ":<digits>", trim it.
        if let Some((head, tail)) = s.rsplit_once(':')
            && !tail.is_empty()
            && tail.bytes().all(|b| b.is_ascii_digit())
        {
            return head;
        }
        s
    }
    fn extract_terminal_name(target_id: &str) -> Option<&str> {
        // Strip the "sym:<kind>:" prefix when present.
        let rest = target_id
            .strip_prefix("sym:function:")
            .or_else(|| target_id.strip_prefix("sym:class:"))
            .unwrap_or(target_id);

        // Case A: path-shaped composite "<path>:<name>:<line>".
        let segs: Vec<&str> = rest.split(':').collect();
        if segs.len() >= 3
            && segs.last().is_some_and(|seg| seg.parse::<u64>().is_ok())
            && let Some(name) = segs.get(segs.len() - 2).copied()
            && !name.is_empty()
        {
            return Some(name);
        }

        // Case B: dotted FQN "Namespace.Type.Method".
        if let Some(last) = rest.rsplit('.').next()
            && !last.is_empty()
        {
            return Some(last);
        }

        // Case C: plain bare name.
        (!rest.is_empty()).then_some(rest)
    }
    let unresolved_target_name = |target_id: &str| -> Option<String> {
        if target_id.starts_with("::") {
            return Some(target_id.trim_start_matches(':').to_string());
        }
        if let Some(stripped) = target_id.strip_prefix("sym:function:") {
            let mut parts = stripped.rsplitn(2, ':');
            let line = parts.next().unwrap_or_default();
            let name = parts.next().unwrap_or_default();
            if line == "0" && !name.is_empty() {
                return Some(name.to_string());
            }
        }
        None
    };

    // Step 1: Collect all nodes under App_Code/
    // We check for both "App_Code/" and "app_code/" since path casing varies.
    let all_classes = graph.query_nodes(project_id, Some("class"), None, None, 10_000)?;
    let all_functions = graph.query_nodes(project_id, Some("function"), None, None, 50_000)?;

    let mut app_code_by_name_ci: HashMap<String, String> = HashMap::new();
    let mut app_code_by_name: HashMap<String, String> = HashMap::new();
    let mut terminal_to_fqn: HashMap<String, Vec<String>> = HashMap::new();

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
        let inferred_fqn = node
            .metadata
            .as_ref()
            .and_then(|m| m.get("fqn"))
            .and_then(|v| v.as_str())
            .map(|f| f.to_string())
            .or_else(|| {
                node.node_id
                    .strip_prefix("sym:")
                    .and_then(|rest| rest.split_once(':'))
                    .and_then(|(_, maybe_fqn)| {
                        maybe_fqn.contains('.').then(|| maybe_fqn.to_string())
                    })
            })
            .or_else(|| node.name.contains('.').then(|| node.name.clone()));
        if let Some(fqn) = inferred_fqn {
            if let Some(short_raw) = fqn.split('.').next_back() {
                let short = strip_line_suffix(short_raw);
                if short.is_empty() {
                    continue;
                }
                app_code_by_name_ci.insert(short.to_lowercase(), node.node_id.clone());
                app_code_by_name.insert(short.to_string(), node.node_id.clone());

                let lowered_node_type = node.node_type.to_ascii_lowercase();
                if app_code_function_kinds.contains(&lowered_node_type.as_str()) {
                    terminal_to_fqn
                        .entry(short.to_string())
                        .or_default()
                        .push(fqn.to_string());
                }
            }
            // Also register the full FQN for direct lookups
            let normalized_fqn = strip_line_suffix(&fqn);
            app_code_by_name_ci.insert(normalized_fqn.to_lowercase(), node.node_id.clone());
            app_code_by_name.insert(normalized_fqn.to_string(), node.node_id.clone());
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
    let all_call_edges = graph.list_edges(project_id, Some(engram_graph::EdgeKind::Calls))?;

    let mut new_edges: Vec<engram_graph::Edge> = Vec::new();
    let mut resolved_set: HashSet<(String, String)> = HashSet::new();

    for edge in all_dep_edges.iter().chain(all_call_edges.iter()) {
        let Some(unresolved_name) = unresolved_target_name(&edge.target_id) else {
            continue;
        };

        // Try exact match first, then case-insensitive
        let resolved_target = app_code_by_name
            .get(unresolved_name.as_str())
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

    // Step 3: Rewrite unqualified call edges (`::Foo`) to qualified App_Code FQNs
    // when there is exactly one matching App_Code function terminal.
    tracing::info!(
        project_id = %project_id,
        terminal_to_fqn_len = terminal_to_fqn.len(),
        app_code_symbol_count = app_code_by_name.len(),
        "resolve_app_code_globals: step3_lookup_sizes"
    );
    if terminal_to_fqn.is_empty() {
        return Ok(count);
    }

    for fqns in terminal_to_fqn.values_mut() {
        fqns.sort();
        fqns.dedup();
    }

    let call_edges = graph.list_edges(project_id, Some(engram_graph::EdgeKind::Calls))?;
    let mut rewritten_edges: Vec<engram_graph::Edge> = Vec::new();
    let mut rewritten = 0usize;
    let mut ambiguous = 0usize;
    let mut unmatched = 0usize;
    let mut skipped_empty = 0usize;
    let mut skipped_already_app_code = 0usize;
    let mut no_terminal_match = 0usize;
    let mut fqn_not_in_node_map = 0usize;
    let sample_target_ids: Vec<String> = call_edges
        .iter()
        .take(3)
        .map(|edge| edge.target_id.clone())
        .collect();
    let sample_terminals: Vec<String> = terminal_to_fqn.keys().take(3).cloned().collect();
    tracing::info!(
        project_id = %project_id,
        sample_target_ids = ?sample_target_ids,
        sample_terminals = ?sample_terminals,
        "resolve_app_code_globals: step3_samples"
    );

    for edge in &call_edges {
        let Some(bare_name) = extract_terminal_name(&edge.target_id) else {
            no_terminal_match += 1;
            unmatched += 1;
            continue;
        };
        if bare_name.is_empty() {
            skipped_empty += 1;
            continue;
        }
        // Skip edges whose target already resolves to App_Code paths.
        let target_lower = edge.target_id.to_lowercase();
        if edge.target_id.starts_with("sym:function:Site/App_Code/")
            || edge.target_id.starts_with("sym:function:Site\\App_Code\\")
            || target_lower.contains("/app_code/")
            || target_lower.contains("\\app_code\\")
        {
            skipped_already_app_code += 1;
            continue;
        }

        let bare_name = strip_line_suffix(bare_name);
        match terminal_to_fqn.get(bare_name) {
            Some(matches) if matches.len() == 1 => {
                let matched_fqn = &matches[0];
                let new_target_id =
                    match graph.resolve_symbol(project_id, matched_fqn, None, None)? {
                        ResolveResult::Unique(node) => node.node_id,
                        _ => {
                            unmatched += 1;
                            fqn_not_in_node_map += 1;
                            continue;
                        }
                    };

                if edge.target_id == new_target_id {
                    continue;
                }

                let mut metadata_obj = edge
                    .metadata
                    .clone()
                    .and_then(|m| m.as_object().cloned())
                    .unwrap_or_default();
                metadata_obj.insert(
                    "original_target_name".into(),
                    serde_json::Value::String(bare_name.to_string()),
                );
                metadata_obj.insert(
                    "resolved_target_fqn".into(),
                    serde_json::Value::String(matched_fqn.to_string()),
                );

                let mut rewritten_edge = edge.clone();
                rewritten_edge.target_id = new_target_id;
                rewritten_edge.metadata = Some(serde_json::Value::Object(metadata_obj));
                rewritten_edge.generation = generation;
                rewritten_edge.updated_at_ms = now_ms();
                rewritten_edges.push(rewritten_edge);
                rewritten += 1;
            }
            Some(matches) if matches.len() > 1 => {
                ambiguous += 1;
                tracing::debug!(
                    target_name = bare_name,
                    matching_fqns = ?matches,
                    count = matches.len(),
                    "resolve_app_code_globals: ambiguous_bare_call"
                );
            }
            _ => {
                unmatched += 1;
                no_terminal_match += 1;
            }
        }
    }

    if !rewritten_edges.is_empty() {
        graph.upsert_edges(project_id, &rewritten_edges)?;
    }

    tracing::info!(
        "resolve_app_code_globals: rewrote {} unqualified call edges to qualified FQNs, {} ambiguous unchanged, {} unmatched, skipped_empty={}, skipped_already_app_code={}, no_terminal_match={}, fqn_not_in_node_map={}",
        rewritten,
        ambiguous,
        unmatched,
        skipped_empty,
        skipped_already_app_code,
        no_terminal_match,
        fqn_not_in_node_map
    );
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
        if let Some(col_id) = column_by_name.get(&field_lower)
            && linked.insert((bf_node.node_id.clone(), col_id.clone()))
        {
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

// -------------------- Migration Slicer --------------------

/// A compiled vertical slice of a legacy feature, ready for modernization.
pub struct MigrationSlice {
    pub entry_node_id: String,
    pub entry_node_type: String,
    pub entry_file: String,
    /// JS/AJAX endpoints and DOM manipulators (ManipulatesDom, TriggersPostback, ApiCall).
    pub frontend_deps: Vec<SliceItem>,
    /// Backend methods reached via Calls/EventWiring/Dependency/Contains.
    pub backend_methods: Vec<SliceItem>,
    /// Session/ViewState/Cookie mutations (ReadsState, WritesState).
    pub state_mutations: Vec<SliceItem>,
    /// Database tables and columns (SqlCalls, QueriesTable, ReadsColumn, HasColumn).
    pub database_deps: Vec<SliceItem>,
    /// User controls, includes, service endpoints.
    pub component_deps: Vec<SliceItem>,
    /// Data-binding fields (Eval/Bind).
    pub data_bindings: Vec<String>,
    /// Config registrations (modules, handlers, connection strings).
    pub config_deps: Vec<SliceItem>,
    /// Lifecycle metadata collected from traversed nodes.
    pub lifecycle_info: Vec<(String, String, u32)>, // (node_id, stage, sequence)
    /// Side-effect metadata collected from traversed nodes.
    pub side_effects: Vec<(String, String)>, // (node_id, effects)
    /// Number of dead-code nodes skipped during traversal.
    pub dead_code_skipped: usize,
    /// Total unique nodes visited.
    pub nodes_visited: usize,
}

/// A categorized item in the migration slice with source context.
pub struct SliceItem {
    pub node_id: String,
    pub node_type: String,
    pub file_path: String,
    pub edge_kind: String,
    pub depth: usize,
}

/// BFS-traverse the graph from `entry_node_id` up to `max_depth`, collecting
/// the ecosystem of frontend, backend, state, DB, and config dependencies.
pub fn compile_migration_slice(
    graph: &engram_graph::GraphStore,
    project_id: &str,
    entry_node_id: &str,
    max_depth: usize,
) -> anyhow::Result<MigrationSlice> {
    use engram_graph::EdgeKind;

    // Resolve entry node
    let entry = graph
        .get_node(project_id, entry_node_id)?
        .ok_or_else(|| anyhow::anyhow!("Entry node '{}' not found", entry_node_id))?;

    let mut slice = MigrationSlice {
        entry_node_id: entry.node_id.clone(),
        entry_node_type: entry.node_type.clone(),
        entry_file: entry.file_path.as_str().to_string(),
        frontend_deps: Vec::new(),
        backend_methods: Vec::new(),
        state_mutations: Vec::new(),
        database_deps: Vec::new(),
        component_deps: Vec::new(),
        data_bindings: Vec::new(),
        config_deps: Vec::new(),
        lifecycle_info: Vec::new(),
        side_effects: Vec::new(),
        dead_code_skipped: 0,
        nodes_visited: 0,
    };

    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(String, usize)> = VecDeque::new();

    queue.push_back((entry.node_id.clone(), 0));

    // Edge kinds that propagate the BFS (we follow these to deeper nodes)
    let propagating_kinds = [
        EdgeKind::Dependency,
        EdgeKind::Contains,
        EdgeKind::Imports,
        EdgeKind::RegistersControl,
        EdgeKind::IncludesFile,
        EdgeKind::ExposesWebService,
        EdgeKind::ExposesHttpHandler,
        EdgeKind::ExposesWcfService,
    ];

    // All edge kinds we want to collect from each node
    let all_collect_kinds = EdgeKind::ALL;

    while let Some((current_id, depth)) = queue.pop_front() {
        if !visited.insert(current_id.clone()) {
            continue;
        }

        // Check for dead code
        if let Some(node) = graph.get_node(project_id, &current_id)? {
            if node
                .metadata
                .as_ref()
                .and_then(|m| m.get("is_dead_code"))
                .is_some()
            {
                slice.dead_code_skipped += 1;
                continue;
            }

            // Collect lifecycle info from node metadata
            if let Some(meta) = &node.metadata {
                if let Some(stage) = meta.get("lifecycle_stage").and_then(|v| v.as_str()) {
                    let seq = meta
                        .get("lifecycle_sequence")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32;
                    slice
                        .lifecycle_info
                        .push((current_id.clone(), stage.to_string(), seq));
                }
                if let Some(fx) = meta.get("side_effects").and_then(|v| v.as_str())
                    && !fx.is_empty()
                {
                    slice
                        .side_effects
                        .push((current_id.clone(), fx.to_string()));
                }
            }
        }

        slice.nodes_visited += 1;

        // Collect outbound edges across all kinds
        for ek in all_collect_kinds {
            let neighbors = graph
                .neighbors(project_id, ek.clone(), &current_id, 200)
                .unwrap_or_default();

            for (target_id, _weight) in neighbors {
                let target_node = graph.get_node(project_id, &target_id)?;
                let (ttype, tfile) = match &target_node {
                    Some(n) => (n.node_type.clone(), n.file_path.as_str().to_string()),
                    None => ("unknown".to_string(), String::new()),
                };

                let item = SliceItem {
                    node_id: target_id.clone(),
                    node_type: ttype,
                    file_path: tfile,
                    edge_kind: ek.as_str().to_string(),
                    depth,
                };

                match ek {
                    EdgeKind::ManipulatesDom | EdgeKind::TriggersPostback | EdgeKind::ApiCall => {
                        slice.frontend_deps.push(item);
                    }
                    EdgeKind::ReadsState
                    | EdgeKind::WritesState
                    | EdgeKind::UnresolvedStateRead
                    | EdgeKind::UnresolvedStateWrite => {
                        slice.state_mutations.push(item);
                    }
                    EdgeKind::SqlCalls
                    | EdgeKind::QueriesTable
                    | EdgeKind::ReadsColumn
                    | EdgeKind::HasColumn
                    | EdgeKind::ForeignKey => {
                        slice.database_deps.push(item);
                    }
                    EdgeKind::DataBinding => {
                        slice.data_bindings.push(target_id.clone());
                    }
                    EdgeKind::RegistersControl
                    | EdgeKind::IncludesFile
                    | EdgeKind::ExposesWebService
                    | EdgeKind::ExposesHttpHandler
                    | EdgeKind::ExposesWcfService => {
                        slice.component_deps.push(item);
                    }
                    EdgeKind::RegistersModule | EdgeKind::RegistersHandler => {
                        slice.config_deps.push(item);
                    }
                    EdgeKind::Dependency | EdgeKind::Contains | EdgeKind::Imports => {
                        slice.backend_methods.push(item);
                    }
                    // Skip non-structural edges for categorization
                    _ => {}
                }

                // Propagate BFS for structural/dependency edges only
                if depth < max_depth
                    && propagating_kinds.contains(ek)
                    && !visited.contains(&target_id)
                {
                    queue.push_back((target_id, depth + 1));
                }
            }
        }

        // Also check incoming edges at depth 0 to capture JS files
        // that manipulate this entry point
        if depth == 0 {
            for ek in &[
                EdgeKind::ManipulatesDom,
                EdgeKind::TriggersPostback,
                EdgeKind::ApiCall,
            ] {
                let incoming = graph
                    .find_incoming_edges(project_id, Some(ek.clone()), &current_id, 200)
                    .unwrap_or_default();
                for (source_id, _weight) in incoming {
                    let source_node = graph.get_node(project_id, &source_id)?;
                    let (stype, sfile) = match &source_node {
                        Some(n) => (n.node_type.clone(), n.file_path.as_str().to_string()),
                        None => ("unknown".to_string(), String::new()),
                    };
                    slice.frontend_deps.push(SliceItem {
                        node_id: source_id.clone(),
                        node_type: stype,
                        file_path: sfile,
                        edge_kind: format!("incoming_{}", ek.as_str()),
                        depth: 0,
                    });
                    // Follow into JS files too
                    if !visited.contains(&source_id) && max_depth > 0 {
                        queue.push_back((source_id, 1));
                    }
                }
            }
        }
    }

    // Deduplicate all vectors by node_id
    dedup_slice_items(&mut slice.frontend_deps);
    dedup_slice_items(&mut slice.backend_methods);
    dedup_slice_items(&mut slice.state_mutations);
    dedup_slice_items(&mut slice.database_deps);
    dedup_slice_items(&mut slice.component_deps);
    dedup_slice_items(&mut slice.config_deps);
    slice.data_bindings.sort();
    slice.data_bindings.dedup();
    slice.lifecycle_info.sort_by_key(|(_, _, seq)| *seq);

    Ok(slice)
}

fn dedup_slice_items(items: &mut Vec<SliceItem>) {
    let mut seen = HashSet::new();
    items.retain(|item| seen.insert(item.node_id.clone()));
}

/// Format a `MigrationSlice` into a token-efficient Markdown dossier.
pub fn format_migration_blueprint(slice: &MigrationSlice) -> String {
    let mut md = String::with_capacity(4096);

    md.push_str(&format!(
        "# Migration Blueprint for `{}`\n\n",
        slice.entry_node_id
    ));
    md.push_str(&format!(
        "**Type:** {} | **File:** `{}`\n\n",
        slice.entry_node_type, slice.entry_file
    ));

    if slice.dead_code_skipped > 0 {
        md.push_str(&format!(
            "> Filtered out {} dead-code branches during traversal.\n\n",
            slice.dead_code_skipped
        ));
    }

    md.push_str(&format!("**Nodes visited:** {}\n\n", slice.nodes_visited));

    // Section 1: Frontend
    if !slice.frontend_deps.is_empty() {
        md.push_str("## 1. Frontend & Client-Side Dependencies\n");
        md.push_str("JS files, DOM manipulations, AJAX calls, and postback triggers:\n\n");
        for item in &slice.frontend_deps {
            md.push_str(&format!(
                "- `{}` ({}) via `{}` in `{}`\n",
                item.node_id, item.node_type, item.edge_kind, item.file_path
            ));
        }
        md.push('\n');
    }

    // Section 2: Backend
    if !slice.backend_methods.is_empty() {
        md.push_str("## 2. Backend Methods & Classes\n");
        md.push_str("Code-behind, utility classes, and dependencies:\n\n");
        for item in &slice.backend_methods {
            md.push_str(&format!(
                "- `{}` ({}) via `{}` in `{}`\n",
                item.node_id, item.node_type, item.edge_kind, item.file_path
            ));
        }
        md.push('\n');
    }

    // Section 3: State
    if !slice.state_mutations.is_empty() {
        md.push_str("## 3. State Mutations (CRITICAL for migration)\n");
        md.push_str("Session, ViewState, cookies, and application state. Convert to local state, Context, or external stores:\n\n");
        for item in &slice.state_mutations {
            md.push_str(&format!(
                "- `{}` ({}) via `{}`\n",
                item.node_id, item.node_type, item.edge_kind
            ));
        }
        md.push('\n');
    }

    // Section 4: Database
    if !slice.database_deps.is_empty() {
        md.push_str("## 4. Database Dependencies\n");
        md.push_str("Tables, columns, SQL calls, and foreign keys. Ensure ORM/API layer resolves these:\n\n");
        for item in &slice.database_deps {
            md.push_str(&format!(
                "- `{}` ({}) via `{}`\n",
                item.node_id, item.node_type, item.edge_kind
            ));
        }
        md.push('\n');
    }

    // Section 5: Data bindings
    if !slice.data_bindings.is_empty() {
        md.push_str("## 5. Data-Binding Fields\n");
        md.push_str("Eval/Bind expressions referencing model fields:\n\n");
        for field in &slice.data_bindings {
            md.push_str(&format!("- `{field}`\n"));
        }
        md.push('\n');
    }

    // Section 6: Component deps
    if !slice.component_deps.is_empty() {
        md.push_str("## 6. Component Dependencies\n");
        md.push_str("User controls, includes, and service endpoints:\n\n");
        for item in &slice.component_deps {
            md.push_str(&format!(
                "- `{}` ({}) via `{}` in `{}`\n",
                item.node_id, item.node_type, item.edge_kind, item.file_path
            ));
        }
        md.push('\n');
    }

    // Section 7: Config
    if !slice.config_deps.is_empty() {
        md.push_str("## 7. Configuration Registrations\n");
        md.push_str("HTTP modules, handlers, and config entries:\n\n");
        for item in &slice.config_deps {
            md.push_str(&format!(
                "- `{}` ({}) via `{}`\n",
                item.node_id, item.node_type, item.edge_kind
            ));
        }
        md.push('\n');
    }

    // Section 8: Lifecycle
    if !slice.lifecycle_info.is_empty() {
        md.push_str("## 8. Page Lifecycle Sequence\n");
        md.push_str("Order matters for migration — initialize in the same sequence:\n\n");
        for (node_id, stage, seq) in &slice.lifecycle_info {
            md.push_str(&format!("- [{seq}] `{stage}`: `{node_id}`\n"));
        }
        md.push('\n');
    }

    // Section 9: Side effects
    if !slice.side_effects.is_empty() {
        md.push_str("## 9. Side-Effect Classification\n");
        md.push_str("Methods with external effects (DB, UI mutation, state access):\n\n");
        for (node_id, fx) in &slice.side_effects {
            md.push_str(&format!("- `{node_id}`: {fx}\n"));
        }
        md.push('\n');
    }

    md
}
