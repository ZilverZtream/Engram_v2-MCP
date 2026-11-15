//! Source operations remain separate from CREATE TABLE's declared baseline.
use super::*;
use sqlparser::ast::TableConstraint;

pub(super) struct Declaration {
    pub key: Name,
    pub foreign_key: ForeignKey,
    citation: Citation,
}
impl Declaration {
    pub fn row(&self, binding: &str) -> Value {
        json!({"table":self.key.display(),"foreign_key":self.foreign_key,
            "citation":self.citation,"binding":binding,
            "declaration_origin":"alter_table_add_foreign_key",
            "application_status":"not_established",
            "semantics":"Source-declared operation only. Conditional execution, migration order, later changes, constraint trust and deployed database state are not established. This does not modify the CREATE TABLE baseline."})
    }
}

pub(super) fn parse(
    tokens: &[TokenWithSpan],
    start: usize,
    mut after_table: usize,
    table: &Name,
    citation: &impl Fn(usize, usize) -> Citation,
) -> Vec<Declaration> {
    if kw(tokens, after_table, "WITH")
        && (kw(tokens, after_table + 1, "CHECK") || kw(tokens, after_table + 1, "NOCHECK"))
    {
        after_table += 2;
    }
    if !kw(tokens, after_table, "ADD") {
        return Vec::new();
    }
    let origin = after_table + 1;
    let mut parser =
        Parser::new(&MsSqlDialect {}).with_tokens_with_locations(tokens[origin..].to_vec());
    let mut declarations = Vec::new();
    loop {
        // The existing SQL parser identifies the exact constraint endpoint,
        // leaving surrounding IF/ELSE/END and subsequent statements untouched.
        // The shared recognizer projects the same FK names/actions as CREATE.
        let position = origin + parser.get_current_index();
        let Ok(Some(TableConstraint::ForeignKey { .. })) = parser.parse_optional_table_constraint()
        else {
            break;
        };
        let end = origin + parser.get_current_index();
        if end <= position || end > tokens.len() {
            break;
        }
        let Some(foreign_key) = foreign_key(&tokens[position..end], None) else {
            break;
        };
        if foreign_key.columns.is_empty()
            || foreign_key.columns.len() != foreign_key.referenced_columns.len()
        {
            break;
        }
        declarations.push(Declaration {
            key: table.clone(),
            foreign_key,
            citation: citation(start, end),
        });
        if !parser.consume_token(&Token::Comma) {
            break;
        }
    }
    declarations
}
