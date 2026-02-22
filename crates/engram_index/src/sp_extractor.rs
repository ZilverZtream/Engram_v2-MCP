/// Stored Procedure extractor.
///
/// Parses `.sql` files containing `CREATE PROCEDURE` / `ALTER PROCEDURE` statements
/// and extracts:
///   - `stored_procedure` symbols with parameter metadata
///   - `calls_stored_procedure` edges from code files calling SPs
///   - `stored_proc_reads_table` edges (SP → tables it SELECTs from)
///   - `stored_proc_writes_table` edges (SP → tables it INSERTs/UPDATEs/DELETEs)
///
/// Also scans VB/C# code-behind files for `CommandType.StoredProcedure` invocations
/// to discover code→SP call edges and parameter bindings.
use crate::parsing::{ExtractedEdge, ExtractedSymbol};
use engram_core::RelPath;
use regex::Regex;
use std::collections::HashMap;
use std::sync::OnceLock;

// ── Static Regex Definitions ─────────────────────────────────────────────────

static CREATE_PROC_RE: OnceLock<Regex> = OnceLock::new();
static ALTER_PROC_RE: OnceLock<Regex> = OnceLock::new();
static PARAM_LINE_RE: OnceLock<Regex> = OnceLock::new();
static SP_BODY_SELECT_RE: OnceLock<Regex> = OnceLock::new();
static SP_BODY_INSERT_RE: OnceLock<Regex> = OnceLock::new();
static SP_BODY_UPDATE_RE: OnceLock<Regex> = OnceLock::new();
static SP_BODY_DELETE_RE: OnceLock<Regex> = OnceLock::new();
static SP_BODY_MERGE_RE: OnceLock<Regex> = OnceLock::new();
static SP_BODY_JOIN_RE: OnceLock<Regex> = OnceLock::new();
static SP_BODY_EXEC_RE: OnceLock<Regex> = OnceLock::new();
static SP_DYNAMIC_SQL_RE: OnceLock<Regex> = OnceLock::new();
static SP_CURSOR_RE: OnceLock<Regex> = OnceLock::new();
static SP_RETURN_SELECT_RE: OnceLock<Regex> = OnceLock::new();
static CODE_CMD_TYPE_SP_RE: OnceLock<Regex> = OnceLock::new();
static CODE_CMD_TEXT_RE: OnceLock<Regex> = OnceLock::new();
static CODE_PARAM_ADD_RE: OnceLock<Regex> = OnceLock::new();
static CODE_PARAM_ADDWITHVALUE_RE: OnceLock<Regex> = OnceLock::new();
static SP_AS_KEYWORD_RE: OnceLock<Regex> = OnceLock::new();

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

/// Strip surrounding brackets `[Name]` → `Name`, and optional schema prefix.
fn strip_sql_name(s: &str) -> String {
    let s = s.trim();
    // Remove schema prefix: [dbo].[Name] or dbo.Name
    let name = if let Some(dot_pos) = s.rfind('.') {
        &s[dot_pos + 1..]
    } else {
        s
    };
    // Remove brackets
    let name = name.trim();
    if name.starts_with('[') && name.ends_with(']') {
        name[1..name.len() - 1].to_string()
    } else {
        name.to_string()
    }
}

/// Map T-SQL data types to C# equivalents.
fn sql_type_to_csharp(sql_type: &str) -> String {
    let upper = sql_type.to_uppercase();
    let base = upper.split('(').next().unwrap_or(&upper).trim();
    match base {
        "INT" | "INTEGER" => "int".to_string(),
        "BIGINT" => "long".to_string(),
        "SMALLINT" => "short".to_string(),
        "TINYINT" => "byte".to_string(),
        "BIT" => "bool".to_string(),
        "DECIMAL" | "NUMERIC" | "MONEY" | "SMALLMONEY" => "decimal".to_string(),
        "FLOAT" => "double".to_string(),
        "REAL" => "float".to_string(),
        "CHAR" | "VARCHAR" | "NCHAR" | "NVARCHAR" | "TEXT" | "NTEXT" | "XML" => {
            "string".to_string()
        }
        "DATE" | "DATETIME" | "DATETIME2" | "SMALLDATETIME" => "DateTime".to_string(),
        "DATETIMEOFFSET" => "DateTimeOffset".to_string(),
        "TIME" => "TimeSpan".to_string(),
        "UNIQUEIDENTIFIER" => "Guid".to_string(),
        "BINARY" | "VARBINARY" | "IMAGE" | "TIMESTAMP" | "ROWVERSION" => "byte[]".to_string(),
        "SQL_VARIANT" => "object".to_string(),
        "TABLE" => "DataTable".to_string(),
        "GEOGRAPHY" => "SqlGeography".to_string(),
        "GEOMETRY" => "SqlGeometry".to_string(),
        "HIERARCHYID" => "SqlHierarchyId".to_string(),
        _ => "object".to_string(),
    }
}

// ── Stored Procedure Definition Parsing ──────────────────────────────────────

/// A single parameter extracted from a stored procedure definition.
#[derive(Debug, Clone)]
pub struct SpParameter {
    pub name: String,
    pub sql_type: String,
    pub direction: String,
    pub default_value: Option<String>,
    pub csharp_type: String,
}

/// A stored procedure definition extracted from a .sql file.
#[derive(Debug, Clone)]
pub struct StoredProcedureDefinition {
    pub name: String,
    pub parameters: Vec<SpParameter>,
    pub tables_read: Vec<String>,
    pub tables_written: Vec<String>,
    pub called_procedures: Vec<String>,
    pub line_count: usize,
    pub has_dynamic_sql: bool,
    pub has_cursor: bool,
    pub return_columns: Vec<String>,
    pub start_line: u32,
    pub end_line: u32,
}

/// A code-side SP call extracted from VB/C# code-behind files.
#[derive(Debug, Clone)]
pub struct CodeSideSpCall {
    pub sp_name: String,
    pub parameters: Vec<CodeSideParameter>,
    pub line: u32,
}

/// A parameter binding found in code (Parameters.Add or AddWithValue).
#[derive(Debug, Clone)]
pub struct CodeSideParameter {
    pub name: String,
    pub sql_db_type: Option<String>,
    pub line: u32,
}

/// Extract stored procedure definitions from a .sql file.
///
/// Returns `(symbols, edges)` where symbols include `stored_procedure` entries
/// and edges include table read/write relationships and SP-to-SP calls.
pub fn extract_stored_procedures(
    rel_path: &RelPath,
    source: &str,
) -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>) {
    let mut symbols: Vec<ExtractedSymbol> = Vec::new();
    let mut edges: Vec<ExtractedEdge> = Vec::new();

    // Guard: skip huge files (> 10 MiB)
    if source.len() > 10 * 1024 * 1024 {
        return (symbols, edges);
    }

    let definitions = parse_sp_definitions(source);

    for sp in &definitions {
        // Build parameter metadata as JSON-compatible map
        let mut metadata = HashMap::new();
        metadata.insert("file".to_string(), rel_path.as_str().to_string());
        metadata.insert("param_count".to_string(), sp.parameters.len().to_string());
        metadata.insert("line_count".to_string(), sp.line_count.to_string());
        metadata.insert(
            "has_dynamic_sql".to_string(),
            sp.has_dynamic_sql.to_string(),
        );
        metadata.insert("has_cursor".to_string(), sp.has_cursor.to_string());

        // Serialize parameter signatures for downstream consumption
        let param_sigs: Vec<String> = sp
            .parameters
            .iter()
            .map(|p| {
                let mut sig = format!("{} {} {}", p.name, p.sql_type, p.direction);
                if let Some(ref dv) = p.default_value {
                    sig.push_str(&format!(" = {dv}"));
                }
                sig
            })
            .collect();
        metadata.insert("parameters".to_string(), param_sigs.join("; "));

        // Serialize C# type mappings
        let csharp_sigs: Vec<String> = sp
            .parameters
            .iter()
            .map(|p| format!("{}: {}", p.name, p.csharp_type))
            .collect();
        metadata.insert("csharp_types".to_string(), csharp_sigs.join("; "));

        if !sp.return_columns.is_empty() {
            metadata.insert("return_columns".to_string(), sp.return_columns.join(", "));
        }

        if !sp.called_procedures.is_empty() {
            metadata.insert(
                "calls_procedures".to_string(),
                sp.called_procedures.join(", "),
            );
        }

        symbols.push(ExtractedSymbol {
            name: sp.name.clone(),
            kind: "stored_procedure",
            start_line: sp.start_line,
            end_line: sp.end_line,
            metadata: Some(metadata),
        });

        // Emit edges: SP reads table
        for table in &sp.tables_read {
            edges.push(ExtractedEdge {
                source_name: sp.name.clone(),
                source_kind: "stored_procedure",
                source_start_line: sp.start_line,
                source_language: "sql",
                target_name: table.clone(),
                target_kind: Some("db_table"),
                target_start_line: None,
                kind: "stored_proc_reads_table",
                metadata: None,
            });
        }

        // Emit edges: SP writes table
        for table in &sp.tables_written {
            edges.push(ExtractedEdge {
                source_name: sp.name.clone(),
                source_kind: "stored_procedure",
                source_start_line: sp.start_line,
                source_language: "sql",
                target_name: table.clone(),
                target_kind: Some("db_table"),
                target_start_line: None,
                kind: "stored_proc_writes_table",
                metadata: None,
            });
        }

        // Emit edges: SP calls other SP
        for called in &sp.called_procedures {
            edges.push(ExtractedEdge {
                source_name: sp.name.clone(),
                source_kind: "stored_procedure",
                source_start_line: sp.start_line,
                source_language: "sql",
                target_name: called.clone(),
                target_kind: Some("stored_procedure"),
                target_start_line: None,
                kind: "calls_stored_procedure",
                metadata: None,
            });
        }
    }

    (symbols, edges)
}

/// Parse all stored procedure definitions from SQL source.
pub fn parse_sp_definitions(source: &str) -> Vec<StoredProcedureDefinition> {
    let mut results = Vec::new();

    // Build line offsets for line number computation
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

    // Match CREATE [OR ALTER] PROCEDURE and ALTER PROCEDURE
    let Some(create_re) = get_compiled_regex(
        &CREATE_PROC_RE,
        r"(?i)CREATE\s+(?:OR\s+ALTER\s+)?PROC(?:EDURE)?\s+((?:\[?\w+\]?\.)?(?:\[?\w+\]?))",
        "sp_create_proc",
    ) else {
        return results;
    };

    let Some(alter_re) = get_compiled_regex(
        &ALTER_PROC_RE,
        r"(?i)ALTER\s+PROC(?:EDURE)?\s+((?:\[?\w+\]?\.)?(?:\[?\w+\]?))",
        "sp_alter_proc",
    ) else {
        return results;
    };

    // Collect all procedure start positions (CREATE or ALTER)
    let mut proc_starts: Vec<(usize, String)> = Vec::new();

    for cap in create_re.captures_iter(source) {
        let m = cap.get(0).expect("full match");
        let name = strip_sql_name(cap.get(1).map_or("", |m| m.as_str()));
        proc_starts.push((m.start(), name));
    }

    for cap in alter_re.captures_iter(source) {
        let m = cap.get(0).expect("full match");
        let name = strip_sql_name(cap.get(1).map_or("", |m| m.as_str()));
        // Only add ALTER if no CREATE at the same position
        if !proc_starts.iter().any(|(pos, _)| *pos == m.start()) {
            proc_starts.push((m.start(), name));
        }
    }

    proc_starts.sort_by_key(|(pos, _)| *pos);

    // For each procedure, find its body (between AS and the next GO/CREATE/ALTER or EOF)
    for (idx, (start_pos, name)) in proc_starts.iter().enumerate() {
        let end_boundary = if idx + 1 < proc_starts.len() {
            proc_starts[idx + 1].0
        } else {
            source.len()
        };

        let proc_text = &source[*start_pos..end_boundary];

        // Find the AS keyword that marks the body start
        let body_start = find_as_keyword(proc_text);
        let param_region = if let Some(as_pos) = body_start {
            &proc_text[..as_pos]
        } else {
            proc_text
        };

        // Extract parameters from the region between the name and AS
        let parameters = extract_parameters(param_region);

        // Extract body (everything after AS, up to GO or end)
        let body = if let Some(as_pos) = body_start {
            let after_as = &proc_text[as_pos + 2..]; // skip "AS"
            // Trim optional BEGIN at the start
            let trimmed = after_as.trim_start();
            let body_content = if trimmed.to_uppercase().starts_with("BEGIN") {
                &trimmed[5..]
            } else {
                trimmed
            };
            // Trim GO at the end
            trim_go_suffix(body_content)
        } else {
            ""
        };

        // Analyze body for table references
        let tables_read = extract_tables_read(body);
        let tables_written = extract_tables_written(body);
        let called_procedures = extract_called_procedures(body);
        let has_dynamic_sql = check_dynamic_sql(body);
        let has_cursor = check_cursor(body);
        let return_columns = extract_return_columns(body);

        let start_line = char_to_line(*start_pos);
        let end_line = char_to_line(*start_pos + proc_text.len().saturating_sub(1));
        let line_count = (end_line - start_line + 1) as usize;

        results.push(StoredProcedureDefinition {
            name: name.clone(),
            parameters,
            tables_read,
            tables_written,
            called_procedures,
            line_count,
            has_dynamic_sql,
            has_cursor,
            return_columns,
            start_line,
            end_line,
        });
    }

    results
}

/// Find the position of the `AS` keyword that starts the procedure body.
/// Must appear after parameters, on its own or after a newline.
fn find_as_keyword(text: &str) -> Option<usize> {
    // Look for AS that's a standalone keyword (not part of ALIAS, CAST, etc.)
    let re = get_compiled_regex(
        &SP_AS_KEYWORD_RE,
        r"(?im)(?:^|\s)AS\s*$|(?:^|\s)AS\s+(?:BEGIN|SET|DECLARE|SELECT|INSERT|UPDATE|DELETE|IF|WHILE|EXEC|PRINT|RETURN|MERGE|WITH|--)",
        "sp_as_keyword",
    )?;

    if let Some(m) = re.find(text) {
        // Find the exact position of "AS" within the match
        let match_text = m.as_str().to_uppercase();
        if let Some(as_offset) = match_text.find("AS") {
            return Some(m.start() + as_offset);
        }
    }
    None
}

/// Trim trailing GO statement and anything after it.
/// Single-pass implementation: tracks byte offset while iterating lines.
fn trim_go_suffix(body: &str) -> &str {
    let mut offset = 0usize;
    for line in body.lines() {
        if line.trim().eq_ignore_ascii_case("GO") {
            return &body[..offset];
        }
        offset += line.len() + 1; // +1 for newline separator
    }
    body
}

/// Extract parameters from the region before AS.
fn extract_parameters(param_region: &str) -> Vec<SpParameter> {
    let Some(param_re) = get_compiled_regex(
        &PARAM_LINE_RE,
        r"(?i)(@\w+)\s+(\w+(?:\s*\([^)]*\))?(?:\s*\(\s*MAX\s*\))?)\s*(?:=\s*([^,\r\n]+?))?\s*(OUTPUT|OUT)?\s*(?:,|$)",
        "sp_param",
    ) else {
        return Vec::new();
    };

    let mut params = Vec::new();

    for cap in param_re.captures_iter(param_region) {
        let name = cap.get(1).map_or("", |m| m.as_str()).to_string();
        let sql_type_raw = cap.get(2).map_or("", |m| m.as_str()).trim().to_string();
        let default_value = cap.get(3).map(|m| m.as_str().trim().to_string());
        let is_output = cap.get(4).is_some();

        let direction = if is_output {
            if default_value.is_some() {
                "INOUT".to_string()
            } else {
                "OUT".to_string()
            }
        } else {
            "IN".to_string()
        };

        let csharp_type = sql_type_to_csharp(&sql_type_raw);

        params.push(SpParameter {
            name,
            sql_type: sql_type_raw,
            direction,
            default_value,
            csharp_type,
        });
    }

    params
}

/// Extract table names that the SP reads from (SELECT ... FROM / JOIN).
fn extract_tables_read(body: &str) -> Vec<String> {
    let mut tables = Vec::new();

    // SELECT ... FROM [schema.]table
    if let Some(re) = get_compiled_regex(
        &SP_BODY_SELECT_RE,
        r"(?i)\bFROM\s+((?:\[?\w+\]?\.)?(?:\[?\w+\]?))",
        "sp_select_from",
    ) {
        for cap in re.captures_iter(body) {
            let table = strip_sql_name(cap.get(1).map_or("", |m| m.as_str()));
            if !is_sql_keyword(&table) && !table.starts_with('@') && !table.starts_with('#') {
                if !tables.contains(&table) {
                    tables.push(table);
                }
            }
        }
    }

    // JOIN [schema.]table
    if let Some(re) = get_compiled_regex(
        &SP_BODY_JOIN_RE,
        r"(?i)\bJOIN\s+((?:\[?\w+\]?\.)?(?:\[?\w+\]?))",
        "sp_join",
    ) {
        for cap in re.captures_iter(body) {
            let table = strip_sql_name(cap.get(1).map_or("", |m| m.as_str()));
            if !is_sql_keyword(&table) && !table.starts_with('@') && !table.starts_with('#') {
                if !tables.contains(&table) {
                    tables.push(table);
                }
            }
        }
    }

    tables
}

/// Extract table names that the SP writes to (INSERT/UPDATE/DELETE/MERGE).
fn extract_tables_written(body: &str) -> Vec<String> {
    let mut tables = Vec::new();

    // INSERT INTO [schema.]table
    if let Some(re) = get_compiled_regex(
        &SP_BODY_INSERT_RE,
        r"(?i)\bINSERT\s+(?:INTO\s+)?((?:\[?\w+\]?\.)?(?:\[?\w+\]?))",
        "sp_insert",
    ) {
        for cap in re.captures_iter(body) {
            let table = strip_sql_name(cap.get(1).map_or("", |m| m.as_str()));
            if !is_sql_keyword(&table) && !table.starts_with('@') && !table.starts_with('#') {
                if !tables.contains(&table) {
                    tables.push(table);
                }
            }
        }
    }

    // UPDATE [schema.]table
    if let Some(re) = get_compiled_regex(
        &SP_BODY_UPDATE_RE,
        r"(?i)\bUPDATE\s+((?:\[?\w+\]?\.)?(?:\[?\w+\]?))\s+SET\b",
        "sp_update",
    ) {
        for cap in re.captures_iter(body) {
            let table = strip_sql_name(cap.get(1).map_or("", |m| m.as_str()));
            if !is_sql_keyword(&table) && !table.starts_with('@') && !table.starts_with('#') {
                if !tables.contains(&table) {
                    tables.push(table);
                }
            }
        }
    }

    // DELETE FROM [schema.]table
    if let Some(re) = get_compiled_regex(
        &SP_BODY_DELETE_RE,
        r"(?i)\bDELETE\s+(?:FROM\s+)?((?:\[?\w+\]?\.)?(?:\[?\w+\]?))",
        "sp_delete",
    ) {
        for cap in re.captures_iter(body) {
            let table = strip_sql_name(cap.get(1).map_or("", |m| m.as_str()));
            if !is_sql_keyword(&table) && !table.starts_with('@') && !table.starts_with('#') {
                if !tables.contains(&table) {
                    tables.push(table);
                }
            }
        }
    }

    // MERGE INTO [schema.]table
    if let Some(re) = get_compiled_regex(
        &SP_BODY_MERGE_RE,
        r"(?i)\bMERGE\s+(?:INTO\s+)?((?:\[?\w+\]?\.)?(?:\[?\w+\]?))",
        "sp_merge",
    ) {
        for cap in re.captures_iter(body) {
            let table = strip_sql_name(cap.get(1).map_or("", |m| m.as_str()));
            if !is_sql_keyword(&table) && !table.starts_with('@') && !table.starts_with('#') {
                if !tables.contains(&table) {
                    tables.push(table);
                }
            }
        }
    }

    tables
}

/// Extract called stored procedures (EXEC/EXECUTE calls within the body).
fn extract_called_procedures(body: &str) -> Vec<String> {
    let mut called = Vec::new();

    if let Some(re) = get_compiled_regex(
        &SP_BODY_EXEC_RE,
        r"(?i)\bEXEC(?:UTE)?\s+((?:\[?\w+\]?\.)?(?:\[?\w+\]?))(?:\s|$|;|,)",
        "sp_exec",
    ) {
        for cap in re.captures_iter(body) {
            let name = strip_sql_name(cap.get(1).map_or("", |m| m.as_str()));
            // Filter out system procedures and dynamic SQL (sp_executesql)
            if !name.eq_ignore_ascii_case("sp_executesql")
                && !name.starts_with('@')
                && !is_sql_keyword(&name)
                && !called.contains(&name)
            {
                called.push(name);
            }
        }
    }

    called
}

/// Check if the SP body contains dynamic SQL (EXEC(@sql) or sp_executesql).
fn check_dynamic_sql(body: &str) -> bool {
    if let Some(re) = get_compiled_regex(
        &SP_DYNAMIC_SQL_RE,
        r"(?i)\bEXEC(?:UTE)?\s*\(\s*@|sp_executesql",
        "sp_dynamic_sql",
    ) {
        re.is_match(body)
    } else {
        false
    }
}

/// Check if the SP body contains cursor usage.
fn check_cursor(body: &str) -> bool {
    if let Some(re) = get_compiled_regex(
        &SP_CURSOR_RE,
        r"(?i)\bDECLARE\s+\w+\s+CURSOR\b|\bOPEN\s+\w+\b.*\bFETCH\s+NEXT\b",
        "sp_cursor",
    ) {
        re.is_match(body)
    } else {
        false
    }
}

/// Extract column names from the first SELECT statement (return schema hint).
fn extract_return_columns(body: &str) -> Vec<String> {
    let Some(re) = get_compiled_regex(
        &SP_RETURN_SELECT_RE,
        r"(?i)\bSELECT\s+((?:(?:TOP\s+\d+\s+)?[\w.\[\]]+(?:\s+(?:AS\s+)?\w+)?(?:\s*,\s*[\w.\[\]]+(?:\s+(?:AS\s+)?\w+)?)*)\s*)\s*FROM\b",
        "sp_return_select",
    ) else {
        return Vec::new();
    };

    let Some(cap) = re.captures(body) else {
        return Vec::new();
    };

    let select_list = cap.get(1).map_or("", |m| m.as_str());
    let mut columns = Vec::new();

    for col_expr in select_list.split(',') {
        let col_expr = col_expr.trim();
        if col_expr.is_empty() || col_expr.eq_ignore_ascii_case("TOP") {
            continue;
        }
        // Get the alias or the column name
        let parts: Vec<&str> = col_expr.split_whitespace().collect();
        if parts.len() >= 3 && parts[parts.len() - 2].eq_ignore_ascii_case("AS") {
            // "col AS alias" → alias
            let alias = strip_sql_name(parts[parts.len() - 1]);
            columns.push(alias);
        } else if parts.len() >= 2
            && !parts[parts.len() - 1].eq_ignore_ascii_case("AS")
            && !parts[0].eq_ignore_ascii_case("TOP")
        {
            // "col alias" → alias
            let alias = strip_sql_name(parts[parts.len() - 1]);
            columns.push(alias);
        } else if let Some(last_part) = parts.last() {
            // "table.col" → col, or just "col" → col
            let name = if let Some(dot) = last_part.rfind('.') {
                strip_sql_name(&last_part[dot + 1..])
            } else {
                strip_sql_name(last_part)
            };
            if !name.eq_ignore_ascii_case("TOP") {
                columns.push(name);
            }
        }
    }

    columns
}

/// Check if a name is a SQL keyword (should not be treated as a table name).
fn is_sql_keyword(name: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "select",
        "from",
        "where",
        "insert",
        "update",
        "delete",
        "into",
        "values",
        "set",
        "join",
        "inner",
        "outer",
        "left",
        "right",
        "cross",
        "on",
        "and",
        "or",
        "not",
        "in",
        "exists",
        "between",
        "like",
        "is",
        "null",
        "as",
        "order",
        "by",
        "group",
        "having",
        "union",
        "all",
        "top",
        "distinct",
        "case",
        "when",
        "then",
        "else",
        "end",
        "begin",
        "commit",
        "rollback",
        "transaction",
        "return",
        "declare",
        "if",
        "while",
        "exec",
        "execute",
        "create",
        "alter",
        "drop",
        "table",
        "view",
        "procedure",
        "function",
        "trigger",
        "index",
        "constraint",
        "primary",
        "key",
        "foreign",
        "references",
        "output",
        "with",
        "nolock",
        "go",
        "use",
        "database",
        "schema",
        "grant",
        "revoke",
        "deny",
        "print",
        "raiserror",
        "throw",
        "try",
        "catch",
        "merge",
        "using",
        "matched",
        "source",
        "target",
        "openquery",
        "openrowset",
        "over",
        "partition",
        "cursor",
        "fetch",
        "next",
        "prior",
        "first",
        "last",
        "close",
        "deallocate",
        "scope_identity",
        "identity",
        "rowcount",
        "nocount",
        "asc",
        "desc",
        "default",
        "check",
        "unique",
        "clustered",
        "nonclustered",
        "truncate",
        "pivot",
        "unpivot",
        "cross_apply",
        "outer_apply",
        "tablesample",
        "compute",
        "option",
        "recompile",
        "inserted",
        "deleted",
    ];
    KEYWORDS.iter().any(|kw| name.eq_ignore_ascii_case(kw))
}

// ── Code-Side SP Call Extraction ─────────────────────────────────────────────

/// Extract stored procedure calls from VB/C# code-behind files.
///
/// Looks for patterns like:
///   cmd.CommandType = CommandType.StoredProcedure
///   cmd.CommandText = "usp_GetCustomerOrders"
///   cmd.Parameters.Add("@CustomerId", SqlDbType.Int)
///   cmd.Parameters.AddWithValue("@Name", name)
///
/// Returns edges of type `calls_stored_procedure` from the code file to the SP.
pub fn extract_code_side_sp_calls(
    rel_path: &RelPath,
    source: &str,
) -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>) {
    let symbols: Vec<ExtractedSymbol> = Vec::new();
    let mut edges: Vec<ExtractedEdge> = Vec::new();

    if source.len() > 5 * 1024 * 1024 {
        return (symbols, edges);
    }

    // Build line offsets
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

    // Detect CommandType.StoredProcedure usage
    let Some(cmd_type_re) = get_compiled_regex(
        &CODE_CMD_TYPE_SP_RE,
        r"(?i)CommandType\s*=\s*CommandType\.StoredProcedure",
        "code_cmd_type_sp",
    ) else {
        return (symbols, edges);
    };

    // Only process if file uses stored procedures
    if !cmd_type_re.is_match(source) {
        return (symbols, edges);
    }

    // Extract SP names from CommandText assignments
    let Some(cmd_text_re) = get_compiled_regex(
        &CODE_CMD_TEXT_RE,
        r#"(?i)\.CommandText\s*=\s*"([^"]+)""#,
        "code_cmd_text",
    ) else {
        return (symbols, edges);
    };

    // Extract parameters from Parameters.Add
    let param_add_re = get_compiled_regex(
        &CODE_PARAM_ADD_RE,
        r#"(?i)\.Parameters\.Add\s*\(\s*"(@\w+)"\s*,\s*SqlDbType\.(\w+)"#,
        "code_param_add",
    );

    // Extract parameters from Parameters.AddWithValue
    let param_awv_re = get_compiled_regex(
        &CODE_PARAM_ADDWITHVALUE_RE,
        r#"(?i)\.Parameters\.AddWithValue\s*\(\s*"(@\w+)""#,
        "code_param_awv",
    );

    // Collect all SP name assignments
    let mut sp_calls: Vec<CodeSideSpCall> = Vec::new();

    for cap in cmd_text_re.captures_iter(source) {
        let sp_name = cap.get(1).map_or("", |m| m.as_str()).trim().to_string();
        let match_pos = cap.get(0).map_or(0, |m| m.start());
        let line = char_to_line(match_pos);

        // Don't add SELECT/INSERT/etc. statements — only SP names
        let upper = sp_name.to_uppercase();
        if upper.starts_with("SELECT ")
            || upper.starts_with("INSERT ")
            || upper.starts_with("UPDATE ")
            || upper.starts_with("DELETE ")
            || upper.starts_with("EXEC ")
        {
            continue;
        }

        sp_calls.push(CodeSideSpCall {
            sp_name,
            parameters: Vec::new(),
            line,
        });
    }

    // For each SP call, look for nearby parameter bindings (within ±30 lines)
    for sp_call in &mut sp_calls {
        let target_line = sp_call.line;

        if let Some(re) = param_add_re {
            for cap in re.captures_iter(source) {
                let param_name = cap.get(1).map_or("", |m| m.as_str()).to_string();
                let sql_type = cap.get(2).map(|m| m.as_str().to_string());
                let match_pos = cap.get(0).map_or(0, |m| m.start());
                let param_line = char_to_line(match_pos);

                // Associate with nearest SP call (within ±30 lines)
                if param_line.abs_diff(target_line) <= 30 {
                    sp_call.parameters.push(CodeSideParameter {
                        name: param_name,
                        sql_db_type: sql_type,
                        line: param_line,
                    });
                }
            }
        }

        if let Some(re) = param_awv_re {
            for cap in re.captures_iter(source) {
                let param_name = cap.get(1).map_or("", |m| m.as_str()).to_string();
                let match_pos = cap.get(0).map_or(0, |m| m.start());
                let param_line = char_to_line(match_pos);

                if param_line.abs_diff(target_line) <= 30 {
                    sp_call.parameters.push(CodeSideParameter {
                        name: param_name,
                        sql_db_type: None,
                        line: param_line,
                    });
                }
            }
        }
    }

    // Emit symbols and edges for each discovered SP call
    for sp_call in &sp_calls {
        let mut metadata = HashMap::new();
        metadata.insert("file".to_string(), rel_path.as_str().to_string());
        metadata.insert(
            "param_count".to_string(),
            sp_call.parameters.len().to_string(),
        );

        if !sp_call.parameters.is_empty() {
            let param_list: Vec<String> = sp_call
                .parameters
                .iter()
                .map(|p| {
                    if let Some(ref t) = p.sql_db_type {
                        format!("{} ({})", p.name, t)
                    } else {
                        p.name.clone()
                    }
                })
                .collect();
            metadata.insert("code_params".to_string(), param_list.join(", "));
        }

        // Emit the edge: code file → stored procedure
        edges.push(ExtractedEdge {
            source_name: rel_path.as_str().to_string(),
            source_kind: "file",
            source_start_line: sp_call.line,
            source_language: if rel_path.as_str().ends_with(".vb") {
                "vb"
            } else {
                "csharp"
            },
            target_name: sp_call.sp_name.clone(),
            target_kind: Some("stored_procedure"),
            target_start_line: None,
            kind: "calls_stored_procedure",
            metadata: Some(metadata),
        });
    }

    (symbols, edges)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_create_procedure_with_in_params() {
        let sql = r#"
CREATE PROCEDURE [dbo].[usp_GetCustomerOrders]
    @CustomerId INT,
    @StartDate DATETIME,
    @EndDate DATETIME
AS
BEGIN
    SELECT OrderId, OrderDate, TotalAmount
    FROM Orders
    WHERE CustomerId = @CustomerId
      AND OrderDate BETWEEN @StartDate AND @EndDate
    ORDER BY OrderDate DESC
END
GO
"#;
        let rel = RelPath::new("procedures/orders.sql");
        let (syms, edges) = extract_stored_procedures(&rel, sql);

        assert_eq!(syms.len(), 1, "Expected 1 stored procedure symbol");
        assert_eq!(syms[0].name, "usp_GetCustomerOrders");
        assert_eq!(syms[0].kind, "stored_procedure");

        let meta = syms[0].metadata.as_ref().expect("metadata");
        assert_eq!(meta.get("param_count").expect("param_count"), "3");

        // Should read from Orders table
        let reads: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == "stored_proc_reads_table")
            .collect();
        assert!(
            reads.iter().any(|e| e.target_name == "Orders"),
            "Should detect SELECT FROM Orders"
        );
    }

    #[test]
    fn test_output_parameters() {
        let sql = r#"
CREATE PROCEDURE usp_CreateOrder
    @CustomerId INT,
    @OrderDate DATETIME,
    @NewOrderId INT OUTPUT,
    @StatusMessage NVARCHAR(200) OUTPUT
AS
BEGIN
    INSERT INTO Orders (CustomerId, OrderDate, Status)
    VALUES (@CustomerId, @OrderDate, 'Pending')

    SET @NewOrderId = SCOPE_IDENTITY()
    SET @StatusMessage = 'Order created successfully'
END
"#;
        let rel = RelPath::new("sp/create_order.sql");
        let defs = parse_sp_definitions(sql);

        assert_eq!(defs.len(), 1);
        let sp = &defs[0];
        assert_eq!(sp.name, "usp_CreateOrder");
        assert_eq!(sp.parameters.len(), 4);

        // Check IN params
        assert_eq!(sp.parameters[0].name, "@CustomerId");
        assert_eq!(sp.parameters[0].direction, "IN");
        assert_eq!(sp.parameters[0].csharp_type, "int");

        // Check OUTPUT params
        assert_eq!(sp.parameters[2].name, "@NewOrderId");
        assert_eq!(sp.parameters[2].direction, "OUT");
        assert_eq!(sp.parameters[2].csharp_type, "int");

        assert_eq!(sp.parameters[3].name, "@StatusMessage");
        assert_eq!(sp.parameters[3].direction, "OUT");
        assert_eq!(sp.parameters[3].csharp_type, "string");

        // Should write to Orders table
        assert!(
            sp.tables_written.contains(&"Orders".to_string()),
            "Should detect INSERT INTO Orders"
        );
    }

    #[test]
    fn test_default_values() {
        let sql = r#"
CREATE PROCEDURE usp_SearchProducts
    @SearchTerm NVARCHAR(100) = NULL,
    @CategoryId INT = 0,
    @PageSize INT = 25,
    @PageNumber INT = 1
AS
BEGIN
    SELECT ProductId, ProductName, Price
    FROM Products
    WHERE (@SearchTerm IS NULL OR ProductName LIKE '%' + @SearchTerm + '%')
      AND (@CategoryId = 0 OR CategoryId = @CategoryId)
    ORDER BY ProductName
    OFFSET (@PageNumber - 1) * @PageSize ROWS
    FETCH NEXT @PageSize ROWS ONLY
END
"#;
        let rel = RelPath::new("sp/search.sql");
        let defs = parse_sp_definitions(sql);

        assert_eq!(defs.len(), 1);
        let sp = &defs[0];
        assert_eq!(sp.parameters.len(), 4);

        assert_eq!(
            sp.parameters[0].default_value.as_deref(),
            Some("NULL"),
            "SearchTerm should have default NULL"
        );
        assert_eq!(
            sp.parameters[1].default_value.as_deref(),
            Some("0"),
            "CategoryId should have default 0"
        );
        assert_eq!(
            sp.parameters[2].default_value.as_deref(),
            Some("25"),
            "PageSize should have default 25"
        );
    }

    #[test]
    fn test_multi_statement_sp_with_insert_update_select() {
        let sql = r#"
CREATE PROCEDURE usp_ProcessOrder
    @OrderId INT
AS
BEGIN
    UPDATE Orders SET Status = 'Processing' WHERE OrderId = @OrderId

    INSERT INTO OrderHistory (OrderId, Action, ActionDate)
    VALUES (@OrderId, 'Processing', GETDATE())

    SELECT o.OrderId, o.Status, c.CustomerName
    FROM Orders o
    INNER JOIN Customers c ON o.CustomerId = c.CustomerId
    WHERE o.OrderId = @OrderId
END
"#;
        let rel = RelPath::new("sp/process.sql");
        let (syms, edges) = extract_stored_procedures(&rel, sql);

        assert_eq!(syms.len(), 1);

        let reads: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == "stored_proc_reads_table")
            .map(|e| e.target_name.as_str())
            .collect();
        let writes: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == "stored_proc_writes_table")
            .map(|e| e.target_name.as_str())
            .collect();

        assert!(reads.contains(&"Orders"), "Should read Orders via SELECT");
        assert!(
            reads.contains(&"Customers"),
            "Should read Customers via JOIN"
        );
        assert!(writes.contains(&"Orders"), "Should write Orders via UPDATE");
        assert!(
            writes.contains(&"OrderHistory"),
            "Should write OrderHistory via INSERT"
        );
    }

    #[test]
    fn test_code_side_sql_command_parameter_extraction() {
        let code = r#"
Private Sub LoadOrders()
    Dim cmd As New SqlCommand()
    cmd.Connection = conn
    cmd.CommandType = CommandType.StoredProcedure
    cmd.CommandText = "usp_GetCustomerOrders"
    cmd.Parameters.Add("@CustomerId", SqlDbType.Int).Value = Me.CustomerId
    cmd.Parameters.Add("@StartDate", SqlDbType.DateTime).Value = dtStart.Value
    cmd.Parameters.Add("@EndDate", SqlDbType.DateTime).Value = dtEnd.Value

    Dim reader As SqlDataReader = cmd.ExecuteReader()
    While reader.Read()
        ' process rows
    End While
End Sub
"#;
        let rel = RelPath::new("pages/Orders.aspx.vb");
        let (_, edges) = extract_code_side_sp_calls(&rel, code);

        assert!(!edges.is_empty(), "Should detect SP call from code-behind");

        let sp_edge = edges
            .iter()
            .find(|e| e.kind == "calls_stored_procedure")
            .expect("Should have calls_stored_procedure edge");
        assert_eq!(sp_edge.target_name, "usp_GetCustomerOrders");
        assert_eq!(sp_edge.source_name, "pages/Orders.aspx.vb");

        let meta = sp_edge.metadata.as_ref().expect("metadata");
        assert_eq!(meta.get("param_count").expect("param_count"), "3");
    }

    #[test]
    fn test_sp_calling_other_sp() {
        let sql = r#"
CREATE PROCEDURE usp_PlaceOrder
    @CustomerId INT,
    @ProductId INT,
    @Quantity INT
AS
BEGIN
    DECLARE @OrderId INT

    EXEC usp_ValidateInventory @ProductId, @Quantity
    EXEC usp_CreateOrder @CustomerId, @OrderId OUTPUT
    EXEC usp_AddOrderLine @OrderId, @ProductId, @Quantity
    EXEC usp_SendConfirmationEmail @OrderId
END
"#;
        let rel = RelPath::new("sp/place_order.sql");
        let (syms, edges) = extract_stored_procedures(&rel, sql);

        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "usp_PlaceOrder");

        let sp_calls: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == "calls_stored_procedure")
            .map(|e| e.target_name.as_str())
            .collect();
        assert_eq!(sp_calls.len(), 4, "Should detect 4 EXEC calls");
        assert!(sp_calls.contains(&"usp_ValidateInventory"));
        assert!(sp_calls.contains(&"usp_CreateOrder"));
        assert!(sp_calls.contains(&"usp_AddOrderLine"));
        assert!(sp_calls.contains(&"usp_SendConfirmationEmail"));
    }

    #[test]
    fn test_dynamic_sql_detection() {
        let sql = r#"
CREATE PROCEDURE usp_DynamicSearch
    @TableName NVARCHAR(128),
    @SearchColumn NVARCHAR(128),
    @SearchValue NVARCHAR(500)
AS
BEGIN
    DECLARE @sql NVARCHAR(MAX)
    SET @sql = N'SELECT * FROM ' + QUOTENAME(@TableName) +
               N' WHERE ' + QUOTENAME(@SearchColumn) + N' = @val'

    EXEC sp_executesql @sql, N'@val NVARCHAR(500)', @SearchValue
END
"#;
        let rel = RelPath::new("sp/dynamic.sql");
        let defs = parse_sp_definitions(sql);

        assert_eq!(defs.len(), 1);
        assert!(
            defs[0].has_dynamic_sql,
            "Should detect dynamic SQL via sp_executesql"
        );
    }

    #[test]
    fn test_cursor_detection() {
        let sql = r#"
CREATE PROCEDURE usp_ProcessBatch
AS
BEGIN
    DECLARE @OrderId INT
    DECLARE order_cursor CURSOR FOR
        SELECT OrderId FROM Orders WHERE Status = 'Pending'

    OPEN order_cursor
    FETCH NEXT FROM order_cursor INTO @OrderId

    WHILE @@FETCH_STATUS = 0
    BEGIN
        EXEC usp_ProcessSingleOrder @OrderId
        FETCH NEXT FROM order_cursor INTO @OrderId
    END

    CLOSE order_cursor
    DEALLOCATE order_cursor
END
"#;
        let rel = RelPath::new("sp/batch.sql");
        let defs = parse_sp_definitions(sql);

        assert_eq!(defs.len(), 1);
        assert!(defs[0].has_cursor, "Should detect CURSOR usage");
        assert!(
            defs[0].tables_read.contains(&"Orders".to_string()),
            "Should detect SELECT FROM Orders"
        );
    }

    #[test]
    fn test_multiple_procedures_in_one_file() {
        let sql = r#"
CREATE PROCEDURE usp_GetCustomer
    @CustomerId INT
AS
BEGIN
    SELECT CustomerId, Name, Email FROM Customers WHERE CustomerId = @CustomerId
END
GO

CREATE PROCEDURE usp_UpdateCustomer
    @CustomerId INT,
    @Name NVARCHAR(100),
    @Email NVARCHAR(255)
AS
BEGIN
    UPDATE Customers SET Name = @Name, Email = @Email WHERE CustomerId = @CustomerId
END
GO

CREATE PROCEDURE usp_DeleteCustomer
    @CustomerId INT
AS
BEGIN
    DELETE FROM Orders WHERE CustomerId = @CustomerId
    DELETE FROM Customers WHERE CustomerId = @CustomerId
END
GO
"#;
        let rel = RelPath::new("sp/customers.sql");
        let (syms, _edges) = extract_stored_procedures(&rel, sql);

        assert_eq!(syms.len(), 3, "Should find 3 stored procedures");
        let names: Vec<_> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"usp_GetCustomer"));
        assert!(names.contains(&"usp_UpdateCustomer"));
        assert!(names.contains(&"usp_DeleteCustomer"));
    }

    #[test]
    fn test_alter_procedure() {
        let sql = r#"
ALTER PROCEDURE [dbo].[usp_GetReport]
    @ReportDate DATE,
    @DepartmentId INT = NULL
AS
BEGIN
    SELECT r.ReportId, r.Title, d.DeptName
    FROM Reports r
    LEFT JOIN Departments d ON r.DepartmentId = d.DepartmentId
    WHERE r.ReportDate = @ReportDate
      AND (@DepartmentId IS NULL OR r.DepartmentId = @DepartmentId)
END
"#;
        let rel = RelPath::new("sp/alter_report.sql");
        let defs = parse_sp_definitions(sql);

        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "usp_GetReport");
        assert_eq!(defs[0].parameters.len(), 2);
        assert!(
            defs[0].tables_read.contains(&"Reports".to_string()),
            "Should detect Reports table"
        );
        assert!(
            defs[0].tables_read.contains(&"Departments".to_string()),
            "Should detect Departments via JOIN"
        );
    }

    #[test]
    fn test_code_side_csharp_sp_call() {
        let code = r#"
protected void btnSave_Click(object sender, EventArgs e)
{
    using (var conn = new SqlConnection(connStr))
    {
        conn.Open();
        var cmd = new SqlCommand();
        cmd.Connection = conn;
        cmd.CommandType = CommandType.StoredProcedure;
        cmd.CommandText = "usp_SaveEmployee";
        cmd.Parameters.Add("@EmployeeId", SqlDbType.Int).Value = int.Parse(txtId.Text);
        cmd.Parameters.Add("@Name", SqlDbType.NVarChar, 100).Value = txtName.Text;
        cmd.Parameters.AddWithValue("@Email", txtEmail.Text);
        cmd.ExecuteNonQuery();
    }
}
"#;
        let rel = RelPath::new("pages/Employee.aspx.cs");
        let (_, edges) = extract_code_side_sp_calls(&rel, code);

        assert!(!edges.is_empty(), "Should detect C# SP call");
        let edge = &edges[0];
        assert_eq!(edge.target_name, "usp_SaveEmployee");
        assert_eq!(edge.kind, "calls_stored_procedure");

        let meta = edge.metadata.as_ref().expect("metadata");
        assert_eq!(meta.get("param_count").expect("pc"), "3");
    }

    #[test]
    fn test_sql_type_mapping() {
        assert_eq!(sql_type_to_csharp("INT"), "int");
        assert_eq!(sql_type_to_csharp("BIGINT"), "long");
        assert_eq!(sql_type_to_csharp("NVARCHAR(100)"), "string");
        assert_eq!(sql_type_to_csharp("DATETIME"), "DateTime");
        assert_eq!(sql_type_to_csharp("UNIQUEIDENTIFIER"), "Guid");
        assert_eq!(sql_type_to_csharp("DECIMAL(18,2)"), "decimal");
        assert_eq!(sql_type_to_csharp("BIT"), "bool");
        assert_eq!(sql_type_to_csharp("VARBINARY(MAX)"), "byte[]");
        assert_eq!(sql_type_to_csharp("MONEY"), "decimal");
        assert_eq!(sql_type_to_csharp("GEOGRAPHY"), "SqlGeography");
    }

    #[test]
    fn test_return_columns_extraction() {
        let body = r#"
    SELECT o.OrderId, o.OrderDate, c.CustomerName AS Customer, o.TotalAmount
    FROM Orders o
    JOIN Customers c ON o.CustomerId = c.CustomerId
"#;
        let cols = extract_return_columns(body);
        assert!(cols.contains(&"OrderId".to_string()));
        assert!(cols.contains(&"OrderDate".to_string()));
        assert!(cols.contains(&"Customer".to_string())); // AS alias
        assert!(cols.contains(&"TotalAmount".to_string()));
    }

    #[test]
    fn test_merge_statement_detection() {
        let sql = r#"
CREATE PROCEDURE usp_UpsertProduct
    @ProductId INT,
    @Name NVARCHAR(100),
    @Price DECIMAL(18,2)
AS
BEGIN
    MERGE INTO Products AS target
    USING (SELECT @ProductId AS ProductId) AS source
    ON target.ProductId = source.ProductId
    WHEN MATCHED THEN
        UPDATE SET Name = @Name, Price = @Price
    WHEN NOT MATCHED THEN
        INSERT (ProductId, Name, Price) VALUES (@ProductId, @Name, @Price);
END
"#;
        let defs = parse_sp_definitions(sql);
        assert_eq!(defs.len(), 1);
        assert!(
            defs[0].tables_written.contains(&"Products".to_string()),
            "Should detect MERGE INTO Products as a write"
        );
    }

    #[test]
    fn test_strip_sql_name() {
        assert_eq!(strip_sql_name("[dbo].[Orders]"), "Orders");
        assert_eq!(strip_sql_name("dbo.Orders"), "Orders");
        assert_eq!(strip_sql_name("[Orders]"), "Orders");
        assert_eq!(strip_sql_name("Orders"), "Orders");
        assert_eq!(strip_sql_name("  [dbo].[MyTable]  "), "MyTable");
    }

    #[test]
    fn test_no_false_positive_on_inline_sql() {
        let code = r#"
Private Sub LoadData()
    Dim cmd As New SqlCommand("SELECT * FROM Orders WHERE Status = 'Active'", conn)
    Dim reader As SqlDataReader = cmd.ExecuteReader()
End Sub
"#;
        let rel = RelPath::new("pages/Data.aspx.vb");
        let (_, edges) = extract_code_side_sp_calls(&rel, code);

        assert!(
            edges.is_empty(),
            "Should NOT detect inline SQL as SP call — no CommandType.StoredProcedure"
        );
    }

    #[test]
    fn test_exec_with_schema_prefix() {
        let sql = r#"
CREATE PROCEDURE usp_Orchestrator
AS
BEGIN
    EXEC [dbo].[usp_Step1]
    EXEC dbo.usp_Step2
    EXECUTE [usp_Step3]
END
"#;
        let defs = parse_sp_definitions(sql);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].called_procedures.len(), 3);
        assert!(defs[0].called_procedures.contains(&"usp_Step1".to_string()));
        assert!(defs[0].called_procedures.contains(&"usp_Step2".to_string()));
        assert!(defs[0].called_procedures.contains(&"usp_Step3".to_string()));
    }
}
