//! Rust source bindings and lexical call sites, without macro expansion.
use engram_core::rust_modules::{Binding, Call, Module, Site};
use tree_sitter::Node;
pub(super) mod config;

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut c = node.walk();
    node.named_children(&mut c).collect()
}
fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}
fn ident(value: &str) -> String {
    value.strip_prefix("r#").unwrap_or(value).into()
}
fn site(node: Node<'_>) -> Site {
    Site {
        offset: node.start_byte(),
        end_offset: node.end_byte(),
        line: node.start_position().row as u32 + 1,
        column: node.start_position().column as u32 + 1,
        end_line: node.end_position().row as u32 + 1,
    }
}
fn context(mut node: Node<'_>, source: &str) -> Vec<String> {
    let mut result = Vec::new();
    while let Some(parent) = node.parent() {
        if parent.kind() == "mod_item"
            && parent
                .child_by_field_name("body")
                .is_some_and(|b| b.start_byte() <= node.start_byte())
        {
            if let Some(name) = parent.child_by_field_name("name") {
                result.push(ident(text(name, source)));
            }
        }
        node = parent;
    }
    result.reverse();
    result
}
fn scope(mut node: Node<'_>) -> (usize, usize) {
    while let Some(parent) = node.parent() {
        if matches!(parent.kind(), "source_file" | "mod_item") {
            return (0, parent.end_byte());
        }
        if parent.kind() == "block" {
            return (parent.start_byte() + 1, parent.end_byte());
        }
        node = parent;
    }
    (0, node.end_byte())
}
fn path(node: Node<'_>, source: &str) -> Option<Vec<String>> {
    match node.kind() {
        "identifier" | "self" | "super" | "crate" => Some(vec![ident(text(node, source))]),
        "scoped_identifier" => {
            let mut result = if let Some(prefix) = node.child_by_field_name("path") {
                path(prefix, source)?
            } else {
                vec![String::new()]
            };
            result.push(ident(text(node.child_by_field_name("name")?, source)));
            Some(result)
        }
        "generic_function" => path(node.child_by_field_name("function")?, source),
        _ => None,
    }
}
fn bound_names(node: Node<'_>, source: &str) -> Vec<String> {
    match node.kind() {
        "identifier" => vec![ident(text(node, source))],
        "parameter" | "let_declaration" | "field_pattern" => {
            if let Some(pattern) = node.child_by_field_name("pattern") {
                bound_names(pattern, source)
            } else if node.kind() == "field_pattern" {
                node.child_by_field_name("name")
                    .map(|n| vec![ident(text(n, source))])
                    .unwrap_or_default()
            } else {
                vec![]
            }
        }
        "tuple_pattern" | "slice_pattern" | "reference_pattern" | "mut_pattern"
        | "captured_pattern" | "or_pattern" | "match_pattern" | "closure_parameters"
        | "parameters" => children(node)
            .into_iter()
            .flat_map(|n| bound_names(n, source))
            .collect(),
        "tuple_struct_pattern" | "struct_pattern" => children(node)
            .into_iter()
            .filter(|n| Some(n.id()) != node.child_by_field_name("type").map(|t| t.id()))
            .flat_map(|n| bound_names(n, source))
            .collect(),
        _ => vec![],
    }
}
fn attributes(node: Node<'_>, source: &str) -> (Option<String>, Option<String>) {
    let mut at = node.prev_named_sibling();
    let mut reason = None;
    let mut file = None;
    while let Some(n) = at {
        if matches!(n.kind(), "line_comment" | "block_comment") {
            at = n.prev_named_sibling();
            continue;
        }
        if n.kind() != "attribute_item" {
            break;
        }
        if let Some(attr) = n.named_child(0) {
            let name = attr.named_child(0).map(|n| text(n, source)).unwrap_or("");
            match name {
                "path" => {
                    file = attr
                        .child_by_field_name("value")
                        .and_then(|n| serde_json::from_str::<String>(text(n, source)).ok());
                    if file.is_none() {
                        reason = Some("unsupported_module_path_attribute".into());
                    }
                }
                "cfg" | "cfg_attr" | "test" => reason = Some("conditional_compilation".into()),
                "doc" | "allow" | "warn" | "deny" | "forbid" | "expect" | "deprecated"
                | "inline" | "cold" | "must_use" | "no_mangle" | "export_name" | "track_caller"
                | "derive" => {}
                _ => reason = Some("unexpanded_attribute".into()),
            }
        }
        at = n.prev_named_sibling();
    }
    (reason, file)
}
fn visibility(node: Node<'_>, source: &str) -> String {
    children(node)
        .into_iter()
        .find(|n| n.kind() == "visibility_modifier")
        .map(|n| {
            text(n, source)
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect()
        })
        .unwrap_or_default()
}
pub(super) struct Extraction {
    pub facts: Module,
    shadows: Vec<(String, usize, usize)>,
}
impl Extraction {
    pub fn new(root: Node<'_>, source: &str) -> Self {
        let mut this = Self {
            facts: Module {
                version: 1,
                bindings: vec![],
                calls: vec![],
                issues: vec![],
            },
            shadows: vec![],
        };
        if root.has_error() {
            this.facts.issues.push("parse_error".into());
        }
        let mut pending = vec![root];
        let mut visited = 0;
        while let Some(node) = pending.pop() {
            visited += 1;
            if visited > 200_000 {
                this.facts.issues.push("scope_budget_exceeded".into());
                break;
            }
            match node.kind() {
                "use_declaration" => {
                    if let Some(argument) = node.child_by_field_name("argument") {
                        this.use_tree(argument, vec![], node, source);
                    }
                }
                "mod_item" | "function_item" | "struct_item" | "enum_item" | "trait_item"
                | "type_item" | "const_item" | "static_item" => {
                    let mut ancestor = node.parent();
                    let mut associated = false;
                    while let Some(n) = ancestor {
                        if matches!(n.kind(), "impl_item" | "trait_item") {
                            associated = true;
                            break;
                        }
                        if matches!(n.kind(), "mod_item" | "source_file" | "block") {
                            break;
                        }
                        ancestor = n.parent();
                    }
                    if !associated {
                        if let Some(name) = node.child_by_field_name("name") {
                            let (reason, path_attribute) = attributes(node, source);
                            this.facts.bindings.push(Binding {
                                name: ident(text(name, source)),
                                symbol: text(name, source).into(),
                                kind: match node.kind() {
                                    "mod_item" => "module",
                                    "function_item" => "function",
                                    _ => "noncallable",
                                }
                                .into(),
                                context: context(node, source),
                                scope: scope(node).0,
                                path: vec![],
                                site: site(node),
                                visibility: visibility(node, source),
                                inline: node.kind() == "mod_item"
                                    && node.child_by_field_name("body").is_some(),
                                path_attribute,
                                reason,
                            });
                        }
                    }
                    if node.kind() == "function_item" {
                        if let (Some(parameters), Some(body)) = (
                            node.child_by_field_name("parameters"),
                            node.child_by_field_name("body"),
                        ) {
                            for name in bound_names(parameters, source) {
                                this.shadows
                                    .push((name, body.start_byte(), body.end_byte()));
                            }
                        }
                    }
                }
                "closure_expression" => {
                    if let (Some(parameters), Some(body)) = (
                        node.child_by_field_name("parameters"),
                        node.child_by_field_name("body"),
                    ) {
                        for name in bound_names(parameters, source) {
                            this.shadows
                                .push((name, body.start_byte(), body.end_byte()));
                        }
                    }
                }
                "let_declaration" => {
                    for name in bound_names(node, source) {
                        this.shadows.push((name, node.end_byte(), scope(node).1));
                    }
                }
                "for_expression" | "match_arm" => {
                    if let (Some(pattern), Some(body)) = (
                        node.child_by_field_name("pattern"),
                        node.child_by_field_name("body")
                            .or_else(|| node.child_by_field_name("value")),
                    ) {
                        for name in bound_names(pattern, source) {
                            this.shadows
                                .push((name, body.start_byte(), body.end_byte()));
                        }
                    }
                }
                "let_condition" => {
                    if let (Some(pattern), Some(parent)) =
                        (node.child_by_field_name("pattern"), node.parent())
                    {
                        for name in bound_names(pattern, source) {
                            this.shadows
                                .push((name, node.end_byte(), parent.end_byte()));
                        }
                    }
                }
                "inner_attribute_item" => {
                    if text(node, source).contains("cfg") {
                        this.facts
                            .issues
                            .push("conditional_file_configuration".into());
                    }
                }
                _ => {}
            }
            pending.extend(children(node).into_iter().rev());
        }
        this
    }
    fn use_tree(&mut self, node: Node<'_>, prefix: Vec<String>, owner: Node<'_>, source: &str) {
        match node.kind() {
            "use_list" => {
                for child in children(node) {
                    self.use_tree(child, prefix.clone(), owner, source);
                }
            }
            "scoped_use_list" => {
                let mut prefix = prefix;
                if let Some(p) = node
                    .child_by_field_name("path")
                    .and_then(|n| path(n, source))
                {
                    prefix.extend(p);
                }
                if let Some(list) = node.child_by_field_name("list") {
                    self.use_tree(list, prefix, owner, source);
                }
            }
            _ => {
                let wildcard = node.kind() == "use_wildcard";
                let alias = node
                    .child_by_field_name("alias")
                    .map(|n| ident(text(n, source)));
                let target = if wildcard {
                    node.named_child(0)
                } else {
                    node.child_by_field_name("path")
                        .filter(|_| node.kind() == "use_as_clause")
                        .or(Some(node))
                };
                let mut segments = prefix;
                if let Some(target) = target.and_then(|n| path(n, source)) {
                    segments.extend(target);
                }
                if segments.last().is_some_and(|s| s == "self") && segments.len() > 1 {
                    segments.pop();
                }
                let name = if wildcard {
                    "*".into()
                } else {
                    alias.unwrap_or_else(|| segments.last().cloned().unwrap_or_default())
                };
                if name == "_" {
                    return;
                }
                let (reason, _) = attributes(owner, source);
                self.facts.bindings.push(Binding {
                    name: name.clone(),
                    symbol: name,
                    kind: if wildcard { "glob" } else { "use" }.into(),
                    context: context(owner, source),
                    scope: scope(owner).0,
                    path: segments,
                    site: site(owner),
                    visibility: visibility(owner, source),
                    inline: false,
                    path_attribute: None,
                    reason,
                });
            }
        }
    }
    pub fn call(&self, node: Node<'_>, source: &str) -> Call {
        let parts = path(node, source);
        let mut call = Call {
            version: 1,
            local: text(node, source).into(),
            module: String::new(),
            imported: String::new(),
            context: context(node, source),
            path_from_use: false,
            local_target: None,
            glob_paths: vec![],
            import_site: site(node),
            site: site(node),
            unbound_reason: None,
        };
        if !self.facts.issues.is_empty() {
            call.unbound_reason = Some("incomplete_module_syntax".into());
            return call;
        }
        let Some(mut parts) = parts else {
            call.unbound_reason = Some("unsupported_receiver_or_associated_call".into());
            return call;
        };
        if parts.len() == 1
            && self.shadows.iter().any(|(name, start, end)| {
                name == &parts[0] && *start <= node.start_byte() && *end >= node.end_byte()
            })
        {
            call.unbound_reason = Some("shadowed_value_binding".into());
            return call;
        }
        let mut scopes = vec![];
        let mut at = node;
        while let Some(parent) = at.parent() {
            if parent.kind() == "block" {
                scopes.push(parent.start_byte() + 1);
            }
            if matches!(parent.kind(), "source_file" | "mod_item") {
                scopes.push(0);
                break;
            }
            let (reason, _) = attributes(parent, source);
            if let Some(reason) = reason {
                call.unbound_reason = Some(reason);
                return call;
            }
            at = parent;
        }
        for scope in scopes {
            let bindings: Vec<_> = self
                .facts
                .bindings
                .iter()
                .filter(|b| b.scope == scope && b.context == call.context && b.name == parts[0])
                .collect();
            if bindings.len() > 1 {
                call.unbound_reason = Some("ambiguous_binding".into());
                return call;
            }
            if let Some(binding) = bindings.first() {
                if let Some(reason) = &binding.reason {
                    call.unbound_reason = Some(reason.clone());
                    return call;
                }
                if binding.kind == "use" {
                    let mut expanded = binding.path.clone();
                    expanded.extend_from_slice(&parts[1..]);
                    parts = expanded;
                    call.path_from_use = true;
                    call.import_site = binding.site.clone();
                    break;
                }
                if scope != 0 && binding.kind == "function" && parts.len() == 1 {
                    call.local_target = Some((binding.symbol.clone(), binding.site.line));
                    call.import_site = binding.site.clone();
                    break;
                }
                if scope != 0 {
                    call.unbound_reason = Some("unsupported_block_item".into());
                    return call;
                }
                break;
            }
            if scope != 0 {
                for glob in
                    self.facts.bindings.iter().filter(|b| {
                        b.scope == scope && b.context == call.context && b.kind == "glob"
                    })
                {
                    if let Some(reason) = &glob.reason {
                        call.unbound_reason = Some(reason.clone());
                        return call;
                    }
                    call.glob_paths.push(glob.path.clone());
                    call.import_site = glob.site.clone();
                }
                if !call.glob_paths.is_empty() {
                    break;
                }
            }
        }
        call.imported = parts.pop().unwrap_or_default();
        call.module = parts.join("::");
        call
    }
}
