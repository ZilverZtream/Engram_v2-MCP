//! Class member names and branch counts from the source grammar.
use std::path::Path;
use tree_sitter::{Node, Parser};

pub struct Member {
    pub name: String,
    pub line: usize,
    pub declaration_line: usize,
    pub signature: String,
    pub return_type: Option<String>,
    pub public: bool,
    pub accessor: bool,
    pub decisions: u32,
}

fn decisions(body: Node<'_>) -> u32 {
    let mut count = 1;
    let mut stack = vec![body];
    while let Some(node) = stack.pop() {
        count += u32::from(matches!(
            node.kind(),
            "if_statement"
                | "for_statement"
                | "for_in_statement"
                | "while_statement"
                | "do_statement"
                | "catch_clause"
                | "switch_case"
                | "ternary_expression"
                | "&&"
                | "||"
                | "??"
        ));
        if node.kind() != "method_definition" {
            let mut c = node.walk();
            stack.extend(node.children(&mut c));
        }
    }
    count
}

pub fn members(path: &Path, source: &str) -> Vec<Member> {
    let language = match path.extension().and_then(|s| s.to_str()) {
        Some("ts" | "mts" | "cts") => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Some("tsx") => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Some("js" | "jsx" | "mjs" | "cjs") => tree_sitter_javascript::LANGUAGE.into(),
        _ => return Vec::new(),
    };
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        let mut c = node.walk();
        if node.kind() == "method_definition"
            && node.parent().is_some_and(|p| p.kind() == "class_body")
            && let Some(name) = node.child_by_field_name("name")
            && matches!(
                name.kind(),
                "property_identifier" | "private_property_identifier"
            )
            && let Ok(text) = name.utf8_text(bytes)
            && text != "constructor"
            && let Some(body) = node.child_by_field_name("body")
        {
            let public = !text.starts_with('#')
                && !node.children(&mut c).any(|n| {
                    n.kind() == "accessibility_modifier" && n.utf8_text(bytes) != Ok("public")
                });
            found.push(Member {
                name: text.to_owned(),
                line: name.start_position().row + 1,
                declaration_line: node.start_position().row + 1,
                signature: source[node.start_byte()..body.start_byte()].trim().to_owned(),
                return_type: node.child_by_field_name("return_type")
                    .and_then(|annotation| annotation.utf8_text(bytes).ok())
                    .map(|annotation| annotation.trim_start_matches(':').trim().to_owned()),
                public,
                accessor:node.children(&mut c).any(|v|matches!(v.kind(),"get"|"set")),
                decisions: decisions(body),
            });
        }
        stack.extend(node.named_children(&mut c));
    }
    found.sort_by_key(|m| m.line);
    found
}
