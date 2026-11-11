//! Complete-hit response paging with explicit, provider-independent estimates.
use crate::models::requests::{MAX_SEARCH_RESULTS, SearchMemoryRequest};
use rmcp::ErrorData as McpError;

pub(super) fn bytes(req: &SearchMemoryRequest) -> Result<Option<usize>, McpError> {
    token_bytes(req.max_tokens)
}

pub(super) fn token_bytes(max_tokens: Option<usize>) -> Result<Option<usize>, McpError> {
    max_tokens.map(|tokens| {
        tokens.checked_mul(4).filter(|bytes| *bytes > 0).ok_or_else(||
            McpError::invalid_params("max_tokens must be positive and fit the UTF-8 byte estimate", None))
    }).transpose()
}

fn too_small(required: usize) -> McpError {
    McpError::invalid_params(format!(
        "max_tokens is too small: minimum {} estimated tokens ({} UTF-8 text bytes) for one complete hit plus required context, or for the complete no-hit reply. Increase max_tokens or reduce the per-result content or result limit. Warnings and repository guidance are not omitted to fit.",
        required.div_ceil(4), required), None)
}

pub(super) fn message(output: String, req: &SearchMemoryRequest) -> Result<String, McpError> {
    bounded_message(output, req.max_tokens)
}

pub(super) fn bounded_message(output: String, max_tokens: Option<usize>) -> Result<String, McpError> {
    if token_bytes(max_tokens)?.is_some_and(|limit| output.len() > limit) {
        return Err(too_small(output.len()));
    }
    Ok(output)
}

pub(super) fn pagination(offset: usize, returned: usize) -> String {
    let next = offset + returned;
    if next >= MAX_SEARCH_RESULTS {
        "result_cap_reached: 200; further matches may exist. Narrow the query or scope and restart at offset=0.\n".into()
    } else {
        format!("next_page: offset={next}; results are bounded to 200 and are not a snapshot of mutable ranking inputs.\n")
    }
}

pub(super) fn finish(
    header: String,
    hits: &[String],
    suffix: &str,
    freshness: &str,
    guidance: bool,
    req: &SearchMemoryRequest,
) -> Result<(String, usize), McpError> {
    let offset = req.sanitized_offset();
    finish_with(header, hits, suffix, freshness, guidance, req.max_tokens,
        |count| pagination(offset, count))
}

pub(super) fn vector_pagination(offset: usize, returned: usize, window_len: usize, top_k: usize) -> String {
    let next = offset + returned;
    let position = if next < window_len {
        format!("next_page: offset={next}; keep the same query, filters, top_k and use_mmr.\n")
    } else {
        format!("ranking_window_end: {window_len}; further matches may exist. Increase top_k (max 200) or narrow scope and restart at offset=0.\n")
    };
    format!("{position}ranking_window: top_k={top_k}; pagination is not a snapshot of mutable ranking inputs.\n")
}

pub(super) fn finish_with(
    mut header: String,
    hits: &[String],
    suffix: &str,
    freshness: &str,
    guidance: bool,
    max_tokens: Option<usize>,
    pagination: impl Fn(usize) -> String,
) -> Result<(String, usize), McpError> {
    let limit = token_bytes(max_tokens)?;
    let mut prefix = Vec::with_capacity(hits.len() + 1);
    prefix.push(0usize);
    for hit in hits { prefix.push(prefix.last().copied().unwrap_or_default() + hit.len()); }
    let tail = |count: usize, budget: Option<usize>| {
        let mut value = String::new();
        if let Some(tokens) = budget {
            value.push_str(&format!(
                "response_budget: max_tokens={tokens}; accounting=ceil(utf8_text_bytes/4); returned={count}; omitted_from_requested_page={}; truncated={}\n",
                hits.len() - count, count < hits.len()));
            if count < hits.len() {
                value.push_str("Continue with next_page offset and the same other arguments; omitted hits have not been skipped.\n");
                if guidance {
                    value.push_str("guidance_scope: requested page paths, including budget-omitted hits.\n");
                }
            }
        }
        value.push_str(&pagination(count));
        value.push_str(freshness);
        value
    };
    let fixed = header.len() + suffix.len();
    let count = if let Some(limit) = limit {
        (1..=hits.len()).rev().find(|&count| fixed + prefix[count] + tail(count, max_tokens).len() <= limit)
            .ok_or_else(|| {
                // The suggested budget appears in the reply too. Account for
                // its digit width so following the minimum succeeds exactly.
                let required = |tokens| fixed + prefix[1] + tail(1, Some(tokens)).len();
                let mut minimum = required(max_tokens.unwrap_or_default()).div_ceil(4);
                while required(minimum) > minimum * 4 { minimum = required(minimum).div_ceil(4); }
                too_small(required(minimum))
            })?
    } else {
        hits.len()
    };
    for hit in &hits[..count] { header.push_str(hit); }
    header.push_str(suffix);
    header.push_str(&tail(count, max_tokens));
    debug_assert!(limit.is_none_or(|limit| header.len() <= limit));
    Ok((header, count))
}
