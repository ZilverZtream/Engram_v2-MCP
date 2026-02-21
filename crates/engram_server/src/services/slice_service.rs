//! Logical slicing for code chunks.
//!
//! Filters code content by method category so that agents can request
//! only the subset relevant to their current task (e.g. UI methods,
//! data access, event handlers) without consuming full context windows.

use std::sync::OnceLock;

use regex::Regex;

fn get_re(lock: &'static OnceLock<Regex>, pattern: &str) -> Option<&'static Regex> {
    if let Some(re) = lock.get() {
        return Some(re);
    }
    match Regex::new(pattern) {
        Ok(re) => Some(lock.get_or_init(|| re)),
        Err(e) => {
            tracing::error!("slice_service regex init: {e}");
            None
        }
    }
}

// ── Regex statics ──────────────────────────────────────────────────────────

static EVENT_HANDLER_RE: OnceLock<Regex> = OnceLock::new();
static UI_METHOD_RE: OnceLock<Regex> = OnceLock::new();
static DATA_METHOD_RE: OnceLock<Regex> = OnceLock::new();
static SQL_QUERY_RE: OnceLock<Regex> = OnceLock::new();
static STATE_ACCESS_RE: OnceLock<Regex> = OnceLock::new();

// ── Public API ─────────────────────────────────────────────────────────────

/// Apply a logical slice to source code, returning only the relevant lines.
///
/// `slice_type` values:
/// - `"event_handlers"` — ASP.NET event handler methods (Page_Load, Button_Click, etc.)
/// - `"ui_methods"` — UI manipulation methods (Response.Write, FindControl, etc.)
/// - `"data_methods"` — Data access methods (SqlCommand, DataReader, DataAdapter, etc.)
/// - `"sql_queries"` — Lines containing SQL statements
/// - `"state_access"` — Session, ViewState, Application state access
/// - `"all"` — No filtering, return everything
///
/// If the slice type doesn't match any known category, returns the full content.
pub fn apply_logical_slice(content: &str, slice_type: &str, _language: &str) -> String {
    match slice_type {
        "all" | "" => content.to_string(),
        "event_handlers" => slice_event_handlers(content),
        "ui_methods" => slice_ui_methods(content),
        "data_methods" => slice_data_methods(content),
        "sql_queries" => slice_sql_queries(content),
        "state_access" => slice_state_access(content),
        _ => content.to_string(),
    }
}

/// List available slice types with descriptions.
pub fn available_slices() -> &'static [(&'static str, &'static str)] {
    &[
        ("all", "No filtering — return everything"),
        (
            "event_handlers",
            "ASP.NET event handler methods (Page_Load, Button_Click, etc.)",
        ),
        (
            "ui_methods",
            "UI manipulation (Response.Write, FindControl, Controls.Add, etc.)",
        ),
        (
            "data_methods",
            "Data access (SqlCommand, DataReader, DataAdapter, etc.)",
        ),
        (
            "sql_queries",
            "Lines containing SQL statements (SELECT, INSERT, UPDATE, DELETE, EXEC)",
        ),
        (
            "state_access",
            "Session, ViewState, Application, Cache, Context state access",
        ),
    ]
}

// ── Slice implementations ──────────────────────────────────────────────────

fn slice_event_handlers(content: &str) -> String {
    let re = get_re(
        &EVENT_HANDLER_RE,
        r"(?i)(Sub|Function|Protected\s+Sub|Private\s+Sub|Protected\s+Overrides\s+Sub|void|protected\s+void)\s+\w+_(Load|Click|Init|PreRender|Unload|DataBound|SelectedIndexChanged|TextChanged|Command|RowCommand|RowDataBound|ItemDataBound|PageIndexChanging|Sorting|RowEditing|RowUpdating|RowDeleting|RowCreated)\b",
    );
    extract_method_blocks(content, re)
}

fn slice_ui_methods(content: &str) -> String {
    let re = get_re(
        &UI_METHOD_RE,
        r"(?i)(Response\.Write|FindControl|Controls\.Add|Page\.Title|Master\.|ContentPlaceHolder|Panel\.\w+|Label\.\w+|TextBox\.\w+|GridView\.\w+|DropDownList\.\w+|Literal\.\w+|Visible\s*=|Enabled\s*=|CssClass\s*=)",
    );
    extract_matching_context(content, re)
}

fn slice_data_methods(content: &str) -> String {
    let re = get_re(
        &DATA_METHOD_RE,
        r"(?i)(SqlCommand|SqlConnection|SqlDataAdapter|SqlDataReader|DataTable|DataSet|DataRow|ExecuteReader|ExecuteNonQuery|ExecuteScalar|OleDbCommand|OleDbConnection|DbCommand|DbConnection|EntityFramework|DbContext|\.SaveChanges|\.ToList\(\)|\.FirstOrDefault\(\))",
    );
    extract_matching_context(content, re)
}

fn slice_sql_queries(content: &str) -> String {
    let re = get_re(
        &SQL_QUERY_RE,
        r"(?i)(SELECT\s+.+FROM|INSERT\s+INTO|UPDATE\s+\w+\s+SET|DELETE\s+FROM|EXEC\s+\w+|CREATE\s+PROCEDURE|ALTER\s+TABLE|CREATE\s+TABLE|DROP\s+TABLE)",
    );
    extract_matching_context(content, re)
}

fn slice_state_access(content: &str) -> String {
    let re = get_re(
        &STATE_ACCESS_RE,
        r#"(?i)(Session\s*[\[\(]|ViewState\s*[\[\(]|Application\s*[\[\(]|Cache\s*[\[\(]|Context\.Items\s*[\[\(]|HttpContext\.Current\.(Session|Application|Cache|Items))"#,
    );
    extract_matching_context(content, re)
}

// ── Helpers ────────────────────────────────────────────────────────────────

/// Extract complete method blocks whose signature matches `re`.
///
/// Scans for lines matching the regex, then captures forward until the
/// matching `End Sub`/`End Function`/closing brace is found.
fn extract_method_blocks(content: &str, re: Option<&Regex>) -> String {
    let Some(re) = re else {
        return content.to_string();
    };

    let lines: Vec<&str> = content.lines().collect();
    let mut result = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        if re.is_match(lines[i]) {
            // Capture the method block
            let start = i;
            let mut depth = 0i32;
            let mut found_end = false;

            for j in i..lines.len() {
                let trimmed = lines[j].trim().to_lowercase();

                // VB-style blocks
                if trimmed.starts_with("sub ")
                    || trimmed.starts_with("function ")
                    || trimmed.starts_with("protected sub")
                    || trimmed.starts_with("private sub")
                    || trimmed.starts_with("protected overrides sub")
                {
                    depth += 1;
                } else if trimmed.starts_with("end sub") || trimmed.starts_with("end function") {
                    depth -= 1;
                    if depth <= 0 {
                        for line in &lines[start..=j] {
                            result.push(*line);
                        }
                        result.push(""); // blank separator
                        i = j + 1;
                        found_end = true;
                        break;
                    }
                }

                // C#-style blocks
                if trimmed.contains('{') {
                    depth += trimmed.matches('{').count() as i32;
                }
                if trimmed.contains('}') {
                    depth -= trimmed.matches('}').count() as i32;
                    if depth <= 0 && j > start {
                        for line in &lines[start..=j] {
                            result.push(*line);
                        }
                        result.push("");
                        i = j + 1;
                        found_end = true;
                        break;
                    }
                }
            }

            if !found_end {
                // Couldn't find end — include everything from match to end
                for line in &lines[start..] {
                    result.push(*line);
                }
                break;
            }
        } else {
            i += 1;
        }
    }

    if result.is_empty() {
        "(no event handler methods found in this chunk)".to_string()
    } else {
        result.join("\n")
    }
}

/// Extract lines matching `re` with surrounding context (2 lines before/after).
fn extract_matching_context(content: &str, re: Option<&Regex>) -> String {
    let Some(re) = re else {
        return content.to_string();
    };

    let lines: Vec<&str> = content.lines().collect();
    let mut included = vec![false; lines.len()];

    for (i, line) in lines.iter().enumerate() {
        if re.is_match(line) {
            let start = i.saturating_sub(2);
            let end = (i + 3).min(lines.len());
            for flag in &mut included[start..end] {
                *flag = true;
            }
        }
    }

    let mut result = Vec::new();
    let mut in_block = false;

    for (i, line) in lines.iter().enumerate() {
        if included[i] {
            if !in_block && !result.is_empty() {
                result.push("  ..."); // gap indicator
            }
            result.push(*line);
            in_block = true;
        } else {
            in_block = false;
        }
    }

    if result.is_empty() {
        format!("(no matching lines found for this slice type in this chunk)")
    } else {
        result.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_slice_returns_everything() {
        let content = "line 1\nline 2\nline 3";
        assert_eq!(apply_logical_slice(content, "all", "vb"), content);
        assert_eq!(apply_logical_slice(content, "", "vb"), content);
    }

    #[test]
    fn test_event_handler_slice_vb() {
        let content = r#"Imports System

Protected Sub Page_Load(ByVal sender As Object, ByVal e As EventArgs)
    If Not IsPostBack Then
        BindGrid()
    End If
End Sub

Private Sub HelperMethod()
    ' not an event handler
End Sub

Protected Sub btnSave_Click(ByVal sender As Object, ByVal e As EventArgs)
    SaveData()
End Sub
"#;
        let sliced = apply_logical_slice(content, "event_handlers", "vb");
        assert!(sliced.contains("Page_Load"), "Should include Page_Load");
        assert!(
            sliced.contains("btnSave_Click"),
            "Should include btnSave_Click"
        );
        assert!(
            !sliced.contains("HelperMethod"),
            "Should NOT include HelperMethod"
        );
    }

    #[test]
    fn test_data_methods_slice() {
        let content = r#"Dim conn As New SqlConnection(connStr)
Dim cmd As New SqlCommand("SELECT * FROM Users", conn)
conn.Open()
Dim reader = cmd.ExecuteReader()
While reader.Read()
    ' process row
End While
reader.Close()
conn.Close()

' unrelated UI code
Label1.Text = "Hello"
"#;
        let sliced = apply_logical_slice(content, "data_methods", "vb");
        assert!(
            sliced.contains("SqlConnection"),
            "Should include SqlConnection"
        );
        assert!(sliced.contains("SqlCommand"), "Should include SqlCommand");
        assert!(
            sliced.contains("ExecuteReader"),
            "Should include ExecuteReader"
        );
    }

    #[test]
    fn test_state_access_slice() {
        let content = r#"Session("UserName") = txtUser.Text
ViewState("PageIndex") = 0
Dim cached = Cache("ReportData")
Label1.Text = "Status OK"
Application("AppVersion") = "2.0"
"#;
        let sliced = apply_logical_slice(content, "state_access", "vb");
        assert!(sliced.contains("Session("), "Should include Session access");
        assert!(
            sliced.contains("ViewState("),
            "Should include ViewState access"
        );
        assert!(sliced.contains("Cache("), "Should include Cache access");
        assert!(
            sliced.contains("Application("),
            "Should include Application access"
        );
    }

    #[test]
    fn test_sql_queries_slice() {
        let content = r#"Dim query = "SELECT u.Name, u.Email FROM Users u WHERE u.Active = 1"
Dim insert = "INSERT INTO Logs (Msg) VALUES (@msg)"
Label1.Text = "Done"
cmd.CommandText = "EXEC sp_GetUserRoles @userId"
"#;
        let sliced = apply_logical_slice(content, "sql_queries", "vb");
        assert!(sliced.contains("SELECT"), "Should include SELECT");
        assert!(sliced.contains("INSERT INTO"), "Should include INSERT");
        assert!(sliced.contains("EXEC"), "Should include EXEC");
    }

    #[test]
    fn test_no_matches_returns_message() {
        let content = "just some random text\nnothing interesting here";
        let sliced = apply_logical_slice(content, "state_access", "vb");
        assert!(
            sliced.contains("no matching lines"),
            "Should return no-match message"
        );
    }

    #[test]
    fn test_unknown_slice_returns_everything() {
        let content = "line 1\nline 2";
        let sliced = apply_logical_slice(content, "nonexistent_type", "vb");
        assert_eq!(sliced, content);
    }
}
