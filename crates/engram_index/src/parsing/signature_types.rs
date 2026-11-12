//! Authored callable type contracts. No inference from bodies or call sites.
use super::ExtractedEdge;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use tree_sitter::Node;

mod vb;
pub(crate) use vb::fallback_edges as vb_fallback_edges;

fn text<'s>(node: Node<'_>, source: &'s str) -> &'s str {
    &source[node.byte_range()]
}

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

fn field<'a>(node: Node<'a>, names: &[&str]) -> Option<Node<'a>> {
    names.iter().find_map(|name| node.child_by_field_name(name))
}

fn type_parameters(mut node: Node<'_>, source: &str) -> BTreeSet<String> {
    fn first_name(node: Node<'_>) -> Option<Node<'_>> {
        if matches!(node.kind(), "type_identifier" | "identifier") {
            return Some(node);
        }
        // Python's PEP695 parameter list wraps each name in a `type` and
        // possibly `constrained_type`/`splat_type`. The first child is the
        // declared name, not its constraint/default.
        if matches!(node.kind(), "type" | "constrained_type" | "splat_type") {
            return node.named_child(0).and_then(first_name);
        }
        children(node)
            .into_iter()
            .find(|n| matches!(n.kind(), "type_identifier" | "identifier"))
    }
    let mut result = BTreeSet::new();
    loop {
        if let Some(list) = field(node, &["type_parameters"]).or_else(|| {
            (node.kind() == "template_declaration")
                .then(|| field(node, &["parameters"]))
                .flatten()
        }) {
            for parameter in children(list) {
                let mut cursor = parameter.walk();
                let mut names: Vec<_> = parameter
                    .children_by_field_name("name", &mut cursor)
                    .collect();
                if names.is_empty() {
                    names.extend(first_name(parameter));
                }
                // Go permits multiple names in one constrained declaration.
                for name in names {
                    result.insert(text(name, source).to_owned());
                }
            }
        }
        match node.parent() {
            Some(parent) => node = parent,
            None => break,
        }
    }
    result
}

// A reference is taken from a type subtree, never from a declaration's body,
// parameter default, annotation decorator or member/property name.
fn references(node: Node<'_>, source: &str, refs: &mut BTreeSet<(String, &'static str)>) {
    fn generic_arguments(
        node: Node<'_>,
        source: &str,
        refs: &mut BTreeSet<(String, &'static str)>,
    ) {
        if matches!(
            node.kind(),
            "type_arguments" | "type_argument_list" | "template_argument_list"
        ) {
            for argument in children(node) {
                references(argument, source, refs);
            }
        } else {
            for child in children(node) {
                generic_arguments(child, source, refs);
            }
        }
    }
    let kind = node.kind();
    if matches!(
        kind,
        "array_type" | "slice_type" | "pointer_type" | "reference_type" | "nullable_type"
    ) && let Some(element) = field(node, &["element", "type"])
    {
        // Array dimensions may contain identifiers/expressions. They are
        // preserved in type_expression but cannot become type references.
        references(element, source, refs);
        return;
    }
    if matches!(
        kind,
        "comment"
            | "line_comment"
            | "block_comment"
            | "annotation"
            | "marker_annotation"
            | "attribute_list"
            | "lifetime"
            | "type_qualifier"
            | "mutable_specifier"
            | "dimensions"
            | "integer_literal"
            | "literal_type"
            | "type_query"
            | "type_predicate"
            | "const"
            | "expression_statement"
    ) {
        return;
    }
    if matches!(
        kind,
        "predefined_type"
            | "primitive_type"
            | "integral_type"
            | "floating_point_type"
            | "boolean_type"
            | "void_type"
            | "none"
            | "unit_type"
    ) {
        refs.insert((text(node, source).to_owned(), "builtin"));
        return;
    }
    if matches!(
        kind,
        "qualified_name"
            | "alias_qualified_name"
            | "scoped_type_identifier"
            | "scoped_identifier"
            | "nested_type_identifier"
            | "qualified_type"
            | "attribute"
            | "qualified_identifier"
    ) {
        // Preserve qualification. Generic arguments get their own references;
        // a constructed qualifier is retained as an expression, never guessed.
        let raw = text(node, source);
        if !raw.contains(['<', '>', '[', ']']) {
            refs.insert((raw.to_owned(), "named"));
            return;
        }
        refs.insert((raw.to_owned(), "expression"));
        // Visiting generic_name itself would also emit its bare identifier,
        // dropping the qualifier and allowing an unrelated local type match.
        generic_arguments(node, source, refs);
        return;
    }
    if matches!(
        kind,
        "type_identifier" | "identifier" | "package_identifier"
    ) {
        refs.insert((text(node, source).to_owned(), "named"));
        return;
    }
    if matches!(
        kind,
        "property_signature"
            | "tuple_element"
            | "parameter"
            | "required_parameter"
            | "optional_parameter"
            | "formal_parameter"
            | "typed_parameter"
            | "typed_default_parameter"
            | "parameter_declaration"
            | "variadic_parameter_declaration"
    ) {
        if let Some(ty) = field(node, &["type"]) {
            references(ty, source, refs);
        }
        return;
    }
    if matches!(kind, "function_type" | "function_pointer_type") {
        for name in ["parameters", "return_type", "type"] {
            if let Some(child) = field(node, &[name]) {
                references(child, source, refs);
            }
        }
        return;
    }
    if matches!(
        kind,
        "struct_specifier" | "enum_specifier" | "class_specifier"
    ) {
        if let Some(name) = field(node, &["name"]) {
            references(name, source, refs);
        }
        return;
    }
    // Python quoted forward references and unsupported forms remain visible as
    // the authored expression, without turning arbitrary string text into IDs.
    if matches!(kind, "string" | "concatenated_string") {
        refs.insert((text(node, source).to_owned(), "expression"));
        return;
    }
    for child in children(node) {
        references(child, source, refs);
    }
}

fn function_declarator(node: Node<'_>) -> Option<Node<'_>> {
    if node.kind() == "function_declarator" {
        return Some(node);
    }
    children(node).into_iter().find_map(|child| {
        if child.kind().contains("declarator") {
            function_declarator(child)
        } else {
            None
        }
    })
}

fn declarator_name(node: Node<'_>) -> Option<Node<'_>> {
    if matches!(node.kind(), "identifier" | "field_identifier") {
        return Some(node);
    }
    if let Some(child) = field(node, &["declarator", "name"]) {
        return declarator_name(child);
    }
    children(node).into_iter().find_map(|child| {
        if child.kind().contains("declarator") || child.kind() == "identifier" {
            declarator_name(child)
        } else {
            None
        }
    })
}

/// One edge per referenced type and role. `type_uses` retains every parameter
/// occurrence even when the graph key merges two parameters of the same type.
pub(super) fn extract(
    declaration: Node<'_>,
    source: &str,
    language: &str,
    owner: &str,
    line: u32,
) -> Vec<ExtractedEdge> {
    let callable = field(declaration, &["value"])
        .filter(|n| matches!(n.kind(), "arrow_function" | "function_expression"))
        .unwrap_or(declaration);
    let type_params = type_parameters(callable, source);
    let c_declarator = field(callable, &["declarator"]).and_then(function_declarator);
    let mut groups: BTreeMap<(String, String, String), Vec<serde_json::Value>> = BTreeMap::new();
    let mut add = |ty: Node<'_>,
                   role: &str,
                   index: Option<usize>,
                   name: Option<String>,
                   spelling: String,
                   declaration_text: String| {
        if ty.has_error() || ty.is_missing() {
            return;
        }
        let mut refs = BTreeSet::new();
        references(ty, source, &mut refs);
        if refs.is_empty() && !spelling.is_empty() {
            refs.insert((spelling.clone(), "expression"));
        }
        let occurrence = serde_json::json!({
            "type_expression": spelling, "parameter_index": index, "parameter_name": name,
            "type_line": ty.start_position().row + 1, "declaration": declaration_text,
        });
        for (reference, mut classification) in refs {
            if type_params.contains(&reference) {
                classification = "type_parameter";
            }
            // These are syntax keywords/types, not user declarations, even in
            // grammars which encode them as ordinary identifiers.
            if matches!(
                reference.as_str(),
                "void"
                    | "None"
                    | "any"
                    | "unknown"
                    | "never"
                    | "auto"
                    | "dynamic"
                    | "int"
                    | "str"
                    | "bool"
                    | "float"
                    | "string"
                    | "object"
            ) {
                classification = "builtin";
            }
            let uses = groups
                .entry((role.into(), reference, classification.into()))
                .or_default();
            if !uses.contains(&occurrence) {
                uses.push(occurrence.clone());
            }
        }
    };

    if let Some(mut ty) = field(callable, &["return_type", "returns", "result", "type"]) {
        // TypeScript annotations include the colon; it isn't part of the type.
        if ty.kind() == "type_annotation" {
            if let Some(inner) = children(ty).into_iter().next() {
                ty = inner;
            }
        }
        if ty.kind() == "parameter_list" {
            // Go named/multiple results; result positions have their own uses.
            let mut index = 0;
            for result in children(ty) {
                if let Some(result_type) = field(result, &["type"]) {
                    let names: Vec<_> = children(result)
                        .into_iter()
                        .filter(|n| n.kind() == "identifier")
                        .collect();
                    for name in names.iter().map(Some).chain(if names.is_empty() {
                        Some(None)
                    } else {
                        None
                    }) {
                        add(
                            result_type,
                            "returns_type",
                            Some(index),
                            name.map(|n| text(*n, source).into()),
                            text(result_type, source).into(),
                            text(result, source).into(),
                        );
                        index += 1;
                    }
                }
            }
        } else {
            let mut spelling = text(ty, source).to_string();
            if let Some(declarator) = c_declarator {
                if let Some(trailing) = children(declarator)
                    .into_iter()
                    .find(|n| n.kind() == "trailing_return_type")
                {
                    if let Some(actual) = children(trailing).into_iter().next() {
                        ty = actual;
                        spelling = text(actual, source).into();
                    }
                } else if ty.end_byte() <= declarator.start_byte() {
                    spelling = source[ty.start_byte()..declarator.start_byte()]
                        .trim()
                        .into();
                }
            }
            add(
                ty,
                "returns_type",
                None,
                None,
                spelling,
                text(ty, source).into(),
            );
        }
    }
    if let Some(params) = field(callable, &["parameters", "parameter_list"])
        .or_else(|| c_declarator.and_then(|n| field(n, &["parameters"])))
    {
        let mut index = 0;
        let array_type = field(params, &["type"]);
        let array_name = field(params, &["name"]);
        for param in children(params) {
            if param.kind().contains("comment")
                || matches!(param.kind(), "positional_separator" | "keyword_separator")
            {
                continue;
            }
            if Some(param) == array_type || Some(param) == array_name {
                continue;
            }
            let mut ty = field(param, &["type"]);
            // Java varargs have an unfielded type and variable_declarator.
            if param.kind() == "spread_parameter" {
                ty = children(param).into_iter().find(|n| {
                    !matches!(
                        n.kind(),
                        "modifiers" | "annotation" | "marker_annotation" | "variable_declarator"
                    )
                });
            }
            if let Some(mut ty) = ty {
                if ty.kind() == "type_annotation" {
                    if let Some(inner) = children(ty).into_iter().next() {
                        ty = inner;
                    }
                }
                let mut names: Vec<_> = if language == "go" {
                    children(param)
                        .into_iter()
                        .filter(|n| n.kind() == "identifier")
                        .collect()
                } else {
                    field(param, &["name", "pattern"])
                        .or_else(|| field(param, &["declarator"]).and_then(declarator_name))
                        .or_else(|| {
                            children(param)
                                .into_iter()
                                .find(|n| n.kind() == "identifier")
                        })
                        .into_iter()
                        .collect()
                };
                if param.kind() == "spread_parameter" {
                    names = children(param)
                        .into_iter()
                        .find(|n| n.kind() == "variable_declarator")
                        .and_then(declarator_name)
                        .into_iter()
                        .collect();
                }
                let mut spelling = text(ty, source).to_string();
                if c_declarator.is_some() {
                    if let Some(decl) = field(param, &["declarator"]) {
                        let raw = text(decl, source);
                        let decoration = if let Some(name) = declarator_name(decl) {
                            let offset = name.start_byte() - decl.start_byte();
                            format!(
                                "{}{}",
                                &raw[..offset],
                                &raw[offset + name.byte_range().len()..]
                            )
                        } else {
                            raw.into()
                        };
                        spelling.push_str(decoration.trim());
                    }
                } else if let Some(dimensions) = field(param, &["dimensions"]) {
                    spelling.push_str(text(dimensions, source));
                }
                if param.kind() == "spread_parameter"
                    || param.kind() == "variadic_parameter_declaration"
                {
                    spelling.push_str("...");
                }
                for name in
                    names
                        .iter()
                        .map(Some)
                        .chain(if names.is_empty() { Some(None) } else { None })
                {
                    add(
                        ty,
                        "parameter_type",
                        Some(index),
                        name.map(|n| text(*n, source).into()),
                        spelling.clone(),
                        text(param, source).into(),
                    );
                    index += 1;
                }
            } else {
                index += 1;
            }
        }
        // The pinned C# grammar exposes the trailing `params` array on the
        // list itself, rather than wrapping it in an ordinary parameter node.
        if let (Some(ty), Some(name)) = (array_type, array_name) {
            add(
                ty,
                "parameter_type",
                Some(index),
                Some(text(name, source).into()),
                text(ty, source).into(),
                format!("params {} {}", text(ty, source), text(name, source)),
            );
        }
    }
    groups
        .into_iter()
        .map(
            |((kind, target_name, reference_kind), uses)| ExtractedEdge {
                source_name: owner.into(),
                source_kind: "function".into(),
                source_start_line: line,
                source_language: language.into(),
                target_name,
                target_kind: Some("signature_type".into()),
                target_start_line: None,
                kind,
                metadata: Some(HashMap::from([
                    (
                        "type_uses".into(),
                        serde_json::to_string(&uses).expect("serializable type uses"),
                    ),
                    ("type_reference_kind".into(), reference_kind),
                    ("type_evidence".into(), "authored_syntax".into()),
                ])),
            },
        )
        .collect()
}

#[cfg(test)]
mod tests;
