//! C# authored value syntax. Resolution and reaching definitions belong to the
//! graph post-pass, not to a regex over flattened method text.
use engram_core::value_flow::*;
use tree_sitter::Node;

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| !child.kind().contains("comment"))
        .collect()
}

struct Lower<'a> {
    source: &'a str,
    remaining: usize,
    issues: Vec<String>,
}

impl Lower<'_> {
    fn text(&self, node: Node<'_>) -> &str {
        &self.source[node.start_byte()..node.end_byte()]
    }

    fn site(&self, node: Node<'_>) -> FlowSite {
        let text = self.text(node);
        FlowSite {
            offset: node.start_byte() as u32,
            end_offset: node.end_byte() as u32,
            line: node.start_position().row as u32 + 1,
            column: node.start_position().column as u32 + 1,
            text: text.chars().take(MAX_SOURCE_TEXT_CHARS).collect(),
            text_truncated: text.chars().count() > MAX_SOURCE_TEXT_CHARS,
        }
    }

    fn issue(&mut self, text: &str) {
        if !self.issues.iter().any(|item| item == text) && self.issues.len() < 32 {
            self.issues.push(text.into());
        }
    }

    fn admit(&mut self, depth: usize) -> bool {
        if depth >= MAX_SYNTAX_DEPTH || self.remaining == 0 {
            self.issue(
                "Authored value syntax exceeds the depth/node budget; lineage is incomplete.",
            );
            return false;
        }
        self.remaining -= 1;
        true
    }

    fn statements<'tree>(
        &mut self,
        nodes: impl IntoIterator<Item = Node<'tree>>,
        depth: usize,
    ) -> Vec<FlowStatement> {
        let mut result = Vec::new();
        for node in nodes {
            result.push(self.statement(node, depth));
            if self.remaining == 0 {
                self.issue("Authored value syntax reaches the node budget; remaining statements may be omitted.");
                break;
            }
        }
        result
    }

    fn statement(&mut self, node: Node<'_>, depth: usize) -> FlowStatement {
        let mut out = FlowStatement {
            kind: "unknown".into(),
            site: self.site(node),
            ..Default::default()
        };
        if !self.admit(depth) {
            return out;
        }
        match node.kind() {
            "block" => {
                out.kind = "block".into();
                out.body = self.statements(children(node), depth + 1);
            }
            "local_declaration_statement" | "variable_declaration" => {
                // A declaration group does not introduce a lexical scope.
                out.kind = "sequence".into();
                let declaration = if node.kind() == "variable_declaration" {
                    Some(node)
                } else {
                    children(node)
                        .into_iter()
                        .find(|n| n.kind() == "variable_declaration")
                };
                if let Some(declaration) = declaration {
                    for variable in children(declaration)
                        .into_iter()
                        .filter(|n| n.kind() == "variable_declarator")
                    {
                        if !self.admit(depth + 1) {
                            out.body.push(FlowStatement {
                                kind: "unknown".into(),
                                site: self.site(variable),
                                ..Default::default()
                            });
                            break;
                        }
                        let Some(name) = variable.child_by_field_name("name") else {
                            self.issue(
                                "Deconstruction or unnamed local declaration is not modeled.",
                            );
                            out.body.push(FlowStatement {
                                kind: "unknown".into(),
                                site: self.site(variable),
                                ..Default::default()
                            });
                            continue;
                        };
                        let initializer =
                            children(variable).into_iter().find(|n| n.id() != name.id());
                        out.body.push(FlowStatement {
                            kind: "declare".into(),
                            site: self.site(variable),
                            name: Some(self.text(name).into()),
                            expression: initializer.map(|n| self.expression(n, depth + 2)),
                            ..Default::default()
                        });
                    }
                }
            }
            "expression_statement"
            | "return_statement"
            | "throw_statement"
            | "arrow_expression_clause" => {
                out.kind = match node.kind() {
                    "return_statement" | "arrow_expression_clause" => "return",
                    "throw_statement" => "throw",
                    _ => "expression",
                }
                .into();
                out.expression = children(node)
                    .first()
                    .map(|n| self.expression(*n, depth + 1));
            }
            "if_statement" => {
                out.kind = "if".into();
                out.expression = node
                    .child_by_field_name("condition")
                    .map(|n| self.expression(n, depth + 1));
                out.body = self.statements(node.child_by_field_name("consequence"), depth + 1);
                out.otherwise = self.statements(node.child_by_field_name("alternative"), depth + 1);
            }
            "empty_statement" => out.kind = "empty".into(),
            "local_function_statement" => {
                out.kind = "nested_callable".into();
                self.issue(
                    "Nested callable bodies and captured-local side effects are not modeled.",
                );
            }
            // Do not flatten loops/try/switch/using into a fictional single path.
            _ => self.issue(&format!(
                "Statement {} is not modeled; reaching values after it are incomplete.",
                node.kind()
            )),
        }
        out
    }

    fn expression(&mut self, node: Node<'_>, depth: usize) -> FlowExpression {
        let mut out = FlowExpression {
            kind: "unknown".into(),
            site: self.site(node),
            ..Default::default()
        };
        if !self.admit(depth) {
            return out;
        }
        match node.kind() {
            "identifier" => {
                out.kind = "name".into();
                out.name = Some(self.text(node).into());
            }
            kind if kind.ends_with("_literal") => out.kind = "literal".into(),
            "parenthesized_expression" | "await_expression" | "checked_expression" => {
                out.kind = "operation".into();
                out.operator = Some(node.kind().into());
                if let Some(value) = children(node).first() {
                    out.children.push(self.expression(*value, depth + 1));
                }
            }
            "cast_expression" => {
                out.kind = "operation".into();
                out.operator = Some("cast".into());
                if let Some(value) = node.child_by_field_name("value") {
                    out.children.push(self.expression(value, depth + 1));
                }
            }
            "binary_expression" | "assignment_expression" => {
                out.kind = if node.kind() == "assignment_expression" {
                    "assign"
                } else {
                    "operation"
                }
                .into();
                out.operator = node
                    .child_by_field_name("operator")
                    .map(|n| self.text(n).into());
                for field in ["left", "right"] {
                    if let Some(value) = node.child_by_field_name(field) {
                        out.children.push(self.expression(value, depth + 1));
                    }
                }
            }
            "prefix_unary_expression" | "postfix_unary_expression" => {
                let operand = children(node).into_iter().next();
                if let Some(value) = operand {
                    let operator = if node.kind() == "prefix_unary_expression" {
                        &self.source[node.start_byte()..value.start_byte()]
                    } else {
                        &self.source[value.end_byte()..node.end_byte()]
                    };
                    let operator = operator.trim();
                    out.kind = if matches!(operator, "++" | "--") {
                        "update"
                    } else {
                        "operation"
                    }
                    .into();
                    out.operator = Some(if out.kind == "update" {
                        format!(
                            "{}{operator}",
                            if node.kind() == "prefix_unary_expression" {
                                "pre"
                            } else {
                                "post"
                            }
                        )
                    } else {
                        operator.into()
                    });
                    out.children.push(self.expression(value, depth + 1));
                }
            }
            "conditional_expression" => {
                out.kind = "conditional".into();
                for field in ["condition", "consequence", "alternative"] {
                    if let Some(value) = node.child_by_field_name(field) {
                        out.children.push(self.expression(value, depth + 1));
                    }
                }
            }
            "member_access_expression" => {
                out.kind = "member".into();
                out.name = node
                    .child_by_field_name("name")
                    .map(|n| self.text(n).into());
                if let Some(receiver) = node.child_by_field_name("expression") {
                    out.children.push(self.expression(receiver, depth + 1));
                }
            }
            "this_expression" | "base_expression" | "this" | "base" => {
                out.kind = "receiver".into();
                out.name = Some(self.text(node).into());
            }
            "invocation_expression" => {
                if node.child_by_field_name("function").is_some_and(|n| self.text(n) == "nameof") {
                    out.kind = "literal".into();
                    return out;
                }
                out.kind = "call".into();
                if let Some(callee) = node.child_by_field_name("function") {
                    out.name = Some(self.text(callee).into());
                    // Keep the receiver separate from arguments. A callee name
                    // is not a read of a same-named local variable.
                    if callee.kind() == "member_access_expression" {
                        if let Some(receiver) = callee.child_by_field_name("expression") {
                            out.children.push(self.expression(receiver, depth + 1));
                        }
                    }
                }
                if let Some(arguments) = node.child_by_field_name("arguments") {
                    for (index, argument) in children(arguments)
                        .into_iter()
                        .filter(|n| n.kind() == "argument")
                        .enumerate()
                    {
                        if !self.admit(depth + 1) {
                            break;
                        }
                        let name = argument.child_by_field_name("name");
                        let value = children(argument)
                            .into_iter()
                            .find(|n| name.is_none_or(|name| n.id() != name.id()));
                        let mut cursor = argument.walk();
                        let by_ref = argument
                            .children(&mut cursor)
                            .any(|n| matches!(n.kind(), "ref" | "out" | "in"));
                        out.arguments.push(FlowArgument {
                            index: index as u32,
                            parameter_index: None,
                            name: name.map(|n| self.text(n).into()),
                            by_ref,
                            site: self.site(argument),
                            value: value
                                .map(|n| self.expression(n, depth + 2))
                                .unwrap_or_default(),
                        });
                    }
                }
            }
            _ => self.issue(&format!(
                "Expression {} is not modeled; value lineage is incomplete.",
                node.kind()
            )),
        }
        out
    }
}

pub(super) fn extract(method: Node<'_>, source: &str) -> FlowProgram {
    let mut lower = Lower {
        source,
        remaining: MAX_SYNTAX_NODES,
        issues: Vec::new(),
    };
    let mut parameters = Vec::new();
    if let Some(list) = method.child_by_field_name("parameters") {
        for (index, parameter) in children(list)
            .into_iter()
            .filter(|n| n.kind() == "parameter")
            .enumerate()
        {
            if !lower.admit(0) {
                break;
            }
            if let Some(name) = parameter.child_by_field_name("name") {
                let modifiers: Vec<_> = children(parameter)
                    .into_iter()
                    .filter(|n| n.kind() == "modifier")
                    .map(|n| lower.text(n))
                    .collect();
                let ty = parameter.child_by_field_name("type");
                parameters.push(FlowParameter {
                    name: lower.text(name).into(),
                    index: index as u32,
                    site: lower.site(name),
                    by_ref: modifiers.iter().any(|m| matches!(*m, "ref" | "out" | "in")),
                    variadic: modifiers.contains(&"params"),
                    optional: children(parameter).iter().any(|n| {
                        n.id() != name.id()
                            && ty.is_none_or(|ty| n.id() != ty.id())
                            && !matches!(n.kind(), "modifier" | "attribute_list")
                    }),
                });
            }
        }
        // In the pinned C# grammar a trailing params parameter is exposed as
        // direct list fields. Preserve its position and label variadic mapping.
        if let Some(name) = children(list)
            .into_iter()
            .find(|n| n.kind() == "identifier")
        {
            if lower.admit(0) {
                parameters.push(FlowParameter {
                    name: lower.text(name).into(),
                    index: parameters.len() as u32,
                    site: lower.site(name),
                    variadic: true,
                    ..Default::default()
                });
            }
        }
    }
    let body = if method.has_error() {
        lower.issue("Method contains syntax errors; value lineage is unavailable.");
        FlowStatement {
            kind: "unknown".into(),
            site: lower.site(method),
            ..Default::default()
        }
    } else if let Some(body) = method.child_by_field_name("body").or_else(|| {
        children(method)
            .into_iter()
            .find(|n| n.kind() == "arrow_expression_clause")
    }) {
        lower.statement(body, 0)
    } else {
        lower.issue(
            "Callable has no implementation body; only parameter definitions are available.",
        );
        FlowStatement {
            kind: "empty".into(),
            site: lower.site(method),
            ..Default::default()
        }
    };
    FlowProgram {
        version: 1,
        language: "cs".into(),
        offset_unit: "utf8_bytes".into(),
        parameters,
        body,
        issues: lower.issues,
    }
}

#[cfg(test)]
mod tests;
