//! Literal VB SQL-call dependencies in already source-verified methods.
//! These are lexical table references, not deployed database bindings or effects.
use super::{
    execution::Hop,
    routing::{self, Source},
};
use engram_graph::Node;
use engram_index::{
    sql_names::object_basename,
    sql_parser::{SqlOp, analyze_body_references, analyze_sql},
};
use serde_json::json;
use sqlparser::{
    dialect::MsSqlDialect,
    tokenizer::{Token, Tokenizer, Whitespace},
};
use std::collections::BTreeSet;

fn literal_expression(tokens: &[routing::Token], start: usize) -> Option<(String, usize)> {
    let mut cursor = start;
    let mut sql = String::new();
    loop {
        let literal = tokens.get(cursor).filter(|token| token.string)?;
        sql.push_str(&literal.text);
        if sql.len() > 16384 {
            return None;
        }
        cursor += 1;
        if tokens
            .get(cursor)
            .is_some_and(|token| matches!(token.text.as_str(), "&" | "+"))
        {
            cursor += 1;
        } else {
            return (!sql.is_empty()).then_some((sql, cursor));
        }
    }
}

fn local_constant(
    tokens: &[routing::Token],
    call: usize,
    argument: usize,
) -> Option<(String, serde_json::Value)> {
    let name = tokens
        .get(argument)
        .filter(|token| !token.string)?
        .text
        .as_str();
    if !tokens
        .get(argument + 1)
        .is_some_and(|token| matches!(token.text.as_str(), "," | ")"))
    {
        return None;
    }
    let declarations: Vec<_> = tokens[..call]
        .iter()
        .enumerate()
        .filter(|(index, token)| {
            !token.string
                && token.text.eq_ignore_ascii_case("Const")
                && tokens
                    .get(index + 1)
                    .is_some_and(|value| !value.string && value.text.eq_ignore_ascii_case(name))
        })
        .map(|(index, _)| index)
        .collect();
    if declarations.len() != 1 {
        return None;
    }
    let declaration = declarations[0];
    let mut expression = declaration + 2;
    if tokens.get(expression)?.text.eq_ignore_ascii_case("As") {
        if !tokens
            .get(expression + 1)?
            .text
            .eq_ignore_ascii_case("String")
        {
            return None;
        }
        expression += 2;
    }
    if tokens.get(expression)?.text != "=" {
        return None;
    }
    let (sql, end) = literal_expression(tokens, expression + 1)?;
    if end > call || (tokens[end].line == tokens[end - 1].line && tokens[end].text != ":") {
        return None;
    }
    // Do not borrow an initializer from a scope/branch that ended before this
    // call. General name binding and propagation remain outside this reader.
    if tokens[end..call].iter().any(|token| {
        !token.string
            && [
                "End", "Else", "ElseIf", "Next", "Loop", "Case", "Catch", "Finally",
            ]
            .iter()
            .any(|keyword| token.text.eq_ignore_ascii_case(keyword))
    }) {
        return None;
    }
    Some((
        sql,
        json!({"kind":"local_const_initializer","name":name,"declaration_kind":"Const",
        "relative_line":tokens[declaration].line}),
    ))
}

fn argument_start(tokens: &[routing::Token], member: usize) -> Option<usize> {
    let mut open = member + 1;
    if tokens.get(open)?.text != "(" {
        return None;
    }
    if tokens[member].text.eq_ignore_ascii_case("ExecuteQuery")
        && tokens.get(open + 1)?.text.eq_ignore_ascii_case("Of")
    {
        // VB generic type arguments precede the invocation's argument list.
        let mut depth = 1;
        open = (open + 1..tokens.len().min(open + 65)).find(|&index| {
            match tokens[index].text.as_str() {
                "(" => depth += 1,
                ")" => depth -= 1,
                _ => (),
            }
            depth == 0
        })? + 1;
    }
    (tokens.get(open)?.text == "(").then_some(open + 1)
}

pub(super) fn append(
    method: &Node,
    source: &Source<'_>,
    body: &str,
    paths: &mut Vec<Hop>,
    gaps: &mut Vec<String>,
    cap: usize,
) {
    if !matches!(method.language.as_str(), "vb" | "vbnet") {
        return;
    }
    let tokens = routing::tokens(body, true);
    for i in 0..tokens.len().saturating_sub(4) {
        if tokens[i].string
            || tokens[i + 1].text != "."
            || tokens[i + 2].string
            || !["ExecuteCommand", "ExecuteQuery"]
                .iter()
                .any(|name| tokens[i + 2].text.eq_ignore_ascii_case(name))
        {
            continue;
        }
        let Some(cursor) = argument_start(&tokens, i + 2) else {
            continue;
        };
        let invocation = if tokens[i + 2].text.eq_ignore_ascii_case("ExecuteQuery") {
            "ExecuteQuery"
        } else {
            "ExecuteCommand"
        };
        if paths.len() >= cap {
            gaps.push("Inline SQL dependencies truncated at complete path limit".into());
            return;
        }
        let line = method.start_line + tokens[i].line as u32 - 1;
        let captured = literal_expression(&tokens, cursor)
            .filter(|(_, end)| {
                tokens
                    .get(*end)
                    .is_some_and(|token| matches!(token.text.as_str(), "," | ")"))
            })
            .map(|(sql, _)| (sql, json!({"kind":"literal_argument"})))
            .or_else(|| local_constant(&tokens, i, cursor));
        let Some((sql, mut sql_source)) = captured else {
            if gaps.len() < 64 {
                gaps.push(format!("{}:{line}: {invocation} SQL is dynamic or outside supported literal/local Const capture; table dependencies unresolved", source.path));
            }
            continue;
        };
        let local = sql_source
            .get("relative_line")
            .and_then(|value| value.as_u64());
        if let Some(relative_line) = local {
            sql_source.as_object_mut().unwrap().remove("relative_line");
            sql_source["line"] = json!(u64::from(method.start_line) + relative_line - 1);
        }
        let Ok(sql_tokens) = Tokenizer::new(&MsSqlDialect {}, &sql).tokenize() else {
            if gaps.len() < 64 {
                gaps.push(format!("{}:{line}: {invocation} SQL tokenization unavailable; table dependencies unresolved", source.path));
            }
            continue;
        };
        // Keep identifier tokens, but remove comments and literal examples before
        // using the existing heuristic statement analyzer.
        let lexical = sql_tokens
            .iter()
            .map(|token| match token {
                Token::Whitespace(
                    Whitespace::SingleLineComment { .. } | Whitespace::MultiLineComment(_),
                ) => " ".into(),
                Token::Word(_) | Token::Whitespace(_) | Token::Number(_, _) => token.to_string(),
                _ if token.to_string().chars().any(char::is_alphabetic) => " ".into(),
                _ => token.to_string(),
            })
            .collect::<Vec<_>>()
            .join(" ");
        let analysis = analyze_sql(&lexical);
        if analysis.is_multi_statement || analysis.operation == SqlOp::Exec {
            if gaps.len() < 64 {
                gaps.push(format!(
                    "{}:{line}: {invocation} batch/procedure effects are not expanded",
                    source.path
                ));
            }
            continue;
        }
        let references = analyze_body_references(&lexical);
        let mut dependencies = BTreeSet::new();
        for table in references.tables_written {
            dependencies.insert(("inline_sql_writes_table", object_basename(&table)));
        }
        for table in references.tables_read {
            dependencies.insert(("inline_sql_reads_table", object_basename(&table)));
        }
        if dependencies.is_empty() && gaps.len() < 64 {
            gaps.push(format!(
                "{}:{line}: no supported literal SQL table reference identified",
                source.path
            ));
        }
        for (kind, table) in dependencies {
            if paths.len() >= cap {
                gaps.push("Inline SQL dependencies truncated at complete path limit".into());
                return;
            }
            paths.push(Hop {
                from: method.node_id.clone(), to: format!("sql-table-reference:{table}"),
                kind: kind.into(), target_name: table.into(), target_file: None, target_line: None,
                target_source_verified: false, table_mapping: None,
                evidence: Some(json!({"origin":if local.is_some() {"current_method_local_literal_sql"} else if invocation == "ExecuteQuery" {"current_method_literal_execute_query"} else {"current_method_literal_execute_command"},
                    "file":source.path,"line":line,"source_blake3":blake3::hash(source.content.as_bytes()).to_hex().to_string(),
                    "receiver":tokens[i].text,"invocation":invocation,"sql":sql,"sql_source":sql_source,
                    "interpretation":format!("Lexical table reference from a literal argument or cited same-method Const initializer for {invocation}. Receiver type, database/schema binding, branch reachability and execution are not proven.")})),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn dependencies(body: &str) -> (Vec<Hop>, Vec<String>) {
        let method = Node {
            node_id: "handler".into(),
            node_type: "function".into(),
            name: "Page_Load".into(),
            namespace: "memory".into(),
            file_path: engram_core::RelPath::new("Reset.aspx.vb"),
            start_line: 4,
            end_line: 20,
            language: "vbnet".into(),
            generation: 1,
            metadata: None,
        };
        let mut paths = vec![];
        let mut gaps = vec![];
        append(
            &method,
            &Source {
                path: "Reset.aspx.vb",
                content: body,
            },
            body,
            &mut paths,
            &mut gaps,
            128,
        );
        (paths, gaps)
    }
    #[test]
    fn constant_concatenated_update_reaches_its_table_with_call_evidence() {
        let (paths, gaps) = dependencies(
            "db.ExecuteCommand(\n\"UPDATE dbo.aspnet_Membership \" &\n\"SET PasswordQuestion = NULL WHERE UserId = {0}\", userId)\n",
        );
        assert!(gaps.is_empty());
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].kind, "inline_sql_writes_table");
        assert_eq!(paths[0].target_name, "aspnet_Membership");
        assert_eq!(paths[0].evidence.as_ref().unwrap()["line"], 4);
        assert!(!paths[0].target_source_verified);
    }
    #[test]
    fn typed_query_returns_literal_select_table_and_invocation_evidence() {
        let (paths, gaps) = dependencies(
            "Dim row = db.ExecuteQuery(Of Models.OrderRow)(\"SELECT * FROM dbo.Orders WHERE Id = {0}\", id).FirstOrDefault()\n",
        );
        assert!(gaps.is_empty());
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].kind, "inline_sql_reads_table");
        assert_eq!(paths[0].target_name, "Orders");
        let evidence = paths[0].evidence.as_ref().unwrap();
        assert_eq!(evidence["line"], 4);
        assert_eq!(evidence["invocation"], "ExecuteQuery");
        assert!(evidence["sql"].as_str().unwrap().contains("{0}"));
        assert!(!paths[0].target_source_verified);
    }
    #[test]
    fn query_returning_updated_rows_is_classified_by_sql_operation() {
        let (paths, gaps) = dependencies(
            "db.ExecuteQuery(Of Integer)(\"UPDATE dbo.Orders SET Status = {0} OUTPUT inserted.Id WHERE Id = {1}\", status, id)\n",
        );
        assert!(gaps.is_empty());
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].kind, "inline_sql_writes_table");
        assert_eq!(paths[0].target_name, "Orders");
    }
    #[test]
    fn typed_query_with_unknown_sql_keeps_dependencies_unresolved() {
        let (paths, gaps) = dependencies("db.ExecuteQuery(Of Integer)(sql, id)\n");
        assert!(paths.is_empty());
        assert_eq!(gaps.len(), 1);
        assert!(gaps[0].contains("ExecuteQuery SQL is dynamic"));
    }
    #[test]
    fn same_method_constant_sql_retains_initializer_and_call_locations() {
        let (paths, gaps) = dependencies(
            "Const sql As String =\n\"SELECT * FROM dbo.Orders \" &\n\"WHERE Id = {0}\"\nUsing db As New WarehouseContext()\ndb.ExecuteQuery(Of OrderRow)(sql, id)\nEnd Using\n",
        );
        assert!(gaps.is_empty());
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].target_name, "Orders");
        let evidence = paths[0].evidence.as_ref().unwrap();
        assert_eq!(evidence["line"], 8);
        assert_eq!(evidence["sql_source"]["line"], 4);
        assert_eq!(evidence["sql_source"]["name"], "sql");
        assert_eq!(evidence["sql"], "SELECT * FROM dbo.Orders WHERE Id = {0}");
    }
    #[test]
    fn constant_outside_call_scope_and_dynamic_initializers_remain_unresolved() {
        for body in [
            "If allowed Then\nConst sql = \"SELECT * FROM Orders\"\nEnd If\ndb.ExecuteQuery(Of Row)(sql)\n",
            "Const sql = \"SELECT * FROM \" & tableName\ndb.ExecuteQuery(Of Row)(sql)\n",
            "Dim sql = BuildQuery()\ndb.ExecuteCommand(sql)\n",
        ] {
            let (paths, gaps) = dependencies(body);
            assert!(paths.is_empty());
            assert_eq!(gaps.len(), 1);
        }
    }
    #[test]
    fn nested_existence_queries_and_outer_joined_tables_are_all_retained() {
        let (paths, gaps) = dependencies(
            "db.ExecuteQuery(Of UserRow)(\"SELECT u.Id, CASE WHEN EXISTS (SELECT 1 FROM dbo.TenantAccess ta WHERE ta.UserId = u.Id) THEN 1 ELSE 0 END, CASE WHEN EXISTS (SELECT 1 FROM dbo.UserAccess ua WHERE ua.UserId = u.Id) THEN 1 ELSE 0 END FROM dbo.Users u INNER JOIN dbo.Membership m ON m.UserId = u.Id LEFT JOIN dbo.UserRoles ur ON ur.UserId = u.Id LEFT JOIN dbo.Roles r ON r.Id = ur.RoleId WHERE u.Id = {0}\", id)\n",
        );
        assert!(gaps.is_empty());
        assert_eq!(
            paths
                .iter()
                .map(|path| path.target_name.as_str())
                .collect::<Vec<_>>(),
            vec![
                "Membership",
                "Roles",
                "TenantAccess",
                "UserAccess",
                "UserRoles",
                "Users"
            ]
        );
        assert!(
            paths
                .iter()
                .all(|path| path.kind == "inline_sql_reads_table")
        );
    }
    #[test]
    fn dynamic_expressions_are_unresolved_and_commented_calls_do_not_count() {
        let (paths, gaps) = dependencies(
            "' db.ExecuteCommand(\"DELETE FROM Secrets\")\ndb.ExecuteCommand(\"UPDATE \" & tableName & \" SET Value = 1\")\n",
        );
        assert!(paths.is_empty());
        assert_eq!(gaps.len(), 1);
        assert!(gaps[0].contains("dynamic"));
    }
    #[test]
    fn sql_examples_in_literals_and_comments_do_not_create_dependencies() {
        let (paths, gaps) = dependencies(
            "db.ExecuteCommand(\"UPDATE [dbo].[Settings] SET Label = 'JOIN Hidden' /* FROM Secret */\")\n",
        );
        assert!(gaps.is_empty());
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].target_name, "Settings");
    }
}
