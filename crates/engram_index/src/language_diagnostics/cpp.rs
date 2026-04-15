use regex::Regex;
use std::sync::LazyLock;

use super::LanguageDiagnostic;

static RAW_NEW_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bnew\s+[A-Za-z_:][A-Za-z0-9_:<>]*").expect("new"));
static RAW_DELETE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bdelete\s+").expect("delete"));
static THROW_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bthrow\b").expect("throw"));
static MANUAL_LOCK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(mutex\.lock\(\)|lock\(\)|unlock\(\))").expect("lock"));

pub fn detect(code_files: &[(&str, &str)]) -> Vec<LanguageDiagnostic> {
    let mut out = Vec::new();
    for (file, content) in code_files {
        let has_delete = RAW_DELETE_RE.is_match(content);
        for (idx, line) in content.lines().enumerate() {
            let line_no = idx + 1;
            if RAW_NEW_RE.is_match(line) {
                out.push(LanguageDiagnostic {
                    location: format!("{}:{}", file, line_no),
                    category: "raw_new_delete_hotspot".to_string(),
                    severity: if has_delete { "medium" } else { "high" }.to_string(),
                    evidence: line.trim().to_string(),
                    guidance: "Prefer RAII with std::unique_ptr/std::shared_ptr or stack ownership over raw new/delete.".to_string(),
                });
            }
            if MANUAL_LOCK_RE.is_match(line) {
                out.push(LanguageDiagnostic {
                    location: format!("{}:{}", file, line_no),
                    category: "raii_violation".to_string(),
                    severity: "medium".to_string(),
                    evidence: line.trim().to_string(),
                    guidance: "Use lock guards (std::lock_guard/std::scoped_lock) to maintain RAII and exception safety.".to_string(),
                });
            }
            if THROW_RE.is_match(line) && content.contains("new ") {
                out.push(LanguageDiagnostic {
                    location: format!("{}:{}", file, line_no),
                    category: "exception_safety_flag".to_string(),
                    severity: "medium".to_string(),
                    evidence: line.trim().to_string(),
                    guidance: "Throw paths in code with manual resource ownership should be audited for strong/basic exception guarantees.".to_string(),
                });
            }
        }
    }
    out
}
