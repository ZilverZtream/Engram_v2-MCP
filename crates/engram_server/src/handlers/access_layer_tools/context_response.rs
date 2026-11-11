//! Budgeted implementation-context presentation. Provider/extraction semantics
//! remain in the parent handler; no durable cursor or new snapshot store.
use super::{AccessFreshness, ImplementationContext};
use crate::handlers::search_tools::response_budget;
use crate::models::PrepareImplementationContextRequest;
use rmcp::{ErrorData as McpError, model::{CallToolResult, Content}};
use serde_json::{Value, json};

const SOURCE_PART_BYTES: usize = 1024;
const HASH_PREFIX: &str = "blake3-context-v1:";

pub(super) fn validate(req: &PrepareImplementationContextRequest) -> Result<(), McpError> {
    response_budget::token_bytes(req.max_tokens)?;
    if req.context_offset > 0 && req.context_expected_hash.is_none() {
        return Err(McpError::invalid_params("context_offset requires context_expected_hash from the returned continuation; restart at context_offset=0",None));
    }
    if let Some(hash)=&req.context_expected_hash {
        let valid=hash.strip_prefix(HASH_PREFIX).is_some_and(|s|s.len()==64 && s.bytes().all(|c|c.is_ascii_hexdigit()));
        if !valid {return Err(McpError::invalid_params("context_expected_hash must be the complete blake3-context-v1 hash from a context page",None));}
    }
    Ok(())
}

fn encode(value: &Value) -> Result<String,McpError> {
    serde_json::to_string(value).map_err(|e|McpError::internal_error(e.to_string(),None))
}

fn source_parts(rows: &mut Vec<Value>, field: &str, index: Option<usize>, text: &str, metadata: Value) {
    let digest=blake3::hash(text.as_bytes()).to_hex().to_string();
    let mut start=0usize;
    loop {
        let end=text.floor_char_boundary(start.saturating_add(SOURCE_PART_BYTES).min(text.len()));
        rows.push(json!({"field":field,"index":index,"kind":"source_utf8_fragment",
            "metadata":metadata,"byte_start":start,"byte_end":end,"total_bytes":text.len(),
            "content":&text[start..end],"content_blake3":digest,
            "source_scope":"Assembled provider text; method/caller bodies are LF-joined. Byte coordinates refer to this field, not raw on-disk bytes. Concatenate fragments in order and verify the field hash. Do not treat a fragment as a complete method."}));
        if end==text.len(){break;}
        start=end;
    }
}

pub(super) fn finish(req: &PrepareImplementationContextRequest, ctx: &ImplementationContext, freshness: &AccessFreshness) -> Result<CallToolResult,McpError> {
    let mut required=serde_json::to_value(ctx).map_err(|e|McpError::internal_error(e.to_string(),None))?;
    let mut request=serde_json::to_value(req).map_err(|e|McpError::internal_error(e.to_string(),None))?;
    // Bind full assembled evidence and semantic request options. Reply format,
    // budget and position may change without changing the evidence identity.
    let mut identity_request=request.clone();
    for key in ["max_tokens","context_offset","context_expected_hash","output_json"] {identity_request.as_object_mut().unwrap().remove(key);}
    let identity=encode(&json!({"request":identity_request,"context":required}))?;
    let hash=format!("{HASH_PREFIX}{}",blake3::hash(identity.as_bytes()).to_hex());
    if req.context_expected_hash.as_ref().is_some_and(|expected|expected!=&hash) {
        request["context_offset"]=json!(0);request["context_expected_hash"]=Value::Null;
        return Err(McpError::invalid_params("context_changed: assembled evidence or request options changed. Discard prior pages and restart; no mixed-context continuation is returned.",Some(json!({"restart":{"tool":"prepare_implementation_context","arguments":request}}))));
    }

    let mut rows=Vec::new();let mut inventory=Vec::new();
    for field in ["method_body","style_profile","pattern_examples","schema_snippets","sp_signatures","state_context","control_mappings"] {
        let value=required.as_object_mut().unwrap().remove(field).unwrap_or(Value::Null);
        let begin=rows.len();
        let count=match field {
            "method_body"=>{
                if let Some(body)=value.as_str() {source_parts(&mut rows,field,None,body,Value::Null);}
                usize::from(!value.is_null())
            },
            "style_profile"=>{
                // Keep generated style qualifications together; never cut a
                // prose exception away from the rule it qualifies.
                if !value.is_null(){rows.push(json!({"field":field,"index":null,"kind":"whole_value","value":value}));}
                usize::from(!value.is_null())
            },
            "pattern_examples"=>{
                let examples=value.as_array().expect("serialized pattern list");
                for (index,example) in examples.iter().enumerate() {
                    let mut meta=example.clone();let body=meta.as_object_mut().unwrap().remove("source_code").unwrap();
                    source_parts(&mut rows,field,Some(index),body.as_str().unwrap(),meta);
                }
                examples.len()
            },
            _=>{
                let items=value.as_array().expect("serialized context list");
                for (index,item) in items.iter().enumerate(){rows.push(json!({"field":field,"index":index,"kind":"whole_value","value":item}));}
                items.len()
            }
        };
        inventory.push(json!({"field":field,"available":!value.is_null(),"items":count,"records":rows.len()-begin}));
    }
    // body_preview is redundant with method_body and may itself be large. Keep
    // it recoverable as its own original field, never silently delete it.
    let preview=required["method_info"].as_object_mut().unwrap().remove("body_preview").unwrap_or(Value::Null);
    let begin=rows.len();
    if let Some(text)=preview.as_str(){source_parts(&mut rows,"method_info.body_preview",None,text,Value::Null);}
    inventory.push(json!({"field":"method_info.body_preview","available":!preview.is_null(),"items":usize::from(!preview.is_null()),"records":rows.len()-begin}));

    if req.context_offset>rows.len() {
        return Err(McpError::invalid_params(format!("context_offset exceeds {} assembled records; follow the returned continuation or restart at zero",rows.len()),None));
    }
    let offset=req.context_offset;let total=rows.len();let rows=&rows[offset..];
    let base=json!({"schema_version":1,"tool":"prepare_implementation_context","project_id":req.project_id,
        "context_hash":hash,"required_context":required,"freshness":freshness,"field_inventory":inventory,
        "interpretation":"Required context, complete policy qualifications, warnings and indexed coverage remain on every page. Deferred fields are in records, not absent dependencies. Recover all pages before claiming complete context. Context hash binds assembled evidence, not a retained filesystem/index snapshot, exhaustive extraction or current edit safety. Provider-level omissions remain distinct from response paging."});
    let mut header=if req.output_json {encode(&base)?} else {
        format!("# Implementation Context: `{}`\n\n## Required context and coverage\n\n```json\n{}\n```\n\n## Context records\n",ctx.method_info.fqn,serde_json::to_string_pretty(&base).map_err(|e|McpError::internal_error(e.to_string(),None))?)
    };
    if req.output_json {header.pop();header.push_str(",\"records\":[");}
    let mut encoded=Vec::new();let mut sizes=vec![0usize];
    for row in rows {
        let value=if req.output_json {format!("{}{}",if encoded.is_empty(){""}else{","},encode(row)?)} else {format!("\n```json\n{}\n```\n",encode(row)?)};
        sizes.push(sizes.last().unwrap()+value.len());encoded.push(value);
    }
    let tail=|count:usize,tokens:Option<usize>| -> Result<String,McpError> {
        let next=offset+count;
        let continuation=if next<total {
            let mut args=request.clone();args["context_offset"]=json!(next);args["context_expected_hash"]=json!(hash);args["max_tokens"]=json!(tokens);
            Some(json!({"tool":"prepare_implementation_context","arguments":args}))
        } else {None};
        let page=json!({"response_budget":tokens.map(|tokens|json!({"max_tokens":tokens,"accounting":"ceil(utf8_text_bytes/4)"})),
            "response_page":{"offset":offset,"returned":count,"total_count":total,"total_count_scope":"assembled_context_records",
                "omitted_before":offset,"omitted_after":total-next,"snapshot":false,"context_complete_in_this_reply":offset==0&&next==total},
            "truncated":offset>0||next<total,"continuation":continuation});
        let value=encode(&page)?;
        Ok(if req.output_json {format!("],{}",&value[1..])} else {format!("\ncontext_page: {value}\n")})
    };
    let length=|count,tokens| -> Result<usize,McpError> {Ok(header.len()+sizes[count]+tail(count,tokens)?.len())};
    let limit=response_budget::token_bytes(req.max_tokens)?;
    let first=usize::from(!rows.is_empty());
    let count=if let Some(limit)=limit {
        let mut selected=None;
        for count in (first..=rows.len()).rev(){if length(count,req.max_tokens)?<=limit {selected=Some(count);break;}}
        match selected {
            Some(count)=>count,
            None=>{
                let mut minimum=length(first,req.max_tokens)?.div_ceil(4);
                while length(first,Some(minimum))?>minimum*4 {minimum=length(first,Some(minimum))?.div_ceil(4);}
                return Err(McpError::invalid_params(format!("max_tokens is too small: minimum {minimum} estimated tokens for required context and the next complete record (or empty page). Increase max_tokens; policy, warnings and coverage are never omitted to fit."),Some(json!({"minimum_max_tokens":minimum}))));
            }
        }
    } else {rows.len()};
    for row in &encoded[..count]{header.push_str(row);}
    header.push_str(&tail(count,req.max_tokens)?);
    debug_assert!(limit.is_none_or(|limit|header.len()<=limit));
    Ok(CallToolResult::success(vec![Content::text(header)]))
}
