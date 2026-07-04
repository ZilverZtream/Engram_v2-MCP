//! VB.NET pre-edit risk diagnostics. VB.NET is the primary OciusX language, so
//! an agent about to edit a VB method needs the same "what to watch out for"
//! signal the C#/C/C++/Rust modules provide. These flag the high-value VB
//! footguns a reviewer would block on.

use regex::Regex;
use std::sync::LazyLock;

use super::LanguageDiagnostic;

static ON_ERROR_RESUME_NEXT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bOn\s+Error\s+Resume\s+Next\b").expect("vb oern"));
static ON_ERROR_GOTO_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bOn\s+Error\s+GoTo\b").expect("vb oeg"));
// `New <Disposable>` not wrapped in a `Using` — VB data code leaks connections/
// streams/contexts when not deterministically disposed.
static DISPOSABLE_NEW_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bNew\s+(SqlConnection|SqlCommand|SqlDataReader|SqlDataAdapter|OleDbConnection|StreamReader|StreamWriter|FileStream|MemoryStream|StringWriter|StringReader|DataContext|TransactionScope)\b")
        .expect("vb disposable")
});
// `= Nothing` / `<> Nothing` used as a COMPARISON (in a conditional) instead of
// `Is Nothing` / `IsNot Nothing`. For reference types `= Nothing` invokes the
// `=` operator (or is always False), a classic VB correctness bug.
static EQ_NOTHING_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:If|ElseIf|While|AndAlso|OrElse|Return)\b.*?(?:<>|=)\s*Nothing\b")
        .expect("vb eqnothing")
});
static ADDHANDLER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bAddHandler\b").expect("vb addhandler"));

pub fn detect(code_files: &[(&str, &str)]) -> Vec<LanguageDiagnostic> {
    let mut out = Vec::new();
    for (file, content) in code_files {
        let has_remove_handler = content.to_ascii_lowercase().contains("removehandler");
        for (idx, line) in content.lines().enumerate() {
            let line_no = idx + 1;
            let trimmed = line.trim();
            if trimmed.starts_with('\'') {
                continue; // VB line comment
            }

            if ON_ERROR_RESUME_NEXT_RE.is_match(line) {
                out.push(LanguageDiagnostic {
                    location: format!("{file}:{line_no}"),
                    category: "on_error_resume_next".to_string(),
                    severity: "high".to_string(),
                    evidence: trimmed.to_string(),
                    guidance: "`On Error Resume Next` silently swallows ALL errors — any failure after it is invisible. Prefer structured `Try...Catch` with specific handling; if you must keep it, scope it tightly and check `Err.Number`.".to_string(),
                });
            } else if ON_ERROR_GOTO_RE.is_match(line) {
                out.push(LanguageDiagnostic {
                    location: format!("{file}:{line_no}"),
                    category: "on_error_goto".to_string(),
                    severity: "medium".to_string(),
                    evidence: trimmed.to_string(),
                    guidance: "Unstructured `On Error GoTo` handling — prefer `Try...Catch...Finally` for new/edited code so error flow is local and disposal is deterministic.".to_string(),
                });
            }

            if DISPOSABLE_NEW_RE.is_match(line) && !line.to_ascii_lowercase().contains("using") {
                out.push(LanguageDiagnostic {
                    location: format!("{file}:{line_no}"),
                    category: "disposable_without_using".to_string(),
                    severity: "high".to_string(),
                    evidence: trimmed.to_string(),
                    guidance: "Disposable created outside a `Using` block — wrap it in `Using … End Using` (or guarantee `.Dispose()` in `Finally`) to avoid leaking connections/streams/contexts.".to_string(),
                });
            }

            if EQ_NOTHING_RE.is_match(line)
                && !line.to_ascii_lowercase().contains("is nothing")
                && !line.to_ascii_lowercase().contains("isnot nothing")
            {
                out.push(LanguageDiagnostic {
                    location: format!("{file}:{line_no}"),
                    category: "equals_nothing_comparison".to_string(),
                    severity: "medium".to_string(),
                    evidence: trimmed.to_string(),
                    guidance: "Comparing a reference with `= Nothing` / `<> Nothing` is a VB bug — use `Is Nothing` / `IsNot Nothing` (the `=` form invokes the `=` operator or is always False for reference types).".to_string(),
                });
            }

            if ADDHANDLER_RE.is_match(line) && !has_remove_handler {
                out.push(LanguageDiagnostic {
                    location: format!("{file}:{line_no}"),
                    category: "addhandler_without_removehandler".to_string(),
                    severity: "medium".to_string(),
                    evidence: trimmed.to_string(),
                    guidance: "`AddHandler` with no `RemoveHandler` anywhere in the file can leak the publisher/subscriber for the object's lifetime; pair them or use `Handles` for page/control events.".to_string(),
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_on_error_resume_next_and_disposable_and_eq_nothing() {
        let code = r#"
Public Sub LoadData()
    On Error Resume Next
    Dim cn = New SqlConnection(connStr)
    If obj = Nothing Then Return
End Sub
"#;
        let d = detect(&[("Foo.vb", code)]);
        let cats: Vec<&str> = d.iter().map(|x| x.category.as_str()).collect();
        assert!(cats.contains(&"on_error_resume_next"), "{cats:?}");
        assert!(cats.contains(&"disposable_without_using"), "{cats:?}");
        assert!(cats.contains(&"equals_nothing_comparison"), "{cats:?}");
    }

    #[test]
    fn clean_vb_has_no_false_positives() {
        let code = r#"
Public Sub LoadData()
    Using cn = New SqlConnection(connStr)
        If obj Is Nothing Then Return
    End Using
End Sub
"#;
        let d = detect(&[("Foo.vb", code)]);
        assert!(
            d.is_empty(),
            "unexpected diagnostics: {:?}",
            d.iter().map(|x| &x.category).collect::<Vec<_>>()
        );
    }
}
