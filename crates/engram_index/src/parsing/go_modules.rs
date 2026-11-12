//! File-local Go imports and lexical shadowing; package binding is a second pass.
use engram_core::go_modules::{Call, Declaration, Import, Module, Site};
use tree_sitter::Node;
pub(super) mod config;

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
fn names(node: Node<'_>, source: &str) -> Vec<String> {
    let mut cursor = node.walk();
    node.children_by_field_name("name", &mut cursor)
        .filter(|n| n.is_named())
        .map(|n| text(n, source).to_string())
        .filter(|s| s != "_")
        .collect()
}
fn local_end(mut node: Node<'_>) -> Option<usize> {
    while let Some(parent) = node.parent() {
        if parent.kind() == "source_file" {
            return None;
        }
        if matches!(
            parent.kind(),
            "block"
                | "if_statement"
                | "for_statement"
                | "expression_switch_statement"
                | "type_switch_statement"
                | "expression_case"
                | "type_case"
                | "communication_case"
                | "default_case"
        ) {
            return Some(parent.end_byte());
        }
        node = parent;
    }
    None
}
fn constrained(path: &std::path::Path, root: Node<'_>, source: &str) -> Option<String> {
    if path
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|name| name.starts_with(['_', '.']))
    {
        return Some("build_constraints_not_evaluated".into());
    }
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let stem = stem.strip_suffix("_test").unwrap_or(stem);
    let suffix = stem.rsplit('_').next().unwrap_or("");
    if matches!(
        suffix,
        "aix"
            | "android"
            | "darwin"
            | "dragonfly"
            | "freebsd"
            | "hurd"
            | "illumos"
            | "ios"
            | "js"
            | "linux"
            | "nacl"
            | "netbsd"
            | "openbsd"
            | "plan9"
            | "solaris"
            | "wasip1"
            | "windows"
            | "zos"
            | "386"
            | "amd64"
            | "amd64p32"
            | "arm"
            | "arm64"
            | "loong64"
            | "mips"
            | "mipsle"
            | "mips64"
            | "mips64le"
            | "ppc64"
            | "ppc64le"
            | "riscv64"
            | "s390x"
            | "wasm"
    ) && stem.contains('_')
    {
        return Some("build_constraints_not_evaluated".into());
    }
    for n in children(root)
        .into_iter()
        .take_while(|n| n.kind() != "package_clause")
    {
        if n.kind() == "comment"
            && (text(n, source).starts_with("//go:build")
                || text(n, source).starts_with("// +build"))
        {
            return Some("build_constraints_not_evaluated".into());
        }
    }
    None
}
pub(super) struct Extraction {
    pub facts: Module,
    shadows: Vec<(String, usize, usize)>,
}
impl Extraction {
    pub(super) fn state_binding_shadowed(&self, node: Node<'_>, name: &str) -> bool {
        self.shadows.iter().any(|(binding, start, end)| binding == name
            && *start <= node.start_byte() && node.end_byte() <= *end)
    }

    pub fn new(path: &std::path::Path, root: Node<'_>, source: &str) -> Self {
        let package = children(root)
            .into_iter()
            .find(|n| n.kind() == "package_clause")
            .and_then(|n| n.named_child(0))
            .map(|n| text(n, source).to_string())
            .unwrap_or_default();
        let mut this = Self {
            facts: Module {
                version: 1,
                package,
                imports: vec![],
                declarations: vec![],
                calls: vec![],
                test_file: path.to_string_lossy().ends_with("_test.go"),
                constraint: constrained(path, root, source),
                issues: vec![],
            },
            shadows: vec![],
        };
        if root.has_error() || this.facts.package.is_empty() {
            this.facts.issues.push("incomplete_package_syntax".into());
        }
        let mut pending = vec![root];
        let mut count = 0;
        while let Some(node) = pending.pop() {
            count += 1;
            if count > 200_000 {
                this.facts.issues.push("scope_budget_exceeded".into());
                break;
            }
            match node.kind() {
                "import_spec" => {
                    if let Some(path) = node
                        .child_by_field_name("path")
                        .and_then(|n| config::unquote(text(n, source)))
                    {
                        if path == "C" {
                            this.facts.constraint = Some("cgo_not_evaluated".into());
                        }
                        this.facts.imports.push(Import {
                            alias: node
                                .child_by_field_name("name")
                                .map(|n| text(n, source).into()),
                            path,
                            site: site(node),
                        });
                    } else {
                        this.facts.issues.push("unsupported_import_string".into());
                    }
                }
                "function_declaration" => {
                    if let Some(name) = node.child_by_field_name("name") {
                        if !matches!(text(name, source), "init" | "_") {
                            this.facts.declarations.push(Declaration {
                                name: text(name, source).into(),
                                kind: "function".into(),
                                site: site(node),
                                has_body: node.child_by_field_name("body").is_some(),
                            });
                        }
                    }
                }
                "var_spec" | "const_spec" | "type_spec" | "type_alias" => {
                    for name in names(node, source) {
                        if let Some(end) = local_end(node) {
                            let start = if matches!(node.kind(), "type_spec" | "type_alias") {
                                node.child_by_field_name("name")
                                    .map_or(node.end_byte(), |n| n.end_byte())
                            } else {
                                node.end_byte()
                            };
                            this.shadows.push((name, start, end));
                        } else {
                            this.facts.declarations.push(Declaration {
                                name,
                                kind: "noncallable".into(),
                                site: site(node),
                                has_body: false,
                            });
                        }
                    }
                }
                "short_var_declaration" | "range_clause" | "receive_statement" => {
                    let declares = node.kind() == "short_var_declaration"
                        || children(node).first().is_some_and(|_| {
                            let mut c = node.walk();
                            node.children(&mut c).any(|n| n.kind() == ":=")
                        });
                    if declares {
                        if let (Some(left), Some(end)) =
                            (node.child_by_field_name("left"), local_end(node))
                        {
                            for n in children(left)
                                .into_iter()
                                .filter(|n| n.kind() == "identifier")
                            {
                                if text(n, source) != "_" {
                                    this.shadows.push((
                                        text(n, source).into(),
                                        node.end_byte(),
                                        end,
                                    ));
                                }
                            }
                        }
                    }
                }
                "type_switch_statement" => {
                    if let (Some(alias), Some(value)) = (
                        node.child_by_field_name("alias"),
                        node.child_by_field_name("value"),
                    ) {
                        for n in children(alias)
                            .into_iter()
                            .filter(|n| n.kind() == "identifier")
                        {
                            this.shadows.push((
                                text(n, source).into(),
                                value.end_byte(),
                                node.end_byte(),
                            ));
                        }
                    }
                }
                _ => {}
            }
            if matches!(
                node.kind(),
                "function_declaration" | "method_declaration" | "func_literal"
            ) {
                if let Some(body) = node.child_by_field_name("body") {
                    for field in ["parameters", "result", "receiver", "type_parameters"] {
                        if let Some(list) = node.child_by_field_name(field) {
                            for item in children(list) {
                                for name in names(item, source) {
                                    this.shadows
                                        .push((name, body.start_byte(), body.end_byte()));
                                }
                            }
                        }
                    }
                }
            }
            pending.extend(children(node).into_iter().rev());
        }
        this
    }
    pub fn call(&self, node: Node<'_>, source: &str) -> Call {
        let mut call = Call {
            version: 1,
            local: text(node, source).into(),
            qualifier: None,
            imported: String::new(),
            module: "<local>".into(),
            import_site: site(node),
            path_from_use: false,
            site: site(node),
            unbound_reason: None,
        };
        let mut n = node;
        while n.kind() == "parenthesized_expression" {
            let Some(inner) = n.named_child(0) else {
                break;
            };
            n = inner;
        }
        let shadow = match n.kind() {
            "identifier" => {
                call.imported = text(n, source).into();
                Some(call.imported.clone())
            }
            "selector_expression" => {
                if let (Some(operand), Some(field)) = (
                    n.child_by_field_name("operand"),
                    n.child_by_field_name("field"),
                ) {
                    if operand.kind() == "identifier" {
                        call.qualifier = Some(text(operand, source).into());
                        call.imported = text(field, source).into();
                        call.qualifier.clone()
                    } else {
                        call.unbound_reason = Some("unsupported_receiver_or_dynamic_call".into());
                        None
                    }
                } else {
                    None
                }
            }
            _ => {
                call.unbound_reason = Some("unsupported_receiver_or_dynamic_call".into());
                None
            }
        };
        if !self.facts.issues.is_empty() {
            call.unbound_reason = Some("incomplete_package_syntax".into());
        } else if let Some(reason) = &self.facts.constraint {
            call.unbound_reason = Some(reason.clone());
        } else if shadow.is_some_and(|name| {
            self.shadows.iter().any(|(n, start, end)| {
                *n == name && *start <= node.start_byte() && *end >= node.end_byte()
            })
        }) {
            call.unbound_reason = Some("shadowed_local_binding".into());
        }
        call
    }
}
