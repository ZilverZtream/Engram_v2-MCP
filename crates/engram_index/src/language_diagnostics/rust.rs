use regex::Regex;
use std::sync::LazyLock;

use super::LanguageDiagnostic;

static UNWRAP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\.(unwrap|expect)\s*\(").expect("unwrap"));
static PANIC_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(panic!|todo!|unimplemented!)").expect("panic"));
static BLOCKING_IN_ASYNC_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(std::thread::sleep|std::fs::|std::net::TcpStream|reqwest::blocking)")
        .expect("blocking")
});

pub fn detect(code_files: &[(&str, &str)]) -> Vec<LanguageDiagnostic> {
    let mut out = Vec::new();
    for (file, content) in code_files {
        let in_async_file = content.contains("async fn");
        for (idx, line) in content.lines().enumerate() {
            let line_no = idx + 1;
            if UNWRAP_RE.is_match(line) {
                out.push(LanguageDiagnostic {
                    location: format!("{}:{}", file, line_no),
                    category: "unwrap_panic_hotspot".to_string(),
                    severity: "medium".to_string(),
                    evidence: line.trim().to_string(),
                    guidance: "Prefer explicit error propagation (? / thiserror) on operational paths instead of unwrap/expect.".to_string(),
                });
            }
            if PANIC_RE.is_match(line) {
                out.push(LanguageDiagnostic {
                    location: format!("{}:{}", file, line_no),
                    category: "panic_hotspot".to_string(),
                    severity: "high".to_string(),
                    evidence: line.trim().to_string(),
                    guidance: "Avoid panic-driven control flow in production paths; return typed errors where possible.".to_string(),
                });
            }
            if in_async_file && BLOCKING_IN_ASYNC_RE.is_match(line) {
                out.push(LanguageDiagnostic {
                    location: format!("{}:{}", file, line_no),
                    category: "blocking_in_async".to_string(),
                    severity: "high".to_string(),
                    evidence: line.trim().to_string(),
                    guidance: "Use async-compatible APIs or spawn_blocking to avoid stalling async executors.".to_string(),
                });
            }
            if line.contains("unsafe") {
                out.push(LanguageDiagnostic {
                    location: format!("{}:{}", file, line_no),
                    category: "unsafe_boundary_check".to_string(),
                    severity: "medium".to_string(),
                    evidence: line.trim().to_string(),
                    guidance: "Document safety invariants and keep unsafe blocks minimal with explicit boundary checks.".to_string(),
                });
            }
        }
    }
    out
}
