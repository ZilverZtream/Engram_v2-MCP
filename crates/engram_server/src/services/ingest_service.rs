use crate::state::AppState;
use crate::utils::now_ms;

/// Process ingest stats: create graph nodes and edges from parsed symbols.
/// This is a large function that builds the graph from the AST extraction results.
pub async fn process_ingest_stats(
    state: &AppState,
    project_id: &str,
    generation: u64,
    stats: &engram_index::IngestStats,
) -> anyhow::Result<()> {
    let mut nodes = Vec::with_capacity(stats.symbols.len() + stats.all_files.len());

    let fp_map: std::collections::HashMap<_, _> = stats
        .fingerprints
        .iter()
        .map(|fp| (&fp.rel_path, fp))
        .collect();

    for rel_path in &stats.all_files {
        if std::path::Path::new(rel_path.as_str()).is_absolute() {
            anyhow::bail!(
                "process_ingest_stats: absolute path in all_files: {}",
                rel_path.as_str()
            );
        }
        let language = engram_core::guess_language(std::path::Path::new(rel_path.as_str()));

        let mut metadata = None;

        if let Some(fp) = fp_map.get(&rel_path.as_str().to_string()) {
            metadata = Some(serde_json::json!({
                "mtime": fp.mtime_ms / 1000,
                "size": fp.size,
                "file_hash": fp.file_hash,
            }));
        }

        nodes.push(engram_graph::Node {
            node_id: engram_core::ids::NodeId::file(rel_path.as_str()).0,
            node_type: "file".into(),
            name: rel_path
                .file_name()
                .unwrap_or_else(|| rel_path.as_str())
                .to_string(),
            namespace: engram_core::namespaces::NAMESPACE_MEMORY.into(),
            language: language.into(),
            file_path: rel_path.clone(),
            start_line: 0,
            end_line: 0,
            generation,
            metadata,
        });
    }

    for (rel_path, sym) in &stats.symbols {
        let language = engram_core::guess_language(std::path::Path::new(rel_path.as_str()));

        let (metadata, fqn) = if let Some(m) = &sym.metadata {
            let fqn_val = m.get("fqn").map(|v| v.as_str().to_string());
            let map: std::collections::HashMap<String, serde_json::Value> = m
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                .collect();
            (
                Some(serde_json::to_value(map).unwrap_or(serde_json::Value::Null)),
                fqn_val,
            )
        } else {
            (None, None)
        };
        let fqn = fqn.as_deref();

        let (node_id, final_kind) = if sym.kind == "page" {
            (
                engram_core::ids::NodeId::page(rel_path.as_str()).0,
                sym.kind.clone(),
            )
        } else if sym.kind == "control" {
            let control_id = sym
                .metadata
                .as_ref()
                .and_then(|m| m.get("control_id"))
                .map(|s| s.as_str())
                .unwrap_or(sym.name.as_str());
            (
                engram_core::ids::NodeId::control(rel_path.as_str(), control_id).0,
                "control".to_string(),
            )
        } else if sym.kind == "control_ref" {
            let path_str = rel_path.as_str();
            let page_path = if let Some(idx) = path_str.find(".designer.") {
                &path_str[..idx]
            } else if let Some(idx) = path_str.find(".aspx.") {
                &path_str[..idx + 5]
            } else if let Some(idx) = path_str.find(".ascx.") {
                &path_str[..idx + 5]
            } else {
                path_str
            };
            (
                engram_core::ids::NodeId::control(page_path, &sym.name).0,
                "control".to_string(),
            )
        } else if sym.kind == "db_table" {
            (
                engram_core::ids::NodeId::table(&sym.name).0,
                "db_table".to_string(),
            )
        } else if sym.kind == "db_column" {
            let table = sym
                .metadata
                .as_ref()
                .and_then(|m| m.get("table"))
                .map(|s| s.as_str())
                .unwrap_or("unknown");
            (
                engram_core::ids::NodeId::column(table, &sym.name).0,
                "db_column".to_string(),
            )
        } else if sym.kind == "global_state" {
            let state_type = sym
                .metadata
                .as_ref()
                .and_then(|m| m.get("state_type"))
                .map(|s| s.as_str())
                .unwrap_or("Session");
            let state_key = sym
                .metadata
                .as_ref()
                .and_then(|m| m.get("state_key"))
                .map(|s| s.as_str())
                .unwrap_or(&sym.name);
            (
                engram_core::ids::NodeId::state(state_type, state_key).0,
                "global_state".to_string(),
            )
        } else {
            (
                engram_core::ids::NodeId::symbol(
                    &sym.kind,
                    fqn,
                    rel_path.as_str(),
                    &sym.name,
                    sym.start_line,
                )
                .0,
                sym.kind.clone(),
            )
        };

        nodes.push(engram_graph::Node {
            node_id,
            node_type: final_kind,
            name: sym.name.clone(),
            namespace: engram_core::namespaces::NAMESPACE_MEMORY.into(),
            language: language.into(),
            file_path: rel_path.clone(),
            start_line: sym.start_line,
            end_line: sym.end_line,
            generation,
            metadata,
        });
    }

    let mut edges = Vec::with_capacity(stats.edges.len());
    for (rel_path, edge) in &stats.edges {
        let language = engram_core::guess_language(std::path::Path::new(&format!(
            "dummy.{}",
            edge.source_language
        )));

        let source_id = if edge.source_name == "file" || edge.source_kind == "file" {
            let path = if edge.source_name == "file" {
                rel_path.as_str()
            } else {
                &edge.source_name
            };
            if std::path::Path::new(path).is_absolute() {
                anyhow::bail!(
                    "process_ingest_stats: absolute path in edge source: {} (file: {})",
                    path,
                    rel_path.as_str()
                );
            }
            if edge.source_kind == "page" {
                engram_core::ids::NodeId::page(path).0
            } else {
                engram_core::ids::NodeId::file(path).0
            }
        } else if edge.source_kind == "page" {
            engram_core::ids::NodeId::page(rel_path.as_str()).0
        } else if edge.source_kind == "control" {
            let control_id = edge
                .metadata
                .as_ref()
                .and_then(|m| m.get("control_id"))
                .map(|s| s.as_str())
                .unwrap_or(edge.source_name.as_str());

            let path_str = rel_path.as_str();
            let page_path = if path_str.ends_with(".cs") || path_str.ends_with(".vb") {
                if let Some(idx) = path_str.find(".aspx.") {
                    &path_str[..idx + 5]
                } else if let Some(idx) = path_str.find(".ascx.") {
                    &path_str[..idx + 5]
                } else {
                    path_str
                }
            } else {
                path_str
            };
            engram_core::ids::NodeId::control(page_path, control_id).0
        } else {
            let fqn = if edge.source_name.contains('.') {
                Some(edge.source_name.as_str())
            } else {
                edge.metadata
                    .as_ref()
                    .and_then(|m| m.get("source_fqn"))
                    .map(|s| s.as_str())
            };
            engram_core::ids::NodeId::symbol(
                &edge.source_kind,
                fqn,
                rel_path.as_str(),
                &edge.source_name,
                edge.source_start_line,
            )
            .0
        };

        let target_id = if edge.target_name == "file" || edge.target_kind.as_deref() == Some("file")
        {
            let path = if edge.target_name == "file" {
                rel_path.as_str()
            } else {
                &edge.target_name
            };
            if std::path::Path::new(path).is_absolute() {
                anyhow::bail!(
                    "process_ingest_stats: absolute path in edge target: {} (file: {})",
                    path,
                    rel_path.as_str()
                );
            }
            if edge.target_kind.as_deref() == Some("page") {
                engram_core::ids::NodeId::page(path).0
            } else {
                engram_core::ids::NodeId::file(path).0
            }
        } else if edge.target_kind.as_deref() == Some("page") {
            engram_core::ids::NodeId::page(rel_path.as_str()).0
        } else if edge.target_kind.as_deref() == Some("control") {
            let control_id = edge
                .metadata
                .as_ref()
                .and_then(|m| m.get("control_id"))
                .map(|s| s.as_str())
                .unwrap_or(edge.target_name.as_str());
            engram_core::ids::NodeId::control(rel_path.as_str(), control_id).0
        } else if edge.target_kind.as_deref() == Some("control_ref") {
            let path_str = rel_path.as_str();
            let page_path = if let Some(idx) = path_str.find(".designer.") {
                &path_str[..idx]
            } else {
                path_str
            };
            let simple_name = edge
                .target_name
                .split('.')
                .next_back()
                .unwrap_or(&edge.target_name);
            engram_core::ids::NodeId::control(page_path, simple_name).0
        } else if edge.target_name.starts_with("sql:") {
            edge.target_name.clone()
        } else if edge.target_name.starts_with("state:") {
            edge.target_name.clone()
        } else if edge.target_name.starts_with("column:") {
            edge.target_name.clone()
        } else if edge.target_kind.as_deref() == Some("db_table") {
            engram_core::ids::NodeId::table(&edge.target_name).0
        } else if edge.target_kind.as_deref() == Some("db_column") {
            let table = edge
                .metadata
                .as_ref()
                .and_then(|m| m.get("local_table").or_else(|| m.get("table")))
                .map(|s| s.as_str())
                .unwrap_or("unknown");
            engram_core::ids::NodeId::column(table, &edge.target_name).0
        } else if let Some(kind) = &edge.target_kind {
            let fqn = if edge.target_name.contains('.') {
                Some(edge.target_name.as_str())
            } else {
                edge.metadata
                    .as_ref()
                    .and_then(|m| m.get("fqn"))
                    .map(|s| s.as_str())
            };
            engram_core::ids::NodeId::symbol(
                kind,
                fqn,
                rel_path.as_str(),
                &edge.target_name,
                edge.target_start_line.unwrap_or(0),
            )
            .0
        } else {
            format!("::{}", edge.target_name)
        };

        // Virtual nodes for SQL targets
        if target_id.starts_with("sql:") {
            let sql_name = target_id.split(':').next_back().unwrap_or(&target_id);
            let sql_kind = if target_id.contains(":stored_proc:") {
                "stored_proc"
            } else {
                "inline_sql"
            };
            nodes.push(engram_graph::Node {
                node_id: target_id.clone(),
                node_type: sql_kind.into(),
                name: sql_name.to_string(),
                namespace: engram_core::namespaces::NAMESPACE_MEMORY.into(),
                language: "sql".into(),
                file_path: rel_path.clone(),
                start_line: edge.target_start_line.unwrap_or(0),
                end_line: edge.target_start_line.unwrap_or(0),
                generation,
                metadata: edge.metadata.as_ref().map(|m| {
                    let mut map = std::collections::HashMap::new();
                    for (k, v) in m {
                        map.insert(k.clone(), serde_json::Value::String(v.clone()));
                    }
                    serde_json::to_value(map).unwrap_or(serde_json::Value::Null)
                }),
            });
        }

        // Virtual nodes for global state targets
        if target_id.starts_with("state:") {
            let parts: Vec<&str> = target_id.splitn(3, ':').collect();
            if parts.len() == 3 {
                let state_type = parts[1];
                let state_key = parts[2];
                nodes.push(engram_graph::Node {
                    node_id: target_id.clone(),
                    node_type: "global_state".into(),
                    name: format!("{}:{}", state_type, state_key),
                    namespace: engram_core::namespaces::NAMESPACE_MEMORY.into(),
                    language: "text".into(),
                    file_path: rel_path.clone(),
                    start_line: 0,
                    end_line: 0,
                    generation,
                    metadata: Some(serde_json::json!({
                        "state_type": state_type,
                        "state_key": state_key,
                    })),
                });
            }
        }

        let edge_kind = match edge.kind.as_str() {
            "contains" | "cb_defines" | "inherits" | "codebehind_file" | "codebehind_class" => {
                engram_graph::EdgeKind::Contains
            }
            "imports" => engram_graph::EdgeKind::Imports,
            "sql_calls" => engram_graph::EdgeKind::SqlCalls,
            "has_column" => engram_graph::EdgeKind::HasColumn,
            "foreign_key" => engram_graph::EdgeKind::ForeignKey,
            "queries_table" => engram_graph::EdgeKind::QueriesTable,
            "reads_state" => engram_graph::EdgeKind::ReadsState,
            "writes_state" => engram_graph::EdgeKind::WritesState,
            _ => engram_graph::EdgeKind::Dependency,
        };

        edges.push(engram_graph::Edge {
            source_id,
            target_id,
            namespace: engram_core::namespaces::NAMESPACE_MEMORY.into(),
            language: language.into(),
            edge_kind,
            weight: 1,
            generation,
            metadata: edge.metadata.as_ref().map(|m| {
                let mut map = std::collections::HashMap::new();
                for (k, v) in m {
                    map.insert(k.clone(), serde_json::Value::String(v.clone()));
                }
                serde_json::to_value(map).unwrap_or(serde_json::Value::Null)
            }),
            updated_at_ms: now_ms(),
        });
    }

    if !nodes.is_empty() {
        let graph = state.graph.clone();
        let pid = project_id.to_string();
        match tokio::task::spawn_blocking(move || graph.upsert_nodes(&pid, &nodes)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::error!("graph upsert_nodes failed for {project_id}: {e}");
                return Err(e);
            }
            Err(e) => {
                tracing::error!("graph upsert_nodes task panicked for {project_id}: {e}");
                anyhow::bail!("graph upsert_nodes task panicked: {e}");
            }
        }
    }

    if !edges.is_empty() {
        let graph = state.graph.clone();
        let pid = project_id.to_string();
        match tokio::task::spawn_blocking(move || graph.upsert_edges(&pid, &edges)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::error!("graph upsert_edges failed for {project_id}: {e}");
                return Err(e);
            }
            Err(e) => {
                tracing::error!("graph upsert_edges task panicked for {project_id}: {e}");
                anyhow::bail!("graph upsert_edges task panicked: {e}");
            }
        }
    }

    Ok(())
}
