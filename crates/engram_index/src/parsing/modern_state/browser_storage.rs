//! Browser storage keys share a project-local identity, with no origin or tab claim.
use super::*;

pub(super) fn extract(
    bindings: &Bindings<'_, '_>,
    nodes: &[Node<'_>],
    root: Node<'_>,
    out: &mut Output<'_>,
) {
    let Some(script) = bindings.script else {
        return;
    };
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
            Some(key) => out.emit(node, category, &key, access.0, access.1),
            None => {
                out.issues.insert(format!(
                    "browser_storage_dynamic_key_unresolved:{category}:{}",
                    node.start_position().row + 1
                ));
            }
        }
    }
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
    out.emit(node, category, &key, reads, writes);
}
