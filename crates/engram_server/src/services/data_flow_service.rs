// Ticket 2: Data Flow Tracing Service
//
// Traces the business logic flow starting from an event handler through SQL
// queries, state access, and data binding to understand what a page actually
// DOES. Uses both graph data (SqlCalls, ReadsState, WritesState, DataBinding
// edges) and direct code content parsing for ordered, fine-grained steps.

use engram_graph::{EdgeKind, GraphStore};
use regex::Regex;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

// ── Compiled regexes ─────────────────────────────────────────────────────────

/// Matches `Protected/Private/Public Sub/Function <name>(...)` in VB.NET or
/// `protected/private/public void/... <name>(...)` in C#.
static RE_METHOD_START_CS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(private|protected|public|internal|static|\s)+[\w<>\[\]]+\s+(\w+)\s*\(")
        .expect("RE_METHOD_START_CS")
});

static RE_METHOD_START_VB: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(Private|Protected|Public|Friend|Shared)\s+(Sub|Function)\s+(\w+)\s*\(")
        .expect("RE_METHOD_START_VB")
});

/// `txtName.Text`, `ddlState.SelectedValue`, `chkActive.Checked`, etc.
static RE_CONTROL_READ: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b([a-zA-Z_]\w*)\.(Text|Value|SelectedValue|SelectedItem|Checked|Items|InnerText|InnerHtml)\b",
    )
    .expect("RE_CONTROL_READ")
});

/// `label.Text = ...`, `grid.Visible = ...`, `panel.Enabled = ...`
/// Note: negative lookahead is not supported by the `regex` crate; the `==` exclusion
/// is handled in code after matching.
static RE_CONTROL_WRITE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b([a-zA-Z_]\w*)\.(Text|Visible|Enabled|CssClass|InnerText|InnerHtml|ToolTip|ForeColor|BackColor)\s*=",
    )
    .expect("RE_CONTROL_WRITE")
});

/// Session("key") or Session["key"]
static RE_STATE_READ: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\b(Session|ViewState|Application|Cache)\s*[\(\[]["']?([\w\.]+)["']?[\)\]]"#)
        .expect("RE_STATE_READ")
});

/// Session("key") = ... or ViewState["key"] = ...
/// Note: negative lookahead is not supported by the `regex` crate; the `==` exclusion
/// is handled in code after matching.
static RE_STATE_WRITE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\b(Session|ViewState|Application|Cache)\s*[\(\[]["']?([\w\.]+)["']?[\)\]]\s*="#)
        .expect("RE_STATE_WRITE")
});

/// new SqlCommand, new SqlDataAdapter, .Fill(, .ExecuteReader(), .ExecuteNonQuery(), .ExecuteScalar()
static RE_SQL_SETUP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(new\s+Sql(?:Command|DataAdapter|DataReader)|SqlCommand\s*\()")
        .expect("RE_SQL_SETUP")
});

static RE_SQL_EXECUTE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\.(ExecuteReader|ExecuteNonQuery|ExecuteScalar|Fill|Update)\s*\(")
        .expect("RE_SQL_EXECUTE")
});

/// .DataSource = ..., .DataBind(), .DataBind(
static RE_DATA_BIND: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\.DataSource\s*=|\.DataBind\s*\(\s*\)").expect("RE_DATA_BIND")
});

/// Response.Redirect(...), Server.Transfer(...)
static RE_REDIRECT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(Response\.Redirect|Server\.Transfer)\s*\(\s*([^)]+)\)").expect("RE_REDIRECT")
});

/// Helper method call: BindGrid(), LoadData(), etc. — bare call on its own line
static RE_METHOD_CALL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*([A-Z][a-zA-Z0-9_]+)\s*\(\s*\)\s*;?\s*$").expect("RE_METHOD_CALL")
});

/// If / ElseIf / else conditional
static RE_CONDITIONAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*(If|else\s*if|ElseIf|else)\b").expect("RE_CONDITIONAL"));

// ── Public result structs ─────────────────────────────────────────────────────

/// The full data flow trace for an event handler.
#[derive(Debug, Clone, Serialize)]
pub struct DataFlowTrace {
    /// The event handler name used as entry point (e.g. "btnSearch_Click").
    pub entry_point: String,
    /// Human-readable trigger description.
    pub trigger: String,
    /// Ordered list of steps through the handler logic.
    pub steps: Vec<DataFlowStep>,
    /// DB tables touched (from graph edges + parsed SQL commands).
    pub tables_touched: Vec<String>,
    /// State keys read by this handler.
    pub state_reads: Vec<StateAccessInfo>,
    /// State keys written by this handler.
    pub state_writes: Vec<StateAccessInfo>,
    /// Controls whose values are read (input side).
    pub controls_read: Vec<String>,
    /// Controls whose properties are written (output side).
    pub controls_written: Vec<String>,
    /// Helper methods invoked within the handler.
    pub methods_called: Vec<String>,
    /// Suggested modern pattern equivalent.
    pub modern_flow_hint: String,
}

/// A single ordered step in a data flow trace.
#[derive(Debug, Clone, Serialize)]
pub struct DataFlowStep {
    /// 1-based sequence number reflecting code order.
    pub sequence: usize,
    /// Step kind: "ReadControl", "ReadState", "WriteState", "SqlQuery",
    /// "SqlExecute", "DataBind", "SetControl", "MethodCall", "Redirect",
    /// "Conditional", "GraphEdge".
    pub step_type: String,
    /// Human-readable description of what is happening.
    pub description: String,
    /// What is being read from (control id, state key, table name, …).
    pub source: String,
    /// What is being written to (variable, state key, control id, url, …).
    pub target: String,
    /// Additional free-form context key→value pairs.
    pub details: HashMap<String, String>,
}

/// A state access (read or write) found in the handler.
#[derive(Debug, Clone, Serialize)]
pub struct StateAccessInfo {
    /// "Session", "ViewState", "Application", or "Cache".
    pub state_type: String,
    /// The dictionary key being accessed.
    pub key: String,
    /// "read" or "write".
    pub direction: String,
    /// The enclosing method name (typically the entry point).
    pub method_context: String,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Trace the data flow for a single event handler.
///
/// Combines:
/// 1. Line-by-line parsing of `codebehind_content` to extract ordered steps.
/// 2. Graph edge queries to supplement with SQL/state edges already indexed.
///
/// # Arguments
/// * `graph` — shared graph store (may be empty in unit tests).
/// * `project_id` — project namespace for graph lookups.
/// * `file_path` — the legacy code-behind file path.
/// * `entry_point` — event handler name, e.g. `"btnSearch_Click"`.
/// * `codebehind_content` — full source text of the code-behind file.
pub fn trace_data_flow(
    graph: &Arc<GraphStore>,
    project_id: &str,
    file_path: &str,
    entry_point: &str,
    codebehind_content: &str,
) -> anyhow::Result<DataFlowTrace> {
    let trigger = infer_trigger(entry_point);

    // ── Step 1: extract the method body ──────────────────────────────────────
    let method_body = extract_method_body(codebehind_content, entry_point);

    // ── Step 2: parse code steps line by line ────────────────────────────────
    let mut steps: Vec<DataFlowStep> = Vec::new();
    let mut controls_read: Vec<String> = Vec::new();
    let mut controls_written: Vec<String> = Vec::new();
    let mut state_reads: Vec<StateAccessInfo> = Vec::new();
    let mut state_writes: Vec<StateAccessInfo> = Vec::new();
    let mut methods_called: Vec<String> = Vec::new();
    let mut tables_touched: Vec<String> = Vec::new();

    // Track whether we have seen an active SqlCommand so we can annotate
    // the Execute step with the paired setup.
    let mut pending_sql_source: Option<String> = None;

    for line in method_body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with("'") {
            continue;
        }

        let seq = steps.len() + 1;

        // ── State writes (must come before state reads so that
        //    `Session["x"] = value` is not also matched as a read) ────────────
        // Guard: skip `==` comparisons (regex crate has no negative lookahead).
        if let Some(m) = RE_STATE_WRITE.find(trimmed) {
            let after_eq = trimmed[m.end()..].trim_start();
            let is_double_eq = after_eq.starts_with('=');
            if !is_double_eq
                && let Some(cap) = RE_STATE_WRITE.captures(trimmed) {
                    let state_type = cap[1].to_string();
                    let key = cap[2].to_string();
                    let rhs = trimmed[m.end()..].trim().to_string();

                    let sai = StateAccessInfo {
                        state_type: state_type.clone(),
                        key: key.clone(),
                        direction: "write".into(),
                        method_context: entry_point.to_string(),
                    };
                    // Avoid duplicate entries
                    if !state_writes
                        .iter()
                        .any(|s: &StateAccessInfo| s.key == key && s.state_type == state_type)
                    {
                        state_writes.push(sai);
                    }

                    let mut details = HashMap::new();
                    details.insert("value_expr".into(), rhs.clone());
                    steps.push(DataFlowStep {
                        sequence: seq,
                        step_type: "WriteState".into(),
                        description: format!("Write {state_type}[\"{key}\"] = {rhs}"),
                        source: rhs,
                        target: format!("{state_type}[\"{key}\"]"),
                        details,
                    });
                    continue;
                } // end !is_double_eq
        }

        // ── State reads ───────────────────────────────────────────────────────
        if RE_STATE_READ.is_match(trimmed) && !trimmed.contains("=") {
            for cap in RE_STATE_READ.captures_iter(trimmed) {
                let state_type = cap[1].to_string();
                let key = cap[2].to_string();

                let sai = StateAccessInfo {
                    state_type: state_type.clone(),
                    key: key.clone(),
                    direction: "read".into(),
                    method_context: entry_point.to_string(),
                };
                if !state_reads
                    .iter()
                    .any(|s: &StateAccessInfo| s.key == key && s.state_type == state_type)
                {
                    state_reads.push(sai);
                }

                steps.push(DataFlowStep {
                    sequence: seq,
                    step_type: "ReadState".into(),
                    description: format!("Read {state_type}[\"{key}\"]"),
                    source: format!("{state_type}[\"{key}\"]"),
                    target: "local variable".into(),
                    details: HashMap::new(),
                });
            }
            continue;
        }

        // State read on assignment RHS: `var x = Session["key"]`
        if trimmed.contains('=') && RE_STATE_READ.is_match(trimmed) {
            let lhs = trimmed.split('=').next().unwrap_or("").trim().to_string();
            for cap in RE_STATE_READ.captures_iter(trimmed) {
                let state_type = cap[1].to_string();
                let key = cap[2].to_string();

                let sai = StateAccessInfo {
                    state_type: state_type.clone(),
                    key: key.clone(),
                    direction: "read".into(),
                    method_context: entry_point.to_string(),
                };
                if !state_reads
                    .iter()
                    .any(|s: &StateAccessInfo| s.key == key && s.state_type == state_type)
                {
                    state_reads.push(sai);
                }

                let mut details = HashMap::new();
                details.insert("assigned_to".into(), lhs.clone());
                steps.push(DataFlowStep {
                    sequence: seq,
                    step_type: "ReadState".into(),
                    description: format!("Read {state_type}[\"{key}\"] → {lhs}"),
                    source: format!("{state_type}[\"{key}\"]"),
                    target: lhs.clone(),
                    details,
                });
            }
            continue;
        }

        // ── SQL setup: new SqlCommand, new SqlDataAdapter ─────────────────────
        if RE_SQL_SETUP.is_match(trimmed) {
            // Try to pick out the SQL string literal from the line
            let sql_hint = extract_sql_hint(trimmed);
            let table = extract_table_from_sql_hint(&sql_hint);
            if !table.is_empty() && !tables_touched.contains(&table) {
                tables_touched.push(table.clone());
            }
            pending_sql_source = Some(sql_hint.clone());

            let mut details = HashMap::new();
            if !sql_hint.is_empty() {
                details.insert("sql_hint".into(), sql_hint.clone());
            }
            if !table.is_empty() {
                details.insert("table".into(), table.clone());
            }
            steps.push(DataFlowStep {
                sequence: seq,
                step_type: "SqlQuery".into(),
                description: format!(
                    "Prepare SQL command{}",
                    if table.is_empty() {
                        String::new()
                    } else {
                        format!(" → {table}")
                    }
                ),
                source: sql_hint,
                target: table,
                details,
            });
            continue;
        }

        // ── SQL execution ─────────────────────────────────────────────────────
        if let Some(cap) = RE_SQL_EXECUTE.captures(trimmed) {
            let method = cap[1].to_string();
            let method_lower = method.to_lowercase();
            let sql_src = pending_sql_source.clone().unwrap_or_default();
            let mut details = HashMap::new();
            if !sql_src.is_empty() {
                details.insert("sql_hint".into(), sql_src.clone());
            }
            let step_type = match method_lower.as_str() {
                "fill" => "SqlQuery",
                "executereader" => "SqlQuery",
                _ => "SqlExecute",
            };
            steps.push(DataFlowStep {
                sequence: seq,
                step_type: step_type.into(),
                description: format!("Execute SQL: .{method}()"),
                source: sql_src,
                target: method,
                details,
            });
            // Only clear pending_sql after a final execution
            if matches!(
                method_lower.as_str(),
                "executereader" | "executenonquery" | "executescalar" | "fill"
            ) {
                pending_sql_source = None;
            }
            continue;
        }

        // ── Data binding ──────────────────────────────────────────────────────
        if RE_DATA_BIND.is_match(trimmed) {
            let control = extract_control_id_from_binding(trimmed);
            let lhs = trimmed.split('.').next().unwrap_or("").trim().to_string();
            let is_source_assign = trimmed.to_lowercase().contains("datasource");

            if is_source_assign {
                let rhs = trimmed.split('=').nth(1).unwrap_or("").trim().to_string();
                let mut details = HashMap::new();
                details.insert("data_source".into(), rhs.clone());
                steps.push(DataFlowStep {
                    sequence: seq,
                    step_type: "DataBind".into(),
                    description: format!("Set {}.DataSource = {rhs}", lhs),
                    source: rhs,
                    target: lhs,
                    details,
                });
            } else {
                let mut details = HashMap::new();
                details.insert("control".into(), control.clone());
                steps.push(DataFlowStep {
                    sequence: seq,
                    step_type: "DataBind".into(),
                    description: format!("Call {}.DataBind()", lhs),
                    source: "data source".into(),
                    target: lhs,
                    details,
                });
            }
            continue;
        }

        // ── Redirect ──────────────────────────────────────────────────────────
        if let Some(cap) = RE_REDIRECT.captures(trimmed) {
            let method = cap[1].to_string();
            let url = cap[2]
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string();
            let mut details = HashMap::new();
            details.insert("url".into(), url.clone());
            steps.push(DataFlowStep {
                sequence: seq,
                step_type: "Redirect".into(),
                description: format!("{method}(\"{url}\")"),
                source: entry_point.to_string(),
                target: url,
                details,
            });
            continue;
        }

        // ── Control write (label.Text = ..., grid.Visible = ...) ─────────────
        // Guard against `==` comparisons: the regex matches `=` but not `==`
        // (regex crate has no negative lookahead), so filter in code.
        if RE_CONTROL_WRITE.is_match(trimmed) && !trimmed.contains("==") {
            for cap in RE_CONTROL_WRITE.captures_iter(trimmed) {
                let control = cap[1].to_string();
                let prop = cap[2].to_string();
                // Extract RHS: everything after the first bare `=`
                let rhs = trimmed.split('=').nth(1).unwrap_or("").trim().to_string();

                if !controls_written.contains(&control) {
                    controls_written.push(control.clone());
                }

                let mut details = HashMap::new();
                details.insert("property".into(), prop.clone());
                details.insert("value_expr".into(), rhs.clone());
                steps.push(DataFlowStep {
                    sequence: seq,
                    step_type: "SetControl".into(),
                    description: format!("Set {control}.{prop} = {rhs}"),
                    source: rhs,
                    target: format!("{control}.{prop}"),
                    details,
                });
            }
            continue;
        }

        // ── Control read (only on assignment RHS or standalone expression) ────
        if RE_CONTROL_READ.is_match(trimmed) {
            for cap in RE_CONTROL_READ.captures_iter(trimmed) {
                let control = cap[1].to_string();
                let prop = cap[2].to_string();

                // Skip obviously noise-y identifiers (cmd, conn, da, reader…)
                if matches!(
                    control.to_lowercase().as_str(),
                    "cmd" | "conn" | "da" | "dr" | "reader" | "adapter" | "ds"
                ) {
                    continue;
                }

                if !controls_read.contains(&control) {
                    controls_read.push(control.clone());
                }

                let mut details = HashMap::new();
                details.insert("property".into(), prop.clone());
                steps.push(DataFlowStep {
                    sequence: seq,
                    step_type: "ReadControl".into(),
                    description: format!("Read {control}.{prop}"),
                    source: format!("{control}.{prop}"),
                    target: "local variable".into(),
                    details,
                });
            }
            continue;
        }

        // ── Conditional ───────────────────────────────────────────────────────
        if RE_CONDITIONAL.is_match(trimmed) {
            let condition = trimmed
                .trim_start_matches(|c: char| c.is_alphabetic() || c == ' ')
                .trim()
                .to_string();
            let mut details = HashMap::new();
            details.insert("condition".into(), condition.clone());
            steps.push(DataFlowStep {
                sequence: seq,
                step_type: "Conditional".into(),
                description: format!(
                    "Branch: {}",
                    trimmed
                        .split_whitespace()
                        .take(4)
                        .collect::<Vec<_>>()
                        .join(" ")
                ),
                source: condition,
                target: String::new(),
                details,
            });
            continue;
        }

        // ── Bare method call: BindGrid(), LoadData() … ────────────────────────
        if let Some(cap) = RE_METHOD_CALL.captures(trimmed) {
            let name = cap[1].to_string();
            // Filter out known keywords that regex might catch
            if !matches!(
                name.to_lowercase().as_str(),
                "if" | "else" | "for" | "while" | "return" | "throw" | "catch" | "try"
            ) {
                if !methods_called.contains(&name) {
                    methods_called.push(name.clone());
                }
                steps.push(DataFlowStep {
                    sequence: seq,
                    step_type: "MethodCall".into(),
                    description: format!("Call helper: {name}()"),
                    source: entry_point.to_string(),
                    target: name,
                    details: HashMap::new(),
                });
            }
        }
    }

    // ── Step 3: supplement with graph edges ──────────────────────────────────
    let graph_steps = collect_graph_steps(
        graph,
        project_id,
        file_path,
        entry_point,
        &mut tables_touched,
        &mut state_reads,
        &mut state_writes,
    )?;
    let code_step_count = steps.len();
    for mut gs in graph_steps {
        gs.sequence += code_step_count;
        steps.push(gs);
    }

    // ── Step 4: re-sequence all steps ────────────────────────────────────────
    for (i, step) in steps.iter_mut().enumerate() {
        step.sequence = i + 1;
    }

    // ── Step 5: deduplicate and sort summary lists ────────────────────────────
    controls_read.sort();
    controls_read.dedup();
    controls_written.sort();
    controls_written.dedup();
    methods_called.sort();
    methods_called.dedup();
    tables_touched.sort();
    tables_touched.dedup();

    // ── Step 6: generate modern_flow_hint ────────────────────────────────────
    let modern_flow_hint = generate_flow_hint(
        &steps,
        &controls_read,
        &state_reads,
        &state_writes,
        &tables_touched,
    );

    Ok(DataFlowTrace {
        entry_point: entry_point.to_string(),
        trigger,
        steps,
        tables_touched,
        state_reads,
        state_writes,
        controls_read,
        controls_written,
        methods_called,
        modern_flow_hint,
    })
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Infer a human-readable trigger description from the handler name.
fn infer_trigger(entry_point: &str) -> String {
    if entry_point.eq_ignore_ascii_case("Page_Load") {
        "Page load".into()
    } else if entry_point.eq_ignore_ascii_case("Page_Init") {
        "Page initialization".into()
    } else if entry_point.eq_ignore_ascii_case("Page_PreRender") {
        "Page pre-render".into()
    } else if entry_point.ends_with("_Click") {
        "Button click (postback)".into()
    } else if entry_point.ends_with("_SelectedIndexChanged") {
        "Selection change (postback)".into()
    } else if entry_point.ends_with("_RowCommand") {
        "Grid row command".into()
    } else if entry_point.ends_with("_RowEditing") {
        "Grid row edit".into()
    } else if entry_point.ends_with("_RowUpdating") {
        "Grid row update".into()
    } else if entry_point.ends_with("_RowDeleting") {
        "Grid row delete".into()
    } else if entry_point.ends_with("_PageIndexChanging") {
        "Grid page change".into()
    } else if entry_point.ends_with("_Sorting") {
        "Grid sort".into()
    } else if entry_point.ends_with("_TextChanged") {
        "Text change (postback)".into()
    } else if entry_point.ends_with("_CheckedChanged") {
        "Checkbox change (postback)".into()
    } else if entry_point.ends_with("_Command") {
        "Command event (postback)".into()
    } else if entry_point.ends_with("_Changed") {
        "Value change (postback)".into()
    } else if entry_point.ends_with("_ItemCommand") {
        "Repeater/list item command".into()
    } else if entry_point.ends_with("_DataBound") {
        "Data bound lifecycle event".into()
    } else {
        format!("Event handler: {entry_point}")
    }
}

/// Extract the source lines of the named method from the file content.
///
/// Uses brace depth (C#) or `End Sub/End Function` (VB.NET) to find the
/// method boundary. Falls back to an empty string when not found.
fn extract_method_body(content: &str, method_name: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let is_vb = content.contains("End Sub") || content.contains("End Function");

    if is_vb {
        extract_method_body_vb(&lines, method_name)
    } else {
        extract_method_body_cs(&lines, method_name)
    }
}

fn extract_method_body_cs(lines: &[&str], method_name: &str) -> String {
    let mut start_line = None;

    for (i, line) in lines.iter().enumerate() {
        // Look for a line that names the method and has a `(`
        if line.contains(method_name) && line.contains('(') {
            // Verify it looks like a method declaration via regex
            if RE_METHOD_START_CS.is_match(line) || line.contains(method_name) {
                start_line = Some(i);
                break;
            }
        }
    }

    let start = match start_line {
        Some(s) => s,
        None => return String::new(),
    };

    // Scan forward to find the opening brace then track depth
    let mut depth = 0i32;
    let mut in_body = false;
    let mut body_lines: Vec<&str> = Vec::new();

    for line in &lines[start..] {
        for ch in line.chars() {
            if ch == '{' {
                depth += 1;
                in_body = true;
            } else if ch == '}' {
                depth -= 1;
            }
        }
        if in_body {
            body_lines.push(line);
        }
        if in_body && depth == 0 {
            break;
        }
    }

    body_lines.join("\n")
}

fn extract_method_body_vb(lines: &[&str], method_name: &str) -> String {
    let mut start_line = None;

    for (i, line) in lines.iter().enumerate() {
        if line.contains(method_name) && line.contains('(')
            && (RE_METHOD_START_VB.is_match(line) || line.contains(method_name)) {
                start_line = Some(i);
                break;
            }
    }

    let start = match start_line {
        Some(s) => s,
        None => return String::new(),
    };

    let mut body_lines: Vec<&str> = Vec::new();
    for line in &lines[start..] {
        body_lines.push(line);
        let trimmed = line.trim().to_lowercase();
        if trimmed.starts_with("end sub") || trimmed.starts_with("end function") {
            break;
        }
    }

    body_lines.join("\n")
}

/// Try to pull a SQL string literal or command name out of a single line.
fn extract_sql_hint(line: &str) -> String {
    // Grab first double-quoted string
    if let Some(start) = line.find('"')
        && let Some(end) = line[start + 1..].find('"') {
            let candidate = &line[start + 1..start + 1 + end];
            let upper = candidate.to_uppercase();
            if upper.starts_with("SELECT")
                || upper.starts_with("INSERT")
                || upper.starts_with("UPDATE")
                || upper.starts_with("DELETE")
                || upper.starts_with("EXEC")
                || upper.starts_with("sp_")
                || upper.starts_with("SP_")
            {
                return candidate.to_string();
            }
        }
    // Try single-quoted
    if let Some(start) = line.find('\'')
        && let Some(end) = line[start + 1..].find('\'') {
            let candidate = &line[start + 1..start + 1 + end];
            let upper = candidate.to_uppercase();
            if upper.starts_with("SELECT")
                || upper.starts_with("INSERT")
                || upper.starts_with("UPDATE")
                || upper.starts_with("DELETE")
                || upper.starts_with("EXEC")
            {
                return candidate.to_string();
            }
        }
    String::new()
}

/// Extract table name from a SQL hint string.
fn extract_table_from_sql_hint(sql: &str) -> String {
    let upper = sql.to_uppercase();
    for keyword in &["FROM ", "INTO ", "UPDATE ", "JOIN "] {
        if let Some(pos) = upper.find(keyword) {
            let after = &sql[pos + keyword.len()..];
            let table = after
                .trim()
                .split(|c: char| c.is_whitespace() || c == '(' || c == ';' || c == ',')
                .next()
                .unwrap_or("")
                .trim_matches('[')
                .trim_matches(']');
            if !table.is_empty() {
                return table.to_string();
            }
        }
    }
    String::new()
}

/// Extract the control id from a `.DataSource = ...` or `.DataBind()` line.
fn extract_control_id_from_binding(line: &str) -> String {
    line.split('.').next().unwrap_or("").trim().to_string()
}

/// Query the graph for edges relevant to this entry point and convert them to
/// supplemental DataFlowStep entries.
fn collect_graph_steps(
    graph: &Arc<GraphStore>,
    project_id: &str,
    file_path: &str,
    entry_point: &str,
    tables_touched: &mut Vec<String>,
    state_reads: &mut Vec<StateAccessInfo>,
    state_writes: &mut Vec<StateAccessInfo>,
) -> anyhow::Result<Vec<DataFlowStep>> {
    let mut steps: Vec<DataFlowStep> = Vec::new();
    let mut seq = 1usize;

    // Helper: is an edge relevant to this handler?
    let relevant = |source_id: &str| -> bool {
        source_id.contains(entry_point) || source_id.contains(file_path)
    };

    // SqlCalls
    let sql_edges = graph.list_edges_by_kind(project_id, EdgeKind::SqlCalls, 10_000)?;
    for edge in &sql_edges {
        if !relevant(&edge.source_id) {
            continue;
        }
        let sql_text = edge
            .metadata
            .as_ref()
            .and_then(|m| m.get("sql").or_else(|| m.get("command_text")))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let table = edge
            .target_id
            .strip_prefix("table:")
            .unwrap_or(&edge.target_id)
            .to_string();

        if !table.is_empty() && !tables_touched.contains(&table) {
            tables_touched.push(table.clone());
        }

        let mut details = HashMap::new();
        if !sql_text.is_empty() {
            details.insert("sql".into(), sql_text.clone());
        }
        details.insert("source".into(), "graph_edge".into());

        steps.push(DataFlowStep {
            sequence: seq,
            step_type: "GraphEdge".into(),
            description: format!(
                "Graph: SQL call → {}{}",
                table,
                if sql_text.is_empty() {
                    String::new()
                } else {
                    format!(" [{sql_text:.40}]")
                }
            ),
            source: sql_text,
            target: table,
            details,
        });
        seq += 1;
    }

    // ReadsState
    let rs_edges = graph.list_edges_by_kind(project_id, EdgeKind::ReadsState, 10_000)?;
    for edge in &rs_edges {
        if !relevant(&edge.source_id) {
            continue;
        }
        let (state_type, key) = parse_state_target(&edge.target_id);

        let sai = StateAccessInfo {
            state_type: state_type.clone(),
            key: key.clone(),
            direction: "read".into(),
            method_context: entry_point.to_string(),
        };
        if !state_reads
            .iter()
            .any(|s| s.key == key && s.state_type == state_type)
        {
            state_reads.push(sai);
        }

        let mut details = HashMap::new();
        details.insert("source".into(), "graph_edge".into());
        steps.push(DataFlowStep {
            sequence: seq,
            step_type: "GraphEdge".into(),
            description: format!("Graph: reads {state_type}[\"{key}\"]"),
            source: format!("{state_type}[\"{key}\"]"),
            target: "local variable".into(),
            details,
        });
        seq += 1;
    }

    // WritesState
    let ws_edges = graph.list_edges_by_kind(project_id, EdgeKind::WritesState, 10_000)?;
    for edge in &ws_edges {
        if !relevant(&edge.source_id) {
            continue;
        }
        let (state_type, key) = parse_state_target(&edge.target_id);

        let sai = StateAccessInfo {
            state_type: state_type.clone(),
            key: key.clone(),
            direction: "write".into(),
            method_context: entry_point.to_string(),
        };
        if !state_writes
            .iter()
            .any(|s| s.key == key && s.state_type == state_type)
        {
            state_writes.push(sai);
        }

        let mut details = HashMap::new();
        details.insert("source".into(), "graph_edge".into());
        steps.push(DataFlowStep {
            sequence: seq,
            step_type: "GraphEdge".into(),
            description: format!("Graph: writes {state_type}[\"{key}\"]"),
            source: "value".into(),
            target: format!("{state_type}[\"{key}\"]"),
            details,
        });
        seq += 1;
    }

    // DataBinding edges
    let db_edges = graph.list_edges_by_kind(project_id, EdgeKind::DataBinding, 10_000)?;
    for edge in &db_edges {
        if !relevant(&edge.source_id) {
            continue;
        }
        let control = edge
            .target_id
            .strip_prefix("control:")
            .unwrap_or(&edge.target_id)
            .to_string();

        let mut details = HashMap::new();
        details.insert("source".into(), "graph_edge".into());
        steps.push(DataFlowStep {
            sequence: seq,
            step_type: "GraphEdge".into(),
            description: format!("Graph: data binding → {control}"),
            source: "data source".into(),
            target: control,
            details,
        });
        seq += 1;
    }

    Ok(steps)
}

/// Parse a state edge target_id like "state:Session:UserName" into
/// ("Session", "UserName").
fn parse_state_target(target_id: &str) -> (String, String) {
    let stripped = target_id.strip_prefix("state:").unwrap_or(target_id);
    // Try "<Type>:<Key>" split
    let lower = stripped.to_lowercase();
    for prefix in &["session:", "viewstate:", "application:", "cache:"] {
        if lower.starts_with(prefix) {
            let kind = prefix.trim_end_matches(':');
            let key = &stripped[prefix.len()..];
            return (
                capitalize_first(kind),
                key.trim_matches(|c: char| c == '"' || c == '\'')
                    .to_string(),
            );
        }
    }
    // Fallback: treat as Session key
    ("Session".into(), stripped.to_string())
}

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().to_string() + chars.as_str(),
    }
}

/// Generate a modern flow hint based on the detected step pattern.
fn generate_flow_hint(
    steps: &[DataFlowStep],
    controls_read: &[String],
    state_reads: &[StateAccessInfo],
    state_writes: &[StateAccessInfo],
    tables_touched: &[String],
) -> String {
    let has_sql_query = steps.iter().any(|s| {
        s.step_type == "SqlQuery" || s.step_type == "GraphEdge" && s.description.contains("SQL")
    });
    let has_data_bind = steps.iter().any(|s| s.step_type == "DataBind");
    let has_redirect = steps.iter().any(|s| s.step_type == "Redirect");
    let has_state = !state_reads.is_empty() || !state_writes.is_empty();
    let has_control_reads = !controls_read.is_empty();

    // INSERT/UPDATE detection: SqlExecute steps that do not come from a SELECT
    let has_insert = steps.iter().any(|s| {
        s.source.to_uppercase().contains("INSERT")
            || s.description.to_uppercase().contains("INSERT")
            || (s.step_type == "SqlExecute" && !s.source.to_uppercase().contains("SELECT"))
    });
    let has_update = steps.iter().any(|s| {
        s.source.to_uppercase().contains("UPDATE")
            || s.description.to_uppercase().contains("UPDATE")
    });

    // Pattern: pure navigation
    if has_redirect && !has_sql_query && !has_data_bind {
        return "Navigation pattern: replace Response.Redirect with NavigationManager.NavigateTo() \
                in Blazor or router.push() in React/Angular."
            .into();
    }

    // Pattern: search-and-bind (read controls → SQL SELECT → DataBind)
    if has_control_reads && has_sql_query && has_data_bind {
        let table_hint = if tables_touched.is_empty() {
            String::new()
        } else {
            format!(" (tables: {})", tables_touched.join(", "))
        };
        return format!(
            "Search-and-bind pattern{table_hint}: inject IRepository, call SearchAsync() with \
             parameters from bound component properties, bind results to component state and \
             trigger re-render (StateHasChanged in Blazor / useState in React)."
        );
    }

    // Pattern: form submit → INSERT or UPDATE
    if has_control_reads && (has_insert || has_update) {
        let op = if has_insert { "insert" } else { "update" };
        return format!(
            "Form submit pattern ({op}): bind model properties from form input components, \
             validate with DataAnnotations or FluentValidation, call SaveAsync() / UpdateAsync() \
             via injected IRepository, redirect or show confirmation on success."
        );
    }

    // Pattern: load SQL + bind (no control reads — page load scenario)
    if has_sql_query && has_data_bind && !has_control_reads {
        return "Data load pattern: on OnInitializedAsync / ngOnInit / useEffect, call \
                GetAllAsync() via injected IRepository and bind result to component state."
            .into();
    }

    // Pattern: state read/write (Session manipulation)
    if has_state && !has_sql_query {
        let session_keys: Vec<_> = state_reads
            .iter()
            .chain(state_writes.iter())
            .filter(|s| s.state_type == "Session")
            .map(|s| s.key.as_str())
            .collect();
        let key_hint = if session_keys.is_empty() {
            String::new()
        } else {
            format!(" (keys: {})", session_keys.join(", "))
        };
        return format!(
            "State management pattern{key_hint}: replace Session with component state \
             (@code private T field) for page-scoped data, or IDistributedCache / JWT claims \
             for cross-request / cross-server data. Consider IMemoryCache for single-server."
        );
    }

    // Pattern: method call delegation
    if !steps.iter().filter(|s| s.step_type == "MethodCall").count() == 0 {
        let callee: Vec<_> = steps
            .iter()
            .filter(|s| s.step_type == "MethodCall")
            .map(|s| s.target.as_str())
            .collect();
        return format!(
            "Delegation pattern: handler delegates to helper(s) [{}]. Extract each helper \
             as a private service method or inject dedicated service classes.",
            callee.join(", ")
        );
    }

    // Fallback
    "General handler pattern: identify reads (controls, state), data transformations \
     (SQL, service calls), and writes (state, controls, redirect). Map each to async \
     service calls with injected repositories and component state."
        .into()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use engram_graph::{Edge, EdgeKind, GraphStore};
    use std::sync::Arc;
    use tempfile::tempdir;

    fn make_graph() -> Arc<GraphStore> {
        let dir = tempdir().unwrap();
        Arc::new(GraphStore::open(&dir.path().join("graph.db")).unwrap())
    }

    fn make_edge(source: &str, target: &str, kind: EdgeKind, meta_json: Option<&str>) -> Edge {
        Edge {
            source_id: source.into(),
            target_id: target.into(),
            namespace: "test".into(),
            language: "csharp".into(),
            edge_kind: kind,
            weight: 1,
            generation: 1,
            metadata: meta_json.map(|s| serde_json::from_str(s).unwrap_or_default()),
            updated_at_ms: 0,
        }
    }

    // ── Test 1: Search-and-bind pattern ──────────────────────────────────────

    #[test]
    fn search_and_bind_pattern() {
        let cs = r#"
protected void btnSearch_Click(object sender, EventArgs e)
{
    string keyword = txtKeyword.Text;
    string dept = ddlDept.SelectedValue;

    SqlCommand cmd = new SqlCommand("SELECT * FROM Employees WHERE Name LIKE @kw AND Dept = @d", conn);
    cmd.Parameters.AddWithValue("@kw", "%" + keyword + "%");
    cmd.Parameters.AddWithValue("@d", dept);
    SqlDataAdapter da = new SqlDataAdapter(cmd);
    DataTable dt = new DataTable();
    da.Fill(dt);

    grdResults.DataSource = dt;
    grdResults.DataBind();

    lblCount.Text = dt.Rows.Count.ToString();
}
"#;
        let graph = make_graph();
        let trace = trace_data_flow(&graph, "proj", "Search.aspx.cs", "btnSearch_Click", cs)
            .expect("trace ok");

        assert_eq!(trace.entry_point, "btnSearch_Click");
        assert_eq!(trace.trigger, "Button click (postback)");

        // Controls read
        assert!(
            trace.controls_read.contains(&"txtKeyword".to_string())
                || trace.controls_read.contains(&"ddlDept".to_string()),
            "expected control reads, got {:?}",
            trace.controls_read
        );

        // Tables touched
        assert!(
            trace.tables_touched.contains(&"Employees".to_string()),
            "expected Employees in tables_touched, got {:?}",
            trace.tables_touched
        );

        // Controls written
        assert!(
            trace.controls_written.contains(&"lblCount".to_string()),
            "expected lblCount written, got {:?}",
            trace.controls_written
        );

        // Modern hint should mention search-and-bind or IRepository
        assert!(
            trace.modern_flow_hint.to_lowercase().contains("search")
                || trace.modern_flow_hint.to_lowercase().contains("repository")
                || trace.modern_flow_hint.to_lowercase().contains("bind"),
            "unexpected hint: {}",
            trace.modern_flow_hint
        );

        // Steps should be in logical order: ReadControl → SqlQuery → DataBind → SetControl
        let types: Vec<&str> = trace.steps.iter().map(|s| s.step_type.as_str()).collect();
        let has_read_ctrl = types.contains(&"ReadControl");
        let has_sql = types.contains(&"SqlQuery");
        let has_bind = types.contains(&"DataBind");
        assert!(has_read_ctrl, "missing ReadControl step");
        assert!(has_sql, "missing SqlQuery step");
        assert!(has_bind, "missing DataBind step");
    }

    // ── Test 2: Form submission pattern (INSERT) ──────────────────────────────

    #[test]
    fn form_submit_insert_pattern() {
        let cs = r#"
protected void btnSave_Click(object sender, EventArgs e)
{
    string name = txtName.Text;
    string email = txtEmail.Text;
    string role = ddlRole.SelectedValue;

    SqlCommand cmd = new SqlCommand("INSERT INTO Users (Name, Email, Role) VALUES (@n, @e, @r)", conn);
    cmd.Parameters.AddWithValue("@n", name);
    cmd.Parameters.AddWithValue("@e", email);
    cmd.Parameters.AddWithValue("@r", role);
    cmd.ExecuteNonQuery();

    lblStatus.Text = "User saved.";
    Response.Redirect("UserList.aspx");
}
"#;
        let graph = make_graph();
        let trace = trace_data_flow(&graph, "proj", "Users.aspx.cs", "btnSave_Click", cs)
            .expect("trace ok");

        assert_eq!(trace.trigger, "Button click (postback)");

        // Should detect control reads
        assert!(!trace.controls_read.is_empty(), "expected control reads");

        // Table should be detected
        assert!(
            trace.tables_touched.contains(&"Users".to_string()),
            "expected Users table, got {:?}",
            trace.tables_touched
        );

        // Should have a Redirect step
        let has_redirect = trace.steps.iter().any(|s| s.step_type == "Redirect");
        assert!(has_redirect, "expected Redirect step");

        // Hint should mention form submit or insert/save
        assert!(
            trace.modern_flow_hint.to_lowercase().contains("form")
                || trace.modern_flow_hint.to_lowercase().contains("submit")
                || trace.modern_flow_hint.to_lowercase().contains("insert")
                || trace.modern_flow_hint.to_lowercase().contains("save"),
            "unexpected hint: {}",
            trace.modern_flow_hint
        );
    }

    // ── Test 3: Navigation / redirect-only pattern ────────────────────────────

    #[test]
    fn redirect_only_pattern() {
        let cs = r#"
protected void btnCancel_Click(object sender, EventArgs e)
{
    Response.Redirect("Home.aspx");
}
"#;
        let graph = make_graph();
        let trace = trace_data_flow(&graph, "proj", "Form.aspx.cs", "btnCancel_Click", cs)
            .expect("trace ok");

        assert_eq!(trace.trigger, "Button click (postback)");

        let has_redirect = trace.steps.iter().any(|s| s.step_type == "Redirect");
        assert!(has_redirect, "expected Redirect step");

        let redirect_step = trace
            .steps
            .iter()
            .find(|s| s.step_type == "Redirect")
            .unwrap();
        assert!(
            redirect_step.target.contains("Home"),
            "target should contain Home.aspx, got {}",
            redirect_step.target
        );

        assert!(
            trace.modern_flow_hint.to_lowercase().contains("navigat"),
            "hint should mention navigation, got: {}",
            trace.modern_flow_hint
        );
    }

    // ── Test 4: State read / write pattern ────────────────────────────────────

    #[test]
    fn state_read_write_pattern() {
        let cs = r#"
protected void Page_Load(object sender, EventArgs e)
{
    string userId = Session["UserId"].ToString();
    string role = Session["UserRole"].ToString();

    if (!IsPostBack)
    {
        ViewState["Filter"] = "All";
    }

    Session["LastPage"] = Request.Url.ToString();
    lblWelcome.Text = "Hello " + userId;
}
"#;
        let graph = make_graph();
        let trace =
            trace_data_flow(&graph, "proj", "Default.aspx.cs", "Page_Load", cs).expect("trace ok");

        assert_eq!(trace.trigger, "Page load");

        // State reads
        assert!(
            trace.state_reads.iter().any(|s| s.key == "UserId"),
            "expected UserId in state_reads, got {:?}",
            trace.state_reads
        );
        assert!(
            trace.state_reads.iter().any(|s| s.key == "UserRole"),
            "expected UserRole in state_reads"
        );

        // State writes
        assert!(
            trace
                .state_writes
                .iter()
                .any(|s| s.key == "LastPage" || s.key == "Filter"),
            "expected state write, got {:?}",
            trace.state_writes
        );

        // All accesses should have correct direction
        for r in &trace.state_reads {
            assert_eq!(r.direction, "read");
            assert_eq!(r.method_context, "Page_Load");
        }
        for w in &trace.state_writes {
            assert_eq!(w.direction, "write");
        }

        // Hint should mention state management
        assert!(
            trace.modern_flow_hint.to_lowercase().contains("state")
                || trace.modern_flow_hint.to_lowercase().contains("session"),
            "unexpected hint: {}",
            trace.modern_flow_hint
        );
    }

    // ── Test 5: Method call chaining pattern ──────────────────────────────────

    #[test]
    fn method_call_chaining() {
        let cs = r#"
protected void ddlCategory_SelectedIndexChanged(object sender, EventArgs e)
{
    ClearResults();
    LoadProducts();
    BindGrid();
}
"#;
        let graph = make_graph();
        let trace = trace_data_flow(
            &graph,
            "proj",
            "Products.aspx.cs",
            "ddlCategory_SelectedIndexChanged",
            cs,
        )
        .expect("trace ok");

        assert_eq!(trace.trigger, "Selection change (postback)");

        assert!(
            trace.methods_called.contains(&"ClearResults".to_string())
                || trace.methods_called.contains(&"LoadProducts".to_string())
                || trace.methods_called.contains(&"BindGrid".to_string()),
            "expected method calls, got {:?}",
            trace.methods_called
        );

        let method_steps: Vec<_> = trace
            .steps
            .iter()
            .filter(|s| s.step_type == "MethodCall")
            .collect();
        assert!(!method_steps.is_empty(), "expected MethodCall steps");
    }

    // ── Test 6: Empty handler ─────────────────────────────────────────────────

    #[test]
    fn empty_handler() {
        let cs = r#"
protected void btnDummy_Click(object sender, EventArgs e)
{
}
"#;
        let graph = make_graph();
        let trace = trace_data_flow(&graph, "proj", "Page.aspx.cs", "btnDummy_Click", cs)
            .expect("trace ok");

        assert_eq!(trace.entry_point, "btnDummy_Click");
        assert!(trace.controls_read.is_empty());
        assert!(trace.controls_written.is_empty());
        assert!(trace.state_reads.is_empty());
        assert!(trace.state_writes.is_empty());
        assert!(trace.tables_touched.is_empty());
        assert!(trace.methods_called.is_empty());
        // Steps come from code parse only (none in empty body) + graph (empty graph)
        assert!(trace.steps.is_empty());
    }

    // ── Test 7: Graph edge integration ───────────────────────────────────────

    #[test]
    fn graph_edge_integration() {
        let dir = tempdir().unwrap();
        let graph = Arc::new(GraphStore::open(&dir.path().join("g.db")).unwrap());

        // Insert a SqlCalls edge referencing the handler and file.
        // Use metadata: None — bincode (used by GraphStore) does not support
        // deserializing serde_json::Value via deserialize_any, so we rely on
        // target_id for the table name.
        let sql_edge = make_edge(
            "fn:Search.aspx.cs:btnSearch_Click",
            "table:Orders",
            EdgeKind::SqlCalls,
            None,
        );
        graph
            .upsert_edges("proj", &[sql_edge])
            .expect("upsert sql edge");

        // Insert a WritesState edge
        let ws_edge = make_edge(
            "fn:Search.aspx.cs:btnSearch_Click",
            "state:Session:LastSearch",
            EdgeKind::WritesState,
            None,
        );
        graph
            .upsert_edges("proj", &[ws_edge])
            .expect("upsert state edge");

        // Minimal code (no inline SQL/state — comes purely from graph)
        let cs = r#"
protected void btnSearch_Click(object sender, EventArgs e)
{
    // logic delegated
}
"#;
        let trace = trace_data_flow(&graph, "proj", "Search.aspx.cs", "btnSearch_Click", cs)
            .expect("trace ok");

        // Graph edge should have contributed a table
        assert!(
            trace.tables_touched.contains(&"Orders".to_string()),
            "expected Orders from graph edge, got {:?}",
            trace.tables_touched
        );

        // Graph edge should have contributed a state write
        assert!(
            trace
                .state_writes
                .iter()
                .any(|s| s.key == "LastSearch" || s.key.contains("LastSearch")),
            "expected LastSearch in state_writes, got {:?}",
            trace.state_writes
        );

        // Steps should include at least one GraphEdge step
        let has_graph_step = trace.steps.iter().any(|s| s.step_type == "GraphEdge");
        assert!(has_graph_step, "expected GraphEdge step");
    }

    // ── Test 8: Output structure / serialization ──────────────────────────────

    #[test]
    fn output_format_serializes_cleanly() {
        let cs = r#"
protected void btnLoad_Click(object sender, EventArgs e)
{
    string filter = txtFilter.Text;
    SqlCommand cmd = new SqlCommand("SELECT Id, Name FROM Products WHERE Category = @cat", conn);
    cmd.Parameters.AddWithValue("@cat", filter);
    SqlDataAdapter da = new SqlDataAdapter(cmd);
    DataTable dt = new DataTable();
    da.Fill(dt);
    grdProducts.DataSource = dt;
    grdProducts.DataBind();
    lblTotal.Text = dt.Rows.Count.ToString() + " items";
    Session["LastFilter"] = filter;
}
"#;
        let graph = make_graph();
        let trace = trace_data_flow(&graph, "proj", "Products.aspx.cs", "btnLoad_Click", cs)
            .expect("trace ok");

        // Must serialize without error
        let json = serde_json::to_string_pretty(&trace).expect("serialize");
        assert!(json.contains("\"entry_point\""));
        assert!(json.contains("\"trigger\""));
        assert!(json.contains("\"steps\""));
        assert!(json.contains("\"tables_touched\""));
        assert!(json.contains("\"state_reads\""));
        assert!(json.contains("\"state_writes\""));
        assert!(json.contains("\"controls_read\""));
        assert!(json.contains("\"controls_written\""));
        assert!(json.contains("\"methods_called\""));
        assert!(json.contains("\"modern_flow_hint\""));

        // Sequence numbers should be 1-based and monotonically increasing
        for (i, step) in trace.steps.iter().enumerate() {
            assert_eq!(step.sequence, i + 1);
        }

        // Verify state write captured
        assert!(
            trace.state_writes.iter().any(|s| s.key == "LastFilter"),
            "expected LastFilter state write"
        );
    }

    // ── Additional: trigger inference ─────────────────────────────────────────

    #[test]
    fn trigger_inference_variants() {
        assert_eq!(infer_trigger("Page_Load"), "Page load");
        assert_eq!(infer_trigger("btnOk_Click"), "Button click (postback)");
        assert_eq!(
            infer_trigger("ddlState_SelectedIndexChanged"),
            "Selection change (postback)"
        );
        assert_eq!(infer_trigger("grd_RowCommand"), "Grid row command");
        assert_eq!(infer_trigger("grd_PageIndexChanging"), "Grid page change");
        assert_eq!(infer_trigger("grd_Sorting"), "Grid sort");
        assert_eq!(
            infer_trigger("chkActive_CheckedChanged"),
            "Checkbox change (postback)"
        );
        assert_eq!(
            infer_trigger("SomeCustomHandler"),
            "Event handler: SomeCustomHandler"
        );
    }

    // ── Additional: state target parsing ──────────────────────────────────────

    #[test]
    fn state_target_parse_variants() {
        let (t, k) = parse_state_target("state:Session:UserId");
        assert_eq!(t, "Session");
        assert_eq!(k, "UserId");

        let (t, k) = parse_state_target("state:ViewState:SortColumn");
        assert_eq!(t, "Viewstate");
        assert_eq!(k, "SortColumn");

        let (t, k) = parse_state_target("state:Application:AppVersion");
        assert_eq!(t, "Application");
        assert_eq!(k, "AppVersion");

        let (t, k) = parse_state_target("state:Cache:ProductList");
        assert_eq!(t, "Cache");
        assert_eq!(k, "ProductList");

        // Plain key falls back to Session
        let (t, k) = parse_state_target("state:SomeKey");
        assert_eq!(t, "Session");
        assert_eq!(k, "SomeKey");
    }
}
