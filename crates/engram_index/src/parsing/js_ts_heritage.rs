//! Declared JS/TS type relationships, without evaluating base expressions.
use super::ExtractedEdge;
use std::collections::HashMap;
use tree_sitter::Node;

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).filter(|child| child.kind() != "comment").collect()
}

fn static_name(node: Node<'_>, source: &str) -> Option<String> {
    if node.has_error() || node.is_missing() { return None; }
    match node.kind() {
        "identifier" | "type_identifier" | "property_identifier" => Some(source[node.byte_range()].into()),
        "generic_type" => static_name(node.child_by_field_name("name")?, source),
        "member_expression" | "nested_identifier" | "nested_type_identifier" => {
            if node.child_by_field_name("optional_chain").is_some() { return None; }
            let parts: Option<Vec<_>> = children(node).into_iter().map(|child| static_name(child, source)).collect();
            let parts = parts?;
            (parts.len() >= 2).then(|| parts.join("."))
        }
        _ => None,
    }
}

pub(super) fn extract(
    declaration: Node<'_>, source: &str, language: &str, owner: &str, declaration_line: u32,
) -> (Vec<ExtractedEdge>, Vec<String>) {
    if !matches!(declaration.kind(), "class_declaration" | "abstract_class_declaration" | "interface_declaration") {
        return (vec![], vec![]);
    }
    let mut candidates = Vec::new();
    for child in children(declaration) {
        match child.kind() {
            "class_heritage" => {
                for clause in children(child) {
                    match clause.kind() {
                        "extends_clause" | "implements_clause" => {
                            let kind = if clause.kind() == "implements_clause" { "implements_interface" } else { "inherits_from" };
                            candidates.extend(children(clause).into_iter()
                                .filter(|base| base.kind() != "type_arguments").map(|base| (base, kind)));
                        }
                        _ => candidates.push((clause, "inherits_from")),
                    }
                }
            }
            "extends_type_clause" => candidates.extend(children(child).into_iter().map(|base| (base, "inherits_from"))),
            _ => (),
        }
    }
    let mut edges = Vec::new();
    let mut unsupported = Vec::new();
    for (base, kind) in candidates {
        let end = base.next_named_sibling().filter(|next| next.kind() == "type_arguments")
            .map_or(base.end_byte(), |arguments| arguments.end_byte());
        let spelling = source[base.start_byte()..end].to_owned();
        let Some(target) = static_name(base, source) else {
            unsupported.push(spelling);
            continue;
        };
        edges.push(ExtractedEdge {
            source_name: owner.into(), source_kind: "class".into(), source_start_line: declaration_line,
            source_language: language.into(), target_name: target, target_kind: Some("class".into()),
            target_start_line: None, kind: kind.into(), metadata: Some(HashMap::from([
                ("heritage_expression".into(), spelling),
                ("heritage_line".into(), (base.start_position().row + 1).to_string()),
                ("heritage_column".into(), (base.start_position().column + 1).to_string()),
                ("relationship_evidence".into(), "declared_syntax; generic arguments are not target identity; binding and runtime dispatch are not proven".into()),
            ])),
        });
    }
    (edges, unsupported)
}

#[cfg(test)]
mod tests {
    use super::super::SymbolExtractor;
    use std::path::Path;

    #[test]
    fn javascript_inheritance_keeps_qualified_base_and_source_location() {
        for path in ["app.js", "view.jsx"] {
            let source = "// class Fake extends Wrong {}\nclass Page extends ui.base.Page { render() { return 1; } }\n";
            let (symbols, edges) = SymbolExtractor::new().extract(Path::new(path), source);
            let inheritance: Vec<_> = edges.iter().filter(|edge| edge.kind == "inherits_from").collect();
            assert_eq!(inheritance.len(), 1);
            let edge = inheritance[0];
            assert_eq!(edge.source_name, "Page");
            assert_eq!(edge.source_start_line, 2);
            assert_eq!(edge.target_name, "ui.base.Page");
            assert_eq!(edge.metadata.as_ref().unwrap()["heritage_line"], "2");
            assert_eq!(edge.metadata.as_ref().unwrap()["heritage_expression"], "ui.base.Page");
            assert!(symbols.iter().any(|symbol| symbol.name == "render" && symbol.kind == "function"));
        }
    }

    #[test]
    fn typed_extends_and_implements_preserve_each_declared_relationship() {
        let source = "namespace View {\n interface Readable<T> {}\n interface Named {}\n interface Child<T> extends Readable<T>, ui.Named {}\n abstract class Page<T> extends ui.Base<T> implements Readable<T>, Named {}\n}\n";
        for path in ["view.ts", "view.tsx"] {
            let (symbols, edges) = SymbolExtractor::new().extract(Path::new(path), source);
            let rows: Vec<_> = edges.iter().filter(|edge| matches!(edge.kind.as_str(), "inherits_from" | "implements_interface"))
                .map(|edge| (edge.source_name.as_str(), edge.kind.as_str(), edge.target_name.as_str())).collect();
            assert_eq!(rows.len(), 5);
            for expected in [("View.Child", "inherits_from", "Readable"), ("View.Child", "inherits_from", "ui.Named"),
                ("View.Page", "inherits_from", "ui.Base"), ("View.Page", "implements_interface", "Readable"),
                ("View.Page", "implements_interface", "Named")] {
                assert!(rows.contains(&expected), "Missing authored relationship {expected:?}: {rows:?}");
            }
            assert!(symbols.iter().any(|symbol| symbol.name == "Page" && symbol.kind == "class"));
            let base = edges.iter().find(|edge| edge.source_name == "View.Page" && edge.kind == "inherits_from").unwrap();
            assert_eq!(base.metadata.as_ref().unwrap()["heritage_expression"], "ui.Base<T>");
        }
    }

    #[test]
    fn computed_bases_remain_unresolved_instead_of_becoming_false_type_edges() {
        let source = "class Mixed extends withFeature(Base) {}\nclass Indexed extends registry[choice] {}\n";
        for path in ["app.js", "app.ts"] {
            let (symbols, edges) = SymbolExtractor::new().extract(Path::new(path), source);
            assert!(!edges.iter().any(|edge| edge.kind == "inherits_from"));
            for (name, expression) in [("Mixed", "withFeature(Base)"), ("Indexed", "registry[choice]")] {
                let symbol = symbols.iter().find(|symbol| symbol.name == name && symbol.kind == "class").unwrap();
                let pending: Vec<String> = serde_json::from_str(&symbol.metadata.as_ref().unwrap()["heritage_unresolved"]).unwrap();
                assert_eq!(pending, [expression]);
            }
        }
    }
}
