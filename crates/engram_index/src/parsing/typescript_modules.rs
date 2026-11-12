//! Static ES-module binding evidence. No filesystem reads or global name
//! guesses: the graph pass supplies indexed module/export identities later.
use engram_core::typescript_modules::{Call, Declaration, Export, Import, Module, Site};
use tree_sitter::Node;

pub(super) mod config;
#[cfg(test)]
mod tests;

pub(super) struct Extraction<'tree> {
    pub facts: Module,
    pub anonymous_defaults: Vec<Node<'tree>>,
    shadows: Vec<(String, usize, usize)>,
    dynamic_imports: Vec<(Import, usize, usize)>,
}

fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}

pub(super) fn site(node: Node<'_>) -> Site {
    Site {
        offset: node.start_byte(),
        end_offset: node.end_byte(),
        line: node.start_position().row as u32 + 1,
        column: node.start_position().column as u32 + 1,
        end_line: node.end_position().row as u32 + 1,
    }
}

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

fn token(node: Node<'_>, kind: &str) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|n| n.kind() == kind)
}

// Escaped module/export names need JS string decoding. Decline them rather
// than resolving a different path. Ordinary quotes are never part of a name.
fn name(node: Node<'_>, source: &str) -> Option<String> {
    let raw = text(node, source);
    if node.kind() != "string" {
        return Some(raw.into());
    }
    let value = raw.get(1..raw.len().checked_sub(1)?)?;
    (!value.contains(['\\', '\0', '\n', '\r'])).then(|| value.into())
}

fn function_scope(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "function_declaration"
            | "generator_function_declaration"
            | "function_expression"
            | "generator_function"
            | "arrow_function"
            | "method_definition"
    )
}

fn scope(mut node: Node<'_>, function_only: bool) -> Node<'_> {
    while let Some(parent) = node.parent() {
        node = parent;
        if node.kind() == "program"
            || function_scope(node)
            || (!function_only
                && matches!(
                    node.kind(),
                    "statement_block"
                        | "for_statement"
                        | "for_in_statement"
                        | "catch_clause"
                        | "switch_body"
                ))
        {
            break;
        }
    }
    node
}

// Only binding positions, never property keys, initializers or type names.
fn bound_names(node: Node<'_>, source: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut pending = vec![node];
    while let Some(n) = pending.pop() {
        match n.kind() {
            "identifier" | "shorthand_property_identifier_pattern" => {
                names.push(text(n, source).into())
            }
            "pair_pattern" => pending.extend(n.child_by_field_name("value")),
            "assignment_pattern" | "object_assignment_pattern" => {
                pending.extend(n.child_by_field_name("left"))
            }
            "required_parameter" | "optional_parameter" => pending.extend(
                n.child_by_field_name("pattern")
                    .or_else(|| n.child_by_field_name("name")),
            ),
            "object_pattern" | "array_pattern" | "rest_pattern" | "formal_parameters" => {
                pending.extend(children(n))
            }
            _ => {}
        }
    }
    names
}

fn dynamic_import(node: Node<'_>, source: &str) -> bool {
    let mut pending = vec![node];
    while let Some(node) = pending.pop() {
        if function_scope(node) {
            continue;
        }
        if node.kind() == "call_expression"
            && node.child_by_field_name("function").is_some_and(|n| {
                n.kind() == "import" || (n.kind() == "identifier" && text(n, source) == "require")
            })
        {
            return true;
        }
        pending.extend(children(node));
    }
    false
}

impl<'tree> Extraction<'tree> {
    pub(super) fn state_binding_shadowed_below(&self, node: Node<'_>, name: &str, scope: Node<'_>) -> bool {
        self.shadows.iter().any(|(binding,start,end)| binding==name
            && *start>=scope.start_byte() && *end<=scope.end_byte()
            && (*start!=scope.start_byte() || *end!=scope.end_byte())
            && *start<=node.start_byte() && node.end_byte()<=*end)
    }

    pub(super) fn state_binding_shadowed(&self, node: Node<'_>, name: &str, root: Node<'_>, ignore_module: bool) -> bool {
        self.shadows.iter().any(|(binding, start, end)| {
            binding == name && *start <= node.start_byte() && node.end_byte() <= *end
                && (!ignore_module || *start != root.start_byte() || *end != root.end_byte())
        }) || (!ignore_module && self.facts.imports.iter().any(|binding| binding.local == name))
    }

    pub fn new(root: Node<'tree>, source: &str) -> Self {
        let mut this = Self {
            facts: Module {
                version: 1,
                imports: vec![],
                exports: vec![],
                declarations: vec![],
                calls: vec![],
                issues: vec![],
            },
            anonymous_defaults: vec![],
            shadows: vec![],
            dynamic_imports: vec![],
        };
        if root.has_error() {
            this.facts.issues.push("parse_error".into());
        }
        for statement in children(root) {
            match statement.kind() {
                "import_statement" => this.import(statement, source),
                "export_statement" => this.export(statement, source),
                _ => this.declaration(statement, source, false, false),
            }
        }
        let mut pending = vec![root];
        let mut visited = 0;
        while let Some(node) = pending.pop() {
            visited += 1;
            if visited > 200_000 {
                this.facts.issues.push("scope_budget_exceeded".into());
                break;
            }
            if node.kind() == "import_statement" {
                continue;
            }
            if function_scope(node) {
                for parameter in [
                    node.child_by_field_name("parameters"),
                    node.child_by_field_name("parameter"),
                ]
                .into_iter()
                .flatten()
                {
                    this.add_shadows(parameter, node, source);
                }
                if node.kind() == "function_expression" {
                    if let Some(n) = node.child_by_field_name("name") {
                        this.add_shadows(n, node, source);
                    }
                }
            }
            match node.kind() {
                "variable_declarator" => {
                    if let Some(n) = node.child_by_field_name("name") {
                        let is_var = node
                            .parent()
                            .is_some_and(|n| n.kind() == "variable_declaration");
                        this.add_shadows(n, scope(node, is_var), source);
                        if node
                            .child_by_field_name("value")
                            .is_some_and(|v| dynamic_import(v, source))
                        {
                            let owner = scope(node, is_var);
                            for local in bound_names(n, source) {
                                this.dynamic_imports.push((
                                    Import {
                                        local,
                                        imported: "*".into(),
                                        module: "<dynamic>".into(),
                                        type_only: false,
                                        site: site(node),
                                    },
                                    owner.start_byte(),
                                    owner.end_byte(),
                                ));
                            }
                        }
                    }
                }
                "class" => {
                    if let Some(n) = node.child_by_field_name("name") {
                        this.shadows.push((
                            text(n, source).into(),
                            node.start_byte(),
                            node.end_byte(),
                        ));
                    }
                }
                "function_declaration"
                | "generator_function_declaration"
                | "class_declaration"
                | "enum_declaration"
                | "internal_module" => {
                    if let Some(n) = node.child_by_field_name("name") {
                        // Class names use type_identifier but also bind values.
                        this.shadows.push((
                            text(n, source).into(),
                            scope(node, false).start_byte(),
                            scope(node, false).end_byte(),
                        ));
                    }
                }
                "catch_clause" => {
                    if let Some(n) = node.child_by_field_name("parameter") {
                        this.add_shadows(n, node, source);
                    }
                }
                "for_in_statement" => {
                    if let Some(n) = node.child_by_field_name("left") {
                        let owner = if token(node, "var") {
                            scope(node, true)
                        } else {
                            node
                        };
                        this.add_shadows(n, owner, source);
                    }
                }
                _ => {}
            }
            pending.extend(children(node));
        }
        this
    }

    fn add_shadows(&mut self, binding: Node<'_>, owner: Node<'_>, source: &str) {
        for n in bound_names(binding, source) {
            self.shadows.push((n, owner.start_byte(), owner.end_byte()));
        }
    }

    fn import(&mut self, node: Node<'_>, source: &str) {
        // Retain the binding even when its module spelling is unsupported:
        // dropping it would send a later call back to name-only resolution.
        if let Some(require) = children(node)
            .into_iter()
            .find(|n| n.kind() == "import_require_clause")
        {
            self.facts.issues.push("unsupported_require_import".into());
            if let Some(local) = children(require)
                .into_iter()
                .find(|n| n.kind() == "identifier")
            {
                self.facts.imports.push(Import {
                    local: text(local, source).into(),
                    imported: "*".into(),
                    module: require
                        .child_by_field_name("source")
                        .map(|n| text(n, source).to_string())
                        .unwrap_or_default(),
                    type_only: false,
                    site: site(require),
                });
            }
            return;
        }
        let Some(module_node) = node.child_by_field_name("source") else {
            self.facts.issues.push("unsupported_import".into());
            return;
        };
        let module = name(module_node, source).unwrap_or_else(|| {
            self.facts.issues.push("unsupported_import".into());
            text(module_node, source).into()
        });
        let type_only = token(node, "type");
        let Some(clause) = children(node)
            .into_iter()
            .find(|n| n.kind() == "import_clause")
        else {
            return;
        };
        for binding in children(clause) {
            match binding.kind() {
                "identifier" => self.facts.imports.push(Import {
                    local: text(binding, source).into(),
                    imported: "default".into(),
                    module: module.clone(),
                    type_only,
                    site: site(binding),
                }),
                "namespace_import" => {
                    if let Some(local) = children(binding)
                        .into_iter()
                        .find(|n| n.kind() == "identifier")
                    {
                        self.facts.imports.push(Import {
                            local: text(local, source).into(),
                            imported: "*".into(),
                            module: module.clone(),
                            type_only,
                            site: site(binding),
                        });
                    }
                }
                "named_imports" => {
                    for spec in children(binding)
                        .into_iter()
                        .filter(|n| n.kind() == "import_specifier")
                    {
                        let Some(imported_node) = spec.child_by_field_name("name") else {
                            continue;
                        };
                        let imported = name(imported_node, source).unwrap_or_else(|| {
                            self.facts.issues.push("unsupported_import_name".into());
                            text(imported_node, source).into()
                        });
                        let local = spec.child_by_field_name("alias").unwrap_or(imported_node);
                        self.facts.imports.push(Import {
                            local: text(local, source).into(),
                            imported,
                            module: module.clone(),
                            type_only: type_only || token(spec, "type"),
                            site: site(spec),
                        });
                    }
                }
                _ => {}
            }
        }
    }

    fn declaration(&mut self, node: Node<'tree>, source: &str, exported: bool, default: bool) {
        if matches!(node.kind(), "lexical_declaration" | "variable_declaration") {
            for declarator in children(node)
                .into_iter()
                .filter(|n| n.kind() == "variable_declarator")
            {
                self.declaration(declarator, source, exported, false);
            }
            return;
        }
        let is_function = matches!(
            node.kind(),
            "function_declaration"
                | "function_expression"
                | "function_signature"
                | "arrow_function"
        );
        let value = node.child_by_field_name("value");
        let callable = is_function
            || value.is_some_and(|n| matches!(n.kind(), "arrow_function" | "function_expression"));
        let binding = node.child_by_field_name("name");
        let local = if let Some(binding) = binding {
            if !matches!(binding.kind(), "identifier" | "type_identifier") {
                return;
            }
            text(binding, source).to_string()
        } else if default && callable {
            self.anonymous_defaults.push(node);
            "default".into()
        } else {
            return;
        };
        self.facts.declarations.push(Declaration {
            local: local.clone(),
            site: site(node),
            callable,
            has_body: if is_function {
                node.child_by_field_name("body").is_some()
            } else {
                callable
            },
        });
        if exported {
            self.facts.exports.push(Export {
                exported: if default {
                    "default".into()
                } else {
                    local.clone()
                },
                local,
                module: None,
                kind: "local".into(),
                type_only: matches!(
                    node.kind(),
                    "interface_declaration" | "type_alias_declaration"
                ),
                site: site(node),
            });
        }
    }

    fn export(&mut self, node: Node<'tree>, source: &str) {
        let module_node = node.child_by_field_name("source");
        let module = module_node.and_then(|n| name(n, source));
        if module_node.is_some() && module.is_none() {
            self.facts.issues.push("unsupported_export_source".into());
            return;
        }
        let type_only = token(node, "type");
        if let Some(declaration) = node.child_by_field_name("declaration") {
            self.declaration(declaration, source, true, token(node, "default"));
            return;
        }
        if token(node, "default") {
            if let Some(value) = node.child_by_field_name("value") {
                if value.kind() == "identifier" {
                    self.facts.exports.push(Export {
                        exported: "default".into(),
                        local: text(value, source).into(),
                        module: None,
                        kind: "local".into(),
                        type_only: false,
                        site: site(value),
                    });
                } else if matches!(value.kind(), "function_expression" | "arrow_function") {
                    self.declaration(value, source, true, true);
                } else {
                    self.facts
                        .issues
                        .push("unsupported_default_expression".into());
                }
            }
            return;
        }
        for child in children(node) {
            if child.kind() == "export_clause" {
                for spec in children(child)
                    .into_iter()
                    .filter(|n| n.kind() == "export_specifier")
                {
                    let Some(local_node) = spec.child_by_field_name("name") else {
                        continue;
                    };
                    let Some(local) = name(local_node, source) else {
                        self.facts.issues.push("unsupported_export_name".into());
                        continue;
                    };
                    let Some(exported) = name(
                        spec.child_by_field_name("alias").unwrap_or(local_node),
                        source,
                    ) else {
                        self.facts.issues.push("unsupported_export_name".into());
                        continue;
                    };
                    self.facts.exports.push(Export {
                        exported,
                        local,
                        kind: if module.is_some() {
                            "reexport"
                        } else {
                            "local"
                        }
                        .into(),
                        module: module.clone(),
                        type_only: type_only || token(spec, "type"),
                        site: site(spec),
                    });
                }
                return;
            }
            if child.kind() == "namespace_export" {
                if let Some(exported) = children(child).first().and_then(|n| name(*n, source)) {
                    self.facts.exports.push(Export {
                        exported,
                        local: "*".into(),
                        module: module.clone(),
                        kind: "namespace".into(),
                        type_only,
                        site: site(child),
                    });
                }
                return;
            }
        }
        if token(node, "*") && module.is_some() {
            self.facts.exports.push(Export {
                exported: "*".into(),
                local: "*".into(),
                module,
                kind: "star".into(),
                type_only,
                site: site(node),
            });
        } else {
            self.facts.issues.push("unsupported_export".into());
        }
    }

    /// The capture is the actual callee identifier/property, so two calls on
    /// the same line retain different byte/column identities.
    pub fn call(&self, callee: Node<'_>, source: &str) -> Option<Call> {
        let member = callee.parent().filter(|n| n.kind() == "member_expression");
        let local_node = member
            .and_then(|n| n.child_by_field_name("object"))
            .unwrap_or(callee);
        if local_node.kind() != "identifier" {
            return None;
        }
        let local = text(local_node, source);
        if let Some((binding, _, _)) = self.dynamic_imports.iter().find(|(i, start, end)| {
            i.local == local && *start <= callee.start_byte() && callee.end_byte() <= *end
        }) {
            return Some(Call {
                version: 1,
                local: local.into(),
                imported: text(callee, source).into(),
                module: binding.module.clone(),
                import_site: binding.site.clone(),
                site: site(callee),
                unbound_reason: Some("unsupported_dynamic_import".into()),
            });
        }
        let mut imports = self.facts.imports.iter().filter(|i| i.local == local);
        let binding = imports.next()?;
        let reason = if imports.next().is_some() {
            Some("ambiguous_import")
        } else if !self.facts.issues.is_empty() {
            Some("incomplete_module_syntax")
        } else if self.shadows.iter().any(|(n, start, end)| {
            n == local && *start <= callee.start_byte() && callee.end_byte() <= *end
        }) {
            Some("shadowed_import")
        } else if binding.type_only {
            Some("type_only_import")
        } else if member.is_some() && binding.imported != "*" {
            Some("unsupported_import_receiver")
        } else if member.is_none() && binding.imported == "*" {
            Some("namespace_not_callable")
        } else {
            None
        };
        Some(Call {
            version: 1,
            local: local.into(),
            imported: if member.is_some() {
                text(callee, source).into()
            } else {
                binding.imported.clone()
            },
            module: binding.module.clone(),
            import_site: binding.site.clone(),
            site: site(callee),
            unbound_reason: reason.map(str::to_string),
        })
    }
}
