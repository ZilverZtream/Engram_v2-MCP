//! Whole-record paging for both grep renderings. Search/overlay scope is unchanged.
use super::{GrepProjectRequest, McpError, markdown_parts};
use engram_index::grep::GrepResult;
use serde_json::{Value, json};

pub(super) fn byte_limit(tokens: Option<usize>) -> Result<Option<usize>, McpError> {
    tokens.map(|tokens| tokens.checked_mul(4).filter(|bytes| *bytes>0)
        .ok_or_else(|| McpError::invalid_params("max_tokens must be positive and fit the UTF-8 byte estimate", None)))
        .transpose()
}

fn encode(value: &Value) -> Result<String, McpError> {
    serde_json::to_string(value).map_err(|error| McpError::internal_error(error.to_string(), None))
}

pub(super) fn render(result: &GrepResult, req: &GrepProjectRequest, value: Option<Value>, freshness: &str) -> Result<String, McpError> {
    let limit=byte_limit(req.max_tokens)?;
    let window=result.matches.len();
    let start=req.offset.min(window);
    let available=window-start;
    let (header,rows,footer,separator)=if let Some(mut value)=value {
        let matches=value.as_object_mut().and_then(|object|object.remove("matches"))
            .and_then(|matches|if let Value::Array(matches)=matches {Some(matches)} else {None})
            .ok_or_else(||McpError::internal_error("grep result missing match records",None))?;
        let mut rows=Vec::new();
        let mut rendered_bytes=0usize;
        for hit in matches.iter().skip(start) {
            if !rows.is_empty() && limit.is_some_and(|limit|rendered_bytes>limit) {break;}
            let row=encode(hit)?;
            rendered_bytes+=row.len()+usize::from(!rows.is_empty());
            rows.push(row);
        }
        let mut header=encode(&value)?;
        header.pop(); // The server-created nonempty object ends with its closing brace.
        header.push_str(",\"matches\":[");
        (header,rows,String::new(),",")
    } else {
        let parts=markdown_parts(result,&req.pattern,req.regex,&req.project_id,&req.namespace,req.max_results.clamp(1,1000),start,limit.unwrap_or(3000));
        (parts.header,parts.hits,format!("{}{freshness}",parts.footer),"")
    };
    let mut sizes=Vec::with_capacity(rows.len()+1);
    sizes.push(0usize);
    for row in &rows {
        let comma=if sizes.len()>1 {separator.len()} else {0};
        sizes.push(sizes.last().copied().unwrap_or_default()+comma+row.len());
    }
    let tail=|count:usize,tokens:Option<usize>| -> Result<String,McpError> {
        let next=(count<available).then(||req.offset+count);
        let page=json!({"offset":req.offset,"returned":count,"window_matches":window,
            "remaining_in_window":available-count,"next_offset":next,"window_end":next.is_none(),
            "scope":"same bounded max_results window; keep pattern, filters, context and freshness fixed; source is not snapshotted across calls"});
        let budget=tokens.map(|tokens|json!({"max_tokens":tokens,"accounting":"ceil(utf8_text_bytes/4)",
            "truncated":count<available,"omitted_from_requested_page":available-count}));
        if req.output_json {
            let mut fields=json!({"response_page":page});
            if let Some(budget)=budget {fields["response_budget"]=budget;}
            let fields=encode(&fields)?;
            // The closing metadata brace closes the original result object.
            Ok(format!("],{}",&fields[1..]))
        } else {
            let mut text=String::new();
            if count<available && tokens.is_none() {
                text.push_str(&format!("\n_… {} more match(es) from this result not shown (Markdown output budget reached). Follow next_page with the same request, or use `output_json: true`. Search/result caps and coverage warnings still apply._\n",available-count));
            }
            text.push_str(&format!("response_page: {}\n",encode(&page)?));
            if let Some(budget)=budget {text.push_str(&format!("response_budget: {}\n",encode(&budget)?));}
            if let Some(next)=next {
                text.push_str(&format!("next_page: offset={next}; keep the same other arguments.\n"));
            } else {
                text.push_str("result_window_end: further matches may exist beyond search/verification limits; narrow scope or raise max_results (max 1000) and restart at offset=0.\n");
            }
            text.push_str(&footer);
            Ok(text)
        }
    };
    let length=|count,tokens| -> Result<usize,McpError> {Ok(header.len()+sizes[count]+tail(count,tokens)?.len())};
    let count=if let Some(limit)=limit {
        let mut selected=None;
        let first=usize::from(available>0);
        for count in (first..=rows.len()).rev() {
            if length(count,req.max_tokens)?<=limit {selected=Some(count);break;}
        }
        match selected {
            Some(count)=>count,
            None=>{
                let mut minimum=length(first,req.max_tokens)?.div_ceil(4);
                while length(first,Some(minimum))?>minimum*4 {
                    minimum=length(first,Some(minimum))?.div_ceil(4);
                }
                return Err(McpError::invalid_params(format!(
                    "max_tokens is too small: minimum {minimum} estimated tokens for the complete required context and one match (or the empty reply). Increase max_tokens, reduce context_before/context_after, or narrow path scope. Coverage and verification warnings are retained.",
                ),None));
            }
        }
    } else {
        // Unbudgeted JSON materializes every match. Default Markdown retains
        // the old approximate matches limit in markdown_parts.
        rows.len()
    };
    let output=format!("{header}{}{}",rows[..count].join(separator),tail(count,req.max_tokens)?);
    debug_assert!(limit.is_none_or(|limit|output.len()<=limit));
    Ok(output)
}
