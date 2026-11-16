//! Complete delivery for selected reports without a handler-owned budget.
//! Fresh analysis retains its existing effects; saved recovery never repeats them.
use crate::handlers::planning_tools::change_set_delivery as snapshots;
use rmcp::{
    ErrorData as McpError,
    model::{CallToolRequestParam, CallToolResult, Content, Tool},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;

const TOOLS: &[&str] = &[
    "list_projects",
    "list_advanced_tools",
    "project_info",
    "list_jobs",
    "get_job_status",
    "derive_test_matrix",
    "query_business_logic",
    "describe_table",
    "describe_setting",
    "validate_sql_fragment",
    "evaluate_safety",
    "autonomous_decision_gate",
    "get_index_freshness",
    "detect_design_patterns",
    "find_dependency_cycles",
    "ast_dependency_graph",
    "graph_centrality_rerank",
    "reconcile_runtime_evidence",
    "validate_generated_code",
    "anti_pattern_guard",
    "plan_user_story",
    "find_connection_path",
    "produce_support_kb",
    "query_graph_nodes",
    "find_references",
    "graph_search",
    "analyze_reverts",
    "list_repo_rules",
    "trace_state_usage",
    "analyze_business_logic",
    "explain_change",
    "get_method_edit_context",
    "traverse_graph",
    "list_memory_bank",
    "pre_push_audit",
    "project_health",
    "one_shot_feature_plan",
    "map_guards_and_settings",
    "immune_check",
    "get_review_decisions",
    "get_review_precedents",
    "get_table_schema",
    "list_triggers",
    "get_sp_details",
    "query_schema",
    "check_edit_safety",
    "find_symbol_references",
    "find_similar_changes",
    "find_merged_work",
    "resolve_id",
    "get_ui_conformance",
    "get_gis_inventory",
    "list_settings",
    "get_setting",
    "analyze_temporal_couplings",
    "analyze_error_stack",
    "analyze_file_coding_style",
    "get_extraction_confidence",
    "map_validation_controls",
    "map_auth_config",
    "map_page_lifecycle",
    "analyze_viewstate_deps",
    "map_ajax_regions",
    "trace_data_flow",
    "get_migration_dossier",
    "check_migration_coverage",
    "get_migration_progress",
    "analyze_full_project_migration",
    "analyze_sync_hazards",
    "get_jquery_inventory",
    "get_session_workflows",
    "get_vb_translation_traps",
    "find_tests_for_method",
    "find_dead_methods",
    "trace_ui_event",
    "trace_ui_action",
    "get_ui_blueprint",
];
const CONTROLS: &[&str] = &["max_tokens", "response_id", "response_offset"];

fn server_discovery(tool: &str) -> bool {
    matches!(tool, "list_projects" | "list_advanced_tools" | "list_jobs" | "get_job_status")
}

#[cfg(test)]
#[path = "read_report_delivery_tests.rs"]
mod tests;

/// Original handler controls always retain their own schemas and semantics.
pub fn eligible(tool: &Tool) -> Option<&'static str> {
    let name = TOOLS
        .iter()
        .copied()
        .find(|name| *name == tool.name.as_ref())?;
    let properties = tool.input_schema.get("properties").and_then(Value::as_object);
    if properties.is_some_and(|properties| CONTROLS.iter().any(|key| properties.contains_key(*key))) {
        return None;
    }
    if server_discovery(name) {
        return Some(name);
    }
    tool.input_schema
        .get("required")?
        .as_array()?
        .iter()
        .any(|key| key.as_str() == Some("project_id"))
        .then_some(name)
}

pub fn advertise(mut tool: Tool) -> Tool {
    if eligible(&tool).is_none() {
        return tool;
    }
    let global = server_discovery(tool.name.as_ref());
    let schema = std::sync::Arc::make_mut(&mut tool.input_schema);
    schema.entry("properties").or_insert_with(|| json!({}));
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        if !properties.contains_key("output_json") {
            properties.insert("output_json".into(), json!({"type":"boolean","default":false,
                "description":"Return a versioned JSON delivery envelope containing the complete original MCP result. Preserves every text part and existing coverage warnings. Does not make handler coverage exhaustive. Existing budgets and immutable recovery apply to the escaped envelope."}));
        }
        for name in required
            .iter()
            .filter_map(Value::as_str)
            .filter(|name| *name != "project_id")
        {
            if let Some(property) = properties.get_mut(name).and_then(Value::as_object_mut) {
                let original = property
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                property.insert("description".into(), json!(format!("{original} Required for a fresh analysis; omit only for response_id recovery.")));
            }
        }
        properties.insert("max_tokens".into(), json!({"type":"integer","minimum":1,
            "description":"Complete reply-text budget, estimated at4UTF-8bytes/token; default10240. Too-small recovery metadata budgets fail before analysis. Oversized reports return exact immutable recovery."}));
        properties.insert("response_id".into(), json!({"type":"string",
            "description":"Recover the saved original MCP result for this tool/project without rerunning analysis or generation. Follow response_delivery.next_call."}));
        properties.insert("response_offset".into(), json!({"type":"integer","minimum":0,"default":0,
            "description":"UTF-8 byte offset from the preceding recovery page; requires response_id when nonzero."}));
    }
    // Conditional required fields are stated in prose rather than root anyOf:
    // strict function-schema clients must also be able to invoke recovery.
    schema.insert("required".into(), if global { json!([]) } else { json!(["project_id"]) });
    let description = tool.description.as_deref().unwrap_or("");
    let recovery = if global { "returned delivery controls; no project_id is required" } else { "project_id and returned delivery controls" };
    tool.description = Some(format!("{description} Read-report delivery: max_tokens budgets complete reply text (default40KiB). Original analysis fields are required for fresh calls. Oversized results provide response_delivery.next_call; recovery needs only {recovery} and does not rerun the tool. Reconstruct the original MCP result before interpreting its evidence. Errors and non-text content retain their original delivery.").into());
    tool
}

#[derive(Default, Deserialize)]
struct Controls {
    max_tokens: Option<usize>,
    response_id: Option<String>,
    response_offset: Option<usize>,
    output_json: Option<bool>,
}

#[derive(Debug)]
pub struct Report {
    pub tool: &'static str,
    project: String,
    budget: usize,
    response_id: Option<String>,
    offset: usize,
    capture_window: bool,
    json_envelope: bool,
    server_discovery: bool,
}

pub fn prepare(
    request: &mut CallToolRequestParam,
    registry: &[Tool],
) -> Result<Option<Report>, McpError> {
    let Some(definition) = registry
        .iter()
        .find(|tool| tool.name == request.name)
    else {
        return Ok(None);
    };
    let Some(tool) = eligible(definition) else {
        return Ok(None);
    };
    let global = server_discovery(tool);
    // Handler-owned JSON retains its schema, arguments and original result.
    let owns_json = definition.input_schema.get("properties")
        .and_then(|properties| properties.get("output_json")).is_some();
    let empty = serde_json::Map::new();
    let args = request.arguments.as_ref().or(global.then_some(&empty)).ok_or_else(|| {
        McpError::invalid_params(
            "project_id and fresh-analysis fields or response_id are required",
            None,
        )
    })?;
    let controls: Controls = serde_json::from_value(json!({
        "max_tokens":args.get("max_tokens"),"response_id":args.get("response_id"),"response_offset":args.get("response_offset"),
        "output_json":if owns_json {None} else {args.get("output_json")}
    })).map_err(|error| McpError::invalid_params(format!("Invalid read-report delivery controls: {error}"), None))?;
    let project = if global { "server-discovery".to_owned() } else { args
        .get("project_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            McpError::invalid_params("project_id must be supplied for read-report delivery", None)
        })?
        .to_owned() };
    crate::handlers::validate_project_id(&project)?;
    let budget = crate::handlers::search_tools::response_budget::token_bytes(controls.max_tokens)?
        .unwrap_or(40960);
    let report = Report {
        tool,
        project,
        budget,
        response_id: controls.response_id,
        offset: controls.response_offset.unwrap_or(0),
        capture_window: false,
        json_envelope: controls.output_json.unwrap_or(false),
        server_discovery: global,
    }.checked()?;
    if let Some(args) = request.arguments.as_mut() {
        for key in CONTROLS {
            args.remove(*key);
        }
        if !owns_json {
            args.remove("output_json");
        }
    }
    Ok(Some(report))
}

impl Report {
    /// Reuse immutable complete-result recovery for a handler that already owns
    /// its schema and default budget. Transport-level eligibility stays unchanged.
    pub(crate) fn handler_owned(
        tool: &'static str, project: String, budget: usize,
        response_id: Option<String>, offset: usize,
    ) -> Result<Self, McpError> {
        Self { tool, project, budget, response_id, offset, capture_window: false, json_envelope: false, server_discovery: false }.checked()
    }

    pub(crate) fn retained_window(
        tool: &'static str, project: String, budget: usize,
        response_id: Option<String>, offset: usize,
    ) -> Result<Self, McpError> {
        Self { tool, project, budget, response_id, offset, capture_window: true, json_envelope: false, server_discovery: false }.checked()
    }

    fn checked(self) -> Result<Self, McpError> {
        crate::handlers::validate_project_id(&self.project)?;
        if self.response_id.is_none() && self.offset != 0 {
            return Err(McpError::invalid_params(
                "response_offset requires response_id from read-report recovery", None));
        }
        let minimum = self.manifest(&"0".repeat(64), 8 * 1024 * 1024).len();
        if self.budget < minimum {
            return Err(McpError::invalid_params(format!(
                "max_tokens is too small for read-report identity and recovery metadata; increase to at least {} estimated tokens. Analysis was not run.",
                minimum.div_ceil(4)), None));
        }
        Ok(self)
    }

    fn manifest(&self, id: &str, bytes: usize) -> String {
        let mut value = json!({"tool":self.tool,"project_id":self.project,"report_content":"DEFERRED_NOT_SUPPLIED",
            "response_delivery":{"status":"DEFERRED_READ_REPORT","complete":false,
                "response_id":id,"original_bytes":bytes,"original_format":"mcp_call_tool_result_json",
                "response_budget_bytes":self.budget,
                "estimate":"Complete reply-text UTF-8 bytes/4; not provider tokens or transport-envelope bytes",
                "retention":"At most24hours or32newer snapshots for this tool/project",
                "instruction":"Recover all pages, concatenate content by byte offset and verify response_id as BLAKE3. Parse the reconstructed original MCP CallToolResult, including its original text, metadata, guidance, warnings and status. No original report evidence is supplied here. Recovery does not rerun analysis/generation or establish current freshness.",
                "next_call":snapshots::read_report_continuation(self.tool,&self.project,id,0,self.budget)}});
        if self.capture_window {
            value["response_delivery"]["capture_scope"] = json!("Immutable original requested result window, captured before reply-budget clipping. Existing retrieval/window/content limits still apply. This retains the report, not underlying source objects or the whole corpus. Guidance and freshness describe capture time. Follow saved next_call pages; original offset actions outside this capture are fresh queries.");
        }
        if self.server_discovery {
            value.as_object_mut().unwrap().remove("project_id");
            value["report_scope"] = json!("server_discovery");
            Self::discovery_action(&mut value["response_delivery"]["next_call"]);
        }
        value.to_string()
    }

    fn discovery_action(action: &mut Value) {
        if let Some(arguments) = action.get_mut("arguments").and_then(Value::as_object_mut) {
            arguments.remove("project_id");
        }
    }

    pub fn recover(&self, data: &Path) -> Result<Option<CallToolResult>, McpError> {
        self.response_id
            .as_deref()
            .map(|id| {
                snapshots::recover_read_report(
                    data,
                    &self.project,
                    id,
                    self.offset,
                    self.budget,
                    self.tool,
                )
                .and_then(|page| {
                    let page = if self.server_discovery {
                        let mut value: Value = serde_json::from_str(&page)
                            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
                        // Removing a private storage binding cannot grow a page
                        // that the existing recovery reader already budgeted.
                        Self::discovery_action(&mut value["next_call"]);
                        value.to_string()
                    } else { page };
                    Ok(CallToolResult::success(vec![Content::text(page)]))
                })
            })
            .transpose()
    }

    pub fn finish(
        &self,
        data: &Path,
        original: CallToolResult,
    ) -> Result<CallToolResult, McpError> {
        if original.is_error == Some(true)
            || original
                .content
                .iter()
                .any(|content| content.as_text().is_none())
        {
            return Ok(original);
        }
        let original = if self.json_envelope {
            let result = serde_json::to_value(&original)
                .map_err(|error| McpError::internal_error(error.to_string(), None))?;
            let mut envelope = json!({
                "schema_version":1,"tool":self.tool,"project_id":self.project,
                "result":result,
                "response_delivery":{"status":"COMPLETE","complete":true,"truncated":false,
                    "scope":"complete_original_selected_report",
                    "coverage":"Existing handler limits, omissions, warnings and freshness remain in result. Complete delivery does not prove exhaustive source coverage or runtime correctness."}
            });
            if self.server_discovery {
                envelope.as_object_mut().unwrap().remove("project_id");
                envelope["report_scope"] = json!("server_discovery");
            }
            CallToolResult::success(vec![Content::text(envelope.to_string())])
        } else {
            original
        };
        let text_bytes: usize = original
            .content
            .iter()
            .filter_map(|content| content.as_text())
            .map(|text| text.text.len())
            .sum();
        let value = serde_json::to_value(&original)
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        let structured_bytes = value
            .get("structuredContent")
            .filter(|value| !value.is_null())
            .map(|value| value.to_string().len())
            .unwrap_or(0);
        if !self.capture_window && text_bytes.saturating_add(structured_bytes) <= self.budget {
            return Ok(original);
        }
        let body = serde_json::to_string(&original)
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        let id = blake3::hash(body.as_bytes()).to_hex().to_string();
        let manifest = self.manifest(&id, body.len());
        if manifest.len() > self.budget {
            return Err(McpError::invalid_params(
                "Increase max_tokens for complete read-report recovery metadata",
                None,
            ));
        }
        snapshots::save_read_report(data, &self.project, &body, self.tool)?;
        Ok(CallToolResult::success(vec![Content::text(manifest)]))
    }
}
