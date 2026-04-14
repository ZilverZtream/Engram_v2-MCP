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
            kind: "db_table".to_string(),
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
                    source_kind: "db_column".to_string(),
                    source_start_line: start_line,
                    source_language: "sql".to_string(),
                    target_name: format!(
                        "column:{}:{}",
                        ref_table.to_lowercase(),
                        ref_col.to_lowercase()
                    ),
                    target_kind: Some("db_column".to_string()),
                    target_start_line: None,
                    kind: "foreign_key".to_string(),
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
                    kind: "db_column".to_string(),
                    start_line: col_line,
                    end_line: col_line,
                    metadata: Some(col_meta),
                });

                // Emit has_column edge: table → column.
                edges.push(ExtractedEdge {
                    source_name: table_name.clone(),
                    source_kind: "db_table".to_string(),
                    source_start_line: start_line,
                    source_language: "sql".to_string(),
                    target_name: col_name.clone(),
                    target_kind: Some("db_column".to_string()),
                    target_start_line: Some(col_line),
                    kind: "has_column".to_string(),
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
    let slice = &source[open_pos..];
    let bytes = slice.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        let c = bytes[i] as char;

        if in_string {
            if c == string_char {
                in_string = false;
            }
            i += 1;
            continue;
        }

        // Skip `--` line comments to avoid misinterpreting single-quotes in comment text.
        if c == '-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
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
        i += 1;
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

    // ── New tests ──────────────────────────────────────────────────────────────

    #[test]
    fn create_table_extracts_table_name() {
        let ddl = "CREATE TABLE Products (\n    Id INT NOT NULL\n)";
        let rel = RelPath::new("schema.sql");
        let (syms, _) = extract_ddl(&rel, ddl);
        let tables: Vec<_> = syms.iter().filter(|s| s.kind == "db_table").collect();
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].name, "Products");
    }

    #[test]
    fn create_table_extracts_column_names() {
        let ddl = "CREATE TABLE Inventory (\n    ItemId INT NOT NULL,\n    ItemName VARCHAR(100) NOT NULL,\n    Quantity INT NULL\n)";
        let rel = RelPath::new("schema.sql");
        let (syms, _) = extract_ddl(&rel, ddl);
        let col_names: Vec<&str> = syms
            .iter()
            .filter(|s| s.kind == "db_column")
            .map(|s| s.name.as_str())
            .collect();
        assert!(col_names.contains(&"ItemId"), "col_names: {col_names:?}");
        assert!(col_names.contains(&"ItemName"), "col_names: {col_names:?}");
        assert!(col_names.contains(&"Quantity"), "col_names: {col_names:?}");
        assert_eq!(col_names.len(), 3);
    }

    #[test]
    fn create_table_extracts_column_types() {
        let ddl = "CREATE TABLE Products (\n    Price DECIMAL(10,2) NOT NULL,\n    Name NVARCHAR(200) NULL\n)";
        let rel = RelPath::new("schema.sql");
        let (syms, _) = extract_ddl(&rel, ddl);

        let price = syms.iter().find(|s| s.name == "Price").expect("Price col");
        let meta = price.metadata.as_ref().expect("meta");
        assert!(
            meta["data_type"].contains("DECIMAL"),
            "data_type was: {}",
            meta["data_type"]
        );

        let name_col = syms.iter().find(|s| s.name == "Name").expect("Name col");
        let name_meta = name_col.metadata.as_ref().expect("name meta");
        assert!(
            name_meta["data_type"].contains("NVARCHAR"),
            "data_type was: {}",
            name_meta["data_type"]
        );
    }

    #[test]
    fn create_table_with_primary_key() {
        // CONSTRAINT syntax — PK line is filtered by reserved keyword "CONSTRAINT"
        let ddl = "CREATE TABLE Orders (\n    OrderId INT NOT NULL,\n    CONSTRAINT PK_Orders PRIMARY KEY (OrderId)\n)";
        let rel = RelPath::new("schema.sql");
        let (syms, _) = extract_ddl(&rel, ddl);
        let cols: Vec<_> = syms.iter().filter(|s| s.kind == "db_column").collect();
        // "CONSTRAINT" keyword line is skipped, so only OrderId should be extracted
        assert_eq!(
            cols.len(),
            1,
            "cols: {:?}",
            cols.iter().map(|c| &c.name).collect::<Vec<_>>()
        );
        assert_eq!(cols[0].name, "OrderId");
    }

    #[test]
    fn create_table_with_foreign_key_extracted() {
        let ddl = "CREATE TABLE Orders (\n    OrderId INT NOT NULL,\n    CustomerId INT NULL,\n    FOREIGN KEY (CustomerId) REFERENCES Customers(CustomerId)\n)";
        let rel = RelPath::new("schema.sql");
        let (syms, edges) = extract_ddl(&rel, ddl);

        let fks: Vec<_> = edges.iter().filter(|e| e.kind == "foreign_key").collect();
        assert_eq!(fks.len(), 1);

        let fk = &fks[0];
        assert_eq!(fk.source_name, "column:orders:customerid");
        assert_eq!(fk.target_name, "column:customers:customerid");
        assert_eq!(fk.source_kind, "db_column");
        assert_eq!(fk.target_kind, Some("db_column"));

        // Metadata should carry the table/column names
        let meta = fk.metadata.as_ref().expect("fk metadata");
        assert_eq!(meta["local_table"], "Orders");
        assert_eq!(meta["local_column"], "CustomerId");
        assert_eq!(meta["ref_table"], "Customers");
        assert_eq!(meta["ref_column"], "CustomerId");

        // Still emits columns as symbols
        let cols: Vec<_> = syms.iter().filter(|s| s.kind == "db_column").collect();
        assert!(cols.iter().any(|c| c.name == "CustomerId"));
    }

    #[test]
    fn create_table_with_not_null_constraint() {
        let ddl =
            "CREATE TABLE Accounts (\n    Id INT NOT NULL,\n    Balance DECIMAL(18,2) NULL\n)";
        let rel = RelPath::new("schema.sql");
        let (syms, _) = extract_ddl(&rel, ddl);

        let id_col = syms.iter().find(|s| s.name == "Id").expect("Id col");
        let id_meta = id_col.metadata.as_ref().expect("meta");
        // NOT NULL → nullable = false
        assert_eq!(id_meta["nullable"], "false");

        let bal_col = syms
            .iter()
            .find(|s| s.name == "Balance")
            .expect("Balance col");
        let bal_meta = bal_col.metadata.as_ref().expect("meta");
        assert_eq!(bal_meta["nullable"], "true");
    }

    #[test]
    fn create_table_with_identity_column() {
        let ddl = "CREATE TABLE Logs (\n    LogId INT IDENTITY(1,1) NOT NULL,\n    Message NVARCHAR(500) NULL\n)";
        let rel = RelPath::new("schema.sql");
        let (syms, _) = extract_ddl(&rel, ddl);

        // LogId should be extracted as a column
        let log_col = syms.iter().find(|s| s.name == "LogId").expect("LogId col");
        assert_eq!(log_col.kind, "db_column");
    }

    #[test]
    fn alter_table_add_column_not_treated_as_table() {
        // ALTER TABLE is NOT CREATE TABLE — the extractor only handles CREATE TABLE
        // so this should not produce any table or column symbols
        let ddl = "ALTER TABLE Products ADD Description NVARCHAR(500) NULL;";
        let rel = RelPath::new("migration.sql");
        let (syms, _) = extract_ddl(&rel, ddl);
        // No CREATE TABLE → no symbols produced
        assert!(
            syms.iter().all(|s| s.kind != "db_table"),
            "ALTER TABLE should not produce db_table symbols"
        );
    }

    #[test]
    fn alter_table_add_foreign_key_not_treated_as_table() {
        let ddl = "ALTER TABLE Orders ADD CONSTRAINT FK_Cust FOREIGN KEY (CustId) REFERENCES Customers(Id);";
        let rel = RelPath::new("migration.sql");
        let (syms, edges) = extract_ddl(&rel, ddl);
        // No CREATE TABLE block → nothing extracted
        assert!(syms.is_empty());
        assert!(edges.is_empty());
    }

    #[test]
    fn create_index_on_table_no_extra_table_symbols() {
        // CREATE INDEX is not CREATE TABLE — should not produce extra db_table symbols
        let ddl = "CREATE TABLE Items (\n    ItemId INT NOT NULL\n)\n\nCREATE INDEX IX_ItemId ON Items (ItemId);";
        let rel = RelPath::new("schema.sql");
        let (syms, _) = extract_ddl(&rel, ddl);
        let tables: Vec<_> = syms.iter().filter(|s| s.kind == "db_table").collect();
        // Only the CREATE TABLE produces a table symbol
        assert_eq!(
            tables.len(),
            1,
            "Only one table expected, got: {:?}",
            tables.iter().map(|t| &t.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn multiple_tables_in_one_script() {
        let ddl = r#"
CREATE TABLE Alpha (
    AlphaId INT NOT NULL,
    AlphaName VARCHAR(50) NOT NULL
)

CREATE TABLE Beta (
    BetaId INT NOT NULL,
    AlphaId INT NULL,
    FOREIGN KEY (AlphaId) REFERENCES Alpha(AlphaId)
)

CREATE TABLE Gamma (
    GammaId INT NOT NULL
)
"#;
        let rel = RelPath::new("schema.sql");
        let (syms, edges) = extract_ddl(&rel, ddl);

        let tables: Vec<_> = syms.iter().filter(|s| s.kind == "db_table").collect();
        assert_eq!(tables.len(), 3);
        assert!(tables.iter().any(|t| t.name == "Alpha"));
        assert!(tables.iter().any(|t| t.name == "Beta"));
        assert!(tables.iter().any(|t| t.name == "Gamma"));

        let fks: Vec<_> = edges.iter().filter(|e| e.kind == "foreign_key").collect();
        assert_eq!(fks.len(), 1);
        assert_eq!(fks[0].source_name, "column:beta:alphaid");
        assert_eq!(fks[0].target_name, "column:alpha:alphaid");
    }

    #[test]
    fn table_with_schema_prefix_parsed() {
        let ddl = "CREATE TABLE dbo.Products (\n    ProductId INT NOT NULL\n)";
        let rel = RelPath::new("schema.sql");
        let (syms, _) = extract_ddl(&rel, ddl);

        let tables: Vec<_> = syms.iter().filter(|s| s.kind == "db_table").collect();
        assert_eq!(tables.len(), 1);
        // Schema prefix is stripped — only the table name remains
        assert_eq!(tables[0].name, "Products");
    }

    #[test]
    fn table_with_brackets_parsed() {
        let ddl = "CREATE TABLE [dbo].[OrderDetails] (\n    [DetailId] INT NOT NULL,\n    [Qty] INT NOT NULL\n)";
        let rel = RelPath::new("schema.sql");
        let (syms, _) = extract_ddl(&rel, ddl);

        let tables: Vec<_> = syms.iter().filter(|s| s.kind == "db_table").collect();
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].name, "OrderDetails");

        let cols: Vec<_> = syms.iter().filter(|s| s.kind == "db_column").collect();
        let col_names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
        assert!(col_names.contains(&"DetailId"), "col_names: {col_names:?}");
        assert!(col_names.contains(&"Qty"), "col_names: {col_names:?}");
    }

    #[test]
    fn empty_ddl_returns_empty() {
        let rel = RelPath::new("empty.sql");
        let (syms, edges) = extract_ddl(&rel, "");
        assert!(syms.is_empty(), "Expected no symbols for empty input");
        assert!(edges.is_empty(), "Expected no edges for empty input");
    }

    #[test]
    fn ddl_with_sql_comments_parsed() {
        let ddl = r#"
-- This creates the user table
CREATE TABLE Users (
    -- Primary key
    UserId INT NOT NULL,
    -- User's email address
    Email VARCHAR(255) NULL
)
"#;
        let rel = RelPath::new("schema.sql");
        let (syms, _) = extract_ddl(&rel, ddl);

        let tables: Vec<_> = syms.iter().filter(|s| s.kind == "db_table").collect();
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].name, "Users");

        let cols: Vec<_> = syms.iter().filter(|s| s.kind == "db_column").collect();
        let col_names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
        assert!(col_names.contains(&"UserId"), "col_names: {col_names:?}");
        assert!(col_names.contains(&"Email"), "col_names: {col_names:?}");
    }

    #[test]
    fn varchar_length_in_type_captured() {
        let ddl = "CREATE TABLE Contacts (\n    Phone VARCHAR(20) NOT NULL,\n    Notes NVARCHAR(MAX) NULL\n)";
        let rel = RelPath::new("schema.sql");
        let (syms, _) = extract_ddl(&rel, ddl);

        let phone = syms.iter().find(|s| s.name == "Phone").expect("Phone col");
        let meta = phone.metadata.as_ref().expect("meta");
        // The data_type should include the length qualifier
        // The length qualifier must be preserved — VARCHAR without (20) would lose precision info.
        let dt = meta["data_type"].to_uppercase();
        assert!(
            dt.contains("VARCHAR") && dt.contains("20"),
            "data_type must include both type name and length qualifier, got: {}",
            meta["data_type"]
        );
    }

    #[test]
    fn check_constraint_line_skipped() {
        // Lines starting with CONSTRAINT keyword should not be treated as columns
        let ddl = "CREATE TABLE Orders (\n    Total DECIMAL(10,2) NOT NULL,\n    CONSTRAINT CK_Total CHECK (Total >= 0)\n)";
        let rel = RelPath::new("schema.sql");
        let (syms, _) = extract_ddl(&rel, ddl);

        let cols: Vec<_> = syms.iter().filter(|s| s.kind == "db_column").collect();
        // Only "Total" should be a column; the CONSTRAINT line is a keyword and gets skipped
        let col_names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
        assert!(
            !col_names.contains(&"CONSTRAINT"),
            "CONSTRAINT should not be a column name"
        );
        assert!(
            !col_names.contains(&"CK_Total"),
            "CK_Total should not be a column name"
        );
        assert!(
            col_names.contains(&"Total"),
            "Total should be extracted: {col_names:?}"
        );
    }

    #[test]
    fn has_column_edges_point_to_correct_table() {
        let ddl =
            "CREATE TABLE Widgets (\n    WidgetId INT NOT NULL,\n    Color VARCHAR(50) NULL\n)";
        let rel = RelPath::new("schema.sql");
        let (syms, edges) = extract_ddl(&rel, ddl);

        let has_col_edges: Vec<_> = edges.iter().filter(|e| e.kind == "has_column").collect();
        assert_eq!(has_col_edges.len(), 2);

        for e in &has_col_edges {
            assert_eq!(e.source_name, "Widgets", "source_name should be table name");
            assert_eq!(e.source_kind, "db_table");
            assert_eq!(e.target_kind, Some("db_column"));
        }

        let target_names: Vec<&str> = has_col_edges
            .iter()
            .map(|e| e.target_name.as_str())
            .collect();
        assert!(
            target_names.contains(&"WidgetId"),
            "targets: {target_names:?}"
        );
        assert!(target_names.contains(&"Color"), "targets: {target_names:?}");

        // All symbols must include the table
        let table = syms.iter().find(|s| s.kind == "db_table").expect("table");
        assert_eq!(table.name, "Widgets");
    }

    #[test]
    fn fk_edge_uses_lowercase_names() {
        let ddl = "CREATE TABLE Orders (\n    OrderId INT NOT NULL,\n    CustId INT NULL,\n    FOREIGN KEY (CustId) REFERENCES Customers(CustomerId)\n)";
        let rel = RelPath::new("schema.sql");
        let (_, edges) = extract_ddl(&rel, ddl);

        let fks: Vec<_> = edges.iter().filter(|e| e.kind == "foreign_key").collect();
        assert_eq!(fks.len(), 1);
        // Both source and target names in the FK edge use lowercase
        assert_eq!(fks[0].source_name, "column:orders:custid");
        assert_eq!(fks[0].target_name, "column:customers:customerid");
    }

    #[test]
    fn table_ddl_stored_in_metadata() {
        let ddl = "CREATE TABLE Events (\n    EventId INT NOT NULL\n)";
        let rel = RelPath::new("schema.sql");
        let (syms, _) = extract_ddl(&rel, ddl);

        let table = syms.iter().find(|s| s.kind == "db_table").expect("table");
        let meta = table.metadata.as_ref().expect("metadata");
        let ddl_text = meta.get("ddl").expect("ddl key");
        assert!(ddl_text.contains("Events"), "DDL should include table name");
        assert!(
            ddl_text.contains("CREATE TABLE"),
            "DDL should include CREATE TABLE"
        );
    }

    #[test]
    fn file_path_stored_in_table_metadata() {
        let ddl = "CREATE TABLE T (\n    Id INT NOT NULL\n)";
        let rel = RelPath::new("migrations/v1/schema.sql");
        let (syms, _) = extract_ddl(&rel, ddl);

        let table = syms.iter().find(|s| s.kind == "db_table").expect("table");
        let meta = table.metadata.as_ref().expect("metadata");
        let file_val = meta.get("file").expect("file key");
        assert_eq!(file_val, "migrations/v1/schema.sql");
    }

    #[test]
    fn column_table_name_in_metadata() {
        let ddl = "CREATE TABLE Shipments (\n    ShipId INT NOT NULL\n)";
        let rel = RelPath::new("schema.sql");
        let (syms, _) = extract_ddl(&rel, ddl);

        let col = syms.iter().find(|s| s.kind == "db_column").expect("col");
        let meta = col.metadata.as_ref().expect("meta");
        assert_eq!(meta["table"], "Shipments");
    }

    #[test]
    fn create_procedure_not_treated_as_table() {
        let ddl = r#"
CREATE TABLE Employees (
    EmployeeId INT NOT NULL,
    Name NVARCHAR(100) NOT NULL
)

CREATE PROCEDURE GetEmployee
    @Id INT
AS
BEGIN
    SELECT * FROM Employees WHERE EmployeeId = @Id
END
"#;
        let rel = RelPath::new("schema.sql");
        let (syms, _) = extract_ddl(&rel, ddl);

        let tables: Vec<_> = syms.iter().filter(|s| s.kind == "db_table").collect();
        // Only Employees should be found, not any fake "table" from the stored procedure
        assert_eq!(
            tables.len(),
            1,
            "Only one table expected: {:?}",
            tables.iter().map(|t| &t.name).collect::<Vec<_>>()
        );
        assert_eq!(tables[0].name, "Employees");
    }

    #[test]
    fn create_unique_index_not_treated_as_table() {
        let ddl = "CREATE TABLE Cats (\n    CatId INT NOT NULL,\n    Tag VARCHAR(30) NOT NULL\n)\n\nCREATE UNIQUE INDEX UX_CatTag ON Cats (Tag);";
        let rel = RelPath::new("schema.sql");
        let (syms, _) = extract_ddl(&rel, ddl);

        let tables: Vec<_> = syms.iter().filter(|s| s.kind == "db_table").collect();
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].name, "Cats");
    }

    #[test]
    fn fk_constraint_syntax_with_brackets() {
        let ddl = r#"
CREATE TABLE [dbo].[LineItems] (
    [LineItemId] INT NOT NULL,
    [OrderId] INT NULL,
    FOREIGN KEY ([OrderId]) REFERENCES [dbo].[Orders]([OrderId])
)
"#;
        let rel = RelPath::new("schema.sql");
        let (_, edges) = extract_ddl(&rel, ddl);

        let fks: Vec<_> = edges.iter().filter(|e| e.kind == "foreign_key").collect();
        assert_eq!(fks.len(), 1);
        assert_eq!(fks[0].source_name, "column:lineitems:orderid");
        assert_eq!(fks[0].target_name, "column:orders:orderid");
    }

    #[test]
    fn whitespace_only_input_returns_empty() {
        let rel = RelPath::new("blank.sql");
        let (syms, edges) = extract_ddl(&rel, "   \n\t\n   ");
        assert!(syms.is_empty());
        assert!(edges.is_empty());
    }

    #[test]
    fn fk_edge_language_is_sql() {
        let ddl = "CREATE TABLE T (\n    A INT NOT NULL,\n    B INT NULL,\n    FOREIGN KEY (B) REFERENCES R(C)\n)";
        let rel = RelPath::new("schema.sql");
        let (_, edges) = extract_ddl(&rel, ddl);
        let fks: Vec<_> = edges.iter().filter(|e| e.kind == "foreign_key").collect();
        assert_eq!(fks.len(), 1);
        assert_eq!(fks[0].source_language, "sql");
    }

    #[test]
    fn has_column_edge_language_is_sql() {
        let ddl = "CREATE TABLE T (\n    A INT NOT NULL\n)";
        let rel = RelPath::new("schema.sql");
        let (_, edges) = extract_ddl(&rel, ddl);
        let has_col: Vec<_> = edges.iter().filter(|e| e.kind == "has_column").collect();
        assert_eq!(has_col.len(), 1);
        assert_eq!(has_col[0].source_language, "sql");
    }
}
