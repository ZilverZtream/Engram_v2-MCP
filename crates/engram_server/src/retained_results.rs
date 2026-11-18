//! Default overflow and opt-in immutable delivery of bounded query windows.
//! Retrieval/filtering remains owned by the original tools; this only removes
//! reply clipping for capture and reuses the existing project/tool snapshot store.
use crate::read_report_delivery::Report;
use rmcp::{
    ErrorData as McpError,
    model::{CallToolRequestParam, Tool},
};
use serde::Deserialize;
use serde_json::{Value, json};

const TOOLS: &[&str] = &[
    "search_memory",
    "search_history",
    "grep_project",
    "vector_search",
];
const CONTROLS: &[&str] = &["retain_response", "response_id", "response_offset"];

fn eligible(tool: &Tool) -> Option<&'static str> {
    let name = TOOLS
        .iter()
        .copied()
        .find(|name| *name == tool.name.as_ref())?;
    let properties = tool.input_schema.get("properties")?.as_object()?;
    (properties.contains_key("project_id")
        && properties.contains_key("output_json")
        && !properties.contains_key("response_id"))
    .then_some(name)
}

pub fn advertise(mut tool: Tool) -> Tool {
    if eligible(&tool).is_none() {
        return tool;
    }
    let schema = std::sync::Arc::make_mut(&mut tool.input_schema);
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        properties.insert("retain_response".into(), json!({"type":"boolean","default":false,
            "description":"Opt in with output_json=true to retain the complete original requested result window before max_tokens reply clipping. Returns immutable response_delivery.next_call even when the selected window fits one reply."}));
        properties.insert("response_id".into(), json!({"type":"string",
            "description":"Recover this tool/project's retained original MCP result without rerunning retrieval. Needs only project_id, response_id, response_offset and optional max_tokens; no query required."}));
        properties.insert("response_offset".into(), json!({"type":"integer","minimum":0,"default":0,
            "description":"Saved result UTF-8 byte offset from response_delivery.next_call; requires response_id when nonzero. This differs from the original live-result offset."}));
    }
    schema.insert("required".into(), json!(["project_id"]));
    let description = tool.description.as_deref().unwrap_or("");
    tool.description = Some(format!("{description} Retained result window: retain_response=true plus output_json=true captures the requested window before reply clipping. Follow response_delivery.next_call with project_id and returned controls; fresh-query fields are required only for new calls. Recovery never reruns retrieval. max_tokens budgets the reply (default40KiB); captures are limited to8MiB and at most24hours or32newer captures. Reassemble and verify the original MCP result before use. Captured guidance/freshness is historical; source objects and results outside that window are not retained.").into());
    let description = tool.description.as_deref().unwrap_or("");
    tool.description = Some(format!("{description} Ordinary query replies also default to40KiB when max_tokens is omitted: oversized JSON or Markdown results use exact saved recovery; fitting replies stay unchanged. Explicit max_tokens retains whole-record paging.").into());
    tool
}

#[derive(Default, Deserialize)]
struct Controls {
    #[serde(default)]
    retain_response: bool,
    response_id: Option<String>,
    response_offset: Option<usize>,
    max_tokens: Option<usize>,
}

pub fn prepare(
    request: &mut CallToolRequestParam,
    registry: &[Tool],
) -> Result<Option<Report>, McpError> {
    let Some(tool) = registry
        .iter()
        .find(|tool| tool.name == request.name)
        .and_then(eligible)
    else {
        return Ok(None);
    };
    let Some(args) = request.arguments.as_mut() else {
        return Ok(None);
    };
    let controls: Controls = serde_json::from_value(json!({
        "retain_response":args.get("retain_response").cloned().unwrap_or(json!(false)),
        "response_id":args.get("response_id"), "response_offset":args.get("response_offset"),
        "max_tokens":args.get("max_tokens")
    }))
    .map_err(|error| {
        McpError::invalid_params(format!("Invalid retained-result controls: {error}"), None)
    })?;
    if !controls.retain_response && controls.response_id.is_none() {
        if controls.response_offset.unwrap_or(0) != 0 {
            return Err(McpError::invalid_params(
                "response_offset requires response_id",
                None,
            ));
        }
        for name in CONTROLS {
            args.remove(*name);
        }
        // Keep the selected original result when no caller budget is supplied.
        // Bound ordinary public query replies without clipping/repeating retrieval.
        if controls.max_tokens.is_none() {
            let project = args.get("project_id").and_then(Value::as_str)
                .ok_or_else(|| McpError::invalid_params("project_id is required for query delivery", None))?;
            return Report::handler_owned(tool, project.to_owned(), 40960, None, 0).map(Some);
        }
        return Ok(None);
    }
    if controls.response_id.is_none()
        && args.get("output_json").and_then(Value::as_bool) != Some(true)
    {
        return Err(McpError::invalid_params(
            "retain_response=true requires output_json=true to capture the complete requested result window. Analysis was not run.",
            None,
        ));
    }
    let project = args
        .get("project_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            McpError::invalid_params("project_id is required for retained results", None)
        })?
        .to_owned();
    let budget = crate::handlers::search_tools::response_budget::token_bytes(controls.max_tokens)?
        .unwrap_or(40960);
    let report = Report::retained_window(
        tool,
        project,
        budget,
        controls.response_id,
        controls.response_offset.unwrap_or(0),
    )?;
    for name in CONTROLS {
        args.remove(*name);
    }
    // Materialize the selected original window, not a budget-clipped first page.
    // All retrieval limits, source checks, original offsets and filters survive.
    args.remove("max_tokens");
    Ok(Some(report))
}
