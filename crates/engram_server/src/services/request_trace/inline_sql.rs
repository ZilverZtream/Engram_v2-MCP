//! Literal VB ExecuteCommand dependencies in already source-verified methods.
//! These are lexical table references, not deployed database bindings or effects.
use super::{
    execution::Hop,
    routing::{self, Source},
};
use engram_graph::Node;
use engram_index::{
    sql_names::object_basename,
    sql_parser::{SqlOp, analyze_sql},
};
use serde_json::json;
use sqlparser::{
    dialect::MsSqlDialect,
    tokenizer::{Token, Tokenizer, Whitespace},
};
use std::collections::BTreeSet;

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
            || !tokens[i + 2].text.eq_ignore_ascii_case("ExecuteCommand")
            || tokens[i + 3].text != "("
        {
            continue;
        }
        if paths.len() >= cap {
            gaps.push("Inline SQL dependencies truncated at complete path limit".into());
            return;
        }
        let line = method.start_line + tokens[i].line as u32 - 1;
        let mut cursor = i + 4;
        let mut sql = String::new();
        while let Some(literal) = tokens.get(cursor).filter(|token| token.string) {
            sql.push_str(&literal.text);
            cursor += 1;
            if sql.len() > 16384 {
                break;
            }
            if tokens
                .get(cursor)
                .is_some_and(|t| matches!(t.text.as_str(), "&" | "+"))
            {
                cursor += 1;
            } else {
                break;
            }
        }
        if sql.is_empty()
            || sql.len() > 16384
            || !tokens
                .get(cursor)
                .is_some_and(|t| matches!(t.text.as_str(), "," | ")"))
        {
            if gaps.len() < 64 {
                gaps.push(format!("{}:{line}: ExecuteCommand SQL is dynamic or exceeds literal capture; table dependencies unresolved", source.path));
            }
            continue;
        }
        let Ok(sql_tokens) = Tokenizer::new(&MsSqlDialect {}, &sql).tokenize() else {
            if gaps.len() < 64 {
                gaps.push(format!("{}:{line}: ExecuteCommand SQL tokenization unavailable; table dependencies unresolved", source.path));
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
                    "{}:{line}: ExecuteCommand batch/procedure effects are not expanded",
                    source.path
                ));
            }
            continue;
        }
        let primary_kind = if matches!(
            analysis.operation,
            SqlOp::Insert | SqlOp::Update | SqlOp::Delete
        ) {
            "inline_sql_writes_table"
        } else {
            "inline_sql_reads_table"
        };
        let mut dependencies = BTreeSet::new();
        if let Some(table) = &analysis.primary_table {
            dependencies.insert((primary_kind, object_basename(table)));
        }
        for join in &analysis.joined_tables {
            dependencies.insert(("inline_sql_reads_table", object_basename(&join.table)));
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
                evidence: Some(json!({"origin":"current_method_literal_execute_command",
                    "file":source.path,"line":line,"source_blake3":blake3::hash(source.content.as_bytes()).to_hex().to_string(),
                    "receiver":tokens[i].text,"sql":sql,
                    "interpretation":"Lexical table reference in literal ExecuteCommand SQL at the cited source call. Receiver type, database/schema binding, branch reachability and execution are not proven."})),
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
