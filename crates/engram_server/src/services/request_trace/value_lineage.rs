//! Compose the existing indexed value reader with resolved request entries.
use super::{execution, routing::Source};
use engram_graph::GraphStore;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

const MAX_ENTRY_REPORTS: usize = 8;

fn unavailable(reason: String) -> Value {
    json!({"status":"unavailable","reason":reason,"nodes":[],"edges":[],"contexts":[],
        "runtime_value_proven":false})
}

fn selected_points(
    graph: &GraphStore,
    project: &str,
    flow: &Value,
    sources: &[Source<'_>],
    selected: &BTreeMap<String, u64>,
) -> anyhow::Result<Vec<Value>> {
    let mut seen = BTreeSet::new();
    let mut verified = Vec::new();
    for point in flow["nodes"].as_array().into_iter().flatten() {
        let path = point["file_path"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Value point has no source path"))?;
        let revision = point["generation"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("Value point has no source revision"))?;
        anyhow::ensure!(
            selected.get(path) == Some(&revision),
            "Value point source revision is not selected: {path}"
        );
        if !seen.insert((path.to_owned(), revision)) {
            continue;
        }
        let source = sources
            .iter()
            .find(|source| source.path == path)
            .ok_or_else(|| anyhow::anyhow!("Value source was not admitted: {path}"))?;
        let file = graph
            .get_node(project, &format!("file:{path}"))?
            .ok_or_else(|| anyhow::anyhow!("Value source fingerprint is unavailable: {path}"))?;
        let hash = blake3::hash(source.content.as_bytes()).to_hex().to_string();
        anyhow::ensure!(
            file.generation == revision
                && file
                    .metadata
                    .as_ref()
                    .and_then(|meta| meta.get("file_hash"))
                    .and_then(Value::as_str)
                    == Some(hash.as_str()),
            "Value source differs from selected indexed fingerprint: {path}"
        );
        verified.push(json!({"file":path,"physical_revision":revision,"source_blake3":hash}));
    }
    Ok(verified)
}

fn entry(
    graph: &GraphStore,
    project: &str,
    handler: &Value,
    sources: &[Source<'_>],
    selected: &BTreeMap<String, u64>,
) -> anyhow::Result<Value> {
    let id = handler["node_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Entry declaration identity unavailable"))?;
    let node = graph
        .get_node(project, id)?
        .ok_or_else(|| anyhow::anyhow!("Entry declaration unavailable"))?;
    anyhow::ensure!(
        node.node_type == "function"
            && execution::verified_node(graph, project, &node, sources, selected)?,
        "Entry source is unavailable or differs from selected physical revision"
    );
    let source = sources
        .iter()
        .find(|source| source.path == node.file_path.as_str())
        .ok_or_else(|| anyhow::anyhow!("Entry source was not admitted"))?;
    let hash = blake3::hash(source.content.as_bytes()).to_hex().to_string();
    let mut flow = graph.trace_value_flow(project, source.path, id, &hash)?;
    // The original reader labels peer disk bytes unverified. Strengthen only
    // source admission here; never upgrade its semantic status or limitations.
    let verification = match selected_points(graph, project, &flow, sources, selected) {
        Ok(files) => {
            json!({"status":if files.is_empty() {"no_returned_points"} else {"returned_points_selected_source_checked"},"files":files,
            "scope":"Each returned point matches the selected physical file revision and admitted current raw source hash. Checks are not an atomic graph/disk snapshot; no runtime value or semantic completeness proof."})
        }
        Err(error) => {
            flow = unavailable(format!("Selected-source value lineage withheld: {error:#}"));
            json!({"status":"withheld","files":[]})
        }
    };
    Ok(
        json!({"entry_node_id":id,"file":source.path,"line":node.start_line,
        "source_blake3":hash,"flow":flow,"source_verification":verification}),
    )
}

fn attach_trace(
    graph: &GraphStore,
    project: &str,
    trace: &mut Value,
    sources: &[Source<'_>],
    selected: &BTreeMap<String, u64>,
    remaining: &mut usize,
) {
    let mut reports = Vec::new();
    let mut omitted = 0usize;
    for handler in trace["handlers"].as_array().into_iter().flatten() {
        if *remaining == 0 {
            omitted += 1;
            continue;
        }
        *remaining -= 1;
        reports.push(match entry(graph, project, handler, sources, selected) {
            Ok(report) => report,
            Err(error) => json!({"entry_node_id":handler["node_id"],
                "flow":unavailable(format!("Entry value lineage unavailable: {error:#}")),
                "source_verification":{"status":"withheld","files":[]}}),
        });
    }
    let count = reports.len();
    trace["value_lineage"] = json!(reports);
    trace["value_lineage_coverage"] = json!({"entry_reports":count,"entries_omitted":omitted,
        "max_total_entry_reports":MAX_ENTRY_REPORTS,
        "interpretation":"Existing indexed static value dependencies, separated by invocation context. Partial/unavailable/omitted lineage is not absence of a dependency; source order and bindings do not prove execution."});
}

pub(crate) fn attach(
    graph: &GraphStore,
    project: &str,
    report: &mut Value,
    sources: &[Source<'_>],
    selected: &BTreeMap<String, u64>,
) {
    let mut remaining = MAX_ENTRY_REPORTS;
    for page in report["pages"].as_array_mut().into_iter().flatten() {
        attach_trace(graph, project, page, sources, selected, &mut remaining);
    }
    for api in report["api_traces"].as_array_mut().into_iter().flatten() {
        for controller in api["controllers"].as_array_mut().into_iter().flatten() {
            attach_trace(
                graph,
                project,
                controller,
                sources,
                selected,
                &mut remaining,
            );
        }
    }
}
