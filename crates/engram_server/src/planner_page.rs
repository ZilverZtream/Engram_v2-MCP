//! A bounded first reading of a saved report. Recovery always starts at offset
//! zero of the unchanged original, independently of the preview representation.
use serde_json::{Value, json};

pub(crate) fn with_preview(mut packet: Value, evidence: &str, budget: usize) -> String {
    let fallback = packet.to_string();
    if evidence.trim().is_empty() {
        return fallback;
    }
    packet["report_content"] = json!("PARTIAL_EVIDENCE");
    packet["response_delivery"]["instruction"] = json!(
        "Selected evidence is shown. Follow next_call from offset zero for the complete saved report, including omitted rows, guidance and qualifications."
    );
    packet["source_basis"] = json!(
        "Indexed registered working checkout. Use get_index_freshness to bind cited files to a commit."
    );
    packet["first_page"] = json!("");
    let fixed = packet.to_string().len();
    let Some(space) = budget.checked_sub(fixed) else {
        return fallback;
    };
    // Count JSON escapes, not just source bytes. Never cut a UTF-8 character.
    let mut used = 0;
    let mut end = 0;
    for (idx, ch) in evidence.char_indices() {
        used += match ch {
            '"' | '\\' | '\n' | '\r' | '\t' | '\x08' | '\x0c' => 2,
            '\0'..='\x1f' => 6,
            _ => ch.len_utf8(),
        };
        if used > space {
            break;
        }
        end = idx + ch.len_utf8();
    }
    if end == 0 {
        return fallback;
    }
    packet["first_page"] = json!(&evidence[..end]);
    packet.to_string()
}

pub(crate) fn evidence(tool: &str, body: &str) -> String {
    match tool {
        "find_symbol_references" => {
            let mut rows = Vec::new();
            let mut relevant = false;
            let mut shown = 0;
            for line in body.lines() {
                let text = line.trim();
                if line.starts_with("Symbol:")
                    || text.starts_with("node_id:")
                    || text.starts_with("Outgoing dependencies")
                    || text.starts_with("Incoming references")
                    || text.contains("distinct symbols match")
                {
                    rows.push(line);
                    relevant = false;
                } else if text.starts_with('[') {
                    relevant = text.starts_with("[calls]") || text.starts_with("[writes_state]");
                    shown = 0;
                    if relevant {
                        rows.push(line);
                    }
                } else if relevant
                    && (text.starts_with("<-") || text.starts_with("->"))
                    && shown < 5
                {
                    rows.push(line);
                    shown += 1;
                }
            }
            if rows.is_empty() {
                body.to_owned()
            } else {
                rows.join("\n")
            }
        }
        "get_method_edit_context" => {
            if let Ok(value) = serde_json::from_str::<Value>(body) {
                let info = &value["method_info"];
                if info.is_object() {
                    return json!({"method":info["fqn"],"file":info["file_path"],"signature":info["signature"],
                        "callers":info["called_by"],"tables":info["db_tables_accessed"],
                        "ordered_source_observations":value["ordered_source_observations"],"effects":info["effects"],"consumer_call_paths":value["consumer_call_paths"],
                        "freshness":value["freshness"],"source_excerpt":value["full_source"].as_str().unwrap_or("").chars().take(1600).collect::<String>()}).to_string();
                }
            }
            // Use the handler's own rendered sections, including their caller labels.
            let mut selected = vec!["Source basis: indexed working checkout".to_string()];
            // The embedded method report has its own level-one heading. Split
            // at both heading levels so its signature cannot be selected again
            // as part of the preceding consumer section.
            let sections: Vec<_> = body.split("\n## ").flat_map(|s| s.split("\n# ")).collect();
            for title in [
                "Method:",
                "Ordered source observations",
                "Called By",
                "Database Tables",
                "Consumer call paths",
                "Calls",
                "Full Source",
                "Body Preview",
            ] {
                if let Some(section) = sections.iter().find(|s| {
                    if title == "Method:" {
                        s.starts_with("Method: ")
                    } else {
                        s.lines().next() == Some(title)
                    }
                }) {
                    selected.push(section.chars().take(6500).collect());
                }
            }
            selected.join("\n## ")
        }
        "get_ui_blueprint" => {
            if let Ok(v) = serde_json::from_str::<Value>(body) {
                let containers: Vec<_> = v["containers"].as_array().into_iter().flatten().map(|c|
                    json!({"id": c["id"], "children":c["children"].as_array().map(Vec::len)})).collect();
                return json!({"file":v["file"],"container_count":v["container_count"],"containers":containers,
                    "narrow_with":"Call get_ui_blueprint on the same file with control_id set to an exact identifier."}).to_string();
            }
            body.to_owned()
        }
        "get_review_precedents" => body.split("\n--- #").skip(1).map(|row| row.lines()
            .take_while(|line| !line.starts_with("Imported discussion recovery")).take(8).map(|line| line.chars().take(320).collect::<String>()).collect::<Vec<_>>().join("\n"))
            .collect::<Vec<_>>().join("\n--- #"),
        "trace_data_flow" => {
            if let Ok(report) = serde_json::from_str::<Value>(body)
                && report.get("entry_point").is_some()
            {
                serde_json::json!({
                    "entry_point": report["entry_point"],
                    "tables_touched": report["tables_touched"],
                    "state_reads": report["state_reads"],
                    "state_writes": report["state_writes"],
                    "unresolved_calls": report["unresolved_calls"],
                    "depth_cap": report["follow"]["depth_cap"],
                    "truncated_nodes": report["follow"]["truncated_nodes"]
                }).to_string()
            } else {
                body.lines().take_while(|line| !line.starts_with("- stop:") && !line.starts_with("## Value lineage"))
                    .collect::<Vec<_>>().join("\n")
            }
        }
        "derive_test_matrix" | "map_page_lifecycle" | "get_ui_conformance" | "trace_ui_event" | "trace_ui_action" => body.lines().take_while(|line|
            !line.starts_with("## Repository policy sources")).collect::<Vec<_>>().join("\n"),
        "pre_push_audit" => body.lines().take_while(|line| !line.starts_with("Retrieval sampling")).collect::<Vec<_>>().join("\n"),
        "plan_user_story" => body
            .lines()
            .skip_while(|line| !line.starts_with("concepts:"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn method_preview_shows_identity_once_alongside_consumer_paths() {
        let report = "# Edit Context\n\n## Consumer call paths\ncaller -> persist\n\n# Method: Store.Save\n- **Signature**: Save()\n\n## Calls\n- Write()";
        let preview = super::evidence("get_method_edit_context", report);
        for text in ["Method: Store.Save", "**Signature**", "caller -> persist", "Write()"] {
            assert_eq!(preview.matches(text).count(), 1, "{preview}");
        }
    }
}
