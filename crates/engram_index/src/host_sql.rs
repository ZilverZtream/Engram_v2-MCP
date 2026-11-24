use crate::source_tokens::{Token, tokens};

pub fn literal_expression(tokens: &[Token], start: usize) -> Option<(String, usize)> {
    let mut cursor = start;
    let mut sql = String::new();
    loop {
        let literal = tokens.get(cursor).filter(|token| token.string)?;
        sql.push_str(&literal.text);
        if sql.len() > 16384 {
            return None;
        }
        cursor = continuation(tokens, cursor + 1);
        if tokens
            .get(cursor)
            .is_some_and(|token| matches!(token.text.as_str(), "&" | "+"))
        {
            cursor = continuation(tokens, cursor + 1);
        } else {
            return (!sql.is_empty()).then_some((sql, cursor));
        }
    }
}

fn continuation(tokens: &[Token], cursor: usize) -> usize {
    if tokens
        .get(cursor)
        .is_some_and(|token| !token.string && token.text == "_")
        && tokens
            .get(cursor + 1)
            .is_some_and(|next| next.line > tokens[cursor].line)
    {
        cursor + 1
    } else {
        cursor
    }
}

pub fn argument_start(tokens: &[Token], member: usize) -> Option<usize> {
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

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct TableSite {
    pub line: u32,
    pub table: String,
    pub access: &'static str,
    pub operation: String,
}

fn keyword(token: &Token, name: &str) -> bool {
    !token.string && token.text.eq_ignore_ascii_case(name)
}

/// Inspect one containing method. A configured bulk target is a potential
/// write; a dynamic command or destination cannot establish a table identity.
pub fn table_sites(source: &str, first_line: u32) -> Vec<TableSite> {
    use sqlparser::{
        dialect::MsSqlDialect,
        tokenizer::{Token as Sql, Tokenizer, Whitespace},
    };
    let input = tokens(source, true);
    let mut sites = std::collections::BTreeSet::new();
    for (i, t) in input.iter().enumerate() {
        if t.string || i == 0 {
            continue;
        }
        let call = (keyword(t, "ExecuteCommand") || keyword(t, "ExecuteQuery"))
            && input[i - 1].text == "."
            || keyword(t, "SqlCommand")
                && (keyword(&input[i - 1], "New") || input[i - 1].text == ".");
        let assignment = input[i - 1].text == "."
            && input.get(i + 1).is_some_and(|x| keyword(x, "="))
            && (keyword(t, "DestinationTableName") || keyword(t, "CommandText"));
        let start = if call {
            argument_start(&input, i)
        } else if assignment {
            Some(i + 2)
        } else {
            None
        };
        let Some(start) = start else { continue };
        let Some((literal, end)) = literal_expression(&input, start) else {
            continue;
        };
        if call
            && !input
                .get(end)
                .is_some_and(|x| !x.string && matches!(x.text.as_str(), "," | ")"))
        {
            continue;
        }
        if assignment
            && input
                .get(end)
                .is_some_and(|x| x.line == input[end - 1].line && x.text != ":")
        {
            continue;
        }
        let line = first_line + input[start].line as u32 - 1;
        if keyword(t, "DestinationTableName") {
            if literal.split('.').all(|part| {
                let name = part.trim().trim_start_matches('[').trim_end_matches(']');
                !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_')
            }) {
                sites.insert(TableSite {
                    line,
                    table: crate::sql_names::object_basename(&literal).to_lowercase(),
                    access: "write",
                    operation: "bulk destination assignment".into(),
                });
            }
            continue;
        }
        let Ok(sql) = Tokenizer::new(&MsSqlDialect {}, &literal).tokenize() else {
            continue;
        };
        let lexical = sql
            .into_iter()
            .map(|token| match token {
                Sql::Whitespace(
                    Whitespace::SingleLineComment { .. } | Whitespace::MultiLineComment(_),
                ) => " ".into(),
                Sql::Word(_) | Sql::Number(_, _) | Sql::Whitespace(_) => token.to_string(),
                _ if token.to_string().chars().any(char::is_alphabetic) => " ".into(),
                _ => token.to_string(),
            })
            .collect::<Vec<_>>()
            .join("");
        let written = crate::sql_parser::analyze_write_operations(&lexical);
        for table in crate::sql_parser::analyze_body_references(&lexical).tables_read {
            let name = crate::sql_names::object_basename(&table);
            if !written.iter().any(|op| {
                op.operation == "DELETE"
                    && crate::sql_names::object_basename(&op.table).eq_ignore_ascii_case(name)
            }) {
                sites.insert(TableSite {
                    line,
                    table: name.to_lowercase(),
                    access: "read",
                    operation: t.text.clone(),
                });
            }
        }
        for op in written {
            sites.insert(TableSite {
                line,
                table: crate::sql_names::object_basename(&op.table).to_lowercase(),
                access: "write",
                operation: op.operation.into(),
            });
        }
    }
    sites.into_iter().collect()
}
