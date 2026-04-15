use regex::Regex;
use std::sync::LazyLock;

use super::LanguageDiagnostic;

static ASYNC_WITHOUT_CONFIGURE_AWAIT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bawait\s+[A-Za-z0-9_\.\(\)\[\],\s]+;").expect("csharp await")
});
static EVENT_SUBSCRIBE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\+=\s*(?:new\s+)?[A-Za-z0-9_\.]+\s*;?").expect("event sub"));
static EVENT_UNSUBSCRIBE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"-=\s*(?:new\s+)?[A-Za-z0-9_\.]+\s*;?").expect("event unsub"));
static DISPOSABLE_WITHOUT_USING_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(new\s+(SqlConnection|HttpClient|StreamReader|StreamWriter|FileStream|MemoryStream|CancellationTokenSource)\b)")
        .expect("disposable")
});

pub fn detect(code_files: &[(&str, &str)]) -> Vec<LanguageDiagnostic> {
    let mut out = Vec::new();
    for (file, content) in code_files {
        let lines: Vec<&str> = content.lines().collect();
        let has_unsub = EVENT_UNSUBSCRIBE_RE.is_match(content);

        for (idx, line) in lines.iter().enumerate() {
            let line_no = idx + 1;
            if ASYNC_WITHOUT_CONFIGURE_AWAIT_RE.is_match(line) && !line.contains("ConfigureAwait(")
            {
                out.push(LanguageDiagnostic {
                    location: format!("{}:{}", file, line_no),
                    category: "async_configure_await".to_string(),
                    severity: "medium".to_string(),
                    evidence: line.trim().to_string(),
                    guidance: "Library/shared code should usually use ConfigureAwait(false) to avoid context-capture deadlocks.".to_string(),
                });
            }

            if EVENT_SUBSCRIBE_RE.is_match(line) && !has_unsub {
                out.push(LanguageDiagnostic {
                    location: format!("{}:{}", file, line_no),
                    category: "event_leak_pattern".to_string(),
                    severity: "high".to_string(),
                    evidence: line.trim().to_string(),
                    guidance: "Event subscription without a matching unsubscription can leak long-lived publishers/subscribers.".to_string(),
                });
            }

            if DISPOSABLE_WITHOUT_USING_RE.is_match(line)
                && !line.contains("using")
                && !line.contains("await using")
            {
                out.push(LanguageDiagnostic {
                    location: format!("{}:{}", file, line_no),
                    category: "idisposable_misuse".to_string(),
                    severity: "high".to_string(),
                    evidence: line.trim().to_string(),
                    guidance: "Wrap IDisposable instances in using/await using or ensure deterministic Dispose() ownership.".to_string(),
                });
            }
        }
    }
    out
}
