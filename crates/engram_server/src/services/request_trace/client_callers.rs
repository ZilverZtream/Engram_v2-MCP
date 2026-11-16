//! Compose existing indexed browser-to-server routes into request navigation.
use super::{execution, routing::Source};
use anyhow::{Context, Result, ensure};
use engram_graph::{EdgeKind, GraphStore, Node};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::Path,
};

const MAX_ENTRIES: usize = 8;
const MAX_CALLERS: usize = 64;

struct Capture<'a> {
    graph: &'a GraphStore,
    project: &'a str,
    root: &'a Path,
    selected: &'a BTreeMap<String, u64>,
    source_bytes: &'a mut usize,
    scripts: BTreeMap<String, String>,
}

impl Capture<'_> {
    fn caller(&mut self, node: &Node, target: &str) -> Result<Value> {
        let path = node.file_path.as_str();
        ensure!(
            self.selected.get(path) == Some(&node.generation),
            "caller revision is not selected"
        );
        if !self.scripts.contains_key(path) {
            let limit = (64usize * 1024 * 1024)
                .saturating_sub(*self.source_bytes)
                .min(2 * 1024 * 1024);
            ensure!(limit > 0, "shared request source-byte budget exhausted");
            let target = engram_core::safe_join(self.root, path)?;
            let mut content = String::new();
            std::fs::File::open(target)?
                .take(limit as u64 + 1)
                .read_to_string(&mut content)?;
            ensure!(
                content.len() <= limit,
                "caller source exceeds remaining file/combined source-byte budget"
            );
            *self.source_bytes += content.len();
            self.scripts.insert(path.to_owned(), content);
        }
        let content = &self.scripts[path];
        ensure!(
            execution::verified_node(
                self.graph,
                self.project,
                node,
                &[Source { path, content }],
                self.selected
            )?,
            "caller source/hash is not current"
        );
        let edge = self
            .graph
            .get_edges_by_endpoints(
                self.project,
                &[(node.node_id.clone(), EdgeKind::ApiCall, target.to_owned())],
            )?
            .into_iter()
            .next()
            .context("indexed API edge is unavailable")?;
        ensure!(
            edge.generation == node.generation,
            "API edge revision differs from selected caller"
        );
        let evidence = edge.metadata.unwrap_or(Value::Null);
        let line = evidence
            .get("src_line")
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .context("indexed API call has no source line")?;
        ensure!(line > 0, "indexed API call has no valid source line");
        let excerpt = content
            .lines()
            .nth(line as usize - 1)
            .context("API call line exceeds source")?;
        let shown: String = excerpt.chars().take(2048).collect();
        Ok(
            json!({"node_id":node.node_id,"name":node.name,"file":path,"line":line,
            "target_node_id":target,"kind":"api_call","physical_revision":node.generation,
            "source_blake3":blake3::hash(content.as_bytes()).to_hex().to_string(),
            "excerpt_truncated":shown.len() < excerpt.len(),"excerpt":shown,"evidence":evidence,
            "interpretation":"Existing indexed static API-call candidate; source verification does not prove URL binding, script inclusion, event wiring, verb or runtime execution."}),
        )
    }
}

pub(crate) fn attach(
    graph: &GraphStore,
    project: &str,
    root: &Path,
    report: &mut Value,
    sources: &[Source<'_>],
    selected: &BTreeMap<String, u64>,
    source_bytes: &mut usize,
) -> BTreeSet<String> {
    let mut entries = BTreeSet::new();
    for trace in report["pages"].as_array().into_iter().flatten().chain(
        report["api_traces"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|api| api["controllers"].as_array().into_iter().flatten()),
    ) {
        for handler in trace["handlers"].as_array().into_iter().flatten() {
            if let Some(id) = handler["node_id"].as_str() {
                entries.insert(id.to_owned());
            }
        }
    }
    let mut capture = Capture {
        graph,
        project,
        root,
        selected,
        source_bytes,
        scripts: BTreeMap::new(),
    };
    let mut callers = Vec::new();
    let mut gaps = Vec::new();
    let mut inspected = 0;
    let mut truncated = entries.len() > MAX_ENTRIES;
    let mut returned_files = BTreeSet::new();
    for target in entries.iter().take(MAX_ENTRIES) {
        let result = (|| -> Result<()> {
            let target_node = graph
                .get_node(project, target)?
                .context("handler is unavailable")?;
            ensure!(
                execution::verified_node(graph, project, &target_node, sources, selected)?,
                "handler source/hash is not current"
            );
            let (incoming, more) = graph.incoming_edge_prefix(
                project,
                EdgeKind::ApiCall,
                target,
                MAX_CALLERS.saturating_sub(inspected),
            )?;
            inspected += incoming.len();
            truncated |= more;
            for (id, _, _) in incoming {
                let candidate = (|| -> Result<Option<Value>> {
                    let node = graph
                        .get_node(project, &id)?
                        .context("caller node is unavailable")?;
                    let path = node.file_path.as_str().to_ascii_lowercase();
                    if ![".js", ".jsx", ".ts", ".tsx"]
                        .iter()
                        .any(|ext| path.ends_with(ext))
                    {
                        return Ok(None);
                    }
                    capture.caller(&node, target).map(Some)
                })();
                match candidate {
                    Ok(Some(caller)) => {
                        returned_files
                            .insert(caller["file"].as_str().unwrap_or_default().to_owned());
                        callers.push(caller);
                    }
                    Ok(None) => (),
                    Err(error) => gaps.push(format!("{id} -> {target}: withheld: {error:#}")),
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            gaps.push(format!("{target}: browser callers withheld: {error:#}"));
        }
    }
    report["client_caller_coverage"] = json!({"status":if truncated || !gaps.is_empty() {"partial"} else {"selected_indexed_candidates"},
        "entries_inspected":entries.len().min(MAX_ENTRIES),"entries_omitted":entries.len().saturating_sub(MAX_ENTRIES),
        "incoming_edges_inspected":inspected,"callers_returned":callers.len(),"script_files_read":capture.scripts.len(),
        "max_entry_reports":MAX_ENTRIES,"max_incoming_edges":MAX_CALLERS,"truncated":truncated,"gaps":gaps,
        "runtime_execution_proven":false,
        "interpretation":"Bounded existing incoming API edges for selected handler entries, not complete client discovery. Missing/withheld callers are not proof of no usage; script inclusion, event dispatch, dynamic URLs and runtime binding remain unknown."});
    report["client_callers"] = json!(callers);
    returned_files
}
