//! MiniLang pre-edit risk diagnostics. Flags the footguns the language
//! reference documents as hazards, so an agent editing a `.ml` method gets
//! the same "what to watch out for" signal the VB/C#/C/C++/Rust modules
//! provide.

use regex::Regex;
use std::sync::LazyLock;

use super::LanguageDiagnostic;

/// The word following `Spawn`. The `regex` crate has no look-around, so
/// the "not Detached" condition is checked on the capture rather than in
/// the pattern. The root scope joins every non-detached child before exit,
/// so spawning a non-terminating fiber hangs the program.
static SPAWN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*Spawn\s+(\w+)").expect("ml spawn"));
/// Bare `Unsafe` grants every capability the compiler can reason about.
/// A capability-granular block is nearly always the right call.
static BARE_UNSAFE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*Unsafe\s*$").expect("ml bare unsafe"));
static ALLOC_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bStd\.Memory\.Alloc\s*\(").expect("ml alloc"));

pub fn detect(code_files: &[(&str, &str)]) -> Vec<LanguageDiagnostic> {
    let mut out = Vec::new();
    for (file, content) in code_files {
        let has_free = content.contains("Std.Memory.Free");
        let has_arena = content.contains("Using Arena");

        for (idx, line) in content.lines().enumerate() {
            let line_no = idx + 1;
            let trimmed = line.trim();
            // MiniLang accepts ', # and // as comment markers.
            if trimmed.starts_with('\'') || trimmed.starts_with('#') || trimmed.starts_with("//") {
                continue;
            }

            if let Some(c) = SPAWN_RE.captures(line) {
                if &c[1] != "Detached" {
                    out.push(LanguageDiagnostic {
                        location: format!("{file}:{line_no}"),
                        category: "non_detached_spawn".to_string(),
                        severity: "medium".to_string(),
                        evidence: trimmed.to_string(),
                        guidance: "The root scope joins this fiber before the program exits. If \
                                   it never terminates the program hangs — use `Spawn Detached` \
                                   for daemons, loggers, and watchdogs."
                            .to_string(),
                    });
                }
            }

            if BARE_UNSAFE_RE.is_match(line) {
                out.push(LanguageDiagnostic {
                    location: format!("{file}:{line_no}"),
                    category: "bare_unsafe".to_string(),
                    severity: "medium".to_string(),
                    evidence: trimmed.to_string(),
                    guidance: "Bare `Unsafe` grants every capability `All` covers. Narrow it to \
                               what the block actually needs — `Unsafe(RawPtr)`, \
                               `Unsafe(Alloc)`, `Unsafe(Asm)` — so a capability added in a later \
                               release does not silently widen this block."
                        .to_string(),
                });
            }

            if ALLOC_RE.is_match(line) && !has_free && !has_arena {
                out.push(LanguageDiagnostic {
                    location: format!("{file}:{line_no}"),
                    category: "unfreed_alloc".to_string(),
                    severity: "high".to_string(),
                    evidence: trimmed.to_string(),
                    guidance: "This file allocates with `Std.Memory.Alloc` but never calls \
                               `Std.Memory.Free` and opens no `Using Arena` scope. Pair the \
                               allocation with a free, or bump-allocate inside an arena."
                        .to_string(),
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::detect;

    #[test]
    fn flags_the_documented_minilang_footguns() {
        let src = "\
Function Logger() As Int
    While True
        Say 1
    End While
    Return 0
End Function
Function Boot() As Int
    Spawn Call Logger()
    Unsafe
        Say 2
    End Unsafe
    Var p As Int
    Set p To Std.Memory.Alloc(64)
    Return 0
End Function
";
        let files = vec![("Boot.ml", src)];
        let out = detect(&files);
        let cats: Vec<&str> = out.iter().map(|d| d.category.as_str()).collect();

        assert!(cats.contains(&"non_detached_spawn"), "got {cats:?}");
        assert!(cats.contains(&"bare_unsafe"), "got {cats:?}");
        assert!(cats.contains(&"unfreed_alloc"), "got {cats:?}");
    }

    #[test]
    fn clean_source_produces_no_findings() {
        let src = "\
Function Boot() As Int
    Spawn Detached Call Logger()
    Unsafe(RawPtr)
        Say 2
    End Unsafe
    Return 0
End Function
";
        let files = vec![("Clean.ml", src)];
        assert!(detect(&files).is_empty(), "clean source must not fire");
    }
}
