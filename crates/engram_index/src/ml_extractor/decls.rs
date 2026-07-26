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
    body: &[&str],
    symbols: &mut Vec<ExtractedSymbol>,
    edges: &mut Vec<ExtractedEdge>,
) -> (String, Option<usize>) {
    match keyword {
        "Namespace" => {
            let Some(name) = declaration_name(trimmed, "Namespace") else {
                return (String::new(), None);
            };
            let fqn = qualify(parent_fqn, &name);
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
            let fqn = qualify(parent_fqn, &name);
            symbols.push(ExtractedSymbol {
                name: fqn.clone(),
                kind: "function".to_string(),
                start_line: line_no,
                end_line: 0,
                metadata: meta(&[
                    ("is_sub", (keyword == "Sub").to_string()),
                    ("generic_params", generic_params(trimmed)),
                    ("access", access_modifier(trimmed)),
                ]),
            });
            (fqn, Some(symbols.len() - 1))
        }
        "Type" => {
            let Some(name) = declaration_name(trimmed, keyword) else {
                return (String::new(), None);
            };
            let fqn = qualify(parent_fqn, &name);

            // A row shaped `Name As Type` is a struct field; `Name(…)` or a
            // bare `Name` is a union variant. A `Type` is a union only when
            // it has at least one variant-shaped row and NO field rows; any
            // field row makes it a struct. Mixed bodies (both field and
            // variant rows) never occur in valid MiniLang — the corpus has
            // zero such cases — so this is a defined fallback, not a
            // dominance/majority rule: a single field row forces
            // `kind: "struct"` even alongside variant-shaped rows, and any
            // such rows are still recorded in the `variants` metadata.
            let mut fields: Vec<String> = Vec::new();
            let mut variants: Vec<String> = Vec::new();
            for row in body {
                if let Some((lhs, rhs)) = row.split_once(" As ") {
                    if !lhs.contains('(') {
                        let ty = rhs.trim();
                        // Strong `Ref(Of T)` fields can form ownership
                        // cycles; `Weak(Of T)` is the documented break edge.
                        let strength = if ty.starts_with("Weak(") {
                            "weak"
                        } else if ty.starts_with("Ref(") {
                            "strong"
                        } else {
                            ""
                        };
                        if strength.is_empty() {
                            fields.push(format!("{}:{}", lhs.trim(), ty));
                        } else {
                            fields.push(format!("{}:{}:{}", lhs.trim(), ty, strength));
                        }
                        continue;
                    }
                }
                let vname: String = row
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if vname.is_empty() {
                    continue;
                }
                let arity = match row.split_once('(') {
                    Some((_, args)) => {
                        let args = args.trim_end_matches(')').trim();
                        if args.is_empty() {
                            0
                        } else {
                            args.matches(',').count() + 1
                        }
                    }
                    None => 0,
                };
                variants.push(format!("{vname}/{arity}"));
            }

            let is_union = !variants.is_empty() && fields.is_empty();
            let kind = if is_union { "union" } else { "struct" };

            if let Some(iface) = implements_target(trimmed) {
                edges.push(ExtractedEdge {
                    source_name: fqn.clone(),
                    source_kind: kind.to_string(),
                    source_start_line: line_no,
                    source_language: "ml".to_string(),
                    target_name: iface,
                    target_kind: Some("interface".to_string()),
                    target_start_line: None,
                    kind: "implements_interface".to_string(),
                    metadata: None,
                });
            }

            symbols.push(ExtractedSymbol {
                name: fqn.clone(),
                kind: kind.to_string(),
                start_line: line_no,
                end_line: 0,
                metadata: meta(&[
                    ("fields", fields.join("||")),
                    ("variants", variants.join("||")),
                    ("generic_params", generic_params(trimmed)),
                    ("access", access_modifier(trimmed)),
                ]),
            });
            (fqn, Some(symbols.len() - 1))
        }
        "Enum" => {
            let Some(name) = declaration_name(trimmed, keyword) else {
                return (String::new(), None);
            };
            let fqn = qualify(parent_fqn, &name);
            let members: Vec<String> = body
                .iter()
                .map(|row| row.split_whitespace().collect::<Vec<_>>().join(""))
                .filter(|row| !row.is_empty())
                .collect();
            symbols.push(ExtractedSymbol {
                name: fqn.clone(),
                kind: "enum".to_string(),
                start_line: line_no,
                end_line: 0,
                metadata: meta(&[("members", members.join("||"))]),
            });
            (fqn, Some(symbols.len() - 1))
        }
        "Interface" => {
            let Some(name) = declaration_name(trimmed, keyword) else {
                return (String::new(), None);
            };
            let fqn = qualify(parent_fqn, &name);
            symbols.push(ExtractedSymbol {
                name: fqn.clone(),
                kind: "interface".to_string(),
                start_line: line_no,
                end_line: 0,
                metadata: meta(&[("members", body.join("||"))]),
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

/// Join a parent scope and a local name into an FQN.
pub(crate) fn qualify(parent_fqn: &str, name: &str) -> String {
    if parent_fqn.is_empty() {
        name.to_string()
    } else {
        format!("{parent_fqn}.{name}")
    }
}

/// `Public`/`Private` on a declaration line, or empty.
pub(crate) fn access_modifier(trimmed: &str) -> String {
    for modifier in ["Public", "Private"] {
        if trimmed.starts_with(modifier) && trimmed[modifier.len()..].starts_with(' ') {
            return modifier.to_string();
        }
    }
    String::new()
}

/// The interface named by an `Implements X` clause, or `None`.
pub(crate) fn implements_target(trimmed: &str) -> Option<String> {
    let (_, rest) = trimmed.split_once(" Implements ")?;
    let name: String = rest
        .trim()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '.')
        .collect();
    if name.is_empty() { None } else { Some(name) }
}

/// Generic parameters from an `Of T As Constraint, U` clause, rendered as
/// `T:Constraint||U`. Empty when the declaration is not generic.
///
/// MiniLang places this clause BETWEEN the declared name and the parameter
/// list, so it must be parsed from the segment starting at ` Of ` and
/// ending at the first `(` that follows it.
pub(crate) fn generic_params(trimmed: &str) -> String {
    let Some(idx) = trimmed.find(" Of ") else {
        return String::new();
    };
    let after = &trimmed[idx + 4..];
    let clause = match after.find('(') {
        Some(p) => &after[..p],
        None => after,
    };
    let clause = match clause.find(" Implements ") {
        Some(p) => &clause[..p],
        None => clause,
    };
    clause
        .split(',')
        .filter_map(|part| {
            let part = part.trim();
            if part.is_empty() {
                return None;
            }
            Some(match part.split_once(" As ") {
                Some((n, c)) => format!("{}:{}", n.trim(), c.trim()),
                None => part.to_string(),
            })
        })
        .collect::<Vec<_>>()
        .join("||")
}
