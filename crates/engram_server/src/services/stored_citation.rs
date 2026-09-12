//! Exact stored UTF-8 citation pages. These hashes do not normalize newlines and
//! say nothing about semantic truth or correspondence to current source files.
use crate::models::{StoredCitationRequest, StoredCitationUnit};
use rmcp::ErrorData as McpError;
use serde_json::{Value, json};

/// Selected content bytes, not serialized JSON bytes (escaping may expand it).
pub const MAX_CONTENT_BYTES: usize = 40_000;

pub fn raw_hash(content: &str) -> String {
    format!(
        "blake3-raw-utf8:{}",
        blake3::hash(content.as_bytes()).to_hex()
    )
}

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

pub fn render(
    content: &str,
    req: &StoredCitationRequest,
    identity: Value,
    metadata: Value,
) -> Result<Value, McpError> {
    if let Some(quote) = &req.verify_quote {
        if req.expected_raw_hash.is_none() {
            return Err(invalid("quote_verification_requires_expected_raw_hash"));
        }
        if quote.trim().is_empty() || quote.len() > 4_000 {
            return Err(invalid("quote_verification_invalid: quote must be non-whitespace and at most 4000 UTF-8 bytes"));
        }
    }
    let hash = raw_hash(content);
    if req.expected_raw_hash.as_ref().is_some_and(|h| h != &hash) {
        return Err(invalid(
            "citation_hash_mismatch: stored content changed or expected_raw_hash is invalid; rediscover the document and restart citation paging",
        ));
    }
    // LF alone delimits stored lines. CR, Unicode separators and all terminators
    // are retained. A final LF belongs to its line, not a new empty line.
    let mut starts = vec![0];
    for (i, b) in content.bytes().enumerate() {
        if b == b'\n' && i + 1 < content.len() {
            starts.push(i + 1);
        }
    }
    let lines = if content.is_empty() { 0 } else { starts.len() };
    let (start, end, byte_start, byte_end) = match req.unit {
        StoredCitationUnit::Lines => {
            let start = req.start.unwrap_or(1);
            if lines == 0 {
                if start != 1 || req.end.is_some() {
                    return Err(invalid(
                        "citation_range_invalid: empty document accepts only initial open-ended line page",
                    ));
                }
                (1, 0, 0, 0)
            } else {
                if start == 0 || start > lines {
                    return Err(invalid(format!(
                        "citation_range_invalid: line start must be 1..={lines}"
                    )));
                }
                let bs = starts[start - 1];
                let end = if let Some(end) = req.end {
                    if end < start || end > lines {
                        return Err(invalid(format!(
                            "citation_range_invalid: inclusive line end must be {start}..={lines}"
                        )));
                    }
                    end
                } else {
                    let mut end = start - 1;
                    while end < lines {
                        let next_byte = starts.get(end + 1).copied().unwrap_or(content.len());
                        if next_byte - bs > MAX_CONTENT_BYTES {
                            break;
                        }
                        end += 1;
                    }
                    if end < start {
                        return Err(McpError::invalid_params(
                            "line_exceeds_citation_limit: next whole line exceeds 40000 selected content bytes; use utf8_bytes continuation".to_string(),
                            Some(json!({"continuation":{"unit":"utf8_bytes","start":bs,"expected_raw_hash":hash},"selected_content_byte_limit":MAX_CONTENT_BYTES})),
                        ));
                    }
                    end
                };
                (
                    start,
                    end,
                    bs,
                    starts.get(end).copied().unwrap_or(content.len()),
                )
            }
        }
        StoredCitationUnit::Utf8Bytes => {
            let start = req.start.unwrap_or(0);
            if start > content.len() || !content.is_char_boundary(start) {
                return Err(invalid(
                    "citation_range_invalid: byte start must be an in-bounds UTF-8 boundary",
                ));
            }
            let end = if let Some(end) = req.end {
                end
            } else {
                let mut end = start.saturating_add(MAX_CONTENT_BYTES).min(content.len());
                while !content.is_char_boundary(end) {
                    end -= 1;
                }
                end
            };
            if end < start || end > content.len() || !content.is_char_boundary(end) {
                return Err(invalid(
                    "citation_range_invalid: exclusive byte end must be an in-bounds UTF-8 boundary at or after start",
                ));
            }
            if end == start && start < content.len() {
                return Err(invalid(
                    "citation_range_invalid: empty byte range before end of document cannot advance paging",
                ));
            }
            (start, end, start, end)
        }
    };
    if byte_end - byte_start > MAX_CONTENT_BYTES {
        return Err(McpError::invalid_params("citation_range_exceeds_limit: explicit range exceeds 40000 selected content bytes; omit end for bounded paging".to_string(), Some(json!({"continuation":{"unit":req.unit,"start":start,"expected_raw_hash":hash},"selected_content_byte_limit":MAX_CONTENT_BYTES}))));
    }
    let selected = &content[byte_start..byte_end];
    let continuation = if byte_end < content.len() {
        Some(
            json!({"unit":req.unit,"start":if req.unit == StoredCitationUnit::Lines { end + 1 } else { byte_end },"expected_raw_hash":hash}),
        )
    } else {
        None
    };
    let mut page = json!({
        "citation_version":1,
        "identity":identity,
        "metadata":metadata,
        "hash_scope":"raw_stored_utf8_no_normalization",
        "line_scope":"stored_document_lf_delimited_not_physical_source",
        "raw_content_hash":hash,
        "slice_hash":raw_hash(selected),
        "total_bytes":content.len(),
        "total_lines":lines,
        "selected_content_byte_limit":MAX_CONTENT_BYTES,
        "budget_scope":"selected_content_bytes_not_serialized_json",
        "returned_range":{"unit":req.unit,"start":start,"end":end,"byte_start":byte_start,"byte_end":byte_end,
            "initial_partial_line":byte_end > byte_start && byte_start > 0 && content.as_bytes()[byte_start - 1] != b'\n',
            "final_partial_line":byte_end > byte_start && byte_end < content.len() && content.as_bytes()[byte_end - 1] != b'\n'},
        "requested_range_complete":req.end.is_some() || continuation.is_none(),
        "document_complete":byte_start == 0 && byte_end == content.len(),
        "content":selected,
        "continuation":continuation,
    });
    if let Some(quote) = &req.verify_quote {
        let found = selected.find(quote.as_str());
        // Verification binds previously retrieved bytes; do not resend the page
        // for every proposed quote. Range/completeness fields describe the
        // searched slice, while content_status explicitly records non-delivery.
        page.as_object_mut().expect("citation page is an object").remove("content");
        page["content_status"] = json!("not_returned_for_quote_verification");
        page["quote_verification"] = json!({
            "version":1,
            "status":if found.is_some() { "exact_match" } else { "not_found_in_selected_range" },
            "quote_raw_hash":raw_hash(quote),
            "quote_bytes":quote.len(),
            "searched_byte_range":{"start":byte_start,"end":byte_end},
            "first_match_byte_range":found.map(|offset| json!({"start":byte_start+offset,"end":byte_start+offset+quote.len()})),
            "selected_range_search_complete":true,
            "document_complete":byte_start == 0 && byte_end == content.len(),
            "scope":"Exact raw stored UTF-8 substring in the selected range only; byte end is exclusive. No normalization. First occurrence only; uniqueness, semantic truth and primary-source authority are not verified. Absence includes quotes crossing the selected boundary and is not document-wide absence."
        });
    }
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn page(s: &str, req: Value) -> Result<Value, McpError> {
        render(
            s,
            &serde_json::from_value(req).unwrap(),
            json!({}),
            json!({}),
        )
    }
    #[test]
    fn exact_bytes_quotes_markdown_and_only_lf_lines() {
        let s = "# ‘don’t’ `x` 'quote'\r\n\nA\rB\u{2028}C\u{85}e\u{301}é";
        let p = page(s, json!({"start":2,"end":3})).unwrap();
        assert_eq!(p["content"], "\nA\rB\u{2028}C\u{85}e\u{301}é");
        assert_eq!(p["total_lines"], 3);
        assert_eq!(
            p["raw_content_hash"],
            format!("blake3-raw-utf8:{}", blake3::hash(s.as_bytes()).to_hex())
        );
        assert_eq!(p["slice_hash"], raw_hash(p["content"].as_str().unwrap()));
        assert_ne!(raw_hash("x\r\n"), raw_hash("x\n"));
        assert_ne!(raw_hash("é"), raw_hash("e\u{301}"));
        assert_eq!(page("x\n", json!({})).unwrap()["total_lines"], 1);
    }
    #[test]
    fn reconstructs_over_80kb_with_hash_bound_continuations() {
        let s = "‘Exact’ **markdown**\r\n".repeat(6000);
        let mut req = json!({});
        let mut all = String::new();
        let mut count = 0;
        loop {
            let p = page(&s, req).unwrap();
            count += 1;
            assert!(p["content"].as_str().unwrap().len() <= MAX_CONTENT_BYTES);
            all.push_str(p["content"].as_str().unwrap());
            if p["continuation"].is_null() {
                break;
            }
            req = p["continuation"].clone();
        }
        assert!(count > 2);
        assert_eq!(all.as_bytes(), s.as_bytes());
    }
    #[test]
    fn long_line_byte_pages_preserve_utf8_and_partial_flags() {
        let s = "é🦀".repeat(16000);
        assert!(
            page(&s, json!({}))
                .unwrap_err()
                .message
                .contains("line_exceeds")
        );
        let mut req = json!({"unit":"utf8_bytes"});
        let mut all = String::new();
        loop {
            let p = page(&s, req).unwrap();
            if !all.is_empty() {
                assert_eq!(p["returned_range"]["initial_partial_line"], true);
            }
            all.push_str(p["content"].as_str().unwrap());
            if p["continuation"].is_null() {
                break;
            }
            assert_eq!(p["returned_range"]["final_partial_line"], true);
            req = p["continuation"].clone();
        }
        assert_eq!(all, s);
    }
    #[test]
    fn invalid_ranges_and_newline_hash_change_fail_closed() {
        for req in [
            json!({"start":0}),
            json!({"start":3}),
            json!({"start":2,"end":1}),
            json!({"unit":"utf8_bytes","start":1}),
            json!({"unit":"utf8_bytes","end":1}),
            json!({"unit":"utf8_bytes","end":99}),
        ] {
            assert!(page("é\nx", req).is_err());
        }
        assert!(page("x\n", json!({"expected_raw_hash":raw_hash("x\r\n")})).is_err());
        assert!(page(&"x".repeat(40001), json!({"unit":"utf8_bytes","end":40001})).is_err());
        assert!(page(&"x\n".repeat(20001), json!({"end":20001})).is_err());
        let p = page("", json!({})).unwrap();
        assert_eq!(p["returned_range"]["start"], 1);
        assert_eq!(p["returned_range"]["end"], 0);
        assert_eq!(p["total_lines"], 0);
        assert_eq!(p["document_complete"], true);
        assert!(page("", json!({"end":1})).is_err());
        assert_eq!(
            page("", json!({"unit":"utf8_bytes"})).unwrap()["content"],
            ""
        );
    }
    #[test]
    fn optional_quote_verifies_exact_bytes_and_preserves_plain_response() {
        let source = "prefix \u{00c5}\r\n**exact** suffix";
        let quote = "\u{00c5}\r\n**exact**";
        let plain = page(source, json!({})).unwrap();
        let mut verified = page(source, json!({"expected_raw_hash":raw_hash(source),"verify_quote":quote})).unwrap();
        let proof = verified.as_object_mut().unwrap().remove("quote_verification").unwrap();
        assert!(verified.get("content").is_none());
        assert_eq!(verified["content_status"], "not_returned_for_quote_verification");
        let mut expected_envelope = plain.clone();
        expected_envelope.as_object_mut().unwrap().remove("content");
        expected_envelope["content_status"] = json!("not_returned_for_quote_verification");
        assert_eq!(verified, expected_envelope);
        assert_eq!(page(source, json!({"expected_raw_hash":raw_hash(source)})).unwrap(), plain);
        assert_eq!(proof["status"], "exact_match");
        assert_eq!(proof["quote_raw_hash"], raw_hash(quote));
        assert_eq!(proof["first_match_byte_range"], json!({"start":7,"end":7+quote.len()}));
        assert!(proof["scope"].as_str().unwrap().contains("authority are not verified"));
        for altered in ["\u{00c5}\n**exact**", "\u{00c5}\r\nexact", "\u{00e5}\r\n**exact**", "A\u{030a}\r\n**exact**"] {
            assert_eq!(page(source,json!({"expected_raw_hash":raw_hash(source),"verify_quote":altered})).unwrap()["quote_verification"]["status"], "not_found_in_selected_range");
        }
    }
    #[test]
    fn quote_scope_does_not_cross_pages_or_claim_uniqueness() {
        let source = "left-right left-right";
        let partial = page(source,json!({"unit":"utf8_bytes","start":0,"end":5,"expected_raw_hash":raw_hash(source),"verify_quote":"left-right"})).unwrap();
        assert_eq!(partial["quote_verification"]["status"],"not_found_in_selected_range");
        assert_eq!(partial["quote_verification"]["document_complete"],false);
        assert!(partial["quote_verification"]["first_match_byte_range"].is_null());
        let repeated = page(source,json!({"expected_raw_hash":raw_hash(source),"verify_quote":"left-right"})).unwrap();
        assert_eq!(repeated["quote_verification"]["first_match_byte_range"],json!({"start":0,"end":10}));
        assert!(repeated["quote_verification"].get("unique").is_none());
    }
    #[test]
    fn quote_validation_requires_hash_and_bounded_nonblank_bytes() {
        for req in [json!({"verify_quote":"x"}), json!({"expected_raw_hash":raw_hash("x"),"verify_quote":" \r\n\t"}), json!({"expected_raw_hash":raw_hash("x"),"verify_quote":""}), json!({"expected_raw_hash":raw_hash("x"),"verify_quote":"\u{65e5}".repeat(2001)})] {
            assert!(page("x",req).is_err());
        }
        assert!(page("x",json!({"expected_raw_hash":raw_hash("y"),"verify_quote":"x"})).unwrap_err().message.contains("citation_hash_mismatch"));
        let source="x".repeat(4000);
        assert_eq!(page(&source,json!({"expected_raw_hash":raw_hash(&source),"verify_quote":source})).unwrap()["quote_verification"]["status"],"exact_match");
    }

}
