use super::*;

pub(super) fn record(
    bindings: &Bindings<'_, '_>,
    root: Node<'_>,
    node: Node<'_>,
    base: &str,
    suffix: &str,
    category: Option<&str>,
    access: (bool, bool),
    out: &mut Output<'_>,
) -> bool {
    let Some(script) = bindings.script else {
        return false;
    };
    if script.state_binding_shadowed(node, base, root, true)
        || script
            .facts
            .declarations
            .iter()
            .any(|declaration| declaration.local == base)
    {
        return false;
    }
    let imports = script
        .facts
        .imports
        .iter()
        .filter(|import| import.local == base)
        .collect::<Vec<_>>();
    if imports.len() != 1 || imports[0].type_only {
        return false;
    }
    let import = imports[0];
    if out.references.len() >= 4096 {
        out.issues.insert("state_reference_budget_exceeded".into());
        return true;
    }
    let (imported, suffix) = if import.imported == "*" {
        let Some(rest) = suffix.strip_prefix('.') else {
            return false;
        };
        let split = rest.find(['.', '[']).unwrap_or(rest.len());
        if split == 0 {
            return false;
        }
        (rest[..split].to_owned(), rest[split..].to_owned())
    } else {
        (import.imported.clone(), suffix.to_owned())
    };
    if suffix.is_empty() && access.1 && category.is_none() {
        out.issues.insert(format!(
            "assignment_to_readonly_import:{}",
            node.start_position().row + 1
        ));
        return true;
    }
    let (source_name, source_kind, source_line) = out.owner(node);
    out.references.push(engram_core::modern_state::Reference {
        source_name,
        source_kind,
        source_line: source_line.unwrap_or(0),
        site: typescript_modules::site(node),
        import: import.clone(),
        imported,
        suffix,
        category: category.map(str::to_owned),
        reads: access.0,
        writes: access.1,
        unbound_reason: None,
    });
    true
}

pub(super) fn extract(
    bindings: &Bindings<'_, '_>,
    nodes: &[Node<'_>],
    root: Node<'_>,
    out: &mut Output<'_>,
) {
    if bindings.script.is_none() {
        return;
    }
    for &node in nodes {
        if node.kind() != "identifier" && !member(node) {
            continue;
        }
        if node.parent().is_some_and(|parent| {
            member(parent)
                || (parent.kind() == "call_expression"
                    && field(parent, &["function"]).is_some_and(|callee| callee.id() == node.id()))
        }) {
            continue;
        }
        let mut parent = node.parent();
        let mut binding = false;
        while let Some(at) = parent {
            if matches!(
                at.kind(),
                "import_statement"
                    | "export_specifier"
                    | "formal_parameters"
                    | "type_annotation"
                    | "type_arguments"
            ) {
                binding = true;
                break;
            }
            if matches!(
                at.kind(),
                "statement_block" | "program" | "function_declaration" | "arrow_function"
            ) {
                break;
            }
            if field(at, &["name"]).is_some_and(|name| contains(name, node)) {
                binding = true;
                break;
            }
            parent = at.parent();
        }
        if binding {
            continue;
        }
        if let Some((base, suffix)) = access_path(node, out.source) {
            record(
                bindings,
                root,
                node,
                &base,
                &suffix,
                None,
                direction(node),
                out,
            );
        }
    }
}
