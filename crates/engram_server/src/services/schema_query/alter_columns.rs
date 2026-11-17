//! Column declarations in source migrations do not change the CREATE baseline.
use super::*;

pub(super) struct Declaration {
    pub key: Name,
    pub column_name: String,
    operation: &'static str,
    column: Option<Column>,
    declaration: String,
    citation: Citation,
}

impl Declaration {
    pub fn row(&self, binding: &str) -> Value {
        json!({"table":self.key.display(),"column_name":self.column_name,
            "operation":self.operation,"column":self.column,"declaration":self.declaration,
            "citation":self.citation,"binding":binding,
            "declaration_origin":"alter_table_column","application_status":"not_established",
            "semantics":"Source-declared column operation only. Conditions, execution, migration order, later operations and deployed schema are unknown. This does not modify the CREATE TABLE baseline; separate rows do not imply execution order."})
    }
}

pub(super) fn parse(
    tokens: &[TokenWithSpan],
    start: usize,
    after_table: usize,
    table: &Name,
    citation: &impl Fn(usize, usize) -> Citation,
) -> Vec<Declaration> {
    let (operation, origin) = if kw(tokens, after_table, "ADD") {
        let origin = after_table + 1 + usize::from(kw(tokens, after_table + 1, "COLUMN"));
        ("add_column", origin)
    } else if kw(tokens, after_table, "ALTER") && kw(tokens, after_table + 1, "COLUMN") {
        ("alter_column", after_table + 2)
    } else if kw(tokens, after_table, "DROP") && kw(tokens, after_table + 1, "COLUMN") {
        ("drop_column", after_table + 2)
    } else {
        return Vec::new();
    };
    let mut parser =
        Parser::new(&MsSqlDialect {}).with_tokens_with_locations(tokens[origin..].to_vec());
    let mut declarations = Vec::new();
    loop {
        let position = origin + parser.index();
        // ADD constraints have their own evidence path. They are not columns,
        // even if a keyword could be parsed as an identifier/custom type.
        if [
            "CONSTRAINT",
            "PRIMARY",
            "FOREIGN",
            "CHECK",
            "UNIQUE",
            "INDEX",
            "PERIOD",
        ]
        .iter()
        .any(|word| kw(tokens, position, word))
        {
            break;
        }
        let (column_name, column) = if operation == "drop_column" {
            let Ok(name) = parser.parse_identifier() else {
                break;
            };
            (name.value, None)
        } else {
            // Reuse SQLServer column-definition parsing for the endpoint and
            // the existing CREATE projection for type/null/default/identity.
            if parser.parse_column_def().is_err() {
                break;
            }
            let end = origin + parser.index();
            let Some(body) = tokens.get(position..end) else {
                break;
            };
            let Ok(column) = super::column(body) else {
                break;
            };
            (column.name.clone(), Some(column))
        };
        let end = origin + parser.index();
        if end <= position || end > tokens.len() {
            break;
        }
        declarations.push(Declaration {
            key: table.clone(),
            column_name,
            operation,
            column,
            declaration: text(&tokens[start..end]),
            citation: citation(start, end),
        });
        if operation == "alter_column" || !parser.consume_token(&Token::Comma) {
            break;
        }
    }
    declarations
}
