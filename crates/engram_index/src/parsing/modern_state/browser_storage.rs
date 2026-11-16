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
