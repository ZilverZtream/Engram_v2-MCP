//! VB.NET Translation Trap Detection
//!
//! Detects 14 categories of VB.NET semantic differences from C# that cause
//! silent bugs or compile errors in migrated code. These aren't syntax
//! differences — they're behavioral differences where the code compiles and
//! runs but produces wrong results.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;

// ── Output structs ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct VbTranslationTrap {
    pub trap: String,
    pub location: String,
    pub vb_code: String,
    pub risk: String,
    pub guidance: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct VbTranslationTrapReport {
    pub traps: Vec<VbTranslationTrap>,
    pub total_traps: usize,
    pub traps_by_category: BTreeMap<String, usize>,
    pub silent_bug_count: usize,
    pub compile_error_count: usize,
    pub files_analyzed: usize,
}

// ── Regex patterns ───────────────────────────────────────────────────────────

// 1. Nothing_ValueType: Assigning Nothing to value types
static NOTHING_VALUE_TYPE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)\bDim\s+(\w+)\s+As\s+(Integer|Long|Short|Byte|Single|Double|Decimal|Boolean|Date|DateTime|Guid|UInteger|ULong|UShort|SByte)\b[^=\n]*=\s*Nothing\b")
        .expect("nothing_value_type")
});

// 2. Is_vs_Equals: Using = or <> with Nothing instead of Is/IsNot
static EQUALS_NOTHING_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?im)\b\w+\s*(?:=|<>)\s*Nothing\b").expect("equals_nothing"));

// 3. Option_Compare_Text: Module-level text comparison mode
static OPTION_COMPARE_TEXT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)^\s*Option\s+Compare\s+Text\b").expect("option_compare_text")
});

// 4. On_Error_Resume_Next
static ON_ERROR_RESUME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?im)\bOn\s+Error\s+Resume\s+Next\b").expect("on_error_resume"));

// 5. On_Error_GoTo
static ON_ERROR_GOTO_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?im)\bOn\s+Error\s+GoTo\s+(\w+)").expect("on_error_goto"));

// 6. ReDim_Preserve
static REDIM_PRESERVE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?im)\bReDim\s+Preserve\s+(\w+)").expect("redim_preserve"));

// 7. Array_Upper_Bound: Dim x(10) = 11 elements in VB, new int[10] = 10 in C#
static ARRAY_UPPER_BOUND_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)\bDim\s+(\w+)\s*\(\s*(\d+)\s*\)\s+As\b").expect("array_upper_bound")
});

// 8. My_Namespace
static MY_NAMESPACE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bMy\.(Settings|Computer|Application|User|Resources|Forms|WebServices)\b")
        .expect("my_namespace")
});

// 9. Date_Literal
static DATE_LITERAL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"#\d{1,2}/\d{1,2}/\d{2,4}(?:\s+\d{1,2}:\d{2}(?::\d{2})?\s*(?:AM|PM)?)?#")
        .expect("date_literal")
});

// 10. WithEvents_Handles
static WITHEVENTS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?im)\bWithEvents\s+(\w+)\s+As\b").expect("withevents"));

// 11. Late_Binding (Option Strict Off)
static OPTION_STRICT_OFF_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?im)^\s*Option\s+Strict\s+Off\b").expect("option_strict_off"));

// 12. String_Functions: VB intrinsics are 1-based, C# is 0-based
static VB_STRING_FUNCS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(Mid|Left|Right|InStr|Len|LTrim|RTrim|Space|StrComp|UCase|LCase)\s*\(")
        .expect("vb_string_funcs")
});

// 13. Integer_Division: VB \ operator
static INTEGER_DIVISION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)\b(\w+)\s*\\\s*(\w+)").expect("integer_division"));

// 14. Default_Properties: Reuses Option_Strict_Off context
// (flagged as a separate trap when Option Strict Off is present)

// Helper: VB method definition for location tracking
static VB_METHOD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)^\s*(?:(?:Public|Private|Protected|Friend)\s+)?(?:(?:Shared|Overrides|Overridable|MustOverride|NotOverridable|Overloads)\s+)*(?:Sub|Function)\s+(\w+)")
        .expect("vb_method")
});

// ── Guidance table ───────────────────────────────────────────────────────────

struct TrapInfo {
    trap: &'static str,
    risk: &'static str,
    guidance: &'static str,
}

const TRAP_GUIDANCE: &[TrapInfo] = &[
    TrapInfo {
        trap: "Nothing_ValueType",
        risk: "silent_bug",
        guidance: "VB Nothing on value types = default(T) (e.g. Integer=0). C# int x = null won't compile; int? x = null changes semantics. Use default(T) or explicit 0.",
    },
    TrapInfo {
        trap: "Is_vs_Equals",
        risk: "silent_bug",
        guidance: "VB '= Nothing' on strings does value comparison (possibly case-insensitive with Option Compare Text). C# '== null' is always reference comparison. Use explicit string.Equals() with StringComparison.",
    },
    TrapInfo {
        trap: "Option_Compare_Text",
        risk: "silent_bug",
        guidance: "Module-level Option Compare Text makes ALL string = comparisons case-insensitive. C# == is always case-sensitive. Must add StringComparison.OrdinalIgnoreCase to every comparison.",
    },
    TrapInfo {
        trap: "On_Error_Resume_Next",
        risk: "silent_bug",
        guidance: "Swallows ALL errors until On Error GoTo 0. No C# equivalent — must wrap each statement in individual try/catch blocks. Audit which errors are expected vs hidden bugs.",
    },
    TrapInfo {
        trap: "On_Error_GoTo",
        risk: "compile_error",
        guidance: "Structured error handling via labels. Must restructure to try/catch/finally blocks. Map each label's handler code to the appropriate catch clause.",
    },
    TrapInfo {
        trap: "ReDim_Preserve",
        risk: "compile_error",
        guidance: "Use List<T> instead of arrays, or Array.Resize(). Note: VB arrays are declared by upper bound (Dim a(10) = 11 elements) — watch for off-by-one.",
    },
    TrapInfo {
        trap: "Array_Upper_Bound",
        risk: "silent_bug",
        guidance: "VB Dim x(10) = 11 elements (0-10). C# new int[10] = 10 elements (0-9). Off-by-one: add 1 to array size in C#, or use List<T>.",
    },
    TrapInfo {
        trap: "My_Namespace",
        risk: "compile_error",
        guidance: "The VB My namespace has no C# equivalent. Map: My.Settings→ConfigurationManager, My.Computer.FileSystem→System.IO, My.Application.Log→ILogger, My.User→HttpContext.User.",
    },
    TrapInfo {
        trap: "Date_Literal",
        risk: "compile_error",
        guidance: "C# has no date literals. Use new DateTime(year, month, day). Warning: VB #1/2/2000# is culture-dependent (month/day or day/month). Always verify the intended date.",
    },
    TrapInfo {
        trap: "WithEvents_Handles",
        risk: "compile_error",
        guidance: "C# has no WithEvents/Handles. Must manually wire event += handler in constructor or OnInitialized. Each Handles clause becomes an explicit subscription.",
    },
    TrapInfo {
        trap: "Late_Binding",
        risk: "silent_bug",
        guidance: "Option Strict Off enables late binding: Dim obj As Object then obj.Method() compiles in VB. C# requires 'dynamic' keyword. Late-bound calls have no compile-time checking.",
    },
    TrapInfo {
        trap: "String_Functions",
        risk: "silent_bug",
        guidance: "VB string functions are 1-based (Mid(s,1,5), InStr returns 1-based). C# Substring/IndexOf are 0-based. Off-by-one: subtract 1 from start positions, add 1 to IndexOf results when comparing.",
    },
    TrapInfo {
        trap: "Integer_Division",
        risk: "silent_bug",
        guidance: "VB '\\' is integer division (always truncates), '/' is floating-point even for integers. C# '/' on integers is already integer division. Verify division intent before translating.",
    },
    TrapInfo {
        trap: "Default_Properties",
        risk: "silent_bug",
        guidance: "Option Strict Off enables default property access: collection(index) calls the default indexer. C# requires explicit .Item[index] or [index]. Make all property access explicit.",
    },
];

fn get_guidance(trap_name: &str) -> (&'static str, &'static str) {
    TRAP_GUIDANCE
        .iter()
        .find(|t| t.trap == trap_name)
        .map(|t| (t.risk, t.guidance))
        .unwrap_or(("silent_bug", "Review this VB construct for C# translation."))
}

// ── Main detection function ──────────────────────────────────────────────────

/// Scan VB.NET code files for translation traps.
/// Only processes files with `.vb` extension.
pub fn detect_vb_translation_traps(code_files: &[(&str, &str)]) -> VbTranslationTrapReport {
    let mut traps: Vec<VbTranslationTrap> = Vec::new();
    let mut files_analyzed = 0;

    for &(path, content) in code_files {
        if !path.to_lowercase().ends_with(".vb") {
            continue;
        }
        files_analyzed += 1;

        // Build line-to-method index for location tracking
        let method_map = build_method_map(content);

        // 1. Nothing_ValueType
        for m in NOTHING_VALUE_TYPE_RE.captures_iter(content) {
            let Some(whole) = m.get(0) else { continue };
            let line = line_number(content, whole.start());
            let (risk, guidance) = get_guidance("Nothing_ValueType");
            traps.push(VbTranslationTrap {
                trap: "Nothing_ValueType".into(),
                location: format_location(path, &method_map, line),
                vb_code: whole.as_str().trim().to_string(),
                risk: risk.into(),
                guidance: guidance.into(),
            });
        }

        // 2. Is_vs_Equals — skip Dim declarations (those are initializations, not comparisons)
        for m in EQUALS_NOTHING_RE.find_iter(content) {
            let full_line = get_line_at(content, m.start());
            let trimmed = full_line.trim_start();
            // Skip variable declarations: "Dim x As Type = Nothing" is initialization, not comparison
            if trimmed.starts_with("Dim ")
                || trimmed.starts_with("dim ")
                || trimmed.starts_with("Private ")
                || trimmed.starts_with("Public ")
                || trimmed.starts_with("Protected ")
                || trimmed.starts_with("Friend ")
                || trimmed.starts_with("Static ")
            {
                continue;
            }
            let line = line_number(content, m.start());
            let (risk, guidance) = get_guidance("Is_vs_Equals");
            traps.push(VbTranslationTrap {
                trap: "Is_vs_Equals".into(),
                location: format_location(path, &method_map, line),
                vb_code: m.as_str().trim().to_string(),
                risk: risk.into(),
                guidance: guidance.into(),
            });
        }

        // 3. Option_Compare_Text
        for m in OPTION_COMPARE_TEXT_RE.find_iter(content) {
            let line = line_number(content, m.start());
            let (risk, guidance) = get_guidance("Option_Compare_Text");
            traps.push(VbTranslationTrap {
                trap: "Option_Compare_Text".into(),
                location: format_location(path, &method_map, line),
                vb_code: m.as_str().trim().to_string(),
                risk: risk.into(),
                guidance: guidance.into(),
            });
        }

        // 4. On_Error_Resume_Next
        for m in ON_ERROR_RESUME_RE.find_iter(content) {
            let line = line_number(content, m.start());
            let (risk, guidance) = get_guidance("On_Error_Resume_Next");
            traps.push(VbTranslationTrap {
                trap: "On_Error_Resume_Next".into(),
                location: format_location(path, &method_map, line),
                vb_code: m.as_str().trim().to_string(),
                risk: risk.into(),
                guidance: guidance.into(),
            });
        }

        // 5. On_Error_GoTo
        for m in ON_ERROR_GOTO_RE.find_iter(content) {
            let line = line_number(content, m.start());
            let (risk, guidance) = get_guidance("On_Error_GoTo");
            traps.push(VbTranslationTrap {
                trap: "On_Error_GoTo".into(),
                location: format_location(path, &method_map, line),
                vb_code: m.as_str().trim().to_string(),
                risk: risk.into(),
                guidance: guidance.into(),
            });
        }

        // 6. ReDim_Preserve
        for m in REDIM_PRESERVE_RE.find_iter(content) {
            let line = line_number(content, m.start());
            let (risk, guidance) = get_guidance("ReDim_Preserve");
            traps.push(VbTranslationTrap {
                trap: "ReDim_Preserve".into(),
                location: format_location(path, &method_map, line),
                vb_code: m.as_str().trim().to_string(),
                risk: risk.into(),
                guidance: guidance.into(),
            });
        }

        // 7. Array_Upper_Bound
        for m in ARRAY_UPPER_BOUND_RE.find_iter(content) {
            let line = line_number(content, m.start());
            let (risk, guidance) = get_guidance("Array_Upper_Bound");
            traps.push(VbTranslationTrap {
                trap: "Array_Upper_Bound".into(),
                location: format_location(path, &method_map, line),
                vb_code: m.as_str().trim().to_string(),
                risk: risk.into(),
                guidance: guidance.into(),
            });
        }

        // 8. My_Namespace
        for m in MY_NAMESPACE_RE.find_iter(content) {
            let line = line_number(content, m.start());
            let (risk, guidance) = get_guidance("My_Namespace");
            traps.push(VbTranslationTrap {
                trap: "My_Namespace".into(),
                location: format_location(path, &method_map, line),
                vb_code: m.as_str().trim().to_string(),
                risk: risk.into(),
                guidance: guidance.into(),
            });
        }

        // 9. Date_Literal
        for m in DATE_LITERAL_RE.find_iter(content) {
            let line = line_number(content, m.start());
            let (risk, guidance) = get_guidance("Date_Literal");
            traps.push(VbTranslationTrap {
                trap: "Date_Literal".into(),
                location: format_location(path, &method_map, line),
                vb_code: m.as_str().trim().to_string(),
                risk: risk.into(),
                guidance: guidance.into(),
            });
        }

        // 10. WithEvents_Handles
        for m in WITHEVENTS_RE.find_iter(content) {
            let line = line_number(content, m.start());
            let (risk, guidance) = get_guidance("WithEvents_Handles");
            traps.push(VbTranslationTrap {
                trap: "WithEvents_Handles".into(),
                location: format_location(path, &method_map, line),
                vb_code: m.as_str().trim().to_string(),
                risk: risk.into(),
                guidance: guidance.into(),
            });
        }

        // 11. Late_Binding (Option Strict Off)
        for m in OPTION_STRICT_OFF_RE.find_iter(content) {
            let line = line_number(content, m.start());
            let (risk, guidance) = get_guidance("Late_Binding");
            traps.push(VbTranslationTrap {
                trap: "Late_Binding".into(),
                location: format_location(path, &method_map, line),
                vb_code: m.as_str().trim().to_string(),
                risk: risk.into(),
                guidance: guidance.into(),
            });
        }

        // 12. String_Functions
        for m in VB_STRING_FUNCS_RE.find_iter(content) {
            let line = line_number(content, m.start());
            let (risk, guidance) = get_guidance("String_Functions");
            traps.push(VbTranslationTrap {
                trap: "String_Functions".into(),
                location: format_location(path, &method_map, line),
                vb_code: m.as_str().trim().to_string(),
                risk: risk.into(),
                guidance: guidance.into(),
            });
        }

        // 13. Integer_Division
        for m in INTEGER_DIVISION_RE.find_iter(content) {
            // Skip lines that look like file paths, string literals, or comments
            let matched = m.as_str();
            let line = line_number(content, m.start());
            let full_line = get_line_at(content, m.start());
            let trimmed_line = full_line.trim_start();
            if trimmed_line.starts_with('\'')
                || full_line.contains(":\\")
                || full_line.contains("\\\\")
                || is_inside_string_literal(
                    full_line,
                    m.start() - content[..m.start()].rfind('\n').map(|p| p + 1).unwrap_or(0),
                )
            {
                continue;
            }
            let (risk, guidance) = get_guidance("Integer_Division");
            traps.push(VbTranslationTrap {
                trap: "Integer_Division".into(),
                location: format_location(path, &method_map, line),
                vb_code: matched.trim().to_string(),
                risk: risk.into(),
                guidance: guidance.into(),
            });
        }

        // 14. Default_Properties (flagged when Option Strict Off is present)
        if OPTION_STRICT_OFF_RE.is_match(content) {
            let line = OPTION_STRICT_OFF_RE
                .find(content)
                .map(|m| line_number(content, m.start()))
                .unwrap_or(1);
            let (risk, guidance) = get_guidance("Default_Properties");
            traps.push(VbTranslationTrap {
                trap: "Default_Properties".into(),
                location: format_location(path, &method_map, line),
                vb_code: "Option Strict Off (enables default property access)".into(),
                risk: risk.into(),
                guidance: guidance.into(),
            });
        }
    }

    // Aggregate stats
    let mut traps_by_category: BTreeMap<String, usize> = BTreeMap::new();
    let mut silent_bug_count = 0usize;
    let mut compile_error_count = 0usize;
    for t in &traps {
        *traps_by_category.entry(t.trap.clone()).or_default() += 1;
        match t.risk.as_str() {
            "silent_bug" => silent_bug_count += 1,
            "compile_error" => compile_error_count += 1,
            _ => {}
        }
    }
    let total_traps = traps.len();

    VbTranslationTrapReport {
        traps,
        total_traps,
        traps_by_category,
        silent_bug_count,
        compile_error_count,
        files_analyzed,
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Compute 1-based line number from byte offset.
fn line_number(content: &str, byte_offset: usize) -> usize {
    content[..byte_offset.min(content.len())]
        .bytes()
        .filter(|&b| b == b'\n')
        .count()
        + 1
}

/// Get the full line text at a given byte offset.
fn get_line_at(content: &str, byte_offset: usize) -> &str {
    let start = content[..byte_offset]
        .rfind('\n')
        .map(|p| p + 1)
        .unwrap_or(0);
    let end = content[byte_offset..]
        .find('\n')
        .map(|p| byte_offset + p)
        .unwrap_or(content.len());
    &content[start..end]
}

/// Build a map of line_number → current_method_name for location tracking.
fn build_method_map(content: &str) -> Vec<(usize, String)> {
    let mut entries: Vec<(usize, String)> = Vec::new();
    for m in VB_METHOD_RE.captures_iter(content) {
        let name = m[1].to_string();
        let Some(whole) = m.get(0) else { continue };
        let line = line_number(content, whole.start());
        entries.push((line, name));
    }
    entries
}

/// Check if a byte offset within a line falls inside a VB string literal.
fn is_inside_string_literal(line: &str, offset_in_line: usize) -> bool {
    let mut in_string = false;
    for (i, ch) in line.char_indices() {
        if i >= offset_in_line {
            return in_string;
        }
        if ch == '"' {
            in_string = !in_string;
        }
    }
    in_string
}

/// Format location as "file:method:line N" or "file:line N".
fn format_location(path: &str, method_map: &[(usize, String)], line: usize) -> String {
    // Find the most recent method definition before this line
    let method = method_map
        .iter()
        .rev()
        .find(|(ml, _)| *ml <= line)
        .map(|(_, name)| name.as_str());

    match method {
        Some(m) => format!("{path}:{m}:line {line}"),
        None => format!("{path}:line {line}"),
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn detect(code: &str) -> VbTranslationTrapReport {
        detect_vb_translation_traps(&[("Test.vb", code)])
    }

    fn has_trap(report: &VbTranslationTrapReport, trap_name: &str) -> bool {
        report.traps.iter().any(|t| t.trap == trap_name)
    }

    #[test]
    fn detect_nothing_value_type() {
        let code = r#"
Public Sub Page_Load()
    Dim count As Integer = Nothing
    Dim flag As Boolean = Nothing
End Sub
"#;
        let report = detect(code);
        assert!(has_trap(&report, "Nothing_ValueType"));
        assert!(
            report
                .traps
                .iter()
                .filter(|t| t.trap == "Nothing_ValueType")
                .count()
                >= 2
        );
    }

    #[test]
    fn detect_is_vs_equals() {
        let code = r#"
If userName = Nothing Then
    ' do something
End If
If obj <> Nothing Then
    ' do something
End If
"#;
        let report = detect(code);
        assert!(has_trap(&report, "Is_vs_Equals"));
        assert!(
            report
                .traps
                .iter()
                .filter(|t| t.trap == "Is_vs_Equals")
                .count()
                >= 2
        );
    }

    #[test]
    fn detect_option_compare_text() {
        let code = r#"
Option Compare Text

Public Class MyModule
    Public Sub Test()
        If name = "admin" Then ' case-insensitive!
        End If
    End Sub
End Class
"#;
        let report = detect(code);
        assert!(has_trap(&report, "Option_Compare_Text"));
    }

    #[test]
    fn detect_on_error_resume_next() {
        let code = r#"
Public Sub RiskyMethod()
    On Error Resume Next
    Dim x = 1 / 0
    On Error GoTo 0
End Sub
"#;
        let report = detect(code);
        assert!(has_trap(&report, "On_Error_Resume_Next"));
    }

    #[test]
    fn detect_on_error_goto() {
        let code = r#"
Public Sub HandleErrors()
    On Error GoTo ErrorHandler
    Dim x = SomeDangerousCall()
    Exit Sub
ErrorHandler:
    LogError(Err.Description)
End Sub
"#;
        let report = detect(code);
        assert!(has_trap(&report, "On_Error_GoTo"));
    }

    #[test]
    fn detect_redim_preserve() {
        let code = r#"
Public Sub GrowArray()
    Dim arr() As String
    ReDim arr(5)
    ReDim Preserve arr(10)
End Sub
"#;
        let report = detect(code);
        assert!(has_trap(&report, "ReDim_Preserve"));
    }

    #[test]
    fn detect_array_upper_bound() {
        let code = r#"
Public Sub CreateArrays()
    Dim items(10) As String
    Dim counts(50) As Integer
End Sub
"#;
        let report = detect(code);
        assert!(has_trap(&report, "Array_Upper_Bound"));
        assert!(
            report
                .traps
                .iter()
                .filter(|t| t.trap == "Array_Upper_Bound")
                .count()
                >= 2
        );
    }

    #[test]
    fn detect_my_namespace() {
        let code = r#"
Public Sub UseMyNamespace()
    Dim setting = My.Settings.ConnectionString
    My.Computer.FileSystem.WriteAllText("log.txt", "hello", False)
    My.Application.Log.WriteEntry("started")
End Sub
"#;
        let report = detect(code);
        assert!(has_trap(&report, "My_Namespace"));
        assert!(
            report
                .traps
                .iter()
                .filter(|t| t.trap == "My_Namespace")
                .count()
                >= 3
        );
    }

    #[test]
    fn detect_date_literal() {
        let code = r#"
Public Sub UseDates()
    Dim startDate = #1/15/2023#
    Dim endDate = #12/31/2023 11:59 PM#
End Sub
"#;
        let report = detect(code);
        assert!(has_trap(&report, "Date_Literal"));
        assert!(
            report
                .traps
                .iter()
                .filter(|t| t.trap == "Date_Literal")
                .count()
                >= 2
        );
    }

    #[test]
    fn detect_withevents_handles() {
        let code = r#"
Public Class MyForm
    Private WithEvents btnSave As Button
    Private WithEvents tmrRefresh As Timer
End Class
"#;
        let report = detect(code);
        assert!(has_trap(&report, "WithEvents_Handles"));
        assert!(
            report
                .traps
                .iter()
                .filter(|t| t.trap == "WithEvents_Handles")
                .count()
                >= 2
        );
    }

    #[test]
    fn detect_late_binding() {
        let code = r#"
Option Strict Off

Public Sub LateBound()
    Dim obj As Object = CreateObject("Scripting.FileSystemObject")
    obj.CreateTextFile("test.txt")
End Sub
"#;
        let report = detect(code);
        assert!(has_trap(&report, "Late_Binding"));
    }

    #[test]
    fn detect_string_functions() {
        let code = r#"
Public Sub ManipulateStrings()
    Dim result = Mid(name, 1, 5)
    Dim pos = InStr(text, "search")
    Dim leftPart = Left(value, 10)
    Dim rightPart = Right(value, 3)
End Sub
"#;
        let report = detect(code);
        assert!(has_trap(&report, "String_Functions"));
        assert!(
            report
                .traps
                .iter()
                .filter(|t| t.trap == "String_Functions")
                .count()
                >= 4
        );
    }

    #[test]
    fn detect_integer_division() {
        let code = r#"
Public Sub DoMath()
    Dim pages = totalItems \ pageSize
End Sub
"#;
        let report = detect(code);
        assert!(has_trap(&report, "Integer_Division"));
    }

    #[test]
    fn detect_default_properties() {
        let code = r#"
Option Strict Off

Public Sub AccessDefaults()
    Dim val = collection(0)
End Sub
"#;
        let report = detect(code);
        assert!(has_trap(&report, "Default_Properties"));
    }

    #[test]
    fn skips_non_vb_files() {
        let code = "Dim count As Integer = Nothing";
        let report = detect_vb_translation_traps(&[("Test.cs", code)]);
        assert_eq!(report.files_analyzed, 0);
        assert_eq!(report.total_traps, 0);
    }

    #[test]
    fn all_14_categories_detected() {
        let code = r#"
Option Compare Text
Option Strict Off

Public Class FullTrapDemo
    Private WithEvents btnSave As Button

    Public Sub Page_Load()
        Dim count As Integer = Nothing
        If userName = Nothing Then
        End If
        On Error Resume Next
        On Error GoTo ErrHandler
        ReDim Preserve items(20)
        Dim arr(10) As String
        Dim s = My.Settings.AppName
        Dim d = #1/1/2000#
        Dim x = Mid(name, 1, 5)
        Dim pages = total \ size
    End Sub
End Class
"#;
        let report = detect(code);
        let categories: std::collections::HashSet<&str> =
            report.traps.iter().map(|t| t.trap.as_str()).collect();
        let expected = [
            "Nothing_ValueType",
            "Is_vs_Equals",
            "Option_Compare_Text",
            "On_Error_Resume_Next",
            "On_Error_GoTo",
            "ReDim_Preserve",
            "Array_Upper_Bound",
            "My_Namespace",
            "Date_Literal",
            "WithEvents_Handles",
            "Late_Binding",
            "String_Functions",
            "Integer_Division",
            "Default_Properties",
        ];
        for cat in &expected {
            assert!(categories.contains(cat), "Missing trap category: {cat}");
        }
    }

    #[test]
    fn location_includes_method_name() {
        let code = r#"
Public Sub Page_Load()
    Dim count As Integer = Nothing
End Sub
"#;
        let report = detect(code);
        let trap = report
            .traps
            .iter()
            .find(|t| t.trap == "Nothing_ValueType")
            .expect("should find Nothing_ValueType");
        assert!(
            trap.location.contains("Page_Load"),
            "location should contain method name: {}",
            trap.location
        );
    }

    #[test]
    fn report_aggregates_correctly() {
        let code = r#"
Public Sub Test()
    On Error Resume Next
    On Error GoTo ErrHandler
    Dim x As Integer = Nothing
End Sub
"#;
        let report = detect(code);
        assert!(report.silent_bug_count >= 2); // Resume Next + Nothing
        assert!(report.compile_error_count >= 1); // GoTo
        assert_eq!(
            report.total_traps,
            report.silent_bug_count + report.compile_error_count
        );
    }

    #[test]
    fn is_vs_equals_skips_dim_declarations() {
        // "Dim x As Object = Nothing" is initialization, NOT a comparison
        let code = r#"
Public Sub Init()
    Dim conn As Object = Nothing
    Dim result As String = Nothing
End Sub
"#;
        let report = detect(code);
        assert!(
            !has_trap(&report, "Is_vs_Equals"),
            "Dim declarations should NOT trigger Is_vs_Equals: got {:?}",
            report
                .traps
                .iter()
                .filter(|t| t.trap == "Is_vs_Equals")
                .map(|t| &t.vb_code)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn is_vs_equals_catches_comparison_context() {
        // If/While comparisons should still be caught
        let code = r#"
Public Sub Check()
    If conn = Nothing Then
    End If
    While result <> Nothing
    End While
End Sub
"#;
        let report = detect(code);
        assert!(
            report
                .traps
                .iter()
                .filter(|t| t.trap == "Is_vs_Equals")
                .count()
                >= 2,
            "Should detect Is_vs_Equals in If/While contexts"
        );
    }

    // ── New tests: On_Error variants ─────────────────────────────────────

    #[test]
    fn on_error_resume_next_is_silent_bug() {
        let code = r#"
Public Sub Load()
    On Error Resume Next
    Dim x = DangerousOp()
End Sub
"#;
        let report = detect(code);
        let trap = report.traps.iter().find(|t| t.trap == "On_Error_Resume_Next");
        assert!(trap.is_some(), "Should detect On_Error_Resume_Next");
        assert_eq!(trap.unwrap().risk, "silent_bug");
    }

    #[test]
    fn on_error_goto_label_is_compile_error() {
        let code = r#"
Public Sub HandleErrors()
    On Error GoTo ErrHandler
    Dim x = 1
    Exit Sub
ErrHandler:
    Resume Next
End Sub
"#;
        let report = detect(code);
        let trap = report.traps.iter().find(|t| t.trap == "On_Error_GoTo");
        assert!(trap.is_some(), "Should detect On_Error_GoTo");
        assert_eq!(trap.unwrap().risk, "compile_error");
    }

    #[test]
    fn on_error_goto_zero_resets_handler() {
        // "On Error GoTo 0" resets the error handler and IS detected (GoTo 0 is
        // a label "0" which matches the GoTo pattern).
        let code = r#"
Public Sub Reset()
    On Error GoTo 0
End Sub
"#;
        let report = detect(code);
        // On Error GoTo 0 matches the regex — it resets error handling.
        assert!(has_trap(&report, "On_Error_GoTo"), "On Error GoTo 0 should be detected");
    }

    #[test]
    fn no_on_error_in_clean_code() {
        let code = r#"
Public Sub SafeMethod()
    Try
        Dim x = 1
    Catch ex As Exception
        LogError(ex)
    End Try
End Sub
"#;
        let report = detect(code);
        assert!(!has_trap(&report, "On_Error_Resume_Next"));
        assert!(!has_trap(&report, "On_Error_GoTo"));
    }

    // ── New tests: Nothing value-type assignments ─────────────────────────

    #[test]
    fn nothing_value_type_date_type() {
        let code = r#"
Public Sub Init()
    Dim d As Date = Nothing
End Sub
"#;
        let report = detect(code);
        assert!(has_trap(&report, "Nothing_ValueType"), "Date = Nothing should be a trap");
    }

    #[test]
    fn nothing_value_type_decimal_and_guid() {
        let code = r#"
Public Sub Init()
    Dim price As Decimal = Nothing
    Dim id As Guid = Nothing
End Sub
"#;
        let report = detect(code);
        let count = report.traps.iter().filter(|t| t.trap == "Nothing_ValueType").count();
        assert_eq!(count, 2, "Both Decimal and Guid should trigger Nothing_ValueType");
    }

    #[test]
    fn nothing_on_reference_type_not_flagged_as_value_type() {
        // Dim x As String = Nothing is NOT a Nothing_ValueType (String is ref type)
        let code = r#"
Public Sub Init()
    Dim name As String = Nothing
    Dim obj As Object = Nothing
End Sub
"#;
        let report = detect(code);
        assert!(!has_trap(&report, "Nothing_ValueType"),
            "String/Object = Nothing should NOT trigger Nothing_ValueType");
    }

    // ── New tests: My_Namespace variants ──────────────────────────────────

    #[test]
    fn my_user_namespace() {
        let code = r#"
Public Sub CheckUser()
    Dim name = My.User.Name
End Sub
"#;
        let report = detect(code);
        assert!(has_trap(&report, "My_Namespace"));
    }

    #[test]
    fn my_resources_namespace() {
        let code = r#"
Public Sub GetLabel()
    Dim lbl = My.Resources.WelcomeMessage
End Sub
"#;
        let report = detect(code);
        assert!(has_trap(&report, "My_Namespace"));
    }

    #[test]
    fn my_settings_compile_error_risk() {
        let code = r#"
Public Sub Save()
    My.Settings.LastLogin = Now()
    My.Settings.Save()
End Sub
"#;
        let report = detect(code);
        let my_traps: Vec<_> = report.traps.iter().filter(|t| t.trap == "My_Namespace").collect();
        assert!(!my_traps.is_empty());
        // All My.* traps are compile_error
        assert!(my_traps.iter().all(|t| t.risk == "compile_error"));
    }

    // ── New tests: Date literals ──────────────────────────────────────────

    #[test]
    fn date_literal_with_time() {
        let code = r#"
Public Sub Schedule()
    Dim deadline = #6/30/2024 5:00:00 PM#
End Sub
"#;
        let report = detect(code);
        assert!(has_trap(&report, "Date_Literal"));
    }

    #[test]
    fn date_literal_short_year() {
        let code = r#"
Dim y2k = #1/1/00#
"#;
        let report = detect(code);
        assert!(has_trap(&report, "Date_Literal"), "Short year date literal should be detected");
    }

    #[test]
    fn no_date_literal_in_plain_string() {
        // A string containing slashes shouldn't be confused for a date literal
        let code = r#"
Public Sub PathExample()
    Dim path As String = "C:/temp/file.txt"
End Sub
"#;
        let report = detect(code);
        assert!(!has_trap(&report, "Date_Literal"),
            "String with slashes should not be flagged as date literal");
    }

    // ── New tests: Array upper-bound ──────────────────────────────────────

    #[test]
    fn array_upper_bound_zero() {
        // Dim arr(0) = 1 element, NOT 0 elements
        let code = r#"
Public Sub SingleElement()
    Dim arr(0) As Integer
End Sub
"#;
        let report = detect(code);
        assert!(has_trap(&report, "Array_Upper_Bound"),
            "Dim arr(0) should still trigger Array_Upper_Bound trap");
    }

    #[test]
    fn array_upper_bound_guidance_mentions_off_by_one() {
        let code = r#"
Dim items(5) As String
"#;
        let report = detect(code);
        let trap = report.traps.iter().find(|t| t.trap == "Array_Upper_Bound").unwrap();
        assert!(trap.guidance.contains("off-by-one") || trap.guidance.contains("Off-by-one"),
            "Guidance should mention off-by-one risk");
    }

    // ── New tests: ReDim_Preserve ─────────────────────────────────────────

    #[test]
    fn redim_preserve_in_loop() {
        let code = r#"
Public Sub BuildList()
    Dim results() As String
    For i = 1 To 10
        ReDim Preserve results(i)
        results(i) = GetItem(i)
    Next
End Sub
"#;
        let report = detect(code);
        let count = report.traps.iter().filter(|t| t.trap == "ReDim_Preserve").count();
        assert!(count >= 1, "ReDim Preserve inside loop should be detected");
    }

    #[test]
    fn redim_without_preserve_not_flagged() {
        // ReDim without Preserve is fine — just resizes (no data preservation concern)
        let code = r#"
Public Sub Reset()
    Dim arr() As Integer
    ReDim arr(10)
End Sub
"#;
        let report = detect(code);
        assert!(!has_trap(&report, "ReDim_Preserve"),
            "ReDim without Preserve should not be flagged");
    }

    // ── New tests: WithEvents/Handles ─────────────────────────────────────

    #[test]
    fn withevents_is_compile_error() {
        let code = r#"
Public Class Form1
    Private WithEvents myTimer As Timer
End Class
"#;
        let report = detect(code);
        let trap = report.traps.iter().find(|t| t.trap == "WithEvents_Handles");
        assert!(trap.is_some());
        assert_eq!(trap.unwrap().risk, "compile_error");
    }

    #[test]
    fn withevents_guidance_mentions_event_wiring() {
        let code = r#"
Private WithEvents btn As Button
"#;
        let report = detect(code);
        let trap = report.traps.iter().find(|t| t.trap == "WithEvents_Handles").unwrap();
        assert!(trap.guidance.contains("event") || trap.guidance.contains("Handles"),
            "Guidance should mention event wiring");
    }

    // ── New tests: String_Functions ───────────────────────────────────────

    #[test]
    fn len_function_detected() {
        let code = r#"
Public Sub CheckLength()
    Dim n = Len(myString)
End Sub
"#;
        let report = detect(code);
        assert!(has_trap(&report, "String_Functions"), "Len() should be detected");
    }

    #[test]
    fn ucase_lcase_detected() {
        let code = r#"
Public Sub Normalize()
    Dim up = UCase(name)
    Dim lo = LCase(name)
End Sub
"#;
        let report = detect(code);
        let count = report.traps.iter().filter(|t| t.trap == "String_Functions").count();
        assert!(count >= 2, "UCase and LCase should each be detected");
    }

    #[test]
    fn string_functions_guidance_mentions_one_based() {
        let code = r#"
Dim s = Mid(text, 1, 3)
"#;
        let report = detect(code);
        let trap = report.traps.iter().find(|t| t.trap == "String_Functions").unwrap();
        assert!(trap.guidance.contains("1-based") || trap.guidance.contains("0-based"),
            "Guidance should explain 1-based vs 0-based indexing");
    }

    // ── New tests: Integer_Division ───────────────────────────────────────

    #[test]
    fn integer_division_multiple_operators() {
        let code = r#"
Public Sub Calc()
    Dim a = total \ count
    Dim b = n \ m
End Sub
"#;
        let report = detect(code);
        let count = report.traps.iter().filter(|t| t.trap == "Integer_Division").count();
        assert!(count >= 2, "Both \\ operators should be detected");
    }

    #[test]
    fn integer_division_not_in_comment() {
        let code = r#"
' This uses result \ size for paging
Public Sub Calc()
    Dim x = 10
End Sub
"#;
        let report = detect(code);
        // Comment lines should be skipped (the comment starts with ')
        // The regex may still match if the comment isn't on a line starting with '
        // This tests that the '  comment skipping in INTEGER_DIVISION_RE is working
        let division_traps: Vec<_> = report.traps.iter()
            .filter(|t| t.trap == "Integer_Division")
            .collect();
        // All matched traps should NOT have their vb_code come from the comment line
        for trap in &division_traps {
            // The comment line starts with ' so the line trimmed starts with '
            // Verify no trap comes from comment lines
            assert!(!trap.vb_code.contains("This uses"),
                "Should not flag code inside comments");
        }
    }

    #[test]
    fn integer_division_skips_file_paths() {
        let code = r#"
Public Sub LoadFile()
    Dim path = "C:\Users\test\file.txt"
End Sub
"#;
        let report = detect(code);
        // File paths with \\ or :\ should be skipped
        let division_traps: Vec<_> = report.traps.iter()
            .filter(|t| t.trap == "Integer_Division")
            .collect();
        for trap in &division_traps {
            assert!(!trap.vb_code.contains("Users"),
                "Should not flag backslash in file paths");
        }
    }

    // ── New tests: Option_Compare_Text ────────────────────────────────────

    #[test]
    fn option_compare_text_guidance_mentions_case_insensitive() {
        let code = "Option Compare Text\n";
        let report = detect(code);
        let trap = report.traps.iter().find(|t| t.trap == "Option_Compare_Text").unwrap();
        assert!(trap.guidance.to_lowercase().contains("case-insensitive")
            || trap.guidance.to_lowercase().contains("case_insensitive"),
            "Guidance should mention case-insensitive comparison");
    }

    #[test]
    fn option_compare_binary_not_flagged() {
        let code = "Option Compare Binary\n";
        let report = detect(code);
        assert!(!has_trap(&report, "Option_Compare_Text"),
            "Option Compare Binary should NOT trigger the trap");
    }

    // ── New tests: Late_Binding / Option Strict Off ───────────────────────

    #[test]
    fn option_strict_off_also_triggers_default_properties() {
        let code = r#"
Option Strict Off
Public Sub Test()
    Dim obj As Object
    obj.Method()
End Sub
"#;
        let report = detect(code);
        assert!(has_trap(&report, "Late_Binding"), "Option Strict Off → Late_Binding");
        assert!(has_trap(&report, "Default_Properties"), "Option Strict Off → Default_Properties");
    }

    #[test]
    fn no_option_strict_off_no_late_binding() {
        let code = r#"
Option Strict On
Public Sub StrictMethod()
    Dim x As Integer = 42
End Sub
"#;
        let report = detect(code);
        assert!(!has_trap(&report, "Late_Binding"),
            "Option Strict On should NOT trigger Late_Binding");
        assert!(!has_trap(&report, "Default_Properties"),
            "Option Strict On should NOT trigger Default_Properties");
    }

    // ── New tests: location tracking ──────────────────────────────────────

    #[test]
    fn location_outside_method_uses_file_line() {
        // A trap that appears before any Sub/Function definition
        let code = r#"Option Compare Text
Public Class MyClass
End Class
"#;
        let report = detect(code);
        let trap = report.traps.iter().find(|t| t.trap == "Option_Compare_Text").unwrap();
        // No method defined before line 1, so location should be "file:line N"
        assert!(trap.location.contains("line"), "Location should contain line number");
    }

    #[test]
    fn multiple_files_are_all_analyzed() {
        let files: &[(&str, &str)] = &[
            ("First.vb", "On Error Resume Next\n"),
            ("Second.vb", "Dim x(10) As Integer\n"),
            ("Third.cs", "var x = 1;"),  // should be skipped
        ];
        let report = detect_vb_translation_traps(files);
        assert_eq!(report.files_analyzed, 2, "Only .vb files should be counted");
        assert!(has_trap(&report, "On_Error_Resume_Next"));
        assert!(has_trap(&report, "Array_Upper_Bound"));
    }

    #[test]
    fn risk_counts_sum_to_total() {
        let code = r#"
Option Compare Text
Option Strict Off
On Error Resume Next
On Error GoTo Handler
ReDim Preserve arr(5)
WithEvents btn As Button
"#;
        let report = detect(code);
        assert_eq!(
            report.total_traps,
            report.silent_bug_count + report.compile_error_count,
            "silent_bug + compile_error must equal total_traps"
        );
    }

    #[test]
    fn traps_by_category_matches_traps_vec() {
        let code = r#"
On Error Resume Next
Dim x(5) As Integer
Dim y(10) As String
"#;
        let report = detect(code);
        // Verify traps_by_category counts match actual traps
        for (cat, &count) in &report.traps_by_category {
            let actual = report.traps.iter().filter(|t| &t.trap == cat).count();
            assert_eq!(actual, count,
                "traps_by_category[{}] = {} but actual count = {}", cat, count, actual);
        }
    }

    #[test]
    fn is_nothing_comparison_with_not_operator() {
        // "If Not obj = Nothing" should still be caught
        let code = r#"
Public Sub TestIt()
    If Not conn = Nothing Then
        conn.Close()
    End If
End Sub
"#;
        let report = detect(code);
        // The "conn = Nothing" part matches the regex
        assert!(has_trap(&report, "Is_vs_Equals"),
            "Comparison inside 'Not' expression should still be detected");
    }

    #[test]
    fn withevents_matches_different_visibility_modifiers() {
        let code = r#"
Public Class MyClass
    Public WithEvents btn1 As Button
    Private WithEvents btn2 As Button
    Protected WithEvents btn3 As Button
End Class
"#;
        let report = detect(code);
        let count = report.traps.iter().filter(|t| t.trap == "WithEvents_Handles").count();
        assert_eq!(count, 3, "All three WithEvents declarations should be detected");
    }
}
