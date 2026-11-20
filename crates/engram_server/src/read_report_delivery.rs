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
    "produce_claude_md",
    "generate_agent_integration",
    "refresh_corpora",
    "update_project",
    "index_project",
    "repair_project",
    "check_integrity",
    "index_git_history",
    "ingest_zip_history",
    "ingest_quality_gates",
    "ingest_review_verdicts",
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
    "validate_feature_contract",
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

fn project_free(tool: &str) -> bool {
    server_discovery(tool) || matches!(tool, "index_project" | "validate_feature_contract")
}

fn returned_operation(tool: &str) -> bool {
    generation_instruction(tool).is_some()
        || matches!(tool, "update_project" | "index_project" | "refresh_corpora")
        || maintenance_operation(tool)
}

fn maintenance_operation(tool: &str) -> bool {
    matches!(tool, "repair_project" | "check_integrity" | "index_git_history" | "ingest_zip_history" | "ingest_quality_gates" | "ingest_review_verdicts")
}

const MAINTENANCE_DELIVERY_INSTRUCTION: &str = "Recover the original returned outcome; do not repeat this repair, history import or ownership operation to recover its response. A queued acknowledgement is not completion; poll its returned job_id separately. Delivery does not establish rollback, current freshness or policy approval.";

fn maintenance_outcome(tool: &str) -> Value {
    let instruction = if tool == "check_integrity" {
        "Recover the original integrity findings and any repair outcomes. Recovery never repeats the check or repair. The captured report describes the original observation."
    } else {
        MAINTENANCE_DELIVERY_INSTRUCTION
    };
    json!({"handler_returned":true,"instruction":instruction})
}

/// Save a job's separate instruction observation under the existing global job
/// recovery contract. Only this bounded reference belongs in the job row.
pub(crate) fn save_job_instruction_observation(
    data: &Path, job_id: &str, observation: Value,
) -> Result<String, McpError> {
    let report = Report {
        tool: "get_job_status", project: "server-discovery".into(), budget: 40960,
        response_id: None, offset: 0, capture_window: false,
        json_envelope: false, project_free: true,
    }.checked()?;
    let captured = CallToolResult::success(vec![Content::text(json!({
        "job_id":job_id,"observation":observation,
        "interpretation":"Separate worktree instruction observation after this source job; not an atomic index/policy snapshot, current freshness proof or policy approval. Recovery never reruns the update or writes authored guidance."
    }).to_string())]);
    let body = serde_json::to_string(&captured)
        .map_err(|error| McpError::internal_error(error.to_string(), None))?;
    let id = snapshots::save_read_report(data, &report.project, &body, report.tool)?;
    Ok(report.manifest_for(&id, body.len(), None))
}

const GENERATION_DELIVERY_INSTRUCTION: &str = "Do not repeat produce_claude_md to recover this response. Recover the original root, companion files and complete rule ledger, including publication notes and conflicts. A returned proposal does not establish that files were written or policy was accepted. Recovery never generates or publishes files. If recovery expires, inspect existing files and ownership receipts before deciding on another operation.";

const DRAFT_FEATURE_STATUS: &str = "DRAFT_REQUIRES_SOURCE_AND_REQUIREMENT_REVIEW";

fn feature_plan_summary(original: &CallToolResult) -> Option<Value> {
    let text = &original.content.first()?.as_text()?.text;
    let report: Value = serde_json::from_str(text).ok()?;
    let status = report["status"].as_str()?;
    if !matches!(status, "SOURCE_DOSSIER_READY" | "INCOMPLETE" | DRAFT_FEATURE_STATUS) {
        return None;
    }
    Some(json!({
        "status":status,
        "active_generation":report["active_generation"].as_u64(),
        "provider_calls":report["provenance"]["provider_calls"].as_u64()
    }))
}

fn generation_instruction(tool: &str) -> Option<&'static str> {
    match tool {
        "produce_claude_md" => Some(GENERATION_DELIVERY_INSTRUCTION),
        "generate_agent_integration" => Some("Do not repeat generate_agent_integration to recover this response. Recover the original workflow, hooks, AGENTS.md and MCP proposals, including written/skipped ownership notes. A proposal is not publication or policy approval. Recovery never generates or writes files. If recovery expires, inspect existing files and ownership receipts before deciding on another operation."),
        _ => None,
    }
}

const INDEX_DELIVERY_INSTRUCTION: &str = "Do not repeat index_project to recover this response. The original acknowledgement is in the saved result. A queued job acknowledgement does not establish job completion; poll get_job_status for its job_id. If recovery expires, reconcile list_projects/project_info and any returned job_id before deciding on a new operation. Delivery does not imply rollback.";
const UPDATE_DELIVERY_INSTRUCTION: &str = "Do not repeat update_project to recover this response. Recover the original acknowledgement before acting on its outcome. Poll the returned job before assuming completion. Delivery does not establish rollback or current source freshness.";

fn add_job_poll(operation: &mut Value, job_id: &str) {
    operation["job_id"] = json!(job_id);
    operation["poll_job"] = json!({
        "tool": "get_job_status",
        "arguments": {"job_id": job_id, "output_json": true},
    });
}

fn update_operation(job: Option<&str>) -> Value {
    let mut operation = json!({
        "handler_returned": true,
        "outcome": "Original acknowledgement and any separate instruction observation are in the saved result.",
        "instruction": UPDATE_DELIVERY_INSTRUCTION,
    });
    if let Some(job_id) = job {
        operation["acknowledgement"] = json!("update_job_started");
        add_job_poll(&mut operation, job_id);
    }
    operation
}

fn update_job_id(original: &CallToolResult) -> Option<&str> {
    let mut lines = original.content.first()?.as_text()?.text.lines();
    if lines.next()? != "\u{1f7e1} Update job started." {
        return None;
    }
    let job = lines.next()?.strip_prefix("job_id: ")?;
    uuid::Uuid::parse_str(job).ok().map(|_| job)
}

// Read identities only from the handler's acknowledgement header, never from
// report warnings, a project name, a path or a later content part.
fn index_operation(original: &CallToolResult) -> Value {
    let mut value = json!({"handler_returned":true,
        "acknowledgement":"unrecognized", "instruction":INDEX_DELIVERY_INSTRUCTION});
    if original.is_error == Some(true) { return value; }
    let Some(text) = original.content.first().and_then(|part| part.as_text()) else { return value; };
    let mut lines = text.text.lines();
    let first = lines.next().unwrap_or("");
    let (kind, project, job) = if let Some(project) = first.strip_prefix("\u{2705} Indexed project_id: ") {
        ("indexed", Some(project), None)
    } else if first == "Existing project registration." {
        ("existing_registration", lines.next().and_then(|line| line.strip_prefix("project_id: ")), None)
    } else if first == "\u{1f7e1} Index job started." {
        let job = lines.next().and_then(|line| line.strip_prefix("job_id: "));
        let project = lines.next().and_then(|line| line.strip_prefix("project_id: "));
        ("index_job_started", project, job)
    } else { return value; };
    if let Some(project) = project.filter(|id| uuid::Uuid::parse_str(id).is_ok()) {
        value["acknowledgement"] = json!(kind);
        value["project_id"] = json!(project);
        if let Some(job) = job.filter(|id| uuid::Uuid::parse_str(id).is_ok()) {
            add_job_poll(&mut value, job);
        }
    }
    value
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
    if project_free(name) {
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
    let global = project_free(tool.name.as_ref());
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
    let recovery = if global { " Project-free recovery." } else { "" };
    let error_scope = if tool.name == "refresh_corpora" { "Stage-error reports retain isError and also support bounded delivery; non-text results remain unchanged." }
        else if maintenance_operation(tool.name.as_ref()) { "Returned error reports retain isError and support bounded delivery; non-text results remain unchanged." }
        else { "Errors/non-text unchanged." };
    tool.description = Some(format!("{description} 40KiB default reply (max_tokens). Reassemble the immutable original MCP result via response_delivery.next_call; no rerun or fresh-call fields needed.{recovery} {error_scope}").into());
    if generation_instruction(tool.name.as_ref()).is_some() {
        tool.description = Some(format!("{} Recovery never generates or publishes. Inspect saved write/conflict notes; a returned proposal is not publication. Retention failure preserves the original outcome with an explicit budget-overrun warning.", tool.description.as_deref().unwrap_or("")).into());
    }
    if tool.name == "refresh_corpora" {
        tool.description = Some(format!("{} Refresh stage-error reports also honor this budget and retain aggregate isError on the initial response. Recovery does not repeat any refresh stage. Retention failure preserves the original outcome and explicitly reports a budget overrun; it does not roll back earlier writes.", tool.description.as_deref().unwrap_or("" )).into());
    }
    if tool.name == "update_project" {
        tool.description = Some(format!("{} Delivery recovery never repeats the update. A returned operation is not proof of background-job completion; recover the original acknowledgement and poll its job if queued. If retaining an oversized result fails after the operation, the original outcome is returned with an explicit warning that its delivery budget was exceeded.", tool.description.as_deref().unwrap_or("")).into());
    }
    if tool.name == "index_project" {
        tool.description = Some(format!("{} Delivery recovery never repeats indexing or registration and requires no directory/project_name. JSON and deferred results expose returned project/job IDs when present. Queued acknowledgements are not completed jobs. If result retention fails after indexing, the complete original outcome is returned with an explicit budget warning.", tool.description.as_deref().unwrap_or("")).into());
    }
    if tool.name == "check_integrity" {
        tool.description = Some(format!("{} Recovery never repeats integrity observation or repair. Retention failure preserves the original returned outcome with a budget warning.", tool.description.as_deref().unwrap_or("")).into());
    } else if maintenance_operation(tool.name.as_ref()) {
        tool.description = Some(format!("{} Recovery never repeats this operation; queued acknowledgements still require job polling.", tool.description.as_deref().unwrap_or("")).into());
    }
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
    project_free: bool,
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
    let global = project_free(tool);
    // Handler-owned JSON retains its schema, arguments and original result.
    let owns_json = definition.input_schema.get("properties")
        .and_then(|properties| properties.get("output_json")).is_some();
    // Job status adds typed fields to the original result inside the existing envelope.
    let wrap_json = !owns_json || tool == "get_job_status";
    let empty = serde_json::Map::new();
    let args = request.arguments.as_ref().or(global.then_some(&empty)).ok_or_else(|| {
        McpError::invalid_params(
            "project_id and fresh-analysis fields or response_id are required",
            None,
        )
    })?;
    let controls: Controls = serde_json::from_value(json!({
        "max_tokens":args.get("max_tokens"),"response_id":args.get("response_id"),"response_offset":args.get("response_offset"),
        "output_json":if wrap_json {args.get("output_json")} else {None}
    })).map_err(|error| McpError::invalid_params(format!("Invalid read-report delivery controls: {error}"), None))?;
    let project = if global {
        if tool == "index_project" { "server-index-operation" } else { "server-discovery" }.to_owned()
    } else { args
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
        project_free: global,
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
        Self { tool, project, budget, response_id, offset, capture_window: false, json_envelope: false, project_free: false }.checked()
    }

    pub(crate) fn retained_window(
        tool: &'static str, project: String, budget: usize,
        response_id: Option<String>, offset: usize,
    ) -> Result<Self, McpError> {
        Self { tool, project, budget, response_id, offset, capture_window: true, json_envelope: false, project_free: false }.checked()
    }

    fn checked(self) -> Result<Self, McpError> {
        crate::handlers::validate_project_id(&self.project)?;
        if self.response_id.is_none() && self.offset != 0 {
            return Err(McpError::invalid_params(
                "response_offset requires response_id from read-report recovery", None));
        }
        let minimum = self.manifest(&"0".repeat(64), 8 * 1024 * 1024).len();
        if self.budget < minimum {
            let action = if returned_operation(self.tool) { "Operation" } else { "Analysis" };
            return Err(McpError::invalid_params(format!(
                "max_tokens is too small for read-report identity and recovery metadata; increase to at least {} estimated tokens. {action} was not run.",
                minimum.div_ceil(4)), None));
        }
        Ok(self)
    }

    fn manifest(&self, id: &str, bytes: usize) -> String {
        if self.tool == "one_shot_feature_plan" {
            let probe = json!({
                "status":DRAFT_FEATURE_STATUS,"active_generation":u64::MAX,"provider_calls":u64::MAX
            });
            return self.manifest_for(id, bytes, Some(&probe));
        }
        // Reserve both full-length IDs before any indexing is admitted. These
        // placeholders are only a sizing probe; actual replies use the header.
        const SIZING_ID: &str = "00000000-0000-0000-0000-000000000000";
        let probe = if self.tool == "update_project" {
            update_operation(Some(SIZING_ID))
        } else {
            let mut index = json!({"handler_returned":true,"acknowledgement":"existing_registration",
                "project_id":SIZING_ID,"instruction":INDEX_DELIVERY_INSTRUCTION});
            add_job_poll(&mut index, SIZING_ID);
            index
        };
        self.manifest_for(id, bytes, Some(&probe))
    }

    fn report_scope(&self) -> &'static str {
        match self.tool {
            "index_project" => "server_operation",
            "validate_feature_contract" => "contract_validation",
            _ => "server_discovery",
        }
    }

    fn manifest_for(&self, id: &str, bytes: usize, operation: Option<&Value>) -> String {
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
        if self.project_free {
            value.as_object_mut().unwrap().remove("project_id");
            value["report_scope"] = json!(self.report_scope());
            Self::discovery_action(&mut value["response_delivery"]["next_call"]);
        }
        if let Some(instruction) = generation_instruction(self.tool) {
            value["operation"] = json!({"handler_returned":true,
                "instruction":instruction});
        }
        if self.tool == "refresh_corpora" {
            value["operation"] = json!({"handler_returned":true,
                "instruction":"Do not repeat refresh_corpora to recover this report. Original per-stage results/errors are saved; earlier successful writes are not rolled back. A returned result is not proof of complete or fresh corpora."});
        }
        if matches!(self.tool, "index_project" | "update_project") && let Some(operation) = operation {
            value["operation"] = operation.clone();
        }
        if self.tool == "one_shot_feature_plan"
            && let Some(summary) = operation
        {
            value["plan_summary"] = summary.clone();
        }
        if maintenance_operation(self.tool) {
            value["operation"] = maintenance_outcome(self.tool);
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
                .map_err(|error| {
                    let unavailable = error.message.starts_with("Response snapshot unavailable;")
                        || error.message.starts_with("Response snapshot expired;");
                    if let (true, Some(instruction)) = (unavailable, generation_instruction(self.tool)) {
                        McpError::invalid_params(format!("Response snapshot unavailable. {instruction}"), None)
                    } else if returned_operation(self.tool) && unavailable {
                        McpError::invalid_params(format!("Response snapshot unavailable. Do not repeat {} solely to recover its response. Reconcile list_projects/project_info and any returned job_id with get_job_status before deciding on a new operation.", self.tool), None)
                    } else { error }
                })
                .and_then(|page| {
                    let page = if self.project_free {
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
        if !returned_operation(self.tool) {
            return self.finish_report(data, original);
        }
        // The operation has already returned. Failure to retain its report must
        // not replace its outcome with an MCP error that invites a retry.
        let mut fallback = original.clone();
        match self.finish_report(data, original) {
            Ok(delivered) => Ok(delivered),
            Err(error) => {
                tracing::warn!(%error, tool=self.tool, "result retention failed; preserving original operation outcome");
                if self.tool == "refresh_corpora" {
                    fallback.content.push(Content::text(json!({
                        "tool":self.tool,"project_id":self.project,
                        "response_delivery":{"status":"ORIGINAL_OUTCOME_PRESERVED_DELIVERY_FAILED",
                            "budget_honored":false,"response_budget_bytes":self.budget,
                            "recovery_available":false,"reason":error.message,
                            "instruction":"Refresh stages already returned. Their complete original results/errors are retained above and exceed the requested delivery budget. Do not repeat refresh_corpora solely to recover this report; earlier successful writes are not rolled back."}
                    }).to_string()));
                    return Ok(fallback);
                }
                if self.tool == "index_project" {
                    fallback.content.push(Content::text(json!({
                        "tool":self.tool,"report_scope":"server_operation",
                        "operation":index_operation(&fallback),
                        "response_delivery":{
                            "status":"ORIGINAL_OUTCOME_PRESERVED_DELIVERY_FAILED",
                            "budget_honored":false,"response_budget_bytes":self.budget,
                            "recovery_available":false,"reason":error.message,
                            "instruction":"The index handler already returned. Its complete original outcome is retained above and exceeds the requested delivery budget. Do not repeat index_project to recover its result. A queued job still requires get_job_status; this delivery failure does not imply rollback."
                        }
                    }).to_string()));
                    return Ok(fallback);
                }
                if let Some(instruction) = generation_instruction(self.tool) {
                    fallback.content.push(Content::text(json!({
                        "tool":self.tool,"project_id":self.project,
                        "response_delivery":{
                            "status":"ORIGINAL_OUTCOME_PRESERVED_DELIVERY_FAILED",
                            "budget_honored":false,"response_budget_bytes":self.budget,
                            "recovery_available":false,"reason":error.message,
                            "instruction":instruction
                        }
                    }).to_string()));
                    return Ok(fallback);
                }
                if maintenance_operation(self.tool) {
                    fallback.content.push(Content::text(json!({
                        "tool":self.tool,"project_id":self.project,
                        "operation":maintenance_outcome(self.tool),
                        "response_delivery":{
                            "status":"ORIGINAL_OUTCOME_PRESERVED_DELIVERY_FAILED",
                            "budget_honored":false,"response_budget_bytes":self.budget,
                            "recovery_available":false,"reason":error.message,
                            "instruction":"The operation already returned. Its complete original outcome remains above and exceeds this delivery budget. Do not repeat the operation to recover the report; delivery failure does not imply rollback."
                        }
                    }).to_string()));
                    return Ok(fallback);
                }
                fallback.content.push(Content::text(json!({
                    "tool":"update_project","project_id":self.project,
                    "operation":update_operation(update_job_id(&fallback)),
                    "response_delivery":{
                        "status":"ORIGINAL_OUTCOME_PRESERVED_DELIVERY_FAILED",
                        "budget_honored":false,"response_budget_bytes":self.budget,
                        "recovery_available":false,
                        "reason":error.message,
                        "instruction":"The update handler already returned. Its complete original outcome is retained above and exceeds the requested delivery budget. Do not repeat the update to recover its result. A queued job still requires get_job_status; this delivery failure does not imply rollback."
                    }
                }).to_string()));
                Ok(fallback)
            }
        }
    }

    fn finish_report(
        &self,
        data: &Path,
        original: CallToolResult,
    ) -> Result<CallToolResult, McpError> {
        if (original.is_error == Some(true) && self.tool != "refresh_corpora" && !maintenance_operation(self.tool))
            || original
                .content
                .iter()
                .any(|content| content.as_text().is_none())
        {
            return Ok(original);
        }
        let stage_error = (self.tool == "refresh_corpora" || maintenance_operation(self.tool)) && original.is_error == Some(true);
        let operation = if self.tool == "index_project" { Some(index_operation(&original)) }
            else if self.tool == "update_project" { Some(update_operation(update_job_id(&original))) }
            else if maintenance_operation(self.tool) { Some(maintenance_outcome(self.tool)) }
            else { None };
        let plan_summary = if self.tool == "one_shot_feature_plan" {
            feature_plan_summary(&original)
        } else {
            None
        };
        let original = if self.json_envelope {
            let mut original = original;
            let job_status = if self.tool == "get_job_status" && original.content.len() > 1 {
                let part = original.content.remove(1);
                let text = part.as_text().ok_or_else(|| McpError::internal_error("Expected typed job status", None))?;
                Some(serde_json::from_str::<Value>(&text.text)
                    .map_err(|error| McpError::internal_error(error.to_string(), None))?)
            } else {
                None
            };
            let result = serde_json::to_value(&original)
                .map_err(|error| McpError::internal_error(error.to_string(), None))?;
            let mut envelope = json!({
                "schema_version":1,"tool":self.tool,"project_id":self.project,
                "result":result,
                "response_delivery":{"status":"COMPLETE","complete":true,"truncated":false,
                    "scope":"complete_original_selected_report",
                    "coverage":"Existing handler limits, omissions, warnings and freshness remain in result. Complete delivery does not prove exhaustive source coverage or runtime correctness."}
            });
            if self.project_free {
                envelope.as_object_mut().unwrap().remove("project_id");
                envelope["report_scope"] = json!(self.report_scope());
            }
            if let Some(operation) = &operation { envelope["operation"] = operation.clone(); }
            if let Some(job_status) = job_status { envelope["job_status"] = job_status; }
            let mut delivered = CallToolResult::success(vec![Content::text(envelope.to_string())]);
            if stage_error { delivered.is_error = Some(true); }
            delivered
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
        let manifest = self.manifest_for(&id, body.len(), operation.as_ref().or(plan_summary.as_ref()));
        if manifest.len() > self.budget {
            return Err(McpError::invalid_params(
                "Increase max_tokens for complete read-report recovery metadata",
                None,
            ));
        }
        snapshots::save_read_report(data, &self.project, &body, self.tool)?;
        let mut delivered = CallToolResult::success(vec![Content::text(manifest)]);
        if stage_error { delivered.is_error = Some(true); }
        Ok(delivered)
    }
}
