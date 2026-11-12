use super::*;
use engram_core::value_flow::{FlowExpression, FlowSite, FlowStatement};
use std::collections::BTreeSet;

type Environment = Vec<BTreeMap<String, BTreeSet<String>>>;
const MAX_POINTS: usize = 16_384;
const MAX_LINKS: usize = 32_768;

pub(super) struct CallSite {
    pub result: String,
    pub expression: FlowExpression,
    pub arguments: Vec<String>,
}

pub(super) struct LocalFlow {
    pub owner: Node,
    pub program: FlowProgram,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub parameters: Vec<String>,
    pub returns: Vec<String>,
    pub calls: Vec<CallSite>,
    pub issues: Vec<String>,
    prefix: String,
}

pub(super) fn canonical(name: &str, language: &str) -> String {
    if matches!(language, "vb" | "vbnet") {
        name.trim_matches(['[', ']']).to_lowercase()
    } else {
        name.trim_start_matches('@').to_string()
    }
}

fn merge(a: Environment, b: Environment) -> Environment {
    a.into_iter()
        .zip(b)
        .map(|(mut left, right)| {
            for (name, values) in right {
                left.entry(name).or_default().extend(values);
            }
            left
        })
        .collect()
}

impl LocalFlow {
    pub fn build(owner: Node, program: FlowProgram) -> Self {
        let prefix = format!("value:{}:", blake3::hash(owner.node_id.as_bytes()).to_hex());
        let mut flow = Self {
            owner,
            issues: program.issues.clone(),
            program,
            prefix,
            nodes: vec![],
            edges: vec![],
            parameters: vec![],
            returns: vec![],
            calls: vec![],
        };
        let site = FlowSite {
            line: flow.owner.start_line,
            text: flow.owner.name.clone(),
            ..Default::default()
        };
        flow.point("summary", &site, &flow.owner.name.clone());
        let mut env = vec![BTreeMap::new()];
        for parameter in flow.program.parameters.clone() {
            let id = flow.point("parameter", &parameter.site, &parameter.name);
            if let Some(node) = flow.nodes.last_mut().filter(|node| node.node_id == id) {
                node.metadata.as_mut().unwrap()["parameter_index"] = json!(parameter.index);
            }
            flow.parameters.push(id.clone());
            env[0].insert(
                canonical(&parameter.name, &flow.owner.language),
                BTreeSet::from([id]),
            );
        }
        flow.statement(&flow.program.body.clone(), &mut env);
        flow
    }

    pub fn issue(&mut self, issue: impl Into<String>) {
        let issue = issue.into();
        if self.issues.len() < 32 && !self.issues.contains(&issue) {
            self.issues.push(issue);
        }
    }

    fn point(&mut self, role: &str, site: &FlowSite, name: &str) -> String {
        if self.nodes.len() >= MAX_POINTS {
            self.issue("Value point budget reached; lineage is partial.");
            return self.nodes[0].node_id.clone();
        }
        let id = format!("{}{}", self.prefix, self.nodes.len());
        self.nodes.push(Node { node_id: id.clone(), node_type: "value_point".into(),
            name: name.chars().take(120).collect(), namespace: self.owner.namespace.clone(),
            language: self.owner.language.clone(), file_path: self.owner.file_path.clone(),
            start_line: site.line.max(self.owner.start_line), end_line: site.line.max(self.owner.start_line), generation: self.owner.generation,
            metadata: Some(json!({"producer":"value_flow_v1", "owner_id":self.owner.node_id,
                "role":role,"site":site,"offset_unit":self.program.offset_unit,"runtime_value_proven":false})) });
        id
    }

    pub fn link(
        &mut self,
        from: &str,
        to: &str,
        relation: &str,
        call_site: Option<&str>,
        binding: Option<&str>,
    ) {
        if self.edges.len() >= MAX_LINKS {
            self.issue("Value edge budget reached; lineage is partial.");
            return;
        }
        self.edges.push(Edge { source_id: from.into(), target_id: to.into(), edge_kind: EdgeKind::ValueFlow,
            namespace: self.owner.namespace.clone(), language: self.owner.language.clone(), weight: 1,
            generation: self.owner.generation, updated_at_ms: 0,
            metadata: Some(json!({"relation":relation,"call_site":call_site,"binding":binding,
                "owner_id":self.owner.node_id,"evidence":"authored_static_lineage","runtime_value_proven":false})) });
    }

    fn replace(&self, env: &mut Environment, name: &str, id: &str) -> bool {
        let name = canonical(name, &self.owner.language);
        for scope in env.iter_mut().rev() {
            if let Some(values) = scope.get_mut(&name) {
                *values = BTreeSet::from([id.into()]);
                return true;
            }
        }
        false
    }

    fn invalidate(&mut self, env: &mut Environment, site: &FlowSite, reason: &str) {
        self.issue(reason);
        let unknown = self.point("unknown_reaching_value", site, reason);
        for scope in env {
            for values in scope.values_mut() {
                *values = BTreeSet::from([unknown.clone()]);
            }
        }
    }

    fn sequence(&mut self, statements: &[FlowStatement], env: &mut Environment) -> bool {
        for statement in statements {
            if !self.statement(statement, env) {
                return false;
            }
        }
        true
    }

    fn statement(&mut self, statement: &FlowStatement, env: &mut Environment) -> bool {
        match statement.kind.as_str() {
            "block" => {
                env.push(BTreeMap::new());
                let normal = self.sequence(&statement.body, env);
                env.pop();
                normal
            }
            "sequence" => self.sequence(&statement.body, env),
            "declare" => {
                let value = statement
                    .expression
                    .as_ref()
                    .map(|e| self.expression(e, env));
                let name = statement.name.as_deref().unwrap_or("?");
                let id = self.point("local_definition", &statement.site, name);
                if let Some(value) = value {
                    self.link(&value, &id, "initialize", None, None);
                } else {
                    self.issue(format!("{name}: no explicit initializer; default/uninitialized value is not inferred."));
                }
                env.last_mut()
                    .unwrap()
                    .insert(canonical(name, &self.owner.language), BTreeSet::from([id]));
                true
            }
            "expression" => {
                if let Some(e) = &statement.expression {
                    self.expression(e, env);
                }
                true
            }
            "return" | "throw" => {
                if let Some(e) = &statement.expression {
                    let value = self.expression(e, env);
                    let id = self.point(&statement.kind, &statement.site, &statement.site.text);
                    self.link(&value, &id, &statement.kind, None, None);
                    if statement.kind == "return" {
                        self.returns.push(id);
                    }
                }
                false
            }
            "if" => {
                if let Some(e) = &statement.expression {
                    self.expression(e, env);
                }
                let mut yes = env.clone();
                let mut no = env.clone();
                let yes_normal = self.sequence(&statement.body, &mut yes);
                let no_normal = self.sequence(&statement.otherwise, &mut no);
                match (yes_normal, no_normal) {
                    (true, true) => *env = merge(yes, no),
                    (true, false) => *env = yes,
                    (false, true) => *env = no,
                    (false, false) => return false,
                }
                true
            }
            "empty" | "nested_callable" => true,
            _ => {
                self.invalidate(
                    env,
                    &statement.site,
                    "Unsupported statement: subsequent local values are unknown.",
                );
                true
            }
        }
    }

    fn expression(&mut self, expression: &FlowExpression, env: &mut Environment) -> String {
        let name = expression.name.as_deref().unwrap_or(&expression.site.text);
        match expression.kind.as_str() {
            "name" => {
                let key = canonical(name, &self.owner.language);
                let values = env.iter().rev().find_map(|scope| scope.get(&key)).cloned();
                let id = self.point(
                    if values.is_some() {
                        "read"
                    } else {
                        "external_read"
                    },
                    &expression.site,
                    name,
                );
                if let Some(values) = values {
                    for value in values {
                        self.link(&value, &id, "definition_use", None, None);
                    }
                } else {
                    self.issue(format!("{name}: value is outside the modeled local scope."));
                }
                id
            }
            "literal" | "receiver" => self.point(&expression.kind, &expression.site, name),
            "assign" | "update" => {
                let Some(left) = expression.children.first() else {
                    return self.point("unknown", &expression.site, name);
                };
                let operation = expression.operator.as_deref().unwrap_or("=");
                if operation == "??=" {
                    self.invalidate(
                        env,
                        &expression.site,
                        "Conditional assignment is not modeled.",
                    );
                    return self.point("unknown", &expression.site, name);
                }
                let old = if operation != "=" || left.kind != "name" {
                    Some(self.expression(left, env))
                } else {
                    None
                };
                let right = expression.children.get(1).map(|e| self.expression(e, env));
                let id = self.point(
                    "write",
                    &expression.site,
                    left.name.as_deref().unwrap_or(&left.site.text),
                );
                if let Some(old) = &old {
                    self.link(old, &id, "update_operand", None, None);
                }
                if let Some(right) = right {
                    self.link(&right, &id, "assign", None, None);
                }
                if left.kind != "name"
                    || !self.replace(env, left.name.as_deref().unwrap_or(""), &id)
                {
                    self.issue("Assignment reaches a member, alias or unbound name; heap effects are not modeled.");
                }
                if operation.starts_with("post") {
                    old.unwrap_or(id)
                } else {
                    id
                }
            }
            "conditional" if expression.children.len() == 3 => {
                self.expression(&expression.children[0], env);
                let mut yes = env.clone();
                let mut no = env.clone();
                let a = self.expression(&expression.children[1], &mut yes);
                let b = self.expression(&expression.children[2], &mut no);
                *env = merge(yes, no);
                let id = self.point("conditional_value", &expression.site, name);
                self.link(&a, &id, "possible_value", None, None);
                self.link(&b, &id, "possible_value", None, None);
                id
            }
            "operation" | "member" => {
                let id = self.point(&expression.kind, &expression.site, name);
                let short = matches!(
                    expression
                        .operator
                        .as_deref()
                        .unwrap_or("")
                        .to_ascii_lowercase()
                        .as_str(),
                    "&&" | "||" | "??" | "andalso" | "orelse"
                );
                for (i, child) in expression.children.iter().enumerate() {
                    let before = env.clone();
                    let value = self.expression(child, env);
                    if short && i > 0 {
                        *env = merge(before, env.clone());
                    }
                    self.link(
                        &value,
                        &id,
                        if expression.kind == "member" {
                            "receiver_reference"
                        } else {
                            "operand"
                        },
                        None,
                        None,
                    );
                }
                if expression.kind == "member" {
                    self.issue("Member value depends on heap/property behavior not modeled by local lineage.");
                }
                id
            }
            "call" => {
                for receiver in &expression.children {
                    self.expression(receiver, env);
                }
                let mut arguments = Vec::new();
                for argument in &expression.arguments {
                    let value = self.expression(&argument.value, env);
                    let id = self.point("argument", &argument.site, &argument.site.text);
                    if let Some(node) = self.nodes.last_mut().filter(|node| node.node_id == id) {
                        node.metadata.as_mut().unwrap()["argument_index"] = json!(argument.index);
                    }
                    self.link(&value, &id, "argument_value", None, None);
                    arguments.push(id);
                }
                let result = self.point("call_result", &expression.site, name);
                self.calls.push(CallSite {
                    result: result.clone(),
                    expression: expression.clone(),
                    arguments,
                });
                for argument in &expression.arguments {
                    if argument.by_ref
                        || (language(&self.owner.language) == "vb"
                            && expression.target_semantic_id.is_none())
                    {
                        let unknown = self.point(
                            "unknown_reaching_value",
                            &argument.site,
                            "Possible ByRef write",
                        );
                        if argument.value.kind == "name" {
                            self.replace(
                                env,
                                argument.value.name.as_deref().unwrap_or(""),
                                &unknown,
                            );
                        }
                        self.issue("ByRef/out or unresolved VB argument can change a local; post-call value is unknown.");
                    }
                }
                if self
                    .program
                    .issues
                    .iter()
                    .any(|issue| issue.contains("captured-local"))
                {
                    self.invalidate(
                        env,
                        &expression.site,
                        "Call may change captured locals; capture effects are not modeled.",
                    );
                }
                result
            }
            _ => {
                self.invalidate(
                    env,
                    &expression.site,
                    "Unsupported expression: value and subsequent local effects are unknown.",
                );
                self.point("unknown", &expression.site, name)
            }
        }
    }

    pub fn finish(&mut self) {
        self.nodes[0].metadata.as_mut().unwrap()["issues"] = json!(self.issues);
        self.nodes[0].metadata.as_mut().unwrap()["status"] = json!(if self.issues.is_empty() {
            "supported_static_flow"
        } else {
            "partial"
        });
        self.nodes[0].metadata.as_mut().unwrap()["scope"] = json!(
            "Authored local value dependencies with conservative branch joins. Not runtime values, path feasibility, exception flow, heap alias analysis or proof of execution."
        );
    }
}
