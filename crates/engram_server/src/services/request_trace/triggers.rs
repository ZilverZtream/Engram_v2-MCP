//! Source trigger candidates for reached literal SQL writes, never execution proof.
use super::routing::Source;
use crate::services::schema_query::{self, Catalog, Name};
use serde_json::{Value, json};
use sqlparser::tokenizer::Token;
use std::collections::BTreeSet;

const PATH_LIMIT: usize = 128;

// Retain exact authored qualification. Existing report table names use a
// basename convention, which cannot establish schema identity on its own.
fn qualified_writes(sql: &str) -> Vec<(&'static str, Name)> {
    let Ok(tokens) = schema_query::tokens(sql) else {
        return Vec::new();
    };
    let mut writes = Vec::new();
    for i in 0..tokens.len() {
        let Some(operation) = ["INSERT", "UPDATE", "DELETE", "MERGE"]
            .into_iter()
            .find(|word| schema_query::kw(&tokens, i, word))
        else {
            continue;
        };
        let mut after = i + 1;
        if schema_query::kw(&tokens, after, "INTO")
            || (operation == "DELETE" && schema_query::kw(&tokens, after, "FROM"))
        {
            after += 1;
        }
        if let Some((name, _)) = schema_query::name_at(&tokens, after)
            && name.0.len() >= 2
        {
            writes.push((operation, name));
        }
    }
    writes
}

fn lexical_body(sql: &str) -> String {
    let tokens = schema_query::tokens(sql).unwrap_or_default();
    let body = (0..tokens.len())
        .find(|&i| schema_query::kw(&tokens, i, "AS"))
        .map_or(0, |i| i + 1);
    tokens[body..]
        .iter()
        .map(|token| match &token.token {
            Token::Word(_) | Token::Number(_, _) => token.token.to_string(),
            _ if token.token.to_string().chars().any(char::is_alphabetic) => " ".into(),
            _ => token.token.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn attach(report: &mut Value, sources: &[Source<'_>]) {
    if report["coverage"]["include_db"] != true || report["coverage"]["include_triggers"] != true {
        return;
    }
    let mut catalog = Catalog::default();
    for source in sources
        .iter()
        .filter(|source| source.path.to_ascii_lowercase().ends_with(".sql"))
    {
        if let Err(error) = catalog.add_source(source.path, source.content) {
            catalog.gaps.push(format!("{}: {error}", source.path));
        }
    }
    let traces = report["pages"].as_array_mut().into_iter().flatten();
    for trace in traces {
        attach_trace(trace, &catalog, sources);
    }
    for api in report["api_traces"].as_array_mut().into_iter().flatten() {
        for trace in api["controllers"].as_array_mut().into_iter().flatten() {
            attach_trace(trace, &catalog, sources);
        }
    }
}

fn attach_trace(trace: &mut Value, catalog: &Catalog, sources: &[Source<'_>]) {
    let writes: Vec<_> = trace["paths"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|hop| {
            hop["kind"]
                .as_str()
                .is_some_and(|kind| kind.ends_with("_writes_table"))
        })
        .cloned()
        .collect();
    let mut paths = Vec::new();
    let mut members = Vec::new();
    let mut seen = BTreeSet::new();
    let mut unknown = 0usize;
    let mut omitted = 0usize;
    let original_count = trace["paths"].as_array().map_or(0, Vec::len);
    for hop in &writes {
        let sql = hop["evidence"]["sql"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| {
                // Procedure write evidence cites the declaration already admitted
                // by the existing request tracer. Use that exact member excerpt.
                let file = hop["evidence"]["file"].as_str()?;
                let line = hop["evidence"]["line"].as_u64()?;
                trace["source_members"]
                    .as_array()?
                    .iter()
                    .find(|member| member["file"] == file && member["line_start"] == line)
                    .and_then(|member| member["excerpt"].as_str())
                    .map(str::to_owned)
            });
        let target = hop["target_name"]
            .as_str()
            .and_then(|name| schema_query::input_name(name).ok());
        let Some((sql, target)) = sql.zip(target) else {
            unknown += 1;
            continue;
        };
        let candidates: Vec<_> = qualified_writes(&sql)
            .into_iter()
            .filter(|(_, name)| {
                name.0
                    .last()
                    .zip(target.0.last())
                    .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b))
            })
            .collect();
        if candidates.is_empty() {
            unknown += 1;
        }
        for (operation, name) in candidates {
            for object in catalog
                .objects
                .iter()
                .filter(|object| object.kind == "trigger")
            {
                let (Some(table), Some(header)) = (&object.target, &object.trigger_header) else {
                    continue;
                };
                if !table.matches(&name)
                    || (operation != "MERGE"
                        && !header.events.iter().any(|event| event == operation))
                {
                    continue;
                }
                let id = format!(
                    "source-trigger:{}:{}:{}",
                    object.citation.path, object.citation.start_line, object.name
                );
                if !seen.insert((hop["to"].to_string(), id.clone(), operation)) {
                    continue;
                }
                if original_count + paths.len() >= PATH_LIMIT {
                    omitted += 1;
                    continue;
                }
                paths.push(json!({"from":hop["to"],"to":id,"kind":"declared_trigger_candidate",
                    "target_name":object.name,"target_file":object.citation.path,"target_line":object.citation.start_line,"target_source_verified":true,
                    "evidence":{"origin":"exact_qualified_source_trigger_declaration","write_operation":operation,
                        "qualified_table":name.display(),"timing":header.timing,"events":header.events,"citation":object.citation,
                        "write_source":hop["evidence"],"interpretation":"Matching source DML/event and trigger declaration. Deployment, enablement, branch reachability, database identity and trigger execution are not established."}}));
                if !members.iter().any(|member: &Value| member["node_id"] == id) {
                    // The shared catalog citation spans raw source lines; keep
                    // raw source rather than presenting tokenized SQL as a quote.
                    if let Some(source) = sources
                        .iter()
                        .find(|source| source.path == object.citation.path)
                    {
                        let excerpt = source
                            .content
                            .split_inclusive('\n')
                            .skip(object.citation.start_line.saturating_sub(1) as usize)
                            .take(
                                (object.citation.end_line - object.citation.start_line + 1)
                                    as usize,
                            )
                            .collect::<String>();
                        members.push(json!({"node_id":id,"file":object.citation.path,"line_start":object.citation.start_line,
                            "line_end":object.citation.end_line,"source_blake3":object.citation.source_hash,"excerpt":excerpt,
                            "interpretation":"Current source trigger declaration; deployment and execution unknown."}));
                    }
                }
                let refs = engram_index::sql_parser::analyze_body_references(&lexical_body(
                    &object.declaration,
                ));
                for (kind, names) in [
                    ("trigger_body_reads_table", refs.tables_read),
                    ("trigger_body_writes_table", refs.tables_written),
                ] {
                    for target in names.into_iter().filter(|name| {
                        !["inserted", "deleted"]
                            .iter()
                            .any(|pseudo| name.eq_ignore_ascii_case(pseudo))
                    }) {
                        if original_count + paths.len() >= PATH_LIMIT {
                            omitted += 1;
                            continue;
                        }
                        paths.push(json!({"from":id,"to":format!("sql-table-reference:{target}"),"kind":kind,"target_name":target,
                            "target_file":null,"target_line":null,"target_source_verified":false,
                            "evidence":{"origin":"existing_source_sql_body_analyzer","citation":object.citation,
                                "interpretation":"Lexical body reference using the existing basename convention. No physical table binding, nested trigger/procedure expansion or execution proof."}}));
                    }
                }
            }
        }
    }
    trace["paths"].as_array_mut().unwrap().extend(paths);
    trace["source_members"]
        .as_array_mut()
        .unwrap()
        .extend(members);
    trace["trigger_coverage"] = json!("source_candidates_only_runtime_unknown");
    trace["trigger_source_coverage"] = json!({"write_references_inspected":writes.len(),"unresolved_write_bindings":unknown,
        "omitted_trigger_paths":omitted,"catalog_gaps":catalog.gaps,
        "scope":"Selected current SQL source declarations; exact authored qualified names/event candidates only. Missing declarations do not establish trigger absence. Dynamic/alias SQL, nested trigger/procedure effects and deployed state remain unresolved."});
}
