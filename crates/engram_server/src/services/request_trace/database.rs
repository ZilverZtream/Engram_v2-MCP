//! On-demand SP analysis already used by migration reports, scoped here to reached
//! methods. No graph writes and no database execution or default-schema guesses.
use super::{
    execution::Hop,
    routing::{self, Source},
};
use serde_json::{Value, json};
use sqlparser::{
    dialect::MsSqlDialect,
    tokenizer::{Location, Token, Tokenizer, Whitespace},
};
use std::collections::{BTreeMap, BTreeSet};

pub(super) struct Catalog {
    definitions: Vec<Definition>,
    pub gaps: Vec<String>,
}
struct Definition {
    key: Vec<String>,
    name: String,
    path: String,
    line: u32,
    end: u32,
    hash: String,
    excerpt: String,
    reads: Vec<String>,
    writes: Vec<String>,
    dynamic: bool,
}

fn name_key(name: &str) -> Option<Vec<String>> {
    let tokens = Tokenizer::new(&MsSqlDialect {}, name).tokenize().ok()?;
    let tokens: Vec<_> = tokens
        .into_iter()
        .filter(|token| !matches!(token, Token::Whitespace(_)))
        .collect();
    if tokens.is_empty() || tokens.len() > 7 {
        return None;
    }
    let mut names = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        if index % 2 == 0 {
            let Token::Word(word) = token else {
                return None;
            };
            names.push(word.value.to_ascii_lowercase());
        } else if *token != Token::Period {
            return None;
        }
    }
    (tokens.len() % 2 == 1).then_some(names)
}

fn offset(source: &str, lines: &[usize], location: Location) -> usize {
    let line = location.line.saturating_sub(1) as usize;
    let start = lines.get(line).copied().unwrap_or(source.len());
    start
        + source[start..]
            .char_indices()
            .nth(location.column.saturating_sub(1) as usize)
            .map_or(source.len() - start, |(i, _)| i)
}

impl Catalog {
    pub fn new(sources: &[Source<'_>]) -> Self {
        let mut result = Self {
            definitions: Vec::new(),
            gaps: Vec::new(),
        };
        for source in sources
            .iter()
            .filter(|s| s.path.to_ascii_lowercase().ends_with(".sql"))
        {
            let lines: Vec<_> = std::iter::once(0)
                .chain(source.content.match_indices('\n').map(|(i, _)| i + 1))
                .collect();
            let tokens =
                match Tokenizer::new(&MsSqlDialect {}, source.content).tokenize_with_location() {
                    Ok(tokens) => tokens,
                    Err(_) => {
                        if result.gaps.len() < 16 {
                            result.gaps.push(format!(
                                "{}: SQL tokenization unavailable; procedure evidence withheld",
                                source.path
                            ));
                        }
                        continue;
                    }
                };
            // Preserve original byte/line positions while excluding comments and
            // string examples from the existing lexical SP analyzer.
            let mut masked = source.content.as_bytes().to_vec();
            for token in &tokens {
                let comment = matches!(
                    &token.token,
                    Token::Whitespace(
                        Whitespace::SingleLineComment { .. } | Whitespace::MultiLineComment(_)
                    )
                );
                let literal = !matches!(
                    &token.token,
                    Token::Word(_) | Token::Whitespace(_) | Token::Number(_, _) | Token::EOF
                ) && token.token.to_string().chars().any(char::is_alphabetic);
                if comment || literal {
                    for byte in &mut masked[offset(source.content, &lines, token.span.start)
                        ..offset(source.content, &lines, token.span.end)]
                    {
                        if !matches!(*byte, b'\n' | b'\r') {
                            *byte = b' ';
                        }
                    }
                }
            }
            let masked = String::from_utf8(masked).expect("whole-token masking preserves UTF-8");
            let significant: Vec<_> = tokens
                .iter()
                .filter(|t| !matches!(t.token, Token::Whitespace(_)))
                .collect();
            let keyword = |index: usize, expected: &str| {
                significant.get(index).is_some_and(|t|matches!(&t.token,Token::Word(w) if w.quote_style.is_none()&&w.value.eq_ignore_ascii_case(expected)))
            };
            let mut starts = Vec::new();
            for i in 0..significant.len() {
                let mut j = i;
                if keyword(j, "CREATE") {
                    j += 1;
                    if keyword(j, "OR") && keyword(j + 1, "ALTER") {
                        j += 2;
                    }
                } else if keyword(j, "ALTER") && !(i > 0 && keyword(i - 1, "OR")) {
                    j += 1;
                } else {
                    continue;
                }
                if !keyword(j, "PROC") && !keyword(j, "PROCEDURE") {
                    continue;
                }
                j += 1;
                let begin = j;
                if !significant
                    .get(j)
                    .is_some_and(|t| matches!(t.token, Token::Word(_)))
                {
                    continue;
                }
                j += 1;
                while significant.get(j).is_some_and(|t| t.token == Token::Period)
                    && significant
                        .get(j + 1)
                        .is_some_and(|t| matches!(t.token, Token::Word(_)))
                {
                    j += 2;
                }
                let name = significant[begin..j]
                    .iter()
                    .map(|t| t.token.to_string())
                    .collect::<String>();
                if let Some(key) = name_key(&name) {
                    starts.push((
                        offset(source.content, &lines, significant[i].span.start),
                        significant[i].span.start.line as u32,
                        name,
                        key,
                    ));
                }
                if starts.len() > 64 {
                    break;
                }
            }
            for (i, (start, line, name, key)) in starts.iter().enumerate() {
                if result.definitions.len() >= 64 {
                    result.gaps.push("Procedure inventory truncated at 64 declarations; remaining SQL source not inspected".into());
                    return result;
                }
                let end = starts.get(i + 1).map_or(source.content.len(), |s| s.0);
                let parsed = engram_index::sp_extractor::parse_sp_definitions(&masked[*start..end]);
                let [definition] = parsed.as_slice() else {
                    if result.gaps.len() < 16 {
                        result.gaps.push(format!(
                            "{}:{line}: procedure body analysis ambiguous",
                            source.path
                        ));
                    }
                    continue;
                };
                result.definitions.push(Definition {
                    key: key.clone(),
                    name: name.clone(),
                    path: source.path.into(),
                    line: *line,
                    end: line
                        + source.content[*start..end]
                            .lines()
                            .count()
                            .saturating_sub(1) as u32,
                    hash: blake3::hash(source.content.as_bytes()).to_hex().to_string(),
                    excerpt: source.content[*start..end].chars().take(2048).collect(),
                    reads: definition.tables_read.iter().take(64).cloned().collect(),
                    writes: definition.tables_written.iter().take(64).cloned().collect(),
                    dynamic: definition.has_dynamic_sql,
                });
            }
        }
        result
    }

    pub fn append(
        &self,
        project: &str,
        method: &engram_graph::Node,
        body: &str,
        hops: &mut Vec<Hop>,
        members: &mut Vec<Value>,
        gaps: &mut Vec<String>,
        cap: usize,
    ) {
        for command in commands(body, method.language == "vb" || method.language == "vbnet") {
            if hops.len() >= cap {
                gaps.push("Database evidence truncated at complete path limit".into());
                break;
            }
            let matches: Vec<_> = self
                .definitions
                .iter()
                .filter(|d| d.key == command.key)
                .collect();
            let definition = if matches.len() == 1 {
                Some(matches[0])
            } else {
                None
            };
            let id = definition.map_or_else(
                || format!("unresolved-sp:{}", command.name),
                |d| format!("source-sp:{}:{}:{}", d.path, d.line, d.name),
            );
            hops.push(Hop {from:method.node_id.clone(),to:id.clone(),kind:"calls_stored_procedure".into(),target_name:command.name.clone(),
                target_file:definition.map(|d|d.path.clone()),target_line:definition.map(|d|d.line),target_source_verified:definition.is_some(),
                evidence:Some(json!({"origin":"current_method_source_command_candidate","receiver":command.receiver,"call_site_line":method.start_line+command.line as u32-1,
                    "interpretation":"Single literal command name and StoredProcedure assignment on this local receiver. Aliasing, branch reachability, server identity and successful execution are not proven."}))});
            let Some(definition) = definition else {
                gaps.push(format!("{}: {} exact qualified procedure declarations; no default schema or basename binding inferred",command.name,matches.len()));
                continue;
            };
            members.push(json!({"node_id":id,"file":definition.path,"line_start":definition.line,"line_end":definition.end,"source_blake3":definition.hash,
                "excerpt":definition.excerpt,"interpretation":"Current SQL source matched by its full authored procedure name; definition deployment/database identity and runtime effects are not proven.",
                "recovery":{"tool":"get_full_method_body","arguments":{"project_id":project,"file_path":definition.path,"line_start":definition.line,"context_lines":0}}}));
            for (kind, tables) in [
                ("stored_proc_reads_table", &definition.reads),
                ("stored_proc_writes_table", &definition.writes),
            ] {
                for table in tables {
                    if hops.len() >= cap {
                        gaps.push(
                            "Database table evidence truncated at complete path limit".into(),
                        );
                        break;
                    }
                    hops.push(Hop {from:id.clone(),to:format!("sql-table-reference:{table}"),kind:kind.into(),target_name:table.clone(),target_file:None,target_line:None,target_source_verified:false,
                        evidence:Some(json!({"origin":"existing_source_sp_analyzer","file":definition.path,"line":definition.line,"source_blake3":definition.hash,"interpretation":"Lexical table reference in the cited procedure. Database/schema binding, branch reachability and execution are not established."}))});
                }
            }
            if definition.dynamic {
                gaps.push(format!(
                    "{}: dynamic SQL effects remain unresolved",
                    definition.name
                ));
            }
        }
    }
}

struct Command {
    name: String,
    key: Vec<String>,
    receiver: String,
    line: usize,
}
#[derive(Default)]
struct Receiver {
    names: BTreeSet<String>,
    types: BTreeSet<String>,
    executions: Vec<usize>,
    declarations: usize,
}

fn commands(body: &str, vb: bool) -> Vec<Command> {
    let tokens = routing::tokens(body, vb);
    let key = |name: &str| {
        if vb {
            name.to_ascii_lowercase()
        } else {
            name.to_owned()
        }
    };
    let word = |i: usize, name: &str| {
        tokens
            .get(i)
            .is_some_and(|t| !t.string && t.text.eq_ignore_ascii_case(name))
    };
    let mut receivers: BTreeMap<String, Receiver> = BTreeMap::new();
    for i in 0..tokens.len() {
        if !word(i, "New") || i < 2 {
            continue;
        }
        let variable = if word(i - 1, "=") {
            Some(&tokens[i - 2])
        } else if i >= 3 && word(i - 1, "As") && word(i - 3, "Dim") {
            Some(&tokens[i - 2])
        } else {
            None
        };
        let Some(variable) = variable.filter(|v| !v.string && v.text.len() <= 128) else {
            continue;
        };
        let mut j = i + 1;
        while j + 2 < tokens.len() && tokens[j + 1].text == "." {
            j += 2;
        }
        if !word(j, "SqlCommand") || !word(j + 1, "(") {
            continue;
        }
        if receivers.len() >= 32 && !receivers.contains_key(&key(&variable.text)) {
            continue;
        }
        let receiver = receivers.entry(key(&variable.text)).or_default();
        receiver.declarations += 1;
        if let Some(name) = tokens
            .get(j + 2)
            .filter(|t| t.string && t.text.len() <= 512 && (word(j + 3, ",") || word(j + 3, ")")))
        {
            receiver.names.insert(name.text.clone());
        } else if !word(j + 2, ")") {
            receiver.names.insert("<dynamic>".into());
        }
    }
    for i in 0..tokens.len().saturating_sub(3) {
        if tokens[i].string || tokens[i + 1].text != "." {
            continue;
        }
        let Some(receiver) = receivers.get_mut(&key(&tokens[i].text)) else {
            continue;
        };
        if word(i + 2, "CommandText") && word(i + 3, "=") {
            receiver.names.insert(
                tokens
                    .get(i + 4)
                    .filter(|t| {
                        t.string
                            && t.text.len() <= 512
                            && tokens.get(i + 5).is_none_or(|next| {
                                next.text == ";" || (vb && (next.text == ":" || next.line > t.line))
                            })
                    })
                    .map_or_else(|| "<dynamic>".into(), |t| t.text.clone()),
            );
        }
        if word(i + 2, "CommandType") && word(i + 3, "=") {
            let mut j = i + 4;
            while j + 2 < tokens.len() && tokens[j + 1].text == "." {
                j += 2;
            }
            receiver.types.insert(
                if j >= i + 6 && word(j - 2, "CommandType") && word(j, "StoredProcedure") {
                    "stored_procedure"
                } else {
                    "other_or_dynamic"
                }
                .into(),
            );
        }
        if ["ExecuteNonQuery", "ExecuteReader", "ExecuteScalar"]
            .iter()
            .any(|name| word(i + 2, name))
            && word(i + 3, "(")
            && receiver.executions.len() < 32
        {
            receiver.executions.push(tokens[i].line);
        }
    }
    let mut result = Vec::new();
    for (name, receiver) in receivers {
        if receiver.declarations != 1
            || receiver.names.len() != 1
            || receiver.types.len() != 1
            || !receiver.types.contains("stored_procedure")
        {
            continue;
        }
        let target = receiver.names.into_iter().next().expect("one name");
        let Some(parts) = name_key(&target) else {
            continue;
        };
        for line in receiver.executions {
            result.push(Command {
                name: target.clone(),
                key: parts.clone(),
                receiver: name.clone(),
                line,
            });
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn qualified_command_constructor_tracks_its_receiver_and_ignores_examples() {
        let source = "Dim cmd As New SqlCommand(\"dbo.CreateOrder\")\ncmd.CommandType = System.Data.CommandType.StoredProcedure\ncmd.ExecuteNonQuery()\n' fake.ExecuteNonQuery()\n";
        let found = commands(source, true);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key, vec!["dbo", "createorder"]);
        assert_eq!(found[0].line, 3);
        assert!(
            commands(
                &source.replace("cmd.CommandType", "other.CommandType"),
                true
            )
            .is_empty()
        );
        assert!(
            commands(
                &format!("{source}\ncmd.CommandText = \"audit.Other\""),
                true
            )
            .is_empty()
        );
        assert!(
            commands(
                &source.replace("\"dbo.CreateOrder\")", "\"dbo.CreateOrder\" & suffix)"),
                true
            )
            .is_empty()
        );
        assert!(
            commands(
                &source.replace(
                    "SqlCommand(\"dbo.CreateOrder\")",
                    "SqlCommand()\ncmd.CommandText = \"dbo.CreateOrder\" & suffix"
                ),
                true
            )
            .is_empty()
        );
    }
    #[test]
    fn procedure_catalog_keeps_schemas_separate_and_excludes_sql_examples() {
        let sql = "-- CREATE PROCEDURE fake AS SELECT * FROM Bad\nCREATE PROCEDURE dbo.Save AS\nINSERT INTO dbo.Orders (Name) VALUES ('SELECT * FROM Fake');\nGO\nCREATE PROCEDURE audit.Save AS\nINSERT INTO audit.Log (Name) VALUES ('test');\n";
        let catalog = Catalog::new(&[Source {
            path: "db/schema.sql",
            content: sql,
        }]);
        assert_eq!(catalog.definitions.len(), 2);
        assert_ne!(catalog.definitions[0].key, catalog.definitions[1].key);
        assert_eq!(catalog.definitions[0].line, 2);
        assert!(
            catalog.definitions[0]
                .writes
                .iter()
                .any(|t| t.eq_ignore_ascii_case("Orders"))
        );
        assert!(
            !catalog.definitions[0]
                .reads
                .iter()
                .any(|t| t.eq_ignore_ascii_case("Fake"))
        );
    }
}
