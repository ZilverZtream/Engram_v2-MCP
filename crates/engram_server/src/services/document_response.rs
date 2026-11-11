//! Complete reply accounting for stored-document reads. Paging never drops
//! provenance or guidance, and continuations bind exact stored UTF-8 bytes.
use crate::models::{StoredCitationRequest, StoredCitationUnit};
use rmcp::ErrorData as McpError;
use serde_json::{Value, json};

pub fn byte_limit(tokens: Option<usize>) -> Result<Option<usize>, McpError> {
    tokens
        .map(|n| {
            n.checked_mul(4).filter(|n| *n > 0).ok_or_else(|| {
                McpError::invalid_params(
                    "max_tokens must be positive and fit the UTF-8 byte estimate",
                    None,
                )
            })
        })
        .transpose()
}

fn budget(tokens: Option<usize>) -> Value {
    tokens
        .map(|n| json!({"max_tokens":n,"accounting":"ceil(utf8_text_bytes/4)"}))
        .unwrap_or(Value::Null)
}

/// Candidates are complete UTF-8 code points or lines. Check the full candidate
/// first: its absent continuation can make it smaller than a partial page.
fn fit(
    ends: &[usize],
    tokens: Option<usize>,
    encode: impl Fn(usize, Option<usize>) -> String,
) -> Result<String, McpError> {
    let limit = byte_limit(tokens)?;
    let full = encode(*ends.last().expect("at least one candidate"), tokens);
    let Some(limit) = limit else {
        return Ok(full);
    };
    if full.len() <= limit {
        return Ok(full);
    }
    let first = encode(ends[0], tokens);
    if first.len() > limit {
        let mut minimum = first.len().div_ceil(4);
        loop {
            let size = encode(ends[0], Some(minimum)).len().div_ceil(4);
            if size <= minimum {
                break;
            }
            minimum = size;
        }
        return Err(McpError::invalid_params(
            format!(
                "max_tokens is too small: minimum {minimum} estimated tokens for the next complete unit and required context. Increase max_tokens; line citations can use utf8_bytes for smaller units. Guidance and provenance are retained."
            ),
            Some(json!({"minimum_max_tokens":minimum})),
        ));
    }
    let (mut low, mut high) = (0, ends.len() - 1);
    while low + 1 < high {
        let middle = low + (high - low) / 2;
        if encode(ends[middle], tokens).len() <= limit {
            low = middle;
        } else {
            high = middle;
        }
    }
    Ok(encode(ends[low], tokens))
}

pub fn complete(text: String, tokens: Option<usize>) -> Result<String, McpError> {
    fit(&[0], tokens, |_, _| text.clone())
}

pub struct Document<'a> {
    pub tool: &'a str,
    pub content: &'a str,
    pub identity: Value,
    pub metadata: Value,
    pub prefix: &'a str,
    pub suffix: &'a str,
    /// Transformed output is indivisible: injected rules must not be paged away.
    pub transformed: bool,
    pub content_cap: Option<usize>,
}

impl Document<'_> {
    pub fn render(self, tokens: Option<usize>, output_json: bool) -> Result<String, McpError> {
        byte_limit(tokens)?;
        let hash = crate::services::stored_citation::raw_hash(self.content);
        let cap = if self.transformed {
            self.content.len()
        } else {
            self.content_cap
                .unwrap_or(self.content.len())
                .min(self.content.len())
        };
        let mut ends: Vec<usize> = if self.transformed {
            vec![self.content.len()]
        } else {
            self.content
                .char_indices()
                .map(|(i, c)| i + c.len_utf8())
                .take_while(|end| *end <= cap)
                .collect()
        };
        if ends.is_empty() {
            ends.push(0);
        }
        fit(&ends, tokens, |end, tokens| {
            let truncated = end < self.content.len();
            let recovery = if truncated {
                let mut arguments = self.identity.clone();
                arguments["citation"] =
                    json!({"unit":"utf8_bytes","start":end,"expected_raw_hash":hash});
                if let Some(tokens) = tokens {
                    arguments["max_tokens"] = json!(tokens);
                }
                Some(json!({"tool":self.tool,"arguments":arguments}))
            } else {
                None
            };
            let page = json!({"unit":"utf8_bytes","start":0,"end":end,"total_count":self.content.len(),
                "total_count_scope":if self.transformed {"transformed_content_bytes"} else {"stored_content_bytes"},"next":recovery});
            if output_json {
                json!({"schema_version":1,"tool":self.tool,"status":"ok","identity":self.identity,"metadata":self.metadata,
                    "content":&self.content[..end],"content_status":if self.transformed {"transformed"}else{"raw_stored_utf8"},
                    "raw_content_hash":if self.transformed {None}else{Some(&hash)},"response_page":page,
                    "response_budget":budget(tokens),"truncated":truncated}).to_string()
            } else {
                format!(
                    "{}{}{}\nresponse_page: {}\nresponse_budget: {}\ntruncated: {}\n",
                    self.prefix,
                    &self.content[..end],
                    self.suffix,
                    page,
                    budget(tokens),
                    truncated
                )
            }
        })
    }
}

/// Explicit ranges and quote proofs keep their full meaning. Only open-ended
/// retrieval pages shrink; byte and line continuations retain the raw hash.
pub fn citation(
    content: &str,
    req: &StoredCitationRequest,
    identity: Value,
    metadata: Value,
    tokens: Option<usize>,
) -> Result<String, McpError> {
    byte_limit(tokens)?;
    let page = crate::services::stored_citation::render(content, req, identity, metadata)?;
    if tokens.is_none() {
        return Ok(page.to_string());
    }
    let start = page["returned_range"]["byte_start"].as_u64().unwrap() as usize;
    let end = page["returned_range"]["byte_end"].as_u64().unwrap() as usize;
    let whole = req.end.is_some() || req.verify_quote.is_some();
    let mut ends = if whole {
        vec![end]
    } else {
        content[start..end]
            .char_indices()
            .filter_map(|(i, c)| {
                let boundary = start + i + c.len_utf8();
                (req.unit == StoredCitationUnit::Utf8Bytes
                    || c == '\n'
                    || boundary == content.len())
                .then_some(boundary)
            })
            .collect::<Vec<_>>()
    };
    if ends.is_empty() {
        ends.push(end);
    }
    fit(&ends, tokens, |cut, tokens| {
        let mut value = page.clone();
        if cut < end {
            let selected = &content[start..cut];
            let range_end = if req.unit == StoredCitationUnit::Lines {
                page["returned_range"]["start"].as_u64().unwrap() as usize
                    + selected.bytes().filter(|b| *b == b'\n').count()
                    - 1
            } else {
                cut
            };
            value["content"] = json!(selected);
            value["slice_hash"] = json!(crate::services::stored_citation::raw_hash(selected));
            value["returned_range"]["end"] = json!(range_end);
            value["returned_range"]["byte_end"] = json!(cut);
            value["returned_range"]["final_partial_line"] =
                json!(cut > start && cut < content.len() && content.as_bytes()[cut - 1] != b'\n');
            value["requested_range_complete"] = json!(false);
            value["document_complete"] = json!(false);
            value["continuation"] = json!({"unit":req.unit,"start":if req.unit == StoredCitationUnit::Lines {range_end+1}else{cut},"expected_raw_hash":page["raw_content_hash"]});
        }
        value["response_budget"] = budget(tokens);
        value["budget_scope"] = json!("complete_serialized_reply_utf8_bytes");
        value["truncated"] = json!(cut < end || !value["continuation"].is_null());
        value.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_escaped_and_long_line_pages_preserve_exact_utf8() {
        for body in ["".to_owned(), "\0\t\"\\🍊e\u{301}\r".repeat(5000)] {
            let mut request = json!({"unit":"utf8_bytes"});
            let mut recovered = String::new();
            loop {
                let output = citation(
                    &body,
                    &serde_json::from_value(request).unwrap(),
                    json!({"section":"exact"}),
                    json!({"warning":"must retain"}),
                    Some(512),
                )
                .unwrap();
                assert!(output.len() <= 2048);
                let value: Value = serde_json::from_str(&output).unwrap();
                assert_eq!(value["metadata"]["warning"], "must retain");
                recovered.push_str(value["content"].as_str().unwrap());
                if value["continuation"].is_null() {
                    break;
                }
                assert!(!value["content"].as_str().unwrap().is_empty());
                request = value["continuation"].clone();
            }
            assert_eq!(recovered, body);
        }
    }
    #[test]
    fn explicit_range_and_quote_proof_are_never_reinterpreted_to_fit() {
        let body = "\"🍊exact quote\"\n".repeat(80);
        for proof in [false, true] {
            let mut req = json!({"start":1,"end":80,"expected_raw_hash":crate::services::stored_citation::raw_hash(&body)});
            if proof {
                req["verify_quote"] = json!("exact quote");
            }
            let req = serde_json::from_value(req).unwrap();
            let error = citation(&body, &req, json!({}), json!({}), Some(1)).unwrap_err();
            let minimum = error.data.unwrap()["minimum_max_tokens"].as_u64().unwrap() as usize;
            let output = citation(&body, &req, json!({}), json!({}), Some(minimum)).unwrap();
            assert!(output.len() <= minimum * 4);
            let value: Value = serde_json::from_str(&output).unwrap();
            assert_eq!(value["returned_range"]["end"], 80);
            assert_eq!(value["requested_range_complete"], true);
            if proof {
                assert_eq!(value["quote_verification"]["status"], "exact_match");
                assert!(value.get("content").is_none());
            } else {
                assert_eq!(value["content"], body);
            }
        }
    }
    #[test]
    fn empty_document_and_invalid_limits_do_not_bypass_accounting() {
        let document = || Document {
            tool: "read_memory_bank",
            content: "",
            identity: json!({}),
            metadata: json!({"provenance":"retain"}),
            prefix: "provenance",
            suffix: "warning",
            transformed: false,
            content_cap: None,
        };
        for mode in [false, true] {
            let error = document().render(Some(1), mode).unwrap_err();
            let minimum = error.data.unwrap()["minimum_max_tokens"].as_u64().unwrap() as usize;
            assert!(document().render(Some(minimum), mode).unwrap().len() <= minimum * 4);
            for tokens in [0, usize::MAX] {
                assert!(document().render(Some(tokens), mode).is_err());
            }
        }
    }
}
