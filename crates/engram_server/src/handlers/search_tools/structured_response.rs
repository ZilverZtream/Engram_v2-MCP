//! Shared JSON contract for ranked search tools; no Markdown parsing.
use super::{SymbolSpan, line_ranges_overlap, response_budget};
use engram_index::HybridHit;
use rmcp::ErrorData as McpError;
use serde_json::{Value, json};

pub(crate) struct Page {
    pub offset: usize,
    pub window_limit: usize,
    /// Known vector ranking-window count, never the total corpus match count.
    /// Search's optimized prefix cannot establish a complete window count.
    pub total: Option<usize>,
}

pub(super) fn quality(quality: engram_index::SemanticQuality) -> &'static str {
    match quality {
        engram_index::SemanticQuality::Semantic=>"semantic",
        engram_index::SemanticQuality::DegradedTrigram=>"degraded_trigram",
        engram_index::SemanticQuality::Off=>"off",
    }
}

pub(super) fn envelope(tool: &str, project: &str, query: &str, generation: Option<u64>) -> Value {
    json!({"schema_version":1,"tool":tool,"project_id":project,"query":query,
        "active_generation":generation,"status":"ok","search_executed":true,
        "warnings":[],"message":null,"freshness":null,"semantic_quality":null,"guidance":null,"guidance_scope":null,"next_tools":[],
        "recovery_notes":"full_chunk returns content with applicable live guidance; citation_chunk returns exact stored content without injected guidance. Follow citation continuations with the same project_id/namespace/doc_id. Resolve policy omissions before editing."})
}

pub(super) struct Hit<'a> {
    pub project: &'a str,
    pub namespace: &'a str,
    pub source: &'a str,
    pub rank: usize,
    pub hit: &'a HybridHit,
    pub content_requested: bool,
    pub content: Option<&'a str>,
    pub max_chars: usize,
    pub symbols: Option<&'a [SymbolSpan]>,
}

impl Hit<'_> {
    pub(super) fn value(self) -> Value {
        let h=self.hit;
        let mut full=json!({"project_id":self.project,"namespace":self.namespace,"doc_id":h.doc_id});
        let mut citation=full.clone(); citation["citation"]=json!({});
        if self.namespace=="memory" {full["inject_rules"]=json!(true);}
        let mut recovery=json!({"full_chunk":{"tool":"get_chunk","arguments":full},
            "citation_chunk":{"tool":"get_chunk","arguments":citation}});
        if self.namespace=="memory_bank" && let Some(section)=h.path.as_str().strip_prefix("memory_bank:") {
            recovery["full_section"]=json!({"tool":"read_memory_bank","arguments":{"project_id":self.project,"section":section}});
            recovery["citation_section"]=json!({"tool":"read_memory_bank","arguments":{"project_id":self.project,"section":section,"citation":{}}});
        }
        let symbols=self.symbols.unwrap_or_default().iter()
            .filter(|(_,_,_,start,end)|line_ranges_overlap(h.start_line,h.end_line,*start,*end)).take(3)
            .map(|(name,kind,id,start,end)|json!({"name":name,"kind":kind,"node_id":id,"line_start":start,"line_end":end}))
            .collect::<Vec<_>>();
        let content=self.content.map(|text|text.chars().take(self.max_chars).collect::<String>());
        let truncated=self.content.is_some_and(|text|text.chars().count()>self.max_chars);
        let snippet=if content.is_some() {None} else {h.snippet.as_deref()};
        let snippet_truncated=snippet.is_some() && h.snippet_truncated;
        json!({"rank":self.rank,"project_id":self.project,"namespace":self.namespace,"source":self.source,
            "doc_id":h.doc_id,"chunk_id":h.chunk_id,"path":h.path.as_str(),"line_start":h.start_line,"line_end":h.end_line,
            "score":h.score,"symbols":symbols,"snippet":snippet,"snippet_truncated":snippet_truncated,
            "content":content,"content_status":if content.is_some(){"included"}else if self.content_requested{"unavailable"}else{"not_requested"},
            "content_truncated":truncated,"recovery":recovery})
    }
}

fn encode(value: &Value) -> Result<String,McpError> {
    serde_json::to_string(value).map_err(|error|McpError::internal_error(error.to_string(),None))
}

/// Keep metadata and whole records, accounting for JSON escaping and commas.
pub(crate) fn finish(base: Value, hits: &[Value], page: Page, tokens: Option<usize>) -> Result<(String,usize),McpError> {
    let limit=response_budget::token_bytes(tokens)?;
    let mut header=encode(&base)?;
    header.pop(); header.push_str(",\"results\":[");
    let mut rows=Vec::new(); let mut sizes=vec![0usize];
    for hit in hits {
        if !rows.is_empty() && limit.is_some_and(|limit|*sizes.last().unwrap()>limit) {break;}
        let row=encode(hit)?;
        sizes.push(sizes.last().unwrap()+row.len()+usize::from(!rows.is_empty()));
        rows.push(row);
    }
    let tail=|count:usize,budget:Option<usize>| -> Result<String,McpError> {
        let position=page.offset.saturating_add(count);
        let next=(count>0 && position<page.window_limit && page.total.is_none_or(|total|position<total)).then_some(position);
        let omitted=hits.len()-count;
        let content_truncated=hits[..count].iter().any(|hit|hit["content_truncated"]==true);
        let snippets_truncated=hits[..count].iter().any(|hit|hit["snippet_truncated"]==true);
        let files_truncated=hits[..count].iter().any(|hit|hit["files_truncated"]==true);
        let metadata=json!({"response_page":{"offset":page.offset,"returned":count,"requested_hits":hits.len(),
            "next_offset":next,"window_limit":page.window_limit,"total_count":page.total,
            "total_count_scope":if page.total.is_some(){"bounded_ranking_window"}else{"not_computed"},
            "window_end":next.is_none(),"has_more":if omitted>0 || (page.total.is_some() && next.is_some()){Some(true)}else if next.is_none(){Some(false)}else{None},
            "snapshot":false,"scope":"Keep query, filters, scope, result limits and ranking options fixed. Offset is not a durable cursor; mutable inputs may change results. Window end does not prove corpus completeness."},
            "response_budget":budget.map(|max_tokens|json!({"max_tokens":max_tokens,"accounting":"ceil(utf8_text_bytes/4)","omitted_from_requested_page":omitted,"truncated":omitted>0})),
            "truncated":omitted>0 || content_truncated || snippets_truncated || files_truncated,
            "truncation":{"whole_hits_omitted":omitted,"included_content_truncated":content_truncated,"snippets_truncated":snippets_truncated,"file_lists_truncated":files_truncated}});
        let metadata=encode(&metadata)?;
        Ok(format!("],{}",&metadata[1..]))
    };
    let length=|count,budget| -> Result<usize,McpError> {Ok(header.len()+sizes[count]+tail(count,budget)?.len())};
    let count=if let Some(limit)=limit {
        let first=usize::from(!hits.is_empty());
        let mut selected=None;
        for count in (first..=rows.len()).rev() {
            if length(count,tokens)?<=limit {selected=Some(count);break;}
        }
        match selected {
            Some(count)=>count,
            None=>{
                let mut minimum=length(first,tokens)?.div_ceil(4);
                while length(first,Some(minimum))?>minimum*4 {minimum=length(first,Some(minimum))?.div_ceil(4);}
                return Err(McpError::invalid_params(format!("max_tokens is too small: minimum {minimum} estimated tokens for one complete hit and required JSON context, or the complete empty reply. Increase max_tokens or reduce per-result content. Warnings and repository guidance are retained."),None));
            }
        }
    } else {rows.len()};
    let output=format!("{header}{}{}",rows[..count].join(","),tail(count,tokens)?);
    debug_assert!(limit.is_none_or(|limit|output.len()<=limit));
    Ok((output,count))
}
