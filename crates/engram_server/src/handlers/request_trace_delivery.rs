//! Compact navigation for oversized traces; full evidence uses existing immutable recovery.
use crate::handlers::planning_tools::change_set_delivery as snapshots;
use crate::handlers::search_tools::response_budget::token_bytes;
use rmcp::ErrorData as McpError;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};

pub(super) fn budget(tokens: Option<usize>) -> Result<usize, McpError> {
    Ok(token_bytes(tokens)?.unwrap_or(16_384 * 4))
}

fn node(id: &Value, target: Option<&Value>, nodes: &mut Vec<Value>, indices: &mut BTreeMap<String, usize>) -> usize {
    let key = id.as_str().unwrap_or_default().to_owned();
    let index = *indices.entry(key).or_insert_with(|| {
        nodes.push(json!({"node_id":id})); nodes.len() - 1
    });
    if let Some(target) = target {
        for field in ["name", "file", "line"] {
            nodes[index][field] = target[format!("target_{field}")].clone();
        }
    }
    index
}

fn compact(trace: &mut Value, counts: &mut [usize; 3]) {
    if let Some(paths) = trace.get_mut("paths").and_then(Value::as_array_mut) {
        let mut nodes = Vec::new();
        let mut indices = BTreeMap::new();
        for (index, path) in paths.iter_mut().enumerate() {
            let from = node(&path["from"], None, &mut nodes, &mut indices);
            let to = node(&path["to"], Some(path), &mut nodes, &mut indices);
            let evidence = &path["evidence"];
            let lines = evidence.get("call_site_lines").or_else(|| evidence.get("call_lines")).cloned()
                .unwrap_or_else(|| evidence.get("src_line").map(|line| json!([line])).unwrap_or(json!([])));
            *path = json!({"from":from,"to":to,"kind":path["kind"],
                "target_source_verified":path["target_source_verified"],"source_lines":lines,"evidence_index":index});
            counts[0] += 1;
        }
        trace["path_nodes"] = json!(nodes);
    }
    if let Some(members) = trace.get_mut("source_members").and_then(Value::as_array_mut) {
        for member in members {
            if let Some(object) = member.as_object_mut()
                && let Some(excerpt) = object.remove("excerpt") {
                counts[1] += 1;
                counts[2] += excerpt.as_str().map(str::len).unwrap_or(0);
                object.insert("shown_lines".into(), json!(0));
                object.insert("excerpt_truncated".into(), json!(true));
            }
        }
    }
}

pub(super) fn finish(data: &Path, project: &str, body: String, limit: usize) -> Result<String, McpError> {
    if body.len() <= limit { return Ok(body); }
    let mut report: Value = serde_json::from_str(&body)
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
    let mut counts = [0usize; 3];
    for page in report["pages"].as_array_mut().into_iter().flatten() {
        compact(page, &mut counts);
    }
    for api in report["api_traces"].as_array_mut().into_iter().flatten() {
        for controller in api["controllers"].as_array_mut().into_iter().flatten() {
            compact(controller, &mut counts);
        }
    }
    let id = blake3::hash(body.as_bytes()).to_hex().to_string();
    report["response_delivery"] = json!({
        "status":"DEFERRED_TRACE_EVIDENCE","complete":false,"response_id":id,
        "original_bytes":body.len(),"response_budget_bytes":limit,
        "path_count":counts[0],"omitted_paths":0,"deferred_path_evidence_records":counts[0],
        "deferred_source_excerpts":counts[1],"deferred_excerpt_bytes":counts[2],
        "interpretation":"Each trace retains every selected path. Path from/to integers index that trace's path_nodes; evidence_index locates the same path in the immutable original. Handlers, source locations, hashes, gaps and runtime limits remain inline. Source lines are call-site hints, not a substitute for recovered binding/database evidence.",
        "instruction":"Recover the full original before asserting detailed bindings or database effects. Concatenate recovery content by byte offset and verify response_id as BLAKE3. Recovery does not recheck current source. Existing method-body actions read current source, not this snapshot.",
        "retention":"At most 24 hours or 32 newer request-trace snapshots for this project; unavailable snapshots require a new trace.",
        "next_call":snapshots::request_trace_continuation(project,&id,0,limit)
    });
    let text = serde_json::to_string(&report).map_err(|e| McpError::internal_error(e.to_string(),None))?;
    if text.len() > limit {
        return Err(McpError::invalid_params(format!("max_tokens is too small for the complete trace navigation view: at least {} estimated tokens ({} UTF-8 bytes) required. No path was silently removed.", text.len().div_ceil(4), text.len()),None));
    }
    snapshots::save_request_trace(data,project,&body)?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_trace_keeps_navigation_and_recovers_original_evidence() {
        let temporary = tempfile::tempdir().unwrap();
        let report = json!({"status":"static_candidates","pages":[],"api_traces":[{"controllers":[{
            "handlers":[{"node_id":"controller","file":"Controller.vb","line":15}],
            "paths":[{"from":"controller","to":"service","kind":"interface_implementation_candidate",
                "target_name":"SaveEventsToDB","target_file":"Service.vb","target_line":11,
                "target_source_verified":true,"evidence":{"call_lines":[17],"compiler_details":"å".repeat(80_000)}}],
            "source_members":[{"node_id":"service","file":"Service.vb","line_start":11,"source_blake3":"retained-hash",
                "excerpt":"original source","shown_lines":1,"excerpt_truncated":false}],
            "gaps":["Runtime DI selection is not proven"],"trigger_coverage":"unknown"
        }]}],"coverage":{"runtime_execution_proven":false}});
        let body = report.to_string();
        let first = finish(temporary.path(),"project",body.clone(),65_536).unwrap();
        assert!(first.len() <= 65_536);
        let manifest: Value = serde_json::from_str(&first).unwrap();
        let controller = &manifest["api_traces"][0]["controllers"][0];
        let path = &controller["paths"][0];
        assert_eq!(controller["path_nodes"][path["from"].as_u64().unwrap() as usize]["node_id"],"controller");
        assert_eq!(controller["path_nodes"][path["to"].as_u64().unwrap() as usize]["file"],"Service.vb");
        assert_eq!(path["source_lines"],json!([17]));
        assert_eq!(controller["handlers"],report["api_traces"][0]["controllers"][0]["handlers"]);
        assert_eq!(controller["gaps"],report["api_traces"][0]["controllers"][0]["gaps"]);
        assert_eq!(manifest["coverage"],report["coverage"]);
        assert_eq!(manifest["response_delivery"]["omitted_paths"],0);
        assert_eq!(controller["source_members"][0]["source_blake3"],"retained-hash");
        let id = manifest["response_delivery"]["response_id"].as_str().unwrap();
        let mut recovered = String::new();
        loop {
            let page = snapshots::recover_request_trace(temporary.path(),"project",id,recovered.len(),65_536).unwrap();
            assert!(page.len() <= 65_536);
            let page: Value = serde_json::from_str(&page).unwrap();
            recovered.push_str(page["content"].as_str().unwrap());
            if page["complete"] == true { break; }
            assert_eq!(page["next_call"]["tool"],"trace_user_request");
        }
        assert_eq!(recovered,body);
        assert_eq!(blake3::hash(recovered.as_bytes()).to_hex().as_str(),id);
    }

    #[test]
    fn ordinary_trace_is_unchanged_and_small_budget_refuses_missing_navigation() {
        let temporary = tempfile::tempdir().unwrap();
        let body = json!({"pages":[],"api_traces":[],"coverage":{"runtime_execution_proven":false}}).to_string();
        assert_eq!(finish(temporary.path(),"project",body.clone(),65_536).unwrap(),body);
        assert!(finish(temporary.path(),"project",body,4).is_err());
        assert_eq!(std::fs::read_dir(temporary.path()).unwrap().count(),0);
    }
}
