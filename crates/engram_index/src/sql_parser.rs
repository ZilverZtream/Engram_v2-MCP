//! Lightweight SQL statement analyzer for scaffold generation.
//!
//! Extracts structural information from SQL strings captured in `SqlCalls` edge
//! metadata — tables, columns, parameters, JOINs, WHERE bindings, aggregations.
//! Not a full SQL grammar parser; uses regex heuristics that handle 70%+ of
//! real-world legacy SQL reliably, which is vastly better than the previous
//! "just look at the first keyword" approach.

use std::sync::OnceLock;

use regex::Regex;
use serde::Serialize;

// ─── Output types ──────────────────────────────────────────────────────────────

/// The kind of SQL operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum SqlOp {
    Select,
    Insert,
    Update,
    Delete,
    Exec,
}

impl std::fmt::Display for SqlOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Select => write!(f, "SELECT"),
            Self::Insert => write!(f, "INSERT"),
            Self::Update => write!(f, "UPDATE"),
            Self::Delete => write!(f, "DELETE"),
            Self::Exec => write!(f, "EXEC"),
        }
    }
}

/// A JOIN clause extracted from a SQL statement.
#[derive(Debug, Clone, Serialize)]
pub struct JoinInfo {
    pub table: String,
    pub alias: Option<String>,
    pub join_type: String,
    pub on_condition: Option<String>,
}

/// A column reference from a SELECT clause.
#[derive(Debug, Clone, Serialize)]
pub struct ColumnRef {
    pub table_alias: Option<String>,
    pub column_name: String,
    pub alias: Option<String>,
}

/// A SQL parameter (@ or ? placeholder).
#[derive(Debug, Clone, Serialize)]
pub struct SqlParam {
    pub name: String,
    pub inferred_type: String,
}

/// A binding between a WHERE parameter and the column it's compared against.
#[derive(Debug, Clone, Serialize)]
pub struct ParamColumnBinding {
    pub param_name: String,
    pub column: String,
    pub operator: String,
}

/// Complete structural analysis of a SQL statement.
#[derive(Debug, Clone, Serialize)]
pub struct SqlAnalysis {
    pub operation: SqlOp,
    pub primary_table: Option<String>,
    pub joined_tables: Vec<JoinInfo>,
    pub selected_columns: Vec<ColumnRef>,
    pub parameters: Vec<SqlParam>,
    pub where_bindings: Vec<ParamColumnBinding>,
    pub has_aggregation: bool,
    pub group_by_columns: Vec<String>,
    pub has_subquery: bool,
    pub has_cte: bool,
    pub is_multi_statement: bool,
    pub raw_sql: String,
}

// ─── Regex singletons ──────────────────────────────────────────────────────────

fn re_join() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Match JOIN clauses. ON condition captured non-greedily up to the next major
        // SQL keyword (WHERE, GROUP, ORDER, HAVING, UNION, JOIN variants, SET, or semicolon).
        Regex::new(r"(?i)\b(INNER\s+JOIN|LEFT\s+(?:OUTER\s+)?JOIN|RIGHT\s+(?:OUTER\s+)?JOIN|CROSS\s+JOIN|FULL\s+(?:OUTER\s+)?JOIN|JOIN)\s+(\[?\w+\]?)(?:\s+(?:AS\s+)?(\w+))?(?:\s+ON\s+(.+?))?(?:\s+(?:INNER|LEFT|RIGHT|CROSS|FULL|JOIN|WHERE|GROUP|ORDER|HAVING|UNION|SET)\b|;|$)").expect("re_join")
    })
}

fn re_param() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"@(\w+)").expect("re_param"))
}

fn re_where_binding() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(\[?\w+\]?\.)?(\[?\w+\]?)\s*(=|!=|<>|>=?|<=?|LIKE|IN)\s*\(?@(\w+)")
            .expect("re_where_binding")
    })
}

fn re_aggregate() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\b(COUNT|SUM|AVG|MAX|MIN)\s*\(").expect("re_agg"))
}

fn re_group_by() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\bGROUP\s+BY\s+((?:\[?\w+\]?\.?\[?\w+\]?(?:\s*,\s*)?)+)")
            .expect("re_group")
    })
}

fn re_from_table() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\bFROM\s+(\[?\w+\]?)(?:\s+(?:AS\s+)?(\w+))?").expect("re_from")
    })
}

fn re_insert_table() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\bINSERT\s+INTO\s+(\[?\w+\]?)").expect("re_insert"))
}

fn re_update_table() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\bUPDATE\s+(\[?\w+\]?)").expect("re_update"))
}

fn re_delete_table() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\bDELETE\s+(?:FROM\s+)?(\[?\w+\]?)").expect("re_delete"))
}

fn re_exec() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\b(?:EXEC(?:UTE)?)\s+(\[?\w+\]?)").expect("re_exec"))
}

#[allow(dead_code)]
fn re_exec_params() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"@(\w+)\s*=\s*@(\w+)").expect("re_exec_params"))
}

fn re_select_columns() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\bSELECT\s+(?:DISTINCT\s+|TOP\s+\d+\s+)?(.*?)(?:\bFROM\b)")
            .expect("re_sel_cols")
    })
}

fn re_insert_columns() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\bINSERT\s+INTO\s+\[?\w+\]?\s*\(([^)]+)\)").expect("re_ins_cols")
    })
}

fn re_update_set() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\bSET\s+(.*?)(?:\bWHERE\b|\bFROM\b|;|$)").expect("re_up_set")
    })
}

// ─── Public API ────────────────────────────────────────────────────────────────

/// Analyze a SQL string and return a structured `SqlAnalysis`.
pub fn analyze_sql(sql: &str) -> SqlAnalysis {
    let normalized = normalize_sql(sql);
    let operation = detect_operation(&normalized);

    let primary_table = detect_primary_table(&normalized, operation);
    let joined_tables = detect_joins(&normalized);
    let selected_columns = detect_select_columns(&normalized, operation);
    let parameters = detect_parameters(&normalized);
    let where_bindings = detect_where_bindings(&normalized);
    let has_aggregation = re_aggregate().is_match(&normalized);
    let group_by_columns = detect_group_by(&normalized);
    let has_subquery = detect_subquery(&normalized);
    let has_cte = detect_cte(&normalized);
    let is_multi_statement = detect_multi_statement(&normalized);

    // Infer parameter types from where bindings
    let parameters = infer_param_types(parameters, &where_bindings);

    SqlAnalysis {
        operation,
        primary_table,
        joined_tables,
        selected_columns,
        parameters,
        where_bindings,
        has_aggregation,
        group_by_columns,
        has_subquery,
        has_cte,
        is_multi_statement,
        raw_sql: sql.to_string(),
    }
}

/// Generate a repository method name from a SQL analysis.
pub fn generate_method_name(analysis: &SqlAnalysis) -> String {
    let entity = analysis
        .primary_table
        .as_deref()
        .map(|t| to_pascal(strip_brackets(t)))
        .unwrap_or_else(|| "Data".into());

    match analysis.operation {
        SqlOp::Select => {
            if analysis.has_aggregation {
                generate_aggregate_method_name(analysis, &entity)
            } else if analysis.where_bindings.is_empty() && analysis.parameters.is_empty() {
                format!("GetAll{entity}Async")
            } else {
                let param_part = generate_by_clause(&analysis.where_bindings, &analysis.parameters);
                format!("Get{entity}By{param_part}Async")
            }
        }
        SqlOp::Insert => format!("Create{entity}Async"),
        SqlOp::Update => {
            if analysis.where_bindings.is_empty() && analysis.parameters.is_empty() {
                format!("Update{entity}Async")
            } else {
                let param_part = generate_by_clause(&analysis.where_bindings, &analysis.parameters);
                format!("Update{entity}By{param_part}Async")
            }
        }
        SqlOp::Delete => {
            if analysis.where_bindings.is_empty() && analysis.parameters.is_empty() {
                format!("Delete{entity}Async")
            } else {
                let param_part = generate_by_clause(&analysis.where_bindings, &analysis.parameters);
                format!("Delete{entity}By{param_part}Async")
            }
        }
        SqlOp::Exec => {
            let proc = analysis
                .primary_table
                .as_deref()
                .map(|t| to_pascal(strip_brackets(t)))
                .unwrap_or_else(|| "Procedure".into());
            format!("Execute{proc}Async")
        }
    }
}

/// Generate a method signature including parameter list and return type.
pub fn generate_method_signature(analysis: &SqlAnalysis) -> String {
    let method_name = generate_method_name(analysis);
    let entity = analysis
        .primary_table
        .as_deref()
        .map(|t| to_pascal(strip_brackets(t)))
        .unwrap_or_else(|| "Data".into());

    let params = generate_param_list(analysis);

    let return_type = match analysis.operation {
        SqlOp::Select => {
            if analysis.has_aggregation {
                "Task<int>".to_string()
            } else {
                let dto_name = if analysis.joined_tables.is_empty() {
                    entity
                } else {
                    let join_suffix: String = analysis
                        .joined_tables
                        .iter()
                        .take(2) // limit to 2 joins for readability
                        .map(|j| to_pascal(strip_brackets(&j.table)))
                        .collect::<Vec<_>>()
                        .join("");
                    format!("{entity}With{join_suffix}")
                };
                format!("Task<IEnumerable<{dto_name}>>")
            }
        }
        SqlOp::Insert => format!("Task<int>"),
        SqlOp::Update | SqlOp::Delete => "Task".to_string(),
        SqlOp::Exec => format!("Task<IEnumerable<{entity}>>"),
    };

    if params.is_empty() {
        format!("{return_type} {method_name}()")
    } else {
        format!("{return_type} {method_name}({params})")
    }
}

/// Generate a composite DTO class from a JOIN query analysis.
pub fn generate_composite_dto(analysis: &SqlAnalysis) -> Option<String> {
    if analysis.joined_tables.is_empty() || analysis.operation != SqlOp::Select {
        return None;
    }

    let entity = analysis
        .primary_table
        .as_deref()
        .map(|t| to_pascal(strip_brackets(t)))
        .unwrap_or_else(|| "Data".into());

    let join_suffix: String = analysis
        .joined_tables
        .iter()
        .take(2)
        .map(|j| to_pascal(strip_brackets(&j.table)))
        .collect::<Vec<_>>()
        .join("");

    let class_name = format!("{entity}With{join_suffix}");
    let mut code = String::with_capacity(512);
    code.push_str(&format!("public class {class_name}\n{{\n"));

    // Build alias→table mapping for column attribution
    let mut alias_to_table = std::collections::HashMap::new();
    if let Some(ref pt) = analysis.primary_table {
        let stripped = strip_brackets(pt);
        alias_to_table.insert(stripped.to_lowercase(), stripped.to_string());
        // Check FROM clause for alias
        // We'll use a simple heuristic: first letter lowercase of table name
    }
    for join in &analysis.joined_tables {
        let stripped = strip_brackets(&join.table);
        if let Some(ref a) = join.alias {
            alias_to_table.insert(a.to_lowercase(), stripped.to_string());
        }
        alias_to_table.insert(stripped.to_lowercase(), stripped.to_string());
    }

    // Group columns by their source table
    let mut primary_cols = Vec::new();
    let mut joined_cols: std::collections::BTreeMap<String, Vec<&ColumnRef>> =
        std::collections::BTreeMap::new();

    for col in &analysis.selected_columns {
        if col.column_name == "*" {
            continue;
        }
        let source_table = col
            .table_alias
            .as_deref()
            .and_then(|a| alias_to_table.get(&a.to_lowercase()))
            .cloned();

        match &source_table {
            Some(t)
                if analysis
                    .primary_table
                    .as_deref()
                    .map(|pt| strip_brackets(pt))
                    == Some(strip_brackets(t)) =>
            {
                primary_cols.push(col);
            }
            Some(t) => {
                joined_cols.entry(t.clone()).or_default().push(col);
            }
            None => {
                primary_cols.push(col);
            }
        }
    }

    if !primary_cols.is_empty() {
        let pt_name = analysis
            .primary_table
            .as_deref()
            .map(|t| strip_brackets(t))
            .unwrap_or("Primary");
        code.push_str(&format!("    // From {pt_name} (primary)\n"));
        for col in &primary_cols {
            let prop_name = to_pascal(
                col.alias
                    .as_deref()
                    .unwrap_or(strip_brackets(&col.column_name)),
            );
            let prop_type = infer_csharp_type_extended(&prop_name);
            code.push_str(&format!(
                "    public {prop_type} {prop_name} {{ get; set; }}\n"
            ));
        }
    }

    for (table, cols) in &joined_cols {
        code.push_str(&format!("    // From {} (joined)\n", strip_brackets(table)));
        for col in cols {
            let prop_name = to_pascal(
                col.alias
                    .as_deref()
                    .unwrap_or(strip_brackets(&col.column_name)),
            );
            let prop_type = infer_csharp_type_extended(&prop_name);
            code.push_str(&format!(
                "    public {prop_type} {prop_name} {{ get; set; }}\n"
            ));
        }
    }

    // If no columns were extracted, add a placeholder
    if primary_cols.is_empty() && joined_cols.is_empty() {
        code.push_str("    // TODO: add properties — columns could not be parsed from SQL\n");
    }

    code.push_str("}\n");
    Some(code)
}

// ─── Internal helpers ──────────────────────────────────────────────────────────

/// Normalize SQL: collapse whitespace, strip VB string concatenation artifacts.
fn normalize_sql(sql: &str) -> String {
    let s = sql
        .replace("\" & \"", " ")
        .replace("\" + \"", " ")
        .replace("\" &_\n\"", " ")
        .replace("\" & _\r\n\"", " ")
        .replace("\" & vbCrLf & \"", " ")
        .replace('\r', " ")
        .replace('\n', " ")
        .replace('\t', " ");
    // Collapse multiple whitespace
    let mut result = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_space {
                result.push(' ');
            }
            prev_space = true;
        } else {
            result.push(c);
            prev_space = false;
        }
    }
    result.trim().to_string()
}

fn detect_operation(sql: &str) -> SqlOp {
    let trimmed = sql.trim_start();
    let upper = trimmed.to_uppercase();
    // Check for CTE: WITH ... AS (...) SELECT
    if upper.starts_with("WITH ") {
        return SqlOp::Select;
    }
    if upper.starts_with("SELECT") {
        SqlOp::Select
    } else if upper.starts_with("INSERT") {
        SqlOp::Insert
    } else if upper.starts_with("UPDATE") {
        SqlOp::Update
    } else if upper.starts_with("DELETE") {
        SqlOp::Delete
    } else if upper.starts_with("EXEC") {
        SqlOp::Exec
    } else {
        SqlOp::Select // default fallback
    }
}

fn detect_primary_table(sql: &str, op: SqlOp) -> Option<String> {
    match op {
        SqlOp::Select => re_from_table()
            .captures(sql)
            .map(|c| c.get(1).map(|m| m.as_str().to_string()))
            .flatten(),
        SqlOp::Insert => re_insert_table()
            .captures(sql)
            .map(|c| c.get(1).map(|m| m.as_str().to_string()))
            .flatten(),
        SqlOp::Update => re_update_table()
            .captures(sql)
            .map(|c| c.get(1).map(|m| m.as_str().to_string()))
            .flatten(),
        SqlOp::Delete => re_delete_table()
            .captures(sql)
            .map(|c| c.get(1).map(|m| m.as_str().to_string()))
            .flatten(),
        SqlOp::Exec => re_exec()
            .captures(sql)
            .map(|c| c.get(1).map(|m| m.as_str().to_string()))
            .flatten(),
    }
}

fn detect_joins(sql: &str) -> Vec<JoinInfo> {
    re_join()
        .captures_iter(sql)
        .map(|cap| {
            let join_type = cap
                .get(1)
                .map(|m| m.as_str().to_uppercase())
                .unwrap_or_else(|| "JOIN".into());
            let table = cap
                .get(2)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            let alias = cap.get(3).map(|m| m.as_str().to_string());
            let on_condition = cap.get(4).map(|m| m.as_str().trim().to_string());
            JoinInfo {
                table,
                alias,
                join_type,
                on_condition,
            }
        })
        .collect()
}

fn detect_select_columns(sql: &str, op: SqlOp) -> Vec<ColumnRef> {
    match op {
        SqlOp::Select => parse_select_list(sql),
        SqlOp::Insert => parse_insert_columns(sql),
        SqlOp::Update => parse_update_columns(sql),
        _ => Vec::new(),
    }
}

fn parse_select_list(sql: &str) -> Vec<ColumnRef> {
    let Some(caps) = re_select_columns().captures(sql) else {
        return Vec::new();
    };
    let col_list = caps.get(1).map(|m| m.as_str()).unwrap_or("");
    if col_list.trim() == "*" {
        return vec![ColumnRef {
            table_alias: None,
            column_name: "*".into(),
            alias: None,
        }];
    }

    parse_column_list(col_list)
}

fn parse_column_list(col_list: &str) -> Vec<ColumnRef> {
    let mut cols = Vec::new();
    let mut depth = 0;
    let mut current = String::new();

    for c in col_list.chars() {
        match c {
            '(' => {
                depth += 1;
                current.push(c);
            }
            ')' => {
                depth -= 1;
                current.push(c);
            }
            ',' if depth == 0 => {
                if let Some(col) = parse_single_column(current.trim()) {
                    cols.push(col);
                }
                current.clear();
            }
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        if let Some(col) = parse_single_column(current.trim()) {
            cols.push(col);
        }
    }
    cols
}

fn parse_single_column(s: &str) -> Option<ColumnRef> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    // Handle "expr AS alias" or "expr alias"
    let (expr, alias) = if let Some(pos) = s.to_uppercase().rfind(" AS ") {
        (&s[..pos], Some(s[pos + 4..].trim().to_string()))
    } else {
        // Check for trailing word after space that isn't a keyword
        let parts: Vec<&str> = s.split_whitespace().collect();
        if parts.len() == 2 && !is_sql_keyword(parts[1]) {
            (parts[0], Some(parts[1].to_string()))
        } else {
            (s, None)
        }
    };

    let expr = expr.trim();

    // Handle "table.column" or "alias.column"
    if let Some(dot_pos) = expr.rfind('.') {
        let table_alias = expr[..dot_pos].trim();
        let column = expr[dot_pos + 1..].trim();
        Some(ColumnRef {
            table_alias: Some(strip_brackets(table_alias).to_string()),
            column_name: strip_brackets(column).to_string(),
            alias,
        })
    } else {
        Some(ColumnRef {
            table_alias: None,
            column_name: strip_brackets(expr).to_string(),
            alias,
        })
    }
}

fn parse_insert_columns(sql: &str) -> Vec<ColumnRef> {
    let Some(caps) = re_insert_columns().captures(sql) else {
        return Vec::new();
    };
    let col_list = caps.get(1).map(|m| m.as_str()).unwrap_or("");
    col_list
        .split(',')
        .filter_map(|c| {
            let c = c.trim();
            if c.is_empty() {
                None
            } else {
                Some(ColumnRef {
                    table_alias: None,
                    column_name: strip_brackets(c).to_string(),
                    alias: None,
                })
            }
        })
        .collect()
}

fn parse_update_columns(sql: &str) -> Vec<ColumnRef> {
    let Some(caps) = re_update_set().captures(sql) else {
        return Vec::new();
    };
    let set_clause = caps.get(1).map(|m| m.as_str()).unwrap_or("");
    set_clause
        .split(',')
        .filter_map(|assignment| {
            let parts: Vec<&str> = assignment.splitn(2, '=').collect();
            if parts.is_empty() {
                return None;
            }
            let col = parts[0].trim();
            if col.is_empty() {
                return None;
            }
            if let Some(dot_pos) = col.rfind('.') {
                Some(ColumnRef {
                    table_alias: Some(strip_brackets(&col[..dot_pos]).to_string()),
                    column_name: strip_brackets(&col[dot_pos + 1..]).to_string(),
                    alias: None,
                })
            } else {
                Some(ColumnRef {
                    table_alias: None,
                    column_name: strip_brackets(col).to_string(),
                    alias: None,
                })
            }
        })
        .collect()
}

fn detect_parameters(sql: &str) -> Vec<SqlParam> {
    let mut seen = std::collections::HashSet::new();
    let mut params = Vec::new();
    for cap in re_param().captures_iter(sql) {
        let name = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        if !name.is_empty() && seen.insert(name.to_string()) {
            params.push(SqlParam {
                name: name.to_string(),
                inferred_type: "object".into(),
            });
        }
    }
    params
}

fn detect_where_bindings(sql: &str) -> Vec<ParamColumnBinding> {
    // Extract WHERE clause
    let upper = sql.to_uppercase();
    let where_start = match upper.find(" WHERE ") {
        Some(pos) => pos,
        None => return Vec::new(),
    };
    // Take from WHERE to end (or next major clause)
    let where_clause = &sql[where_start..];

    re_where_binding()
        .captures_iter(where_clause)
        .map(|cap| {
            let _table_alias = cap.get(1).map(|m| m.as_str().trim_end_matches('.'));
            let column = cap
                .get(2)
                .map(|m| strip_brackets(m.as_str()).to_string())
                .unwrap_or_default();
            let operator = cap
                .get(3)
                .map(|m| m.as_str().to_uppercase())
                .unwrap_or_default();
            let param = cap
                .get(4)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            ParamColumnBinding {
                param_name: param,
                column,
                operator,
            }
        })
        .collect()
}

fn detect_group_by(sql: &str) -> Vec<String> {
    let Some(caps) = re_group_by().captures(sql) else {
        return Vec::new();
    };
    let group_list = caps.get(1).map(|m| m.as_str()).unwrap_or("");
    group_list
        .split(',')
        .map(|c| strip_brackets(c.trim()).to_string())
        .filter(|c| !c.is_empty())
        .collect()
}

fn detect_subquery(sql: &str) -> bool {
    let upper = sql.to_uppercase();
    // Subquery = SELECT inside parentheses (not the leading SELECT)
    let trimmed = upper.trim_start();
    if let Some(rest) = trimmed.strip_prefix("SELECT") {
        rest.contains("(SELECT") || rest.contains("( SELECT")
    } else if trimmed.starts_with("WITH") {
        // CTE body may have nested selects
        let after_cte = trimmed.find(")").map(|p| &trimmed[p..]).unwrap_or("");
        after_cte.contains("(SELECT") || after_cte.contains("( SELECT")
    } else {
        false
    }
}

fn detect_cte(sql: &str) -> bool {
    sql.trim_start().to_uppercase().starts_with("WITH ")
        && sql.to_uppercase().contains(" AS ")
        && sql.to_uppercase().contains("SELECT")
}

fn detect_multi_statement(sql: &str) -> bool {
    // Look for semicolons followed by another statement keyword
    let parts: Vec<&str> = sql.split(';').collect();
    if parts.len() < 2 {
        return false;
    }
    parts.iter().skip(1).any(|p| {
        let t = p.trim().to_uppercase();
        t.starts_with("SELECT")
            || t.starts_with("INSERT")
            || t.starts_with("UPDATE")
            || t.starts_with("DELETE")
            || t.starts_with("EXEC")
    })
}

fn infer_param_types(mut params: Vec<SqlParam>, bindings: &[ParamColumnBinding]) -> Vec<SqlParam> {
    for param in &mut params {
        if let Some(binding) = bindings.iter().find(|b| b.param_name == param.name) {
            param.inferred_type = infer_csharp_type_extended(&binding.column);
        }
    }
    params
}

/// Extended C# type inference from column/property name.
pub fn infer_csharp_type_extended(name: &str) -> String {
    let lower = name.to_lowercase();

    // Boolean
    if lower.starts_with("is")
        || lower.starts_with("has")
        || lower.starts_with("can")
        || lower.starts_with("should")
        || lower.ends_with("flag")
        || lower.ends_with("active")
        || lower.ends_with("enabled")
    {
        return "bool".into();
    }

    // GUID/UUID
    if lower.ends_with("guid") || lower.ends_with("uuid") {
        return "Guid".into();
    }

    // Integer IDs and counts
    if lower.ends_with("id") || lower == "id" {
        return "int".into();
    }
    if lower.ends_with("count")
        || lower.ends_with("qty")
        || lower.ends_with("quantity")
        || lower.ends_with("number")
    {
        return "int".into();
    }

    // Date/time
    if lower.ends_with("date")
        || lower.ends_with("time")
        || lower.ends_with("at")
        || lower.ends_with("on")
        || lower.starts_with("created")
        || lower.starts_with("modified")
        || lower.starts_with("updated")
        || lower.starts_with("deleted")
    {
        return "DateTime".into();
    }

    // Money/decimal
    if lower.ends_with("amount")
        || lower.ends_with("total")
        || lower.ends_with("price")
        || lower.ends_with("cost")
        || lower.ends_with("balance")
    {
        return "decimal".into();
    }

    // Double/float
    if lower.ends_with("percent") || lower.ends_with("rate") || lower.ends_with("ratio") {
        return "double".into();
    }

    // String types
    if lower.ends_with("name")
        || lower.ends_with("title")
        || lower.ends_with("description")
        || lower.ends_with("text")
        || lower.ends_with("email")
        || lower.ends_with("address")
        || lower.ends_with("url")
        || lower.ends_with("uri")
        || lower.ends_with("link")
        || lower.ends_with("phone")
        || lower.ends_with("status")
        || lower.ends_with("type")
        || lower.ends_with("code")
        || lower.ends_with("path")
    {
        return "string".into();
    }

    // Default
    "string".into()
}

fn generate_by_clause(bindings: &[ParamColumnBinding], params: &[SqlParam]) -> String {
    if !bindings.is_empty() {
        bindings
            .iter()
            .take(3) // limit for readability
            .map(|b| to_pascal(strip_brackets(&b.column)))
            .collect::<Vec<_>>()
            .join("And")
    } else {
        params
            .iter()
            .take(3)
            .map(|p| to_pascal(&p.name))
            .collect::<Vec<_>>()
            .join("And")
    }
}

fn generate_aggregate_method_name(analysis: &SqlAnalysis, entity: &str) -> String {
    // Try to find the aggregation function from selected columns
    let agg_col = analysis.selected_columns.iter().find(|c| {
        let lower = c.column_name.to_lowercase();
        lower.starts_with("count(")
            || lower.starts_with("sum(")
            || lower.starts_with("avg(")
            || lower.starts_with("max(")
            || lower.starts_with("min(")
    });

    if let Some(col) = agg_col {
        let alias = col
            .alias
            .as_deref()
            .map(|a| to_pascal(a))
            .unwrap_or_else(|| format!("{entity}Count"));
        format!("Get{alias}Async")
    } else if !analysis.group_by_columns.is_empty() {
        let group_col = to_pascal(strip_brackets(&analysis.group_by_columns[0]));
        format!("Get{entity}CountBy{group_col}Async")
    } else {
        format!("Get{entity}CountAsync")
    }
}

fn generate_param_list(analysis: &SqlAnalysis) -> String {
    if analysis.operation == SqlOp::Insert {
        let entity = analysis
            .primary_table
            .as_deref()
            .map(|t| to_pascal(strip_brackets(t)))
            .unwrap_or_else(|| "Entity".into());
        return format!("{entity} entity");
    }

    if analysis.operation == SqlOp::Update {
        let entity = analysis
            .primary_table
            .as_deref()
            .map(|t| to_pascal(strip_brackets(t)))
            .unwrap_or_else(|| "Entity".into());
        if analysis.parameters.is_empty() {
            return format!("{entity} entity");
        }
        // Parameters for WHERE + entity for SET
        let where_params: Vec<String> = analysis
            .parameters
            .iter()
            .map(|p| format!("{} {}", p.inferred_type, to_camel(&p.name)))
            .collect();
        let mut parts = where_params;
        parts.push(format!("{entity} entity"));
        return parts.join(", ");
    }

    analysis
        .parameters
        .iter()
        .map(|p| format!("{} {}", p.inferred_type, to_camel(&p.name)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn is_sql_keyword(s: &str) -> bool {
    matches!(
        s.to_uppercase().as_str(),
        "SELECT"
            | "FROM"
            | "WHERE"
            | "AND"
            | "OR"
            | "ON"
            | "AS"
            | "JOIN"
            | "LEFT"
            | "RIGHT"
            | "INNER"
            | "OUTER"
            | "CROSS"
            | "FULL"
            | "GROUP"
            | "ORDER"
            | "BY"
            | "HAVING"
            | "UNION"
            | "INTO"
            | "SET"
            | "VALUES"
            | "INSERT"
            | "UPDATE"
            | "DELETE"
            | "DISTINCT"
            | "TOP"
            | "ASC"
            | "DESC"
            | "NULL"
            | "NOT"
            | "IN"
            | "LIKE"
            | "BETWEEN"
            | "EXISTS"
            | "IS"
    )
}

fn strip_brackets(s: &str) -> &str {
    s.trim_start_matches('[').trim_end_matches(']')
}

fn to_pascal(s: &str) -> String {
    let s = strip_brackets(s);
    let mut result = String::with_capacity(s.len());
    let mut cap_next = true;
    for c in s.chars() {
        if c == '_' || c == ' ' || c == '-' {
            cap_next = true;
            continue;
        }
        if cap_next {
            result.extend(c.to_uppercase());
            cap_next = false;
        } else {
            result.push(c);
        }
    }
    result
}

fn to_camel(s: &str) -> String {
    let pascal = to_pascal(s);
    let mut chars = pascal.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_lowercase().to_string() + chars.as_str(),
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_select() {
        let sql = "SELECT OrderId, Total FROM Orders";
        let a = analyze_sql(sql);
        assert_eq!(a.operation, SqlOp::Select);
        assert_eq!(a.primary_table.as_deref(), Some("Orders"));
        assert!(a.joined_tables.is_empty());
        assert_eq!(a.selected_columns.len(), 2);
        assert_eq!(a.selected_columns[0].column_name, "OrderId");
        assert_eq!(a.selected_columns[1].column_name, "Total");
    }

    #[test]
    fn parse_select_with_join() {
        let sql = "SELECT o.OrderId, o.Total, c.Name FROM Orders o JOIN Customers c ON o.CustomerId = c.Id WHERE o.Status = @status";
        let a = analyze_sql(sql);
        assert_eq!(a.operation, SqlOp::Select);
        assert_eq!(a.primary_table.as_deref(), Some("Orders"));
        assert_eq!(a.joined_tables.len(), 1);
        assert_eq!(a.joined_tables[0].table, "Customers");
        assert_eq!(a.joined_tables[0].alias.as_deref(), Some("c"));
        assert_eq!(a.selected_columns.len(), 3);
        assert_eq!(a.parameters.len(), 1);
        assert_eq!(a.parameters[0].name, "status");
        assert_eq!(a.where_bindings.len(), 1);
        assert_eq!(a.where_bindings[0].param_name, "status");
        assert_eq!(a.where_bindings[0].column, "Status");
    }

    #[test]
    fn parse_select_with_where_params() {
        let sql = "SELECT * FROM Orders WHERE Status = @status AND Date > @since AND CustomerId = @custId";
        let a = analyze_sql(sql);
        assert_eq!(a.parameters.len(), 3);
        assert_eq!(a.where_bindings.len(), 3);
        assert_eq!(a.where_bindings[0].column, "Status");
        assert_eq!(a.where_bindings[1].column, "Date");
        assert_eq!(a.where_bindings[2].column, "CustomerId");
    }

    #[test]
    fn parse_insert_with_columns() {
        let sql =
            "INSERT INTO Orders (CustomerId, OrderDate, Total) VALUES (@custId, @date, @total)";
        let a = analyze_sql(sql);
        assert_eq!(a.operation, SqlOp::Insert);
        assert_eq!(a.primary_table.as_deref(), Some("Orders"));
        assert_eq!(a.selected_columns.len(), 3);
        assert_eq!(a.parameters.len(), 3);
    }

    #[test]
    fn parse_update_with_set_and_where() {
        let sql = "UPDATE Orders SET Status = @status, Total = @total WHERE OrderId = @id";
        let a = analyze_sql(sql);
        assert_eq!(a.operation, SqlOp::Update);
        assert_eq!(a.primary_table.as_deref(), Some("Orders"));
        assert_eq!(a.selected_columns.len(), 2); // SET columns
        assert_eq!(a.parameters.len(), 3);
        assert_eq!(a.where_bindings.len(), 1);
        assert_eq!(a.where_bindings[0].column, "OrderId");
    }

    #[test]
    fn parse_delete_with_where() {
        let sql = "DELETE FROM Orders WHERE OrderId = @id";
        let a = analyze_sql(sql);
        assert_eq!(a.operation, SqlOp::Delete);
        assert_eq!(a.primary_table.as_deref(), Some("Orders"));
        assert_eq!(a.parameters.len(), 1);
        assert_eq!(a.where_bindings.len(), 1);
    }

    #[test]
    fn parse_exec_with_params() {
        let sql = "EXEC sp_GetOrders @CustomerId = @id, @Status = @status";
        let a = analyze_sql(sql);
        assert_eq!(a.operation, SqlOp::Exec);
        assert_eq!(a.primary_table.as_deref(), Some("sp_GetOrders"));
        assert!(a.parameters.len() >= 2);
    }

    #[test]
    fn flag_subquery() {
        let sql =
            "SELECT * FROM Orders WHERE CustomerId IN (SELECT Id FROM Customers WHERE Active = 1)";
        let a = analyze_sql(sql);
        assert!(a.has_subquery);
    }

    #[test]
    fn flag_cte() {
        let sql = "WITH ActiveOrders AS (SELECT * FROM Orders WHERE Active = 1) SELECT * FROM ActiveOrders";
        let a = analyze_sql(sql);
        assert!(a.has_cte);
    }

    #[test]
    fn flag_multi_statement() {
        let sql =
            "UPDATE Orders SET Status = 'Closed'; SELECT * FROM Orders WHERE Status = 'Closed'";
        let a = analyze_sql(sql);
        assert!(a.is_multi_statement);
    }

    #[test]
    fn handle_messy_formatting() {
        let sql = "SELECT\n  o.OrderId,\n  o.Total\nFROM\n  Orders o\nWHERE\n  o.Status = @status";
        let a = analyze_sql(sql);
        assert_eq!(a.operation, SqlOp::Select);
        assert_eq!(a.primary_table.as_deref(), Some("Orders"));
        assert_eq!(a.selected_columns.len(), 2);
        assert_eq!(a.parameters.len(), 1);
    }

    #[test]
    fn generate_method_name_two_param_where() {
        let sql = "SELECT * FROM Orders WHERE Status = @status AND Date > @since";
        let a = analyze_sql(sql);
        let name = generate_method_name(&a);
        assert_eq!(name, "GetOrdersByStatusAndDateAsync");
    }

    #[test]
    fn generate_composite_dto_from_join() {
        let sql = "SELECT o.OrderId, o.Total, c.Name AS CustomerName FROM Orders o JOIN Customers c ON o.CustomerId = c.Id";
        let a = analyze_sql(sql);
        let dto = generate_composite_dto(&a);
        assert!(dto.is_some());
        let dto_code = dto.expect("dto");
        assert!(
            dto_code.contains("OrdersWithCustomers"),
            "DTO code: {dto_code}"
        );
        assert!(dto_code.contains("OrderId"));
        assert!(dto_code.contains("CustomerName"));
    }

    #[test]
    fn infer_param_types_from_column_heuristics() {
        let sql = "SELECT * FROM Orders WHERE Status = @status AND OrderDate > @since AND Total < @maxAmount";
        let a = analyze_sql(sql);
        let status_param = a.parameters.iter().find(|p| p.name == "status");
        assert_eq!(
            status_param.map(|p| p.inferred_type.as_str()),
            Some("string")
        );
        let since_param = a.parameters.iter().find(|p| p.name == "since");
        assert_eq!(
            since_param.map(|p| p.inferred_type.as_str()),
            Some("DateTime")
        );
        let amount_param = a.parameters.iter().find(|p| p.name == "maxAmount");
        assert_eq!(
            amount_param.map(|p| p.inferred_type.as_str()),
            Some("decimal")
        );
    }

    #[test]
    fn generate_method_no_where_returns_getall() {
        let sql = "SELECT * FROM Products";
        let a = analyze_sql(sql);
        let name = generate_method_name(&a);
        assert_eq!(name, "GetAllProductsAsync");
    }

    #[test]
    fn handle_aliased_columns() {
        let sql = "SELECT o.Total AS OrderTotal, o.OrderId FROM Orders o";
        let a = analyze_sql(sql);
        assert_eq!(a.selected_columns.len(), 2);
        let total_col = a.selected_columns.iter().find(|c| c.column_name == "Total");
        assert!(total_col.is_some());
        assert_eq!(
            total_col.and_then(|c| c.alias.as_deref()),
            Some("OrderTotal")
        );
    }

    #[test]
    fn handle_aggregation_with_group_by() {
        let sql = "SELECT Status, COUNT(*) AS OrderCount FROM Orders GROUP BY Status";
        let a = analyze_sql(sql);
        assert!(a.has_aggregation);
        assert_eq!(a.group_by_columns, vec!["Status"]);
        let name = generate_method_name(&a);
        assert!(name.contains("Count") || name.contains("Async"));
    }

    #[test]
    fn generate_exec_method_name() {
        let sql = "EXEC sp_GetOrders @CustomerId = @id";
        let a = analyze_sql(sql);
        let name = generate_method_name(&a);
        assert_eq!(name, "ExecuteSpGetOrdersAsync");
    }

    #[test]
    fn vb_string_concat_normalized() {
        let sql = r#""SELECT * " & "FROM Orders " & "WHERE Status = @status""#;
        let a = analyze_sql(sql);
        assert_eq!(a.operation, SqlOp::Select);
        assert_eq!(a.primary_table.as_deref(), Some("Orders"));
    }

    #[test]
    fn method_signature_includes_params() {
        let sql = "SELECT o.OrderId, c.Name FROM Orders o JOIN Customers c ON o.CustomerId = c.Id WHERE o.Status = @status";
        let a = analyze_sql(sql);
        let sig = generate_method_signature(&a);
        assert!(
            sig.contains("Task<IEnumerable<OrdersWithCustomers>>"),
            "sig: {sig}"
        );
        assert!(sig.contains("GetOrdersByStatusAsync"));
        assert!(sig.contains("string status"));
    }

    #[test]
    fn insert_method_signature() {
        let sql = "INSERT INTO Orders (CustomerId, Total) VALUES (@custId, @total)";
        let a = analyze_sql(sql);
        let sig = generate_method_signature(&a);
        assert!(sig.contains("Task<int>"));
        assert!(sig.contains("CreateOrdersAsync"));
        assert!(sig.contains("Orders entity"));
    }

    #[test]
    fn delete_method_name_with_param() {
        let sql = "DELETE FROM Orders WHERE OrderId = @id";
        let a = analyze_sql(sql);
        let name = generate_method_name(&a);
        assert_eq!(name, "DeleteOrdersByOrderIdAsync");
    }

    #[test]
    fn extended_type_inference() {
        assert_eq!(infer_csharp_type_extended("IsActive"), "bool");
        assert_eq!(infer_csharp_type_extended("HasPermission"), "bool");
        assert_eq!(infer_csharp_type_extended("OrderGuid"), "Guid");
        assert_eq!(infer_csharp_type_extended("TotalAmount"), "decimal");
        assert_eq!(infer_csharp_type_extended("CreatedAt"), "DateTime");
        assert_eq!(infer_csharp_type_extended("SuccessRate"), "double");
        assert_eq!(infer_csharp_type_extended("CustomerName"), "string");
        assert_eq!(infer_csharp_type_extended("ItemCount"), "int");
        assert_eq!(infer_csharp_type_extended("UserId"), "int");
    }

    #[test]
    fn no_false_positive_on_single_semicolon() {
        let sql = "SELECT * FROM Orders;";
        let a = analyze_sql(sql);
        assert!(!a.is_multi_statement);
    }

    #[test]
    fn left_join_detected() {
        let sql = "SELECT o.*, c.Name FROM Orders o LEFT JOIN Customers c ON o.CustomerId = c.Id";
        let a = analyze_sql(sql);
        assert_eq!(a.joined_tables.len(), 1);
        assert!(a.joined_tables[0].join_type.contains("LEFT"));
    }
}
