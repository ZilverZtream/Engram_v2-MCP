//! Java lexical facts. Cross-file type and import binding belongs to pass two.
use engram_core::java_modules::{Call, Import, Method, Module, Site, Type};
use tree_sitter::Node;

fn children(n: Node<'_>) -> Vec<Node<'_>> {
    let mut c = n.walk();
    n.named_children(&mut c).collect()
}
fn text<'a>(n: Node<'_>, s: &'a str) -> &'a str {
    &s[n.byte_range()]
}
fn site(n: Node<'_>) -> Site {
    Site {
        offset: n.start_byte(),
        end_offset: n.end_byte(),
        line: n.start_position().row as u32 + 1,
        column: n.start_position().column as u32 + 1,
        end_line: n.end_position().row as u32 + 1,
    }
}
fn type_node(n: Node<'_>) -> bool {
    matches!(
        n.kind(),
        "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "annotation_type_declaration"
    )
}
fn enclosing(mut n: Node<'_>, predicate: impl Fn(Node<'_>) -> bool) -> Option<Node<'_>> {
    while let Some(p) = n.parent() {
        if predicate(p) {
            return Some(p);
        }
        n = p;
    }
    None
}
fn modifiers(n: Node<'_>, word: &str, s: &str) -> bool {
    children(n)
        .into_iter()
        .find(|c| c.kind() == "modifiers")
        .is_some_and(|m| {
            let mut c = m.walk();
            m.children(&mut c).any(|t| text(t, s) == word)
        })
}
fn access(n: Node<'_>, s: &str, interface: bool) -> String {
    for a in ["public", "private", "protected"] {
        if modifiers(n, a, s) {
            return a.into();
        }
    }
    if interface {
        "public".into()
    } else {
        "package".into()
    }
}
fn canonical(n: Node<'_>, s: &str, package: &str) -> String {
    let mut parts = Vec::new();
    let mut at = Some(n);
    while let Some(n) = at {
        if type_node(n) {
            if let Some(name) = n.child_by_field_name("name") {
                parts.push(text(name, s));
            }
        }
        at = n.parent();
    }
    parts.reverse();
    if !package.is_empty() {
        parts.insert(0, package);
    }
    parts.join(".")
}
fn type_spelling(n: Node<'_>, s: &str) -> Option<String> {
    match n.kind() {
        "type_identifier" | "scoped_type_identifier" => {
            Some(text(n, s).split_whitespace().collect())
        }
        "generic_type" => children(n).into_iter().find_map(|c| type_spelling(c, s)),
        "annotated_type" => children(n)
            .into_iter()
            .rev()
            .find_map(|c| type_spelling(c, s)),
        _ => None,
    }
    .filter(|v| v != "var")
}
struct Binding {
    name: String,
    ty: Option<String>,
    site: Site,
    start: usize,
    end: usize,
    field: bool,
    is_static: bool,
}
pub(super) struct Extraction {
    pub facts: Module,
    bindings: Vec<Binding>,
    type_shadows: Vec<(String, usize, usize, &'static str)>,
}
impl Extraction {
    pub fn new(root: Node<'_>, source: &str) -> Self {
        let package = children(root)
            .into_iter()
            .find(|n| n.kind() == "package_declaration")
            .and_then(|n| {
                children(n)
                    .into_iter()
                    .find(|c| matches!(c.kind(), "identifier" | "scoped_identifier"))
            })
            .map(|n| text(n, source).split_whitespace().collect())
            .unwrap_or_default();
        let mut this = Self {
            facts: Module {
                version: 1,
                package,
                imports: vec![],
                types: vec![],
                methods: vec![],
                calls: vec![],
                named_module: false,
                issues: vec![],
            },
            bindings: vec![],
            type_shadows: vec![],
        };
        if root.has_error() {
            this.facts.issues.push("source_parse_error".into());
        }
        let mut stack = vec![root];
        let mut visited = 0;
        while let Some(n) = stack.pop() {
            visited += 1;
            if visited > 200_000 {
                this.facts.issues.push("source_scope_budget".into());
                break;
            }
            let children = children(n);
            stack.extend(children.iter().rev().copied());
            if n.kind() == "module_declaration" {
                this.facts.named_module = true;
            }
            if n.kind() == "import_declaration" {
                let path = children
                    .iter()
                    .find(|c| matches!(c.kind(), "identifier" | "scoped_identifier"));
                if let Some(path) = path {
                    let mut c = n.walk();
                    let tokens: Vec<_> = n.children(&mut c).map(|t| text(t, source)).collect();
                    this.facts.imports.push(Import {
                        path: text(*path, source).split_whitespace().collect(),
                        is_static: tokens.contains(&"static"),
                        wildcard: children.iter().any(|c| c.kind() == "asterisk"),
                        site: site(n),
                    });
                    if tokens.contains(&"module") {
                        this.facts.issues.push("module_import_not_evaluated".into());
                    }
                }
            }
            if type_node(n) {
                let Some(name) = n.child_by_field_name("name") else {
                    continue;
                };
                let parent = enclosing(n, type_node);
                let owner = parent.map(|p| canonical(p, source, &this.facts.package));
                let scoped = enclosing(n, |p| {
                    matches!(
                        p.kind(),
                        "method_declaration"
                            | "constructor_declaration"
                            | "lambda_expression"
                            | "object_creation_expression"
                    )
                })
                .is_some();
                if scoped {
                    if let Some(scope) = enclosing(n, |p| p.kind() == "block") {
                        this.type_shadows.push((
                            text(name, source).into(),
                            n.start_byte(),
                            scope.end_byte(),
                            "local_type_not_resolved",
                        ));
                    }
                }
                this.facts.types.push(Type {
                    name: text(name, source).into(),
                    owner,
                    canonical: canonical(n, source, &this.facts.package),
                    access: access(
                        n,
                        source,
                        parent.is_some_and(|p| p.kind() == "interface_declaration"),
                    ),
                    inherited: n.child_by_field_name("superclass").is_some()
                        || n.child_by_field_name("interfaces").is_some()
                        || children.iter().any(|c| c.kind() == "extends_interfaces"),
                    scoped,
                    site: site(n),
                });
            }
            if n.kind() == "method_declaration" {
                if let (Some(owner), Some(name), Some(params)) = (
                    enclosing(n, type_node),
                    n.child_by_field_name("name"),
                    n.child_by_field_name("parameters"),
                ) {
                    let parameters = children_of_parameters(params);
                    this.facts.methods.push(Method {
                        owner: canonical(owner, source, &this.facts.package),
                        name: text(name, source).into(),
                        access: access(n, source, owner.kind() == "interface_declaration"),
                        is_static: modifiers(n, "static", source),
                        arity: parameters.len(),
                        variadic: parameters.iter().any(|p| p.kind() == "spread_parameter"),
                        site: site(n),
                    });
                }
            }
            if matches!(
                n.kind(),
                "field_declaration" | "constant_declaration" | "local_variable_declaration"
            ) {
                let field = n.kind() != "local_variable_declaration";
                let scope = if field {
                    enclosing(n, type_node)
                } else {
                    enclosing(n, |p| {
                        matches!(
                            p.kind(),
                            "block"
                                | "for_statement"
                                | "try_with_resources_statement"
                                | "switch_block"
                        )
                    })
                };
                if let Some(scope) = scope {
                    let ty = n
                        .child_by_field_name("type")
                        .and_then(|t| type_spelling(t, source));
                    for d in children
                        .iter()
                        .filter(|c| c.kind() == "variable_declarator")
                    {
                        if let Some(name) = d.child_by_field_name("name") {
                            this.bindings.push(Binding {
                                name: text(name, source).into(),
                                ty: if d.child_by_field_name("dimensions").is_some() {
                                    None
                                } else {
                                    ty.clone()
                                },
                                site: site(*d),
                                start: if field {
                                    scope.start_byte()
                                } else {
                                    name.start_byte()
                                },
                                end: scope.end_byte(),
                                field,
                                is_static: modifiers(n, "static", source)
                                    || n.kind() == "constant_declaration",
                            });
                        }
                    }
                }
            }
            if matches!(
                n.kind(),
                "formal_parameter"
                    | "spread_parameter"
                    | "catch_formal_parameter"
                    | "enhanced_for_statement"
                    | "resource"
            ) {
                let scope = if n.kind() == "enhanced_for_statement" {
                    n.child_by_field_name("body")
                } else {
                    enclosing(n, |p| {
                        matches!(
                            p.kind(),
                            "method_declaration"
                                | "constructor_declaration"
                                | "lambda_expression"
                                | "catch_clause"
                                | "try_with_resources_statement"
                        )
                    })
                };
                let name = n.child_by_field_name("name").or_else(|| {
                    children
                        .iter()
                        .find(|c| c.kind() == "variable_declarator")
                        .and_then(|d| d.child_by_field_name("name"))
                });
                if let (Some(scope), Some(name)) = (scope, name) {
                    let ty = if n.kind() == "spread_parameter"
                        || n.child_by_field_name("dimensions").is_some()
                    {
                        None
                    } else {
                        n.child_by_field_name("type")
                            .and_then(|t| type_spelling(t, source))
                    };
                    this.bindings.push(Binding {
                        name: text(name, source).into(),
                        ty,
                        site: site(n),
                        start: scope.start_byte(),
                        end: scope.end_byte(),
                        field: false,
                        is_static: false,
                    });
                }
            }
            if n.kind() == "lambda_expression" {
                if let Some(params) = n.child_by_field_name("parameters") {
                    let names = if params.kind() == "identifier" {
                        vec![params]
                    } else {
                        children_of_parameters(params)
                            .into_iter()
                            .filter(|p| p.kind() == "identifier")
                            .collect()
                    };
                    for name in names {
                        this.bindings.push(Binding {
                            name: text(name, source).into(),
                            ty: None,
                            site: site(name),
                            start: n.start_byte(),
                            end: n.end_byte(),
                            field: false,
                            is_static: false,
                        });
                    }
                }
            }
            if n.kind() == "type_parameter" {
                if let (Some(name), Some(scope)) = (
                    children.iter().find(|c| c.kind() == "type_identifier"),
                    n.parent().and_then(|p| p.parent()),
                ) {
                    this.type_shadows.push((
                        text(*name, source).into(),
                        scope.start_byte(),
                        scope.end_byte(),
                        "type_parameter_receiver_not_resolved",
                    ));
                }
            }
            if matches!(
                n.kind(),
                "instanceof_expression" | "type_pattern" | "record_pattern_component"
            ) {
                let name = n.child_by_field_name("name").or_else(|| {
                    if n.kind() == "instanceof_expression" {
                        None
                    } else {
                        children
                            .iter()
                            .rev()
                            .find(|c| c.kind() == "identifier")
                            .copied()
                    }
                });
                if let (Some(name), Some(scope)) = (
                    name,
                    enclosing(n, |p| {
                        matches!(p.kind(), "method_declaration" | "constructor_declaration")
                    }),
                ) {
                    this.type_shadows.push((
                        text(name, source).into(),
                        scope.start_byte(),
                        scope.end_byte(),
                        "pattern_scope_not_resolved",
                    ));
                }
            }
        }
        this
    }
    pub fn declaration_fqn(&self, n: Node<'_>, source: &str) -> Option<String> {
        if type_node(n) {
            return Some(canonical(n, source, &self.facts.package));
        }
        if matches!(n.kind(), "method_declaration" | "constructor_declaration") {
            let owner = enclosing(n, type_node)?;
            let name = n.child_by_field_name("name")?;
            return Some(format!(
                "{}.{}",
                canonical(owner, source, &self.facts.package),
                text(name, source)
            ));
        }
        None
    }
    pub fn call(&self, n: Node<'_>, source: &str) -> Call {
        let name = n
            .child_by_field_name("name")
            .map(|n| text(n, source))
            .unwrap_or("");
        let owner = enclosing(n, type_node)
            .map(|o| canonical(o, source, &self.facts.package))
            .unwrap_or_default();
        let mut call = Call {
            version: 1,
            local: name.into(),
            imported: name.into(),
            module: "<local>".into(),
            import_site: site(n),
            path_from_use: false,
            site: site(n),
            owner,
            qualifier: None,
            receiver_type: None,
            receiver_site: None,
            invocation: "bare".into(),
            arity: n
                .child_by_field_name("arguments")
                .map(|a| {
                    children(a)
                        .iter()
                        .filter(|c| !c.kind().contains("comment"))
                        .count()
                })
                .unwrap_or(0),
            static_context: enclosing(n, |p| {
                matches!(
                    p.kind(),
                    "method_declaration"
                        | "static_initializer"
                        | "field_declaration"
                        | "constant_declaration"
                )
            })
            .is_some_and(|m| m.kind() == "static_initializer" || modifiers(m, "static", source)),
            unbound_reason: None,
        };
        if !self.facts.issues.is_empty() {
            call.unbound_reason = Some("unsupported_source_facts".into());
        }
        if enclosing(n, |p| {
            p.kind() == "object_creation_expression" && p.child_by_field_name("body").is_some()
        })
        .is_some()
        {
            call.unbound_reason = Some("anonymous_owner_not_resolved".into());
        }
        if let Some(mut object) = n.child_by_field_name("object") {
            while object.kind() == "parenthesized_expression" {
                let Some(c) = object.named_child(0) else {
                    break;
                };
                object = c;
            }
            let q = text(object, source).split_whitespace().collect::<String>();
            call.qualifier = Some(q.clone());
            if q == "this" {
                call.invocation = "this".into();
            } else if q == "super" || q.ends_with(".this") || q.ends_with(".super") {
                call.unbound_reason = Some("inherited_or_outer_receiver_not_resolved".into());
            } else if matches!(
                object.kind(),
                "object_creation_expression" | "cast_expression"
            ) {
                call.invocation = "instance".into();
                call.receiver_type = object
                    .child_by_field_name("type")
                    .and_then(|t| type_spelling(t, source));
                call.receiver_site = Some(site(object));
                if call.receiver_type.is_none() {
                    call.unbound_reason = Some("receiver_type_not_resolved".into());
                }
            } else {
                let field = q.strip_prefix("this.");
                let first = field.unwrap_or(&q).split('.').next().unwrap_or("");
                let binding = self
                    .bindings
                    .iter()
                    .filter(|b| {
                        b.name == first
                            && b.start <= n.start_byte()
                            && b.end >= n.end_byte()
                            && (field.is_none() || b.field)
                    })
                    .min_by_key(|b| (b.end - b.start, usize::MAX - b.start));
                if let Some(b) = binding {
                    call.invocation = "instance".into();
                    call.receiver_type = b.ty.clone();
                    call.receiver_site = Some(b.site.clone());
                    if field.unwrap_or(&q).contains('.') || b.ty.is_none() {
                        call.unbound_reason = Some("receiver_type_not_resolved".into());
                    }
                    if b.field && call.static_context && (!b.is_static || field.is_some()) {
                        call.unbound_reason = Some("instance_receiver_in_static_context".into());
                    }
                } else if matches!(
                    object.kind(),
                    "identifier" | "field_access" | "scoped_identifier"
                ) && q.split('.').all(|p| {
                    !p.is_empty()
                        && p.chars()
                            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '$'))
                }) {
                    call.invocation = "type".into();
                    call.receiver_type = Some(q);
                } else {
                    call.unbound_reason = Some("receiver_type_not_resolved".into());
                }
            }
        }
        // Pattern bindings and type parameters can obscure an imported type.
        // Their flow/type substitution is deliberately explicit, never guessed.
        if let Some(receiver) = call.receiver_type.as_deref() {
            let first = receiver.split('.').next().unwrap_or("");
            if let Some((_, _, _, reason)) =
                self.type_shadows.iter().find(|(name, start, end, _)| {
                    name == first && *start <= n.start_byte() && *end >= n.end_byte()
                })
            {
                call.unbound_reason = Some((*reason).into());
            }
        }
        call
    }
}
fn children_of_parameters(n: Node<'_>) -> Vec<Node<'_>> {
    children(n)
        .into_iter()
        .filter(|c| !c.kind().contains("comment"))
        .collect()
}
