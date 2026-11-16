//! Complete source-declared table evidence for the existing SQL question path.
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

pub(super) async fn gather(
    engram: &Engram,
    project_id: &str,
    plan: &QueryPlan,
    generation: u64,
    view: Option<Arc<SourceSnapshotGuard>>,
    deadline: Duration,
) -> Option<SchemaEvidence> {
    if !plan.modalities.iter().any(|m| matches!(m, Modality::Sql)) {
        return None;
    }
    let mut requests = Vec::new();
    for entity in &plan.entities {
        let is_table = entity.resolved.iter().any(|r| r.kind == EntityKind::Table)
            || entity.guessed_kind == EntityKind::Table
            || (plan.contract.entity_type == ContractEntityType::Table
                && entity.guessed_kind != EntityKind::File);
        if !is_table || requests.iter().any(|r: &QuerySchemaRequest| {
            matches!(&r.query_type, Some(SchemaQueryType::Table { name }) if name == &entity.text)
        }) {
            continue;
        }
        let request = QuerySchemaRequest {
            project_id: project_id.into(),
            // Preserve qualification from the question, never a basename graph identity.
            query_type: Some(SchemaQueryType::Table {
                name: entity.text.clone(),
            }),
            offset: Some(0),
            limit: Some(100),
            max_tokens: Some(65536),
            response_id: None,
            response_offset: 0,
        };
        if schema_query::validate(&request).is_ok() {
            requests.push(request);
        }
    }
    if requests.is_empty() {
        return None;
    }
    let mut out = SchemaEvidence {
        items: vec![],
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
    let mut notes = vec!["Source declarations only; no live database, default-schema or collation binding. At most3 named-table lookups; examined counts source-file examinations across lookups.".to_string()];
    let mut failed = false;
    let work = async {
        let view =
            view.ok_or_else(|| "Selected source unavailable for SQL schema evidence".to_string())?;
        for request in requests.into_iter().take(3) {
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
            out.ambiguous |= total > 1;
            *out.provider.available.as_mut().unwrap() += total;
            out.provider.examined += report["coverage"]["selected_sql_files"]
                .as_u64()
                .unwrap_or(0) as usize;
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
            }
            for row in report["results"].as_array().into_iter().flatten() {
                let ambiguous = report["status"] == "ambiguous";
                let table = if ambiguous { row } else { &row["table"] };
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
                    evidence_id: format!("schema_{}", out.items.len() + 1),
                    document_id: None, document_namespace: None,
                    source_verification: Some(format!("SELECTED_SOURCE_FILE_HASH:{hash}")),
                    kind: EvidenceKind::SourceCode, authority: Authority::CurrentCode,
                    path: Some(path.into()), lines: Some((start as u32, end as u32)),
                    symbol_id: Some(format!("schema:{path}:{start}:{hash}")),
                    title: Some(format!("Source table {}", table["name"])),
                    content: if ambiguous { json!({"status":"ambiguous", "candidate":row,
                        "source_coverage":report["coverage"],
                        "clarification":"Choose a qualified source name; duplicate declarations may still require source inspection. No default schema or deployment binding is inferred.",
                        "recovery":{"tool":"query_schema","arguments":{"project_id":project_id,"query_type":{"kind":"table","name":row["name"]}}}}).to_string()
                    } else { json!({"table":table,"source_coverage":report["coverage"],
                        "table_gaps":report["table_gaps"],"unknown_nullability_columns":report["unknown_nullability_columns"],
                        "next_page":report["next_page"]}).to_string() },
                    generation: Some(generation), commit: None, timestamp: None,
                    confidence: 0.95, relevance: 1.0, extraction_method: "source_sql_schema".into(),
                    warnings: vec![if ambiguous {
                        "Ambiguous source-name candidate identity only; columns are not supplied. Choose a qualified name before relying on its schema."
                    } else {
                        "Complete admitted CREATE TABLE declaration; ALTER/DROP and deployment order are not applied. Omitted nullability remains unknown."
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
    Some(out)
}
