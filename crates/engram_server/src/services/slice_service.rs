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
            let mut next_i = i;

            for j in start..lines.len() {
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
                        next_i = j + 1;
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
                        next_i = j + 1;
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
            i = next_i;
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
        "(no matching lines found for this slice type in this chunk)".to_string()
    } else {
        result.join("\n")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
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

    // ── available_slices ─────────────────────────────────────────────────────

    #[test]
    fn available_slices_contains_all_types() {
        let slices = available_slices();
        let names: Vec<&str> = slices.iter().map(|(name, _)| *name).collect();
        assert!(names.contains(&"all"));
        assert!(names.contains(&"event_handlers"));
        assert!(names.contains(&"ui_methods"));
        assert!(names.contains(&"data_methods"));
        assert!(names.contains(&"sql_queries"));
        assert!(names.contains(&"state_access"));
        assert_eq!(slices.len(), 6);
    }

    #[test]
    fn available_slices_all_have_descriptions() {
        for (name, description) in available_slices() {
            assert!(!description.is_empty(), "slice '{name}' has empty description");
        }
    }

    // ── event handler slice: C# patterns ────────────────────────────────────

    #[test]
    fn test_event_handler_slice_cs_button_click() {
        let content = r#"public partial class Orders : Page
{
    protected void Page_Load(object sender, EventArgs e)
    {
        if (!IsPostBack)
        {
            BindGrid();
        }
    }

    private void HelperMethod()
    {
        // not an event handler
    }

    protected void btnSearch_Click(object sender, EventArgs e)
    {
        SearchOrders();
    }
}
"#;
        let sliced = apply_logical_slice(content, "event_handlers", "cs");
        assert!(sliced.contains("Page_Load"), "should include Page_Load");
        assert!(sliced.contains("btnSearch_Click"), "should include btnSearch_Click");
        assert!(!sliced.contains("HelperMethod"), "should not include HelperMethod");
    }

    #[test]
    fn test_event_handler_slice_various_event_types() {
        let content = r#"
Protected Sub gvData_RowDataBound(sender As Object, e As GridViewRowEventArgs) Handles gvData.RowDataBound
    ' row data bound
End Sub

Protected Sub ddlFilter_SelectedIndexChanged(sender As Object, e As EventArgs) Handles ddlFilter.SelectedIndexChanged
    ' filter changed
End Sub

Protected Sub NotAnEvent()
    ' helper
End Sub
"#;
        let sliced = apply_logical_slice(content, "event_handlers", "vb");
        assert!(sliced.contains("RowDataBound"), "should include RowDataBound handler");
        assert!(sliced.contains("SelectedIndexChanged"), "should include SelectedIndexChanged");
    }

    #[test]
    fn test_event_handler_no_handlers_returns_message() {
        let content = "Dim x As Integer = 5\nDim y As String = \"hello\"";
        let sliced = apply_logical_slice(content, "event_handlers", "vb");
        assert!(
            sliced.contains("no event handler methods"),
            "no match should return no-match message: {sliced}"
        );
    }

    // ── UI methods slice ─────────────────────────────────────────────────────

    #[test]
    fn test_ui_methods_slice_finds_response_write() {
        let content = r#"
Dim msg As String = "hello"
Response.Write("<p>" & msg & "</p>")
Dim x As Integer = 42
"#;
        let sliced = apply_logical_slice(content, "ui_methods", "vb");
        assert!(sliced.contains("Response.Write"), "should find Response.Write");
        // Context lines should be included too
        assert!(sliced.contains("msg"));
    }

    #[test]
    fn test_ui_methods_slice_find_control() {
        let content = r#"
Dim ctrl = FindControl("myPanel")
ctrl.Visible = True
Dim unused = 99
"#;
        let sliced = apply_logical_slice(content, "ui_methods", "vb");
        assert!(sliced.contains("FindControl"), "should match FindControl");
    }

    #[test]
    fn test_ui_methods_slice_visible_assignment() {
        let content = r#"
myPanel.Visible = False
myLabel.Text = "Status"
unrelated = "nothing"
"#;
        let sliced = apply_logical_slice(content, "ui_methods", "vb");
        assert!(sliced.contains("Visible"), "should include Visible assignment");
    }

    #[test]
    fn test_ui_methods_no_matches_returns_message() {
        let content = "x = 1\ny = 2\nz = x + y";
        let sliced = apply_logical_slice(content, "ui_methods", "vb");
        assert!(sliced.contains("no matching lines"), "should return no-match message: {sliced}");
    }

    // ── SQL queries slice ────────────────────────────────────────────────────

    #[test]
    fn test_sql_queries_slice_update_statement() {
        let content = r#"Dim sql = "UPDATE Orders SET Status = 'Active' WHERE OrderId = @id"
cmd.CommandText = sql
Label1.Text = "Done"
"#;
        let sliced = apply_logical_slice(content, "sql_queries", "vb");
        assert!(sliced.contains("UPDATE"), "should include UPDATE statement");
    }

    #[test]
    fn test_sql_queries_slice_delete_statement() {
        let content = r#"cmd.CommandText = "DELETE FROM Logs WHERE LogDate < @date"
conn.Open()
"#;
        let sliced = apply_logical_slice(content, "sql_queries", "vb");
        assert!(sliced.contains("DELETE FROM"), "should include DELETE FROM");
    }

    #[test]
    fn test_sql_queries_slice_create_table() {
        let content = r#"Dim ddl = "CREATE TABLE TempResults (Id INT, Name VARCHAR(100))"
cmd.ExecuteNonQuery()
"#;
        let sliced = apply_logical_slice(content, "sql_queries", "vb");
        assert!(sliced.contains("CREATE TABLE"), "should include CREATE TABLE");
    }

    #[test]
    fn test_sql_queries_context_lines_included() {
        let content = "line1\nline2\nDim q = \"SELECT * FROM Users\"\nline4\nline5\nline6";
        let sliced = apply_logical_slice(content, "sql_queries", "vb");
        // 2 lines before: line1, line2
        assert!(sliced.contains("line1"), "context before should be included");
        assert!(sliced.contains("line4"), "context after should be included");
        // But line6 is 3 lines after → out of 2-line context window
        // (i=2 [0-indexed], context range = i-2..i+3 = 0..5)
        assert!(sliced.contains("line5"), "2nd line after should be included");
    }

    // ── State access slice ───────────────────────────────────────────────────

    #[test]
    fn test_state_access_slice_context_items() {
        let content = r#"Dim userId = Context.Items("UserId")
Label1.Text = "Welcome"
"#;
        let sliced = apply_logical_slice(content, "state_access", "vb");
        assert!(sliced.contains("Context.Items"), "should include Context.Items access");
    }

    #[test]
    fn test_state_access_bracket_syntax_cs() {
        let content = r#"Session["UserName"] = "Alice";
var role = Session["Role"];
var x = 5;
"#;
        let sliced = apply_logical_slice(content, "state_access", "cs");
        assert!(sliced.contains("Session["), "should find Session[] bracket syntax");
    }

    // ── Data methods slice ───────────────────────────────────────────────────

    #[test]
    fn test_data_methods_slice_entity_framework() {
        let content = r#"
var results = db.Orders.Where(o => o.Active).ToList();
var count = db.Orders.Count();
var name = "test";
"#;
        let sliced = apply_logical_slice(content, "data_methods", "cs");
        assert!(sliced.contains("ToList()"), "should match .ToList()");
    }

    #[test]
    fn test_data_methods_slice_execute_scalar() {
        let content = r#"
Dim count = CInt(cmd.ExecuteScalar())
Label1.Text = count.ToString()
"#;
        let sliced = apply_logical_slice(content, "data_methods", "vb");
        assert!(sliced.contains("ExecuteScalar"), "should find ExecuteScalar");
    }

    // ── gap indicator between non-contiguous matches ─────────────────────────

    #[test]
    fn test_gap_indicator_between_separate_matches() {
        // Two separate matches with many lines between them
        let lines: Vec<String> = (0..20)
            .map(|i| {
                if i == 0 {
                    "Session(\"key\") = 1".to_string()
                } else if i == 15 {
                    "Session(\"key2\") = 2".to_string()
                } else {
                    format!("unrelated_line_{i}")
                }
            })
            .collect();
        let content = lines.join("\n");
        let sliced = apply_logical_slice(&content, "state_access", "vb");
        assert!(sliced.contains("..."), "non-contiguous matches should show gap indicator");
    }

    // ── language parameter is accepted (currently reserved) ──────────────────

    #[test]
    fn language_parameter_does_not_affect_all_slice() {
        let content = "line 1\nline 2";
        assert_eq!(
            apply_logical_slice(content, "all", "vb"),
            apply_logical_slice(content, "all", "cs")
        );
    }
}
