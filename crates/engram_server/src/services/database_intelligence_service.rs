//! Phase 37: Database Intelligence — SP deep analysis, schema ingestion, trigger detection.
//!
//! Extends the stored procedure catalog with:
//! - LLM-powered SP business logic summaries (with deterministic fallback)
//! - SP→SP call chain detection (EXEC/EXECUTE tracking)
//! - Trigger detection and cross-referencing
//! - CREATE TABLE/VIEW schema parsing and cross-referencing

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;

use super::full_project_migration_service::StoredProcedureCatalog;

// ── Structs ──────────────────────────────────────────────────────────────────

/// Complete database intelligence report.
#[derive(Debug, Clone, Serialize, Default)]
pub struct DatabaseIntelligence {
    pub sp_logic: Vec<SpBusinessLogic>,
    pub sp_call_chains: Vec<SpCallChain>,
    pub triggers: Vec<TriggerInfo>,
    pub schema: SchemaReport,
    pub warnings: Vec<String>,
}

/// Business logic summary for a stored procedure.
#[derive(Debug, Clone, Serialize)]
pub struct SpBusinessLogic {
    pub sp_name: String,
    pub purpose: String,
    pub steps: Vec<String>,
    pub data_tables: Vec<String>,
    pub parameters: Vec<String>,
    pub side_effects: Vec<String>,
    pub calls_other_sps: Vec<String>,
    pub content_hash: String,
}

/// A call chain between stored procedures.
#[derive(Debug, Clone, Serialize)]
pub struct SpCallChain {
    pub chain: Vec<String>,
    pub is_cycle: bool,
}

/// Information about a database trigger.
#[derive(Debug, Clone, Serialize)]
pub struct TriggerInfo {
    pub name: String,
    pub target_table: String,
    pub event_types: Vec<String>,
    pub trigger_type: String, // AFTER, INSTEAD OF, FOR
    pub body_summary: String,
}

/// Schema report from parsed CREATE TABLE/VIEW statements.
#[derive(Debug, Clone, Serialize, Default)]
pub struct SchemaReport {
    pub tables: Vec<SchemaTable>,
    pub views: Vec<ViewInfo>,
    pub cross_reference: Vec<SchemaWarning>,
    pub business_rules: Vec<String>,
}

/// Parsed CREATE TABLE definition.
#[derive(Debug, Clone, Serialize)]
pub struct SchemaTable {
    pub name: String,
    pub columns: Vec<SchemaColumn>,
    pub primary_key: Vec<String>,
    pub foreign_keys: Vec<ForeignKey>,
    pub check_constraints: Vec<String>,
    pub indexes: Vec<String>,
}

/// Column definition within a table.
#[derive(Debug, Clone, Serialize)]
pub struct SchemaColumn {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub default_value: Option<String>,
    pub is_computed: bool,
    pub computed_expression: Option<String>,
}

/// Foreign key relationship.
#[derive(Debug, Clone, Serialize)]
pub struct ForeignKey {
    pub column: String,
    pub references_table: String,
    pub references_column: String,
    pub on_delete: Option<String>,
    pub on_update: Option<String>,
}

/// Parsed VIEW definition.
#[derive(Debug, Clone, Serialize)]
pub struct ViewInfo {
    pub name: String,
    pub source_tables: Vec<String>,
}

/// Cross-reference warning between schema and code.
#[derive(Debug, Clone, Serialize)]
pub struct SchemaWarning {
    pub kind: String, // "table_in_code_not_schema", "table_in_schema_not_code", etc.
    pub message: String,
}

// ── Regex Patterns ───────────────────────────────────────────────────────────

static EXEC_SP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)\bEXEC(?:UTE)?\s+(?:\[?dbo\]?\.)?\[?(\w+)\]?").expect("EXEC_SP_RE")
});

static CREATE_TRIGGER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?ims)CREATE\s+TRIGGER\s+(?:\[?dbo\]?\.)?\[?(\w+)\]?\s+ON\s+(?:\[?dbo\]?\.)?\[?(\w+)\]?\s+(AFTER|INSTEAD\s+OF|FOR)\s+(INSERT|UPDATE|DELETE(?:\s*,\s*(?:INSERT|UPDATE|DELETE))*)",
    )
    .expect("CREATE_TRIGGER_RE")
});

static CREATE_TABLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)CREATE\s+TABLE\s+(?:\[?dbo\]?\.)?\[?(\w+)\]?\s*\(").expect("CREATE_TABLE_RE")
});

static CREATE_VIEW_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?ims)CREATE\s+(?:OR\s+ALTER\s+)?VIEW\s+(?:\[?dbo\]?\.)?\[?(\w+)\]?\s+AS\s+(.*?)(?:(?:\r?\n|\r)GO\b|\z)")
        .expect("CREATE_VIEW_RE")
});

static TABLE_REF_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)(?:FROM|JOIN|INTO|UPDATE)\s+\[?(?:dbo\.)?\]?\[?(\w+)\]?")
        .expect("TABLE_REF_RE")
});

static SQL_SELECT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)(?:FROM|JOIN)\s+\[?(?:dbo\.)?\]?\[?(\w+)\]?").expect("SQL_SELECT_RE")
});

static SQL_WRITE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)(?:INSERT\s+INTO|UPDATE|DELETE\s+FROM)\s+\[?(?:dbo\.)?\]?\[?(\w+)\]?")
        .expect("SQL_WRITE_RE")
});

// ── SP Business Logic (Deterministic) ────────────────────────────────────────

/// Analyze stored procedure business logic using deterministic extraction.
pub fn deterministic_sp_summary(sp_name: &str, sp_body: &str) -> SpBusinessLogic {
    let content_hash = engram_core::ids::ContentHash::compute(sp_body.as_bytes()).0;

    // Extract tables read
    let tables_read: Vec<String> = SQL_SELECT_RE
        .captures_iter(sp_body)
        .map(|c| c[1].to_string())
        .filter(|t| !is_sql_keyword(t))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    // Extract tables written
    let tables_written: Vec<String> = SQL_WRITE_RE
        .captures_iter(sp_body)
        .map(|c| c[1].to_string())
        .filter(|t| !is_sql_keyword(t))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    let mut data_tables: Vec<String> = tables_read
        .iter()
        .chain(tables_written.iter())
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    data_tables.sort();

    // Extract parameters (only from the declaration section, before AS/BEGIN)
    static PARAM_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?im)^\s*(@\w+)\s+([\w]+(?:\(\d+(?:,\s*\d+)?\))?)").expect("PARAM_RE")
    });
    // Only look at the declaration section (before AS or BEGIN)
    let decl_end = sp_body
        .to_uppercase()
        .find("\nAS\n")
        .or_else(|| sp_body.to_uppercase().find("\nAS\r\n"))
        .or_else(|| sp_body.to_uppercase().find("\nBEGIN"))
        .unwrap_or(sp_body.len());
    let decl_section = &sp_body[..decl_end];
    let parameters: Vec<String> = PARAM_RE
        .captures_iter(decl_section)
        .map(|c| format!("{} {}", &c[1], &c[2]))
        .collect();

    // Extract SP calls
    let calls: Vec<String> = EXEC_SP_RE
        .captures_iter(sp_body)
        .map(|c| c[1].to_string())
        .filter(|n| n.to_lowercase() != sp_name.to_lowercase())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    // Build side effects
    let mut side_effects = Vec::new();
    if !tables_written.is_empty() {
        side_effects.push(format!("Writes to: {}", tables_written.join(", ")));
    }
    let body_lower = sp_body.to_lowercase();
    if body_lower.contains("raiserror") || body_lower.contains("throw") {
        side_effects.push("Raises errors".to_string());
    }
    if body_lower.contains("@@trancount")
        || body_lower.contains("begin tran")
        || body_lower.contains("begin transaction")
    {
        side_effects.push("Uses explicit transactions".to_string());
    }
    if body_lower.contains("cursor") && body_lower.contains("fetch") {
        side_effects.push("Uses cursor-based iteration".to_string());
    }
    if body_lower.contains("dynamic") || body_lower.contains("sp_executesql") {
        side_effects.push("Uses dynamic SQL".to_string());
    }
    if body_lower.contains("print") {
        side_effects.push("Print output".to_string());
    }

    // Build purpose
    let purpose = if !tables_read.is_empty() || !tables_written.is_empty() {
        let reads = if tables_read.is_empty() {
            String::new()
        } else {
            format!("reads {}", tables_read.join(", "))
        };
        let writes = if tables_written.is_empty() {
            String::new()
        } else {
            format!("writes {}", tables_written.join(", "))
        };
        let combined = [reads, writes]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("; ");
        format!("Stored procedure that {combined}")
    } else {
        "Stored procedure".to_string()
    };

    // Build steps from structure
    let mut steps = Vec::new();
    if !parameters.is_empty() {
        steps.push(format!("Accepts {} parameters", parameters.len()));
    }
    if !tables_read.is_empty() {
        steps.push(format!("Reads from {}", tables_read.join(", ")));
    }
    if !tables_written.is_empty() {
        steps.push(format!("Writes to {}", tables_written.join(", ")));
    }
    if !calls.is_empty() {
        steps.push(format!("Calls SPs: {}", calls.join(", ")));
    }

    SpBusinessLogic {
        sp_name: sp_name.to_string(),
        purpose,
        steps,
        data_tables,
        parameters,
        side_effects,
        calls_other_sps: calls,
        content_hash,
    }
}

fn is_sql_keyword(s: &str) -> bool {
    matches!(
        s.to_lowercase().as_str(),
        "select"
            | "where"
            | "set"
            | "values"
            | "table"
            | "dbo"
            | "sys"
            | "information_schema"
            | "inserted"
            | "deleted"
            | "declare"
            | "cursor"
            | "fetch"
            | "open"
            | "close"
            | "deallocate"
            | "begin"
            | "end"
            | "if"
            | "else"
            | "while"
            | "return"
            | "null"
            | "not"
            | "and"
            | "or"
            | "exists"
            | "in"
            | "case"
            | "when"
            | "then"
            | "output"
            | "top"
            | "distinct"
            | "as"
            | "on"
            | "inner"
            | "outer"
            | "left"
            | "right"
            | "cross"
            | "full"
            | "into"
            | "exec"
            | "execute"
    )
}

// ── SP Call Chain Detection ──────────────────────────────────────────────────

/// Build SP→SP call chains from SQL file contents.
/// Returns chains (linear paths and cycles).
pub fn detect_sp_call_chains(sql_files: &[(String, String)]) -> Vec<SpCallChain> {
    // Extract all SP names and their bodies
    static SP_DEF_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?ims)CREATE\s+(?:OR\s+ALTER\s+)?PROC(?:EDURE)?\s+\[?(?:dbo\.)?\]?\[?(\w+)\]?")
            .expect("SP_DEF_RE")
    });

    let mut adjacency: HashMap<String, Vec<String>> = HashMap::new();
    let mut all_sps: HashSet<String> = HashSet::new();

    for (_path, content) in sql_files {
        // Find SP definitions and extract their call targets
        let mut sp_starts: Vec<(String, usize)> = Vec::new();
        for cap in SP_DEF_RE.captures_iter(content) {
            let name = cap[1].to_string();
            let pos = cap.get(0).expect("group 0 always present").start();
            all_sps.insert(name.clone());
            sp_starts.push((name, pos));
        }

        // For each SP, extract the body until the next SP or end of file
        for i in 0..sp_starts.len() {
            let (ref sp_name, start) = sp_starts[i];
            let end = if i + 1 < sp_starts.len() {
                sp_starts[i + 1].1
            } else {
                content.len()
            };
            let body = &content[start..end];

            let callees: Vec<String> = EXEC_SP_RE
                .captures_iter(body)
                .map(|c| c[1].to_string())
                .filter(|n| n.to_lowercase() != sp_name.to_lowercase())
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();

            if !callees.is_empty() {
                adjacency.insert(sp_name.clone(), callees);
            }
        }
    }

    let mut chains = Vec::new();

    // Find all chains starting from SPs that call others
    for start_sp in adjacency.keys() {
        let mut visited = HashSet::new();
        let mut path = vec![start_sp.clone()];
        visited.insert(start_sp.clone());

        build_chains(start_sp, &adjacency, &mut visited, &mut path, &mut chains);
    }

    // Deduplicate: keep only unique chains (by sorted representation for non-cycles)
    let mut seen: HashSet<String> = HashSet::new();
    chains.retain(|c| {
        let key = format!("{:?}:{}", c.chain, c.is_cycle);
        seen.insert(key)
    });

    chains
}

fn build_chains(
    current: &str,
    adjacency: &HashMap<String, Vec<String>>,
    visited: &mut HashSet<String>,
    path: &mut Vec<String>,
    chains: &mut Vec<SpCallChain>,
) {
    if let Some(callees) = adjacency.get(current) {
        for callee in callees {
            if visited.contains(callee) {
                // Cycle detected
                let mut cycle_path = path.clone();
                cycle_path.push(callee.clone());
                chains.push(SpCallChain {
                    chain: cycle_path,
                    is_cycle: true,
                });
            } else {
                visited.insert(callee.clone());
                path.push(callee.clone());

                // Only record leaf chains (callee has no further callees)
                // or cycles; this avoids redundant sub-chains like A→B when A→B→C exists
                let is_leaf = !adjacency.contains_key(callee);
                if is_leaf && path.len() >= 2 {
                    chains.push(SpCallChain {
                        chain: path.clone(),
                        is_cycle: false,
                    });
                }

                build_chains(callee, adjacency, visited, path, chains);

                path.pop();
                visited.remove(callee);
            }
        }
    }
}

// ── Trigger Detection ────────────────────────────────────────────────────────

/// Detect triggers from SQL file contents.
pub fn detect_triggers(sql_files: &[(String, String)]) -> Vec<TriggerInfo> {
    let mut triggers = Vec::new();

    for (_path, content) in sql_files {
        for cap in CREATE_TRIGGER_RE.captures_iter(content) {
            let name = cap[1].to_string();
            let table = cap[2].to_string();
            let trigger_type = cap[3].to_string().replace("  ", " ");
            let events_str = &cap[4];

            let event_types: Vec<String> = events_str
                .split(',')
                .map(|s| s.trim().to_uppercase())
                .filter(|s| !s.is_empty())
                .collect();

            // Extract a short summary of what the trigger does
            let trigger_start = cap.get(0).expect("group 0 always present").end();
            let body_end = content[trigger_start..]
                .find("\nGO")
                .or_else(|| content[trigger_start..].find("\nCREATE"))
                .map(|p| trigger_start + p)
                .unwrap_or(content.len().min(trigger_start + 500));
            let body = &content[trigger_start..body_end];

            let tables_touched: Vec<String> = TABLE_REF_RE
                .captures_iter(body)
                .map(|c| c[1].to_string())
                .filter(|t| !is_sql_keyword(t) && t.to_lowercase() != table.to_lowercase())
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();

            let body_summary = if tables_touched.is_empty() {
                format!("Trigger on {table}")
            } else {
                format!(
                    "Trigger on {table}, also touches {}",
                    tables_touched.join(", ")
                )
            };

            triggers.push(TriggerInfo {
                name,
                target_table: table,
                event_types,
                trigger_type,
                body_summary,
            });
        }
    }

    triggers
}

/// Cross-reference triggers with code-level SQL operations.
pub fn cross_reference_triggers(
    triggers: &[TriggerInfo],
    code_tables: &HashSet<String>,
) -> Vec<String> {
    let mut warnings = Vec::new();

    for trigger in triggers {
        let table_lower = trigger.target_table.to_lowercase();
        // Check if code writes to this table
        if code_tables.iter().any(|t| t.to_lowercase() == table_lower) {
            warnings.push(format!(
                "Code interacts with table '{}' which has trigger '{}' ({} on {}). \
                 Ensure migration preserves this behavior.",
                trigger.target_table,
                trigger.name,
                trigger.event_types.join(", "),
                trigger.trigger_type,
            ));
        }
    }

    warnings
}

// ── Schema Parsing (Ticket 37.4) ─────────────────────────────────────────────

/// Parse CREATE TABLE statements from SQL files.
pub fn parse_create_tables(sql_files: &[(String, String)]) -> Vec<SchemaTable> {
    let mut tables = Vec::new();

    for (_path, content) in sql_files {
        for cap in CREATE_TABLE_RE.captures_iter(content) {
            let name = cap[1].to_string();
            let paren_start = cap.get(0).expect("group 0 always present").end(); // position right after '('

            // Find matching closing paren using balanced counting
            let body = match find_balanced_paren(content, paren_start) {
                Some(b) => b,
                None => continue,
            };

            let mut columns = Vec::new();
            let mut primary_key = Vec::new();
            let mut foreign_keys = Vec::new();
            let mut check_constraints = Vec::new();
            let mut indexes = Vec::new();

            // Split on commas that aren't inside parentheses
            for segment in split_top_level_commas(body) {
                let trimmed = segment.trim();
                let upper = trimmed.to_uppercase();

                if upper.starts_with("CONSTRAINT") || upper.starts_with("PRIMARY KEY") {
                    parse_constraint(
                        trimmed,
                        &mut primary_key,
                        &mut foreign_keys,
                        &mut check_constraints,
                    );
                } else if upper.starts_with("INDEX") || upper.starts_with("UNIQUE") {
                    indexes.push(trimmed.to_string());
                } else if !trimmed.is_empty()
                    && let Some(col) = parse_column_def(trimmed)
                {
                    columns.push(col);
                }
            }

            tables.push(SchemaTable {
                name,
                columns,
                primary_key,
                foreign_keys,
                check_constraints,
                indexes,
            });
        }
    }

    tables
}

/// Find the balanced closing parenthesis and return the content inside.
fn find_balanced_paren(s: &str, start: usize) -> Option<&str> {
    let bytes = s.as_bytes();
    let mut depth = 1;
    let mut i = start;
    while i < bytes.len() && depth > 0 {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b'\'' => {
                // Skip string literals
                i += 1;
                while i < bytes.len() && bytes[i] != b'\'' {
                    i += 1;
                }
            }
            _ => {}
        }
        if depth > 0 {
            i += 1;
        }
    }
    if depth == 0 { Some(&s[start..i]) } else { None }
}

/// Split a string on commas that are not inside parentheses.
fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut depth = 0;
    let mut start = 0;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                segments.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < s.len() {
        segments.push(&s[start..]);
    }
    segments
}

fn parse_column_def(line: &str) -> Option<SchemaColumn> {
    // Matches: [ColName] NVARCHAR(100), [ColName] INT, [ColName] NVARCHAR(MAX),
    //          [ColName] DECIMAL(10,2), [ColName] DATETIME2(7), ColName UNIQUEIDENTIFIER
    static COL_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^\[?(\w+)\]?\s+(\w+(?:\(\s*(?:\d+|MAX)(?:\s*,\s*\d+)?\s*\))?)(.*)$")
            .expect("COL_RE")
    });
    static COMPUTED_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^\[?(\w+)\]?\s+AS\s+(.+)$").expect("COMPUTED_RE"));

    // Check for computed column first
    if let Some(cap) = COMPUTED_RE.captures(line) {
        return Some(SchemaColumn {
            name: cap[1].to_string(),
            data_type: "COMPUTED".to_string(),
            nullable: true,
            default_value: None,
            is_computed: true,
            computed_expression: Some(cap[2].trim().to_string()),
        });
    }

    let cap = COL_RE.captures(line)?;
    let name = cap[1].to_string();
    let data_type = cap[2].to_string();
    let rest = &cap[3];
    let rest_upper = rest.to_uppercase();

    let nullable = !rest_upper.contains("NOT NULL");

    // Extract DEFAULT value — handle both DEFAULT (expr) and DEFAULT 'literal'
    static DEFAULT_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)DEFAULT\s+(\([^)]*\)|'[^']*'|\S+)").expect("DEFAULT_RE"));
    let default_value = DEFAULT_RE.captures(rest).map(|c| c[1].trim().to_string());

    Some(SchemaColumn {
        name,
        data_type,
        nullable,
        default_value,
        is_computed: false,
        computed_expression: None,
    })
}

fn parse_constraint(
    line: &str,
    primary_key: &mut Vec<String>,
    foreign_keys: &mut Vec<ForeignKey>,
    check_constraints: &mut Vec<String>,
) {
    let upper = line.to_uppercase();

    // PRIMARY KEY
    static PK_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)PRIMARY\s+KEY\s*(?:CLUSTERED|NONCLUSTERED)?\s*\(([^)]+)\)").expect("PK_RE")
    });
    if let Some(cap) = PK_RE.captures(line) {
        for col in cap[1].split(',') {
            let col_name = col
                .trim()
                .trim_matches('[')
                .trim_matches(']')
                .trim()
                .to_string();
            if !col_name.is_empty() {
                primary_key.push(col_name);
            }
        }
        return;
    }

    // FOREIGN KEY
    static FK_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)FOREIGN\s+KEY\s*\(\[?(\w+)\]?\)\s*REFERENCES\s+\[?(?:dbo\.)?\]?\[?(\w+)\]?\s*\(\[?(\w+)\]?\)")
            .expect("FK_RE")
    });
    if let Some(cap) = FK_RE.captures(line) {
        static ON_DELETE_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"(?i)ON\s+DELETE\s+(CASCADE|SET\s+NULL|SET\s+DEFAULT|NO\s+ACTION)")
                .expect("ON_DELETE_RE")
        });
        static ON_UPDATE_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"(?i)ON\s+UPDATE\s+(CASCADE|SET\s+NULL|SET\s+DEFAULT|NO\s+ACTION)")
                .expect("ON_UPDATE_RE")
        });

        let on_delete = ON_DELETE_RE.captures(line).map(|c| c[1].to_string());
        let on_update = ON_UPDATE_RE.captures(line).map(|c| c[1].to_string());

        foreign_keys.push(ForeignKey {
            column: cap[1].to_string(),
            references_table: cap[2].to_string(),
            references_column: cap[3].to_string(),
            on_delete,
            on_update,
        });
        return;
    }

    // CHECK constraint
    if upper.contains("CHECK") {
        static CHECK_RE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"(?i)CHECK\s*\((.+)\)").expect("CHECK_RE"));
        if let Some(cap) = CHECK_RE.captures(line) {
            check_constraints.push(cap[1].trim().to_string());
        }
    }
}

/// Parse CREATE VIEW statements.
pub fn parse_create_views(sql_files: &[(String, String)]) -> Vec<ViewInfo> {
    let mut views = Vec::new();

    for (_path, content) in sql_files {
        for cap in CREATE_VIEW_RE.captures_iter(content) {
            let name = cap[1].to_string();
            let body = &cap[2];
            let source_tables: Vec<String> = TABLE_REF_RE
                .captures_iter(body)
                .map(|c| c[1].to_string())
                .filter(|t| !is_sql_keyword(t))
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();

            views.push(ViewInfo {
                name,
                source_tables,
            });
        }
    }

    views
}

/// Cross-reference schema against code-level table references and build warnings.
/// Note: `tables` and `views` are passed here to be included in the returned
/// SchemaReport — the caller does NOT need to reassign them.
pub fn cross_reference_schema(
    tables: Vec<SchemaTable>,
    views: Vec<ViewInfo>,
    code_tables: &HashSet<String>,
) -> SchemaReport {
    let mut cross_reference = Vec::new();
    let mut business_rules = Vec::new();

    let schema_table_names: HashSet<String> =
        tables.iter().map(|t| t.name.to_lowercase()).collect();
    let code_tables_lower: HashSet<String> = code_tables.iter().map(|t| t.to_lowercase()).collect();

    // Tables referenced in code but not in schema
    for code_table in code_tables {
        if !schema_table_names.contains(&code_table.to_lowercase()) {
            cross_reference.push(SchemaWarning {
                kind: "table_in_code_not_schema".to_string(),
                message: format!(
                    "Table '{code_table}' referenced in code but no schema definition found"
                ),
            });
        }
    }

    // Tables in schema but not referenced in code
    for table in &tables {
        if !code_tables_lower.contains(&table.name.to_lowercase()) {
            cross_reference.push(SchemaWarning {
                kind: "table_in_schema_not_code".to_string(),
                message: format!(
                    "Table '{}' exists in schema but not referenced in code — possibly used by SPs or triggers",
                    table.name
                ),
            });
        }

        // Surface CHECK constraints as business rules
        for check in &table.check_constraints {
            business_rules.push(format!("Table '{}' CHECK: {}", table.name, check));
        }

        // Surface NOT NULL columns (important for migration — must not omit these in INSERT)
        let required_cols: Vec<&str> = table
            .columns
            .iter()
            .filter(|c| !c.nullable && !c.is_computed && c.default_value.is_none())
            .map(|c| c.name.as_str())
            .collect();
        if !required_cols.is_empty() {
            business_rules.push(format!(
                "Table '{}' required columns (NOT NULL, no default): {}",
                table.name,
                required_cols.join(", ")
            ));
        }

        // Surface computed columns
        for col in &table.columns {
            if col.is_computed
                && let Some(expr) = &col.computed_expression
            {
                business_rules.push(format!(
                    "Table '{}' column '{}' is computed: {} — do not INSERT into it",
                    table.name, col.name, expr
                ));
            }
            // Surface defaults
            if let Some(default) = &col.default_value {
                business_rules.push(format!(
                    "Table '{}' column '{}' defaults to {} when not provided",
                    table.name, col.name, default
                ));
            }
        }

        // Surface IDENTITY columns (auto-increment — don't INSERT into them)
        for col in &table.columns {
            let dt_lower = col.data_type.to_lowercase();
            if dt_lower.contains("identity") {
                business_rules.push(format!(
                    "Table '{}' column '{}' is IDENTITY (auto-increment) — do not INSERT",
                    table.name, col.name
                ));
            }
        }

        // Surface foreign keys as relationships
        for fk in &table.foreign_keys {
            let mut actions = Vec::new();
            if let Some(action) = &fk.on_delete {
                actions.push(format!("ON DELETE {action}"));
            }
            if let Some(action) = &fk.on_update {
                actions.push(format!("ON UPDATE {action}"));
            }
            let action_str = if actions.is_empty() {
                String::new()
            } else {
                format!(" ({})", actions.join(", "))
            };
            business_rules.push(format!(
                "Table '{}' FK: {}.{} → {}.{}{}",
                table.name,
                table.name,
                fk.column,
                fk.references_table,
                fk.references_column,
                action_str
            ));
        }
    }

    // Surface view dependencies as business rules
    for view in &views {
        if !view.source_tables.is_empty() {
            business_rules.push(format!(
                "View '{}' depends on: {} — changes to these tables affect the view",
                view.name,
                view.source_tables.join(", ")
            ));
        }
    }

    SchemaReport {
        tables,
        views,
        cross_reference,
        business_rules,
    }
}

// ── Full Database Intelligence Builder ───────────────────────────────────────

/// Build the complete database intelligence report.
pub fn build_database_intelligence(
    sp_catalog: &StoredProcedureCatalog,
    sql_files: &[(String, String)],
    code_tables: &HashSet<String>,
) -> DatabaseIntelligence {
    if sql_files.is_empty() {
        return DatabaseIntelligence::default();
    }

    // SP business logic (deterministic)
    let sp_logic: Vec<SpBusinessLogic> = sp_catalog
        .procedures
        .iter()
        .filter_map(|sp| {
            // Find the SP body in sql_files
            let body = find_sp_body(sql_files, &sp.name)?;
            Some(deterministic_sp_summary(&sp.name, body))
        })
        .collect();

    // SP call chains
    let sp_call_chains = detect_sp_call_chains(sql_files);

    // Triggers
    let triggers = detect_triggers(sql_files);

    // Schema parsing + cross-reference
    let schema_tables = parse_create_tables(sql_files);
    let schema_views = parse_create_views(sql_files);
    let schema_report = cross_reference_schema(schema_tables, schema_views, code_tables);

    // Cross-reference triggers with code
    let trigger_warnings = cross_reference_triggers(&triggers, code_tables);

    let mut warnings = trigger_warnings;

    // Add cycle warnings from call chains
    for chain in &sp_call_chains {
        if chain.is_cycle {
            warnings.push(format!(
                "Circular SP call detected: {}",
                chain.chain.join(" → ")
            ));
        }
    }

    DatabaseIntelligence {
        sp_logic,
        sp_call_chains,
        triggers,
        schema: schema_report,
        warnings,
    }
}

/// Find the body of a stored procedure in SQL files.
fn find_sp_body<'a>(sql_files: &'a [(String, String)], sp_name: &str) -> Option<&'a str> {
    static SP_BODY_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?ims)CREATE\s+(?:OR\s+ALTER\s+)?PROC(?:EDURE)?\s+(?:\[?dbo\]?\.)?\[?(\w+)\]?")
            .expect("SP_BODY_RE")
    });

    for (_path, content) in sql_files {
        // Collect all SP positions first to find boundaries
        let sp_positions: Vec<(String, usize)> = SP_BODY_RE
            .captures_iter(content)
            .map(|cap| {
                (
                    cap[1].to_string(),
                    cap.get(0).expect("group 0 always present").start(),
                )
            })
            .collect();

        for (i, (name, start)) in sp_positions.iter().enumerate() {
            if name.eq_ignore_ascii_case(sp_name) {
                let end = sp_positions.get(i + 1).map_or(content.len(), |(_, s)| *s);
                return Some(&content[*start..end]);
            }
        }
    }
    None
}

// ── Rendering ────────────────────────────────────────────────────────────────

/// Render database intelligence as a markdown section.
pub fn render_database_intelligence_markdown(intel: &DatabaseIntelligence) -> String {
    if intel.sp_logic.is_empty()
        && intel.triggers.is_empty()
        && intel.schema.tables.is_empty()
        && intel.sp_call_chains.is_empty()
    {
        return String::new();
    }

    let mut md = String::with_capacity(16_000);
    md.push_str("## Database Intelligence\n\n");

    // SP summaries
    if !intel.sp_logic.is_empty() {
        md.push_str("### Stored Procedure Analysis\n\n");
        md.push_str("| SP Name | Purpose | Tables | Calls Other SPs |\n|---|---|---|---|\n");
        for sp in &intel.sp_logic {
            let tables = if sp.data_tables.is_empty() {
                "—".to_string()
            } else {
                sp.data_tables.join(", ")
            };
            let calls = if sp.calls_other_sps.is_empty() {
                "—".to_string()
            } else {
                sp.calls_other_sps.join(", ")
            };
            md.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                sp.sp_name,
                sp.purpose.replace('|', "\\|"),
                tables.replace('|', "\\|"),
                calls.replace('|', "\\|"),
            ));
        }
        md.push('\n');
    }

    // SP call chains
    let significant_chains: Vec<_> = intel
        .sp_call_chains
        .iter()
        .filter(|c| c.chain.len() >= 3 || c.is_cycle)
        .collect();
    if !significant_chains.is_empty() {
        md.push_str("### SP Call Chains\n\n");
        for chain in &significant_chains {
            let marker = if chain.is_cycle { " ⚠️ CYCLE" } else { "" };
            md.push_str(&format!("- {}{marker}\n", chain.chain.join(" → ")));
        }
        md.push('\n');
    }

    // Triggers
    if !intel.triggers.is_empty() {
        md.push_str("### Database Triggers\n\n");
        md.push_str("| Trigger | Table | Events | Type | Summary |\n|---|---|---|---|---|\n");
        for t in &intel.triggers {
            md.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                t.name,
                t.target_table,
                t.event_types.join(", "),
                t.trigger_type,
                t.body_summary.replace('|', "\\|"),
            ));
        }
        md.push('\n');
    }

    // Schema overview — full column inventory for migration
    if !intel.schema.tables.is_empty() {
        md.push_str("### Schema Overview\n\n");
        for table in &intel.schema.tables {
            md.push_str(&format!("#### {}", table.name));
            if !table.primary_key.is_empty() {
                md.push_str(&format!(" (PK: {})", table.primary_key.join(", ")));
            }
            md.push('\n');

            if !table.columns.is_empty() {
                md.push_str(
                    "\n| Column | Type | Nullable | Default | Notes |\n|---|---|---|---|---|\n",
                );
                for col in &table.columns {
                    let nullable = if col.nullable { "YES" } else { "NO" };
                    let default = col.default_value.as_deref().unwrap_or("—");
                    let notes = if col.is_computed {
                        col.computed_expression
                            .as_deref()
                            .map(|e| format!("Computed: {}", e.replace('|', "\\|")))
                            .unwrap_or_else(|| "Computed".to_string())
                    } else {
                        String::new()
                    };
                    md.push_str(&format!(
                        "| {} | {} | {} | {} | {} |\n",
                        col.name,
                        col.data_type.replace('|', "\\|"),
                        nullable,
                        default.replace('|', "\\|"),
                        notes,
                    ));
                }
            }

            if !table.foreign_keys.is_empty() {
                md.push_str(&format!(
                    "\nForeign Keys: {}\n",
                    table
                        .foreign_keys
                        .iter()
                        .map(|fk| format!(
                            "{} → {}.{}",
                            fk.column, fk.references_table, fk.references_column
                        ))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            md.push('\n');
        }
    }

    // Views
    if !intel.schema.views.is_empty() {
        md.push_str("### Views\n\n");
        for view in &intel.schema.views {
            md.push_str(&format!(
                "- **{}**: sources from {}\n",
                view.name,
                if view.source_tables.is_empty() {
                    "unknown".to_string()
                } else {
                    view.source_tables.join(", ")
                }
            ));
        }
        md.push('\n');
    }

    // Business rules from schema
    if !intel.schema.business_rules.is_empty() {
        md.push_str("### Schema-Derived Business Rules\n\n");
        for rule in &intel.schema.business_rules {
            md.push_str(&format!("- {rule}\n"));
        }
        md.push('\n');
    }

    // Cross-reference warnings
    if !intel.schema.cross_reference.is_empty() {
        md.push_str("### Schema Cross-Reference Warnings\n\n");
        for warn in &intel.schema.cross_reference {
            md.push_str(&format!("- {}\n", warn.message));
        }
        md.push('\n');
    }

    // Trigger warnings
    if !intel.warnings.is_empty() {
        md.push_str("### Database Warnings\n\n");
        for w in &intel.warnings {
            md.push_str(&format!("- ⚠️ {w}\n"));
        }
        md.push('\n');
    }

    md
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_deterministic_sp_summary_basic() {
        let body = r#"CREATE PROCEDURE usp_GetCustomers
    @Region NVARCHAR(50),
    @Status INT
AS
BEGIN
    SELECT CustomerID, Name, Region
    FROM Customers
    WHERE Region = @Region AND Status = @Status
END"#;

        let result = deterministic_sp_summary("usp_GetCustomers", body);
        assert_eq!(result.sp_name, "usp_GetCustomers");
        assert!(!result.purpose.is_empty());
        assert!(result.data_tables.contains(&"Customers".to_string()));
        assert_eq!(result.parameters.len(), 2);
        assert!(result.calls_other_sps.is_empty());
    }

    #[test]
    fn test_deterministic_sp_with_exec() {
        let body = r#"CREATE PROCEDURE usp_CreateOrder
    @CustomerID INT
AS
BEGIN
    INSERT INTO Orders (CustomerID, OrderDate)
    VALUES (@CustomerID, GETDATE())

    EXEC usp_ValidateInventory @CustomerID
    EXECUTE usp_UpdateStock @CustomerID
END"#;

        let result = deterministic_sp_summary("usp_CreateOrder", body);
        assert!(result.data_tables.contains(&"Orders".to_string()));
        assert!(
            result
                .calls_other_sps
                .contains(&"usp_ValidateInventory".to_string())
        );
        assert!(
            result
                .calls_other_sps
                .contains(&"usp_UpdateStock".to_string())
        );
    }

    #[test]
    fn test_sp_call_chain_linear() {
        let sql = vec![(
            "procs.sql".to_string(),
            r#"
CREATE PROCEDURE usp_A AS BEGIN EXEC usp_B END
GO
CREATE PROCEDURE usp_B AS BEGIN EXEC usp_C END
GO
CREATE PROCEDURE usp_C AS BEGIN SELECT 1 END
GO"#
            .to_string(),
        )];

        let chains = detect_sp_call_chains(&sql);
        // Should find A→B→C (maximal chain only, not sub-chains)
        let has_abc = chains.iter().any(|c| {
            c.chain.len() == 3
                && c.chain[0] == "usp_A"
                && c.chain[1] == "usp_B"
                && c.chain[2] == "usp_C"
                && !c.is_cycle
        });
        assert!(has_abc, "Should detect A→B→C chain");
    }

    #[test]
    fn test_sp_call_chain_cycle() {
        let sql = vec![(
            "procs.sql".to_string(),
            r#"
CREATE PROCEDURE usp_A AS BEGIN EXEC usp_B END
GO
CREATE PROCEDURE usp_B AS BEGIN EXEC usp_A END
GO"#
            .to_string(),
        )];

        let chains = detect_sp_call_chains(&sql);
        let has_cycle = chains.iter().any(|c| c.is_cycle);
        assert!(has_cycle, "Should detect cycle");
    }

    #[test]
    fn test_detect_trigger_after_insert() {
        let sql = vec![(
            "triggers.sql".to_string(),
            r#"
CREATE TRIGGER trg_AuditInsert ON Orders
AFTER INSERT
AS
BEGIN
    INSERT INTO AuditLog (Action, TableName)
    SELECT 'INSERT', 'Orders'
    FROM inserted
END
GO"#
            .to_string(),
        )];

        let triggers = detect_triggers(&sql);
        assert_eq!(triggers.len(), 1);
        assert_eq!(triggers[0].name, "trg_AuditInsert");
        assert_eq!(triggers[0].target_table, "Orders");
        assert!(triggers[0].event_types.contains(&"INSERT".to_string()));
        assert_eq!(triggers[0].trigger_type, "AFTER");
    }

    #[test]
    fn test_detect_trigger_instead_of_delete() {
        let sql = vec![(
            "triggers.sql".to_string(),
            r#"
CREATE TRIGGER trg_SoftDelete ON Customers
INSTEAD OF DELETE
AS
BEGIN
    UPDATE Customers SET IsDeleted = 1
    WHERE CustomerID IN (SELECT CustomerID FROM deleted)
END
GO"#
            .to_string(),
        )];

        let triggers = detect_triggers(&sql);
        assert_eq!(triggers.len(), 1);
        assert_eq!(triggers[0].trigger_type, "INSTEAD OF");
        assert!(triggers[0].event_types.contains(&"DELETE".to_string()));
    }

    #[test]
    fn test_trigger_cross_reference() {
        let triggers = vec![TriggerInfo {
            name: "trg_AuditInsert".to_string(),
            target_table: "Orders".to_string(),
            event_types: vec!["INSERT".to_string()],
            trigger_type: "AFTER".to_string(),
            body_summary: "Trigger on Orders".to_string(),
        }];

        let mut code_tables = HashSet::new();
        code_tables.insert("Orders".to_string());

        let warnings = cross_reference_triggers(&triggers, &code_tables);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("trg_AuditInsert"));
    }

    // ── Schema Parsing Tests (Ticket 37.4) ──────────────────────────────

    #[test]
    fn test_parse_create_table_basic() {
        let sql = vec![(
            "schema.sql".to_string(),
            r#"CREATE TABLE [dbo].[Customers] (
    [CustomerID] INT NOT NULL,
    [Name] NVARCHAR(100) NOT NULL,
    [Email] NVARCHAR(200) NULL,
    [Status] NVARCHAR(20) NOT NULL DEFAULT ('Active'),
    CONSTRAINT PK_Customers PRIMARY KEY CLUSTERED ([CustomerID])
);
GO"#
            .to_string(),
        )];

        let tables = parse_create_tables(&sql);
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].name, "Customers");
        assert!(tables[0].columns.len() >= 4);
        assert!(!tables[0].primary_key.is_empty());
    }

    #[test]
    fn test_parse_default_constraint() {
        let sql = vec![(
            "schema.sql".to_string(),
            r#"CREATE TABLE [dbo].[Orders] (
    [OrderID] INT NOT NULL,
    [CreatedDate] DATETIME NOT NULL DEFAULT (GETDATE()),
    CONSTRAINT PK_Orders PRIMARY KEY ([OrderID])
);
GO"#
            .to_string(),
        )];

        let tables = parse_create_tables(&sql);
        assert_eq!(tables.len(), 1);
        let created_col = tables[0].columns.iter().find(|c| c.name == "CreatedDate");
        assert!(created_col.is_some());
        assert!(created_col.unwrap().default_value.is_some());
    }

    #[test]
    fn test_parse_check_constraint() {
        let sql = vec![(
            "schema.sql".to_string(),
            r#"CREATE TABLE [dbo].[Products] (
    [ProductID] INT NOT NULL,
    [Price] DECIMAL(10,2) NOT NULL,
    CONSTRAINT PK_Products PRIMARY KEY ([ProductID]),
    CONSTRAINT CK_Price CHECK (Price > 0)
);
GO"#
            .to_string(),
        )];

        let tables = parse_create_tables(&sql);
        assert_eq!(tables.len(), 1);
        assert!(!tables[0].check_constraints.is_empty());
        assert!(tables[0].check_constraints[0].contains("Price > 0"));
    }

    #[test]
    fn test_parse_foreign_key_with_cascade() {
        let sql = vec![(
            "schema.sql".to_string(),
            r#"CREATE TABLE [dbo].[OrderItems] (
    [ItemID] INT NOT NULL,
    [OrderID] INT NOT NULL,
    CONSTRAINT PK_Items PRIMARY KEY ([ItemID]),
    CONSTRAINT FK_Order FOREIGN KEY ([OrderID]) REFERENCES [Orders]([OrderID]) ON DELETE CASCADE
);
GO"#
            .to_string(),
        )];

        let tables = parse_create_tables(&sql);
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].foreign_keys.len(), 1);
        assert_eq!(tables[0].foreign_keys[0].references_table, "Orders");
        assert_eq!(
            tables[0].foreign_keys[0].on_delete.as_deref(),
            Some("CASCADE")
        );
    }

    #[test]
    fn test_parse_computed_column() {
        let sql = vec![(
            "schema.sql".to_string(),
            r#"CREATE TABLE [dbo].[People] (
    [FirstName] NVARCHAR(50) NOT NULL,
    [LastName] NVARCHAR(50) NOT NULL,
    [FullName] AS ([FirstName] + ' ' + [LastName])
);
GO"#
            .to_string(),
        )];

        let tables = parse_create_tables(&sql);
        assert_eq!(tables.len(), 1);
        let computed = tables[0].columns.iter().find(|c| c.name == "FullName");
        assert!(computed.is_some());
        assert!(computed.unwrap().is_computed);
    }

    #[test]
    fn test_schema_cross_reference_table_in_code_not_schema() {
        let tables = vec![];
        let mut code_tables = HashSet::new();
        code_tables.insert("MissingTable".to_string());

        let report = cross_reference_schema(tables, vec![], &code_tables);
        assert!(
            report
                .cross_reference
                .iter()
                .any(|w| w.kind == "table_in_code_not_schema")
        );
    }

    #[test]
    fn test_schema_cross_reference_check_surfaced() {
        let tables = vec![SchemaTable {
            name: "Products".to_string(),
            columns: vec![],
            primary_key: vec![],
            foreign_keys: vec![],
            check_constraints: vec!["Price > 0".to_string()],
            indexes: vec![],
        }];
        let code_tables = HashSet::new();

        let report = cross_reference_schema(tables, vec![], &code_tables);
        assert!(
            report
                .business_rules
                .iter()
                .any(|r| r.contains("Price > 0"))
        );
    }

    #[test]
    fn test_parse_view() {
        let sql = vec![(
            "views.sql".to_string(),
            r#"CREATE VIEW [dbo].[vw_ActiveCustomers] AS
SELECT CustomerID, Name
FROM Customers
JOIN Orders ON Customers.CustomerID = Orders.CustomerID
WHERE Customers.Status = 'Active'
GO"#
            .to_string(),
        )];

        let views = parse_create_views(&sql);
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].name, "vw_ActiveCustomers");
        assert!(views[0].source_tables.contains(&"Customers".to_string()));
    }

    #[test]
    fn test_empty_sql_files_no_crash() {
        let sql: Vec<(String, String)> = vec![];
        let code_tables = HashSet::new();
        let sp_catalog = StoredProcedureCatalog {
            procedures: vec![],
            total_procedures: 0,
            procedures_with_params: 0,
            procedures_called_from_code: 0,
            uncalled_procedures: vec![],
        };

        let intel = build_database_intelligence(&sp_catalog, &sql, &code_tables);
        assert!(intel.sp_logic.is_empty());
        assert!(intel.triggers.is_empty());
        assert!(intel.schema.tables.is_empty());
    }

    #[test]
    fn test_sp_chain_rendered_in_markdown() {
        let intel = DatabaseIntelligence {
            sp_logic: vec![],
            sp_call_chains: vec![SpCallChain {
                chain: vec![
                    "usp_A".to_string(),
                    "usp_B".to_string(),
                    "usp_C".to_string(),
                ],
                is_cycle: false,
            }],
            triggers: vec![],
            schema: SchemaReport::default(),
            warnings: vec![],
        };

        let md = render_database_intelligence_markdown(&intel);
        assert!(md.contains("usp_A → usp_B → usp_C"));
    }

    #[test]
    fn test_trigger_alert_rendered_in_markdown() {
        let intel = DatabaseIntelligence {
            sp_logic: vec![],
            sp_call_chains: vec![],
            triggers: vec![TriggerInfo {
                name: "trg_Audit".to_string(),
                target_table: "Orders".to_string(),
                event_types: vec!["INSERT".to_string()],
                trigger_type: "AFTER".to_string(),
                body_summary: "Logs to AuditLog".to_string(),
            }],
            schema: SchemaReport::default(),
            warnings: vec![],
        };

        let md = render_database_intelligence_markdown(&intel);
        assert!(md.contains("trg_Audit"));
        assert!(md.contains("Orders"));
    }
}
