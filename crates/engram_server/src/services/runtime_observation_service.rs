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
    by_method_in_file: HashMap<(String, String), String>,
    by_method: HashMap<String, String>,
    by_control: HashMap<String, String>,
}

fn normalize(v: &str) -> String {
    v.trim().to_ascii_lowercase()
}

fn is_key_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'
}

fn next_kv_boundary(line: &str, start: usize) -> usize {
    let mut i = start;
    while i < line.len() {
        let rest = &line[i..];
        let Some(ch) = rest.chars().next() else { break };
        if matches!(ch, '|' | ',' | ';' | '\t') {
            let mut j = i + ch.len_utf8();
            while j < line.len() {
                let Some(ws) = line[j..].chars().next() else {
                    break;
                };
                if ws.is_whitespace() {
                    j += ws.len_utf8();
                } else {
                    break;
                }
            }

            let key_start = j;
            while j < line.len() {
                let Some(kc) = line[j..].chars().next() else {
                    break;
                };
                if is_key_char(kc) {
                    j += kc.len_utf8();
                } else {
                    break;
                }
            }

            if j > key_start {
                while j < line.len() {
                    let Some(ws) = line[j..].chars().next() else {
                        break;
                    };
                    if ws.is_whitespace() {
                        j += ws.len_utf8();
                    } else {
                        break;
                    }
                }
                if line[j..].starts_with('=') {
                    return i;
                }
            }
        }
        i += ch.len_utf8();
    }
    line.len()
}

fn add_kv_hints(line: &str, row: &mut RuntimeEvidenceRow) {
    let mut i = 0;
    while i < line.len() {
        while i < line.len() {
            let Some(ch) = line[i..].chars().next() else {
                break;
            };
            if matches!(ch, '|' | ',' | ';' | '\t') || ch.is_whitespace() {
                i += ch.len_utf8();
            } else {
                break;
            }
        }

        if i >= line.len() {
            break;
        }

        let key_start = i;
        while i < line.len() {
            let Some(ch) = line[i..].chars().next() else {
                break;
            };
            if is_key_char(ch) {
                i += ch.len_utf8();
            } else {
                break;
            }
        }

        if i == key_start {
            let Some(ch) = line[i..].chars().next() else {
                break;
            };
            i += ch.len_utf8();
            continue;
        }

        while i < line.len() {
            let Some(ch) = line[i..].chars().next() else {
                break;
            };
            if ch.is_whitespace() {
                i += ch.len_utf8();
            } else {
                break;
            }
        }

        if !line[i..].starts_with('=') {
            continue;
        }
        i += 1;

        let key = normalize(&line[key_start..i - 1]);
        let end = next_kv_boundary(line, i);
        let val = line[i..end].trim().trim_matches('"').to_string();
        i = if end < line.len() { end + 1 } else { end };

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
    let mut by_method_candidates: HashMap<String, HashSet<String>> = HashMap::new();
    for n in nodes {
        let file_key = normalize(n.file_path.as_str());
        lu.by_file
            .entry(file_key.clone())
            .or_insert_with(|| n.node_id.clone());

        if n.node_type.eq_ignore_ascii_case("class") {
            lu.by_class
                .entry(normalize(&n.name))
                .or_insert_with(|| n.node_id.clone());
        }
        if n.node_type.eq_ignore_ascii_case("function") {
            let method_key = normalize(&n.name);
            lu.by_method_in_file
                .entry((file_key.clone(), method_key.clone()))
                .or_insert_with(|| n.node_id.clone());
            by_method_candidates
                .entry(method_key)
                .or_default()
                .insert(n.node_id.clone());
        }
        if n.node_type.eq_ignore_ascii_case("control") {
            lu.by_control
                .entry(normalize(&n.name))
                .or_insert_with(|| n.node_id.clone());
        }
    }

    for (method_key, ids) in by_method_candidates {
        if ids.len() == 1 {
            if let Some(node_id) = ids.into_iter().next() {
                lu.by_method.insert(method_key, node_id);
            }
        }
    }

    lu
}

fn find_best_source(row: &RuntimeEvidenceRow, lu: &NodeLookup) -> Option<String> {
    row.file_hint
        .as_ref()
        .and_then(|file| {
            row.method_hint.as_ref().and_then(|method| {
                lu.by_method_in_file
                    .get(&(normalize(file), normalize(method)))
                    .cloned()
            })
        })
        .or_else(|| {
            row.method_hint
                .as_ref()
                .and_then(|v| lu.by_method.get(&normalize(v)).cloned())
        })
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

    #[test]
    fn preserves_sql_with_commas_semicolons_and_equals() {
        let rows = parse_rows(&RuntimeArtifactInput {
            kind: RuntimeArtifactKind::SqlProfilerExport,
            content:
                "file=Report.aspx,sql=SELECT id, name FROM users WHERE role='Admin'; method=Run"
                    .into(),
            label: None,
        });
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].file_hint.as_deref(), Some("Report.aspx"));
        assert_eq!(rows[0].method_hint.as_deref(), Some("Run"));
        assert_eq!(
            rows[0].sql_text.as_deref(),
            Some("SELECT id, name FROM users WHERE role='Admin'")
        );
    }

    #[test]
    fn resolves_method_hint_by_file_before_global_name() {
        let users = Node {
            node_id: "fn:users:save".into(),
            node_type: "function".into(),
            name: "Save".into(),
            namespace: "code".into(),
            language: "vb".into(),
            file_path: "Users.aspx.vb".into(),
            start_line: 1,
            end_line: 10,
            generation: 1,
            metadata: None,
        };
        let orders = Node {
            node_id: "fn:orders:save".into(),
            node_type: "function".into(),
            name: "Save".into(),
            namespace: "code".into(),
            language: "vb".into(),
            file_path: "Orders.aspx.vb".into(),
            start_line: 1,
            end_line: 10,
            generation: 1,
            metadata: None,
        };

        let lu = build_lookup(&[users, orders]);
        let row = RuntimeEvidenceRow {
            file_hint: Some("Orders.aspx.vb".into()),
            class_hint: None,
            method_hint: Some("Save".into()),
            control_hint: None,
            sql_text: Some("DELETE FROM Orders".into()),
        };

        assert_eq!(
            find_best_source(&row, &lu).as_deref(),
            Some("fn:orders:save")
        );
        assert!(!lu.by_method.contains_key("save"));
    }
}
