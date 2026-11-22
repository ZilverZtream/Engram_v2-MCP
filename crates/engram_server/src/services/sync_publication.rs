//! Inspect repository-authored event publication contracts alongside indexed writes.
use super::project_policy::{self, PolicyBudget};
use engram_graph::{EdgeKind, GraphStore};
use regex::Regex;
use serde_json::{Value, json};
use std::{collections::{BTreeMap, BTreeSet}, path::Path, sync::LazyLock};

pub(crate) fn inspect(graph: &GraphStore, project: &str, root: &Path, file: &str) -> anyhow::Result<Value> {
    static PUBLISH: LazyLock<Regex> = LazyLock::new(|| Regex::new(
        r"(?i)\b(?:publish|emit|enqueue|raise|dispatch|record|write)\s+(?:(?:its|an?|the|via|using|with|call|event|to)\s+)*`([A-Za-z_]\w*(?:\.\w+)*)\s*(?:\(\))?`"
    ).unwrap());
    let policy = project_policy::resolve_with_budget(root, &[file.to_owned()], &[], PolicyBudget {
        max_sources: 32, max_characters_per_source: 120_000, max_characters_total: 400_000,
    });
    let mut contracts = BTreeMap::new();
    for document in &policy.sources {
        let mut fenced = false;
        for (index, line) in document.content.lines().enumerate() {
            if line.trim_start().starts_with("```") || line.trim_start().starts_with("~~~") { fenced = !fenced; continue; }
            if fenced { continue; }
            let lower = line.to_ascii_lowercase();
            if !["sync", "replicat", "propagat", "tenant"].iter().any(|word| lower.contains(word)) { continue; }
            for hit in PUBLISH.captures_iter(line) {
                let before = &lower[..hit.get(0).unwrap().start()];
                if before.trim_end().ends_with("not") || before.trim_end().ends_with("never") { continue; }
                let name = hit[1].to_owned();
                contracts.entry(name).or_insert_with(|| json!({"file":document.path,"line":index+1,"requirement":line.trim()}));
            }
        }
    }
    if contracts.is_empty() { return Ok(json!({"status":"no_named_publisher_in_applicable_guidance","policy_omissions":policy.omissions.len()})); }
    let source = crate::planner_source::read(graph, project, root, file)?;
    let vb = file.to_ascii_lowercase().ends_with(".vb");
    let masked = super::business_outcome_dependencies::executable_lines(&source, vb);
    let lines: Vec<_> = masked.lines().collect();
    let publishers = contracts.keys().map(|name| {
        let member = name.rsplit('.').next().unwrap_or(name);
        let re = regex::RegexBuilder::new(&format!(r"\b{}\s*\(",regex::escape(member))).case_insensitive(vb).build()?;
        Ok((name,re))
    }).collect::<Result<Vec<_>,regex::Error>>()?;
    let methods = graph.functions_for_source_lines(project, file)?;
    let mut writers = Vec::new();
    let mut publishers_by_table = BTreeMap::<String,BTreeSet<String>>::new();
    let mut partial = methods.len() > 512 || !policy.omissions.is_empty();
    for method in methods.into_iter().take(512) {
        if method.start_line == 0 || method.end_line as usize > lines.len() || method.end_line <= method.start_line { continue; }
        let (edges, truncated) = graph.outgoing_edges_of_kind(project, EdgeKind::QueriesTable, &method.node_id, 128)?;
        partial |= truncated;
        let mut writes = BTreeSet::new();
        for edge in edges {
            if edge.metadata.as_ref().and_then(|m|m["access"].as_str()).is_some_and(|a| matches!(a, "write"|"readwrite"|"delete")) {
                writes.insert(edge.target_id);
            }
        }
        if writes.is_empty() { continue; }
        let mut calls = Vec::new();
        for (line, text) in lines.iter().enumerate().take(method.end_line as usize).skip(method.start_line as usize) {
            for (name, pattern) in &publishers {
                if pattern.is_match(text) { calls.push(json!({"publisher":name,"line":line+1})); }
            }
        }
        if !calls.is_empty() {
            for target in &writes { publishers_by_table.entry(target.clone()).or_default().insert(method.name.clone()); }
        }
        writers.push((method.name,method.start_line,writes,calls));
    }
    let rows = writers.into_iter().map(|(method,line,tables,calls)| {
        let peers = tables.iter().filter_map(|t| publishers_by_table.get(t)).flatten().filter(|p| **p != method).collect::<BTreeSet<_>>();
        let status = if !calls.is_empty() { "direct_publisher_call_observed" }
            else if !peers.is_empty() { "inspect_publication_in_callees_or_transaction" } else { "publisher_applicability_unresolved" };
        json!({"method":method,"file":file,"line":line,"indexed_write_targets":tables,"publisher_calls":calls,"publishing_peers":peers,"status":status})
    }).collect::<Vec<_>>();
    Ok(json!({"status":"inspected","authored_publishers":contracts,"writer_count":rows.len(),"writers":rows,
        "partial":partial,"scope":"Indexed write edges and literal publisher calls in the selected file. Peers share an indexed write target. Indirect events, transaction outcomes, delivery and tenant isolation require separate evidence."}))
}
