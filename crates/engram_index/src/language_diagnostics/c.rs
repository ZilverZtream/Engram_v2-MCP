use regex::Regex;
use std::sync::LazyLock;

use super::LanguageDiagnostic;

static RAW_COPY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(strcpy|strcat|sprintf|gets)\s*\(").expect("c raw"));
static ALLOC_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(malloc|calloc|realloc)\s*\(").expect("alloc"));
static FREE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bfree\s*\(").expect("free"));

pub fn detect(code_files: &[(&str, &str)]) -> Vec<LanguageDiagnostic> {
    let mut out = Vec::new();
    for (file, content) in code_files {
        let has_free = FREE_RE.is_match(content);
        for (idx, line) in content.lines().enumerate() {
            let line_no = idx + 1;
            if RAW_COPY_RE.is_match(line) {
                out.push(LanguageDiagnostic {
                    location: format!("{}:{}", file, line_no),
                    category: "buffer_unsafe_api".to_string(),
                    severity: "high".to_string(),
                    evidence: line.trim().to_string(),
                    guidance:
                        "Prefer bounded APIs and explicit size tracking to prevent buffer overruns."
                            .to_string(),
                });
            }
            if ALLOC_RE.is_match(line) && !has_free {
                out.push(LanguageDiagnostic {
                    location: format!("{}:{}", file, line_no),
                    category: "ownership_leak_risk".to_string(),
                    severity: "medium".to_string(),
                    evidence: line.trim().to_string(),
                    guidance: "Allocation without obvious free() in file suggests unclear ownership or leak risk.".to_string(),
                });
            }
        }
    }
    out
}
