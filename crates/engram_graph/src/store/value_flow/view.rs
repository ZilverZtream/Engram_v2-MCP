use super::*;

const MAX_POINTS: usize = 128;
const MAX_EDGES: usize = 256;
const MAX_CONTEXTS: usize = 16;
const MAX_DEPTH: usize = 3;

struct Context {
    owner: String,
    parent: Option<usize>,
    call_site: Option<String>,
    depth: usize,
    points: BTreeMap<String, String>,
}

impl GraphStore {
    /// Invocation contexts keep two calls to the same helper from borrowing
    /// each other's arguments or returns. All facts come from one read snapshot.
    pub fn trace_value_flow(
        &self,
        project: &str,
        file: &str,
        entry: &str,
        source_hash: &str,
    ) -> anyhow::Result<JsonValue> {
        let rtx = self.db.begin_read()?;
        let nodes = rtx.open_table(NODES)?;
        let edges = rtx.open_table(EDGES)?;
        let file_id = engram_core::ids::NodeId::file(file).0;
        let file_key = format!("{project}\0{file_id}");
        let file_node = nodes
            .get(file_key.as_str())?
            .map(|v| bincode::deserialize::<Node>(v.value()))
            .transpose()?;
        if file_node.as_ref().and_then(|node| field(node, "file_hash")) != Some(source_hash) {
            return Ok(
                json!({"status":"unavailable", "reason":"Entry source does not match the indexed snapshot; refresh the file before tracing values.", "nodes":[],"edges":[]}),
            );
        }
        let mut candidates = Vec::new();
        let prefix = format!("{project}\0");
        for row in nodes.range(prefix.as_str()..)? {
            let (key, value) = row?;
            if !key.value().starts_with(&prefix) {
                break;
            }
            let node: Node = bincode::deserialize(value.value())?;
            if node.node_type != "function" || node.file_path.as_str() != file {
                continue;
            }
            let written = local::canonical(entry, &node.language);
            let name = local::canonical(callable_name(&node), &node.language);
            if node.node_id == entry
                || name == written
                || name.rsplit('.').next() == Some(written.as_str())
            {
                candidates.push(node);
            }
        }
        if candidates.len() != 1 {
            return Ok(
                json!({"status":"unavailable", "reason":"Entry method is missing or ambiguous; supply an exact declaration node ID.",
                "candidates":candidates.iter().take(16).map(|node| json!({"node_id":node.node_id,"name":node.name,"line":node.start_line})).collect::<Vec<_>>(),
                "candidates_truncated":candidates.len()>16,"nodes":[],"edges":[]}),
            );
        }
        let entry_node = &candidates[0];
        if file_node
            .as_ref()
            .is_none_or(|file| file.generation != entry_node.generation)
        {
            return Ok(
                json!({"status":"unavailable","reason":"Entry declaration belongs to a different source generation; refresh before tracing values.","nodes":[],"edges":[]}),
            );
        }
        let mut contexts = vec![Context {
            owner: entry_node.node_id.clone(),
            parent: None,
            call_site: None,
            depth: 0,
            points: BTreeMap::new(),
        }];
        let mut output_nodes = Vec::new();
        let mut output_edges = Vec::new();
        let mut issues = std::collections::BTreeSet::new();
        let mut truncated = false;
        let mut cursor = 0;
        while cursor < contexts.len() {
            let owner = contexts[cursor].owner.clone();
            let prefix = format!(
                "{project}\0value:{}:",
                blake3::hash(owner.as_bytes()).to_hex()
            );
            let owner_key = format!("{project}\0{owner}");
            let current_owner = nodes
                .get(owner_key.as_str())?
                .map(|value| bincode::deserialize::<Node>(value.value()))
                .transpose()?;
            let summary_key = format!("{prefix}0");
            let summary = nodes
                .get(summary_key.as_str())?
                .map(|value| bincode::deserialize::<Node>(value.value()))
                .transpose()?;
            let ready =
                current_owner
                    .as_ref()
                    .zip(summary.as_ref())
                    .is_some_and(|(owner, summary)| {
                        owner.generation == summary.generation
                            && field(summary, "owner_id") == Some(owner.node_id.as_str())
                            && field(summary, "producer") == Some(METADATA_KEY)
                    });
            if !ready {
                issues.insert("Value lineage is missing or belongs to an older declaration generation; refresh must finish before these values can be traced.".into());
                cursor += 1;
                continue;
            }
            let mut found = false;
            for row in nodes.range(prefix.as_str()..)? {
                let (key, value) = row?;
                if !key.value().starts_with(&prefix) {
                    break;
                }
                found = true;
                let point: Node = bincode::deserialize(value.value())?;
                let Some(metadata) = point.metadata.as_ref() else {
                    issues.insert("Stored value point lacks source metadata.".into());
                    continue;
                };
                if let Some(values) = metadata.get("issues").and_then(JsonValue::as_array) {
                    for issue in values {
                        if let Some(issue) = issue.as_str() {
                            issues.insert(issue.to_string());
                        }
                    }
                }
                if field(&point, "role") == Some("summary") {
                    continue;
                }
                if output_nodes.len() == MAX_POINTS {
                    truncated = true;
                    break;
                }
                let id = format!("n{}", output_nodes.len());
                contexts[cursor]
                    .points
                    .insert(point.node_id.clone(), id.clone());
                let site = &metadata["site"];
                output_nodes.push(json!({"id":id,"context":format!("c{cursor}"),"node_id":point.node_id,
                    "role":metadata["role"],"name":point.name,"file_path":point.file_path,"line":point.start_line,
                    "column":site["column"],"offset_unit":metadata["offset_unit"],"generation":point.generation,
                    "source":site["text"].as_str().unwrap_or("").chars().take(160).collect::<String>(),
                    "binding":metadata["binding"]}));
                if let Some(callee) = metadata.get("callee_id").and_then(JsonValue::as_str) {
                    if contexts[cursor].depth >= MAX_DEPTH || contexts.len() >= MAX_CONTEXTS {
                        truncated = true;
                        continue;
                    }
                    contexts.push(Context {
                        owner: callee.into(),
                        parent: Some(cursor),
                        call_site: Some(point.node_id),
                        depth: contexts[cursor].depth + 1,
                        points: BTreeMap::new(),
                    });
                }
            }
            if !found {
                issues.insert("No stored value syntax for a selected method; refresh with a supported extractor.".into());
            }
            cursor += 1;
        }
        for (index, context) in contexts.iter().enumerate() {
            for (source, source_id) in &context.points {
                let prefix = format!("{project}\0value_flow\0{source}\0");
                for (scanned, row) in edges.range(prefix.as_str()..)?.enumerate() {
                    let (key, value) = row?;
                    if !key.value().starts_with(&prefix) {
                        break;
                    }
                    if scanned >= 2048 || output_edges.len() >= MAX_EDGES {
                        truncated = true;
                        break;
                    }
                    let edge: Edge = bincode::deserialize(value.value())?;
                    let Some(metadata) = edge.metadata.as_ref() else {
                        continue;
                    };
                    let call_site = metadata.get("call_site").and_then(JsonValue::as_str);
                    let relation = metadata
                        .get("relation")
                        .and_then(JsonValue::as_str)
                        .unwrap_or("");
                    let destination = match relation {
                        "argument_parameter" => contexts.iter().find(|child| {
                            child.parent == Some(index) && child.call_site.as_deref() == call_site
                        }),
                        "return_value" if call_site == context.call_site.as_deref() => {
                            context.parent.map(|parent| &contexts[parent])
                        }
                        "return_value" => None,
                        _ => Some(context),
                    };
                    let Some(target_id) = destination.and_then(|c| c.points.get(&edge.target_id))
                    else {
                        continue;
                    };
                    output_edges.push(json!({"from":source_id,"to":target_id,"relation":relation,"binding":metadata["binding"]}));
                }
            }
        }
        if truncated {
            issues.insert(
                "Trace budget reached; omitted points, calls or edges are not proven absent."
                    .into(),
            );
        }
        Ok(
            json!({"status":if output_nodes.is_empty() {"unavailable"} else if issues.is_empty() {"supported_static_flow"} else {"partial"},
            "entry_node_id":entry_node.node_id,"entry_source_verified":true,"runtime_value_proven":false,
            "scope":"Authored value dependencies and possible branch joins. Callees use indexed snapshots; their current disk bytes are not verified. No runtime, path feasibility, exception or heap-alias proof.",
            "contexts":contexts.iter().enumerate().map(|(i,c)| json!({"id":format!("c{i}"),"owner_id":c.owner,
                "parent":c.parent.map(|p|format!("c{p}")),"call_site":c.call_site})).collect::<Vec<_>>(),
            "nodes":output_nodes,"edges":output_edges,"issues":issues,"truncated":truncated,
            "limits":{"points":MAX_POINTS,"edges":MAX_EDGES,"contexts":MAX_CONTEXTS,"call_depth":MAX_DEPTH}}),
        )
    }
}
