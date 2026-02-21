/// DDL schema extractor.
///
/// Parses `.sql` files containing `CREATE TABLE` statements and extracts:
///   - `db_table` symbols for each table
///   - `db_column` symbols for each column within a table
///   - `has_column` edges (Table → Column)
///   - `foreign_key` edges (Column → Column)
///
/// Uses regex-based extraction (no tree-sitter-sql dependency), consistent with
/// the existing SQL handling approach in the codebase.
use crate::parsing::{ExtractedEdge, ExtractedSymbol};
use engram_core::RelPath;
use regex::Regex;
use std::collections::HashMap;
use std::sync::OnceLock;

// ── Static Regex Definitions (Compiled Once) ────────────────────────────────

static CREATE_TABLE_RE: OnceLock<Regex> = OnceLock::new();
static COLUMN_DEF_RE: OnceLock<Regex> = OnceLock::new();
static FK_CONSTRAINT_RE: OnceLock<Regex> = OnceLock::new();

fn get_compiled_regex<'a>(
    lock: &'a OnceLock<Regex>,
    pattern: &str,
    label: &str,
) -> Option<&'a Regex> {
    if let Some(re) = lock.get() {
        return Some(re);
    }
    match Regex::new(pattern) {
        Ok(re) => Some(lock.get_or_init(|| re)),
        Err(err) => {
            tracing::error!("failed to compile {label} regex: {err}");
            None
        }
    }
}

/// Strip surrounding brackets `[Name]` → `Name`.
fn strip_brackets(s: &str) -> &str {
    let s = s.trim();
    if s.starts_with('[') && s.ends_with(']') {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Extract DDL symbols and edges from a SQL schema file.
///
/// Returns `(symbols, edges)` where symbols include db_table and db_column entries
/// and edges include has_column and foreign_key relationships.
pub fn extract_ddl(rel_path: &RelPath, source: &str) -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>) {
    let mut symbols: Vec<ExtractedSymbol> = Vec::new();
    let mut edges: Vec<ExtractedEdge> = Vec::new();

    // Build line offsets for line number computation.
    let line_offsets: Vec<usize> = {
        let mut offsets = vec![0usize];
        for (i, c) in source.char_indices() {
            if c == '\n' {
                offsets.push(i + 1);
            }
        }
        offsets
    };

    let char_to_line = |char_pos: usize| -> u32 {
        match line_offsets.binary_search(&char_pos) {
            Ok(line) => line as u32,
            Err(line) => line.saturating_sub(1) as u32,
        }
    };

    // Regex: CREATE TABLE [schema.]TableName (
    let Some(create_table_re) = get_compiled_regex(
        &CREATE_TABLE_RE,
        r"(?i)CREATE\s+TABLE\s+(?:\[?\w+\]?\.)?(\[?\w+\]?)\s*\(",
        "ddl_create_table",
    ) else {
        return (symbols, edges);
    };

    // Regex: column definition line
    // Matches: [ColumnName] datatype(params) [NOT] NULL | IDENTITY | etc.
    let Some(column_def_re) = get_compiled_regex(
        &COLUMN_DEF_RE,
        r"(?i)^\s*(\[?\w+\]?)\s+(\w+(?:\s*\([^)]*\))?)\s*(NOT\s+NULL|NULL|IDENTITY)?",
        "ddl_column_def",
    ) else {
        return (symbols, edges);
    };

    // Regex: FOREIGN KEY (col) REFERENCES [schema.]Table(col2)
    let Some(fk_re) = get_compiled_regex(
        &FK_CONSTRAINT_RE,
        r"(?i)FOREIGN\s+KEY\s*\(\s*(\[?\w+\]?)\s*\)\s*REFERENCES\s+(?:\[?\w+\]?\.)?(\[?\w+\]?)\s*\(\s*(\[?\w+\]?)\s*\)",
        "ddl_fk_constraint",
    ) else {
        return (symbols, edges);
    };

    // Keywords that should NOT be treated as column names.
    let reserved_keywords: &[&str] = &[
        "constraint",
        "primary",
        "unique",
        "index",
        "check",
        "foreign",
        "key",
        "create",
        "alter",
        "drop",
        "go",
        "set",
        "exec",
        "if",
        "begin",
        "end",
        "declare",
        "clustered",
        "nonclustered",
        "asc",
        "desc",
        "on",
        "with",
        "references",
    ];

    for cap in create_table_re.captures_iter(source) {
        let table_name_raw = strip_brackets(cap.get(1).map_or("", |m| m.as_str()));
        let table_name = table_name_raw.to_string();
        let match_start = cap.get(0).map_or(0, |m| m.start());
        let start_line = char_to_line(match_start);

        // Find the matching closing paren for this CREATE TABLE block.
        let paren_open = match source[match_start..].find('(') {
            Some(pos) => match_start + pos,
            None => continue,
        };
        let body_end = match find_matching_paren(source, paren_open) {
            Some(pos) => pos,
            None => continue,
        };

        let end_line = char_to_line(body_end);
        let full_ddl = &source[match_start..=body_end];
        let body = &source[paren_open + 1..body_end];

        // Create the table symbol.
        let mut table_meta = HashMap::new();
        table_meta.insert("ddl".to_string(), full_ddl.to_string());
        table_meta.insert("file".to_string(), rel_path.as_str().to_string());

        symbols.push(ExtractedSymbol {
            name: table_name.clone(),
            kind: "db_table",
            start_line,
            end_line,
            metadata: Some(table_meta),
        });

        // Parse column definitions from the body.
        for line in body.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with("--") {
                continue;
            }

            // Check for FK constraints.
            if let Some(fk_cap) = fk_re.captures(trimmed) {
                let local_col = strip_brackets(fk_cap.get(1).map_or("", |m| m.as_str()));
                let ref_table = strip_brackets(fk_cap.get(2).map_or("", |m| m.as_str()));
                let ref_col = strip_brackets(fk_cap.get(3).map_or("", |m| m.as_str()));

                edges.push(ExtractedEdge {
                    source_name: format!(
                        "column:{}:{}",
                        table_name.to_lowercase(),
                        local_col.to_lowercase()
                    ),
                    source_kind: "db_column",
                    source_start_line: start_line,
                    source_language: "sql",
                    target_name: format!(
                        "column:{}:{}",
                        ref_table.to_lowercase(),
                        ref_col.to_lowercase()
                    ),
                    target_kind: Some("db_column"),
                    target_start_line: None,
                    kind: "foreign_key",
                    metadata: Some({
                        let mut m = HashMap::new();
                        m.insert("local_table".to_string(), table_name.clone());
                        m.insert("local_column".to_string(), local_col.to_string());
                        m.insert("ref_table".to_string(), ref_table.to_string());
                        m.insert("ref_column".to_string(), ref_col.to_string());
                        m
                    }),
                });
                continue;
            }

            // Try to parse as a column definition.
            if let Some(col_cap) = column_def_re.captures(trimmed) {
                let col_name_raw = strip_brackets(col_cap.get(1).map_or("", |m| m.as_str()));

                // Skip SQL keywords that appear at the start of constraint lines.
                if reserved_keywords
                    .iter()
                    .any(|kw| col_name_raw.eq_ignore_ascii_case(kw))
                {
                    continue;
                }

                let data_type = col_cap.get(2).map_or("", |m| m.as_str()).trim().to_string();
                let nullable_str = col_cap.get(3).map_or("", |m| m.as_str()).trim();
                let is_nullable = !nullable_str.to_uppercase().contains("NOT NULL");

                let col_name = col_name_raw.to_string();

                // Compute the line number for this column definition.
                let col_line_offset = source[match_start..]
                    .find(trimmed)
                    .map(|off| match_start + off)
                    .unwrap_or(match_start);
                let col_line = char_to_line(col_line_offset);

                let mut col_meta = HashMap::new();
                col_meta.insert("table".to_string(), table_name.clone());
                col_meta.insert("data_type".to_string(), data_type);
                col_meta.insert("nullable".to_string(), is_nullable.to_string());

                symbols.push(ExtractedSymbol {
                    name: col_name.clone(),
                    kind: "db_column",
                    start_line: col_line,
                    end_line: col_line,
                    metadata: Some(col_meta),
                });

                // Emit has_column edge: table → column.
                edges.push(ExtractedEdge {
                    source_name: table_name.clone(),
                    source_kind: "db_table",
                    source_start_line: start_line,
                    source_language: "sql",
                    target_name: col_name.clone(),
                    target_kind: Some("db_column"),
                    target_start_line: Some(col_line),
                    kind: "has_column",
                    metadata: None,
                });
            }
        }
    }

    (symbols, edges)
}

/// Find the position of the matching closing parenthesis for an opening paren at `open_pos`.
fn find_matching_paren(source: &str, open_pos: usize) -> Option<usize> {
    let mut depth = 0;
    let mut in_string = false;
    let mut string_char = ' ';

    for (i, c) in source[open_pos..].char_indices() {
        if in_string {
            if c == string_char {
                in_string = false;
            }
            continue;
        }
        match c {
            '\'' | '"' => {
                in_string = true;
                string_char = c;
            }
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open_pos + i);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_create_table() {
        let ddl = r#"
CREATE TABLE [dbo].[Orders] (
    [OrderId] INT NOT NULL,
    [CustomerId] INT NULL,
    [OrderDate] DATETIME NOT NULL,
    CONSTRAINT PK_Orders PRIMARY KEY ([OrderId]),
    FOREIGN KEY ([CustomerId]) REFERENCES [Customers]([CustomerId])
)
"#;
        let rel = RelPath::new("schema.sql");
        let (syms, edges) = extract_ddl(&rel, ddl);

        // Should have 1 table + 3 columns
        let tables: Vec<_> = syms.iter().filter(|s| s.kind == "db_table").collect();
        let cols: Vec<_> = syms.iter().filter(|s| s.kind == "db_column").collect();
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].name, "Orders");
        assert_eq!(
            cols.len(),
            3,
            "Expected 3 columns: {:?}",
            cols.iter().map(|c| &c.name).collect::<Vec<_>>()
        );

        // Should have 3 has_column edges + 1 foreign_key edge
        let has_col: Vec<_> = edges.iter().filter(|e| e.kind == "has_column").collect();
        let fks: Vec<_> = edges.iter().filter(|e| e.kind == "foreign_key").collect();
        assert_eq!(has_col.len(), 3);
        assert_eq!(fks.len(), 1);
        assert_eq!(fks[0].target_name, "column:customers:customerid");
    }

    #[test]
    fn test_multiple_tables() {
        let ddl = r#"
CREATE TABLE Customers (
    CustomerId INT NOT NULL,
    Name VARCHAR(100) NOT NULL
)

CREATE TABLE Orders (
    OrderId INT NOT NULL,
    CustomerId INT NULL,
    FOREIGN KEY (CustomerId) REFERENCES Customers(CustomerId)
)
"#;
        let rel = RelPath::new("schema.sql");
        let (syms, edges) = extract_ddl(&rel, ddl);

        let tables: Vec<_> = syms.iter().filter(|s| s.kind == "db_table").collect();
        assert_eq!(tables.len(), 2);

        let fks: Vec<_> = edges.iter().filter(|e| e.kind == "foreign_key").collect();
        assert_eq!(fks.len(), 1);
    }

    #[test]
    fn test_ddl_metadata() {
        let ddl = "CREATE TABLE Users (\n    Id INT NOT NULL,\n    Email VARCHAR(255) NULL\n)";
        let rel = RelPath::new("tables.sql");
        let (syms, _edges) = extract_ddl(&rel, ddl);

        let table = syms.iter().find(|s| s.kind == "db_table").expect("table");
        let meta = table.metadata.as_ref().expect("metadata");
        assert!(meta.get("ddl").expect("ddl").contains("CREATE TABLE"));

        let email_col = syms.iter().find(|s| s.name == "Email").expect("email col");
        let col_meta = email_col.metadata.as_ref().expect("col meta");
        assert_eq!(col_meta.get("table").expect("table"), "Users");
        assert_eq!(col_meta.get("nullable").expect("nullable"), "true");
    }
}
