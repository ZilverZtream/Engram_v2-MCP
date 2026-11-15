//! Source-level VB SqlCommand text candidates, not CLR/database execution proof.
use super::{
    execution::Hop,
    inline_sql,
    routing::{self, Source},
};
use engram_graph::Node;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

struct Sql {
    text: String,
    source: Value,
}
#[derive(Default)]
struct Receiver {
    name: String,
    declarations: usize,
    line: usize,
    constructor: Option<Sql>,
    assignments: Vec<Option<Sql>>,
    types: BTreeSet<&'static str>,
    executions: Vec<(String, usize)>,
}
fn word(tokens: &[routing::Token], at: usize, value: &str) -> bool {
    tokens
        .get(at)
        .is_some_and(|token| !token.string && token.text.eq_ignore_ascii_case(value))
}
fn capture(tokens: &[routing::Token], call: usize, start: usize, argument: bool) -> Option<Sql> {
    let (text, source, end) = if let Some((text, end)) =
        inline_sql::literal_expression(tokens, start)
    {
        (
            text,
            json!({"kind":if argument {"literal_argument"} else {"literal_command_text_assignment"},
            "relative_line":tokens[start].line}),
            end,
        )
    } else {
        let (text, source) = inline_sql::local_initializer(tokens, call, start, true)?;
        (text, source, start + 1)
    };
    let boundary = if argument {
        word(tokens, end, ",") || word(tokens, end, ")")
    } else {
        tokens
            .get(end)
            .is_none_or(|next| next.line > tokens[end - 1].line || next.text == ":")
    };
    boundary.then_some(Sql { text, source })
}
fn gap(gaps: &mut Vec<String>, source: &Source<'_>, line: u32, detail: &str) {
    if gaps.len() < 64 {
        gaps.push(format!(
            "{}:{line}: SqlCommand {detail}; text-table dependencies unresolved",
            source.path
        ));
    }
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
    let mut receivers: BTreeMap<String, Receiver> = BTreeMap::new();
    for i in 2..tokens.len() {
        if !word(&tokens, i, "New") {
            continue;
        }
        let variable = if word(&tokens, i - 1, "=") {
            Some(&tokens[i - 2])
        } else if i >= 3
            && word(&tokens, i - 1, "As")
            && (word(&tokens, i - 3, "Dim") || word(&tokens, i - 3, "Using"))
        {
            Some(&tokens[i - 2])
        } else {
            None
        };
        let Some(variable) = variable.filter(|value| !value.string && value.text.len() <= 128)
        else {
            continue;
        };
        let mut constructor = i + 1;
        while word(&tokens, constructor + 1, ".") && tokens.get(constructor + 2).is_some() {
            constructor += 2;
        }
        if !word(&tokens, constructor, "SqlCommand") || !word(&tokens, constructor + 1, "(") {
            continue;
        }
        let key = variable.text.to_ascii_lowercase();
        if receivers.len() >= 32 && !receivers.contains_key(&key) {
            gap(
                gaps,
                source,
                method.start_line + tokens[i].line as u32 - 1,
                "receiver inventory capped",
            );
            continue;
        }
        let receiver = receivers.entry(key).or_default();
        receiver.name = variable.text.clone();
        receiver.declarations += 1;
        receiver.line = tokens[i].line;
        receiver.constructor = capture(&tokens, i, constructor + 2, true);
    }
    for i in 0..tokens.len().saturating_sub(3) {
        if tokens[i].string || !word(&tokens, i + 1, ".") {
            continue;
        }
        let Some(receiver) = receivers.get_mut(&tokens[i].text.to_ascii_lowercase()) else {
            continue;
        };
        if word(&tokens, i + 2, "CommandText") && word(&tokens, i + 3, "=") {
            receiver.assignments.push(capture(&tokens, i, i + 4, false));
        }
        if word(&tokens, i + 2, "CommandType") && word(&tokens, i + 3, "=") {
            let mut value = i + 4;
            while word(&tokens, value + 1, ".") && tokens.get(value + 2).is_some() {
                value += 2;
            }
            receiver.types.insert(
                if value >= i + 6 && word(&tokens, value - 2, "CommandType") {
                    if word(&tokens, value, "Text") {
                        "Text"
                    } else if word(&tokens, value, "StoredProcedure") {
                        "StoredProcedure"
                    } else {
                        "unknown"
                    }
                } else {
                    "unknown"
                },
            );
        }
        if ["ExecuteReader", "ExecuteScalar", "ExecuteNonQuery"]
            .iter()
            .any(|name| word(&tokens, i + 2, name))
            && word(&tokens, i + 3, "(")
            && receiver.executions.len() < 32
        {
            receiver
                .executions
                .push((tokens[i + 2].text.clone(), tokens[i].line));
        }
    }
    for receiver in receivers.values() {
        if receiver.executions.is_empty() {
            continue;
        }
        // Keep the existing stored-procedure reader as the owner of that mode.
        if receiver.types.len() == 1 && receiver.types.contains("StoredProcedure") {
            continue;
        }
        let line = method.start_line + receiver.line as u32 - 1;
        if receiver.declarations != 1 || receiver.types.iter().any(|kind| *kind != "Text") {
            gap(gaps, source, line, "receiver/type is ambiguous or dynamic");
            continue;
        }
        let sql = match (&receiver.constructor, receiver.assignments.as_slice()) {
            (Some(sql), []) => Some(sql),
            (None, [Some(sql)]) => Some(sql),
            _ => None,
        };
        let Some(sql) = sql else {
            gap(
                gaps,
                source,
                line,
                "has no single supported literal or stable Const/Static text initializer",
            );
            continue;
        };
        for (invocation, relative_line) in &receiver.executions {
            if paths.len() >= cap {
                gap(gaps, source, line, "path inventory capped");
                return;
            }
            let mut sql_source = sql.source.clone();
            if let Some(relative) = sql_source.get("relative_line").and_then(Value::as_u64) {
                sql_source.as_object_mut().unwrap().remove("relative_line");
                sql_source["line"] = json!(u64::from(method.start_line) + relative - 1);
            }
            inline_sql::append_sql(
                method,
                source,
                &sql.text,
                json!({
                    "origin":"current_method_literal_sql_command_text","file":source.path,
                    "source_blake3":blake3::hash(source.content.as_bytes()).to_hex().to_string(),
                    "line":method.start_line + *relative_line as u32 - 1,"receiver":receiver.name,
                    "invocation":invocation,"command_declaration_line":line,"command_type":"Text",
                    "command_type_basis":if receiver.types.is_empty() {"SqlCommand source/default Text candidate"} else {"explicit CommandType.Text source"},
                    "sql":sql.text,"sql_source":sql_source,
                    "interpretation":"Lexical table references from a source SqlCommand text candidate and an observed source execute call. CLR receiver type, physical database/schema binding, branch reachability and runtime execution are not proven."
                }),
                paths,
                gaps,
                cap,
            );
        }
    }
}
