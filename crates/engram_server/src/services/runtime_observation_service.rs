use engram_graph::{Edge, EdgeKind, GraphStore, Node};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeArtifactKind {
    IisLog,
    CustomTrace,
    PageLifecycleSnapshot,
    SqlProfilerExport,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RuntimeArtifactInput {
    pub kind: RuntimeArtifactKind,
    pub content: String,
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeIngestionSummary {
    pub artifacts_processed: usize,
    pub evidence_rows_parsed: usize,
    pub observed_runtime_control_edges: usize,
    pub observed_runtime_sql_edges: usize,
    pub merged_static_edges: usize,
}

#[derive(Debug, Clone)]
struct RuntimeEvidenceRow {
    file_hint: Option<String>,
    class_hint: Option<String>,
    method_hint: Option<String>,
    control_hint: Option<String>,
    sql_text: Option<String>,
}

#[derive(Default)]
struct NodeLookup {
    by_file: HashMap<String, String>,
    by_class: HashMap<String, String>,
    by_method: HashMap<String, String>,
    by_control: HashMap<String, String>,
}

fn normalize(v: &str) -> String {
    v.trim().to_ascii_lowercase()
}

fn add_kv_hints(line: &str, row: &mut RuntimeEvidenceRow) {
    for token in line.split(['|', ',', ';', '\t']) {
        let mut it = token.splitn(2, '=');
        let Some(k) = it.next() else { continue };
        let Some(v) = it.next() else { continue };
        let key = normalize(k);
        let val = v.trim().trim_matches('"').to_string();
        if val.is_empty() {
            continue;
        }
        match key.as_str() {
            "file" | "path" | "cs-uri-stem" | "page" => row.file_hint = Some(val),
            "class" | "type" => row.class_hint = Some(val),
            "method" | "handler" | "event" => row.method_hint = Some(val),
            "control" | "controlid" | "control_id" => row.control_hint = Some(val),
            "sql" | "sqltext" | "statement" | "query" => row.sql_text = Some(val),
            _ => {}
        }
    }
}

fn parse_rows(artifact: &RuntimeArtifactInput) -> Vec<RuntimeEvidenceRow> {
    let mut out = Vec::new();
    for line in artifact.content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut row = RuntimeEvidenceRow {
            file_hint: artifact.label.clone(),
            class_hint: None,
            method_hint: None,
            control_hint: None,
            sql_text: None,
        };
        add_kv_hints(trimmed, &mut row);

        if matches!(artifact.kind, RuntimeArtifactKind::SqlProfilerExport)
            && row.sql_text.is_none()
            && (trimmed.contains("select ") || trimmed.contains("SELECT "))
        {
            row.sql_text = Some(trimmed.to_string());
        }

        out.push(row);
    }
    out
}

fn build_lookup(nodes: &[Node]) -> NodeLookup {
    let mut lu = NodeLookup::default();
    for n in nodes {
        lu.by_file
            .entry(normalize(n.file_path.as_str()))
            .or_insert_with(|| n.node_id.clone());

        if n.node_type.eq_ignore_ascii_case("class") {
            lu.by_class
                .entry(normalize(&n.name))
                .or_insert_with(|| n.node_id.clone());
        }
        if n.node_type.eq_ignore_ascii_case("function") {
            lu.by_method
                .entry(normalize(&n.name))
                .or_insert_with(|| n.node_id.clone());
        }
        if n.node_type.eq_ignore_ascii_case("control") {
            lu.by_control
                .entry(normalize(&n.name))
                .or_insert_with(|| n.node_id.clone());
        }
    }
    lu
}

fn find_best_source(row: &RuntimeEvidenceRow, lu: &NodeLookup) -> Option<String> {
    row.method_hint
        .as_ref()
        .and_then(|v| lu.by_method.get(&normalize(v)).cloned())
        .or_else(|| {
            row.class_hint
                .as_ref()
                .and_then(|v| lu.by_class.get(&normalize(v)).cloned())
        })
        .or_else(|| {
            row.control_hint
                .as_ref()
                .and_then(|v| lu.by_control.get(&normalize(v)).cloned())
        })
        .or_else(|| {
            row.file_hint
                .as_ref()
                .and_then(|v| lu.by_file.get(&normalize(v)).cloned())
        })
}

fn sql_target(sql: &str) -> String {
    let hash = blake3::hash(sql.as_bytes()).to_hex().to_string();
    format!("sql:observed:{hash}")
}

pub fn ingest_runtime_artifacts(
    graph: &GraphStore,
    project_id: &str,
    generation: u64,
    artifacts: &[RuntimeArtifactInput],
) -> anyhow::Result<RuntimeIngestionSummary> {
    let mut rows = Vec::new();
    for a in artifacts {
        rows.extend(parse_rows(a));
    }

    let nodes = graph.query_nodes(project_id, None, None, None, 100_000)?;
    let lookup = build_lookup(&nodes);

    let mut runtime_edges: HashMap<(EdgeKind, String, String), u32> = HashMap::new();

    for row in &rows {
        let Some(source_id) = find_best_source(row, &lookup) else {
            continue;
        };

        if let Some(control) = &row.control_hint {
            let target = lookup
                .by_control
                .get(&normalize(control))
                .cloned()
                .unwrap_or_else(|| format!("control:runtime:{}", control));
            *runtime_edges
                .entry((EdgeKind::ObservedRuntimeControl, source_id.clone(), target))
                .or_insert(0) += 1;
        }

        if let Some(sql) = &row.sql_text {
            *runtime_edges
                .entry((EdgeKind::ObservedRuntimeSql, source_id, sql_target(sql)))
                .or_insert(0) += 1;
        }
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let mut upserts: Vec<Edge> = Vec::new();
    for ((kind, source, target), weight) in &runtime_edges {
        upserts.push(Edge {
            source_id: source.clone(),
            target_id: target.clone(),
            namespace: engram_core::namespaces::NAMESPACE_HISTORY.to_string(),
            language: "runtime".to_string(),
            edge_kind: kind.clone(),
            weight: *weight,
            generation,
            metadata: Some(json!({"source":"runtime","provenance":["runtime"]})),
            updated_at_ms: now,
        });
    }

    let mut merged = 0usize;
    let mut touched: HashSet<(EdgeKind, String, String)> = HashSet::new();
    for (_, source, target) in runtime_edges.keys() {
        touched.insert((EdgeKind::SqlCalls, source.clone(), target.clone()));
        touched.insert((EdgeKind::QueriesTable, source.clone(), target.clone()));
        touched.insert((EdgeKind::RegistersControl, source.clone(), target.clone()));
        touched.insert((EdgeKind::TriggersPostback, source.clone(), target.clone()));
    }

    let existing_edges = graph.list_edges(project_id, None)?;
    let existing_map: HashMap<(EdgeKind, String, String), Edge> = existing_edges
        .into_iter()
        .map(|e| {
            (
                (
                    e.edge_kind.clone(),
                    e.source_id.clone(),
                    e.target_id.clone(),
                ),
                e,
            )
        })
        .collect();

    for (kind, source, target) in touched {
        if let Some(mut edge) = existing_map
            .get(&(kind.clone(), source.clone(), target.clone()))
            .cloned()
        {
            merged += 1;
            edge.metadata =
                Some(json!({"source":"static+runtime","provenance":["static","runtime"]}));
            edge.generation = generation;
            edge.updated_at_ms = now;
            upserts.push(edge);
        }
    }

    graph.upsert_edges(project_id, &upserts)?;

    Ok(RuntimeIngestionSummary {
        artifacts_processed: artifacts.len(),
        evidence_rows_parsed: rows.len(),
        observed_runtime_control_edges: runtime_edges
            .keys()
            .filter(|(k, _, _)| matches!(k, EdgeKind::ObservedRuntimeControl))
            .count(),
        observed_runtime_sql_edges: runtime_edges
            .keys()
            .filter(|(k, _, _)| matches!(k, EdgeKind::ObservedRuntimeSql))
            .count(),
        merged_static_edges: merged,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_key_value_artifact_lines() {
        let rows = parse_rows(&RuntimeArtifactInput {
            kind: RuntimeArtifactKind::CustomTrace,
            content: "file=Default.aspx.cs|method=Save|control=btnSave|sql=SELECT 1".into(),
            label: None,
        });
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].method_hint.as_deref(), Some("Save"));
        assert_eq!(rows[0].control_hint.as_deref(), Some("btnSave"));
        assert!(rows[0].sql_text.is_some());
    }
}
