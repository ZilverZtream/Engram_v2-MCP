//! Browser storage keys share a project-local identity, with no origin or tab claim.
use super::*;

const MAX_SITES_PER_RELATION: usize = 8;

fn emit_access(
    out: &mut Output<'_>,
    node: Node<'_>,
    category: &str,
    key: &str,
    operation: &str,
    (reads, writes): (bool, bool),
) {
    let first = out.edges.len();
    out.emit(node, category, key, reads, writes);
    for edge in &mut out.edges[first..] {
        if let Some(metadata) = edge.metadata.as_mut() {
            metadata.insert("browser_storage_operation".into(), operation.into());
        }
    }
}

/// Keep the graph's containing-symbol relationships and retain individual
/// access sites before repeated edges would overwrite one another.
fn retain_access_sites(out: &mut Output<'_>, first_edge: usize) {
    use serde_json::{Value, json};

    let edges = out.edges.split_off(first_edge);
    let mut positions = HashMap::new();
    let mut sites: Vec<(usize, Vec<Value>, usize)> = Vec::new();
    for edge in edges {
        let Some(metadata) = edge.metadata.as_ref() else {
            out.edges.push(edge);
            continue;
        };
        let Some(operation) = metadata.get("browser_storage_operation").filter(|op| *op != "clear") else {
            out.edges.push(edge);
            continue;
        };
        let key = (
            edge.source_name.clone(), edge.source_kind.clone(),
            metadata.get("source_declaration_line").cloned(),
            edge.target_name.clone(), edge.kind.clone(),
        );
        let group = *positions.entry(key).or_insert_with(|| {
            sites.push((out.edges.len(), Vec::new(), 0));
            out.edges.push(edge.clone());
            sites.len() - 1
        });
        let (index, observed, omitted) = &mut sites[group];
        if observed.len() < MAX_SITES_PER_RELATION {
            observed.push(json!({
                "operation": operation,
                "line": edge.source_start_line,
                "column": metadata.get("state_access_column").and_then(|value| value.parse::<usize>().ok()),
                "start_byte": metadata.get("state_access_byte").and_then(|value| value.parse::<usize>().ok()),
                "end_byte": metadata.get("state_access_end_byte").and_then(|value| value.parse::<usize>().ok()),
            }));
        } else {
            *omitted += 1;
        }
        // Preserve the legacy most recent representative and its span.
        out.edges[*index] = edge;
    }
    for (index, observed, omitted) in sites {
        let metadata = out.edges[index].metadata.get_or_insert_with(Default::default);
        metadata.insert("browser_storage_access_sites".into(), Value::Array(observed).to_string());
        metadata.insert("browser_storage_access_sites_omitted".into(), omitted.to_string());
    }
}

pub(super) fn extract(
    bindings: &Bindings<'_, '_>,
    nodes: &[Node<'_>],
    root: Node<'_>,
    out: &mut Output<'_>,
) {
    let Some(script) = bindings.script else {
        return;
    };
    let first_edge = out.edges.len();
    for &node in nodes {
        if node.kind() != "call_expression" {
            extract_property(script, node, root, out);
            continue;
        }
        let Some(callee) = node.child_by_field_name("function") else {
            continue;
        };
        let Some((base, suffix)) = access_path(callee, out.source) else {
            continue;
        };
        let (storage, operation) = match base.as_str() {
            "localStorage" | "sessionStorage" => {
                (base.as_str(), suffix.strip_prefix('.').unwrap_or(""))
            }
            "window" | "globalThis" => {
                let Some((storage, operation)) =
                    suffix.strip_prefix('.').and_then(|s| s.split_once('.'))
                else {
                    continue;
                };
                (storage, operation)
            }
            _ => continue,
        };
        let category = match storage {
            "localStorage" => "LocalStorage",
            "sessionStorage" => "SessionStorage",
            _ => continue,
        };
        if script.state_binding_shadowed(node, &base, root, false) {
            continue;
        }
        let access = match operation {
            "getItem" => (true, false),
            "setItem" | "removeItem" => (false, true),
            "clear" => {
                let first_edge = out.edges.len();
                let first_symbol = out.symbols.len();
                // A separate identity cannot collide with a literal storage key
                // named '*'. Consumers join this store-wide candidate explicitly.
                out.emit(node, &format!("{category}ClearAll"), "*", false, true);
                for edge in &mut out.edges[first_edge..] {
                    if let Some(metadata) = edge.metadata.as_mut() {
                        metadata.insert("browser_storage_operation".into(), "clear".into());
                        metadata.insert("browser_storage_store".into(), category.into());
                    }
                }
                for symbol in &mut out.symbols[first_symbol..] {
                    if let Some(metadata) = symbol.metadata.as_mut() {
                        metadata.insert("browser_storage_operation".into(), "clear".into());
                        metadata.insert("browser_storage_store".into(), category.into());
                    }
                }
                continue;
            }
            _ => continue,
        };
        let key = node
            .child_by_field_name("arguments")
            .and_then(|arguments| arguments.named_child(0))
            .and_then(|argument| literal(argument, out.source));
        match key {
            Some(key) => emit_access(out, node, category, &key, operation, access),
            None => {
                out.issues.insert(format!(
                    "browser_storage_dynamic_key_unresolved:{category}:{}",
                    node.start_position().row + 1
                ));
            }
        }
    }
    retain_access_sites(out, first_edge);
}

fn property_key(node: Node<'_>, source: &str) -> Option<String> {
    match node.kind() {
        "member_expression" => {
            let property = node.child_by_field_name("property")?;
            (property.kind() == "property_identifier").then(|| text(property, source).into())
        }
        "subscript_expression" => literal(node.child_by_field_name("index")?, source),
        _ => None,
    }
}

fn storage_object(node: Node<'_>, source: &str) -> Option<(String, &'static str)> {
    let (base, storage) = if node.kind() == "identifier" {
        let name = text(node, source);
        (name, name.to_owned())
    } else {
        let object = node.child_by_field_name("object")?;
        if object.kind() != "identifier" {
            return None;
        }
        let base = text(object, source);
        if !matches!(base, "window" | "globalThis") {
            return None;
        }
        (base, property_key(node, source)?)
    };
    let category = match storage.as_str() {
        "localStorage" => "LocalStorage",
        "sessionStorage" => "SessionStorage",
        _ => return None,
    };
    Some((base.to_owned(), category))
}

fn extract_property(
    script: &typescript_modules::Extraction<'_>,
    node: Node<'_>,
    root: Node<'_>,
    out: &mut Output<'_>,
) {
    if !matches!(node.kind(), "member_expression" | "subscript_expression") {
        return;
    }
    let Some(object) = node.child_by_field_name("object") else {
        return;
    };
    let Some((base, category)) = storage_object(object, out.source) else {
        return;
    };
    if script.state_binding_shadowed(node, &base, root, false) {
        return;
    }
    let Some(key) = property_key(node, out.source) else {
        out.issues.insert(format!(
            "browser_storage_dynamic_property_unresolved:{category}:{}",
            node.start_position().row + 1
        ));
        return;
    };
    // These properties expose the Storage API itself rather than a named item.
    // Keys with these spellings remain available through getItem/setItem.
    if matches!(
        key.as_str(),
        "length" | "key" | "getItem" | "setItem" | "removeItem" | "clear"
    ) {
        return;
    }
    let deleted = node.parent().is_some_and(|parent| {
        parent.kind() == "unary_expression"
            && parent
                .child_by_field_name("operator")
                .is_some_and(|operator| text(operator, out.source) == "delete")
    });
    let (reads, writes) = if deleted {
        (false, true)
    } else {
        direction(node)
    };
    let operation = if deleted { "delete_property" } else { "property_access" };
    emit_access(out, node, category, &key, operation, (reads, writes));
}
