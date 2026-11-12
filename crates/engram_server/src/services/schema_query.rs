//! Read-only SQL declaration queries. This is an inventory, not a migration interpreter.
use crate::models::{QuerySchemaRequest, SchemaQueryType};
use serde::Serialize;
use serde_json::{Value, json};
use sqlparser::{
    ast::ColumnOption,
    dialect::MsSqlDialect,
    parser::Parser,
    tokenizer::{Token, TokenWithSpan, Tokenizer},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Name(pub(super) Vec<String>);
impl Name {
    pub(super) fn matches(&self, other: &Self) -> bool {
        self.0.len() == other.0.len()
            && self
                .0
                .iter()
                .zip(&other.0)
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
    }
    pub(super) fn lookup(&self, query: &Self) -> bool {
        self.matches(query)
            || (query.0.len() == 1
                && self
                    .0
                    .last()
                    .is_some_and(|s| s.eq_ignore_ascii_case(&query.0[0])))
    }
    pub(super) fn display(&self) -> String {
        self.0
            .iter()
            .map(|s| format!("[{}]", s.replace(']', "]]")))
            .collect::<Vec<_>>()
            .join(".")
    }
}

pub(super) fn tokens(sql: &str) -> anyhow::Result<Vec<TokenWithSpan>> {
    Ok(Tokenizer::new(&MsSqlDialect {}, sql)
        .tokenize_with_location()?
        .into_iter()
        .filter(|t| !matches!(t.token, Token::Whitespace(_)))
        .collect())
}
pub(super) fn kw(t: &[TokenWithSpan], i: usize, text: &str) -> bool {
    t.get(i).is_some_and(|t| matches!(&t.token, Token::Word(w) if w.quote_style.is_none() && w.value.eq_ignore_ascii_case(text)))
}
pub(super) fn name_at(t: &[TokenWithSpan], mut i: usize) -> Option<(Name, usize)> {
    let mut parts = vec![];
    loop {
        let Token::Word(w) = &t.get(i)?.token else {
            return None;
        };
        parts.push(w.value.clone());
        i += 1;
        if !t.get(i).is_some_and(|t| t.token == Token::Period) {
            break;
        }
        i += 1;
    }
    (parts.len() <= 4).then_some((Name(parts), i))
}
pub(super) fn input_name(s: &str) -> anyhow::Result<Name> {
    anyhow::ensure!(
        !s.trim().is_empty() && s.len() <= 512 && !s.chars().any(char::is_control),
        "SQL name must contain 1..=512 bytes without control characters"
    );
    let t = tokens(s)?;
    let (name, end) = name_at(&t, 0)
        .ok_or_else(|| anyhow::anyhow!("Expected a SQL identifier, optionally schema-qualified"))?;
    anyhow::ensure!(end == t.len(), "Expected only a SQL identifier");
    Ok(name)
}
pub(super) fn text(t: &[TokenWithSpan]) -> String {
    t.iter()
        .map(|t| t.token.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}
pub(super) fn close(t: &[TokenWithSpan], start: usize) -> Option<usize> {
    if t.get(start)?.token != Token::LParen {
        return None;
    }
    let mut depth = 0usize;
    for (i, token) in t.iter().enumerate().skip(start) {
        if token.token == Token::LParen {
            depth += 1;
        }
        if token.token == Token::RParen {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}
pub(super) fn segments(t: &[TokenWithSpan]) -> Vec<&[TokenWithSpan]> {
    let (mut result, mut start, mut depth) = (vec![], 0, 0i32);
    for (i, token) in t.iter().enumerate() {
        match token.token {
            Token::LParen => depth += 1,
            Token::RParen => depth -= 1,
            Token::Comma if depth == 0 => {
                result.push(&t[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < t.len() {
        result.push(&t[start..]);
    }
    result
}
fn identifiers(t: &[TokenWithSpan], start: usize) -> Vec<String> {
    close(t, start)
        .map(|end| {
            segments(&t[start + 1..end])
                .iter()
                .filter_map(|s| name_at(s, 0).map(|(n, _)| n.0.join(".")))
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Clone, Serialize)]
pub struct Citation {
    pub path: String,
    pub start_line: u64,
    pub end_line: u64,
    pub source_hash: String,
}
#[derive(Clone, Serialize)]
pub(super) struct Column {
    pub(super) name: String,
    pub(super) data_type: Option<String>,
    pub(super) nullable: Option<bool>,
    pub(super) default_value: Option<String>,
    pub(super) computed_expression: Option<String>,
    pub(super) identity: Option<String>,
    pub(super) primary_key: bool,
    pub(super) declaration: String,
}
#[derive(Clone, Serialize)]
struct ForeignKey {
    columns: Vec<String>,
    references_table: String,
    referenced_columns: Vec<String>,
    on_delete: Option<String>,
    on_update: Option<String>,
    declaration: String,
    #[serde(skip)]
    target: Name,
}
#[derive(Clone, Serialize)]
pub(super) struct Table {
    pub(super) name: String,
    pub(super) columns: Vec<Column>,
    pub(super) primary_key: Vec<String>,
    foreign_keys: Vec<ForeignKey>,
    pub(super) constraints: Vec<String>,
    pub(super) constraint_indexes: Vec<Value>,
    pub(super) citation: Citation,
    pub(super) gaps: Vec<String>,
    #[serde(skip)]
    pub(super) key: Name,
}
#[derive(Clone, Serialize)]
pub(super) struct Object {
    pub(super) kind: &'static str,
    pub(super) name: String,
    pub(super) target_table: Option<String>,
    pub(super) references: Vec<String>,
    pub(super) declaration: String,
    pub(super) citation: Citation,
    #[serde(skip)]
    pub(super) key: Name,
    #[serde(skip)]
    pub(super) target: Option<Name>,
    #[serde(skip)]
    pub(super) refs: Vec<Name>,
}

#[derive(Default)]
pub struct Catalog {
    pub(super) tables: Vec<Table>,
    pub(super) objects: Vec<Object>,
    pub gaps: Vec<String>,
}

fn action(t: &[TokenWithSpan], verb: &str) -> Option<String> {
    let i = (0..t.len()).find(|&i| kw(t, i, "ON") && kw(t, i + 1, verb))? + 2;
    let end = if kw(t, i, "SET") || kw(t, i, "NO") {
        i + 2
    } else {
        i + 1
    };
    Some(text(t.get(i..end)?))
}
fn foreign_key(t: &[TokenWithSpan], inline: Option<&str>) -> Option<ForeignKey> {
    let r = (0..t.len()).find(|&i| kw(t, i, "REFERENCES"))?;
    let (target, after) = name_at(t, r + 1)?;
    let columns = if let Some(column) = inline {
        vec![column.into()]
    } else {
        let f = (0..r).find(|&i| kw(t, i, "FOREIGN") && kw(t, i + 1, "KEY"))?;
        identifiers(t, f + 2)
    };
    Some(ForeignKey {
        columns,
        references_table: target.display(),
        referenced_columns: identifiers(t, after),
        on_delete: action(t, "DELETE"),
        on_update: action(t, "UPDATE"),
        declaration: text(t),
        target,
    })
}

fn column(t: &[TokenWithSpan]) -> anyhow::Result<Column> {
    let (name, after) = name_at(t, 0).ok_or_else(|| anyhow::anyhow!("Missing column name"))?;
    anyhow::ensure!(after == 1, "Column name must be unqualified");
    if kw(t, 1, "AS") {
        anyhow::ensure!(t.len() > 2, "Computed expression missing");
        return Ok(Column {
            name: name.0[0].clone(),
            data_type: None,
            nullable: None,
            default_value: None,
            computed_expression: Some(text(&t[2..])),
            identity: None,
            primary_key: false,
            declaration: text(t),
        });
    }
    // Reuse the installed SQL parser for types, expressions, named defaults and identity.
    let sql = text(t);
    let mut parser = Parser::new(&MsSqlDialect {}).try_with_sql(&sql)?;
    let parsed = parser.parse_column_def()?;
    anyhow::ensure!(
        parser.peek_token().token == Token::EOF,
        "Unsupported trailing column syntax"
    );
    let mut result = Column {
        name: parsed.name.value,
        data_type: Some(parsed.data_type.to_string()),
        nullable: None,
        default_value: None,
        computed_expression: None,
        identity: None,
        primary_key: false,
        declaration: sql,
    };
    for option in parsed.options {
        match option.option {
            ColumnOption::Null => result.nullable = Some(true),
            ColumnOption::NotNull => result.nullable = Some(false),
            ColumnOption::Default(e) => result.default_value = Some(e.to_string()),
            ColumnOption::Unique {
                is_primary: true, ..
            } => {
                result.primary_key = true;
                result.nullable = Some(false);
            }
            ColumnOption::Identity(i) => result.identity = Some(i.to_string()),
            _ => {}
        }
    }
    Ok(result)
}

fn references(t: &[TokenWithSpan]) -> Vec<Name> {
    let mut names = vec![];
    for i in 0..t.len() {
        if ["FROM", "JOIN", "UPDATE", "INTO", "REFERENCES"]
            .iter()
            .any(|s| kw(t, i, s))
        {
            if let Some((name, _)) = name_at(t, i + 1) {
                if !names.iter().any(|n: &Name| n.matches(&name)) {
                    names.push(name);
                }
            }
        }
    }
    names
}

impl Catalog {
    pub fn code_reference_name(&self, request: &QuerySchemaRequest) -> Option<String> {
        let SchemaQueryType::Table { name } = &request.query_type else {
            return None;
        };
        let key = input_name(name).ok()?;
        let tables: Vec<_> = self.tables.iter().filter(|t| t.key.lookup(&key)).collect();
        if tables.len() != 1 {
            return None;
        }
        tables[0].key.0.last().cloned()
    }

    pub fn add_source(&mut self, path: &str, source: &str) -> anyhow::Result<()> {
        let t = match tokens(source) {
            Ok(t) => t,
            Err(e) => {
                self.gaps
                    .push(format!("{path}: SQL tokenization failed: {e}"));
                return Ok(());
            }
        };
        let hash = blake3::hash(source.as_bytes()).to_hex().to_string();
        let citation = |start: usize, end: usize| Citation {
            path: path.into(),
            start_line: t[start].span.start.line,
            end_line: t[end.saturating_sub(1)].span.end.line,
            source_hash: hash.clone(),
        };
        let mut i = 0;
        while i < t.len() {
            anyhow::ensure!(
                self.tables.len() + self.objects.len() < 20_000,
                "Schema declaration inventory exceeds 20000 objects"
            );
            if !kw(&t, i, "CREATE") && !kw(&t, i, "ALTER") {
                i += 1;
                continue;
            }
            let start = i;
            let alter = kw(&t, i, "ALTER");
            i += 1;
            if kw(&t, i, "OR") && kw(&t, i + 1, "ALTER") {
                i += 2;
            }
            if alter && kw(&t, i, "TABLE") {
                self.gaps.push(format!("{path}:{}: ALTER TABLE is retained as a coverage gap; source declarations are not an executed migration state",t[start].span.start.line));
                i += 1;
                continue;
            }
            if kw(&t, i, "TABLE") {
                let Some((key, open)) = name_at(&t, i + 1) else {
                    i += 1;
                    continue;
                };
                let Some(end) = close(&t, open) else {
                    self.gaps.push(format!(
                        "{path}:{}: Unsupported CREATE TABLE body",
                        t[start].span.start.line
                    ));
                    i += 1;
                    continue;
                };
                let mut table = Table {
                    name: key.display(),
                    key,
                    columns: vec![],
                    primary_key: vec![],
                    foreign_keys: vec![],
                    constraints: vec![],
                    constraint_indexes: vec![],
                    citation: citation(start, end + 1),
                    gaps: vec![],
                };
                for s in segments(&t[open + 1..end]) {
                    let is_constraint = [
                        "CONSTRAINT",
                        "PRIMARY",
                        "FOREIGN",
                        "CHECK",
                        "UNIQUE",
                        "INDEX",
                    ]
                    .iter()
                    .any(|k| kw(s, 0, k));
                    if is_constraint {
                        table.constraints.push(text(s));
                        if (0..s.len())
                            .any(|j| kw(s, j, "PRIMARY") || kw(s, j, "UNIQUE") || kw(s, j, "INDEX"))
                        {
                            table.constraint_indexes.push(json!({"kind":"table_index_or_key_constraint","declaration":text(s),"citation":table.citation}));
                        }
                        if let Some(pk) =
                            (0..s.len()).find(|&j| kw(s, j, "PRIMARY") && kw(s, j + 1, "KEY"))
                        {
                            let open = (pk + 2..s.len()).find(|&j| s[j].token == Token::LParen);
                            if let Some(open) = open {
                                table.primary_key.extend(identifiers(s, open));
                            }
                        }
                        if let Some(fk) = foreign_key(s, None) {
                            table.foreign_keys.push(fk);
                        }
                    } else {
                        match column(s) {
                            Ok(col) => {
                                if col.primary_key || (0..s.len()).any(|j| kw(s, j, "UNIQUE")) {
                                    table.constraint_indexes.push(json!({"kind":"column_key_constraint","declaration":text(s),"citation":table.citation}));
                                }
                                if col.primary_key {
                                    table.primary_key.push(col.name.clone());
                                }
                                if let Some(fk) = foreign_key(s, Some(&col.name)) {
                                    table.foreign_keys.push(fk);
                                }
                                table.columns.push(col);
                            }
                            Err(e) => table
                                .gaps
                                .push(format!("Unparsed column declaration {}: {e}", text(s))),
                        }
                    }
                }
                for col in &mut table.columns {
                    if table
                        .primary_key
                        .iter()
                        .any(|n| n.eq_ignore_ascii_case(&col.name))
                    {
                        col.primary_key = true;
                        col.nullable = Some(false);
                    }
                }
                self.tables.push(table);
                i = end + 1;
                continue;
            }
            // One object body ends at an unquoted GO on its own line. Skip its
            // body afterwards so CREATE TABLE inside a procedure is not a schema declaration.
            let kind = if kw(&t, i, "VIEW") {
                Some("view")
            } else if kw(&t, i, "PROC") || kw(&t, i, "PROCEDURE") {
                Some("stored_proc")
            } else if kw(&t, i, "TRIGGER") {
                Some("trigger")
            } else {
                None
            };
            let mut index = i;
            while ["UNIQUE", "CLUSTERED", "NONCLUSTERED"]
                .iter()
                .any(|k| kw(&t, index, k))
            {
                index += 1;
            }
            let kind = kind.or_else(|| kw(&t, index, "INDEX").then_some("index"));
            let Some(kind) = kind else {
                continue;
            };
            let named = if kind == "index" { index + 1 } else { i + 1 };
            let Some((key, after)) = name_at(&t, named) else {
                i += 1;
                continue;
            };
            let end = (after..t.len())
                .find(|&j| {
                    (kw(&t, j, "GO")
                        && (j == 0 || t[j - 1].span.end.line < t[j].span.start.line)
                        && (j + 1 == t.len() || t[j + 1].span.start.line > t[j].span.end.line))
                        || (kind == "index" && t[j].token == Token::SemiColon)
                })
                .unwrap_or(t.len());
            let target = if kind == "index" || kind == "trigger" {
                (after..end)
                    .find(|&j| kw(&t, j, "ON"))
                    .and_then(|j| name_at(&t, j + 1).map(|(n, _)| n))
            } else {
                None
            };
            let refs = references(&t[after..end]);
            self.objects.push(Object {
                kind,
                name: key.display(),
                target_table: target.as_ref().map(Name::display),
                references: refs.iter().map(Name::display).collect(),
                declaration: text(&t[start..end]),
                citation: citation(start, end),
                key,
                target,
                refs,
            });
            i = end + 1;
        }
        Ok(())
    }

    pub fn query(&self, request: &QuerySchemaRequest) -> anyhow::Result<Value> {
        use SchemaQueryType::*;
        let q = &request.query_type;
        let table_name = match q {
            Table { name } => Some(name),
            Column { table, .. }
            | TablesThatReferenceForeignKey { table, .. }
            | TriggersOnTable { table }
            | ViewsSourcedFrom { table }
            | IndexesOnTable { table }
            | NullableColumns { table }
            | ComputedColumns { table } => Some(table),
            _ => None,
        };
        let matches = if let Some(name) = table_name {
            let key = input_name(name)?;
            self.tables
                .iter()
                .filter(|t| t.key.lookup(&key))
                .collect::<Vec<_>>()
        } else {
            vec![]
        };
        if table_name.is_some() && matches.len() != 1 {
            let offset = request.offset.unwrap_or(0);
            let limit = request.limit.unwrap_or(25);
            let rows: Vec<_> = matches
                .iter()
                .skip(offset)
                .take(limit)
                .map(|t| json!({"name":t.name,"citation":t.citation}))
                .collect();
            let next = (offset + rows.len() < matches.len()).then_some(offset + rows.len());
            return Ok(
                json!({"status":if matches.is_empty(){"not_found"}else{"ambiguous"},"summary":format!("{} matching table declarations; qualify the name or reconcile duplicate source definitions",matches.len()),
                "results":rows,"total_matches":matches.len(),"next_offset":next}),
            );
        }
        let selected = matches.first().copied();
        let relation = |reference: &Name, table: &self::Table| {
            if reference.matches(&table.key) {
                Some("exact_source_name")
            } else if reference.0.len() == 1 && table.key.lookup(reference) {
                Some("unqualified_candidate_default_schema_unknown")
            } else {
                None
            }
        };
        let related = |table: &self::Table, kind: &str| {
            let mut rows:Vec<_>=self.objects.iter().filter(|o|o.kind==kind).filter_map(|o| {
            let binding=if kind=="index" || kind=="trigger" {o.target.as_ref().and_then(|n|relation(n,table))}
                else {o.refs.iter().find_map(|n|relation(n,table))};
            binding.map(|binding| {
                let object=if matches!(q, Table{..}) && kind!="index" {
                    let query_type=match kind {
                        "stored_proc"=>json!({"kind":"stored_proc","sp_name":o.name}),
                        "view"=>json!({"kind":"views_sourced_from","table":table.name}),
                        _=>json!({"kind":"triggers_on_table","table":table.name}),
                    };
                    json!({"kind":o.kind,"name":o.name,"target_table":o.target_table,"references":o.references,"citation":o.citation,
                        "declaration_included":false,"recovery":{"tool":"query_schema","arguments":{"project_id":request.project_id,"query_type":query_type}}})
                } else {json!(o)};
                json!({"object":object,"binding":binding,"reference_semantics":"Lexical source reference; CTEs, aliases, dynamic SQL and execution are not resolved"})
            })
            }).collect();
            if kind == "index" {
                rows.extend(table.constraint_indexes.iter().cloned());
            }
            rows
        };
        let rows:Vec<Value>=match q {
            AllTables => self.tables.iter().map(|t|json!({"name":t.name,"citation":t.citation,"columns":t.columns.len(),"gaps":t.gaps})).collect(),
            TablesWithColumn{column_name_pattern} => { let pattern=column_name_pattern.to_lowercase(); self.tables.iter().filter_map(|t| {
                let cols:Vec<_>=t.columns.iter().filter(|c|c.name.to_lowercase().contains(&pattern)).collect();
                (!cols.is_empty()).then(||json!({"table":t.name,"columns":cols,"citation":t.citation,"gaps":t.gaps}))
            }).collect() },
            StoredProc{sp_name} => {let key=input_name(sp_name)?;self.objects.iter().filter(|o|o.kind=="stored_proc" && o.key.lookup(&key)).map(|o|json!(o)).collect()},
            _ => {
                let t=selected.expect("table queries resolved above");
                match q {
                    Table{..} => vec![json!({"table":t,"indexes":related(t,"index"),"triggers":related(t,"trigger"),"views":related(t,"view"),"stored_procedures":related(t,"stored_proc")})],
                    Column{column,..} => {let col=input_name(column)?;t.columns.iter().filter(|c|c.name.eq_ignore_ascii_case(&col.0[0])).map(|c|json!({"table":t.name,"column":c,"citation":t.citation})).collect()},
                    NullableColumns{..} => t.columns.iter().filter(|c|c.nullable==Some(true)).map(|c|json!({"table":t.name,"column":c,"citation":t.citation})).collect(),
                    ComputedColumns{..} => t.columns.iter().filter(|c|c.computed_expression.is_some()).map(|c|json!({"table":t.name,"column":c,"citation":t.citation})).collect(),
                    TriggersOnTable{..} => related(t,"trigger"), ViewsSourcedFrom{..} => related(t,"view"), IndexesOnTable{..} => related(t,"index"),
                    TablesThatReferenceForeignKey{column,..} => {let col=input_name(column)?;self.tables.iter().flat_map(|other|other.foreign_keys.iter().filter_map(|fk| {
                        let binding=relation(&fk.target,t)?;
                        fk.referenced_columns.iter().any(|c|c.eq_ignore_ascii_case(&col.0[0])).then(||json!({"table":other.name,"foreign_key":fk,"binding":binding,"citation":other.citation}))
                    })).collect()},
                    _=>unreachable!(),
                }
            }
        };
        let total = rows.len();
        let offset = request.offset.unwrap_or(0);
        let limit = request.limit.unwrap_or(25);
        let results: Vec<_> = rows.into_iter().skip(offset).take(limit).collect();
        let next = (offset.saturating_add(results.len()) < total).then_some(offset + results.len());
        let status = if matches!(q, StoredProc { .. }) && total > 1 {
            "ambiguous"
        } else if total == 0 {
            "not_found"
        } else {
            "source_declarations"
        };
        Ok(
            json!({"status":status,"summary":format!("{} source-declared result(s); returning {} from offset {}",total,results.len(),offset),
            "results":results,"total_matches":total,"next_offset":next,"table_gaps":selected.map(|t|&t.gaps),
            "unknown_nullability_columns":selected.map(|t|t.columns.iter().filter(|c|c.nullable.is_none()).map(|c|&c.name).collect::<Vec<_>>())}),
        )
    }
}

pub fn validate(request: &QuerySchemaRequest) -> anyhow::Result<()> {
    use SchemaQueryType::*;
    anyhow::ensure!(
        (1..=100).contains(&request.limit.unwrap_or(25)),
        "limit must be 1..=100"
    );
    anyhow::ensure!(
        request.offset.unwrap_or(0) <= 20_000,
        "offset must be <=20000"
    );
    anyhow::ensure!(
        (1..=65536).contains(&request.max_tokens.unwrap_or(16384)),
        "max_tokens must be 1..=65536"
    );
    let names: Vec<&str> = match &request.query_type {
        Table { name } => vec![name],
        Column { table, column } | TablesThatReferenceForeignKey { table, column } => {
            anyhow::ensure!(
                input_name(column)?.0.len() == 1,
                "column must be unqualified"
            );
            vec![table]
        }
        TriggersOnTable { table }
        | ViewsSourcedFrom { table }
        | IndexesOnTable { table }
        | NullableColumns { table }
        | ComputedColumns { table } => vec![table],
        StoredProc { sp_name } => vec![sp_name],
        TablesWithColumn {
            column_name_pattern,
        } => {
            anyhow::ensure!(
                !column_name_pattern.is_empty()
                    && column_name_pattern.len() <= 128
                    && !column_name_pattern.chars().any(char::is_control),
                "column_name_pattern must be a nonempty literal substring of at most 128 bytes"
            );
            vec![]
        }
        AllTables => vec![],
    };
    for name in names {
        input_name(name)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
