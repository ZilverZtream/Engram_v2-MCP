//! Static state observations from the parser's existing syntax tree. Import
//! evidence is reused; no environment values, modules or application code run.
use super::{
    ExtractedEdge, ExtractedSymbol, go_modules, python_modules, rust_modules, typescript_modules,
};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::Path,
};
use tree_sitter::Node;
mod browser_storage;
mod frameworks;
mod references;

pub(super) struct Bindings<'a, 'tree> {
    pub script: Option<&'a typescript_modules::Extraction<'tree>>,
    pub python: Option<&'a python_modules::Extraction>,
    pub rust: Option<&'a rust_modules::Extraction>,
    pub go: Option<&'a go_modules::Extraction>,
}

fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}
fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}
fn field<'t>(node: Node<'t>, names: &[&str]) -> Option<Node<'t>> {
    names.iter().find_map(|name| node.child_by_field_name(name))
}
fn contains(outer: Node<'_>, inner: Node<'_>) -> bool {
    outer.start_byte() <= inner.start_byte() && inner.end_byte() <= outer.end_byte()
}
fn literal(node: Node<'_>, source: &str) -> Option<String> {
    if !matches!(
        node.kind(),
        "string" | "string_literal" | "interpreted_string_literal" | "raw_string_literal"
    ) {
        return None;
    }
    let raw = text(node, source);
    let quote = *raw.as_bytes().first()?;
    if !matches!(quote, b'\'' | b'"' | b'`') || raw.as_bytes().last() != Some(&quote) {
        return None;
    }
    let value = raw.get(1..raw.len().checked_sub(1)?)?;
    (!value.is_empty() && !value.contains(['\\', '\0', '\r', '\n', '\'', '"', '`']))
        .then(|| value.into())
}
fn member(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "member_expression"
            | "subscript_expression"
            | "attribute"
            | "subscript"
            | "selector_expression"
            | "field_expression"
            | "index_expression"
    )
}
// Keep string indexes distinct from property paths: cache["a.b"] != cache.a.b.
fn access_path(node: Node<'_>, source: &str) -> Option<(String, String)> {
    if node.kind() == "identifier" {
        return Some((text(node, source).into(), String::new()));
    }
    if !member(node) {
        return None;
    }
    let object = field(node, &["object", "value", "operand", "argument"])?;
    let (base, mut suffix) = access_path(object, source)?;
    if let Some(property) = field(node, &["property", "attribute", "field"]) {
        if !matches!(
            property.kind(),
            "identifier" | "property_identifier" | "field_identifier"
        ) {
            return None;
        }
        suffix.push('.');
        suffix.push_str(text(property, source));
    } else {
        let index = field(node, &["index", "subscript"])?;
        suffix.push('[');
        suffix.push_str(&serde_json::to_string(&literal(index, source)?).ok()?);
        suffix.push(']');
    }
    Some((base, suffix))
}
fn direction(node: Node<'_>) -> (bool, bool) {
    let mut at = node;
    while let Some(parent) = at.parent() {
        match parent.kind() {
            "parenthesized_expression" | "expression_list" => {
                at = parent;
                continue;
            }
            "assignment_expression" | "assignment" | "assignment_statement" => {
                if field(parent, &["left"]).is_some_and(|lhs| contains(lhs, node)) {
                    let mut cursor = parent.walk();
                    let compound = parent.children(&mut cursor).any(|child| {
                        matches!(
                            child.kind(),
                            "+=" | "-="
                                | "*="
                                | "/="
                                | "%="
                                | "&="
                                | "|="
                                | "^="
                                | "<<="
                                | ">>="
                                | "**="
                                | "??="
                                | "&&="
                                | "||="
                        )
                    });
                    return (compound, true);
                }
            }
            "augmented_assignment_expression"
            | "augmented_assignment"
            | "compound_assignment_expr" => {
                if field(parent, &["left"]).is_some_and(|lhs| contains(lhs, node)) {
                    return (true, true);
                }
            }
            "update_expression" | "inc_statement" | "dec_statement" => return (true, true),
            "delete_statement" => return (false, true),
            _ => {}
        }
        break;
    }
    (true, false)
}
fn top_level(mut node: Node<'_>, root: Node<'_>) -> bool {
    while let Some(parent) = node.parent() {
        if parent.id() == root.id() {
            return true;
        }
        if !matches!(
            parent.kind(),
            "export_statement"
                | "lexical_declaration"
                | "variable_declaration"
                | "expression_statement"
                | "var_declaration"
                | "var_spec_list"
        ) {
            return false;
        }
        node = parent;
    }
    false
}

struct Declaration<'t> {
    name: String,
    node: Node<'t>,
    name_node: Node<'t>,
    singleton: bool,
}
struct Output<'a> {
    path: String,
    source: &'a str,
    language: &'a str,
    members: &'a [ExtractedSymbol],
    symbols: Vec<ExtractedSymbol>,
    edges: Vec<ExtractedEdge>,
    seen: HashSet<(String, String)>,
    sites: HashSet<(usize, String, String)>,
    issues: BTreeSet<String>,
    references: Vec<engram_core::modern_state::Reference>,
}
impl Output<'_> {
    fn owner(&self, node: Node<'_>) -> (String, String, Option<u32>) {
        let line = node.start_position().row as u32 + 1;
        let mut enclosing = node.parent();
        while enclosing.is_some_and(|parent| {
            !matches!(
                parent.kind(),
                "function_declaration"
                    | "function_definition"
                    | "function_item"
                    | "method_declaration"
                    | "method_definition"
                    | "arrow_function"
                    | "function_expression"
                    | "func_literal"
                    | "lambda"
            )
        }) {
            enclosing = enclosing.and_then(|parent| parent.parent());
        }
        let mut candidates = self
            .members
            .iter()
            .filter(|symbol| {
                enclosing.is_some_and(|owner| {
                    symbol.start_line >= owner.start_position().row as u32 + 1
                        && symbol.start_line <= line
                }) && matches!(symbol.kind.as_str(), "function" | "property")
                    && line <= symbol.end_line
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|symbol| symbol.end_line - symbol.start_line);
        let owner = candidates.first().filter(|first| {
            !candidates.iter().skip(1).any(|other| {
                other.end_line - other.start_line == first.end_line - first.start_line
                    && other.name != first.name
            })
        });
        if let Some(owner) = owner {
            (
                owner
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("fqn"))
                    .unwrap_or(&owner.name)
                    .clone(),
                owner.kind.clone(),
                Some(owner.start_line),
            )
        } else {
            (self.path.clone(), "file".into(), None)
        }
    }
    fn emit(&mut self, node: Node<'_>, category: &str, key: &str, reads: bool, writes: bool) {
        let line = node.start_position().row as u32 + 1;
        let (source_name, source_kind, declaration_line) = self.owner(node);
        let mut metadata = HashMap::from([
            ("state_type".into(), category.into()),
            ("state_key".into(), key.into()),
            ("state_access_line".into(), line.to_string()),
            (
                "state_access_column".into(),
                (node.start_position().column + 1).to_string(),
            ),
            ("state_access_byte".into(), node.start_byte().to_string()),
            ("state_access_end_byte".into(), node.end_byte().to_string()),
            ("state_evidence".into(), "static_syntax_v1".into()),
        ]);
        if let Some(line) = declaration_line {
            metadata.insert("source_declaration_line".into(), line.to_string());
        }
        let target = format!("state:{category}:{key}");
        for (enabled, kind) in [(reads, "reads_state"), (writes, "writes_state")] {
            if enabled
                && self
                    .sites
                    .insert((node.start_byte(), target.clone(), kind.into()))
            {
                self.edges.push(ExtractedEdge {
                    source_name: source_name.clone(),
                    source_kind: source_kind.clone(),
                    source_start_line: line,
                    source_language: self.language.into(),
                    target_name: target.clone(),
                    target_kind: Some("global_state".into()),
                    target_start_line: None,
                    kind: kind.into(),
                    metadata: Some(metadata.clone()),
                });
            }
        }
        if self.seen.insert((category.into(), key.into())) {
            self.symbols.push(ExtractedSymbol {
                name: format!("{category}:{key}"),
                kind: "global_state".into(),
                start_line: line,
                end_line: line,
                metadata: Some(metadata),
            });
        }
    }
    fn environment(&mut self, node: Node<'_>, key: Option<String>, access: (bool, bool)) {
        if let Some(key) = key {
            self.emit(node, "Environment", &key, access.0, access.1);
        } else {
            self.issues.insert(format!(
                "dynamic_or_unsupported_environment_key:{}:{}",
                node.start_position().row + 1,
                node.start_position().column + 1
            ));
        }
    }
}

impl Bindings<'_, '_> {
    fn api(&self, callee: Node<'_>, source: &str) -> Option<(String, String)> {
        if let Some(python) = self.python {
            let call = python.call(callee, source);
            return call
                .unbound_reason
                .is_none()
                .then_some((call.module, call.imported));
        }
        if let Some(rust) = self.rust {
            let call = rust.call(callee, source);
            return call
                .unbound_reason
                .is_none()
                .then_some((call.module, call.imported));
        }
        if let Some(go) = self.go {
            let call = go.call(callee, source);
            if call.unbound_reason.is_some() {
                return None;
            }
            let qualifier = call.qualifier?;
            // Only the standard-library os path has a known package name here.
            // General package aliases remain owned by the existing second pass.
            let mut imports = go.facts.imports.iter().filter(|import| {
                import
                    .alias
                    .as_deref()
                    .unwrap_or(if import.path == "os" { "os" } else { "" })
                    == qualifier
            });
            let import = imports.next()?;
            if imports.next().is_some() || import.path != "os" {
                return None;
            }
            return Some((import.path.clone(), call.imported));
        }
        if let Some(script) = self.script {
            let name = field(callee, &["property"]).unwrap_or(callee);
            let call = script.call(name, source)?;
            return call
                .unbound_reason
                .is_none()
                .then_some((call.module, call.imported));
        }
        None
    }
    fn shadowed(
        &self,
        node: Node<'_>,
        name: &str,
        root: Node<'_>,
        globals: &HashSet<(usize, String)>,
    ) -> bool {
        if let Some(script) = self.script {
            return script.state_binding_shadowed(node, name, root, true);
        }
        if let Some(rust) = self.rust {
            return rust.state_binding_shadowed(node, name);
        }
        if let Some(go) = self.go {
            return go.state_binding_shadowed(node, name);
        }
        if let Some(python) = self.python {
            let mut at = node;
            while let Some(parent) = at.parent() {
                if matches!(
                    parent.kind(),
                    "function_definition"
                        | "lambda"
                        | "class_definition"
                        | "list_comprehension"
                        | "set_comprehension"
                        | "dictionary_comprehension"
                        | "generator_expression"
                ) {
                    let scope = parent.start_byte() + 1;
                    if globals.contains(&(scope, name.into())) {
                        return false;
                    }
                    if python
                        .facts
                        .bindings
                        .iter()
                        .any(|binding| binding.scope == scope && binding.local == name)
                    {
                        return true;
                    }
                }
                at = parent;
            }
        }
        false
    }
}

pub(super) fn extract(
    path: &Path,
    source: &str,
    root: Node<'_>,
    language: &str,
    members: &[ExtractedSymbol],
    bindings: Bindings<'_, '_>,
) -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>, String) {
    let mut out = Output {
        path: path.to_string_lossy().replace('\\', "/"),
        source,
        language,
        members,
        symbols: vec![],
        edges: vec![],
        seen: HashSet::new(),
        sites: HashSet::new(),
        issues: BTreeSet::new(),
        references: Vec::new(),
    };
    let mut nodes = Vec::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if nodes.len() >= 200_000 {
            out.issues.insert("syntax_budget_exceeded".into());
            break;
        }
        nodes.push(node);
        if !matches!(
            node.kind(),
            "comment"
                | "line_comment"
                | "block_comment"
                | "string"
                | "string_literal"
                | "interpreted_string_literal"
                | "raw_string_literal"
                | "regex"
        ) {
            pending.extend(children(node));
        }
    }
    if root.has_error() {
        out.issues.insert("incomplete_source_syntax".into());
    }
    let mut declarations = Vec::new();
    let mut globals = HashSet::new();
    for &node in &nodes {
        if node.kind() == "global_statement" {
            let mut scope = node;
            while let Some(parent) = scope.parent() {
                scope = parent;
                if parent.kind() == "function_definition" {
                    break;
                }
            }
            for name in children(node)
                .into_iter()
                .filter(|n| n.kind() == "identifier")
            {
                globals.insert((scope.start_byte() + 1, text(name, source).into()));
            }
        }
        if !top_level(node, root) {
            continue;
        }
        let (names, value) = match node.kind() {
            "variable_declarator" => {
                let Some(name) = node
                    .child_by_field_name("name")
                    .filter(|n| n.kind() == "identifier")
                else {
                    continue;
                };
                let value = node.child_by_field_name("value");
                let mutable = node
                    .parent()
                    .is_some_and(|p| !text(p, source).trim_start().starts_with("const "));
                let singleton = value
                    .is_some_and(|v| matches!(v.kind(), "object" | "array" | "new_expression"));
                if !mutable && !singleton {
                    continue;
                }
                (vec![name], value)
            }
            "assignment" if language == "py" => {
                let Some(name) = node
                    .child_by_field_name("left")
                    .filter(|n| n.kind() == "identifier")
                else {
                    continue;
                };
                (vec![name], node.child_by_field_name("right"))
            }
            "static_item" => (
                node.child_by_field_name("name").into_iter().collect(),
                node.child_by_field_name("value"),
            ),
            "var_spec" => {
                let mut cursor = node.walk();
                (
                    node.children_by_field_name("name", &mut cursor).collect(),
                    node.child_by_field_name("value"),
                )
            }
            _ => continue,
        };
        for name in names {
            declarations.push(Declaration {
                name: text(name, source).into(),
                node,
                name_node: name,
                singleton: value.is_some_and(|v| {
                    matches!(
                        v.kind(),
                        "object"
                            | "array"
                            | "dictionary"
                            | "list"
                            | "set"
                            | "new_expression"
                            | "composite_literal"
                    )
                }),
            });
        }
    }
    // A recovered tree cannot establish an access, even if a substring resembles one.
    if out.issues.is_empty() {
        for declaration in &declarations {
            if declarations
                .iter()
                .filter(|d| d.name == declaration.name)
                .count()
                != 1
            {
                out.issues
                    .insert(format!("ambiguous_module_binding:{}", declaration.name));
                continue;
            }
            let category = if declaration.singleton {
                "Singleton"
            } else {
                "Module"
            };
            out.emit(
                declaration.name_node,
                category,
                &format!("{}::{}", out.path, declaration.name),
                false,
                true,
            );
        }
        for &node in &nodes {
            if matches!(node.kind(), "call" | "call_expression") {
                if let Some(callee) = field(node, &["function"]) {
                    if let Some((module, name)) = bindings.api(callee, source) {
                        let access = match (module.as_str(), name.as_str()) {
                            ("os", "getenv" | "environ.get")
                            | ("std::env", "var" | "var_os")
                            | ("os", "Getenv" | "LookupEnv") => Some((true, false)),
                            ("os", "putenv" | "unsetenv" | "Setenv" | "Unsetenv")
                            | ("std::env", "set_var" | "remove_var") => Some((false, true)),
                            _ => None,
                        };
                        if let Some(access) = access {
                            let key = field(node, &["arguments"])
                                .and_then(|args| args.named_child(0))
                                .and_then(|arg| literal(arg, source));
                            out.environment(node, key, access);
                            continue;
                        }
                    }
                }
            }
            if member(node) {
                if let Some(object) = field(node, &["object", "value", "operand"]) {
                    let index = field(node, &["index", "subscript"]);
                    let property = field(node, &["property", "attribute", "field"]);
                    let script_environment = bindings.script.is_some_and(|script| {
                        access_path(object, source)
                            .is_some_and(|(base, suffix)| base == "process" && suffix == ".env")
                            && !script.state_binding_shadowed(node, "process", root, false)
                    });
                    let python_environment = bindings.python.is_some()
                        && bindings
                            .api(object, source)
                            .is_some_and(|(module, name)| module == "os" && name == "environ")
                        && index.is_some();
                    if script_environment || python_environment {
                        let key = if let Some(property) = property {
                            Some(text(property, source).into())
                        } else {
                            index.and_then(|i| literal(i, source))
                        };
                        out.environment(node, key, direction(node));
                        continue;
                    }
                }
            }
            if !matches!(node.kind(), "identifier") && !member(node) {
                continue;
            }
            // Inspect only the complete access, never its nested object/property names.
            if node.parent().is_some_and(|parent| member(parent)) {
                continue;
            }
            let Some((base, suffix)) = access_path(node, source) else {
                continue;
            };
            let matching = declarations
                .iter()
                .filter(|d| d.name == base)
                .collect::<Vec<_>>();
            if matching.len() != 1 {
                continue;
            }
            let declaration = matching[0];
            if contains(declaration.node, node) || bindings.shadowed(node, &base, root, &globals) {
                continue;
            }
            let parent = node.parent();
            if parent.is_some_and(|p| {
                matches!(
                    p.kind(),
                    "global_statement"
                        | "nonlocal_statement"
                        | "export_specifier"
                        | "import_specifier"
                        | "parameters"
                        | "formal_parameters"
                        | "parameter"
                        | "parameter_declaration"
                        | "short_var_declaration"
                        | "let_declaration"
                        | "variable_declarator"
                        | "var_spec"
                        | "static_item"
                )
            }) {
                continue;
            }
            if parent
                .is_some_and(|p| field(p, &["name"]).is_some_and(|name| name.id() == node.id()))
            {
                continue;
            }
            let category = if declaration.singleton || !suffix.is_empty() {
                "Singleton"
            } else {
                "Module"
            };
            let key = format!("{}::{base}{suffix}", out.path);
            let access = direction(node);
            out.emit(node, category, &key, access.0, access.1);
        }
        browser_storage::extract(&bindings, &nodes, root, &mut out);
        frameworks::extract(&bindings, &nodes, root, &mut out);
        references::extract(&bindings, &nodes, root, &mut out);
    }
    let coverage=serde_json::json!({"version":engram_core::modern_state::VERSION,"status":"partial_static_coverage","source_language":language,
        "browser_storage_contract":1,"browser_storage":"literal_getItem_setItem_removeItem; dynamic_keys_aliases_property_access_and_clear_all_unresolved",
        "environment_access":"literal_keys_with_source_import_or_unshadowed_process_evidence",
        "module_state":"file_local_declarations_and_explicit_accesses","cross_file_state_binding":"js_ts_static_es_imports; other_languages_not_implemented",
        "references":out.references,"script_module":bindings.script.map(|script| &script.facts),
        "react_redux":"file_local_import_bound_contexts_and_explicit_toolkit_store_reducers","runtime_behavior":"not_verified","issues":out.issues,
        "source_hash":blake3::hash(out.source.as_bytes()).to_hex().to_string()}).to_string();
    (out.symbols, out.edges, coverage)
}

#[cfg(test)]
mod tests;
