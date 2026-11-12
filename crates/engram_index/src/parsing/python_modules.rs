//! Lexical Python bindings. Never execute source or inspect the host sys.path.
use engram_core::python_modules::{Binding, Call, Module, Site};
use tree_sitter::Node;

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}
fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    &source[node.byte_range()]
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
fn is_scope(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "module"
            | "function_definition"
            | "class_definition"
            | "lambda"
            | "list_comprehension"
            | "set_comprehension"
            | "dictionary_comprehension"
            | "generator_expression"
    )
}
fn scope_id(node: Node<'_>) -> usize {
    if node.kind() == "module" {
        0
    } else {
        node.start_byte() + 1
    }
}
fn owner(mut node: Node<'_>) -> Node<'_> {
    while let Some(parent) = node.parent() {
        node = parent;
        if is_scope(node) {
            break;
        }
    }
    node
}
fn conditional(mut node: Node<'_>, scope: Node<'_>) -> bool {
    while let Some(parent) = node.parent() {
        if parent.id() == scope.id() {
            return false;
        }
        if !matches!(
            parent.kind(),
            "block" | "decorated_definition" | "expression_statement"
        ) {
            return true;
        }
        node = parent;
    }
    false
}
fn names(node: Node<'_>, source: &str) -> Vec<String> {
    match node.kind() {
        "identifier" => vec![text(node, source).into()],
        "default_parameter" | "typed_default_parameter" => node
            .child_by_field_name("name")
            .map(|n| names(n, source))
            .unwrap_or_default(),
        "typed_parameter" => children(node)
            .into_iter()
            .filter(|n| n.kind() != "type")
            .flat_map(|n| names(n, source))
            .collect(),
        "pattern_list"
        | "tuple_pattern"
        | "list_pattern"
        | "list_splat_pattern"
        | "dictionary_splat_pattern"
        | "parameters"
        | "lambda_parameters"
        | "as_pattern_target" => children(node)
            .into_iter()
            .flat_map(|n| names(n, source))
            .collect(),
        _ => vec![],
    }
}
fn dotted(node: Node<'_>, source: &str) -> Option<String> {
    match node.kind() {
        "identifier" => Some(text(node, source).into()),
        "attribute" => Some(format!(
            "{}.{}",
            dotted(node.child_by_field_name("object")?, source)?,
            text(node.child_by_field_name("attribute")?, source)
        )),
        _ => None,
    }
}

pub(super) struct Extraction {
    pub facts: Module,
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
            let scope = owner(node);
            match node.kind() {
                "import_statement" | "import_from_statement" => this.import(node, scope, source),
                "function_definition" | "class_definition" => {
                    if let Some(name) = node.child_by_field_name("name") {
                        let kind = if node.kind() == "function_definition" {
                            "function"
                        } else {
                            "class"
                        };
                        let reason = if conditional(node, scope) {
                            Some("conditional_binding")
                        } else if node
                            .parent()
                            .is_some_and(|n| n.kind() == "decorated_definition")
                        {
                            Some("decorated_callable")
                        } else {
                            None
                        };
                        this.bind(text(name, source).into(), kind, node, scope, reason);
                    }
                    if let Some(params) = node.child_by_field_name("parameters") {
                        for name in names(params, source) {
                            this.bind(name, "assignment", params, node, None);
                        }
                    }
                }
                "lambda" => {
                    if let Some(params) = node.child_by_field_name("parameters") {
                        for name in names(params, source) {
                            this.bind(name, "assignment", params, node, None);
                        }
                    }
                }
                "assignment"
                | "augmented_assignment"
                | "named_expression"
                | "for_statement"
                | "for_in_clause" => {
                    if let Some(target) = node
                        .child_by_field_name("left")
                        .or_else(|| node.child_by_field_name("name"))
                    {
                        for name in names(target, source) {
                            this.bind(name, "assignment", target, scope, None);
                        }
                    }
                }
                "as_pattern" => {
                    if let Some(target) = node.child_by_field_name("alias") {
                        for name in names(target, source) {
                            this.bind(name, "assignment", target, scope, None);
                        }
                    }
                }
                "except_clause" => {
                    if let Some(target) = node.child_by_field_name("alias") {
                        for name in names(target, source) {
                            this.bind(name, "assignment", target, scope, None);
                        }
                    }
                }
                "delete_statement" => {
                    for target in children(node) {
                        for name in names(target, source) {
                            this.bind(name, "assignment", target, scope, None);
                        }
                    }
                }
                "global_statement" | "nonlocal_statement" => {
                    for target in children(node) {
                        for name in names(target, source) {
                            this.bind(
                                name,
                                "directive",
                                target,
                                scope,
                                Some("unsupported_scope_directive"),
                            );
                        }
                    }
                }
                // A capture pattern can shadow an import for the entire function.
                "case_pattern" => {
                    for child in children(node) {
                        if child.kind() == "dotted_name"
                            && !text(child, source).contains('.')
                            && text(child, source) != "_"
                        {
                            this.bind(text(child, source).into(), "assignment", child, scope, None);
                        }
                    }
                }
                _ => {}
            }
            pending.extend(children(node).into_iter().rev());
        }
        this
    }
    fn bind(
        &mut self,
        local: String,
        kind: &str,
        node: Node<'_>,
        scope: Node<'_>,
        reason: Option<&str>,
    ) {
        self.facts.bindings.push(Binding {
            access: local.clone(),
            local,
            module: String::new(),
            imported: String::new(),
            kind: kind.into(),
            site: site(node),
            scope: scope_id(scope),
            reason: reason.map(str::to_string),
        });
    }
    fn import(&mut self, node: Node<'_>, scope: Node<'_>, source: &str) {
        let from = node.child_by_field_name("module_name");
        let mut cursor = node.walk();
        for item in node.children_by_field_name("name", &mut cursor) {
            let original = item.child_by_field_name("name").unwrap_or(item);
            let name = text(original, source).to_string();
            let alias = item
                .child_by_field_name("alias")
                .map(|n| text(n, source).to_string());
            let (module, imported, local, access) = if let Some(from) = from {
                let local = alias.unwrap_or_else(|| name.clone());
                (text(from, source).to_string(), name, local.clone(), local)
            } else {
                let access = alias.clone().unwrap_or_else(|| name.clone());
                let local = alias.unwrap_or_else(|| name.split('.').next().unwrap().into());
                (name, "*".into(), local, access)
            };
            self.facts.bindings.push(Binding {
                module,
                imported,
                local,
                access,
                kind: "import".into(),
                site: site(node),
                scope: scope_id(scope),
                reason: conditional(node, scope).then(|| "conditional_import".into()),
            });
        }
        if children(node).iter().any(|n| n.kind() == "wildcard_import") {
            self.bind("*".into(), "wildcard", node, scope, Some("wildcard_import"));
        }
    }
    pub fn call(&self, node: Node<'_>, source: &str) -> Call {
        let expression = node
            .parent()
            .filter(|n| n.kind() == "attribute")
            .unwrap_or(node);
        let spelling =
            dotted(expression, source).unwrap_or_else(|| text(expression, source).into());
        let local = spelling.split('.').next().unwrap_or("");
        let mut call = Call {
            version: 1,
            local: local.into(),
            imported: spelling.clone(),
            module: "<unknown>".into(),
            import_site: site(node),
            site: site(node),
            unbound_reason: None,
        };
        if !self.facts.issues.is_empty() {
            call.unbound_reason = Some("incomplete_module_syntax".into());
            return call;
        }
        // Defaults/decorators are evaluated in the enclosing scope. A class
        // namespace is not an enclosing lexical scope of its methods.
        let mut at = expression;
        let mut scopes = Vec::new();
        let mut skip_class = false;
        while let Some(parent) = at.parent() {
            if is_scope(parent) {
                let in_body = parent.child_by_field_name("body").is_none_or(|body| {
                    expression.start_byte() >= body.start_byte()
                        && expression.end_byte() <= body.end_byte()
                });
                if in_body && !(skip_class && parent.kind() == "class_definition") {
                    scopes.push(scope_id(parent));
                    if parent.kind() != "class_definition" && parent.kind() != "module" {
                        skip_class = true;
                    }
                }
            }
            at = parent;
        }
        for scope in scopes {
            let bindings: Vec<_> = self
                .facts
                .bindings
                .iter()
                .filter(|b| b.scope == scope && b.local == local)
                .collect();
            if bindings.is_empty() {
                if let Some(star) = self
                    .facts
                    .bindings
                    .iter()
                    .find(|b| b.scope == scope && b.kind == "wildcard")
                {
                    call.import_site = star.site.clone();
                    call.unbound_reason = Some("wildcard_import".into());
                    return call;
                }
                continue;
            }
            // Several unaliased imports may bind the same package; select an
            // exact module prefix only when all competing bindings are imports.
            let selected: Vec<_> = bindings
                .iter()
                .copied()
                .filter(|b| {
                    b.kind == "import"
                        && (spelling == b.access || spelling.starts_with(&format!("{}.", b.access)))
                })
                .collect();
            let binding = if bindings.len() == 1 {
                bindings[0]
            } else if bindings.iter().all(|b| b.kind == "import") && selected.len() == 1 {
                selected[0]
            } else {
                call.unbound_reason = Some("ambiguous_or_shadowed_binding".into());
                return call;
            };
            call.import_site = binding.site.clone();
            if let Some(reason) = &binding.reason {
                call.unbound_reason = Some(reason.clone());
                return call;
            }
            if binding.kind == "import" {
                call.module = binding.module.clone();
                let suffix = spelling
                    .strip_prefix(&binding.access)
                    .unwrap_or("")
                    .trim_start_matches('.');
                if !selected.contains(&binding) {
                    call.unbound_reason = Some("unsupported_module_attribute".into());
                } else if binding.imported == "*" {
                    call.imported = suffix.into();
                    if suffix.is_empty() {
                        call.unbound_reason = Some("module_not_callable".into());
                    }
                } else {
                    call.imported = if suffix.is_empty() {
                        binding.imported.clone()
                    } else {
                        format!("{}.{suffix}", binding.imported)
                    };
                }
            } else if matches!(binding.kind.as_str(), "function" | "class") && spelling == local {
                call.module = "<local>".into();
                call.imported = format!("{}:{}:{}", binding.kind, binding.site.line, binding.local);
            } else {
                call.unbound_reason = Some("shadowed_or_nonmodule_binding".into());
            }
            return call;
        }
        call.unbound_reason = Some(
            if spelling.contains('.') {
                "receiver_not_module"
            } else {
                "name_not_imported_or_declared"
            }
            .into(),
        );
        call
    }
}
