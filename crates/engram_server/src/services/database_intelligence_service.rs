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
    /// Analysis of SQL files that are NOT stored-procedure definitions —
    /// post-deployment seed/merge/migration scripts. See `SqlScriptAnalysis`.
    #[serde(default)]
    pub sql_scripts: Vec<SqlScriptAnalysis>,
    pub warnings: Vec<String>,
}

/// Classification of a standalone SQL file that is not a `CREATE PROCEDURE`
/// definition — typically a post-deployment seed/merge or a migration step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptType {
    /// Seeds or refreshes reference data via `INSERT` / `MERGE`.
    Seed,
    /// Changes schema: `ALTER TABLE`, constraint changes, etc.
    Migration,
    /// Creates or drops indexes / non-table objects.
    SchemaChange,
    /// Mixes two or more of the above.
    Mixed,
}

impl ScriptType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ScriptType::Seed => "Seed",
            ScriptType::Migration => "Migration",
            ScriptType::SchemaChange => "SchemaChange",
            ScriptType::Mixed => "Mixed",
        }
    }
}

/// Analysis of a single post-deployment SQL script (non-SP file).
///
/// The business-logic SQL on real migration projects often lives here —
/// seed/merge scripts that populate reference tables, per-locale translation
/// files, and ad-hoc DDL steps. These never show up in the stored-procedure
/// catalog and are otherwise invisible to migration tooling.
#[derive(Debug, Clone, Serialize)]
pub struct SqlScriptAnalysis {
    pub file_path: String,
    pub script_type: ScriptType,
    /// Distinct table names referenced by the detected operations
    /// (`INSERT`, `MERGE`, `UPDATE`, `DELETE`, `ALTER TABLE`, `CREATE/DROP INDEX`).
    pub tables_affected: Vec<String>,
    /// Uppercased operation keywords actually seen, e.g. `INSERT`, `MERGE`, `ALTER`.
    pub operations: Vec<String>,
    /// Rough floor on the number of rows touched when the script runs —
    /// derived by counting `(…),(…)` tuples in `INSERT … VALUES` blocks plus
    /// the number of `WHEN MATCHED … UPDATE` / `WHEN NOT MATCHED … INSERT`
    /// branches in `MERGE` statements. `None` when nothing seedy was found.
    pub row_count_estimate: Option<usize>,
    /// Deterministic one-line summary (for markdown rendering and for
    /// consumers that want a stable, non-LLM description).
    pub purpose_summary: String,
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

// ── Post-deployment script analysis ──────────────────────────────────────────

/// Strip `--` line comments and `/* … */` block comments from a SQL chunk
/// before regex scanning. The markers inside a string literal are not
/// removed here — seed scripts rarely contain them and the downstream
/// counts only need a rough floor.
fn strip_sql_comments(src: &str) -> String {
    // Remove /* ... */ block comments (non-greedy, multi-line).
    static BLOCK_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?s)/\*.*?\*/").expect("BLOCK_RE"));
    // Remove `-- …` to end of line.
    static LINE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"--[^\n]*").expect("LINE_RE"));
    let no_block = BLOCK_RE.replace_all(src, " ");
    LINE_RE.replace_all(&no_block, " ").into_owned()
}

/// Extract the bare table name from an optional `[schema].[name]` form,
/// dropping brackets and quotes so downstream renders are clean.
fn clean_table_name(raw: &str) -> String {
    let last = raw
        .rsplit('.')
        .next()
        .unwrap_or(raw)
        .trim()
        .trim_matches(|c| matches!(c, '[' | ']' | '"' | '`'));
    last.to_string()
}

fn contains_create_procedure(content: &str) -> bool {
    static CREATE_PROC_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\bCREATE\s+(?:OR\s+ALTER\s+)?PROC(?:EDURE)?\b").expect("CREATE_PROC_RE")
    });
    // Strip comments first — a `-- CREATE PROCEDURE …` line in a seed
    // script is not an SP definition and must not exclude the file.
    let stripped = strip_sql_comments(content);
    CREATE_PROC_RE.is_match(&stripped)
}

/// Tokens that sometimes follow `UPDATE` or `INSERT INTO` in real SQL but
/// are not table names — e.g. `UPDATE SET …` inside a `MERGE … WHEN MATCHED
/// THEN UPDATE SET …` clause. Filter these out so they don't leak into
/// `tables_affected`.
fn is_sql_reserved_table_like(token: &str) -> bool {
    matches!(
        token.to_ascii_uppercase().as_str(),
        "SET" | "WHERE" | "FROM" | "ON" | "INTO" | "VALUES" | "SELECT" | "TABLE" | "INDEX"
    )
}

fn analyze_sql_script(path: &str, content: &str) -> Option<SqlScriptAnalysis> {
    static INSERT_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)\bINSERT\s+INTO\s+([\[\]\w.\-]+)").expect("INSERT_RE"));
    static MERGE_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\bMERGE\s+(?:INTO\s+)?([\[\]\w.\-]+)").expect("MERGE_RE")
    });
    static UPDATE_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)\bUPDATE\s+([\[\]\w.\-]+)").expect("UPDATE_RE"));
    static DELETE_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)\bDELETE\s+FROM\s+([\[\]\w.\-]+)").expect("DELETE_RE"));
    static ALTER_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)\bALTER\s+TABLE\s+([\[\]\w.\-]+)").expect("ALTER_RE"));
    static CREATE_INDEX_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\bCREATE\s+(?:UNIQUE\s+)?(?:CLUSTERED\s+|NONCLUSTERED\s+)?INDEX\s+[\[\]\w.\-]+\s+ON\s+([\[\]\w.\-]+)")
            .expect("CREATE_INDEX_RE")
    });
    static DROP_INDEX_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\bDROP\s+INDEX\s+[\[\]\w.\-]+\s+ON\s+([\[\]\w.\-]+)")
            .expect("DROP_INDEX_RE")
    });
    // `(…),` tuple boundaries inside a VALUES list → approximate row count.
    static ROW_TUPLE_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\)\s*,\s*\(").expect("ROW_TUPLE_RE"));
    // `WHEN MATCHED THEN UPDATE` / `WHEN NOT MATCHED THEN INSERT` branches.
    static MERGE_BRANCH_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)\bWHEN\s+(?:NOT\s+)?MATCHED\b[\s\S]{0,200}?\bTHEN\s+(?:INSERT|UPDATE|DELETE)",
        )
        .expect("MERGE_BRANCH_RE")
    });

    let stripped = strip_sql_comments(content);

    let mut tables: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let push_table = |t: &str, tables: &mut Vec<String>, seen: &mut HashSet<String>| {
        let name = clean_table_name(t);
        if name.is_empty() || is_sql_reserved_table_like(&name) {
            return;
        }
        let key = name.to_lowercase();
        if seen.insert(key) {
            tables.push(name);
        }
    };

    let mut operations: Vec<String> = Vec::new();
    let add_op = |op: &str, ops: &mut Vec<String>| {
        if !ops.iter().any(|s| s == op) {
            ops.push(op.to_string());
        }
    };

    let mut insert_count = 0usize;
    for cap in INSERT_RE.captures_iter(&stripped) {
        push_table(&cap[1], &mut tables, &mut seen);
        insert_count += 1;
    }
    if insert_count > 0 {
        add_op("INSERT", &mut operations);
    }

    let mut merge_count = 0usize;
    for cap in MERGE_RE.captures_iter(&stripped) {
        push_table(&cap[1], &mut tables, &mut seen);
        merge_count += 1;
    }
    if merge_count > 0 {
        add_op("MERGE", &mut operations);
    }

    for cap in UPDATE_RE.captures_iter(&stripped) {
        push_table(&cap[1], &mut tables, &mut seen);
        add_op("UPDATE", &mut operations);
    }
    for cap in DELETE_RE.captures_iter(&stripped) {
        push_table(&cap[1], &mut tables, &mut seen);
        add_op("DELETE", &mut operations);
    }

    let mut alter_count = 0usize;
    for cap in ALTER_RE.captures_iter(&stripped) {
        push_table(&cap[1], &mut tables, &mut seen);
        alter_count += 1;
    }
    if alter_count > 0 {
        add_op("ALTER", &mut operations);
    }

    let mut index_count = 0usize;
    for cap in CREATE_INDEX_RE.captures_iter(&stripped) {
        push_table(&cap[1], &mut tables, &mut seen);
        index_count += 1;
    }
    for cap in DROP_INDEX_RE.captures_iter(&stripped) {
        push_table(&cap[1], &mut tables, &mut seen);
        index_count += 1;
    }
    if index_count > 0 {
        add_op("INDEX", &mut operations);
    }

    if operations.is_empty() {
        return None;
    }

    let is_seed = insert_count > 0 || merge_count > 0;
    let is_migration = alter_count > 0;
    let is_index = index_count > 0;
    let distinct_categories = [is_seed, is_migration, is_index]
        .iter()
        .filter(|b| **b)
        .count();
    let script_type = if distinct_categories > 1 {
        ScriptType::Mixed
    } else if is_seed {
        ScriptType::Seed
    } else if is_migration {
        ScriptType::Migration
    } else {
        ScriptType::SchemaChange
    };

    // Row-count estimate: tuple boundaries in INSERT VALUES (n tuples produce
    // n-1 boundaries, so add one per detected INSERT) plus MERGE branches.
    let tuple_boundaries = ROW_TUPLE_RE.find_iter(&stripped).count();
    let merge_branches = MERGE_BRANCH_RE.find_iter(&stripped).count();
    let row_count_estimate = if is_seed {
        let rows = tuple_boundaries + insert_count + merge_branches;
        if rows == 0 { None } else { Some(rows) }
    } else {
        None
    };

    // Short deterministic summary — the first table wins as the "primary"
    // subject when there are several, since seed scripts are usually
    // dominated by one reference table.
    let primary = tables.first().cloned().unwrap_or_else(|| "—".to_string());
    let extra_tables = if tables.len() > 1 {
        format!(" (+{} more)", tables.len() - 1)
    } else {
        String::new()
    };
    let purpose_summary = match (script_type, row_count_estimate) {
        (ScriptType::Seed, Some(n)) => {
            format!("Seeds/updates `{primary}` with ~{n} rows{extra_tables}")
        }
        (ScriptType::Seed, None) => format!("Seeds/updates `{primary}`{extra_tables}"),
        (ScriptType::Migration, _) => format!("Alters schema of `{primary}`{extra_tables}"),
        (ScriptType::SchemaChange, _) => {
            format!("Index or constraint change on `{primary}`{extra_tables}")
        }
        (ScriptType::Mixed, Some(n)) => {
            format!(
                "Mixed operations ({} op kinds, ~{n} rows) on `{primary}`{extra_tables}",
                operations.len()
            )
        }
        (ScriptType::Mixed, None) => format!(
            "Mixed operations ({} op kinds) on `{primary}`{extra_tables}",
            operations.len()
        ),
    };

    Some(SqlScriptAnalysis {
        file_path: path.to_string(),
        script_type,
        tables_affected: tables,
        operations,
        row_count_estimate,
        purpose_summary,
    })
}

/// Analyse every non-SP SQL file in `sql_files` — those are the post-deploy
/// seed/merge/DDL scripts that carry real business logic on top of the
/// stored-procedure catalog.
fn analyze_post_deploy_scripts(sql_files: &[(String, String)]) -> Vec<SqlScriptAnalysis> {
    let mut out = Vec::new();
    for (path, content) in sql_files {
        if contains_create_procedure(content) {
            continue;
        }
        if let Some(analysis) = analyze_sql_script(path, content) {
            out.push(analysis);
        }
    }
    // Seed scripts with the most rows first, then alphabetically by path so
    // the markdown table is deterministic.
    out.sort_by(|a, b| {
        b.row_count_estimate
            .unwrap_or(0)
            .cmp(&a.row_count_estimate.unwrap_or(0))
            .then_with(|| a.file_path.cmp(&b.file_path))
    });
    out
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

    // Post-deployment scripts (seed/merge/DDL files that are NOT SP defs)
    let sql_scripts = analyze_post_deploy_scripts(sql_files);

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
        sql_scripts,
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
        && intel.sql_scripts.is_empty()
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

    // Post-deployment scripts — seed/merge/DDL files that are not SP bodies.
    if !intel.sql_scripts.is_empty() {
        md.push_str("### Post-Deployment Scripts\n\n");
        md.push_str(
            "| Script | Type | Tables Affected | Operations | Rows |\n|---|---|---|---|---|\n",
        );
        for s in &intel.sql_scripts {
            let tables = if s.tables_affected.is_empty() {
                "—".to_string()
            } else {
                s.tables_affected.join(", ")
            };
            let ops = if s.operations.is_empty() {
                "—".to_string()
            } else {
                s.operations.join(", ")
            };
            let rows = match s.row_count_estimate {
                Some(n) => format!("~{n}"),
                None => "—".to_string(),
            };
            md.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                s.file_path.replace('|', "\\|"),
                s.script_type.as_str(),
                tables.replace('|', "\\|"),
                ops.replace('|', "\\|"),
                rows,
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
            sql_scripts: vec![],
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
            sql_scripts: vec![],
            warnings: vec![],
        };

        let md = render_database_intelligence_markdown(&intel);
        assert!(md.contains("trg_Audit"));
        assert!(md.contains("Orders"));
    }

    // ── Post-deployment script analysis ─────────────────────────────────────

    #[test]
    fn contains_create_procedure_detects_ddl_keyword() {
        assert!(contains_create_procedure(
            "CREATE PROCEDURE [dbo].[usp_Get] AS SELECT 1"
        ));
        assert!(contains_create_procedure(
            "create or alter proc dbo.usp_Foo as select 1"
        ));
        assert!(!contains_create_procedure(
            "INSERT INTO dbo.Settings VALUES (1, 'x')"
        ));
        assert!(!contains_create_procedure(
            "-- CREATE PROCEDURE usp_fake\nINSERT INTO T VALUES (1)"
        ));
    }

    #[test]
    fn clean_table_name_strips_schema_and_brackets() {
        assert_eq!(
            clean_table_name("[dbo].[ss_systemSettings]"),
            "ss_systemSettings"
        );
        assert_eq!(clean_table_name("dbo.Orders"), "Orders");
        assert_eq!(clean_table_name("Orders"), "Orders");
        assert_eq!(clean_table_name("\"dbo\".\"t\""), "t");
    }

    #[test]
    fn analyze_sql_script_classifies_seed_insert() {
        let sql = r#"
            -- seed ref table
            INSERT INTO [dbo].[ss_systemSettings] (id, name) VALUES
                (1, 'alpha'),
                (2, 'beta'),
                (3, 'gamma');
        "#;
        let a =
            analyze_sql_script("Scripts/Post/ss_systemsettings.sql", sql).expect("must analyse");
        assert_eq!(a.script_type, ScriptType::Seed);
        assert_eq!(a.tables_affected, vec!["ss_systemSettings"]);
        assert_eq!(a.operations, vec!["INSERT"]);
        // 3 tuples → 2 boundaries + 1 INSERT statement = 3 rows estimate
        assert_eq!(a.row_count_estimate, Some(3));
        assert!(a.purpose_summary.contains("ss_systemSettings"));
        assert!(a.purpose_summary.contains("~3 rows"));
    }

    #[test]
    fn analyze_sql_script_detects_merge_with_branches() {
        let sql = r#"
            MERGE INTO [dbo].[tpr_timeplanRevision] AS tgt
            USING (SELECT 1 AS id) AS src ON tgt.id = src.id
            WHEN MATCHED THEN UPDATE SET tgt.status = 'active'
            WHEN NOT MATCHED THEN INSERT (id, status) VALUES (src.id, 'active');
        "#;
        let a = analyze_sql_script("Scripts/Post/tpr.sql", sql).expect("must analyse");
        assert_eq!(a.script_type, ScriptType::Seed);
        assert!(a.operations.contains(&"MERGE".to_string()));
        assert_eq!(a.tables_affected, vec!["tpr_timeplanRevision"]);
        assert!(a.row_count_estimate.is_some());
    }

    #[test]
    fn analyze_sql_script_classifies_migration_alter() {
        let sql = "ALTER TABLE dbo.Orders ADD newCol INT NULL;";
        let a = analyze_sql_script("Scripts/Migrate/001.sql", sql).expect("must analyse");
        assert_eq!(a.script_type, ScriptType::Migration);
        assert_eq!(a.tables_affected, vec!["Orders"]);
        assert!(a.operations.contains(&"ALTER".to_string()));
        assert!(a.row_count_estimate.is_none());
    }

    #[test]
    fn analyze_sql_script_classifies_schema_change_index() {
        let sql = "CREATE NONCLUSTERED INDEX IX_Orders_CustomerId ON dbo.Orders (CustomerId);";
        let a = analyze_sql_script("Scripts/Indexes/001.sql", sql).expect("must analyse");
        assert_eq!(a.script_type, ScriptType::SchemaChange);
        assert!(a.operations.contains(&"INDEX".to_string()));
        assert_eq!(a.tables_affected, vec!["Orders"]);
    }

    #[test]
    fn analyze_sql_script_classifies_mixed() {
        let sql = r#"
            ALTER TABLE dbo.Settings ADD flag BIT NOT NULL DEFAULT 0;
            INSERT INTO dbo.Settings (id, flag) VALUES (1, 1), (2, 0);
        "#;
        let a = analyze_sql_script("Scripts/Post/mix.sql", sql).expect("must analyse");
        assert_eq!(a.script_type, ScriptType::Mixed);
        assert!(a.operations.contains(&"ALTER".to_string()));
        assert!(a.operations.contains(&"INSERT".to_string()));
        // Both operations touch the same table, deduped.
        assert_eq!(a.tables_affected, vec!["Settings"]);
    }

    #[test]
    fn analyze_sql_script_returns_none_for_pure_select() {
        let sql = "SELECT * FROM dbo.Orders;";
        assert!(analyze_sql_script("readonly.sql", sql).is_none());
    }

    #[test]
    fn analyze_post_deploy_scripts_skips_sp_definitions() {
        let sp = "CREATE PROCEDURE dbo.usp_Foo AS BEGIN SELECT 1; END";
        let seed = "INSERT INTO dbo.T VALUES (1);";
        let files = vec![
            ("Procs/usp_foo.sql".to_string(), sp.to_string()),
            ("Post/seed.sql".to_string(), seed.to_string()),
        ];
        let out = analyze_post_deploy_scripts(&files);
        assert_eq!(out.len(), 1, "SP definition file must be excluded");
        assert_eq!(out[0].file_path, "Post/seed.sql");
    }

    #[test]
    fn render_markdown_includes_post_deployment_scripts_section() {
        let intel = DatabaseIntelligence {
            sp_logic: vec![],
            sp_call_chains: vec![],
            triggers: vec![],
            schema: SchemaReport::default(),
            sql_scripts: vec![SqlScriptAnalysis {
                file_path: "Post/ss_systemsettings.sql".to_string(),
                script_type: ScriptType::Seed,
                tables_affected: vec!["ss_systemSettings".to_string()],
                operations: vec!["INSERT".to_string()],
                row_count_estimate: Some(195),
                purpose_summary: "Seeds/updates `ss_systemSettings` with ~195 rows".to_string(),
            }],
            warnings: vec![],
        };
        let md = render_database_intelligence_markdown(&intel);
        assert!(md.contains("### Post-Deployment Scripts"));
        assert!(md.contains("ss_systemsettings.sql"));
        assert!(md.contains("Seed"));
        assert!(md.contains("~195"));
    }
}
