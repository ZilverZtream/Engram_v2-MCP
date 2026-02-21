//! Database migration strategy advisor — classifies data access patterns, generates
//! repository interfaces, and scores SQL injection risks.
//!
//! Analyzes `SqlCalls`, `QueriesTable`, `ReadsColumn`, `ParameterBinding` edges
//! to classify each file's data access pattern and produce migration recommendations.

use engram_graph::{Edge, EdgeKind, GraphStore};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write;
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
    let pb_edges =
        graph.list_edges_by_kind(project_id, EdgeKind::ParameterBinding, 50_000)?;
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
        let sql = file_sql.get(&file_path).map(|v| v.as_slice()).unwrap_or(&[]);
        let qt = file_qt.get(&file_path).map(|v| v.as_slice()).unwrap_or(&[]);
        let rc = file_rc.get(&file_path).map(|v| v.as_slice()).unwrap_or(&[]);
        let pb = file_pb.get(&file_path).map(|v| v.as_slice()).unwrap_or(&[]);
        let db = file_db.get(&file_path).map(|v| v.as_slice()).unwrap_or(&[]);

        let (primary, secondaries) = classify_file(sql, qt, rc, pb, db);
        let injection_risks = score_sql_injection(sql, &file_path);
        let has_param = injection_risks
            .iter()
            .any(|r| r.risk_type == SqlRiskType::Parameterized);
        let has_concat = injection_risks
            .iter()
            .any(|r| r.risk_type == SqlRiskType::Concatenated || r.risk_type == SqlRiskType::Interpolated);

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

    let mut code = String::with_capacity(2048);
    let _ = writeln!(code, "using System.Threading.Tasks;");
    let _ = writeln!(code, "using System.Collections.Generic;");
    let _ = writeln!(code);

    // Collect table → operations
    let mut table_ops: BTreeMap<String, HashSet<String>> = BTreeMap::new();
    for e in &file_qt {
        let table = e
            .target_id
            .strip_prefix("table:")
            .unwrap_or(&e.target_id)
            .to_string();
        table_ops.entry(table).or_default().insert("select".into());
    }

    // Determine operation types from SQL content
    for e in &file_sql {
        let sql_text = extract_sql_text(e).to_uppercase();
        let tables: Vec<String> = table_ops.keys().cloned().collect();
        for table in &tables {
            if sql_text.contains(&table.to_uppercase()) {
                let ops = table_ops.entry(table.clone()).or_default();
                if sql_text.contains("INSERT") {
                    ops.insert("insert".into());
                }
                if sql_text.contains("UPDATE") {
                    ops.insert("update".into());
                }
                if sql_text.contains("DELETE") {
                    ops.insert("delete".into());
                }
            }
        }
    }

    // Collect columns per table
    let mut table_columns: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for e in &file_rc {
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
        table_columns.entry(table).or_default().push(col);
    }

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

    // Generate DTOs
    for (table, columns) in &table_columns {
        let class_name = to_pascal_case(table);
        let _ = writeln!(code, "public class {class_name}");
        let _ = writeln!(code, "{{");
        for col in columns {
            let prop_name = to_pascal_case(col);
            let prop_type = infer_csharp_type(col);
            let _ = writeln!(
                code,
                "    public {prop_type} {prop_name} {{ get; set; }}"
            );
        }
        let _ = writeln!(code, "}}");
        let _ = writeln!(code);
    }

    // Generate interface
    let _ = writeln!(code, "public interface I{page_name}Repository");
    let _ = writeln!(code, "{{");

    for (table, ops) in &table_ops {
        let entity = to_pascal_case(table);
        if ops.contains("select") {
            let _ = writeln!(
                code,
                "    Task<IEnumerable<{entity}>> GetAll{entity}Async();"
            );
            let _ = writeln!(
                code,
                "    Task<{entity}?> Get{entity}ByIdAsync(int id);"
            );
        }
        if ops.contains("insert") {
            let _ = writeln!(
                code,
                "    Task<int> Create{entity}Async({entity} entity);"
            );
        }
        if ops.contains("update") {
            let _ = writeln!(
                code,
                "    Task Update{entity}Async({entity} entity);"
            );
        }
        if ops.contains("delete") {
            let _ = writeln!(
                code,
                "    Task Delete{entity}Async(int id);"
            );
        }
    }

    let _ = writeln!(code, "}}");
    let _ = writeln!(code);

    // Generate Dapper implementation skeleton
    let _ = writeln!(code, "public class {page_name}Repository : I{page_name}Repository");
    let _ = writeln!(code, "{{");
    let _ = writeln!(
        code,
        "    private readonly string _connectionString;"
    );
    let _ = writeln!(code);
    let _ = writeln!(
        code,
        "    public {page_name}Repository(string connectionString)"
    );
    let _ = writeln!(code, "    {{");
    let _ = writeln!(code, "        _connectionString = connectionString;");
    let _ = writeln!(code, "    }}");

    for (table, ops) in &table_ops {
        let entity = to_pascal_case(table);
        if ops.contains("select") {
            let _ = writeln!(code);
            let _ = writeln!(
                code,
                "    public async Task<IEnumerable<{entity}>> GetAll{entity}Async()"
            );
            let _ = writeln!(code, "    {{");
            let _ = writeln!(code, "        using var conn = new SqlConnection(_connectionString);");
            let _ = writeln!(
                code,
                "        return await conn.QueryAsync<{entity}>(\"SELECT * FROM {table}\");"
            );
            let _ = writeln!(code, "    }}");

            let _ = writeln!(code);
            let _ = writeln!(
                code,
                "    public async Task<{entity}?> Get{entity}ByIdAsync(int id)"
            );
            let _ = writeln!(code, "    {{");
            let _ = writeln!(code, "        using var conn = new SqlConnection(_connectionString);");
            let _ = writeln!(
                code,
                "        return await conn.QuerySingleOrDefaultAsync<{entity}>(\"SELECT * FROM {table} WHERE Id = @Id\", new {{ Id = id }});"
            );
            let _ = writeln!(code, "    }}");
        }
        if ops.contains("insert") {
            let _ = writeln!(code);
            let _ = writeln!(
                code,
                "    public async Task<int> Create{entity}Async({entity} entity)"
            );
            let _ = writeln!(code, "    {{");
            let _ = writeln!(
                code,
                "        // TODO: generate INSERT columns from DTO properties"
            );
            let _ = writeln!(code, "        throw new NotImplementedException();");
            let _ = writeln!(code, "    }}");
        }
        if ops.contains("update") {
            let _ = writeln!(code);
            let _ = writeln!(
                code,
                "    public async Task Update{entity}Async({entity} entity)"
            );
            let _ = writeln!(code, "    {{");
            let _ = writeln!(
                code,
                "        // TODO: generate UPDATE columns from DTO properties"
            );
            let _ = writeln!(code, "        throw new NotImplementedException();");
            let _ = writeln!(code, "    }}");
        }
        if ops.contains("delete") {
            let _ = writeln!(code);
            let _ = writeln!(
                code,
                "    public async Task Delete{entity}Async(int id)"
            );
            let _ = writeln!(code, "    {{");
            let _ = writeln!(code, "        using var conn = new SqlConnection(_connectionString);");
            let _ = writeln!(
                code,
                "        await conn.ExecuteAsync(\"DELETE FROM {table} WHERE Id = @Id\", new {{ Id = id }});"
            );
            let _ = writeln!(code, "    }}");
        }
    }

    let _ = writeln!(code, "}}");
    Ok(code)
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
    let has_sqldatasource = db_edges.iter().any(|e| {
        e.source_id.contains("SqlDataSource")
            || e.target_id.contains("SqlDataSource")
    });
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
        meta.contains("DbContext")
            || meta.contains("ObjectContext")
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
        } else if meta.contains("StoredProcedure")
            || meta.to_uppercase().contains("EXEC")
        {
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
        assert!(DataAccessPattern::InlineSql
            .migration_target()
            .contains("Dapper"));
        assert!(DataAccessPattern::StoredProcedure
            .migration_target()
            .contains("repository"));
        assert!(DataAccessPattern::DatasetAdapter
            .migration_target()
            .contains("EF Core"));
        assert!(DataAccessPattern::EntityFrameworkV1
            .migration_target()
            .contains("EF Core"));
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
        let e = make_edge("fn:Load", "sql:q", Some(r#"{"file_path": "Orders.aspx.cs"}"#));
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
