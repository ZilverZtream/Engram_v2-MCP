//! Presentation of the existing history ranking window; no retrieval changes.
use crate::handlers::search_tools::{response_budget, structured_response};
use crate::models::SearchHistoryRequest;
use rmcp::{ErrorData as McpError, model::{CallToolResult, Content}};
use serde_json::{Value, json};

pub(super) fn validate(req: &SearchHistoryRequest) -> Result<(), McpError> {
    response_budget::token_bytes(req.max_tokens)?;
    if req.offset > crate::models::MAX_SEARCH_RESULTS {
        return Err(McpError::invalid_params("offset must be in 0..=200 within the selected limit window", None));
    }
    Ok(())
}

/// Literal argument arrays, never shell strings. Object identity survives HEAD
/// changes; recovery requires the same repository and retained Git object.
pub(super) fn recovery(project: &str, commit: Option<&str>, doc_id: Option<&str>) -> Value {
    let oid=commit.filter(|oid|oid.len()==40 && oid.bytes().all(|c|c.is_ascii_hexdigit()));
    json!({
        "citation_chunk":doc_id.map(|id|json!({"tool":"get_chunk","arguments":{
            "project_id":project,"namespace":"history","doc_id":id,"citation":{}}})),
        "git_message":oid.map(|oid|json!({"executable":"git","cwd":"project repository",
            "arguments":["show","--no-ext-diff","--no-textconv","--no-patch","--format=%B",oid,"--"]})),
        "git_changed_files":oid.map(|oid|json!({"executable":"git","cwd":"project repository",
            "arguments":["show","--no-ext-diff","--no-textconv","--format=","--name-only","--first-parent","-z",oid,"--"]})),
        "scope":"Citation recovers the exact indexed document, not necessarily a complete original message or diff. Follow its hash-bound continuations. Git actions recover the named commit's full message and first-parent changed files (including generated files), not just query-matched paths. They require the project's local Git object; null means unavailable. No current-policy or applicability guarantee."
    })
}

pub(super) fn append_recovery(out: &mut String, recovery: &Value, truncated: bool) {
    if truncated {
        out.push_str("content_or_files_omitted: true; indexed/card coverage may be partial.\n");
    }
    out.push_str(&format!("recovery: {recovery}\n"));
}

pub(super) fn finish(req: &SearchHistoryRequest, header: String, blocks: &[String], rows: &[Value], mut metadata: Value) -> Result<CallToolResult, McpError> {
    debug_assert_eq!(blocks.len(),rows.len());
    let window=rows.len();
    let offset=req.offset.min(window);
    let rows=&rows[offset..];
    let blocks=&blocks[offset..];
    // Precedent ranking retains the complete ranked candidate list before the
    // caller's limit. Text retrieval only establishes a bounded document pool.
    let ranked_total=metadata["ranked_matches_before_limit"].as_u64();
    let more_ranked=ranked_total.map(|total|total>window as u64);
    metadata["more_ranked_matches"]=json!(more_ranked);
    if rows.is_empty() {
        // Own the empty-page guidance in both formats so middleware cannot
        // suggest inspecting a historical lead that this page did not supply.
        metadata["next_tools"]=json!([]);
        metadata["next_tools_status"]=json!("empty_history_page_no_historical_lead");
        metadata["empty_page_reason"]=json!(if window==0 {"no_results_in_ranking_window"} else {"offset_outside_ranking_window"});
        if window>0 {metadata["restart_offset"]=json!(0);}
    }
    let output=if req.output_json {
        metadata["schema_version"]=json!(1);
        metadata["tool"]=json!("search_history");
        metadata["project_id"]=json!(req.project_id);
        metadata["query"]=json!(req.query);
        metadata["status"]=json!("ok");
        metadata["requested_offset"]=json!(req.offset);
        metadata["coverage"]=json!("Selected ranking window only. Text matched-file lists are drawn from retrieved documents, not the complete commit. Precedent descriptions and non-generated file lists are capped at 1500 characters and 40 paths before previews. Git recovery includes generated paths. No corpus completeness or snapshot guarantee; LLM reranking and mutable inputs can reorder pages.");
        structured_response::finish(metadata,rows,structured_response::Page{
            offset:req.offset,window_limit:req.sanitized_limit(),total:Some(window)},req.max_tokens)?.0
    } else {
        let pagination=|returned| {
            let next=offset+returned;
            let action=if next<window {format!("next_page: offset={next}; keep query, filters, limit and ranking options fixed.\n")}
                else if window==0 {"ranking_window_empty: no results returned for this query and filters; no next page in this window. This does not prove absence from all history.\n".to_string()}
                else if req.offset>=window {format!("ranking_window_end: requested offset={} is outside this {window}-result window; restart at offset=0 to read its results.\n",req.offset)}
                else if more_ranked==Some(false) {"ranking_window_end: no additional ranked matches beyond this window in the selected history scope.\n".to_string()}
                else if more_ranked==Some(true) {format!("ranking_window_end: {} additional ranked matches are outside this window; adjust query/filters or increase limit if below 200, then restart at offset=0.\n",ranked_total.unwrap()-window as u64)}
                else {"ranking_window_end: matches beyond the retrieved window are unknown; adjust query/filters or increase limit if below 200, then restart at offset=0.\n".to_string()};
            format!("{action}ranking_window: {window}; offset={}; limit={}; returned={returned}; pagination is not a snapshot; LLM reranking and mutable inputs can reorder results.\n",req.offset,req.sanitized_limit())
        };
        if blocks.is_empty() {
            response_budget::bounded_message(format!("{header}{}\nnext_tools: []\nnext_tools_status: empty_history_page_no_historical_lead\n",pagination(0)),req.max_tokens)?
        } else if req.max_tokens.is_some() || req.offset>0 {
            response_budget::finish_with(header,blocks,"","",false,req.max_tokens,pagination)?.0
        } else {
            // Preserve the existing default header/record layout.
            format!("{header}{}",blocks.concat()).trim_end().to_string()
        }
    };
    Ok(CallToolResult::success(vec![Content::text(output)]))
}
