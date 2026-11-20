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
                        "effects":info["effects"],"consumer_call_paths":value["consumer_call_paths"],
                        "freshness":value["freshness"],"source_excerpt":value["full_source"].as_str().unwrap_or("").chars().take(1600).collect::<String>()}).to_string();
                }
            }
            // Use the handler's own rendered sections, including their caller labels.
            let mut selected = Vec::new();
            let mut rest = Vec::new();
            for section in body.split("\n## ") {
                if section.starts_with("Called By\n")
                    || section.starts_with("Database Tables\n")
                    || section.starts_with("Consumer call paths\n")
                    || section.contains("**Signature**")
                {
                    selected.push(section);
                } else {
                    rest.push(section);
                }
            }
            selected.extend(rest);
            selected.join("\n## ")
        }
        "plan_user_story" => body
            .lines()
            .skip_while(|line| !line.starts_with("concepts:"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}
