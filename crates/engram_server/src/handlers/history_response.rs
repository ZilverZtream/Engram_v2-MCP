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
        "complete_git_commit":oid.map(|oid|json!({"tool":"search_history","arguments":{
            "project_id":project,"query":oid,"mode":"commit","as_of_rev":oid,
            "output_json":true,"retain_response":true,"max_tokens":1024}})),
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

pub(super) fn recent(
    req: &SearchHistoryRequest,
    scope: &str,
    revision: git2::Oid,
    max_commits: usize,
    window: crate::services::change_card_service::MetadataWindow,
) -> Result<CallToolResult, McpError> {
    let limit = req.sanitized_limit();
    let matches = window.cards.len();
    let older = window.cards.iter().rev().nth(limit).map(|card| card.oid.clone())
        .or_else(|| window.next_revision.map(|oid| oid.to_string()));
    let next_window = older.map(|oid| -> Result<_, McpError> {
        let mut arguments = serde_json::to_value(req)
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        arguments["as_of_rev"] = json!(oid);
        arguments["offset"] = json!(0);
        Ok(json!({"tool":"search_history","arguments":arguments}))
    }).transpose()?;
    let mut blocks = Vec::new();
    let mut rows = Vec::new();
    for card in window.cards.iter().rev().take(limit) {
        let files = card.files.iter().take(12).collect::<Vec<_>>();
        let recovery = recovery(&req.project_id, Some(&card.oid), None);
        let date = crate::utils::ymd_utc(card.timestamp.saturating_mul(1000));
        blocks.push(format!("\ncommit: {}\ndate: {date}\nauthor: {}\nmessage: {}\nfiles: {}\nrecovery: {recovery}\n",
            card.oid, card.author, card.title, json!(files)));
        rows.push(json!({"commit":card.oid,"author":card.author,"timestamp":card.timestamp,
            "message":card.title,"files":files,"file_count":card.file_count,
            "files_truncated":card.file_count>files.len(),"recovery":recovery}));
    }
    let scan = json!({"max_commits":max_commits,"commits_scanned":window.commits_scanned,
        "older_history_not_scanned":window.next_revision.is_some()});
    let header = format!("Recent changes ({scope}), newest on the first-parent line first. No model calls.\n\
        File previews use capped non-generated change cards. Follow commit recovery for the full cohort.\n\
        History acquisition: {scan}\nnext_history_window: {}\n", json!(next_window));
    finish(req, header, &blocks, &rows, json!({"mode":"recent", "scope":scope,
        "resolved_revision":revision.to_string(),"history_scan":scan,"next_history_window":next_window,
        "semantic_retrieval":"not_run","rerank":"not_run","ranked_matches_before_limit":matches}))
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
    let mut next_request = None;
    if rows.len() > 1
        && let Some(revision) = metadata["resolved_revision"].as_str()
        && let Some(mode @ ("text" | "precedent" | "recent")) = metadata["mode"].as_str()
    {
        let mut arguments = serde_json::to_value(req)
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        arguments["as_of_rev"] = json!(revision);
        arguments["mode"] = json!(mode);
        next_request = Some(arguments);
    }
    let next_call = |offset| {
        next_request.clone().map(|mut arguments| {
            arguments["offset"] = json!(offset);
            json!({"tool":"search_history","arguments":arguments})
        })
    };
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
        metadata["coverage"]=json!(if metadata["mode"]=="patch" {
            "One exact file's first-parent Git patch in the selected revision scope, with full object IDs and original UTF-8 patch/hash. Binary entries supply no text patch. No indexed search, embedding, LLM, external diff or text conversion. Existing complete_capture/retain_response preserves the original result; historical evidence does not prove current policy or runtime behavior."
        } else if metadata["mode"]=="commit" {
            "One exact Git commit in the selected revision scope. Complete original UTF-8 message and first-parent changed-file cohort, including generated paths; no patch body or rename inference. No indexed search, embedding or LLM call. Use complete_capture or retain_response=true with output_json=true for immutable full-result recovery when the complete row exceeds a reply budget."
        } else if metadata["mode"]=="recent" {
            "Newest-first first-parent change cards in the scanned window. Path predicates use capped non-generated card paths. Recover any commit for its complete Git changed-file cohort. Older windows may contain more matches. No model calls."
        } else {
            "Selected ranking window only. Text matched-file lists are drawn from retrieved documents, not the complete commit. Precedent descriptions and non-generated file lists are capped at 1500 characters and 40 paths before previews. Git recovery includes generated paths. No corpus completeness or snapshot guarantee; LLM reranking and mutable inputs can reorder pages."
        });
        structured_response::finish_with_next_call(metadata,rows,structured_response::Page{
            offset:req.offset,window_limit:req.sanitized_limit(),total:Some(window)},req.max_tokens,next_call)?.0
    } else {
        let pagination=|returned| {
            let next=offset+returned;
            let action=if next<window {
                    if let Some(call) = next_call(next) {
                        format!("next_page: {call}\n")
                    } else {
                        format!("next_page: offset={next}; keep query, filters, limit and ranking options fixed.\n")
                    }
                }
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
