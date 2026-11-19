//! Complete source-declared table/procedure evidence for the SQL question path.
use std::{sync::Arc, time::Duration};

use engram_index::SourceSnapshotGuard;
use serde_json::{Value, json};

use crate::{
    models::{QuerySchemaRequest, SchemaQueryType},
    services::{
        ask_engine::{
            evidence::{Authority, EvidenceItem, EvidenceKind},
            plan::{ContractEntityType, EntityKind, Modality, QueryPlan},
            status::{AnswerStatus, ProviderReport, ProviderStatus},
        },
        schema_query,
    },
    tools::Engram,
};

pub(super) struct SchemaEvidence {
    pub items: Vec<EvidenceItem>,
    pub provider: ProviderReport,
    pub matched_names: Vec<String>,
    ambiguous: bool,
}

impl SchemaEvidence {
    pub fn qualify_status(&self, status: AnswerStatus) -> AnswerStatus {
        if !matches!(status, AnswerStatus::Answered | AnswerStatus::Partial) {
            return status;
        }
        if self.ambiguous {
            AnswerStatus::Ambiguous
        } else if self.provider.status != ProviderStatus::Hit || self.provider.truncated {
            AnswerStatus::Partial
        } else {
            status
        }
    }
}

/// Related declarations can come from different selected SQL files. Recover
/// their policy scope from the typed citations that actually survived ranking.
pub(super) fn related_source_paths(evidence: &[EvidenceItem]) -> std::collections::BTreeSet<String> {
    fn collect(value: &Value, paths: &mut std::collections::BTreeSet<String>) {
        match value {
            Value::Object(object) => {
                if let Some(path) = object.get("citation").and_then(|c| c.get("path")).and_then(Value::as_str) {
                    paths.insert(path.to_owned());
                }
                for value in object.values() { collect(value, paths); }
            }
            Value::Array(rows) => for row in rows { collect(row, paths); },
            _ => (),
        }
    }
    let mut paths = std::collections::BTreeSet::new();
    for item in evidence.iter().filter(|item| item.provider == "schema"
        && item.authority == Authority::CurrentCode && item.extraction_method == "source_sql_schema") {
        if let Ok(body) = serde_json::from_str::<Value>(&item.content) {
            collect(&body, &mut paths);
        }
    }
    paths
}

fn table_facts(row: &Value, report: &Value, project: &str, schema_available: bool) -> Value {
    let mut facts = row.clone();
    if !schema_available {
        // The source reader's specialized object handoffs need Full. Core
        // already has a hash-bound file reader; use that without hiding the
        // fact that the object body is not inline or a deployed definition.
        for key in ["triggers", "views", "stored_procedures"] {
            for related in facts[key].as_array_mut().into_iter().flatten() {
                let object = &mut related["object"];
                if object["declaration_included"] != false { continue; }
                let citation = object["citation"].clone();
                object["recovery"] = json!({"tool":"get_full_method_body","arguments":{
                    "project_id":project,"file_path":citation["path"],"line_start":1,
                    "context_lines":0,"output_json":true,"expected_file_blake3":citation["source_hash"]}});
                object["recovery_scope"] = json!("Current complete source file in hash-bound pages; follow file_excerpt.next_page to EOF. The object's citation retains its declaration span. This is source recovery, not a live database definition.");
            }
        }
    }
    facts["source_coverage"] = report["coverage"].clone();
    facts["table_gaps"] = report["table_gaps"].clone();
    facts["unknown_nullability_columns"] = report["unknown_nullability_columns"].clone();
    facts["next_page"] = report["next_page"].clone();
    facts
}

pub(super) async fn gather(
    engram: &Engram,
    project_id: &str,
    question: &str,
    plan: &QueryPlan,
    generation: u64,
    view: Option<Arc<SourceSnapshotGuard>>,
    deadline: Duration,
) -> Option<SchemaEvidence> {
    // SQL declarations are read from the selected source catalog. They need
    // not have graph symbols, unlike methods supplied by language extractors.
    let selected_sql = view.as_ref().and_then(|view| view.selected_file_revisions())
        .is_some_and(|files| files.keys().any(|path| path.to_ascii_lowercase().ends_with(".sql")));
    let table_question = plan.contract.entity_type == ContractEntityType::Table
        || plan.entities.iter().any(|entity| entity.guessed_kind == EntityKind::Table
            || entity.resolved.iter().any(|resolved| resolved.kind == EntityKind::Table));
    let selected_dbml_table = table_question && view.as_ref().and_then(|view| view.selected_file_revisions())
        .is_some_and(|files| files.keys().any(|path| path.to_ascii_lowercase().ends_with(".dbml")));
    if !selected_sql && !selected_dbml_table && !plan.modalities.iter().any(|m| matches!(m, Modality::Sql)) {
        return None;
    }
    // Explicit SQL-view wording selects an existing source-object reader.
    // "view" alone can describe UI, so it does not reroute ordinary questions.
    let words = question.split(|c: char| !c.is_alphanumeric())
        .map(str::to_ascii_lowercase).collect::<Vec<_>>();
    let view_question = words.iter().any(|word| word == "sql")
        && words.iter().any(|word| word == "view" || word == "views");
    let function_question = words.iter().any(|word| word == "sql")
        && words.iter().any(|word| word == "function" || word == "functions");
    let mut requests = Vec::new();
    for entity in &plan.entities {
        // Classification selects an existing reader, never proves binding.
        // That reader independently admits selected/current SQL source hashes.
        let is_function = function_question && entity.guessed_kind != EntityKind::File;
        let is_view = !is_function && view_question && entity.guessed_kind != EntityKind::File;
        if (is_view || is_function) && matches!(entity.text.to_ascii_lowercase().as_str(), "sql" | "view" | "views" | "function" | "functions") {
            continue;
        }
        let discover_procedure = !is_function && !is_view && entity.guessed_kind == EntityKind::Symbol && entity.resolved.is_empty();
        let is_procedure = !is_function && !is_view && (discover_procedure || entity.resolved.iter().filter_map(|resolved| resolved.node_id.as_deref())
            .any(|id| engram.state.graph.get_node(project_id, id).is_ok_and(|node| node.is_some_and(|node|
                matches!(node.node_type.as_str(), "stored_proc" | "stored_procedure") && node.file_path.as_str().to_ascii_lowercase().ends_with(".sql")))));
        let is_table = entity.resolved.iter().any(|r| r.kind == EntityKind::Table)
            || entity.guessed_kind == EntityKind::Table
            || (plan.contract.entity_type == ContractEntityType::Table
                && entity.guessed_kind != EntityKind::File);
        if (!is_table && !is_procedure && !is_view && !is_function) || requests.iter().any(|(r, _): &(QuerySchemaRequest, bool)| {
            matches!(&r.query_type, Some(SchemaQueryType::Function { name }) if is_function && name == &entity.text)
                || matches!(&r.query_type, Some(SchemaQueryType::View { name }) if is_view && name == &entity.text)
                || matches!(&r.query_type, Some(SchemaQueryType::Table { name }) if !is_procedure && !is_view && !is_function && name == &entity.text)
                || matches!(&r.query_type, Some(SchemaQueryType::StoredProc { sp_name }) if is_procedure && sp_name == &entity.text)
        }) {
            continue;
        }
        let request = QuerySchemaRequest {
            project_id: project_id.into(),
            // Preserve qualification from the question, never a basename graph identity.
            query_type: Some(if is_function { SchemaQueryType::Function { name: entity.text.clone() } }
                else if is_view { SchemaQueryType::View { name: entity.text.clone() } }
                else if is_procedure { SchemaQueryType::StoredProc { sp_name: entity.text.clone() } }
                else { SchemaQueryType::Table { name: entity.text.clone() } }),
            offset: Some(0),
            limit: Some(100),
            max_tokens: Some(65536),
            response_id: None,
            response_offset: 0,
        };
        if schema_query::validate(&request).is_ok() {
            requests.push((request, discover_procedure || is_view || is_function));
        }
    }
    if requests.is_empty() {
        return None;
    }
    let mut out = SchemaEvidence {
        items: vec![],
        matched_names: vec![],
        ambiguous: false,
        provider: ProviderReport {
            provider: "schema".into(),
            status: ProviderStatus::Empty,
            count: 0,
            note: None,
            examined: 0,
            available: Some(0),
            truncated: requests.len() > 3,
            proof: None,
        },
    };
    let mut notes = vec!["Source declarations only; no live database, default-schema or collation binding. At most3 named-table/procedure/view/function lookups; examined counts source-file examinations across lookups.".to_string()];
    let mut failed = false;
    let work = async {
        let view =
            view.ok_or_else(|| "Selected source unavailable for SQL schema evidence".to_string())?;
        for (request, discover_procedure) in requests.into_iter().take(3) {
            let is_procedure = matches!(&request.query_type, Some(SchemaQueryType::StoredProc { .. }));
            let is_view = matches!(&request.query_type, Some(SchemaQueryType::View { .. }));
            let is_function = matches!(&request.query_type, Some(SchemaQueryType::Function { .. }));
            let result = engram
                .query_schema_selected(
                    request.clone(),
                    generation,
                    view.clone(),
                    plan.qualifiers.path_prefixes.clone(),
                )
                .await
                .map_err(|e| e.to_string())?;
            let text = result
                .content
                .first()
                .and_then(|part| part.as_text())
                .ok_or_else(|| "Schema reader returned no text".to_string())?;
            // Never turn a clipped/deferred schema into an apparent table fact.
            super::super::search_tools::response_budget::bounded_message(
                text.text.clone(),
                Some(65536),
            )
            .map_err(|e| e.to_string())?;
            let report: Value = serde_json::from_str(&text.text).map_err(|e| e.to_string())?;
            let total = report["total_matches"].as_u64().unwrap_or(0) as usize;
            if discover_procedure && total == 0
                && report["coverage"]["gaps"].as_array().is_some_and(|gaps| gaps.is_empty()) {
                continue;
            }
            out.ambiguous |= total > 1;
            *out.provider.available.as_mut().unwrap() += total;
            out.provider.examined += report["coverage"]["selected_sql_files"]
                .as_u64()
                .unwrap_or(0) as usize + report["coverage"]["selected_dbml_files"].as_u64().unwrap_or(0) as usize;
            out.provider.truncated |= !report["next_offset"].is_null();
            if report["coverage"]["gaps"]
                .as_array()
                .is_some_and(|gaps| !gaps.is_empty())
            {
                failed = true;
                notes.push(format!(
                    "SQL sources withheld: {}",
                    report["coverage"]["gaps"]
                ));
            }
            if total == 0 {
                failed = true;
                notes.push(format!(
                    "No admitted source declaration for {}",
                    serde_json::to_value(&request.query_type).unwrap_or(Value::Null)
                ));
            } else {
                match &request.query_type {
                    Some(SchemaQueryType::StoredProc { sp_name }) => out.matched_names.push(sp_name.clone()),
                    Some(SchemaQueryType::Table { name }) => out.matched_names.push(name.clone()),
                    Some(SchemaQueryType::View { name }) => out.matched_names.push(name.clone()),
                    Some(SchemaQueryType::Function { name }) => out.matched_names.push(name.clone()),
                    _ => {},
                }
            }
            for row in report["results"].as_array().into_iter().flatten() {
                let ambiguous = report["status"] == "ambiguous";
                let table = if ambiguous || is_procedure || is_view || is_function { row } else { &row["table"] };
                let citation = &table["citation"];
                let path = citation["path"]
                    .as_str()
                    .ok_or_else(|| "Schema citation path missing".to_string())?;
                let start = citation["start_line"]
                    .as_u64()
                    .ok_or_else(|| "Schema citation span missing".to_string())?;
                let end = citation["end_line"]
                    .as_u64()
                    .ok_or_else(|| "Schema citation span missing".to_string())?;
                let hash = citation["source_hash"]
                    .as_str()
                    .ok_or_else(|| "Schema source hash missing".to_string())?;
                out.items.push(EvidenceItem {
                    memory_record: None,
                    evidence_id: format!("schema_{}", out.items.len() + 1),
                    document_id: None, document_namespace: None,
                    source_verification: Some(format!("SELECTED_SOURCE_FILE_HASH:{hash}")),
                    kind: EvidenceKind::SourceCode, authority: Authority::CurrentCode,
                    path: Some(path.into()), lines: Some((start as u32, end as u32)),
                    symbol_id: Some(format!("schema:{path}:{start}:{hash}")),
                    title: Some(match &request.query_type {
                        Some(SchemaQueryType::StoredProc { sp_name }) => format!("Source procedure {sp_name}: {}", table["name"]),
                        Some(SchemaQueryType::View { name }) => format!("Source view {name}: {}", table["name"]),
                        Some(SchemaQueryType::Function { name }) => format!("Source SQL function {name}: {}", table["name"]),
                        _ if table["source_kind"] == "dbml_mapping" => format!("Source DBML table mapping {}", table["name"]),
                        _ => format!("Source table {}", table["name"]),
                    }),
                    content: if ambiguous {
                        let recovery = if crate::tool_surface::includes(engram.state.cfg.effective_tool_preset(), "query_schema") {
                            json!({"tool":"query_schema","arguments":{"project_id":project_id,
                                "query_type":if is_function {json!({"kind":"function","name":row["name"]})}
                                    else if is_view {json!({"kind":"view","name":row["name"]})}
                                    else if is_procedure {json!({"kind":"stored_proc","sp_name":row["name"]})}
                                    else {json!({"kind":"table","name":row["name"]})}}})
                        } else {
                            json!({"tool":"ask_codebase","arguments":{"project_id":project_id,
                                "question":if is_function {format!("Describe SQL function \"{}\"",row["name"].as_str().unwrap_or_default())}
                                    else if is_view {format!("Describe SQL view \"{}\"",row["name"].as_str().unwrap_or_default())}
                                    else if is_procedure {format!("Describe the stored procedure \"{}\"",row["name"].as_str().unwrap_or_default())}
                                    else {format!("What are the columns of the \"{}\" table?",row["name"].as_str().unwrap_or_default())},
                                "output_format":"json"}})
                        };
                        json!({"status":"ambiguous", "candidate":row,
                        "source_coverage":report["coverage"],
                        "clarification":"Choose a qualified source name; duplicate declarations may still require source inspection. No default schema or deployment binding is inferred.",
                        "recovery":recovery}).to_string()
                    } else if is_function { json!({"function":table,"source_coverage":report["coverage"],
                        "next_page":report["next_page"]}).to_string()
                    } else if is_view { json!({"view":table,"source_coverage":report["coverage"],
                        "next_page":report["next_page"]}).to_string()
                    } else if is_procedure { json!({"stored_procedure":table,"source_coverage":report["coverage"],
                        "next_page":report["next_page"]}).to_string()
                    } else {
                        // The reader already admitted and cited related object
                        // and ALTER declarations. Keep its complete row rather
                        // than silently projecting them out of ordinary answers.
                        table_facts(row, &report, project_id,
                            crate::tool_surface::includes(engram.state.cfg.effective_tool_preset(), "query_schema")).to_string()
                    },
                    generation: Some(generation), commit: None, timestamp: None,
                    confidence: 0.95, relevance: 1.0, extraction_method: "source_sql_schema".into(),
                    warnings: vec![if is_function {
                        "Complete admitted token-normalized SQL function signature/body and lexical references. Return-type checking, referenced-object binding, dependency execution, effective ALTER/DROP order and deployed database behavior are not established."
                    } else if is_view {
                        "Source view declarations and lexical references only. Duplicate names remain candidates; projected column types, default-schema/collation binding, deployment, effective ALTER/DROP order and runtime execution are not established."
                    } else if is_procedure && ambiguous {
                        "Multiple source procedure declarations; choose a qualified name or inspect duplicates. No unique deployed procedure is established."
                    } else if is_procedure {
                        "Complete admitted token-normalized procedure declaration and lexical references. Deployment order, execution, dynamic SQL and runtime argument values are not established."
                    } else if ambiguous {
                        "Ambiguous source-name candidate identity only; columns are not supplied. Choose a qualified name before relying on its schema."
                    } else {
                        "Complete admitted CREATE TABLE, independently cited related object metadata/index declarations and ALTER candidates. Related bodies may be deferred; follow each object's recovery. These are source observations; ALTER/DROP, deployment order and runtime execution are not applied. Omitted nullability remains unknown."
                    }.into()],
                    provider: "schema".into(), score: None, directness: Some(1.0),
                });
            }
        }
        Ok::<_, String>(())
    };
    match tokio::time::timeout(deadline, work).await {
        Ok(Ok(())) => {
            out.provider.status = if failed {
                ProviderStatus::Failed
            } else if out.items.is_empty() {
                ProviderStatus::Empty
            } else {
                ProviderStatus::Hit
            }
        }
        Ok(Err(error)) => {
            out.provider.status = ProviderStatus::Failed;
            notes.push(error);
        }
        Err(_) => {
            out.provider.status = ProviderStatus::TimedOut;
            notes.push("SQL schema retrieval deadline reached".into());
        }
    }
    out.provider.count = out.items.len();
    out.provider.note = Some(notes.join(" "));
    (out.provider.status != ProviderStatus::Empty).then_some(out)
}
