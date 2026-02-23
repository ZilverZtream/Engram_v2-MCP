//! Database migration strategy advisor — classifies data access patterns, generates
//! repository interfaces, and scores SQL injection risks.
//!
//! Analyzes `SqlCalls`, `QueriesTable`, `ReadsColumn`, `ParameterBinding` edges
//! to classify each file's data access pattern and produce migration recommendations.

use engram_graph::{Edge, EdgeKind, GraphStore};
use engram_index::sql_parser::{SqlOp, analyze_sql};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

// ─── Data access pattern classification ───────────────────────────────────────

/// Classified data access pattern for a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DataAccessPattern {
    /// Raw SQL strings embedded in code (SqlCommand with literal).
    InlineSql,
    /// CommandType.StoredProcedure or EXEC/EXECUTE usage.
    StoredProcedure,
    /// SqlDataAdapter + DataSet/DataTable usage.
    DatasetAdapter,
    /// ExecuteReader + manual column indexing.
    DataReaderManual,
    /// <asp:SqlDataSource> in markup.
    SqlDatasourceDeclarative,
    /// ObjectContext or DbContext (old EF).
    EntityFrameworkV1,
    /// DataContext (LINQ to SQL).
    LinqToSql,
    /// .xsd files or TableAdapter usage.
    TypedDataset,
    /// No data access detected.
    None,
}

impl DataAccessPattern {
    /// Recommended migration target for this pattern.
    pub fn migration_target(&self) -> &'static str {
        match self {
            Self::InlineSql => "Repository + parameterized queries (Dapper)",
            Self::StoredProcedure => "Keep SP, wrap in repository interface",
            Self::DatasetAdapter => "EF Core or Dapper with POCOs",
            Self::DataReaderManual => "Dapper Query<T>()",
            Self::SqlDatasourceDeclarative => "Repository + DI injection",
            Self::EntityFrameworkV1 => "Upgrade to EF Core 8+",
            Self::LinqToSql => "Replace with EF Core",
            Self::TypedDataset => "Dapper + POCOs",
            Self::None => "No data access detected",
        }
    }
}

/// Classification result for a single file.
#[derive(Debug, Clone, Serialize)]
pub struct FileDataAccessProfile {
    pub file_path: String,
    pub primary_pattern: DataAccessPattern,
    pub secondary_patterns: Vec<DataAccessPattern>,
    pub migration_target: String,
    pub table_count: usize,
    pub sql_call_count: usize,
    pub column_count: usize,
    pub has_parameterized_queries: bool,
    pub has_concatenated_sql: bool,
    pub sql_injection_risks: Vec<SqlInjectionRisk>,
}

/// SQL injection risk for a specific location.
#[derive(Debug, Clone, Serialize)]
pub struct SqlInjectionRisk {
    pub file_path: String,
    pub source_id: String,
    pub risk_type: SqlRiskType,
    pub severity: &'static str,
    pub sql_snippet: String,
    pub remediation: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SqlRiskType {
    /// @param or ? placeholders.
    Parameterized,
    /// String concatenation: "SELECT * FROM X WHERE id=" + userId
    Concatenated,
    /// String interpolation: $"SELECT ... {id}"
    Interpolated,
    /// Stored procedure call.
    StoredProc,
}

/// Full database strategy report for a project.
#[derive(Debug, Clone, Serialize)]
pub struct DatabaseStrategyReport {
    pub project_id: String,
    pub file_profiles: Vec<FileDataAccessProfile>,
    pub repository_code: Option<String>,
    pub dto_code: Option<String>,
    pub summary: DatabaseStrategySummary,
}

#[derive(Debug, Clone, Serialize)]
pub struct DatabaseStrategySummary {
    pub total_files_with_data_access: usize,
    pub pattern_distribution: BTreeMap<String, usize>,
    pub total_sql_injection_risks: usize,
    pub critical_risk_count: usize,
    pub tables_referenced: Vec<String>,
}

// ─── Public API ───────────────────────────────────────────────────────────────

/// Classify data access patterns for all files in a project.
pub fn classify_data_access_patterns(
    graph: &Arc<GraphStore>,
    project_id: &str,
) -> anyhow::Result<Vec<FileDataAccessProfile>> {
    let sql_edges = graph.list_edges_by_kind(project_id, EdgeKind::SqlCalls, 50_000)?;
    let qt_edges = graph.list_edges_by_kind(project_id, EdgeKind::QueriesTable, 50_000)?;
    let rc_edges = graph.list_edges_by_kind(project_id, EdgeKind::ReadsColumn, 50_000)?;
    let pb_edges = graph.list_edges_by_kind(project_id, EdgeKind::ParameterBinding, 50_000)?;
    let db_edges = graph.list_edges_by_kind(project_id, EdgeKind::DataBinding, 50_000)?;

    // Group edges by file
    let mut file_sql: HashMap<String, Vec<&Edge>> = HashMap::new();
    let mut file_qt: HashMap<String, Vec<&Edge>> = HashMap::new();
    let mut file_rc: HashMap<String, Vec<&Edge>> = HashMap::new();
    let mut file_pb: HashMap<String, Vec<&Edge>> = HashMap::new();
    let mut file_db: HashMap<String, Vec<&Edge>> = HashMap::new();

    for e in &sql_edges {
        let fp = extract_file_from_edge(e);
        file_sql.entry(fp).or_default().push(e);
    }
    for e in &qt_edges {
        let fp = extract_file_from_edge(e);
        file_qt.entry(fp).or_default().push(e);
    }
    for e in &rc_edges {
        let fp = extract_file_from_edge(e);
        file_rc.entry(fp).or_default().push(e);
    }
    for e in &pb_edges {
        let fp = extract_file_from_edge(e);
        file_pb.entry(fp).or_default().push(e);
    }
    for e in &db_edges {
        let fp = extract_file_from_edge(e);
        file_db.entry(fp).or_default().push(e);
    }

    // All files that have any data access
    let mut all_files: HashSet<String> = HashSet::new();
    all_files.extend(file_sql.keys().cloned());
    all_files.extend(file_qt.keys().cloned());
    all_files.extend(file_rc.keys().cloned());

    let mut profiles = Vec::new();

    for file_path in all_files {
        let sql = file_sql
            .get(&file_path)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let qt = file_qt.get(&file_path).map(|v| v.as_slice()).unwrap_or(&[]);
        let rc = file_rc.get(&file_path).map(|v| v.as_slice()).unwrap_or(&[]);
        let pb = file_pb.get(&file_path).map(|v| v.as_slice()).unwrap_or(&[]);
        let db = file_db.get(&file_path).map(|v| v.as_slice()).unwrap_or(&[]);

        let (primary, secondaries) = classify_file(sql, qt, rc, pb, db);
        let injection_risks = score_sql_injection(sql, &file_path);
        let has_param = injection_risks
            .iter()
            .any(|r| r.risk_type == SqlRiskType::Parameterized);
        let has_concat = injection_risks.iter().any(|r| {
            r.risk_type == SqlRiskType::Concatenated || r.risk_type == SqlRiskType::Interpolated
        });

        // Unique tables
        let mut tables: HashSet<String> = HashSet::new();
        for e in qt {
            tables.insert(
                e.target_id
                    .strip_prefix("table:")
                    .unwrap_or(&e.target_id)
                    .to_string(),
            );
        }

        profiles.push(FileDataAccessProfile {
            file_path: file_path.clone(),
            primary_pattern: primary,
            secondary_patterns: secondaries,
            migration_target: primary.migration_target().to_string(),
            table_count: tables.len(),
            sql_call_count: sql.len(),
            column_count: rc.len(),
            has_parameterized_queries: has_param,
            has_concatenated_sql: has_concat,
            sql_injection_risks: injection_risks,
        });
    }

    profiles.sort_by(|a, b| a.file_path.cmp(&b.file_path));
    Ok(profiles)
}

/// Generate repository interfaces from graph data for a specific file.
pub fn generate_repository_interfaces(
    graph: &Arc<GraphStore>,
    project_id: &str,
    file_path: &str,
) -> anyhow::Result<String> {
    let qt_edges = graph.list_edges_by_kind(project_id, EdgeKind::QueriesTable, 50_000)?;
    let sql_edges = graph.list_edges_by_kind(project_id, EdgeKind::SqlCalls, 50_000)?;
    let rc_edges = graph.list_edges_by_kind(project_id, EdgeKind::ReadsColumn, 50_000)?;

    let file_qt: Vec<&Edge> = qt_edges
        .iter()
        .filter(|e| extract_file_from_edge(e) == file_path)
        .collect();
    let file_sql: Vec<&Edge> = sql_edges
        .iter()
        .filter(|e| extract_file_from_edge(e) == file_path)
        .collect();
    let file_rc: Vec<&Edge> = rc_edges
        .iter()
        .filter(|e| extract_file_from_edge(e) == file_path)
        .collect();

    let table_ops = collect_table_operations(&file_qt, &file_sql);
    let table_columns = collect_table_columns(&file_rc, &file_sql);

    let page_name = file_path
        .replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or(file_path)
        .replace(".aspx.cs", "")
        .replace(".aspx.vb", "")
        .replace(".aspx", "")
        .replace(".cs", "")
        .replace(".vb", "");

    let mut b = CSharpCodeBuilder::new();
    generate_file_header(&mut b);
    generate_dto_classes(&mut b, &table_columns);
    generate_repo_interface(&mut b, &page_name, &table_ops);
    generate_repo_implementation(&mut b, &page_name, &table_ops, &table_columns);

    Ok(b.build())
}

fn collect_table_operations(
    file_qt: &[&Edge],
    file_sql: &[&Edge],
) -> BTreeMap<String, HashSet<String>> {
    let mut table_ops: BTreeMap<String, HashSet<String>> = BTreeMap::new();

    // Default to select for queried tables
    for e in file_qt {
        let table = e
            .target_id
            .strip_prefix("table:")
            .unwrap_or(&e.target_id)
            .to_string();
        table_ops.entry(table).or_default().insert("select".into());
    }

    // Enrich from parsed SQL
    for e in file_sql {
        let sql_text = extract_sql_text(e);
        let analysis = analyze_sql(&sql_text);

        let op_key = match analysis.operation {
            SqlOp::Select => "select",
            SqlOp::Insert => "insert",
            SqlOp::Update => "update",
            SqlOp::Delete => "delete",
            SqlOp::Exec => "exec",
        };

        if let Some(table) = analysis.primary_table.as_deref() {
            let table_clean = strip_brackets(table).to_string();
            table_ops
                .entry(table_clean)
                .or_default()
                .insert(op_key.into());
        }

        for join in &analysis.joined_tables {
            let table_clean = strip_brackets(&join.table).to_string();
            table_ops
                .entry(table_clean)
                .or_default()
                .insert("select".into());
        }
    }
    table_ops
}

fn collect_table_columns(file_rc: &[&Edge], file_sql: &[&Edge]) -> BTreeMap<String, Vec<String>> {
    let mut table_columns: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut seen_columns: HashSet<(String, String)> = HashSet::new();

    // 1. From ReadsColumn edges (graph-based)
    for e in file_rc {
        let col = e
            .target_id
            .strip_prefix("col:")
            .unwrap_or(&e.target_id)
            .to_string();
        let table = e
            .metadata
            .as_ref()
            .and_then(|m| m.get("table"))
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown")
            .to_string();
        if seen_columns.insert((table.clone(), col.clone())) {
            table_columns.entry(table).or_default().push(col);
        }
    }

    // 2. From SQL parsing (SqlAnalysis-based)
    for e in file_sql {
        let sql_text = extract_sql_text(e);
        let analysis = analyze_sql(&sql_text);

        if let Some(table) = analysis.primary_table.as_deref() {
            let table_clean = strip_brackets(table).to_string();
            for col_ref in &analysis.selected_columns {
                if col_ref.column_name != "*" {
                    let col_clean = strip_brackets(&col_ref.column_name).to_string();
                    if seen_columns.insert((table_clean.clone(), col_clean.clone())) {
                        table_columns
                            .entry(table_clean.clone())
                            .or_default()
                            .push(col_clean);
                    }
                }
            }
        }
    }
    table_columns
}

fn generate_file_header(b: &mut CSharpCodeBuilder) {
    b.line("using System;");
    b.line("using System.Collections.Generic;");
    b.line("using System.Data;");
    b.line("using System.Data.SqlClient;");
    b.line("using System.Threading.Tasks;");
    b.line("using Dapper;");
    b.blank();
}

fn generate_dto_classes(b: &mut CSharpCodeBuilder, table_columns: &BTreeMap<String, Vec<String>>) {
    for (table, columns) in table_columns {
        let class_name = to_pascal_case(table);
        b.open_block(&format!("public class {class_name}"));
        for col in columns {
            let prop_name = to_pascal_case(col);
            let prop_type = infer_csharp_type(col);
            b.line(&format!("public {prop_type} {prop_name} {{ get; set; }}"));
        }
        b.close_block();
        b.blank();
    }
}

fn generate_repo_interface(
    b: &mut CSharpCodeBuilder,
    page_name: &str,
    table_ops: &BTreeMap<String, HashSet<String>>,
) {
    b.open_block(&format!("public interface I{page_name}Repository"));
    for (table, ops) in table_ops {
        let entity = to_pascal_case(table);
        if ops.contains("select") {
            b.line(&format!(
                "Task<IEnumerable<{entity}>> GetAll{entity}Async();"
            ));
            b.line(&format!("Task<{entity}?> Get{entity}ByIdAsync(int id);"));
        }
        if ops.contains("insert") {
            b.line(&format!("Task<int> Create{entity}Async({entity} entity);"));
        }
        if ops.contains("update") {
            b.line(&format!("Task Update{entity}Async({entity} entity);"));
        }
        if ops.contains("delete") {
            b.line(&format!("Task Delete{entity}Async(int id);"));
        }
    }
    b.close_block();
    b.blank();
}

fn generate_repo_implementation(
    b: &mut CSharpCodeBuilder,
    page_name: &str,
    table_ops: &BTreeMap<String, HashSet<String>>,
    table_columns: &BTreeMap<String, Vec<String>>,
) {
    b.open_block(&format!(
        "public class {page_name}Repository : I{page_name}Repository"
    ));
    b.line("private readonly string _connectionString;");
    b.blank();
    b.open_block(&format!(
        "public {page_name}Repository(string connectionString)"
    ));
    b.line("_connectionString = connectionString ?? throw new ArgumentNullException(nameof(connectionString));");
    b.close_block();

    for (table, ops) in table_ops {
        let entity = to_pascal_case(table);
        let columns_for_table = table_columns.get(table);

        if ops.contains("select") {
            b.blank();
            b.open_block(&format!(
                "public async Task<IEnumerable<{entity}>> GetAll{entity}Async()"
            ));
            b.line("using var conn = new SqlConnection(_connectionString);");
            b.line(&format!(
                "return await conn.QueryAsync<{entity}>(\"SELECT * FROM [{table}]\");"
            ));
            b.close_block();

            b.blank();
            b.open_block(&format!(
                "public async Task<{entity}?> Get{entity}ByIdAsync(int id)"
            ));
            b.line("using var conn = new SqlConnection(_connectionString);");
            b.line(&format!(
                "return await conn.QuerySingleOrDefaultAsync<{entity}>(\"SELECT * FROM [{table}] WHERE Id = @Id\", new {{ Id = id }});"
            ));
            b.close_block();
        }

        if ops.contains("insert") {
            b.blank();
            b.open_block(&format!(
                "public async Task<int> Create{entity}Async({entity} entity)"
            ));
            b.line("using var conn = new SqlConnection(_connectionString);");
            if let Some(cols) = columns_for_table {
                let insert_cols: Vec<&str> = cols
                    .iter()
                    .filter(|c| !c.to_lowercase().ends_with("id") || cols.len() == 1)
                    .map(|c| c.as_str())
                    .collect();
                if insert_cols.is_empty() {
                    b.line(&format!(
                        "return await conn.ExecuteScalarAsync<int>(\"INSERT INTO [{table}] DEFAULT VALUES; SELECT SCOPE_IDENTITY();\");"
                    ));
                } else {
                    let col_list = insert_cols.join("], [");
                    let param_list = insert_cols
                        .iter()
                        .map(|c| format!("@{}", to_pascal_case(c)))
                        .collect::<Vec<_>>()
                        .join(", ");
                    b.line(&format!(
                        "return await conn.ExecuteScalarAsync<int>(\"INSERT INTO [{table}] ([{col_list}]) VALUES ({param_list}); SELECT SCOPE_IDENTITY();\", entity);"
                    ));
                }
            } else {
                b.line(&format!(
                    "return await conn.ExecuteScalarAsync<int>(\"INSERT INTO [{table}] DEFAULT VALUES; SELECT SCOPE_IDENTITY();\");"
                ));
            }
            b.close_block();
        }

        if ops.contains("update") {
            b.blank();
            b.open_block(&format!(
                "public async Task Update{entity}Async({entity} entity)"
            ));
            b.line("using var conn = new SqlConnection(_connectionString);");
            if let Some(cols) = columns_for_table {
                let update_cols: Vec<&str> = cols
                    .iter()
                    .filter(|c| c.to_lowercase() != "id")
                    .map(|c| c.as_str())
                    .collect();
                if update_cols.is_empty() {
                    b.line("// No non-ID columns detected — review manually");
                    b.line(&format!(
                        "await conn.ExecuteAsync(\"UPDATE [{table}] SET /* columns */ WHERE Id = @Id\", entity);"
                    ));
                } else {
                    let set_clause = update_cols
                        .iter()
                        .map(|c| format!("[{c}] = @{}", to_pascal_case(c)))
                        .collect::<Vec<_>>()
                        .join(", ");
                    b.line(&format!(
                        "await conn.ExecuteAsync(\"UPDATE [{table}] SET {set_clause} WHERE Id = @Id\", entity);"
                    ));
                }
            } else {
                b.line(&format!(
                    "await conn.ExecuteAsync(\"UPDATE [{table}] SET /* columns */ WHERE Id = @Id\", entity);"
                ));
            }
            b.close_block();
        }

        if ops.contains("delete") {
            b.blank();
            b.open_block(&format!("public async Task Delete{entity}Async(int id)"));
            b.line("using var conn = new SqlConnection(_connectionString);");
            b.line(&format!(
                "await conn.ExecuteAsync(\"DELETE FROM [{table}] WHERE Id = @Id\", new {{ Id = id }});"
            ));
            b.close_block();
        }
    }
    b.close_block(); // class
}

/// Score SQL injection risks for all files in a project.
pub fn score_sql_injection_risks(
    graph: &Arc<GraphStore>,
    project_id: &str,
) -> anyhow::Result<Vec<SqlInjectionRisk>> {
    let sql_edges = graph.list_edges_by_kind(project_id, EdgeKind::SqlCalls, 50_000)?;

    let mut file_sql: HashMap<String, Vec<&Edge>> = HashMap::new();
    for e in &sql_edges {
        let fp = extract_file_from_edge(e);
        file_sql.entry(fp).or_default().push(e);
    }

    let mut all_risks = Vec::new();
    for (file_path, edges) in &file_sql {
        all_risks.extend(score_sql_injection(edges.as_slice(), file_path));
    }

    // Sort by severity (Critical first)
    all_risks.sort_by(|a, b| severity_order(a.severity).cmp(&severity_order(b.severity)));
    Ok(all_risks)
}

// ─── Internal helpers ─────────────────────────────────────────────────────────

fn classify_file(
    sql_edges: &[&Edge],
    qt_edges: &[&Edge],
    _rc_edges: &[&Edge],
    pb_edges: &[&Edge],
    db_edges: &[&Edge],
) -> (DataAccessPattern, Vec<DataAccessPattern>) {
    let mut patterns: Vec<DataAccessPattern> = Vec::new();

    // Check for SqlDataSource (declarative)
    let has_sqldatasource = db_edges
        .iter()
        .any(|e| e.source_id.contains("SqlDataSource") || e.target_id.contains("SqlDataSource"));
    if has_sqldatasource {
        patterns.push(DataAccessPattern::SqlDatasourceDeclarative);
    }

    // Check for stored procedures
    let has_stored_proc = sql_edges.iter().any(|e| {
        let meta = edge_meta_str(e);
        meta.contains("StoredProcedure")
            || meta.contains("CommandType.StoredProcedure")
            || meta.to_uppercase().contains("EXEC ")
            || meta.to_uppercase().contains("EXECUTE ")
    });
    if has_stored_proc {
        patterns.push(DataAccessPattern::StoredProcedure);
    }

    // Check for DataAdapter/DataSet/DataTable
    let has_dataset = sql_edges.iter().any(|e| {
        let meta = edge_meta_str(e);
        meta.contains("DataAdapter")
            || meta.contains("DataSet")
            || meta.contains("DataTable")
            || e.source_id.contains("DataAdapter")
    });
    if has_dataset {
        patterns.push(DataAccessPattern::DatasetAdapter);
    }

    // Check for ExecuteReader manual
    let has_reader = sql_edges.iter().any(|e| {
        let meta = edge_meta_str(e);
        meta.contains("ExecuteReader") || meta.contains("DataReader")
    });
    if has_reader {
        patterns.push(DataAccessPattern::DataReaderManual);
    }

    // Check for EF/LINQ
    let has_ef = sql_edges.iter().any(|e| {
        let meta = edge_meta_str(e);
        meta.contains("DbContext") || meta.contains("ObjectContext")
    });
    if has_ef {
        patterns.push(DataAccessPattern::EntityFrameworkV1);
    }

    let has_linq = sql_edges.iter().any(|e| {
        let meta = edge_meta_str(e);
        meta.contains("DataContext") && !meta.contains("DbContext")
    });
    if has_linq {
        patterns.push(DataAccessPattern::LinqToSql);
    }

    // Check for TypedDataset
    let has_typed = sql_edges.iter().any(|e| {
        let meta = edge_meta_str(e);
        meta.contains("TableAdapter") || meta.contains(".xsd")
    });
    if has_typed {
        patterns.push(DataAccessPattern::TypedDataset);
    }

    // Default: inline SQL if we have SQL edges and parameter bindings
    if !sql_edges.is_empty() && patterns.is_empty() {
        if !pb_edges.is_empty() || !qt_edges.is_empty() {
            patterns.push(DataAccessPattern::InlineSql);
        } else {
            patterns.push(DataAccessPattern::InlineSql);
        }
    }

    let primary = patterns.first().copied().unwrap_or(DataAccessPattern::None);
    let secondaries: Vec<DataAccessPattern> = patterns.into_iter().skip(1).collect();
    (primary, secondaries)
}

fn score_sql_injection(sql_edges: &[&Edge], file_path: &str) -> Vec<SqlInjectionRisk> {
    let mut risks = Vec::new();

    for edge in sql_edges {
        let sql_text = extract_sql_text(edge);
        let meta = edge_meta_str(edge);

        let risk_type = if meta.contains("concat") || meta.contains("string_concat") {
            SqlRiskType::Concatenated
        } else if meta.contains("interpolat") || sql_text.contains('{') {
            SqlRiskType::Interpolated
        } else if meta.contains("StoredProcedure") || meta.to_uppercase().contains("EXEC") {
            SqlRiskType::StoredProc
        } else if sql_text.contains('@') || sql_text.contains('?') {
            SqlRiskType::Parameterized
        } else if sql_text.contains('+') || sql_text.contains('&') {
            // Likely concatenation in the SQL string itself
            SqlRiskType::Concatenated
        } else {
            SqlRiskType::Parameterized // default assumption if no indicators
        };

        let (severity, remediation) = match risk_type {
            SqlRiskType::Concatenated => (
                "critical",
                "Replace string concatenation with parameterized queries (@param)",
            ),
            SqlRiskType::Interpolated => (
                "critical",
                "Replace string interpolation with parameterized queries (@param)",
            ),
            SqlRiskType::StoredProc => (
                "low",
                "Stored procedure — generally safe, verify SP doesn't use dynamic SQL",
            ),
            SqlRiskType::Parameterized => ("safe", "Already parameterized — no action needed"),
        };

        let snippet = if sql_text.len() > 100 {
            format!("{}...", &sql_text[..100])
        } else {
            sql_text.to_string()
        };

        risks.push(SqlInjectionRisk {
            file_path: file_path.to_string(),
            source_id: edge.source_id.clone(),
            risk_type,
            severity,
            sql_snippet: snippet,
            remediation,
        });
    }

    risks
}

fn extract_file_from_edge(edge: &Edge) -> String {
    // Try metadata first
    if let Some(fp) = edge
        .metadata
        .as_ref()
        .and_then(|m| m.get("file_path"))
        .and_then(|v| v.as_str())
    {
        return fp.to_string();
    }
    // Fallback: extract from source_id
    let src = &edge.source_id;
    if let Some(rest) = src.strip_prefix("file:") {
        return rest.to_string();
    }
    // Try to find file-like path in source_id
    if src.contains('/') || src.contains('\\') {
        return src.clone();
    }
    src.clone()
}

fn extract_sql_text(edge: &Edge) -> String {
    edge.metadata
        .as_ref()
        .and_then(|m| {
            m.get("sql")
                .or_else(|| m.get("command_text"))
                .or_else(|| m.get("query"))
        })
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn edge_meta_str(edge: &Edge) -> String {
    edge.metadata
        .as_ref()
        .map(|m| m.to_string())
        .unwrap_or_default()
}

fn severity_order(s: &str) -> u8 {
    match s {
        "critical" => 0,
        "high" => 1,
        "medium" => 2,
        "low" => 3,
        "safe" => 4,
        _ => 5,
    }
}

fn strip_brackets(s: &str) -> &str {
    s.trim_start_matches('[').trim_end_matches(']')
}

fn to_pascal_case(s: &str) -> String {
    let mut result = String::new();
    let mut capitalize_next = true;
    for c in s.chars() {
        if c == '_' || c == '-' || c == ' ' {
            capitalize_next = true;
            continue;
        }
        if capitalize_next {
            result.push(c.to_uppercase().next().unwrap_or(c));
            capitalize_next = false;
        } else {
            result.push(c);
        }
    }
    result
}

// ─── C# Code Builder ──────────────────────────────────────────────────────────

/// Structured C# code builder that manages indentation levels and brace matching.
/// Eliminates raw string concatenation for code generation — all output goes through
/// `line()`, `open_block()`, and `close_block()` to guarantee syntactically valid nesting.
struct CSharpCodeBuilder {
    buf: String,
    indent: usize,
}

impl CSharpCodeBuilder {
    fn new() -> Self {
        Self {
            buf: String::with_capacity(4096),
            indent: 0,
        }
    }

    /// Emit a single line at the current indentation level.
    fn line(&mut self, text: &str) {
        for _ in 0..self.indent {
            self.buf.push_str("    ");
        }
        self.buf.push_str(text);
        self.buf.push('\n');
    }

    /// Emit an opening brace block: `text\n{\n` and increase indent.
    fn open_block(&mut self, header: &str) {
        self.line(header);
        self.line("{");
        self.indent += 1;
    }

    /// Emit a closing brace `}\n` and decrease indent.
    fn close_block(&mut self) {
        if self.indent > 0 {
            self.indent -= 1;
        }
        self.line("}");
    }

    /// Emit a blank line.
    fn blank(&mut self) {
        self.buf.push('\n');
    }

    /// Consume the builder and return the generated code.
    fn build(self) -> String {
        self.buf
    }
}

// ─── Type inference ───────────────────────────────────────────────────────────

fn infer_csharp_type(col_name: &str) -> &'static str {
    let lower = col_name.to_lowercase();
    if lower.ends_with("id") {
        "int"
    } else if lower.ends_with("name")
        || lower.ends_with("description")
        || lower.ends_with("title")
        || lower.ends_with("email")
        || lower.ends_with("address")
        || lower.ends_with("text")
        || lower.ends_with("url")
    {
        "string"
    } else if lower.ends_with("date")
        || lower.ends_with("time")
        || lower.starts_with("created")
        || lower.starts_with("modified")
    {
        "DateTime"
    } else if lower.ends_with("amount")
        || lower.ends_with("total")
        || lower.ends_with("price")
        || lower.ends_with("cost")
    {
        "decimal"
    } else if lower.starts_with("is") || lower.starts_with("has") {
        "bool"
    } else if lower.ends_with("count") || lower.ends_with("quantity") {
        "int"
    } else {
        "string"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pattern_migration_targets() {
        assert!(
            DataAccessPattern::InlineSql
                .migration_target()
                .contains("Dapper")
        );
        assert!(
            DataAccessPattern::StoredProcedure
                .migration_target()
                .contains("repository")
        );
        assert!(
            DataAccessPattern::DatasetAdapter
                .migration_target()
                .contains("EF Core")
        );
        assert!(
            DataAccessPattern::EntityFrameworkV1
                .migration_target()
                .contains("EF Core")
        );
    }

    fn make_edge(source: &str, target: &str, meta_json: Option<&str>) -> Edge {
        Edge {
            source_id: source.into(),
            target_id: target.into(),
            namespace: "test".into(),
            language: "csharp".into(),
            edge_kind: EdgeKind::SqlCalls,
            weight: 1,
            generation: 1,
            metadata: meta_json.map(|s| serde_json::from_str(s).unwrap_or_default()),
            updated_at_ms: 0,
        }
    }

    #[test]
    fn classify_inline_sql() {
        let e = make_edge("fn:LoadData", "sql:select_orders", None);
        let edges: Vec<&Edge> = vec![&e];
        let (primary, _) = classify_file(&edges, &[], &[], &[], &[]);
        assert_eq!(primary, DataAccessPattern::InlineSql);
    }

    #[test]
    fn classify_stored_proc() {
        let e = make_edge(
            "fn:LoadData",
            "sql:sp_GetOrders",
            Some(r#"{"sql": "EXEC sp_GetOrders", "CommandType.StoredProcedure": true}"#),
        );
        let edges: Vec<&Edge> = vec![&e];
        let (primary, _) = classify_file(&edges, &[], &[], &[], &[]);
        assert_eq!(primary, DataAccessPattern::StoredProcedure);
    }

    #[test]
    fn classify_dataset_adapter() {
        let e = make_edge(
            "fn:LoadData",
            "sql:query",
            Some(r#"{"sql": "SELECT *", "DataAdapter": true}"#),
        );
        let edges: Vec<&Edge> = vec![&e];
        let (primary, _) = classify_file(&edges, &[], &[], &[], &[]);
        assert_eq!(primary, DataAccessPattern::DatasetAdapter);
    }

    #[test]
    fn score_parameterized_sql() {
        let e = make_edge(
            "fn:Load",
            "sql:q",
            Some(r#"{"sql": "SELECT * FROM Orders WHERE Id = @Id"}"#),
        );
        let edges: Vec<&Edge> = vec![&e];
        let risks = score_sql_injection(&edges, "test.cs");
        assert_eq!(risks.len(), 1);
        assert_eq!(risks[0].risk_type, SqlRiskType::Parameterized);
        assert_eq!(risks[0].severity, "safe");
    }

    #[test]
    fn score_concatenated_sql() {
        let e = make_edge(
            "fn:Load",
            "sql:q",
            Some(r#"{"sql": "SELECT * FROM Orders WHERE Id = ", "string_concat": true}"#),
        );
        let edges: Vec<&Edge> = vec![&e];
        let risks = score_sql_injection(&edges, "test.cs");
        assert_eq!(risks.len(), 1);
        assert_eq!(risks[0].risk_type, SqlRiskType::Concatenated);
        assert_eq!(risks[0].severity, "critical");
    }

    #[test]
    fn score_stored_proc_sql() {
        let e = make_edge(
            "fn:Load",
            "sql:sp",
            Some(r#"{"sql": "EXEC sp_GetOrders @id"}"#),
        );
        let edges: Vec<&Edge> = vec![&e];
        let risks = score_sql_injection(&edges, "test.cs");
        assert_eq!(risks.len(), 1);
        assert_eq!(risks[0].risk_type, SqlRiskType::StoredProc);
        assert_eq!(risks[0].severity, "low");
    }

    #[test]
    fn severity_ordering() {
        assert!(severity_order("critical") < severity_order("safe"));
        assert!(severity_order("high") < severity_order("low"));
    }

    #[test]
    fn infer_types() {
        assert_eq!(infer_csharp_type("OrderId"), "int");
        assert_eq!(infer_csharp_type("CustomerName"), "string");
        assert_eq!(infer_csharp_type("IsActive"), "bool");
        assert_eq!(infer_csharp_type("TotalPrice"), "decimal");
        assert_eq!(infer_csharp_type("CreatedDate"), "DateTime");
    }

    #[test]
    fn pascal_case() {
        assert_eq!(to_pascal_case("orders"), "Orders");
        assert_eq!(to_pascal_case("user_profile"), "UserProfile");
        assert_eq!(to_pascal_case("my-table"), "MyTable");
    }

    #[test]
    fn extract_file_from_edge_metadata() {
        let e = make_edge(
            "fn:Load",
            "sql:q",
            Some(r#"{"file_path": "Orders.aspx.cs"}"#),
        );
        assert_eq!(extract_file_from_edge(&e), "Orders.aspx.cs");
    }

    #[test]
    fn extract_file_from_edge_prefix() {
        let mut e = make_edge("file:Orders.aspx.cs", "sql:q", None);
        e.metadata = None;
        assert_eq!(extract_file_from_edge(&e), "Orders.aspx.cs");
    }

    #[test]
    fn empty_edges_gives_no_pattern() {
        let (primary, secondaries) = classify_file(&[], &[], &[], &[], &[]);
        assert_eq!(primary, DataAccessPattern::None);
        assert!(secondaries.is_empty());
    }
}
