use super::*;
use crate::parsing::SymbolExtractor;
use std::path::Path;

fn program(source: &str, name: &str) -> FlowProgram {
    let (symbols, _) = SymbolExtractor::new().extract(Path::new("Flow.cs"), source);
    let method = symbols
        .iter()
        .find(|n| n.kind == "function" && n.name == name)
        .unwrap_or_else(|| panic!("Missing authored method {name}: {symbols:?}"));
    serde_json::from_str(&method.metadata.as_ref().unwrap()[METADATA_KEY]).unwrap()
}

fn expressions<'a>(statement: &'a FlowStatement, out: &mut Vec<&'a FlowExpression>) {
    fn visit<'a>(value: &'a FlowExpression, out: &mut Vec<&'a FlowExpression>) {
        out.push(value);
        for child in &value.children {
            visit(child, out);
        }
        for argument in &value.arguments {
            visit(&argument.value, out);
        }
    }
    if let Some(value) = &statement.expression {
        visit(value, out);
    }
    for child in statement.body.iter().chain(&statement.otherwise) {
        expressions(child, out);
    }
}

#[test]
fn authored_local_values_keep_assignment_order_and_exact_source_sites() {
    let source = "class Flow {\n int Run(int input) {\n  int alias = input;\n  alias = 42;\n  return Helper(alias);\n }\n int Helper(int value) { return value; }\n}";
    let flow = program(source, "Run");
    assert!(flow.issues.is_empty(), "{:?}", flow.issues);
    assert_eq!(flow.parameters[0].name, "input");
    assert_eq!(flow.parameters[0].index, 0);
    let declaration = &flow.body.body[0].body[0];
    assert_eq!(declaration.name.as_deref(), Some("alias"));
    assert_eq!(
        declaration.expression.as_ref().unwrap().name.as_deref(),
        Some("input")
    );
    assert_eq!(declaration.site.line, 3);
    let write = flow.body.body[1].expression.as_ref().unwrap();
    assert_eq!(write.kind, "assign");
    assert_eq!(write.operator.as_deref(), Some("="));
    assert_eq!(write.children[1].site.text, "42");
    let returned = &flow.body.body[2];
    assert_eq!(returned.kind, "return");
    let call = returned.expression.as_ref().unwrap();
    assert_eq!(call.name.as_deref(), Some("Helper"));
    assert_eq!(call.arguments[0].value.name.as_deref(), Some("alias"));
    assert_eq!(
        &source[call.site.offset as usize..call.site.end_offset as usize],
        "Helper(alias)"
    );
    // The helper body must not be included in Run's value facts.
    let mut facts = Vec::new();
    expressions(&flow.body, &mut facts);
    assert!(!facts.iter().any(|v| v.name.as_deref() == Some("value")));
}

#[test]
fn blocks_branches_and_shadowed_locals_remain_distinct() {
    let flow = program(
        "class Flow { int Run(int input, bool flag) { int value = input; if (flag) { int branch = value; value = branch + 1; } else { value = 2; } { int input = 9; Consume(input); } return value; } }",
        "Run",
    );
    assert!(flow.issues.is_empty(), "{:?}", flow.issues);
    let branch = &flow.body.body[1];
    assert_eq!(branch.kind, "if");
    assert_eq!(
        branch.expression.as_ref().unwrap().name.as_deref(),
        Some("flag")
    );
    assert_eq!(branch.body[0].kind, "block");
    assert_eq!(branch.otherwise[0].kind, "block");
    let scope = &flow.body.body[2];
    assert_eq!(scope.kind, "block");
    let shadow = &scope.body[0].body[0];
    assert_eq!(shadow.name.as_deref(), Some("input"));
    assert_ne!(shadow.site.offset, flow.parameters[0].site.offset);
    assert_eq!(flow.body.body[3].kind, "return");
}

#[test]
fn arguments_keep_names_positions_and_ref_modes_without_fake_type_or_member_reads() {
    let flow = program(
        "class Flow { int Run(ref int input, int count = 2, params string[] labels) { return obj.Send(second: ref input, first: (int)count, text: \"input\"); } }",
        "Run",
    );
    assert!(flow.issues.is_empty(), "{:?}", flow.issues);
    assert!(flow.parameters[0].by_ref);
    assert!(flow.parameters[1].optional);
    assert!(flow.parameters[2].variadic);
    assert_eq!(flow.parameters[2].index, 2);
    let call = flow.body.body[0].expression.as_ref().unwrap();
    assert_eq!(call.name.as_deref(), Some("obj.Send"));
    assert_eq!(call.children[0].name.as_deref(), Some("obj"));
    assert_eq!(
        call.arguments.iter().map(|a| a.index).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert_eq!(call.arguments[0].name.as_deref(), Some("second"));
    assert!(call.arguments[0].by_ref);
    let mut facts = Vec::new();
    expressions(&flow.body, &mut facts);
    let names: Vec<_> = facts
        .iter()
        .filter(|e| e.kind == "name")
        .map(|e| e.name.as_deref().unwrap())
        .collect();
    assert_eq!(names, vec!["obj", "input", "count"]);
    assert!(
        facts
            .iter()
            .any(|e| e.kind == "literal" && e.site.text == "\"input\"")
    );
}

#[test]
fn expression_bodies_and_value_modifying_operators_are_not_plain_reads() {
    let flow = program(
        "class Flow { int Run(int input) => Helper(input++); int Other(int count) { count += 2; return -count; } }",
        "Run",
    );
    assert_eq!(flow.body.kind, "return");
    let argument = &flow.body.expression.as_ref().unwrap().arguments[0].value;
    assert_eq!(argument.kind, "update");
    assert_eq!(argument.operator.as_deref(), Some("post++"));
    let other = program(
        "class Flow { int Other(int count) { count += 2; return -count; } }",
        "Other",
    );
    assert_eq!(
        other.body.body[0]
            .expression
            .as_ref()
            .unwrap()
            .operator
            .as_deref(),
        Some("+=")
    );
    assert_eq!(
        other.body.body[1]
            .expression
            .as_ref()
            .unwrap()
            .operator
            .as_deref(),
        Some("-")
    );
}

#[test]
fn unsupported_control_flow_and_nested_callables_are_visible_and_not_flattened() {
    let flow = program(
        "class Flow { int Run(int input) { while (input > 0) { input--; } int Local() { return input; } return input; } }",
        "Run",
    );
    assert_eq!(flow.body.body[0].kind, "unknown");
    assert!(flow.issues.iter().any(|s| s.contains("while_statement")));
    assert_eq!(flow.body.body[1].kind, "nested_callable");
    assert!(flow.issues.iter().any(|s| s.contains("captured-local")));
    let mut facts = Vec::new();
    expressions(&flow.body, &mut facts);
    assert_eq!(
        facts.iter().filter(|e| e.kind == "name").count(),
        1,
        "Loop and closure bodies must not masquerade as one straight execution path"
    );
}

#[test]
fn syntax_budget_and_bodyless_contracts_disclose_missing_lineage() {
    let nested = format!(
        "class Flow {{ int Run(int input) => {}input{}; }}",
        "Helper(".repeat(80),
        ")".repeat(80)
    );
    let deep = program(&nested, "Run");
    assert!(
        deep.issues.iter().any(|s| s.contains("depth/node budget")),
        "Deep syntax must remain readable with an explicit incomplete boundary"
    );
    let source = format!(
        "class Flow {{ int Run(int input) {{ {} return input; }} }}",
        "input = input + 1;".repeat(MAX_SYNTAX_NODES)
    );
    let flow = program(&source, "Run");
    assert!(flow.issues.iter().any(|s| s.contains("budget")));
    let contract = program("interface Flow { int Run(int input); }", "Run");
    assert_eq!(contract.parameters[0].name, "input");
    assert!(
        contract
            .issues
            .iter()
            .any(|s| s.contains("no implementation body"))
    );
}
