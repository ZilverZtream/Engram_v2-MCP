//! MiniLang declaration parsing: Namespace, Function/Sub, Type, Enum,
//! Interface, Const, Include, Declare, Extern.

use crate::parsing::{ExtractedEdge, ExtractedSymbol};
use std::collections::HashMap;

/// Parse the declaration a block-opening line introduces.
///
/// Returns `(fqn, symbol_index)`. The FQN is empty for control-flow blocks,
/// which contribute no naming scope. The symbol index lets the scanner
/// backfill `end_line` when the block closes.
pub(crate) fn open_declaration(
    keyword: &str,
    trimmed: &str,
    parent_fqn: &str,
    line_no: u32,
    symbols: &mut Vec<ExtractedSymbol>,
    edges: &mut Vec<ExtractedEdge>,
) -> (String, Option<usize>) {
    let _ = edges;
    match keyword {
        "Namespace" => {
            let Some(name) = declaration_name(trimmed, "Namespace") else {
                return (String::new(), None);
            };
            let fqn = if parent_fqn.is_empty() {
                name.to_string()
            } else {
                format!("{parent_fqn}.{name}")
            };
            symbols.push(ExtractedSymbol {
                name: fqn.clone(),
                kind: "namespace".to_string(),
                start_line: line_no,
                end_line: 0,
                metadata: None,
            });
            (fqn, Some(symbols.len() - 1))
        }
        "Function" | "Sub" => {
            let Some(name) = declaration_name(trimmed, keyword) else {
                return (String::new(), None);
            };
            let fqn = if parent_fqn.is_empty() {
                name.clone()
            } else {
                format!("{parent_fqn}.{name}")
            };
            symbols.push(ExtractedSymbol {
                name: fqn.clone(),
                kind: "function".to_string(),
                start_line: line_no,
                end_line: 0,
                metadata: meta(&[("is_sub", (keyword == "Sub").to_string())]),
            });
            (fqn, Some(symbols.len() - 1))
        }
        _ => (String::new(), None),
    }
}

/// The declared name on a `Function`/`Sub`/`Type`/`Enum`/`Interface` line.
///
/// The name is captured independently of what follows it, because MiniLang
/// puts generic parameters BETWEEN the name and the parameter list:
/// `Function BTreeMap_Get Of K, V(Borrow tree As …)`. A pattern demanding
/// `name(` would miss every generic declaration in the standard library.
pub(crate) fn declaration_name(trimmed: &str, keyword: &str) -> Option<String> {
    let mut rest = trimmed;
    for modifier in ["Public ", "Private "] {
        if let Some(r) = rest.strip_prefix(modifier) {
            rest = r.trim_start();
        }
    }
    let rest = rest.strip_prefix(keyword)?.trim_start();
    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() { None } else { Some(name) }
}

/// Build a metadata map, skipping empty values so the graph does not carry
/// noise keys.
pub(crate) fn meta(pairs: &[(&str, String)]) -> Option<HashMap<String, String>> {
    let map: HashMap<String, String> = pairs
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| ((*k).to_string(), v.clone()))
        .collect();
    if map.is_empty() { None } else { Some(map) }
}
