//! Direct VB bulk-copy writes with a nearby literal destination assignment.
use super::{
    execution::Hop,
    routing::{Source, Token},
};
use engram_graph::Node;
use serde_json::json;
use std::collections::BTreeMap;

fn word(token: &Token, expected: &str) -> bool {
    !token.string && token.text.eq_ignore_ascii_case(expected)
}

fn qualified_table(text: &str) -> Option<String> {
    let (schema, table) = text.trim().split_once('.')?;
    let identifier = |part: &str| {
        let part = part.trim();
        let bare = part
            .strip_prefix('[')
            .and_then(|part| part.strip_suffix(']'))
            .unwrap_or(part);
        (!bare.is_empty()
            && bare
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'))
        .then(|| bare.to_owned())
    };
    Some(format!("{}.{}", identifier(schema)?, identifier(table)?))
}

pub(crate) struct Write<'a> {
    pub table: String,
    pub receiver: &'a str,
    pub target_spelling: &'a str,
    pub operation: &'a str,
    pub declaration_line: u32,
    pub assignment_line: u32,
    pub call_line: u32,
}

pub(crate) fn writes(tokens: &[Token], first_line: u32) -> impl Iterator<Item = Write<'_>> {
    let mut declarations = BTreeMap::new();
    for (index, part) in tokens.windows(3).enumerate() {
        let [receiver, as_keyword, ty] = part else {
            continue;
        };
        let Some(last) = (word(ty, "SqlBulkCopy")).then_some(ty).or_else(|| {
            tokens
                .get(index + 3)
                .filter(|next| word(ty, "New") && word(next, "SqlBulkCopy"))
        }) else {
            continue;
        };
        if receiver.string || !word(as_keyword, "As") {
            continue;
        }
        declarations
            .entry(receiver.text.to_ascii_lowercase())
            .and_modify(|line| *line = None)
            .or_insert(Some(last.line));
    }
    let file_line = move |relative: usize| first_line + relative as u32 - 1;
    tokens
        .windows(9)
        .enumerate()
        .filter_map(move |(index, pair)| {
            let [
                receiver,
                dot,
                property,
                equals,
                literal,
                invocation,
                call_dot,
                write,
                open,
            ] = pair
            else {
                return None;
            };
            if receiver.string
                || !word(dot, ".")
                || !word(property, "DestinationTableName")
                || !word(equals, "=")
                || !literal.string
                || invocation.line <= literal.line
                || !word(invocation, &receiver.text)
                || !word(call_dot, ".")
                || !(word(write, "WriteToServer") || word(write, "WriteToServerAsync"))
                || !word(open, "(")
                || index
                    .checked_sub(1)
                    .is_some_and(|previous| word(&tokens[previous], "."))
            {
                return None;
            }
            let Some(Some(declaration_line)) =
                declarations.get(&receiver.text.to_ascii_lowercase())
            else {
                return None;
            };
            if *declaration_line >= receiver.line {
                return None;
            }
            Some(Write {
                table: qualified_table(&literal.text)?,
                receiver: &receiver.text,
                target_spelling: &literal.text,
                operation: &write.text,
                declaration_line: file_line(*declaration_line),
                assignment_line: file_line(receiver.line),
                call_line: file_line(invocation.line),
            })
        })
}

pub(super) fn append(
    method: &Node,
    source: &Source<'_>,
    source_blake3: &str,
    tokens: &[Token],
    paths: &mut Vec<Hop>,
    gaps: &mut Vec<String>,
    cap: usize,
) {
    for write in writes(tokens, method.start_line) {
        let table = write.table;
        if paths.len() >= cap {
            if gaps.len() < 64 {
                gaps.push(format!(
                    "{}:{}: bulk-copy writes truncated at the request path limit",
                    source.path, write.call_line
                ));
            }
            return;
        }
        paths.push(Hop {
            from: method.node_id.clone(),
            to: format!("sql-table-reference:{table}"),
            kind: "sql_bulk_copy_writes_table".into(),
            target_name: table,
            target_file: None,
            target_line: None,
            target_source_verified: false,
            table_mapping: None,
            evidence: Some(json!({
                "file": source.path,
                "source_blake3": source_blake3,
                "receiver": write.receiver,
                "declaration_line": write.declaration_line,
                "destination_assignment_line": write.assignment_line,
                "call_site_lines": [write.call_line],
                "operation": write.operation,
                "target_spelling": write.target_spelling,
                "interpretation": "Adjacent VB statements are lexical write candidates. Runtime destination, transaction, trigger options and execution are unverified.",
            })),
        });
    }
}
