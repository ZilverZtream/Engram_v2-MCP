//! The common literal ON/INSERT/OFF wrapper, not a general SQL batch executor.
use super::{inline_sql, routing};
use engram_index::{
    sql_names::object_basename,
    sql_parser::{SqlOp, analyze_body_references, analyze_sql},
};
use serde_json::{Value, json};
use sqlparser::{
    dialect::MsSqlDialect,
    tokenizer::{Token, Tokenizer},
};

pub(crate) struct Batch {
    pub insert: String,
    pub facts: Value,
}
fn sql_word(tokens: &[Token], at: usize, text: &str) -> bool {
    matches!(tokens.get(at),Some(Token::Word(word)) if word.quote_style.is_none() && word.value.eq_ignore_ascii_case(text))
}
fn setting(tokens: &[Token], state: &str) -> Option<String> {
    if tokens.len() < 4
        || tokens.len() > 10
        || !sql_word(tokens, 0, "SET")
        || !sql_word(tokens, 1, "IDENTITY_INSERT")
        || !sql_word(tokens, tokens.len() - 1, state)
    {
        return None;
    }
    let name = &tokens[2..tokens.len() - 1];
    if name.len() % 2 != 1
        || name.iter().enumerate().any(|(i, t)| {
            if i % 2 == 0 {
                !matches!(t, Token::Word(_))
            } else {
                *t != Token::Period
            }
        })
    {
        return None;
    }
    Some(name.iter().map(ToString::to_string).collect())
}
pub(crate) fn parse(sql: &str) -> Option<Batch> {
    if sql.len() > 64 * 1024 {
        return None;
    }
    let tokens = Tokenizer::new(&MsSqlDialect {}, sql).tokenize().ok()?;
    let mut groups: Vec<Vec<Token>> = vec![vec![]];
    for token in tokens {
        match token {
            Token::Whitespace(_) => {}
            Token::SemiColon => {
                if !groups.last()?.is_empty() {
                    groups.push(vec![]);
                    if groups.len() > 4 {
                        return None;
                    }
                }
            }
            token => groups.last_mut()?.push(token),
        }
    }
    if groups.last()?.is_empty() {
        groups.pop();
    }
    if groups.len() != 3 {
        return None;
    }
    let target = setting(&groups[0], "ON")?;
    if setting(&groups[2], "OFF")? != target || !sql_word(&groups[1], 0, "INSERT") {
        return None;
    }
    let insert = groups[1]
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    let analysis = analyze_sql(&insert);
    if analysis.operation != SqlOp::Insert || analysis.is_multi_statement {
        return None;
    }
    let lexical = groups[1]
        .iter()
        .map(|t| match t {
            Token::Word(_) | Token::Number(_, _) => t.to_string(),
            _ if t.to_string().chars().any(char::is_alphabetic) => " ".into(),
            _ => t.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ");
    let references = analyze_body_references(&lexical);
    let writes: Vec<_> = references
        .tables_written
        .iter()
        .map(|t| object_basename(t))
        .collect();
    let reads: Vec<_> = references
        .tables_read
        .iter()
        .map(|t| object_basename(t))
        .collect();
    Some(Batch {
        insert,
        facts: json!({"kind":"literal_identity_insert_wrapper","nonempty_statements":3,
        "identity_target_spelling":target,"on_statement":1,"insert_statement":2,"off_statement":3,
        "insert_write_candidates":writes,"insert_read_candidates":reads,
        "interpretation":"Source has SET IDENTITY_INSERT ON, one INSERT, and OFF with matching target spellings. Statement indices refer to the SQL string, not VB file lines. This does not prove statement execution, physical binding, successful insertion, transaction atomicity, session identity or cleanup after an error."}),
    })
}
fn word(tokens: &[routing::Token], at: usize, text: &str) -> bool {
    tokens
        .get(at)
        .is_some_and(|t| !t.string && t.text.eq_ignore_ascii_case(text))
}
/// A small block-ancestry check for literal local observations. Unsupported
/// statement shapes refuse association rather than claiming compiler binding.
fn scope_at(tokens: &[routing::Token], position: usize) -> Option<Vec<(&'static str, usize)>> {
    let mut scope = Vec::new();
    let mut start = 0;
    while start < position {
        let line = tokens[start].line;
        let mut end = start + 1;
        while end < tokens.len() && tokens[end].line == line {
            end += 1;
        }
        let group = &tokens[start..end];
        if group
            .iter()
            .take(position.saturating_sub(start))
            .any(|t| !t.string && t.text == ":")
        {
            return None;
        }
        if !group[0].string {
            let first = group[0].text.to_ascii_lowercase();
            match first.as_str() {
                "if" if word(group, group.len() - 1, "Then") => scope.push(("if", start)),
                "else" | "elseif" => {
                    if scope.pop()?.0 != "if" {
                        return None;
                    }
                    scope.push(("if", start));
                }
                "try" => scope.push(("try", start)),
                "catch" | "finally" => {
                    if scope.pop()?.0 != "try" {
                        return None;
                    }
                    scope.push(("try", start));
                }
                "using" => scope.push(("using", start)),
                "for" => scope.push(("for", start)),
                "while" => scope.push(("while", start)),
                "do" => scope.push(("do", start)),
                "with" => scope.push(("with", start)),
                "synclock" => scope.push(("synclock", start)),
                "select" if word(group, 1, "Case") => scope.push(("select", start)),
                "case" => {
                    if scope.last().is_some_and(|s| s.0 == "case") {
                        scope.pop();
                    }
                    if scope.last()?.0 != "select" {
                        return None;
                    }
                    scope.push(("case", start));
                }
                "next" => {
                    if scope.pop()?.0 != "for" {
                        return None;
                    }
                }
                "loop" => {
                    if scope.pop()?.0 != "do" {
                        return None;
                    }
                }
                "end" => {
                    let kind = group.get(1)?.text.to_ascii_lowercase();
                    if kind == "select" && scope.last().is_some_and(|s| s.0 == "case") {
                        scope.pop();
                    }
                    if scope.pop()?.0 != kind {
                        return None;
                    }
                }
                _ => {}
            }
        }
        start = end;
    }
    Some(scope)
}
pub(super) fn local_initializer(
    tokens: &[routing::Token],
    call: usize,
    argument: usize,
) -> Option<(String, Value)> {
    let name = &tokens.get(argument).filter(|t| !t.string)?.text;
    let declarations: Vec<_> = tokens
        .iter()
        .enumerate()
        .filter(|(i, _)| word(tokens, *i, "Dim") && word(tokens, *i + 1, name))
        .map(|(i, _)| i)
        .collect();
    if declarations.len() != 1 {
        return None;
    }
    let declaration = declarations[0];
    if declaration >= call
        || (declaration > 0 && tokens[declaration - 1].line == tokens[declaration].line)
    {
        return None;
    }
    let mut expression = declaration + 2;
    if word(tokens, expression, "As") {
        if !word(tokens, expression + 1, "String") {
            return None;
        }
        expression += 2;
    }
    if !word(tokens, expression, "=") {
        return None;
    }
    let (sql, end) = inline_sql::literal_expression(tokens, expression + 1)?;
    if end > call
        || tokens
            .get(end)
            .is_some_and(|next| next.line == tokens[end - 1].line)
        || parse(&sql).is_none()
    {
        return None;
    }
    // Extra references before the constructor could mutate/escape the local.
    // Reject every other direct assignment in the member, without dataflow.
    if tokens[end..call]
        .iter()
        .any(|t| !t.string && t.text.eq_ignore_ascii_case(name))
        || tokens
            .iter()
            .enumerate()
            .any(|(i, _)| i != declaration + 1 && word(tokens, i, name) && word(tokens, i + 1, "="))
    {
        return None;
    }
    let declared = scope_at(tokens, declaration)?;
    let used = scope_at(tokens, call)?;
    if !used.starts_with(&declared) {
        return None;
    }
    Some((
        sql,
        json!({"kind":"local_dim_identity_initializer","name":name,"declaration_kind":"Dim",
        "relative_line":tokens[declaration].line,"association":"single literal local; supported lexical block ancestry; no intervening name reference/direct reassignment; compiler binding and runtime command state unverified"}),
    ))
}
