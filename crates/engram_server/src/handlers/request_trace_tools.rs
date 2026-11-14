use crate::services::request_trace::{api, execution, routing};
use crate::{models::TraceUserRequestRequest, tools::Engram};
use rmcp::{
    ErrorData as McpError,
    model::{CallToolResult, Content},
};
use serde_json::json;
use std::{io::Read, path::Path};
#[path = "request_trace_delivery.rs"]
mod delivery;
#[path = "request_trace_policy.rs"]
mod policy;

impl Engram {
    pub async fn handle_trace_user_request(
        &self,
        req: TraceUserRequestRequest,
    ) -> Result<CallToolResult, McpError> {
        super::validate_project_id(&req.project_id)?;
        let budget = delivery::budget(req.max_tokens)?;
        if let Some(id) = req.response_id.as_deref() {
            let body = super::planning_tools::change_set_delivery::recover_request_trace(
                &self.state.cfg.data_dir, &req.project_id, id, req.response_offset, budget)?;
            return Ok(CallToolResult::success(vec![Content::text(body)]));
        }
        if req.response_offset != 0 {
            return Err(McpError::invalid_params("response_offset requires response_id from trace recovery", None));
        }
        // Validate all caller controls before opening project stores.
        routing::resolve(&req.entry_point, &[])
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        if req.action.as_ref().is_some_and(|s| {
            s.trim().is_empty() || s.len() > 256 || s.chars().any(char::is_control)
        }) {
            return Err(McpError::invalid_params(
                "action must be nonempty, at most 256 bytes, without control characters",
                None,
            ));
        }
        let record = self.ensure_project_record(&req.project_id).await?;
        let runtime = self.ensure_project_runtime(&req.project_id).await?;
        let (generation, view) = self
            .pin_active_source_generation(&req.project_id, &runtime.search)
            .await?;
        let graph = self.state.graph.clone();
        // Cover the bounded 64 MiB source inventory, one tokenized source file,
        // graph/JSON assembly and response encoding before allocating them.
        let reservation = engram_core::memory::AllocationGuard::try_new(
            self.state.memory_budget.as_ref(),
            256 * 1024 * 1024,
            engram_core::memory::Subsystem::Graph,
            "request trace source and graph evidence",
        )
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let project_id = req.project_id.clone();
        let output = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
            let _reservation = reservation;
            let selected = view.selected_file_revisions().ok_or_else(|| anyhow::anyhow!("Request tracing requires a managed source inventory; update_project first"))?;
            anyhow::ensure!(selected.len() <= 50_000, "Selected inventory exceeds request tracing limit of 50000 paths");
            let mut owned = Vec::new();
            let mut gaps = Vec::new();
            let mut bytes = 0usize;
            for path in selected.keys() {
                let lower = path.to_ascii_lowercase();
                if ![".vb", ".cs", ".aspx", ".ashx", ".asmx", ".asax", ".sql", ".dbml"].iter().any(|extension| lower.ends_with(extension)) { continue; }
                let target = match engram_core::safe_join(Path::new(&record.directory), path) {
                    Ok(target) => target, Err(error) => { gaps.push(format!("{path}: {error}")); continue; }
                };
                let source = (|| -> anyhow::Result<String> {
                    let mut file = std::fs::File::open(&target)?.take(2 * 1024 * 1024 + 1);
                    let mut content = String::new(); file.read_to_string(&mut content)?;
                    anyhow::ensure!(content.len() <= 2 * 1024 * 1024, "File exceeds 2 MiB source limit");
                    Ok(content)
                })();
                match source {
                    Ok(content) => {
                        bytes += content.len();
                        anyhow::ensure!(bytes <= 64 * 1024 * 1024, "Request trace source scan exceeds 64 MiB; no complete trace produced");
                        owned.push((path.clone(), content));
                    }
                    Err(error) => gaps.push(format!("{path}: {error}")),
                }
            }
            let sources: Vec<_> = owned.iter().map(|(path, content)| routing::Source { path, content }).collect();
            let routing = routing::resolve(&req.entry_point, &sources)?;
            let mut pages = Vec::new();
            // Duplicate registrations remain visible in routing, but tracing the
            // same page twice must not imply two executions of its handlers.
            let mut seen = std::collections::BTreeSet::new();
            for route in &routing.candidates {
                if !seen.insert(route.page.clone()) { continue; }
                if pages.len() >= 8 { gaps.push("Page expansion truncated at 8 distinct candidates".into()); break; }
                pages.push(execution::trace_page(&graph, &req.project_id, route, req.action.as_deref(), req.include_db, req.include_triggers, &sources, selected)?);
            }
            let mut api_traces = Vec::new();
            for route in routing.api_candidates.iter().take(8) {
                api_traces.push(api::trace(&graph, &req.project_id, route, routing.method.as_deref(), req.action.as_deref(), req.include_db, req.include_triggers, &sources, selected)?);
            }
            if routing.api_candidates.len() > 8 { gaps.push("API route expansion truncated at eight candidates".into()); }
            // The graph is mutable even while search membership is pinned. Each
            // expansion checks physical revision/hash; this is not an atomic graph snapshot.
            let mut report = json!({"status":"static_candidates","project_id":req.project_id,"active_generation":generation,
                "entry_point":req.entry_point,"action":req.action,"routing":routing,"pages":pages,"api_traces":api_traces,
                "coverage":{"source_files_read":sources.len(),"source_bytes_read":bytes,"source_inventory":"selected managed paths; current disk contents","gaps":gaps,
                "graph_consistency":"Per-expanded-member source hash and selected physical revision checks; no atomic graph/disk snapshot or complete call-binding proof.",
                "runtime_execution_proven":false,"include_db":req.include_db,"include_triggers":req.include_triggers}});
            let loaded_paths = sources.iter().map(|source| source.path).collect();
            policy::attach(Path::new(&record.directory), &mut report, &loaded_paths)?;
            Ok(serde_json::to_string(&report)?)
        }).await.map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let output = delivery::finish(&self.state.cfg.data_dir, &project_id, output, budget)?;
        Ok(CallToolResult::success(vec![Content::text(output)]))
    }
}
