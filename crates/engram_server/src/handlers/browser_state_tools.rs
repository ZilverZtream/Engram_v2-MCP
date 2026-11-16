//! Source-admitted store-wide browser mutations alongside existing key references.
use crate::services::{
    project_policy,
    request_trace::{execution, routing::Source},
};
use crate::{models::TraceStateUsageRequest, tools::Engram};
use anyhow::{Context, Result, ensure};
use engram_graph::{EdgeKind, GraphStore};
use rmcp::{
    ErrorData as McpError,
    model::{CallToolResult, Content},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::Path,
};

fn number(value: &Value, field: &str) -> Result<usize> {
    value
        .get(field)
        .and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .and_then(|n| usize::try_from(n).ok())
        .with_context(|| format!("Missing indexed {field}"))
}

fn collect(
    graph: &GraphStore,
    project: &str,
    root: &Path,
    store: &str,
    key: &str,
    selected: &BTreeMap<String, u64>,
    limit: usize,
) -> Result<Value> {
    let target = engram_core::ids::NodeId::state(&format!("{store}ClearAll"), "*").0;
    let (incoming, truncated) =
        graph.incoming_edge_prefix(project, EdgeKind::WritesState, &target, limit)?;
    let mut cache: BTreeMap<String, String> = BTreeMap::new();
    let mut bytes = 0usize;
    let mut candidates = Vec::new();
    let mut gaps = Vec::new();
    let mut paths = BTreeSet::new();
    if incoming.is_empty() {
        gaps.push("No retained indexed clear candidate. This does not prove absence; refresh source extraction for indexes predating the browser clear contract.".into());
    }
    for (id, _, _) in incoming {
        let result = (|| -> Result<Value> {
            let node = graph
                .get_node(project, &id)?
                .context("Clear caller node unavailable")?;
            let path = node.file_path.as_str();
            ensure!(
                selected.get(path) == Some(&node.generation),
                "Clear caller revision is not selected: {path}"
            );
            if !cache.contains_key(path) {
                let allowance = (8usize * 1024 * 1024).saturating_sub(bytes).min(512 * 1024);
                ensure!(
                    allowance > 0,
                    "Clear source capture budget exhausted: {path}"
                );
                let mut source = String::new();
                std::fs::File::open(engram_core::safe_join(root, path)?)?
                    .take(allowance as u64 + 1)
                    .read_to_string(&mut source)?;
                ensure!(
                    source.len() <= allowance,
                    "Clear source exceeds file/capture budget: {path}"
                );
                bytes += source.len();
                cache.insert(path.to_owned(), source);
            }
            let source = &cache[path];
            // A file-level script owner can include the final empty line in its
            // parser range. Its selected file fingerprint is the admission proof;
            // containing methods retain the existing member-span checks.
            let current = if node.node_type == "file" {
                node.node_id == engram_core::ids::NodeId::file(path).0
                    && node
                        .metadata
                        .as_ref()
                        .and_then(|v| v.get("file_hash"))
                        .and_then(Value::as_str)
                        .is_some_and(|hash| {
                            hash == blake3::hash(source.as_bytes()).to_hex().as_str()
                        })
            } else {
                execution::verified_node(
                    graph,
                    project,
                    &node,
                    &[Source {
                        path,
                        content: source,
                    }],
                    selected,
                )?
            };
            ensure!(current, "Clear source hash is not current/selected: {path}");
            let edge = graph
                .get_edges_by_endpoints(
                    project,
                    &[(id.clone(), EdgeKind::WritesState, target.clone())],
                )?
                .into_iter()
                .next()
                .context("Clear edge unavailable")?;
            ensure!(
                edge.generation == node.generation,
                "Clear edge revision differs from its source: {path}"
            );
            let metadata = edge.metadata.context("Clear source evidence unavailable")?;
            ensure!(
                metadata["browser_storage_operation"] == "clear"
                    && metadata["browser_storage_store"] == store,
                "Clear edge does not carry the expected store/operation: {path}"
            );
            let start = number(&metadata, "state_access_byte")?;
            let end = number(&metadata, "state_access_end_byte")?;
            let excerpt = source
                .get(start..end)
                .context("Clear source span exceeds selected UTF8 source")?;
            let shown = excerpt.chars().take(2048).collect::<String>();
            Ok(
                json!({"node_id":id,"name":node.name,"file":path,"line":number(&metadata,"state_access_line")?,
                "column":number(&metadata,"state_access_column")?,"byte_range":[start,end],"store":store,
                "queried_key":key,"scope":"all_keys_in_runtime_store_if_executed","operation":"clear",
                "excerpt":shown,"excerpt_truncated":shown.len()<excerpt.len(),"source_blake3":blake3::hash(source.as_bytes()).to_hex().to_string(),
                "physical_revision":node.generation}),
            )
        })();
        match result {
            Ok(row) => {
                paths.insert(row["file"].as_str().unwrap_or_default().to_owned());
                candidates.push(row);
            }
            Err(error) => gaps.push(format!("{id}: clear candidate withheld: {error:#}")),
        }
    }
    let policy = if paths.is_empty() {
        json!({"status":"not_requested","coverage":"no_returned_clear_sources","sources":[]})
    } else {
        serde_json::to_value(project_policy::resolve(
            root,
            &paths.into_iter().collect::<Vec<_>>(),
            &[],
        ))?
    };
    Ok(
        json!({"store":store,"queried_key":key,"status":"indexed_static_candidates",
        "candidates":candidates,"truncated":truncated,"max_candidates":limit,"gaps":gaps,
        "source_bytes_captured":bytes,"project_policy_sources":policy,"runtime_execution_proven":false,
        "interpretation":"Source-admitted clear() candidates affect all keys only in the runtime store where they execute. Project-wide matching does not establish shared origin, tab/session, script inclusion, execution order, actual values or built-in runtime binding. Relationships may retain one indexed access per containing symbol; no exhaustive access list or absence proof. Guidance scope covers returned clear sources only.",
        "refresh":{"tool":"update_project","arguments":{"project_id":project,"wait":true}}}),
    )
}

impl Engram {
    pub async fn handle_trace_state_usage_with_clear(
        &self,
        req: TraceStateUsageRequest,
    ) -> Result<CallToolResult, McpError> {
        if !matches!(req.state_type.as_str(), "LocalStorage" | "SessionStorage") {
            return self.handle_trace_state_usage(req).await;
        }
        let project = req.project_id.clone();
        let store = req.state_type.clone();
        let key = req.state_key.clone();
        let limit = req.sanitized_limit();
        let mut original = self.handle_trace_state_usage(req).await?;
        if original.is_error == Some(true) {
            return Ok(original);
        }
        let record = self.ensure_project_record(&project).await?;
        let runtime = self.ensure_project_runtime(&project).await?;
        let (_, view) = self
            .pin_active_source_generation(&project, &runtime.search)
            .await?;
        let graph = self.state.graph.clone();
        let reservation = match engram_core::memory::AllocationGuard::try_new(
            self.state.memory_budget.as_ref(),
            32 * 1024 * 1024,
            engram_core::memory::Subsystem::Graph,
            "browser store-wide clear source capture",
        ) {
            Ok(reservation) => reservation,
            Err(error) => {
                let report = json!({"status":"unavailable","candidates":[],"gaps":[error.to_string()],"runtime_execution_proven":false});
                original.content.push(Content::text(format!(
                    "\nstore_wide_mutation_candidates: {report}\n"
                )));
                return Ok(original);
            }
        };
        let report=tokio::task::spawn_blocking(move || {
            let _reservation=reservation;
            let result=(|| {
                let selected=view.selected_file_revisions().context("Managed selected source inventory unavailable; update_project first")?;
                collect(&graph,&project,Path::new(&record.directory),&store,&key,selected,limit)
            })();
            match result {
                Ok(value)=>value.to_string(),
                Err(error)=>json!({"status":"unavailable","candidates":[],"gaps":[format!("{error:#}")],"runtime_execution_proven":false}).to_string(),
            }
        }).await.map_err(|e|McpError::internal_error(e.to_string(),None))?;
        original.content.push(Content::text(format!("\n### Store-wide browser mutation candidates\nKey-specific readers/writers above do not exhaust store-wide effects.\nstore_wide_mutation_candidates: {report}\n")));
        Ok(original)
    }
}
