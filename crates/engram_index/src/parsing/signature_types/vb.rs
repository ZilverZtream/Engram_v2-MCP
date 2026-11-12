use super::*;
use crate::parsing::ExtractedSymbol;

// Split only outside VB generics, arrays, attributes and quoted defaults.
fn split_top_level(input: &str, delimiter: char) -> Vec<&str> {
    let (mut depth, mut quoted, mut start) = (0i32, false, 0);
    let mut result = Vec::new();
    for (i, ch) in input.char_indices() {
        if ch == '"' {
            quoted = !quoted;
        }
        if quoted {
            continue;
        }
        match ch {
            '(' | '<' | '[' => depth += 1,
            ')' | '>' | ']' => depth -= 1,
            _ => {}
        }
        if ch == delimiter && depth == 0 {
            result.push(input[start..i].trim());
            start = i + ch.len_utf8();
        }
    }
    result.push(input[start..].trim());
    result
}

fn parameter_list(signature: &str) -> Option<&str> {
    let mut cursor = signature.find('(')?;
    loop {
        let (mut depth, mut quoted) = (0, false);
        let end = signature[cursor..].char_indices().find_map(|(i, ch)| {
            if ch == '"' {
                quoted = !quoted;
            }
            if !quoted {
                if ch == '(' {
                    depth += 1;
                }
                if ch == ')' {
                    depth -= 1;
                    if depth == 0 {
                        return Some(cursor + i);
                    }
                }
            }
            None
        })?;
        let inner = signature[cursor + 1..end].trim();
        if !inner.to_ascii_lowercase().starts_with("of ") {
            return Some(inner);
        }
        // The generic parameter list precedes the actual value parameters.
        cursor = end + 1 + signature[end + 1..].find('(')?;
    }
}

fn named_types(expression: &str) -> BTreeSet<String> {
    let mut result = BTreeSet::new();
    let mut token = String::new();
    for ch in expression.chars().chain(std::iter::once(' ')) {
        if ch.is_alphanumeric() || matches!(ch, '_' | '.') {
            token.push(ch);
        } else if !token.is_empty() {
            if !matches!(token.to_ascii_lowercase().as_str(), "of" | "as" | "new")
                && !token.chars().all(|c| c.is_numeric())
            {
                result.insert(std::mem::take(&mut token));
            } else {
                token.clear();
            }
        }
    }
    result
}

pub(crate) fn fallback_edges(symbols: &[ExtractedSymbol]) -> Vec<ExtractedEdge> {
    let mut edges = Vec::new();
    for symbol in symbols.iter().filter(|s| s.kind == "function") {
        let Some(meta) = &symbol.metadata else {
            continue;
        };
        let Some(signature) = meta.get("signature") else {
            continue;
        };
        let mut groups: BTreeMap<(String, String), Vec<serde_json::Value>> = BTreeMap::new();
        let mut add = |role: &str,
                       expression: &str,
                       index: Option<usize>,
                       name: Option<String>,
                       declaration: &str| {
            for target in named_types(expression) {
                groups.entry((role.into(), target)).or_default().push(serde_json::json!({
                    "type_expression": expression, "parameter_index": index, "parameter_name": name,
                    "type_line": symbol.start_line, "declaration": declaration,
                }));
            }
        };
        if let Some(return_type) = meta.get("return_type") {
            add("returns_type", return_type, None, None, return_type);
        }
        if let Some(params) = parameter_list(signature) {
            for (index, parameter) in split_top_level(params, ',').into_iter().enumerate() {
                let parameter = split_top_level(parameter, '=')[0];
                let lower = parameter.to_ascii_lowercase();
                if let Some(offset) = lower.find(" as ") {
                    let lhs = parameter[..offset].trim();
                    let name = lhs
                        .split_whitespace()
                        .last()
                        .unwrap_or("")
                        .trim_end_matches("()");
                    let mut expression = parameter[offset + 4..].trim().to_string();
                    if lhs.ends_with("()") {
                        expression.push_str("()");
                    }
                    if !name.is_empty() && !expression.is_empty() {
                        add(
                            "parameter_type",
                            &expression,
                            Some(index),
                            Some(name.into()),
                            parameter,
                        );
                    }
                }
            }
        }
        for ((kind, target_name), uses) in groups {
            // The degraded parser does not prove aliases, namespace scope or
            // generic parameter identity. Keep references explicitly unbound.
            edges.push(ExtractedEdge {
                source_name: symbol.name.clone(),
                source_kind: symbol.kind.clone(),
                source_start_line: symbol.start_line,
                source_language: "vb".into(),
                target_name,
                target_kind: Some("signature_type".into()),
                target_start_line: None,
                kind,
                metadata: Some(HashMap::from([
                    (
                        "type_uses".into(),
                        serde_json::to_string(&uses).expect("type uses"),
                    ),
                    ("type_reference_kind".into(), "fallback_unresolved".into()),
                    ("type_evidence".into(), "authored_syntax_fallback".into()),
                    ("extraction_fallback".into(), "true".into()),
                ])),
            });
        }
    }
    edges
}

#[cfg(test)]
mod tests {
    #[test]
    fn fallback_signature_types_keep_multiline_generics_defaults_and_repeated_parameters() {
        let source = "Public Class Service\n Public Function Run(\n first As Request(),\n second As Request,\n Optional label As String = \"fake As Wrong, extra\") As List(Of Response)\n Return Nothing\n End Function\nEnd Class";
        let (_, edges) = crate::vb_extractor::fallback_extract_vb_for_test(
            std::path::Path::new("Service.vb"),
            source,
        );
        let incoming = edges
            .iter()
            .find(|e| e.kind == "parameter_type" && e.target_name == "Request")
            .unwrap();
        let uses: Vec<serde_json::Value> =
            serde_json::from_str(&incoming.metadata.as_ref().unwrap()["type_uses"]).unwrap();
        assert_eq!(uses.len(), 2);
        assert_eq!(uses[0]["type_expression"], "Request()");
        assert_eq!(uses[1]["parameter_name"], "second");
        assert_eq!(uses[1]["parameter_index"], 1);
        assert!(
            edges
                .iter()
                .any(|e| e.kind == "returns_type" && e.target_name == "Response")
        );
        assert!(
            !edges
                .iter()
                .any(|e| e.target_name == "Wrong" || e.target_name == "extra")
        );
    }
}
