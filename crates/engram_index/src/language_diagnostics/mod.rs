//! Multi-language diagnostics heuristics for migration and pre-edit risk context.

pub mod c;
pub mod cpp;
pub mod csharp;
pub mod rust;
pub mod vb;

use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LanguageFamily {
    CSharp,
    C,
    Cpp,
    Rust,
    /// VB.NET — the primary OciusX language.
    Vb,
}

#[derive(Debug, Clone, Serialize)]
pub struct LanguageDiagnostic {
    pub location: String,
    pub category: String,
    pub severity: String,
    pub evidence: String,
    pub guidance: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LanguageDiagnosticReport {
    pub language_family: String,
    pub diagnostics: Vec<LanguageDiagnostic>,
    pub total_findings: usize,
    pub files_analyzed: usize,
}

impl LanguageDiagnosticReport {
    fn new(
        language_family: &str,
        diagnostics: Vec<LanguageDiagnostic>,
        files_analyzed: usize,
    ) -> Self {
        let total_findings = diagnostics.len();
        Self {
            language_family: language_family.to_string(),
            diagnostics,
            total_findings,
            files_analyzed,
        }
    }
}

pub fn detect_language_diagnostics(
    family: LanguageFamily,
    code_files: &[(&str, &str)],
) -> LanguageDiagnosticReport {
    match family {
        LanguageFamily::CSharp => {
            let diagnostics = csharp::detect(code_files);
            LanguageDiagnosticReport::new("csharp", diagnostics, code_files.len())
        }
        LanguageFamily::C => {
            let diagnostics = c::detect(code_files);
            LanguageDiagnosticReport::new("c", diagnostics, code_files.len())
        }
        LanguageFamily::Cpp => {
            let diagnostics = cpp::detect(code_files);
            LanguageDiagnosticReport::new("cpp", diagnostics, code_files.len())
        }
        LanguageFamily::Rust => {
            let diagnostics = rust::detect(code_files);
            LanguageDiagnosticReport::new("rust", diagnostics, code_files.len())
        }
        LanguageFamily::Vb => {
            let diagnostics = vb::detect(code_files);
            LanguageDiagnosticReport::new("vb", diagnostics, code_files.len())
        }
    }
}
