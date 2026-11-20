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

pub(super) fn append(
    method: &Node,
    source: &Source<'_>,
    source_blake3: &str,
    tokens: &[Token],
    paths: &mut Vec<Hop>,
    gaps: &mut Vec<String>,
    cap: usize,
) {
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
    let file_line = |relative| method.start_line as usize + relative - 1;
    for (index, pair) in tokens.windows(9).enumerate() {
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
            continue;
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
            continue;
        }
        let Some(Some(declaration_line)) = declarations.get(&receiver.text.to_ascii_lowercase())
        else {
            continue;
        };
        if *declaration_line >= receiver.line {
            continue;
        }
        let Some(table) = qualified_table(&literal.text) else {
            continue;
        };
        if paths.len() >= cap {
            if gaps.len() < 64 {
                gaps.push(format!(
                    "{}:{}: bulk-copy writes truncated at the request path limit",
                    source.path,
                    file_line(invocation.line)
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
                "receiver": receiver.text,
                "declaration_line": file_line(*declaration_line),
                "destination_assignment_line": file_line(receiver.line),
                "call_site_lines": [file_line(invocation.line)],
                "operation": write.text,
                "target_spelling": literal.text,
                "interpretation": "Adjacent VB statements are lexical write candidates. Runtime destination, transaction, trigger options and execution are unverified.",
            })),
        });
    }
}
