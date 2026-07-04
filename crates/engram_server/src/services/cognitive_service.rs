//! Cognitive service: dreaming, immune system, style mimicry, temporal coupling.
//!
//! Ported from v1 Python:
//!  - dreaming.py  → dream_project(), analyze_file_style(), find_temporal_couplings()
//!  - generation.py → LLM-backed insight generation (via DreamingEngine)
//!  - immune actor  → anti-pattern indexing (see actors/immune.rs)

use crate::actors::dreamer::{dream_once, record_cooccurrence};
use crate::state::{AppState, SearchHitLite};
use engram_core::RelPath;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Dreaming
// ---------------------------------------------------------------------------

/// Trigger a manual dream cycle for a project.
///
/// Equivalent to v1's `dream_project()` tool handler.
/// Returns the number of new insights generated.
pub async fn dream_project(state: &AppState, project_id: &str) -> anyhow::Result<usize> {
    dream_once(state, project_id, 2, 3, 5).await
}

/// Record a batch of search hits as co-occurrence data (feeds the dreamer).
pub async fn record_search_session(
    state: &AppState,
    project_id: &str,
    hits: &[SearchHitLite],
) -> anyhow::Result<()> {
    record_cooccurrence(state, project_id, hits).await
}

// ---------------------------------------------------------------------------
// Style analysis (v1 dreaming.py::analyze_file_style)
// ---------------------------------------------------------------------------

/// Result of a style analysis for a single file.
#[derive(Debug, Clone)]
pub struct StyleAnalysisResult {
    /// Actionable style guide bullets (None if insufficient data).
    pub style_guide: Option<String>,
    /// Commit hashes that were analyzed.
    pub analyzed_commits: Vec<String>,
    /// File path that was analyzed.
    pub file_path: String,
    /// Error message if analysis failed.
    pub error: Option<String>,
}

/// Analyze the coding style of a file from its recent git history.
///
/// Uses recent diffs + LLM (if configured) to extract naming conventions,
/// error-handling patterns, import style, etc. — identical to v1's
/// `analyze_file_style()` in dreaming.py.
pub async fn analyze_file_style(
    state: &AppState,
    project_id: &str,
    file_path: &str,
    diff_limit: usize,
) -> StyleAnalysisResult {
    // Locate the project directory.
    let pid = project_id.to_string();
    let reg = state.registry.clone();
    let rec = match tokio::task::spawn_blocking(move || reg.get_project(&pid))
        .await
        .unwrap_or_else(|_| Ok(None))
    {
        Ok(Some(r)) => r,
        _ => {
            return StyleAnalysisResult {
                style_guide: None,
                analyzed_commits: Vec::new(),
                file_path: file_path.to_string(),
                error: Some(format!("project '{project_id}' not found")),
            };
        }
    };

    let directory = std::path::PathBuf::from(&rec.directory);
    let fp = file_path.to_string();
    let limit = diff_limit;

    // CPU-bound git I/O.
    let diffs: Vec<(String, String, String)> =
        tokio::task::spawn_blocking(move || collect_file_diffs(&directory, &fp, limit))
            .await
            .unwrap_or_else(|_| Ok(Vec::new()))
            .unwrap_or_default();

    // Always read the current file content — the mimicry engine's
    // language-specific detectors and the new static fallback both need
    // the full file body to produce a rich guide. Before this fix the
    // handler passed the short `file_path` string into `current_file`
    // (misusing the parameter contract) so mimicry only ever saw the
    // diff text, and for stable files like `sharedfunc.vb` the git
    // history is thin — yielding a near-empty style guide.
    let file_content: Option<String> = {
        let dir = std::path::PathBuf::from(&rec.directory);
        let fp = file_path.to_string();
        tokio::task::spawn_blocking(move || match engram_core::safe_join(&dir, &fp) {
            Ok(full) => std::fs::read_to_string(full).ok(),
            Err(_) => None,
        })
        .await
        .ok()
        .flatten()
    };

    if diffs.is_empty() && file_content.is_none() {
        return StyleAnalysisResult {
            style_guide: None,
            analyzed_commits: Vec::new(),
            file_path: file_path.to_string(),
            error: Some(
                "No git history found and file could not be read for static fallback".into(),
            ),
        };
    }

    let analyzed_commits: Vec<String> = diffs.iter().map(|(h, _, _)| h.clone()).collect();

    // Build a combined diff text block (same as v1).
    let mut diffs_text = String::new();
    for (commit_hash, message, diff_content) in &diffs {
        let truncated = if diff_content.len() > 2000 {
            format!("{}\n... (truncated)", &diff_content[..2000])
        } else {
            diff_content.clone()
        };
        diffs_text.push_str(&format!(
            "Commit: {}\nMessage: {}\nDiff:\n{}\n{}\n",
            &commit_hash[..8.min(commit_hash.len())],
            message,
            truncated,
            "-".repeat(40)
        ));
    }

    // Try the AST/regex-based mimicry engine first (always available).
    // Pass the actual file CONTENT as `current_file` (not the file path,
    // which was the old bug) so the mimicry engine can analyse the body
    // even when git history is shallow. Then optionally enhance with LLM.
    let diff_snippets: Vec<String> = diffs.iter().map(|(_, _, d)| d.clone()).collect();
    let mimicry_guide = state
        .mimicry
        .analyze(&diff_snippets, file_content.as_deref())
        .bullets;

    // Static fallback: when git history is shallow (< 5 diffs) and we
    // have the file body, run a light language-specific static pass
    // that emits concrete patterns (Optional-param shape, Using block
    // discipline, Is-Nothing guards, etc.). Static bullets are MERGED
    // after git/mimicry bullets — git signals take priority because
    // they represent active choices by the team, static fills the gaps.
    const SHALLOW_HISTORY_THRESHOLD: usize = 5;
    let mut merged_bullets: Vec<String> = mimicry_guide;
    if diffs.len() < SHALLOW_HISTORY_THRESHOLD
        && let Some(ref content) = file_content
    {
        let static_bullets = static_analyze_file_style(content, file_path);
        for b in static_bullets {
            // Dedup on exact bullet text — cheap because counts are small.
            if !merged_bullets.iter().any(|existing| existing == &b) {
                merged_bullets.push(b);
            }
        }
    }

    let mimicry_combined = merged_bullets.join("\n");

    // Try LLM enhancement with the style-analysis prompt.
    let llm_guide = try_llm_style_analysis(state, file_path, &diffs_text).await;

    // Merge policy:
    //
    // The deterministic mimicry + static-VB pass produces CONCRETE,
    // verifiable rules ("Optional db As iFaltDataContext = Nothing
    // appears in 14 methods"). The LLM pass produces NARRATIVE context
    // ("the file is a shared-helpers module; team prefers explicit
    // disposal via Using").
    //
    // These complement each other — discarding the rules when the LLM
    // is reachable (the previous behaviour) silently removed the most
    // actionable output from every style report on projects where an
    // LLM backend is configured. The merged output leads with rules
    // (so a downstream agent sees the actionable list first) and
    // appends the LLM narrative as supplementary context.
    let style_guide = match (llm_guide, mimicry_combined.is_empty()) {
        (Some(llm), true) => Some(llm),
        (Some(llm), false) => Some(format!(
            "{mimicry_combined}\n\n---\n### LLM Analysis\n{llm}"
        )),
        (None, false) => Some(mimicry_combined),
        (None, true) => None,
    };

    if style_guide.is_none() {
        return StyleAnalysisResult {
            style_guide: None,
            analyzed_commits,
            file_path: file_path.to_string(),
            error: Some("Insufficient data to determine style patterns".into()),
        };
    }

    StyleAnalysisResult {
        style_guide,
        analyzed_commits,
        file_path: file_path.to_string(),
        error: None,
    }
}

// ── Static fallback: language-aware pattern detection ────────────────────────
//
// Runs when git history is shallow. For VB.NET files we look for the
// OciusX-style patterns a reader would expect the style guide to call
// out — `Optional db As <Context> = Nothing`, `Using db …`, `Is Nothing`
// guards, Module vs Class declaration, Handles-clause discipline, etc.
// For C# / Rust / other we emit a generic pass. Every detector returns a
// single bullet; detectors that don't fire contribute nothing.

/// Entry point — dispatches on file extension. Every language that has
/// its own analyzer produces 5-15 quantified bullets on a real project;
/// anything that falls into the generic branch still gets a handful of
/// language-agnostic signals (indentation, comment density, identifier
/// casing, file size, line length).
pub fn static_analyze_file_style(content: &str, file_path: &str) -> Vec<String> {
    let lower = file_path.to_ascii_lowercase();
    if lower.ends_with(".vb") {
        static_analyze_vb(content)
    } else if lower.ends_with(".cs") {
        static_analyze_cs(content)
    } else if lower.ends_with(".ts") || lower.ends_with(".tsx") {
        static_analyze_typescript(content)
    } else if lower.ends_with(".js") || lower.ends_with(".jsx") || lower.ends_with(".mjs") {
        static_analyze_javascript(content)
    } else if lower.ends_with(".aspx") || lower.ends_with(".ascx") || lower.ends_with(".master") {
        static_analyze_aspx(content)
    } else if lower.ends_with(".sql") {
        static_analyze_sql(content)
    } else if lower.ends_with(".py") {
        static_analyze_python(content)
    } else if lower.ends_with(".rs") {
        static_analyze_rust(content)
    } else if lower.ends_with(".go") {
        static_analyze_go(content)
    } else if lower.ends_with(".java") {
        static_analyze_java(content)
    } else {
        static_analyze_generic(content)
    }
}

// ── Shared analyzer helpers ──────────────────────────────────────────────
//
// Every language-specific analyzer needs to classify identifier casing and
// report the dominant convention. Keeping these as free functions so each
// analyzer stays a thin composition rather than a re-implementation of
// the same majority-vote logic.

/// Bucket a single identifier into casing categories.
#[derive(Default, Debug, Clone, Copy)]
struct CasingCounts {
    pub pascal: u32,
    pub camel: u32,
    pub snake: u32,
    pub screaming: u32,
    pub other: u32,
}

impl CasingCounts {
    fn observe(&mut self, name: &str) {
        if name.is_empty() {
            return;
        }
        if name.contains('_') {
            if name.chars().all(|c| !c.is_ascii_lowercase()) {
                self.screaming += 1;
            } else {
                self.snake += 1;
            }
            return;
        }
        match name.chars().next() {
            Some(c) if c.is_ascii_uppercase() => self.pascal += 1,
            Some(c) if c.is_ascii_lowercase() => self.camel += 1,
            _ => self.other += 1,
        }
    }

    fn total(&self) -> u32 {
        self.pascal + self.camel + self.snake + self.screaming + self.other
    }

    /// Produce a `"{conv}: ({dominant}/{total})"` summary, or `None` when
    /// the sample is too small to report on (< 3 observations).
    fn dominant(&self) -> Option<(&'static str, u32, u32)> {
        let total = self.total();
        if total < 3 {
            return None;
        }
        let (label, count) = [
            ("PascalCase", self.pascal),
            ("camelCase", self.camel),
            ("snake_case", self.snake),
            ("SCREAMING_SNAKE", self.screaming),
        ]
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .unwrap_or(("unknown", 0));
        if count == 0 {
            return None;
        }
        Some((label, count, total))
    }
}

/// Cap regex iteration on huge files so a 50k-line monstrosity can't
/// blow our linear time budget. Every analyzer passes this into
/// `count_casing` and similar helpers.
const SCAN_LIMIT: usize = 500;

/// Count identifier occurrences for a regex whose first capture group
/// is the identifier name. Returns the counts + up to 3 sample names
/// for the dominant-report string.
fn count_casing(
    content: &str,
    ident_re: &regex::Regex,
    limit: usize,
) -> (CasingCounts, Vec<String>) {
    let mut counts = CasingCounts::default();
    let mut samples: Vec<String> = Vec::new();
    for cap in ident_re.captures_iter(content).take(limit) {
        if let Some(m) = cap.get(1) {
            let name = m.as_str();
            counts.observe(name);
            if samples.len() < 3 && !name.is_empty() {
                samples.push(name.to_string());
            }
        }
    }
    (counts, samples)
}

/// Format the "Naming: XxxCase (N/T). Examples: A, B, C" bullet. Returns
/// `None` when the sample is too small or no casing dominates.
fn format_casing_bullet(label: &str, counts: &CasingCounts, samples: &[String]) -> Option<String> {
    let (conv, count, total) = counts.dominant()?;
    let tail = if samples.is_empty() {
        String::new()
    } else {
        format!(". Examples: {}", samples.join(", "))
    };
    Some(format!("{label}: **{conv}** ({count}/{total}){tail}"))
}

/// Count how many times any of `needles` appears as a literal substring
/// in `content`. Cheap fallback for simple pattern detection where we
/// don't need a regex.
fn count_any(content: &str, needles: &[&str]) -> usize {
    needles.iter().map(|n| content.matches(n).count()).sum()
}

/// Given a list of `(label, count)` pairs, return the bullet string for
/// the winner relative to the runner-up, or `None` when either no option
/// has any matches or the total is too small to report on.
///
/// Threshold: a winner must have at least 2 hits AND at least 55% of the
/// total, otherwise we report `Mixed` instead.
fn format_popularity_bullet(prefix: &str, pairs: &[(&str, usize)], suffix: &str) -> Option<String> {
    let total: usize = pairs.iter().map(|(_, c)| *c).sum();
    if total < 2 {
        return None;
    }
    let (winner, best) = pairs.iter().max_by_key(|(_, c)| *c).copied()?;
    if best == 0 {
        return None;
    }
    let ratio = best as f32 / total as f32;
    let body = if ratio >= 0.55 {
        format!("**{winner}** ({best}/{total})")
    } else {
        format!("mixed ({total} total)")
    };
    Some(format!("{prefix}: {body}{suffix}"))
}

fn static_analyze_vb(content: &str) -> Vec<String> {
    use regex::Regex;
    use std::sync::LazyLock;

    static METHOD_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(r"(?im)^\s*(?:Public|Private|Protected|Friend|Shared|Overrides|Overridable|Async|Partial)?\s*(?:Public|Private|Protected|Friend|Shared|Overrides|Overridable|Async|Partial)?\s*(?:Sub|Function)\s+(\w+)\s*\(")
            .ok()
    });
    static OPTIONAL_CONTEXT_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(r#"(?i)\bOptional\s+(?:ByVal\s+|ByRef\s+)?(\w+)\s+As\s+(\w+(?:DataContext|Context|Db))\s*=\s*Nothing"#)
            .ok()
    });
    static USING_CONTEXT_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(
            r#"(?im)^\s*Using\s+\w+(?:\s+As\s+(?:New\s+)?\w+|\s*=\s*(?:If\(.+?,\s*New\s+\w+)?)"#,
        )
        .ok()
    });
    static IS_NOTHING_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?i)\bIf\s+\w[\w.]*\s+Is\s+Nothing\b").ok());
    static ON_ERROR_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?im)^\s*On\s+Error\s+Resume\s+Next\b").ok());
    static TRY_CATCH_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?im)^\s*Try\s*$|^\s*Catch\b").ok());
    static MODULE_DECL_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?im)^\s*(?:Public\s+|Friend\s+)?Module\s+(\w+)").ok());
    static CLASS_DECL_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(r"(?im)^\s*(?:Public\s+|Friend\s+|Partial\s+)*Class\s+(\w+)").ok()
    });
    static HANDLES_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?im)\bHandles\s+\w[\w.]*(?:\s*,\s*\w[\w.]*)*").ok());
    static SAFEREDIRECT_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?im)SafeRedirect\s*\([^\)]*\)\s*\n?\s*Return\b").ok());
    static XML_DOC_RE: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"(?m)^\s*'''").ok());
    static IMPORTS_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?im)^\s*Imports\s+([\w.]+)").ok());

    let mut bullets = Vec::new();

    // Method naming convention (PascalCase / camelCase / other).
    if let Some(re) = METHOD_RE.as_ref() {
        let mut pascal = 0u32;
        let mut camel = 0u32;
        let mut other = 0u32;
        let mut samples: Vec<String> = Vec::new();
        for cap in re.captures_iter(content).take(200) {
            if let Some(m) = cap.get(1) {
                let name = m.as_str();
                let first = name.chars().next().unwrap_or('_');
                if first.is_ascii_uppercase() {
                    pascal += 1;
                } else if first.is_ascii_lowercase() {
                    camel += 1;
                } else {
                    other += 1;
                }
                if samples.len() < 3 {
                    samples.push(name.to_string());
                }
            }
        }
        let total = pascal + camel + other;
        if total >= 3 {
            let (conv, count) = if pascal >= camel.max(other) {
                ("PascalCase", pascal)
            } else if camel >= pascal.max(other) {
                ("camelCase", camel)
            } else {
                ("mixed", other)
            };
            bullets.push(format!(
                "Method naming: **{conv}** ({count}/{total} methods). Examples: {}",
                samples.join(", ")
            ));
        }
    }

    // `Optional db As <DataContext> = Nothing` context-injection pattern.
    if let Some(re) = OPTIONAL_CONTEXT_RE.as_ref() {
        let mut hits: Vec<(String, String)> = Vec::new();
        for cap in re.captures_iter(content).take(20) {
            if let (Some(n), Some(t)) = (cap.get(1), cap.get(2)) {
                hits.push((n.as_str().into(), t.as_str().into()));
            }
        }
        if !hits.is_empty() {
            let (param, ctx) = &hits[0];
            bullets.push(format!(
                "Data-context injection: `Optional {param} As {ctx} = Nothing` — \
                 seen in {} method(s). New methods should follow the same shape.",
                hits.len()
            ));
        }
    }

    // Using-block discipline.
    if let Some(re) = USING_CONTEXT_RE.as_ref() {
        let using_count = re.find_iter(content).count();
        if using_count >= 2 {
            bullets.push(format!(
                "`Using` block for context ownership (seen {using_count} times). \
                 Never declare a bare `Dim db As New …Context` without a `Using`."
            ));
        }
    }

    // `Is Nothing` guard before LINQ access.
    if let Some(re) = IS_NOTHING_RE.as_ref() {
        let n = re.find_iter(content).count();
        if n >= 3 {
            bullets.push(format!(
                "Guard pattern: `If x Is Nothing Then …` (seen {n} times) — always \
                 validate nullable references before touching them."
            ));
        }
    }

    // Error-handling: On Error Resume Next vs Try/Catch.
    let on_error = ON_ERROR_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    let try_catch = TRY_CATCH_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    if try_catch > 0 && on_error == 0 {
        bullets.push(format!(
            "Error handling: **`Try/Catch` only** ({try_catch} occurrences). \
             Do NOT introduce `On Error Resume Next` — keep errors explicit."
        ));
    } else if on_error > 0 {
        bullets.push(format!(
            "Error handling: legacy `On Error Resume Next` present ({on_error}). \
             Flag as risk — prefer migrating to `Try/Catch`."
        ));
    }

    // Module vs Class declaration style.
    let has_module = MODULE_DECL_RE
        .as_ref()
        .and_then(|re| re.captures(content))
        .and_then(|cap| cap.get(1).map(|m| m.as_str().to_string()));
    let has_class = CLASS_DECL_RE
        .as_ref()
        .is_some_and(|re| re.is_match(content));
    if let Some(name) = has_module {
        bullets.push(format!(
            "Declaration style: **`Module {name}`** (shared helpers, no instance state)."
        ));
    } else if has_class {
        bullets.push("Declaration style: `Class` (instance state or inheritance)".into());
    }

    // `Handles` clauses — event wiring style.
    if let Some(re) = HANDLES_RE.as_ref() {
        let n = re.find_iter(content).count();
        if n >= 2 {
            bullets.push(format!(
                "Event wiring: `Handles` clauses ({n} occurrences) — prefer attaching \
                 handlers via `Handles` over `AddHandler` for this file's conventions."
            ));
        }
    }

    // `SafeRedirect(...) : Return` mandatory pair (OciusX convention).
    if let Some(re) = SAFEREDIRECT_RE.as_ref()
        && re.is_match(content)
    {
        bullets.push(
            "`SafeRedirect(...)` MUST be followed by `Return` on the next line — \
             the redirect doesn't short-circuit on its own."
                .into(),
        );
    }

    // XML-doc vs inline comment style.
    if let Some(re) = XML_DOC_RE.as_ref() {
        let doc_lines = re.find_iter(content).count();
        if doc_lines >= 3 {
            bullets.push(format!(
                "Documentation: XML doc comments (`'''`) on public API — {doc_lines} lines. \
                 Follow the same shape for new public methods."
            ));
        }
    }

    // Imports convention — top-N most common namespaces.
    if let Some(re) = IMPORTS_RE.as_ref() {
        let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for cap in re.captures_iter(content).take(100) {
            if let Some(m) = cap.get(1) {
                *counts.entry(m.as_str().to_string()).or_insert(0) += 1;
            }
        }
        if counts.len() >= 3 {
            let mut pairs: Vec<(String, usize)> = counts.into_iter().collect();
            pairs.sort_by(|a, b| b.1.cmp(&a.1));
            let top: Vec<String> = pairs.iter().take(4).map(|(n, _)| n.clone()).collect();
            bullets.push(format!(
                "Imports: top-level `Imports` directives — {}",
                top.join(", ")
            ));
        }
    }

    bullets
}

fn static_analyze_cs(content: &str) -> Vec<String> {
    use regex::Regex;
    use std::sync::LazyLock;

    static METHOD_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(
            r"(?m)^\s*(?:public|private|protected|internal|static|virtual|override|async|sealed|abstract|new|partial)\s+(?:[\w<>\[\],\?\s]+?\s+)?(\w+)\s*\(",
        )
        .ok()
    });
    static CLASS_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(
            r"(?m)^\s*(?:public\s+|internal\s+|sealed\s+|abstract\s+|partial\s+)*class\s+(\w+)",
        )
        .ok()
    });
    static RECORD_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(r"(?m)^\s*(?:public\s+|internal\s+)*record(?:\s+struct)?\s+(\w+)").ok()
    });
    static SWITCH_EXPR_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"\bswitch\s*\{").ok());
    static PATTERN_IS_RE: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"\bis\s+\{").ok());

    let mut bullets = Vec::new();

    // Naming convention.
    if let Some(re) = METHOD_RE.as_ref() {
        let (counts, samples) = count_casing(content, re, SCAN_LIMIT);
        if let Some(b) = format_casing_bullet("Method naming", &counts, &samples) {
            bullets.push(b);
        }
    }
    if let Some(re) = CLASS_RE.as_ref() {
        let (counts, samples) = count_casing(content, re, SCAN_LIMIT);
        if let Some(b) = format_casing_bullet("Class / interface naming", &counts, &samples) {
            bullets.push(b);
        }
    }

    // using-statement style (classic `using (…)` vs the newer `using var x =`).
    let using_paren = content.matches("using (").count();
    let using_var = content.matches("using var ").count();
    if let Some(b) = format_popularity_bullet(
        "Resource ownership",
        &[
            ("`using (...)` block", using_paren),
            ("`using var x = …`", using_var),
        ],
        " — IDisposable handles consistently scoped",
    ) {
        bullets.push(b);
    }

    // Async style.
    let async_task = content.matches("async Task").count();
    let async_valuetask = content.matches("async ValueTask").count();
    let configure_false = content.matches(".ConfigureAwait(false)").count();
    if async_task + async_valuetask > 0 {
        let mut note =
            format!("Async: `async Task` ({async_task}) / `async ValueTask` ({async_valuetask})");
        if configure_false > 0 {
            note.push_str(&format!(
                ", `ConfigureAwait(false)` used {configure_false} times"
            ));
        }
        bullets.push(note);
    }

    // Null handling.
    let null_coalesce = content.matches(" ?? ").count();
    let null_prop = content.matches("?.").count();
    let is_null = content.matches(" is null").count();
    let eq_null = content.matches(" == null").count();
    if let Some(b) = format_popularity_bullet(
        "Null checks",
        &[
            ("`is null`", is_null),
            ("`== null`", eq_null),
            ("`?.` null-propagation", null_prop),
            ("`??` null-coalesce", null_coalesce),
        ],
        "",
    ) {
        bullets.push(b);
    }

    // LINQ style — method syntax vs query syntax.
    let linq_method = count_any(
        content,
        &[".Where(", ".Select(", ".FirstOrDefault(", ".Any("],
    );
    let linq_query = content
        .matches("from ")
        .count()
        .min(content.matches(" in ").count())
        .min(content.matches(" select ").count());
    if let Some(b) = format_popularity_bullet(
        "LINQ",
        &[
            ("method syntax (`.Where().Select()`)", linq_method),
            ("query syntax (`from x in …`)", linq_query),
        ],
        "",
    ) {
        bullets.push(b);
    }

    // String interpolation.
    let interp = content.matches(r#"$""#).count();
    let format_call = count_any(content, &["String.Format(", "string.Format("]);
    let plus_concat = count_any(content, &[r#"" + "#, r#" + ""#]);
    if let Some(b) = format_popularity_bullet(
        "String building",
        &[
            ("interpolated `$\"...\"`", interp),
            ("`String.Format`", format_call),
            ("`+` concatenation", plus_concat),
        ],
        "",
    ) {
        bullets.push(b);
    }

    // Records (modern C# 9+).
    if let Some(re) = RECORD_RE.as_ref() {
        let n = re.find_iter(content).count();
        if n > 0 {
            bullets.push(format!(
                "Records: `record` / `record struct` ({n} declared) — modern C# 9+ pattern"
            ));
        }
    }

    // Pattern matching — switch expressions + `is {}`.
    let switch_expr = SWITCH_EXPR_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    let pat_is = PATTERN_IS_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    if switch_expr + pat_is > 0 {
        bullets.push(format!(
            "Pattern matching: {switch_expr} switch-expressions, {pat_is} `is {{…}}` checks"
        ));
    }

    // Dependency injection via constructor parameters — rough signal.
    let di_collection = count_any(content, &["IServiceCollection", "IServiceProvider"]);
    if di_collection > 0 {
        bullets.push(format!(
            "Dependency injection: `IServiceCollection` / `IServiceProvider` used ({di_collection} refs) — DI via constructor is the registered pattern"
        ));
    }

    bullets
}

// ── TypeScript ───────────────────────────────────────────────────────────
fn static_analyze_typescript(content: &str) -> Vec<String> {
    // TypeScript layers on JavaScript — inherit every JS bullet, then add
    // the TS-specific detectors (types, triple-slash, casts, non-null,
    // generics, decorators, ambient declarations).
    let mut bullets = static_analyze_javascript_common(content, /* typescript */ true);

    use regex::Regex;
    use std::sync::LazyLock;

    static TRIPLE_SLASH_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r#"(?m)^///\s*<reference\s+path="([^"]+)""#).ok());
    static INTERFACE_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?m)^\s*(?:export\s+)?interface\s+(\w+)").ok());
    static TYPE_ALIAS_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?m)^\s*(?:export\s+)?type\s+(\w+)\s*=").ok());
    static ENUM_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?m)^\s*(?:export\s+)?(?:const\s+)?enum\s+(\w+)").ok());
    static ANY_RE: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r":\s*any\b").ok());
    static AS_ANY_RE: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"\bas\s+any\b").ok());
    static AS_UNKNOWN_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"\bas\s+unknown\b").ok());
    static NON_NULL_ASSERT_RE: LazyLock<Option<Regex>> =
        // `foo!.bar`, `foo!(…)`, `foo![…]`. Avoid matching `!==`, `!=`, or `!x`.
        LazyLock::new(|| Regex::new(r"[\w\)\]]!(?:\.|\(|\[)").ok());
    static TYPED_PARAM_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r":\s*(?:string|number|boolean|void|unknown|[A-Z]\w*)\b").ok());
    static READONLY_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"\breadonly\s+\w+").ok());
    static GENERIC_FN_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(r"(?m)(?:function\s+\w+|<)\s*<([A-Z]\w*(?:\s+extends\s+[^>]+)?)(?:,\s*[A-Z]\w*(?:\s+extends\s+[^>]+)?)*>").ok()
    });
    static DECORATOR_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?m)^\s*@(\w+)\s*(?:\(|$)").ok());
    static DECLARE_GLOBAL_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(r"(?m)^\s*declare\s+(?:global|namespace|module|const|let|var|function|class)\b")
            .ok()
    });

    // ── Module system: ES6 `import` vs triple-slash references (OciusX) ──
    let es6_imports = content.matches("import {").count()
        + content.matches("import *").count()
        + content.matches("import type ").count()
        + content.matches("import ").count().saturating_sub(
            content.matches("import {").count()
                + content.matches("import *").count()
                + content.matches("import type ").count(),
        );
    let mut triple_paths: Vec<String> = Vec::new();
    if let Some(re) = TRIPLE_SLASH_RE.as_ref() {
        for cap in re.captures_iter(content).take(20) {
            if let Some(m) = cap.get(1) {
                triple_paths.push(m.as_str().to_string());
            }
        }
    }
    if !triple_paths.is_empty() && triple_paths.len() >= es6_imports {
        // OciusX-flavored: triple-slash dominates — cite the referenced paths.
        let sample: Vec<String> = triple_paths
            .iter()
            .take(3)
            .map(|p| format!("`{p}`"))
            .collect();
        bullets.push(format!(
            "Module system: **triple-slash references** ({} directive(s), refs incl. {}) — \
             this file does NOT use ES6 `import`. New code added here must use the same \
             `/// <reference path=\"…\">` mechanism; do not introduce `import`/`require`.",
            triple_paths.len(),
            sample.join(", ")
        ));
    } else if es6_imports > 0 {
        bullets.push(format!(
            "Module system: ES6 `import` ({es6_imports} statements) — follow the same \
             import-based loading for new code."
        ));
    }

    // ── Interface / type / enum counts + preference cue ──────────────────
    let interface_count = INTERFACE_RE
        .as_ref()
        .map(|re| re.find_iter(content).take(SCAN_LIMIT).count())
        .unwrap_or(0);
    let type_alias_count = TYPE_ALIAS_RE
        .as_ref()
        .map(|re| re.find_iter(content).take(SCAN_LIMIT).count())
        .unwrap_or(0);
    let enum_count = ENUM_RE
        .as_ref()
        .map(|re| re.find_iter(content).take(SCAN_LIMIT).count())
        .unwrap_or(0);
    if interface_count + type_alias_count + enum_count > 0 {
        let advice = if interface_count > type_alias_count + 1 {
            " — file prefers `interface` for object shapes; keep new shapes as `interface` \
             unless a union/intersection forces `type`."
        } else if type_alias_count > interface_count + 1 {
            " — file prefers `type` aliases; match the same style."
        } else {
            ""
        };
        bullets.push(format!(
            "Types: {interface_count} `interface`, {type_alias_count} `type`, {enum_count} \
             `enum`{advice}"
        ));
    }

    // ── Typed-annotation density + `any` risk flag ───────────────────────
    let typed_params = TYPED_PARAM_RE
        .as_ref()
        .map(|re| re.find_iter(content).take(SCAN_LIMIT).count())
        .unwrap_or(0);
    let any_usage = ANY_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    if typed_params > 5 {
        bullets.push(format!(
            "Type annotations: {typed_params} typed params/returns — every new \
             parameter AND return must carry an explicit type."
        ));
    }
    if any_usage >= 3 {
        bullets.push(format!(
            "TYPE RISK: `: any` escape hatches — {any_usage} site(s). Do NOT introduce new \
             `: any`; prefer `unknown` + narrowing, or define the actual interface."
        ));
    }

    // ── Cast-based type erasure: `as any` / `as unknown` ─────────────────
    let as_any = AS_ANY_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    let as_unknown = AS_UNKNOWN_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    if as_any + as_unknown > 0 {
        bullets.push(format!(
            "TYPE RISK: type-erasing casts — {as_any} `as any`, {as_unknown} `as unknown`. \
             Each is a compile-time escape hatch; don't copy the pattern into new code."
        ));
    }

    // ── Non-null assertion `!` risk ──────────────────────────────────────
    let non_null = NON_NULL_ASSERT_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    if non_null >= 3 {
        bullets.push(format!(
            "TYPE RISK: non-null assertions `x!.y` — {non_null} site(s). Each bypasses the \
             null check; prefer `if (x) …` or optional chaining `x?.y`."
        ));
    }

    // ── `readonly` discipline ────────────────────────────────────────────
    let readonly = READONLY_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    if readonly >= 3 {
        bullets.push(format!(
            "Immutability: {readonly} `readonly` field(s) — maintain the same discipline on \
             new interface/class properties."
        ));
    }

    // ── Generics usage ───────────────────────────────────────────────────
    let generics = GENERIC_FN_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    if generics >= 2 {
        bullets.push(format!(
            "Generics: {generics} generic function/type declaration(s) — parametrise new \
             utility functions the same way (`<T extends …>`) rather than retyping."
        ));
    }

    // ── Decorators (common in Angular / NestJS / legacy TS) ──────────────
    let mut decorator_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    if let Some(re) = DECORATOR_RE.as_ref() {
        for cap in re.captures_iter(content).take(100) {
            if let Some(m) = cap.get(1) {
                *decorator_counts.entry(m.as_str().to_string()).or_insert(0) += 1;
            }
        }
    }
    if !decorator_counts.is_empty() {
        let mut pairs: Vec<(String, usize)> = decorator_counts.into_iter().collect();
        pairs.sort_by(|a, b| b.1.cmp(&a.1));
        let top: Vec<String> = pairs
            .iter()
            .take(3)
            .map(|(n, c)| format!("`@{n}` ({c})"))
            .collect();
        bullets.push(format!(
            "Decorators: {} — new classes should follow the same decorator conventions.",
            top.join(", ")
        ));
    }

    // ── Ambient declarations (`declare global`, etc.) ────────────────────
    let declares = DECLARE_GLOBAL_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    if declares > 0 {
        bullets.push(format!(
            "Ambient declarations: {declares} `declare …` block(s) — this file bridges \
             untyped runtime state into TS. Do not collapse or rewrite without checking \
             every call site."
        ));
    }

    // ── I-prefix interface naming convention ─────────────────────────────
    // Universal stylistic choice (C#-influenced codebases use I-prefix; modern
    // React/Node TS does not). Just report the file's own convention.
    if let Some(re) = INTERFACE_RE.as_ref() {
        let mut i_prefixed: u32 = 0;
        let mut plain: u32 = 0;
        let mut samples_i: Vec<String> = Vec::new();
        for cap in re.captures_iter(content).take(SCAN_LIMIT) {
            if let Some(m) = cap.get(1) {
                let name = m.as_str();
                let chars: Vec<char> = name.chars().collect();
                let is_ipref = chars.len() >= 2 && chars[0] == 'I' && chars[1].is_ascii_uppercase();
                if is_ipref {
                    i_prefixed += 1;
                    if samples_i.len() < 3 {
                        samples_i.push(name.to_string());
                    }
                } else {
                    plain += 1;
                }
            }
        }
        let total = i_prefixed + plain;
        if total >= 3 {
            if i_prefixed > plain {
                let tail = if samples_i.is_empty() {
                    String::new()
                } else {
                    format!(" (e.g. {})", samples_i.join(", "))
                };
                bullets.push(format!(
                    "Interface naming: **`I`-prefix convention** ({i_prefixed}/{total}){tail} — \
                     new interfaces in this file should also start with `I`."
                ));
            } else if plain > i_prefixed * 2 {
                bullets.push(format!(
                    "Interface naming: **no `I`-prefix** ({plain}/{total} plain PascalCase) — \
                     do NOT add `I` prefixes to new interfaces in this file."
                ));
            }
        }
    }

    // ── Cast style: angle-bracket `<T>expr` vs modern `expr as T` ────────
    // Angle-bracket casts collide with JSX and are treated as legacy by TS
    // style guides (Microsoft / tslint recommend `as`). Cite whichever the
    // file uses and prescribe accordingly.
    static ANGLE_CAST_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        // `<HTMLSpanElement>elem` / `<any>window` — heuristic: angle-bracket
        // name followed directly by an identifier. Avoid matching generics
        // (which have a `,` or `extends` inside).
        Regex::new(r"<(?:HTML\w+|SVG\w+|any|unknown|[A-Z]\w*(?:\s*\[\s*\])?)>[\w\(]").ok()
    });
    static AS_CAST_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"\bas\s+(?:HTML\w+|SVG\w+|[A-Z]\w*(?:\s*\[\s*\])?)\b").ok());
    let angle_casts = ANGLE_CAST_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    let as_casts = AS_CAST_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    if angle_casts + as_casts >= 4 {
        if angle_casts > as_casts + 2 {
            bullets.push(format!(
                "Cast style: **legacy angle-bracket** `<HTMLElement>x` ({angle_casts}, vs \
                 {as_casts} `as`-casts) — match the file's existing style, but prefer \
                 `x as HTMLElement` in brand-new files (angle-bracket clashes with JSX)."
            ));
        } else if as_casts > angle_casts + 2 {
            bullets.push(format!(
                "Cast style: **modern `expr as T`** ({as_casts}, vs {angle_casts} angle-bracket) \
                 — do not introduce angle-bracket casts."
            ));
        }
    }

    // ── Constructor DI / parameter properties ────────────────────────────
    // `constructor(private foo: FooService, readonly bar: BarService)` — the
    // Angular/NestJS canonical DI pattern.
    static PARAM_PROP_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(r"constructor\s*\(\s*(?:public|private|protected|readonly|\s)+\w+\s*:").ok()
    });
    let di_ctor = PARAM_PROP_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    if di_ctor > 0 {
        bullets.push(format!(
            "Dependency injection: {di_ctor} constructor(s) use **parameter-property DI** \
             (`constructor(private x: X, …)`) — follow the same shape when adding collaborators."
        ));
    }

    bullets
}

// ── JavaScript ───────────────────────────────────────────────────────────
fn static_analyze_javascript(content: &str) -> Vec<String> {
    static_analyze_javascript_common(content, /* typescript */ false)
}

/// Shared JS/TS detectors. Designed to reach the same depth as `static_analyze_vb`:
/// every bullet names what was found AND what to do about it (prescriptive),
/// not just "X appears N times" (descriptive).
///
/// `typescript` is true when the caller is TS — a few bullets phrase
/// differently (e.g. `"use strict"` is irrelevant in TS).
fn static_analyze_javascript_common(content: &str, typescript: bool) -> Vec<String> {
    use regex::Regex;
    use std::sync::LazyLock;

    // ── core shape ────────────────────────────────────────────────────────
    static FUNC_DECL_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(r"(?m)^\s*(?:export\s+)?(?:async\s+)?function\s+(\w+)\s*\(").ok()
    });
    static ARROW_CONST_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(r"(?m)^\s*(?:export\s+)?const\s+(\w+)\s*=\s*(?:async\s+)?\(").ok()
    });
    static CLASS_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?m)^\s*(?:export\s+)?class\s+(\w+)").ok());
    static STRICT_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r#"(?m)^\s*["']use strict["']\s*;?\s*$"#).ok());

    // ── top-file declaration + imports (parity with VB `Module` / `Imports`) ─
    static NAMESPACE_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?m)^\s*(?:export\s+)?namespace\s+([\w.]+)").ok());
    static MODULE_DECL_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?m)^\s*(?:export\s+)?module\s+([\w.]+)\s*\{").ok());
    static ES6_IMPORT_FROM_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(r#"(?m)^\s*import\s+(?:[\w*{},\s]+\s+from\s+)?['"]([^'"]+)['"]"#).ok()
    });
    static REQUIRE_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r#"require\(\s*['"]([^'"]+)['"]\s*\)"#).ok());

    // ── null / guard / risk ──────────────────────────────────────────────
    static EARLY_GUARD_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        // `if (!x) return …;` and `if (x == null) return …;` — the
        // JS/TS analogue of VB's `If x Is Nothing Then Return` guard.
        Regex::new(r#"(?m)^\s*if\s*\(\s*!?[\w.\[\]]+(?:\s*(?:==|!=|===|!==)\s*(?:null|undefined|''|""))?\s*\)\s*(?:return|continue|break|throw)\b"#).ok()
    });
    static EMPTY_CATCH_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        // Empty `catch` block — swallowed error, risk flag.
        Regex::new(r"catch\s*(?:\([^)]*\))?\s*\{\s*\}").ok()
    });
    static EMPTY_CATCH_CB_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        // `.catch(() => {})` / `.catch(function(){})` — silently swallowed.
        Regex::new(r"\.catch\(\s*(?:function\s*\([^)]*\)\s*\{\s*\}|\([^)]*\)\s*=>\s*\{\s*\})\s*\)")
            .ok()
    });
    static CONSOLE_LOG_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"\bconsole\.(?:log|debug|info|warn|error)\(").ok());

    // ── security risks (no direct VB analogue — web-specific) ────────────
    static EVAL_RE: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"\beval\s*\(").ok());
    static NEW_FUNCTION_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"\bnew\s+Function\s*\(").ok());
    static INNER_HTML_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"\.innerHTML\s*(?:=|\+=)\s*[^;]+").ok());
    static DOC_WRITE_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"\bdocument\.write\s*\(").ok());

    // ── docs / JSDoc ─────────────────────────────────────────────────────
    static JSDOC_RE: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"(?m)^\s*/\*\*").ok());

    // ── Legacy ASP.NET WebForms bridge (fires anywhere `__doPostBack` is used) ─
    static DOPOSTBACK_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"__doPostBack\s*\(").ok());

    // ── private-underscore field convention (common OO style across JS/TS) ─
    static UNDERSCORE_FIELD_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(r"(?m)^\s*(?:private|protected|public|static|readonly|\s)*\s*_\w+\s*[:=]").ok()
    });
    static UNDERSCORE_THIS_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"\bthis\._\w+\b").ok());

    // ── section-header comment decoration ─────────────────────────────────
    static SECTION_HEADER_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?m)^\s*(?://|/\*)\s*[-=*]{5,}").ok());

    // ── framework signals (universal — React / Vue / Angular / Node / test / RxJS) ─
    static JSX_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        // JSX-only — distinguish from angle-bracket type casts `<T>x`. JSX
        // tag opens require *something after the name*: an attribute
        // (`<Foo key=`), a namespaced name (`<Foo.Bar`), a self-close
        // (`<Foo />`), or a spread (`<Foo {...props}>`). Plain `<Foo>x` is
        // a cast, not JSX.
        Regex::new(r"<[A-Z]\w*(?:\s+\w+=|\s*/>|\.\w+\s*[> /]|\s+\{\.\.\.)").ok()
    });

    // ── transpiled-TS fingerprint (for .js files generated by `tsc`) ──────
    static TRANSPILED_TS_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(r"\bvar\s+(?:__extends|__assign|__awaiter|__generator|__createBinding|__importDefault)\s*=").ok()
    });

    // ── missing-await detection (async fn body scans below) ──────────────
    static ASYNC_FN_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        // Capture the function body opening brace — we'll scan forward for `await`.
        Regex::new(
            r"(?m)\basync\s+(?:function\s*\w*\s*\([^)]*\)|\([^)]*\)\s*=>|\w+\s*\([^)]*\))\s*\{",
        )
        .ok()
    });

    let mut bullets = Vec::new();

    // ── 1. Top-file declaration (namespace / class / module) ─────────────
    // Matches VB's "Declaration style: **`Module sharedfunc`**" bullet.
    let top_ns = NAMESPACE_RE
        .as_ref()
        .and_then(|re| re.captures(content))
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
    let top_module = MODULE_DECL_RE
        .as_ref()
        .and_then(|re| re.captures(content))
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
    let top_class = CLASS_RE
        .as_ref()
        .and_then(|re| re.captures(content))
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
    if let Some(name) = top_ns {
        bullets.push(format!(
            "Declaration style: **`namespace {name}`** — legacy module pattern; new code should be \
             added inside the same namespace unless explicitly splitting."
        ));
    } else if let Some(name) = top_module {
        bullets.push(format!(
            "Declaration style: **`module {name}`** (ambient/legacy) — new code should match \
             the same module shape."
        ));
    } else if let Some(name) = top_class {
        bullets.push(format!(
            "Declaration style: **`class {name}`** — instance-based. Add new logic as methods on \
             this class rather than free functions."
        ));
    }

    // ── 2. Function naming ───────────────────────────────────────────────
    let mut fn_counts = CasingCounts::default();
    let mut fn_samples: Vec<String> = Vec::new();
    for re in [FUNC_DECL_RE.as_ref(), ARROW_CONST_RE.as_ref()]
        .iter()
        .flatten()
    {
        for cap in re.captures_iter(content).take(SCAN_LIMIT) {
            if let Some(m) = cap.get(1) {
                let name = m.as_str();
                fn_counts.observe(name);
                if fn_samples.len() < 3 {
                    fn_samples.push(name.to_string());
                }
            }
        }
    }
    if let Some(b) = format_casing_bullet("Function naming", &fn_counts, &fn_samples) {
        bullets.push(format!("{b}. Follow the same casing for new methods."));
    }

    // ── 3. Class naming (separate bullet — almost always PascalCase) ─────
    if let Some(re) = CLASS_RE.as_ref() {
        let (counts, samples) = count_casing(content, re, SCAN_LIMIT);
        if let Some(b) = format_casing_bullet("Class naming", &counts, &samples) {
            bullets.push(b);
        }
    }

    // ── 4. Variable declarations with legacy-risk advice ─────────────────
    let var_count = count_lines_starting_with_word(content, "var");
    let let_count = count_lines_starting_with_word(content, "let");
    let const_count = count_lines_starting_with_word(content, "const");
    if var_count + let_count + const_count > 0 {
        let suffix = if var_count > (let_count + const_count) && var_count >= 3 {
            " — heavy `var` usage is a legacy signal; prefer `const` for bindings that \
             don't reassign, `let` only when mutation is required. `var` hoists and \
             should be avoided in new code."
        } else if var_count >= 2 {
            " — mixed `var`/`let`/`const`: do not introduce new `var` bindings."
        } else {
            " — prefer `const` by default, `let` only when mutated."
        };
        if let Some(b) = format_popularity_bullet(
            "Variable declarations",
            &[
                ("`const`", const_count),
                ("`let`", let_count),
                ("`var` (legacy)", var_count),
            ],
            suffix,
        ) {
            bullets.push(b);
        }
    }

    // ── 5. Function-expression style ─────────────────────────────────────
    let fn_decl = FUNC_DECL_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    let arrow = ARROW_CONST_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    let fn_expr = content.matches("= function(").count() + content.matches("= function (").count();
    if let Some(b) = format_popularity_bullet(
        "Function style",
        &[
            ("declarations (`function X()`)", fn_decl),
            ("arrow (`const X = () => …`)", arrow),
            ("expressions (`const X = function()`)", fn_expr),
        ],
        "",
    ) {
        bullets.push(b);
    }

    // ── 6. String quote style ────────────────────────────────────────────
    // A mixed (464 total) bullet isn't actionable; always show the actual
    // distribution so the reader can judge what "match the existing style" means.
    let dbl = content.matches('"').count() / 2;
    let sng = content.matches('\'').count() / 2;
    let tick = content.matches('`').count() / 2;
    let total_q = dbl + sng + tick;
    if total_q >= 4 {
        let (dom_label, dom_count) = {
            let mut v = [
                ("double `\"…\"`", dbl),
                ("single `'…'`", sng),
                ("template `` `…` ``", tick),
            ];
            v.sort_by(|a, b| b.1.cmp(&a.1));
            v[0]
        };
        let ratio = dom_count as f32 / total_q as f32;
        if ratio >= 0.55 {
            bullets.push(format!(
                "String quotes: **{dom_label}** ({dom_count}/{total_q}) — match the dominant style."
            ));
        } else {
            // Mixed — show the split so the bullet is actionable.
            bullets.push(format!(
                "String quotes: mixed — {dbl} double / {sng} single / {tick} template. Pick one \
                 and stay consistent; do NOT introduce new quote styles."
            ));
        }
    }

    // ── 7. Semicolon discipline ──────────────────────────────────────────
    let code_lines = content
        .lines()
        .filter(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with("//") && !t.starts_with("*") && !t.starts_with("/*")
        })
        .count();
    let semi_lines = content
        .lines()
        .filter(|l| l.trim_end().ends_with(';'))
        .count();
    if code_lines > 10 {
        let ratio = semi_lines as f32 / code_lines as f32;
        if ratio > 0.6 {
            bullets.push(format!(
                "Semicolons: used ({semi_lines} of {code_lines} code lines terminate with `;`) — \
                 match the convention."
            ));
        } else if ratio < 0.1 {
            bullets.push(
                "Semicolons: omitted (ASI-style) — do not introduce trailing `;` in new code."
                    .into(),
            );
        }
    }

    // ── 8. DOM-access pattern + jQuery migration advice ──────────────────
    let jquery = count_any(
        content,
        &["$(\"", "$('", "jQuery(", "$.ajax", "$.get(", "$.post("],
    );
    let dom_api = count_any(
        content,
        &[
            "document.getElementById",
            "document.querySelector",
            "document.querySelectorAll",
        ],
    );
    if jquery > 0 || dom_api > 0 {
        let advice = if jquery > dom_api + 2 {
            " — **jQuery-heavy**. New features should still use jQuery here for consistency, \
             but treat every `$.ajax` / `$(…)` site as a migration candidate to \
             `fetch`/native DOM when that work is scheduled."
        } else if dom_api > 0 && jquery == 0 {
            " — native-DOM only; do NOT introduce jQuery."
        } else {
            ""
        };
        if let Some(b) = format_popularity_bullet(
            "DOM access",
            &[
                ("jQuery (`$(…)` / `$.ajax`)", jquery),
                ("native DOM (`document.querySelector`)", dom_api),
            ],
            advice,
        ) {
            bullets.push(b);
        }
    }

    // ── 9. Async / error-handling shape (with missing-await risk) ────────
    let try_blocks = content.matches("try {").count() + content.matches("try{").count();
    let catch_chains = content.matches(".catch(").count();
    let then_chains = content.matches(".then(").count();
    let async_await = content.matches("await ").count();
    let async_fn_count = ASYNC_FN_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    if try_blocks + catch_chains + async_await > 0 {
        let mut note = format!(
            "Async / error handling: {try_blocks} try-blocks, {catch_chains} `.catch(`, \
             {then_chains} `.then(`, {async_await} `await` sites"
        );
        if async_fn_count > 0 && async_await == 0 {
            note.push_str(
                " — RISK: async functions declared but no `await` detected; either await \
                 the inner calls or drop the `async` modifier",
            );
        } else if then_chains > async_await && async_await >= 1 {
            note.push_str(
                " — mixed Promise chains and async/await; prefer `await` inside `try`/`catch` \
                 for new code",
            );
        }
        bullets.push(note);
    } else if async_fn_count > 0 {
        bullets.push(format!(
            "Async: {async_fn_count} async function(s) declared — RISK: no `await` sites \
             found; drop `async` or add the `await`."
        ));
    }

    // ── 10. Guard / early-return idiom (VB `Is Nothing` analogue) ────────
    let guards = EARLY_GUARD_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    if guards >= 3 {
        bullets.push(format!(
            "Guard pattern: `if (!x) return …;` (seen {guards} times) — new methods should \
             validate nullable/optional arguments up front before touching them."
        ));
    }

    // ── 11. Error-swallowing risk (empty catch blocks) ───────────────────
    let empty_catches = EMPTY_CATCH_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    let empty_catch_cb = EMPTY_CATCH_CB_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    if empty_catches + empty_catch_cb > 0 {
        bullets.push(format!(
            "RISK: silently-swallowed errors — {empty_catches} empty `catch {{}}` block(s) + \
             {empty_catch_cb} no-op `.catch(() => {{}})` callback(s). Never introduce new \
             empty handlers; at minimum log + rethrow."
        ));
    }

    // ── 12. Security risks (eval / innerHTML / document.write) ───────────
    let eval_hits = EVAL_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    let new_fn_hits = NEW_FUNCTION_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    let inner_html_hits = INNER_HTML_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    let doc_write_hits = DOC_WRITE_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    if eval_hits + new_fn_hits + inner_html_hits + doc_write_hits > 0 {
        let mut parts: Vec<String> = Vec::new();
        if eval_hits > 0 {
            parts.push(format!("{eval_hits} `eval(`"));
        }
        if new_fn_hits > 0 {
            parts.push(format!("{new_fn_hits} `new Function(`"));
        }
        if inner_html_hits > 0 {
            parts.push(format!("{inner_html_hits} `.innerHTML =`"));
        }
        if doc_write_hits > 0 {
            parts.push(format!("{doc_write_hits} `document.write(`"));
        }
        bullets.push(format!(
            "SECURITY RISK: dynamic-code / XSS vectors — {}. Do NOT introduce new call sites; \
             replace existing ones with `textContent`, `createElement`, or a parsed JSON path.",
            parts.join(", ")
        ));
    }

    // ── 13. `"use strict"` directive (JS only) ───────────────────────────
    if !typescript {
        let has_strict = STRICT_RE
            .as_ref()
            .map(|re| re.is_match(content))
            .unwrap_or(false);
        if has_strict {
            bullets.push(
                "`\"use strict\"` declared at top of file — preserve the directive when adding \
                 new top-level code."
                    .into(),
            );
        }
    }

    // ── 14. Exports pattern ──────────────────────────────────────────────
    let export_default = content.matches("export default ").count();
    let export_named = content.matches("export function ").count()
        + content.matches("export const ").count()
        + content.matches("export class ").count();
    let module_exports = count_any(content, &["module.exports", "exports."]);
    if export_default + export_named + module_exports > 0 {
        let advice = if module_exports >= 2 && export_default + export_named == 0 {
            " — CommonJS file; new code should use the same `module.exports` shape, not ES6 \
             `export`."
        } else if export_default + export_named >= 2 && module_exports == 0 {
            " — ES6 module; do not mix in `module.exports`."
        } else if module_exports > 0 && export_default + export_named > 0 {
            " — MIXED export shapes; match the dominant one when adding code."
        } else {
            ""
        };
        if let Some(b) = format_popularity_bullet(
            "Exports",
            &[
                ("ES6 default (`export default`)", export_default),
                ("ES6 named (`export function/const/class`)", export_named),
                ("CommonJS (`module.exports`)", module_exports),
            ],
            advice,
        ) {
            bullets.push(b);
        }
    }

    // ── 15. Paradigm: prototype vs class ─────────────────────────────────
    let prototype = content.matches(".prototype.").count();
    let class_decl = CLASS_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    if prototype + class_decl > 0 {
        let advice = if prototype >= 2 && class_decl == 0 {
            " — legacy prototype-based OO; match the shape when extending this file, but \
             prefer `class` in brand-new files."
        } else {
            ""
        };
        if let Some(b) = format_popularity_bullet(
            "Paradigm",
            &[
                ("`class` declarations", class_decl),
                ("`X.prototype.Y = …` (legacy)", prototype),
            ],
            advice,
        ) {
            bullets.push(b);
        }
    }

    // ── 16. Imports / requires — top-N cited by name (parity with VB `Imports`) ─
    let mut import_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for re in [ES6_IMPORT_FROM_RE.as_ref(), REQUIRE_RE.as_ref()]
        .iter()
        .flatten()
    {
        for cap in re.captures_iter(content).take(SCAN_LIMIT) {
            if let Some(m) = cap.get(1) {
                *import_counts.entry(m.as_str().to_string()).or_insert(0) += 1;
            }
        }
    }
    if import_counts.len() >= 3 {
        let mut pairs: Vec<(String, usize)> = import_counts.into_iter().collect();
        pairs.sort_by(|a, b| b.1.cmp(&a.1));
        let top: Vec<String> = pairs
            .iter()
            .take(4)
            .map(|(n, _)| format!("`{n}`"))
            .collect();
        bullets.push(format!(
            "Imports: top modules — {} — match the same dependency stack when extending this file.",
            top.join(", ")
        ));
    }

    // ── 17. JSDoc convention (VB XML-doc analogue) ───────────────────────
    let jsdoc_lines = JSDOC_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    if jsdoc_lines >= 3 {
        bullets.push(format!(
            "Documentation: JSDoc (`/** … */`) blocks on public API — {jsdoc_lines} site(s). \
             New public functions should carry a JSDoc header of the same shape."
        ));
    }

    // ── 18. Event binding style (.on / addEventListener / inline) ────────
    let on_click = content.matches(".on(\"click").count() + content.matches(".on('click").count();
    let add_listener = content.matches(".addEventListener(").count();
    let inline_onclick = count_any(content, &["onclick=", "onchange=", "onsubmit="]);
    if on_click + add_listener + inline_onclick > 0 {
        if let Some(b) = format_popularity_bullet(
            "Event binding",
            &[
                ("jQuery `.on(\"click\", …)`", on_click),
                ("native `.addEventListener(…)`", add_listener),
                ("inline `onclick=\"…\"` (legacy)", inline_onclick),
            ],
            if inline_onclick >= 3 {
                " — do NOT introduce new inline handlers; bind via `.on` / `.addEventListener`."
            } else {
                ""
            },
        ) {
            bullets.push(b);
        }
    }

    // ── 19. Console logging discipline ───────────────────────────────────
    let console_uses = CONSOLE_LOG_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    if console_uses >= 5 {
        bullets.push(format!(
            "Logging: {console_uses} `console.*` sites — route new logs through the same calls \
             (no `alert(`); strip `console.log` before commit unless the file relies on it."
        ));
    }

    // ── 20. ASP.NET WebForms bridge: __doPostBack ────────────────────────
    let dopostback = DOPOSTBACK_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    if dopostback > 0 {
        bullets.push(format!(
            "WebForms bridge: `__doPostBack(…)` used {dopostback} time(s) — preserves the \
             server-side postback flow. Treat as a migration candidate; do not copy the \
             pattern into new modules."
        ));
    }

    // ── 21. Private-underscore field convention (OO JS/TS style) ─────────
    let underscore_fields = UNDERSCORE_FIELD_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    let underscore_this = UNDERSCORE_THIS_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    if underscore_fields >= 3 || underscore_this >= 10 {
        bullets.push(format!(
            "Private fields: **`_underscorePrefix`** convention ({underscore_fields} declared, \
             {underscore_this} `this._…` access sites) — add new private members as `_name`, \
             do NOT mix in bare-name private fields."
        ));
    }

    // ── 22. Section-header comment decoration ────────────────────────────
    let section_headers = SECTION_HEADER_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    if section_headers >= 4 {
        bullets.push(format!(
            "Section headers: {section_headers} decorative `// ── …` dividers — file is \
             organised into labelled regions; preserve the layout when adding new members."
        ));
    }

    // ── 23. Framework signals (universal) ────────────────────────────────
    // Detect the dominant framework(s) in use so downstream advice can be specific.
    let react_hits = count_any(
        content,
        &[
            "useState(",
            "useEffect(",
            "useMemo(",
            "useCallback(",
            "React.Component",
            "from \"react\"",
            "from 'react'",
        ],
    );
    let jsx_hits = JSX_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    let vue_hits = count_any(
        content,
        &[
            "defineComponent(",
            "createApp(",
            "from \"vue\"",
            "from 'vue'",
            "Vue.extend(",
        ],
    );
    let angular_hits = count_any(
        content,
        &[
            "@Component(",
            "@Injectable(",
            "@NgModule(",
            "from \"@angular",
            "from '@angular",
        ],
    );
    let rxjs_hits = count_any(
        content,
        &[
            ".pipe(",
            ".subscribe(",
            "new Observable(",
            "new Subject(",
            "from \"rxjs\"",
            "from 'rxjs'",
        ],
    );
    let node_hits = count_any(
        content,
        &[
            "process.env.",
            "require(\"fs\")",
            "require('fs')",
            "require(\"path\")",
            "require('path')",
            "__dirname",
            "__filename",
        ],
    );
    let express_hits = count_any(
        content,
        &[
            "express()",
            "app.use(",
            "app.get(",
            "app.post(",
            "app.listen(",
            "req.body",
            "res.send(",
            "res.json(",
        ],
    );
    // Test-framework patterns: bare `it(` / `test(` are too common (matches
    // `.split(`, `.init(`, etc). Require a leading whitespace + opening quote
    // so we only hit `it("…")` / `test('…')` call forms.
    let test_hits = count_any(
        content,
        &[
            "describe(\"",
            "describe('",
            " it(\"",
            " it('",
            "\tit(\"",
            "\tit('",
            " test(\"",
            " test('",
            "expect(",
            "jest.fn(",
            "jest.mock(",
            "vi.mock(",
            "vi.fn(",
        ],
    );
    let mut frameworks: Vec<(&str, usize)> = vec![
        ("React", react_hits + (jsx_hits.min(20))),
        ("Vue", vue_hits),
        ("Angular", angular_hits),
        ("RxJS", rxjs_hits),
        ("Node/fs/path", node_hits),
        ("Express", express_hits),
        ("test runner (Jest/Vitest/Mocha)", test_hits),
    ];
    frameworks.retain(|(_, c)| *c >= 3);
    if !frameworks.is_empty() {
        frameworks.sort_by(|a, b| b.1.cmp(&a.1));
        let top: Vec<String> = frameworks
            .iter()
            .take(3)
            .map(|(n, c)| format!("**{n}** ({c} signals)"))
            .collect();
        bullets.push(format!(
            "Framework signal: {} — follow the same stack when extending this file; do not \
             mix in a second framework.",
            top.join(", ")
        ));
    }

    // ── 24. Transpiled-TS fingerprint (only meaningful on .js) ───────────
    if !typescript {
        let transpiled = TRANSPILED_TS_RE
            .as_ref()
            .map(|re| re.is_match(content))
            .unwrap_or(false);
        if transpiled {
            bullets.push(
                "Transpiled output: file contains `__extends` / `__assign` / `__awaiter` helpers \
                 — this is **generated JS** (likely from `tsc`). Do NOT hand-edit; change the \
                 corresponding `.ts` source and rebuild."
                    .into(),
            );
        }
    }

    bullets
}

fn count_lines_starting_with_word(content: &str, word: &str) -> usize {
    let pat = format!("{word} ");
    content
        .lines()
        .filter(|l| l.trim_start().starts_with(&pat))
        .count()
}

// ── ASPX / ASCX / Master ─────────────────────────────────────────────────
fn static_analyze_aspx(content: &str) -> Vec<String> {
    let mut bullets = Vec::new();

    // Directive type.
    let directive = if content.contains("<%@ Master") {
        Some("Master page (`<%@ Master %>`)")
    } else if content.contains("<%@ Control") {
        Some("User control (`<%@ Control %>`)")
    } else if content.contains("<%@ Page") {
        Some("Page (`<%@ Page %>`)")
    } else {
        None
    };
    if let Some(d) = directive {
        bullets.push(format!("Directive: {d}"));
    }

    // Codebehind vs inline.
    let codebehind = content.contains("CodeBehind=") || content.contains("CodeFile=");
    let inline = content.contains("<script runat=\"server\">")
        || content.contains("<script runat='server'>");
    if codebehind && !inline {
        bullets.push("Code layout: separate codebehind (`CodeBehind=` / `CodeFile=`)".into());
    } else if inline && !codebehind {
        bullets.push("Code layout: inline `<script runat=\"server\">` (no codebehind file)".into());
    } else if codebehind && inline {
        bullets.push("Code layout: mixed — codebehind AND inline server script".into());
    }

    // Master page binding.
    if let Some(start) = content.find("MasterPageFile=") {
        let rest = &content[start + "MasterPageFile=".len()..];
        let q = rest.chars().next().unwrap_or('"');
        if let Some(end) = rest[1..].find(q) {
            let path = &rest[1..1 + end];
            bullets.push(format!("Master page binding: `{path}`"));
        }
    }

    // Control library — counts of common data-display controls.
    let gridview = content.matches("<asp:GridView").count();
    let repeater = content.matches("<asp:Repeater").count();
    let listview = content.matches("<asp:ListView").count();
    let formview = content.matches("<asp:FormView").count();
    let details = content.matches("<asp:DetailsView").count();
    let datalist = content.matches("<asp:DataList").count();
    let total_data = gridview + repeater + listview + formview + details + datalist;
    if total_data > 0 {
        if let Some(b) = format_popularity_bullet(
            "Data-display control",
            &[
                ("`<asp:GridView>`", gridview),
                ("`<asp:Repeater>`", repeater),
                ("`<asp:ListView>`", listview),
                ("`<asp:FormView>`", formview),
                ("`<asp:DetailsView>`", details),
                ("`<asp:DataList>`", datalist),
            ],
            "",
        ) {
            bullets.push(b);
        }
    }

    // Data binding style — declarative vs programmatic.
    let declarative = content.matches("DataSourceID=").count();
    if declarative > 0 {
        bullets.push(format!(
            "Data binding: declarative — {declarative} controls use `DataSourceID=`"
        ));
    }

    // AJAX — UpdatePanel + ScriptManager.
    let update_panels = content.matches("<asp:UpdatePanel").count();
    let timers = content.matches("<asp:Timer").count();
    if update_panels > 0 || timers > 0 {
        bullets.push(format!(
            "AJAX: {update_panels} `<asp:UpdatePanel>`, {timers} `<asp:Timer>` — partial-render regions"
        ));
    }
    let has_sm =
        content.contains("<asp:ScriptManager") || content.contains("<asp:ToolkitScriptManager");
    if has_sm {
        bullets.push("`<asp:ScriptManager>` present".into());
    }

    // Binding expression style.
    let eval_expr = content.matches("<%# Eval(").count();
    let bind_expr = content.matches("<%# Bind(").count();
    let eval_eq = content.matches("<%=").count();
    let eval_enc = content.matches("<%:").count();
    if eval_expr + bind_expr + eval_eq + eval_enc > 0 {
        if let Some(b) = format_popularity_bullet(
            "Binding expressions",
            &[
                ("`<%# Eval(…) %>`", eval_expr),
                ("`<%# Bind(…) %>`", bind_expr),
                ("`<%= … %>` (raw)", eval_eq),
                ("`<%: … %>` (HTML-encoded)", eval_enc),
            ],
            "",
        ) {
            bullets.push(b);
        }
    }

    // Client-side script references.
    let script_refs = content.matches("<script src=").count();
    if script_refs > 0 {
        bullets.push(format!(
            "Client scripts: {script_refs} `<script src=…>` references"
        ));
    }

    // Validation controls.
    let req_val = content.matches("<asp:RequiredFieldValidator").count();
    let rx_val = content.matches("<asp:RegularExpressionValidator").count();
    let cust_val = content.matches("<asp:CustomValidator").count();
    let range_val = content.matches("<asp:RangeValidator").count();
    let compare_val = content.matches("<asp:CompareValidator").count();
    let total_val = req_val + rx_val + cust_val + range_val + compare_val;
    if total_val > 0 {
        bullets.push(format!(
            "Validation: {total_val} validators ({req_val} required, {rx_val} regex, {cust_val} custom, {range_val} range, {compare_val} compare)"
        ));
    }

    // Register directives for user-control libraries.
    let reg_dirs = content.matches("<%@ Register").count();
    if reg_dirs > 0 {
        bullets.push(format!(
            "`<%@ Register %>` directives: {reg_dirs} — page pulls in user-control libraries"
        ));
    }

    bullets
}

// ── SQL (T-SQL / PG / ANSI, with a T-SQL bias) ──────────────────────────
fn static_analyze_sql(content: &str) -> Vec<String> {
    use regex::Regex;
    use std::sync::LazyLock;

    static CREATE_TABLE_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?im)\bCREATE\s+TABLE\s+(?:\[?\w+\]?\.)?\[?(\w+)\]?").ok());
    static CREATE_PROC_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(
            r"(?im)\bCREATE\s+(?:OR\s+ALTER\s+)?PROC(?:EDURE)?\s+(?:\[?\w+\]?\.)?\[?(\w+)\]?",
        )
        .ok()
    });
    static CREATE_VIEW_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?im)\bCREATE\s+VIEW\s+(?:\[?\w+\]?\.)?\[?(\w+)\]?").ok());
    static PARAM_RE: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"@(\w+)").ok());

    let mut bullets = Vec::new();

    // Collect table / proc / view names for casing + prefix detection.
    let mut names: Vec<String> = Vec::new();
    for re in [
        CREATE_TABLE_RE.as_ref(),
        CREATE_PROC_RE.as_ref(),
        CREATE_VIEW_RE.as_ref(),
    ]
    .iter()
    .flatten()
    {
        for cap in re.captures_iter(content).take(SCAN_LIMIT) {
            if let Some(m) = cap.get(1) {
                names.push(m.as_str().to_string());
            }
        }
    }
    if !names.is_empty() {
        let mut counts = CasingCounts::default();
        let mut samples: Vec<String> = Vec::new();
        for n in &names {
            counts.observe(n);
            if samples.len() < 3 {
                samples.push(n.clone());
            }
        }
        if let Some(b) = format_casing_bullet("Object naming", &counts, &samples) {
            bullets.push(b);
        }
    }

    // Prefix frequency (fj_, pr_, aspnet_, …).
    if !names.is_empty() {
        let mut prefix_counts: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for n in &names {
            if let Some(idx) = n.find('_') {
                if idx > 0 && idx <= 6 {
                    let prefix = &n[..idx];
                    *prefix_counts
                        .entry(prefix.to_ascii_lowercase())
                        .or_insert(0) += 1;
                }
            }
        }
        let mut pairs: Vec<(String, usize)> = prefix_counts.into_iter().collect();
        pairs.sort_by(|a, b| b.1.cmp(&a.1));
        let top: Vec<String> = pairs
            .into_iter()
            .filter(|(_, n)| *n >= 2)
            .take(5)
            .map(|(p, n)| format!("`{p}_*` ({n})"))
            .collect();
        if !top.is_empty() {
            bullets.push(format!(
                "Prefix namespaces: {} — consistent domain prefixes in object names",
                top.join(", ")
            ));
        }
    }

    // Keyword casing — sample the first 200 occurrences of each.
    let upper_kw = count_any(
        content,
        &[
            "SELECT ", "INSERT ", "UPDATE ", "DELETE ", "FROM ", "WHERE ", "JOIN ",
        ],
    );
    let lower_kw = count_any(
        content,
        &[
            "select ", "insert ", "update ", "delete ", "from ", "where ", "join ",
        ],
    );
    if let Some(b) = format_popularity_bullet(
        "Keyword casing",
        &[("UPPERCASE", upper_kw), ("lowercase", lower_kw)],
        "",
    ) {
        bullets.push(b);
    }

    // Schema qualification.
    let dbo_refs = content.matches("[dbo].").count() + content.matches("dbo.").count();
    if dbo_refs > 5 {
        bullets.push(format!(
            "Schema qualification: `dbo.` used {dbo_refs} times"
        ));
    }

    // Transaction usage.
    let begin_tx =
        content.matches("BEGIN TRANSACTION").count() + content.matches("BEGIN TRAN").count();
    let commit = content.matches("COMMIT").count();
    let rollback = content.matches("ROLLBACK").count();
    if begin_tx + commit + rollback > 0 {
        bullets.push(format!(
            "Transactions: {begin_tx} BEGIN TRAN, {commit} COMMIT, {rollback} ROLLBACK"
        ));
    }

    // Error handling.
    let try_blocks = content.matches("BEGIN TRY").count();
    let raiserror = content.matches("RAISERROR").count();
    let throw_stmt = content.matches("THROW").count();
    if try_blocks + raiserror + throw_stmt > 0 {
        bullets.push(format!(
            "Error handling: {try_blocks} TRY blocks, {raiserror} RAISERROR, {throw_stmt} THROW"
        ));
    }

    // Proc definition style.
    let create_or_alter = content.matches("CREATE OR ALTER").count();
    let plain_create = CREATE_PROC_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    if plain_create > 0 {
        let idempotent = create_or_alter.min(plain_create);
        bullets.push(format!(
            "Proc style: {plain_create} procs ({idempotent} use `CREATE OR ALTER` — idempotent deploys)"
        ));
    }

    // Parameter naming.
    if let Some(re) = PARAM_RE.as_ref() {
        let mut counts = CasingCounts::default();
        let mut samples: Vec<String> = Vec::new();
        for cap in re.captures_iter(content).take(SCAN_LIMIT) {
            if let Some(m) = cap.get(1) {
                let name = m.as_str();
                counts.observe(name);
                if samples.len() < 3 {
                    samples.push(format!("@{name}"));
                }
            }
        }
        if let Some(b) = format_casing_bullet("Parameter naming", &counts, &samples) {
            bullets.push(b);
        }
    }

    bullets
}

// ── Python ───────────────────────────────────────────────────────────────
fn static_analyze_python(content: &str) -> Vec<String> {
    use regex::Regex;
    use std::sync::LazyLock;

    static DEF_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?m)^\s*(?:async\s+)?def\s+(\w+)\s*\(").ok());
    static CLASS_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?m)^\s*class\s+(\w+)").ok());
    static TYPED_PARAM_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(r":\s*(?:str|int|float|bool|list|dict|tuple|Optional|[A-Z]\w+)\b").ok()
    });
    static RET_ANNOT_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"->\s*[A-Za-z_][\w\[\]\s,]*:").ok());

    let mut bullets = Vec::new();

    if let Some(re) = DEF_RE.as_ref() {
        let (counts, samples) = count_casing(content, re, SCAN_LIMIT);
        if let Some(b) = format_casing_bullet("Function naming", &counts, &samples) {
            bullets.push(b);
        }
    }
    if let Some(re) = CLASS_RE.as_ref() {
        let (counts, samples) = count_casing(content, re, SCAN_LIMIT);
        if let Some(b) = format_casing_bullet("Class naming", &counts, &samples) {
            bullets.push(b);
        }
    }

    // Type hints.
    let typed_params = TYPED_PARAM_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    let ret_annot = RET_ANNOT_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    if typed_params + ret_annot > 0 {
        bullets.push(format!(
            "Type hints: {typed_params} typed params, {ret_annot} return annotations"
        ));
    }

    // Import style.
    let import_x = content.matches("\nimport ").count();
    let from_import = content.matches("\nfrom ").count();
    if import_x + from_import > 0 {
        if let Some(b) = format_popularity_bullet(
            "Imports",
            &[("`import X`", import_x), ("`from X import Y`", from_import)],
            "",
        ) {
            bullets.push(b);
        }
    }

    // String style.
    let fstr = content.matches(r#"f""#).count() + content.matches("f'").count();
    let format_call = content.matches(".format(").count();
    let percent_fmt = content.matches("\" %").count();
    if fstr + format_call + percent_fmt > 0 {
        if let Some(b) = format_popularity_bullet(
            "String formatting",
            &[
                ("f-strings", fstr),
                ("`.format()`", format_call),
                ("`%`-formatting (legacy)", percent_fmt),
            ],
            "",
        ) {
            bullets.push(b);
        }
    }

    // Error handling — bare `except:` is a smell.
    let try_blocks = content.matches("\ntry:").count();
    let bare_except = content.matches("\nexcept:").count();
    let typed_except = content.matches("\nexcept ").count();
    if try_blocks + bare_except + typed_except > 0 {
        let mut note = format!(
            "Error handling: {try_blocks} try, {typed_except} typed `except X`, {bare_except} bare `except:`"
        );
        if bare_except > 0 {
            note.push_str(" — bare except is discouraged");
        }
        bullets.push(note);
    }

    // Docstrings.
    let tdqs = content.matches(r#"""""#).count() / 2;
    if tdqs >= 3 {
        bullets.push(format!("Docstrings: {tdqs} `\"\"\"…\"\"\"` blocks"));
    }

    // Async.
    let async_def = content.matches("async def ").count();
    let await_ct = content.matches("await ").count();
    if async_def + await_ct > 0 {
        bullets.push(format!(
            "Async: {async_def} `async def`, {await_ct} `await` sites"
        ));
    }

    // Comprehension usage.
    let list_c = content.matches("[").filter(|_| true).count(); // rough
    let _ = list_c;
    let comp_markers = content
        .matches(" for ")
        .count()
        .saturating_sub(content.matches("\nfor ").count() + content.matches("\n    for ").count());
    if comp_markers > 5 {
        bullets.push(format!(
            "Comprehensions: ~{comp_markers} list/dict/set comprehensions — Pythonic style"
        ));
    }

    bullets
}

// ── Rust ────────────────────────────────────────────────────────────────
fn static_analyze_rust(content: &str) -> Vec<String> {
    use regex::Regex;
    use std::sync::LazyLock;

    static FN_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(r"(?m)^\s*(?:pub(?:\(crate\))?\s+)?(?:async\s+)?fn\s+(\w+)").ok()
    });
    static TYPE_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(r"(?m)^\s*(?:pub(?:\(crate\))?\s+)?(?:struct|enum|trait)\s+(\w+)").ok()
    });
    static UNSAFE_RE: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"\bunsafe\s*\{").ok());
    static LIFETIME_RE: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"<'\w+").ok());
    static TEST_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"#\[(?:test|tokio::test)\]").ok());

    let mut bullets = Vec::new();

    if let Some(re) = FN_RE.as_ref() {
        let (counts, samples) = count_casing(content, re, SCAN_LIMIT);
        if let Some(b) = format_casing_bullet("Function naming", &counts, &samples) {
            bullets.push(b);
        }
    }
    if let Some(re) = TYPE_RE.as_ref() {
        let (counts, samples) = count_casing(content, re, SCAN_LIMIT);
        if let Some(b) = format_casing_bullet("Type naming", &counts, &samples) {
            bullets.push(b);
        }
    }

    // Error handling — `?` vs `.unwrap()` vs `.expect("…")`.
    let q_ops = content.matches("?\n").count() + content.matches("?;").count();
    let unwraps = content.matches(".unwrap()").count();
    let expects = content.matches(".expect(\"").count();
    if q_ops + unwraps + expects > 0 {
        let mut note = format!(
            "Error handling: {q_ops} `?` operators, {unwraps} `.unwrap()`, {expects} `.expect(\"…\")`"
        );
        if unwraps > expects + q_ops / 4 {
            note.push_str(" — heavy `.unwrap()` usage is a red flag in library code");
        }
        bullets.push(note);
    }

    // Unsafe.
    let unsafe_count = UNSAFE_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    if unsafe_count > 0 {
        bullets.push(format!(
            "`unsafe` blocks: {unsafe_count} — requires invariant docs"
        ));
    }

    // Lifetimes.
    let lifetimes = LIFETIME_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    if lifetimes >= 3 {
        bullets.push(format!("Explicit lifetimes: {lifetimes} annotations"));
    }

    // Tests.
    let tests = TEST_RE
        .as_ref()
        .map(|re| re.find_iter(content).count())
        .unwrap_or(0);
    if tests > 0 {
        bullets.push(format!(
            "Tests: {tests} `#[test]` / `#[tokio::test]` functions"
        ));
    }

    // Async runtime.
    let tokio = count_any(content, &["tokio::", "use tokio"]);
    let async_std = count_any(content, &["async_std::", "use async_std"]);
    if tokio + async_std > 0 {
        if let Some(b) = format_popularity_bullet(
            "Async runtime",
            &[("`tokio`", tokio), ("`async-std`", async_std)],
            "",
        ) {
            bullets.push(b);
        }
    }

    // Doc-comment density.
    let doc_lines = content
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            t.starts_with("///") || t.starts_with("//!")
        })
        .count();
    if doc_lines >= 10 {
        bullets.push(format!("Doc comments: {doc_lines} `///` / `//!` lines"));
    }

    // Macro usage.
    let derive_macros = content.matches("#[derive(").count();
    if derive_macros >= 3 {
        bullets.push(format!("`#[derive(…)]` macros: {derive_macros} uses"));
    }

    // `use` grouping heuristic — just report top-level count.
    let use_lines = content
        .lines()
        .filter(|l| l.trim_start().starts_with("use "))
        .count();
    if use_lines >= 5 {
        bullets.push(format!(
            "`use` statements: {use_lines} — group `std`, external, crate"
        ));
    }

    bullets
}

// ── Go ──────────────────────────────────────────────────────────────────
fn static_analyze_go(content: &str) -> Vec<String> {
    use regex::Regex;
    use std::sync::LazyLock;

    static FUNC_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?m)^func\s+(?:\([^)]*\)\s+)?(\w+)\s*\(").ok());
    static TYPE_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?m)^type\s+(\w+)\s+(?:struct|interface|func|\w)").ok());

    let mut bullets = Vec::new();

    if let Some(re) = FUNC_RE.as_ref() {
        let (counts, samples) = count_casing(content, re, SCAN_LIMIT);
        if let Some(b) = format_casing_bullet("Function naming", &counts, &samples) {
            bullets.push(b);
        }
    }
    if let Some(re) = TYPE_RE.as_ref() {
        let (counts, samples) = count_casing(content, re, SCAN_LIMIT);
        if let Some(b) = format_casing_bullet("Type naming", &counts, &samples) {
            bullets.push(b);
        }
    }

    // Error handling idiom — `if err != nil`.
    let err_check = content.matches("if err != nil").count();
    if err_check > 0 {
        bullets.push(format!(
            "Error checks: {err_check} `if err != nil` patterns"
        ));
    }

    // Interfaces + channels.
    let interfaces = content.matches(" interface {").count();
    let channels = content.matches("chan ").count();
    if interfaces + channels > 0 {
        bullets.push(format!(
            "Interfaces: {interfaces}, channels: {channels} — concurrency idioms"
        ));
    }

    // Goroutines.
    let go_keyword = content.matches("\n\tgo ").count() + content.matches("\n    go ").count();
    if go_keyword > 0 {
        bullets.push(format!("Goroutines: {go_keyword} `go …` launches"));
    }

    // Import grouping — check for multi-line import blocks.
    let import_blocks = content.matches("\nimport (").count();
    if import_blocks > 0 {
        bullets.push(format!(
            "Imports: {import_blocks} grouped `import ( … )` blocks — gofmt style"
        ));
    }

    bullets
}

// ── Java ────────────────────────────────────────────────────────────────
fn static_analyze_java(content: &str) -> Vec<String> {
    use regex::Regex;
    use std::sync::LazyLock;

    static METHOD_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(
            r"(?m)^\s*(?:public|private|protected|static|final|abstract|synchronized|native)\s+(?:[\w<>\[\],\?\s]+?\s+)?(\w+)\s*\(",
        )
        .ok()
    });
    static CLASS_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(r"(?m)^\s*(?:public\s+)?(?:final\s+)?(?:abstract\s+)?(?:class|interface|record|enum)\s+(\w+)")
            .ok()
    });

    let mut bullets = Vec::new();

    if let Some(re) = METHOD_RE.as_ref() {
        let (counts, samples) = count_casing(content, re, SCAN_LIMIT);
        if let Some(b) = format_casing_bullet("Method naming", &counts, &samples) {
            bullets.push(b);
        }
    }
    if let Some(re) = CLASS_RE.as_ref() {
        let (counts, samples) = count_casing(content, re, SCAN_LIMIT);
        if let Some(b) = format_casing_bullet("Type naming", &counts, &samples) {
            bullets.push(b);
        }
    }

    // Checked-exception discipline.
    let throws = content.matches(" throws ").count();
    let try_catch = content.matches("try {").count();
    if throws + try_catch > 0 {
        bullets.push(format!(
            "Error handling: {throws} `throws`, {try_catch} `try/catch` blocks"
        ));
    }

    // Modern Java features.
    let stream_api = content.matches(".stream()").count();
    let optional = content.matches("Optional.").count() + content.matches("Optional<").count();
    let lambda = content.matches(" -> ").count();
    if stream_api + optional + lambda > 0 {
        bullets.push(format!(
            "Modern Java: {stream_api} `.stream()` chains, {optional} `Optional` refs, {lambda} `->` lambdas"
        ));
    }

    // Dependency injection — Spring / CDI annotations.
    let spring_autowired = content.matches("@Autowired").count();
    let inject = content.matches("@Inject").count();
    let bean = content.matches("@Bean").count() + content.matches("@Component").count();
    if spring_autowired + inject + bean > 0 {
        bullets.push(format!(
            "DI annotations: {spring_autowired} `@Autowired`, {inject} `@Inject`, {bean} `@Bean`/`@Component`"
        ));
    }

    bullets
}

// ── Generic fallback — improved ─────────────────────────────────────────
fn static_analyze_generic(content: &str) -> Vec<String> {
    use regex::Regex;
    use std::sync::LazyLock;

    static IDENT_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"\b([A-Za-z_][A-Za-z0-9_]{2,})\b").ok());

    let mut bullets = Vec::new();

    // Indentation.
    let tab_lines = content.lines().filter(|l| l.starts_with('\t')).count();
    let space_lines = content
        .lines()
        .filter(|l| l.starts_with("    ") || l.starts_with("  "))
        .count();
    if tab_lines > space_lines * 2 {
        bullets.push("Indentation: tabs".into());
    } else if space_lines > tab_lines * 2 {
        // Try to detect indent width by looking at the most common
        // leading-space count among indented lines.
        let mut two = 0usize;
        let mut four = 0usize;
        for l in content.lines() {
            if l.starts_with("    ") && !l.starts_with("     ") {
                four += 1;
            } else if l.starts_with("  ") && !l.starts_with("   ") {
                two += 1;
            }
        }
        let width = if four >= two { "4-space" } else { "2-space" };
        bullets.push(format!("Indentation: spaces ({width})"));
    }

    // File length.
    let total_lines = content.lines().count();
    if total_lines > 0 {
        if total_lines > 800 {
            bullets.push(format!(
                "File length: {total_lines} lines — large file; consider splitting"
            ));
        } else if total_lines <= 200 {
            bullets.push(format!(
                "File length: {total_lines} lines — small file (style signal: prefer focused files)"
            ));
        }
    }

    // Line length — P90.
    let mut lens: Vec<usize> = content.lines().map(|l| l.chars().count()).collect();
    if !lens.is_empty() {
        lens.sort_unstable();
        let p90 = lens[(lens.len() * 9) / 10];
        if p90 <= 80 {
            bullets.push(format!(
                "Line length: P90 = {p90} chars — tight line budget (≤80)"
            ));
        } else if p90 >= 120 {
            bullets.push(format!(
                "Line length: P90 = {p90} chars — wide style (>120)"
            ));
        }
    }

    // Comment density.
    let comment_lines = content
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            t.starts_with("//")
                || t.starts_with('#')
                || t.starts_with("/*")
                || t.starts_with('*')
                || t.starts_with("--")
                || t.starts_with("'")
        })
        .count();
    if total_lines >= 50 {
        let pct = (comment_lines as f32 / total_lines as f32) * 100.0;
        if pct >= 15.0 {
            bullets.push(format!(
                "Comment density: {pct:.0}% ({comment_lines}/{total_lines}) — heavily commented"
            ));
        } else if pct <= 3.0 {
            bullets.push(format!(
                "Comment density: {pct:.0}% — sparse comments; rely on self-documenting code"
            ));
        }
    }

    // Identifier casing (language-agnostic).
    if let Some(re) = IDENT_RE.as_ref() {
        let mut counts = CasingCounts::default();
        let mut samples: Vec<String> = Vec::new();
        for cap in re.captures_iter(content).take(SCAN_LIMIT) {
            if let Some(m) = cap.get(1) {
                let name = m.as_str();
                // Skip all-lowercase single-word names (likely keywords).
                if name.len() >= 3 && !is_common_keyword(name) {
                    counts.observe(name);
                    if samples.len() < 3 && name.chars().any(|c| c.is_ascii_uppercase() || c == '_')
                    {
                        samples.push(name.to_string());
                    }
                }
            }
        }
        if let Some(b) = format_casing_bullet("Identifier casing", &counts, &samples) {
            bullets.push(b);
        }
    }

    bullets
}

/// Very small set of tokens we want to exclude from the generic identifier
/// casing scan. We don't need a real keyword table — this just knocks out
/// the words that appear a zillion times in source code and would dominate
/// the sample list with nothing useful.
fn is_common_keyword(s: &str) -> bool {
    matches!(
        s,
        "the"
            | "and"
            | "for"
            | "not"
            | "are"
            | "was"
            | "but"
            | "all"
            | "any"
            | "int"
            | "str"
            | "var"
            | "let"
            | "const"
            | "function"
            | "return"
            | "import"
            | "from"
            | "class"
            | "def"
            | "null"
            | "true"
            | "false"
            | "self"
            | "this"
    )
}

/// Prompt template (from v1 dreaming.py STYLE_ANALYSIS_PROMPT), made
/// language-aware. The original was saturated with Python examples (snake_case,
/// try/except, `from X import Y`, docstrings) and never named the file's
/// language, so the LLM parroted Python conventions for VB/C#/TS files. Now the
/// language is stated up front and the prompt demands language-appropriate
/// conventions only.
const STYLE_ANALYSIS_PROMPT: &str = r#"You are a code style analyzer for {language} code. You will be given recent git diffs for a file written in {language}.
Your task is to extract the coding patterns and conventions ACTUALLY used in THIS {language} file. Use ONLY conventions that are valid for {language}; never assume Python or any other language, and never invent conventions the diffs do not show.

Recent changes to {file_path} ({language}):

{diffs}

From the diffs above, extract (express everything in {language} terms):
1. Naming conventions — the actual casing this file uses for types, methods/subs/functions, locals, and constants.
2. Common patterns — validation, data access, and the specific libraries / framework APIs this file uses.
3. Code organization — member ordering, region/section structure, file layout.
4. Error handling — the approach this file/language actually uses.
5. How dependencies are declared and grouped (the {language} mechanism — e.g. Imports/using/import — as the file does it).
6. Documentation / comment style.

Format your response as a concise style guide (3-5 bullet points) to prepend to an AI agent's context.
Focus on actionable, {language}-specific patterns this file actually demonstrates — NOT generic advice and NOT conventions from other languages.

If there are insufficient changes to determine a clear pattern, respond with "INSUFFICIENT_DATA".

Style Guide:
"#;

/// Human-readable language label for a path, for the style-analysis prompt.
fn style_language_label(file_path: &str) -> &'static str {
    let p = file_path.to_ascii_lowercase();
    if p.ends_with(".vb") {
        "VB.NET"
    } else if p.ends_with(".cs") {
        "C#"
    } else if p.ends_with(".ts") || p.ends_with(".tsx") {
        "TypeScript"
    } else if p.ends_with(".js")
        || p.ends_with(".jsx")
        || p.ends_with(".mjs")
        || p.ends_with(".cjs")
    {
        "JavaScript"
    } else if p.ends_with(".aspx") || p.ends_with(".ascx") || p.ends_with(".master") {
        "ASP.NET WebForms markup"
    } else if p.ends_with(".sql") {
        "SQL"
    } else if p.ends_with(".py") {
        "Python"
    } else if p.ends_with(".rs") {
        "Rust"
    } else if p.ends_with(".go") {
        "Go"
    } else if p.ends_with(".java") {
        "Java"
    } else if p.ends_with(".vbhtml") || p.ends_with(".cshtml") {
        "Razor"
    } else if p.ends_with(".html") || p.ends_with(".htm") {
        "HTML"
    } else if p.ends_with(".css") || p.ends_with(".scss") || p.ends_with(".less") {
        "CSS"
    } else {
        "this language"
    }
}

async fn try_llm_style_analysis(
    state: &AppState,
    file_path: &str,
    diffs_text: &str,
) -> Option<String> {
    let prompt = STYLE_ANALYSIS_PROMPT
        .replace("{language}", style_language_label(file_path))
        .replace("{file_path}", file_path)
        .replace("{diffs}", diffs_text);

    let result = state
        .dreaming
        .generate_insight("", &prompt, Duration::from_secs(30))
        .await;

    if result.is_empty() || result.contains("INSUFFICIENT_DATA") {
        return None;
    }

    Some(result.trim().to_string())
}

// ---------------------------------------------------------------------------
// Temporal coupling (v1 dreaming.py::find_temporal_couplings)
// ---------------------------------------------------------------------------

/// A pair of files that frequently change together.
#[derive(Debug, Clone)]
pub struct TemporalCoupling {
    pub file_a: String,
    pub file_b: String,
    /// Number of commits where both files changed.
    pub frequency: usize,
}

/// Find files that frequently change together in git history.
///
/// This implements v1's "Temporal Coupling Detection" — ported from
/// dreaming.py::find_temporal_couplings(). Uses the graph's weighted
/// co-change edges that are updated by the git indexer.
pub async fn find_temporal_couplings(
    state: &AppState,
    project_id: &str,
    min_frequency: u32,
    limit: usize,
) -> anyhow::Result<Vec<TemporalCoupling>> {
    let graph = state.graph.clone();
    let pid = project_id.to_string();

    tokio::task::spawn_blocking(move || {
        find_temporal_couplings_blocking(&graph, &pid, min_frequency, limit)
    })
    .await
    .unwrap_or_else(|e| Err(anyhow::anyhow!("spawn_blocking panicked: {e}")))
}

fn find_temporal_couplings_blocking(
    graph: &engram_graph::GraphStore,
    project_id: &str,
    min_frequency: u32,
    limit: usize,
) -> anyhow::Result<Vec<TemporalCoupling>> {
    // Query temporal-coupling edges from the graph (set during git history indexing).
    let raw = engram_graph::algorithms::coupling::top_project_couplings(graph, project_id, limit)?;

    let couplings = raw
        .into_iter()
        .filter(|c| c.weight >= min_frequency)
        .map(|c| TemporalCoupling {
            file_a: c.file_node_id,
            file_b: c.neighbor_node_id,
            frequency: c.weight as usize,
        })
        .collect();

    Ok(couplings)
}

// ---------------------------------------------------------------------------
// Git diff collection (CPU-bound helper)
// ---------------------------------------------------------------------------

/// Collect the N most recent diffs for a specific file from the git repo.
/// Returns Vec of (commit_hash, commit_message, diff_text).
fn collect_file_diffs(
    directory: &std::path::Path,
    file_path: &str,
    limit: usize,
) -> anyhow::Result<Vec<(String, String, String)>> {
    use engram_git::{GitWalker, history::MergeCommitPolicy};
    use tokio_util::sync::CancellationToken;

    let repo = GitWalker::open_repo(directory)?;
    let cancel = CancellationToken::new();

    // Walk up to limit*4 commits to find `limit` diffs for this file.
    let oids = GitWalker::walk_new_commits(
        &repo,
        None,
        limit * 4,
        MergeCommitPolicy::FirstParentOnly,
        &cancel,
    )?;

    let target = RelPath::new(file_path);
    let is_dir = std::path::Path::new(directory).join(file_path).is_dir();
    let mut out: Vec<(String, String, String)> = Vec::new();

    for oid in oids.iter().rev() {
        if out.len() >= limit {
            break;
        }
        let commit = repo.find_commit(*oid)?;
        let changed = GitWalker::files_changed_in_commit(&repo, *oid)?;

        let touches_target = changed.iter().any(|fc| {
            let p = fc.path().as_str();
            if is_dir {
                p.starts_with(target.as_str())
            } else {
                fc.path() == &target
            }
        });

        if !touches_target {
            continue;
        }

        let per_file = GitWalker::diff_text_for_commit(&repo, *oid, 4096)?;
        for (path, diff_text) in per_file {
            let matched = if is_dir {
                path.as_str().starts_with(target.as_str())
            } else {
                path == target
            };

            if matched && !diff_text.trim().is_empty() {
                let msg = commit.message().unwrap_or("").to_string();
                out.push((oid.to_string(), msg, diff_text));
                // For directories, we might want multiple files from same commit,
                // but let's stick to one diff text block for simplicity like v1.
                break;
            }
        }
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// Architecture Mimicry: migration boundary suggestions
// ---------------------------------------------------------------------------

/// Suggest microservice/bounded-context migration boundaries.
///
/// Steps:
/// 1. Query top temporal coupling edges from the graph.
/// 2. Group coupled files into clusters using iterative union-find with rank heuristic.
/// 3. For each cluster, gather shared state keys and SQL table references.
/// 4. Perform cross-cluster dependency analysis.
/// 5. Format text blocks and pass to LLM via DreamingEngine.
/// 6. Return proposed boundaries with cross-cluster annotations.
pub async fn suggest_migration_boundaries(
    state: &AppState,
    project_id: &str,
    min_frequency: u32,
    max_clusters: usize,
    timeout_secs: u64,
    include_cross_cluster_deps: bool,
) -> anyhow::Result<Vec<engram_ml::MigrationBoundary>> {
    let graph = state.graph.clone();
    let pid = project_id.to_string();
    let min_freq = min_frequency;
    let max_cl = max_clusters;

    // Step 1-3: Gather graph data (blocking I/O).
    let boundary_data =
        tokio::task::spawn_blocking(move || gather_boundary_data(&graph, &pid, min_freq, max_cl))
            .await
            .unwrap_or_else(|_| Ok(BoundaryData::empty()))?;

    if boundary_data.clusters_text.is_empty() {
        return Ok(Vec::new());
    }

    // Step 4-5: Call LLM / fallback with configurable timeout.
    let mut boundaries = state
        .dreaming
        .suggest_boundaries(
            &boundary_data.clusters_text,
            &boundary_data.state_text,
            &boundary_data.tables_text,
            Duration::from_secs(timeout_secs),
        )
        .await;

    // Step 6: Cross-cluster dependency annotation.
    if include_cross_cluster_deps && boundaries.len() > 1 {
        annotate_cross_cluster_deps(&mut boundaries, &boundary_data);
    }

    Ok(boundaries)
}

/// Internal data gathered from the graph for boundary analysis.
struct BoundaryData {
    clusters_text: String,
    state_text: String,
    tables_text: String,
    /// file -> set of state keys it touches.
    file_state_keys: std::collections::HashMap<String, std::collections::HashSet<String>>,
    /// file -> set of SQL tables it references.
    file_table_refs: std::collections::HashMap<String, std::collections::HashSet<String>>,
}

impl BoundaryData {
    fn empty() -> Self {
        Self {
            clusters_text: String::new(),
            state_text: String::new(),
            tables_text: String::new(),
            file_state_keys: std::collections::HashMap::new(),
            file_table_refs: std::collections::HashMap::new(),
        }
    }
}

/// Annotate boundaries with cross-cluster shared state/table dependencies.
fn annotate_cross_cluster_deps(
    boundaries: &mut [engram_ml::MigrationBoundary],
    data: &BoundaryData,
) {
    use std::collections::{HashMap, HashSet};

    // Build map: data_key -> set of context indices that reference it.
    let mut data_owners: HashMap<String, HashSet<usize>> = HashMap::new();

    for (idx, boundary) in boundaries.iter().enumerate() {
        let boundary_files: HashSet<&str> = boundary.files.iter().map(|s| s.as_str()).collect();
        // Check which state keys files in this boundary touch.
        for f in &boundary_files {
            if let Some(keys) = data.file_state_keys.get(*f) {
                for k in keys {
                    data_owners
                        .entry(format!("state:{k}"))
                        .or_default()
                        .insert(idx);
                }
            }
            if let Some(tables) = data.file_table_refs.get(*f) {
                for t in tables {
                    data_owners
                        .entry(format!("table:{t}"))
                        .or_default()
                        .insert(idx);
                }
            }
        }
    }

    // For each boundary, find which other boundaries share its data.
    let names: Vec<String> = boundaries.iter().map(|b| b.context_name.clone()).collect();
    for (idx, boundary) in boundaries.iter_mut().enumerate() {
        let mut shared: HashSet<String> = HashSet::new();
        let boundary_files: HashSet<&str> = boundary.files.iter().map(|s| s.as_str()).collect();
        for f in &boundary_files {
            if let Some(keys) = data.file_state_keys.get(*f) {
                for k in keys {
                    if let Some(owners) = data_owners.get(&format!("state:{k}")) {
                        for &o in owners {
                            if o != idx {
                                shared.insert(names[o].clone());
                            }
                        }
                    }
                }
            }
            if let Some(tables) = data.file_table_refs.get(*f) {
                for t in tables {
                    if let Some(owners) = data_owners.get(&format!("table:{t}")) {
                        for &o in owners {
                            if o != idx {
                                shared.insert(names[o].clone());
                            }
                        }
                    }
                }
            }
        }
        if !shared.is_empty() {
            boundary.shared_across = shared.into_iter().collect();
            boundary.shared_across.sort();
            // Elevate risk if data is shared across boundaries.
            if boundary.risk != "HIGH" {
                boundary.risk = "HIGH".into();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Iterative union-find with rank heuristic (no stack overflow risk)
// ---------------------------------------------------------------------------

struct UnionFind {
    parent: std::collections::HashMap<String, String>,
    rank: std::collections::HashMap<String, usize>,
}

impl UnionFind {
    fn new() -> Self {
        Self {
            parent: std::collections::HashMap::new(),
            rank: std::collections::HashMap::new(),
        }
    }

    fn make_set(&mut self, x: &str) {
        if !self.parent.contains_key(x) {
            self.parent.insert(x.to_string(), x.to_string());
            self.rank.insert(x.to_string(), 0);
        }
    }

    /// Iterative find with path compression — O(alpha(n)) amortized, no stack overflow.
    fn find(&mut self, x: &str) -> String {
        self.make_set(x);

        // Walk to root.
        let mut current = x.to_string();
        while self.parent[&current] != current {
            current = self.parent[&current].clone();
        }
        let root = current;

        // Path compression: re-walk and point everything to root.
        let mut current = x.to_string();
        while current != root {
            let next = self.parent[&current].clone();
            self.parent.insert(current, root.clone());
            current = next;
        }

        root
    }

    /// Union by rank: attach smaller tree under larger tree.
    fn union(&mut self, a: &str, b: &str) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra == rb {
            return;
        }
        let rank_a = self.rank[&ra];
        let rank_b = self.rank[&rb];
        if rank_a < rank_b {
            self.parent.insert(ra, rb);
        } else if rank_a > rank_b {
            self.parent.insert(rb, ra);
        } else {
            self.parent.insert(rb, ra.clone());
            *self.rank.entry(ra).or_insert(0) += 1;
        }
    }

    fn keys(&self) -> Vec<String> {
        self.parent.keys().cloned().collect()
    }
}

/// Gather temporal coupling clusters + state/SQL context from the graph.
fn gather_boundary_data(
    graph: &engram_graph::GraphStore,
    project_id: &str,
    min_frequency: u32,
    max_clusters: usize,
) -> anyhow::Result<BoundaryData> {
    use std::collections::{HashMap, HashSet};

    // Step 1: Get temporal coupling edges.
    let coupling_edges =
        graph.list_edges_by_kind(project_id, engram_graph::EdgeKind::TemporalCoupling, 500)?;

    if coupling_edges.is_empty() {
        return Ok(BoundaryData::empty());
    }

    // Filter by minimum frequency (weight).
    let strong_edges: Vec<_> = coupling_edges
        .iter()
        .filter(|e| e.weight >= min_frequency)
        .collect();

    if strong_edges.is_empty() {
        return Ok(BoundaryData::empty());
    }

    // Step 2: Iterative union-find with rank heuristic.
    let mut uf = UnionFind::new();
    for e in &strong_edges {
        uf.union(&e.source_id, &e.target_id);
    }

    // Collect clusters.
    let mut clusters: HashMap<String, HashSet<String>> = HashMap::new();
    for key in uf.keys() {
        let root = uf.find(&key);
        clusters.entry(root).or_default().insert(key);
    }

    // Sort clusters by size (largest first), limit count.
    let mut sorted_clusters: Vec<Vec<String>> = clusters
        .into_values()
        .map(|s| {
            let mut v: Vec<_> = s.into_iter().collect();
            v.sort();
            v
        })
        .collect();
    sorted_clusters.sort_by_key(|b| std::cmp::Reverse(b.len()));
    sorted_clusters.truncate(max_clusters);

    // Format clusters text.
    let mut clusters_text = String::new();
    for (i, cluster) in sorted_clusters.iter().enumerate() {
        clusters_text.push_str(&format!(
            "Cluster {} ({} files): {}\n",
            i + 1,
            cluster.len(),
            cluster.join(", ")
        ));
    }

    // Step 3: Gather shared state keys and SQL tables (per-file granularity for cross-cluster).
    let all_files: HashSet<&str> = sorted_clusters
        .iter()
        .flat_map(|c| c.iter().map(|s| s.as_str()))
        .collect();

    let mut global_state_keys: HashSet<String> = HashSet::new();
    let mut global_tables: HashSet<String> = HashSet::new();
    let mut file_state_keys: HashMap<String, HashSet<String>> = HashMap::new();
    let mut file_table_refs: HashMap<String, HashSet<String>> = HashMap::new();

    // Query state edges.
    for kind in &[
        engram_graph::EdgeKind::ReadsState,
        engram_graph::EdgeKind::WritesState,
    ] {
        if let Ok(state_edges) = graph.list_edges_by_kind(project_id, kind.clone(), 200) {
            for e in &state_edges {
                if all_files.contains(e.source_id.as_str()) {
                    global_state_keys.insert(e.target_id.clone());
                    file_state_keys
                        .entry(e.source_id.clone())
                        .or_default()
                        .insert(e.target_id.clone());
                }
            }
        }
    }

    // Query SQL table references.
    if let Ok(sql_edges) =
        graph.list_edges_by_kind(project_id, engram_graph::EdgeKind::QueriesTable, 200)
    {
        for e in &sql_edges {
            if all_files.contains(e.source_id.as_str()) {
                global_tables.insert(e.target_id.clone());
                file_table_refs
                    .entry(e.source_id.clone())
                    .or_default()
                    .insert(e.target_id.clone());
            }
        }
    }

    let state_text = if global_state_keys.is_empty() {
        "(none detected)".to_string()
    } else {
        let mut keys: Vec<_> = global_state_keys.into_iter().collect();
        keys.sort();
        keys.join(", ")
    };

    let tables_text = if global_tables.is_empty() {
        "(none detected)".to_string()
    } else {
        let mut t: Vec<_> = global_tables.into_iter().collect();
        t.sort();
        t.join(", ")
    };

    // sorted_clusters is consumed into clusters_text; per-file maps are used
    // for cross-cluster dependency analysis.
    let _ = sorted_clusters;

    Ok(BoundaryData {
        clusters_text,
        state_text,
        tables_text,
        file_state_keys,
        file_table_refs,
    })
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{static_analyze_file_style, style_language_label};

    #[test]
    fn style_language_label_maps_by_extension() {
        // The LLM style prompt was emitting Python conventions for non-Python
        // files because it was never told the language. Each file type must
        // resolve to its real language so the prompt can constrain the LLM.
        assert_eq!(style_language_label("App_Code/Foo.vb"), "VB.NET");
        assert_eq!(style_language_label("Services/Bar.cs"), "C#");
        assert_eq!(style_language_label("ts/map/x.ts"), "TypeScript");
        assert_eq!(style_language_label("~.js/map.js"), "JavaScript");
        assert_eq!(
            style_language_label("pages/x.aspx"),
            "ASP.NET WebForms markup"
        );
        assert_eq!(style_language_label("Scripts/x.sql"), "SQL");
        assert_eq!(style_language_label("m.py"), "Python");
        assert_ne!(style_language_label("App_Code/Foo.vb"), "Python");
    }

    const OCIUSX_VB_SAMPLE: &str = r#"
Imports System
Imports System.Web
Imports System.Linq

Module sharedfunc
    ''' <summary>
    ''' Redirects and short-circuits the calling method.
    ''' </summary>
    Public Sub SafeRedirect(url As String)
        HttpContext.Current.Response.Redirect(url)
    End Sub

    Public Function GetValidFileName(name As String) As String
        If name Is Nothing Then
            Return String.Empty
        End If
        Return name.Trim()
    End Function

    Public Function GetUser(id As Integer, Optional db As iFaltDataContext = Nothing) As Object
        Using ctx = If(db, New iFaltDataContext())
            Dim row = ctx.Users.FirstOrDefault(Function(u) u.Id = id)
            If row Is Nothing Then
                Return Nothing
            End If
            Return row
        End Using
    End Function

    Public Function SaveUser(user As Object, Optional db As iFaltDataContext = Nothing) As Boolean
        Using ctx = If(db, New iFaltDataContext())
            Try
                ctx.SubmitChanges()
                Return True
            Catch ex As Exception
                Return False
            End Try
        End Using
    End Function

    Public Sub Redirect(url As String)
        SafeRedirect(url)
        Return
    End Sub

    Public Sub TranslateUnit(v As Double)
        If v Is Nothing Then
            Return
        End If
    End Sub
End Module
"#;

    #[test]
    fn vb_static_analyzer_detects_multiple_patterns() {
        let bullets = static_analyze_file_style(OCIUSX_VB_SAMPLE, "sharedfunc.vb");
        // Sanity: we get at least 5 distinct rule bullets on the OciusX shape,
        // covering naming + context injection + Using + Is Nothing + Module.
        assert!(
            bullets.len() >= 5,
            "expected ≥5 bullets, got {}: {bullets:#?}",
            bullets.len()
        );

        let joined = bullets.join(" ");
        assert!(
            joined.contains("PascalCase"),
            "method naming convention must be detected (PascalCase), got: {joined}"
        );
        assert!(
            joined.contains("Optional") && joined.contains("iFaltDataContext"),
            "optional-context-injection pattern must be called out"
        );
        assert!(
            joined.contains("Using"),
            "Using-block discipline must be called out"
        );
        assert!(
            joined.contains("Is Nothing"),
            "Is Nothing guard must be called out"
        );
        assert!(
            joined.contains("Module sharedfunc"),
            "declaration style must cite `Module sharedfunc`, got: {joined}"
        );
    }

    #[test]
    fn vb_static_analyzer_flags_safe_redirect_return_pair() {
        let bullets = static_analyze_file_style(OCIUSX_VB_SAMPLE, "sharedfunc.vb");
        assert!(
            bullets
                .iter()
                .any(|b| b.contains("SafeRedirect") && b.contains("Return")),
            "SafeRedirect+Return pair rule must fire, got: {bullets:#?}"
        );
    }

    #[test]
    fn vb_static_analyzer_prefers_try_catch_when_no_on_error() {
        let bullets = static_analyze_file_style(OCIUSX_VB_SAMPLE, "sharedfunc.vb");
        // The "Try/Catch only" bullet describes the current style AND
        // advises against introducing `On Error Resume Next`, so both
        // phrases appear in the same bullet. We only care that:
        //   - the `Try/Catch only` rule fired, and
        //   - the legacy `On Error Resume Next present` risk bullet did NOT fire.
        assert!(
            bullets.iter().any(|b| b.contains("`Try/Catch` only")),
            "Try/Catch-only rule must fire, got: {bullets:#?}"
        );
        assert!(
            !bullets
                .iter()
                .any(|b| b.contains("On Error Resume Next present")),
            "legacy On Error risk bullet must NOT fire for clean Try/Catch code"
        );
    }

    #[test]
    fn vb_static_analyzer_flags_legacy_on_error() {
        let legacy = r#"
Module Legacy
    Public Sub Do()
        On Error Resume Next
        DoStuff()
    End Sub
End Module
"#;
        let bullets = static_analyze_file_style(legacy, "legacy.vb");
        assert!(
            bullets
                .iter()
                .any(|b| b.contains("On Error Resume Next") && b.contains("risk")),
            "legacy On Error must be flagged as risk, got: {bullets:#?}"
        );
    }

    #[test]
    fn cs_static_analyzer_emits_generic_bullets() {
        let cs = r#"
using System;
public class Foo {
    public async Task Bar() {
        using (var db = new Context()) {
            await db.SaveChangesAsync();
        }
        using (var tx = db.BeginTransaction()) {
            tx.Commit();
        }
    }

    public async Task Baz() {
        using (var conn = new SqlConnection()) {
            await conn.OpenAsync();
        }
    }
}
"#;
        let bullets = static_analyze_file_style(cs, "Foo.cs");
        assert!(
            bullets.iter().any(|b| b.contains("using")),
            "C# using-pattern bullet expected, got: {bullets:#?}"
        );
        assert!(
            bullets.iter().any(|b| b.contains("async")),
            "C# async-pattern bullet expected"
        );
    }

    #[test]
    fn generic_static_analyzer_detects_indent_style() {
        let tabbed = "\tfn a() {\n\t\tlet x = 1;\n\t}\n";
        let bullets = static_analyze_file_style(tabbed, "unknown.xyz");
        assert!(
            bullets.iter().any(|b| b.contains("tabs")),
            "tab indentation must be detected in generic fallback"
        );
    }

    #[test]
    fn vb_static_analyzer_returns_empty_on_tiny_file() {
        let tiny = "Module X\nEnd Module\n";
        let bullets = static_analyze_file_style(tiny, "x.vb");
        // Module declaration fires but nothing else — that's fine and
        // prevents the caller from claiming rich style on trivial files.
        assert!(bullets.len() <= 2);
    }

    // ── TypeScript ───────────────────────────────────────────────────────
    // Representative OciusX-style file: triple-slash references (no ES6
    // modules), jQuery, camelCase functions, const declarations, typed
    // parameters. This exercises most TS detectors at once.
    const OCIUSX_TS_SAMPLE: &str = r##"
/// <reference path="../Q.ts" />
/// <reference path="../jquery.d.ts" />

namespace App.Forms {
    export class OrderForm {
        private orderId: number;

        constructor(id: number) {
            this.orderId = id;
        }

        public loadOrder(): void {
            $.ajax({ url: "/api/order/" + this.orderId }).done((r: any) => {
                $("#customer").val(r.customer);
                $("#total").text(r.total);
            });
        }
    }

    export function formatMoney(amount: number): string {
        return "$" + amount.toFixed(2);
    }

    export const saveOrder = (o: any): Promise<void> => {
        return $.ajax({ url: "/api/save", data: o, method: "POST" });
    };

    interface OrderDto {
        id: number;
        customer: string;
        total: number;
    }

    type OrderId = number;
}
"##;

    #[test]
    fn ts_static_analyzer_detects_ociusx_patterns() {
        let bullets = static_analyze_file_style(OCIUSX_TS_SAMPLE, "OrderForm.ts");
        // Parity with VB: ≥8 prescriptive bullets on an OciusX-shape TS file.
        assert!(
            bullets.len() >= 8,
            "expected ≥8 TS bullets for VB parity, got {}: {bullets:#?}",
            bullets.len()
        );
        let joined = bullets.join(" | ");
        assert!(
            joined.contains("triple-slash"),
            "triple-slash reference detector must fire on OciusX-style TS, got: {joined}"
        );
        // Triple-slash paths should be cited verbatim (parity with VB's `Module sharedfunc`).
        assert!(
            joined.contains("../Q.ts") || joined.contains("Q.ts"),
            "triple-slash bullet must cite the referenced path, got: {joined}"
        );
        assert!(
            joined.contains("jQuery"),
            "jQuery usage must be called out, got: {joined}"
        );
        assert!(
            joined.contains("interface") || joined.contains("type"),
            "interface / type-alias count must be reported, got: {joined}"
        );
        // Declaration-style bullet must cite the actual namespace.
        assert!(
            joined.contains("namespace App.Forms") || joined.contains("App.Forms"),
            "namespace declaration bullet must cite the name, got: {joined}"
        );
    }

    #[test]
    fn ts_static_analyzer_reports_type_annotations_and_any_risk() {
        let bullets = static_analyze_file_style(OCIUSX_TS_SAMPLE, "OrderForm.ts");
        let joined = bullets.join(" | ");
        assert!(
            joined.contains("Type annotations"),
            "typed-param detector must fire, got: {joined}"
        );
        // The `: any` RISK bullet is exercised separately by TS_RISK_SAMPLE where it
        // has 3+ occurrences. The OciusX-shape sample deliberately has only 2, so
        // here we just confirm the sample stays under the risk threshold.
        assert!(
            !joined.contains("TYPE RISK") || !joined.contains("`: any`"),
            "clean OciusX sample should not trip the `: any` risk, got: {joined}"
        );
    }

    // Rich TS sample exercising the new risk / cast / generic detectors.
    const TS_RISK_SAMPLE: &str = r##"
import { Foo } from "./foo";
import { Bar } from "./bar";
import { Baz } from "./baz";

/**
 * Public order service.
 */
export class OrderService<T extends Entity, U> {
    readonly id: number;
    readonly name: string;
    readonly flag: boolean;

    constructor(id: number) {
        this.id = id;
        this.name = "";
        this.flag = false;
    }

    load(raw: any): Order {
        const input = raw as any;
        const other = input as unknown;
        const parsed = input!.payload;
        const node = document.getElementById(input!.id)!.dataset;
        const more = parsed!.next;
        return parsed as Order;
    }

    async save(): Promise<void> {
        try {
            await this.send();
        } catch {
        }
    }

    private send(): Promise<void> {
        return fetch("/api").then(() => {}).catch(() => {});
    }
}

export function identity<T>(x: T): T { return x; }
export function pair<A, B>(a: A, b: B): [A, B] { return [a, b]; }
"##;

    #[test]
    fn ts_static_analyzer_flags_type_and_empty_catch_risks() {
        let bullets = static_analyze_file_style(TS_RISK_SAMPLE, "OrderService.ts");
        let joined = bullets.join(" | ");
        assert!(
            joined.contains("TYPE RISK")
                && (joined.contains("as any") || joined.contains("as unknown")),
            "`as any` / `as unknown` erasure-cast risk must fire, got: {joined}"
        );
        assert!(
            joined.contains("TYPE RISK") && joined.contains("non-null assertions"),
            "non-null-assertion risk must fire (sample has 3+ `x!.y`), got: {joined}"
        );
        assert!(
            joined.contains("Immutability") || joined.contains("readonly"),
            "readonly discipline bullet must fire, got: {joined}"
        );
        assert!(
            joined.contains("Generics"),
            "generics bullet must fire, got: {joined}"
        );
        assert!(
            joined.contains("silently-swallowed errors") || joined.contains("empty `catch"),
            "empty-catch risk must fire, got: {joined}"
        );
    }

    #[test]
    fn ts_static_analyzer_cites_import_modules() {
        let bullets = static_analyze_file_style(TS_RISK_SAMPLE, "OrderService.ts");
        let joined = bullets.join(" | ");
        assert!(
            joined.contains("Imports") && joined.contains("./foo"),
            "imports bullet must cite top modules by name, got: {joined}"
        );
    }

    // ── JavaScript ───────────────────────────────────────────────────────
    // Legacy-flavored JS: mix of `var`/`let`/`const`, function declarations +
    // arrow expressions, jQuery, "use strict", module.exports, prototype.
    const LEGACY_JS_SAMPLE: &str = r##"
"use strict";

var counter = 0;
let running = false;
const MAX = 10;

function initPage() {
    $("#submit").on("click", function() {
        counter++;
        $.ajax({ url: "/api/ping" });
    });
}

const nextTick = () => {
    running = true;
    setTimeout(initPage, 100);
};

function Widget(id) {
    this.id = id;
}

Widget.prototype.render = function() {
    return document.getElementById(this.id);
};

Widget.prototype.destroy = function() {
    this.id = null;
};

module.exports = { initPage: initPage, Widget: Widget };
"##;

    #[test]
    fn js_static_analyzer_detects_legacy_patterns() {
        let bullets = static_analyze_file_style(LEGACY_JS_SAMPLE, "legacy.js");
        // Parity with VB: ≥7 prescriptive bullets on a representative JS file.
        assert!(
            bullets.len() >= 7,
            "expected ≥7 JS bullets for VB parity, got {}: {bullets:#?}",
            bullets.len()
        );
        let joined = bullets.join(" | ");
        assert!(
            joined.contains("use strict"),
            "`use strict` directive must be reported, got: {joined}"
        );
        assert!(
            joined.contains("Variable declarations"),
            "var/let/const distribution must be reported, got: {joined}"
        );
        assert!(
            joined.contains("jQuery") || joined.contains("DOM"),
            "jQuery / DOM-access detector must fire, got: {joined}"
        );
        assert!(
            joined.contains("prototype") || joined.contains("Paradigm"),
            "prototype-vs-class paradigm detector must fire, got: {joined}"
        );
        // The bullets must now carry prescriptive advice (parity with VB).
        assert!(
            joined.contains("prefer") || joined.contains("match") || joined.contains("Follow"),
            "bullets must be prescriptive (`prefer`/`match`/`Follow`), got: {joined}"
        );
    }

    // JS sample with security + error-swallowing risks.
    const JS_RISK_SAMPLE: &str = r##"
function loadUser(id) {
    var el = document.getElementById("out");
    try {
        el.innerHTML = fetchHtml(id);
    } catch (e) {
    }
    document.write("<div>" + id + "</div>");
    eval("var x = " + id);
    const dynamic = new Function("return " + id);
    $.ajax({ url: "/api" }).catch(function() {});
    $.ajax({ url: "/api2" }).catch(() => {});
    __doPostBack("ctl00$Main$btnSave", "");
}

async function noop() {
    return 42;
}
"##;

    #[test]
    fn js_static_analyzer_flags_security_risks() {
        let bullets = static_analyze_file_style(JS_RISK_SAMPLE, "unsafe.js");
        let joined = bullets.join(" | ");
        assert!(
            joined.contains("SECURITY RISK"),
            "security-risk bullet must fire, got: {joined}"
        );
        assert!(
            joined.contains("eval")
                && joined.contains("innerHTML")
                && joined.contains("document.write"),
            "security bullet must enumerate each risky call, got: {joined}"
        );
    }

    #[test]
    fn js_static_analyzer_flags_empty_catch_and_postback() {
        let bullets = static_analyze_file_style(JS_RISK_SAMPLE, "unsafe.js");
        let joined = bullets.join(" | ");
        assert!(
            joined.contains("silently-swallowed errors"),
            "empty `catch` / no-op `.catch()` must be flagged, got: {joined}"
        );
        assert!(
            joined.contains("WebForms bridge") && joined.contains("__doPostBack"),
            "OciusX `__doPostBack` bridge must be flagged, got: {joined}"
        );
    }

    #[test]
    fn js_static_analyzer_flags_missing_await() {
        let bullets = static_analyze_file_style(JS_RISK_SAMPLE, "unsafe.js");
        let joined = bullets.join(" | ");
        // `async function noop()` has no `await` inside → should fire as RISK.
        assert!(
            joined.contains("async") && joined.contains("await"),
            "missing-await risk must be detected for `async` without `await`, got: {joined}"
        );
    }

    // ── Framework-aware detectors (React / Node / test runner) ───────────
    const REACT_SAMPLE: &str = r##"
import React, { useState, useEffect, useCallback } from "react";
import { render } from "react-dom";

export function OrderPanel(props: { id: number }) {
    const [loading, setLoading] = useState(false);
    const [order, setOrder] = useState<Order | null>(null);

    useEffect(() => {
        setLoading(true);
        fetch(`/api/orders/${props.id}`)
            .then(r => r.json())
            .then(setOrder)
            .finally(() => setLoading(false));
    }, [props.id]);

    const reload = useCallback(() => {
        setOrder(null);
    }, []);

    return (
        <div className="panel">
            <h1>Order {props.id}</h1>
            {loading && <Spinner size="sm" />}
            {order && <OrderTable data={order} onChange={reload} />}
        </div>
    );
}
"##;

    #[test]
    fn ts_static_analyzer_detects_react_framework() {
        let bullets = static_analyze_file_style(REACT_SAMPLE, "OrderPanel.tsx");
        let joined = bullets.join(" | ");
        assert!(
            joined.contains("Framework signal") && joined.contains("React"),
            "React signal must be detected on JSX + hooks file, got: {joined}"
        );
    }

    const NODE_EXPRESS_SAMPLE: &str = r##"
import express from "express";
import path from "path";

const app = express();
app.use(express.json());

app.get("/health", (req, res) => {
    res.json({ ok: true, dir: __dirname });
});

app.post("/submit", (req, res) => {
    const body = req.body;
    if (!body || !body.name) {
        return res.status(400).json({ error: "name required" });
    }
    res.json({ saved: true });
});

app.listen(process.env.PORT || 3000);
"##;

    #[test]
    fn ts_static_analyzer_detects_node_express() {
        let bullets = static_analyze_file_style(NODE_EXPRESS_SAMPLE, "server.ts");
        let joined = bullets.join(" | ");
        assert!(
            joined.contains("Framework signal") && joined.contains("Express"),
            "Express signal must be detected, got: {joined}"
        );
    }

    #[test]
    fn ts_static_analyzer_does_not_falsely_flag_react_on_angle_casts() {
        // Pure angle-bracket casts must NOT be counted as JSX (the false-positive
        // we saw on OciusX `<HTMLInputElement>elem` sites).
        let sample = r##"
/// <reference path="./q.ts" />
class widgetCtrl {
    private _id: number;
    constructor(el: HTMLElement) {
        this._id = Number((<HTMLInputElement>el).value);
        const anchor = <HTMLAnchorElement>document.getElementById("a");
        const span = <HTMLSpanElement>document.getElementById("b");
        const div = <HTMLDivElement>document.getElementById("c");
    }
}
"##;
        let bullets = static_analyze_file_style(sample, "widget.ts");
        let joined = bullets.join(" | ");
        assert!(
            !joined.contains("React"),
            "angle-bracket casts must NOT be mistaken for JSX/React, got: {joined}"
        );
        // But the cast-style detector SHOULD flag the legacy style.
        assert!(
            joined.contains("angle-bracket"),
            "angle-bracket cast style must be reported, got: {joined}"
        );
    }

    #[test]
    fn js_static_analyzer_detects_transpiled_output() {
        let transpiled = r##"
"use strict";
var __extends = (this && this.__extends) || (function () { return function () {}; })();
var __assign = (this && this.__assign) || function () { };
var q;
(function (q) {
    var helper = (function () {
        function helper() {}
        helper.prototype.doThing = function () { return 42; };
        return helper;
    })();
    q.helper = helper;
})(q || (q = {}));
"##;
        let bullets = static_analyze_file_style(transpiled, "dist/helper.js");
        let joined = bullets.join(" | ");
        assert!(
            joined.contains("Transpiled output") && joined.contains("__extends"),
            "transpiled-TS fingerprint must fire, got: {joined}"
        );
    }

    #[test]
    fn ts_static_analyzer_detects_underscore_field_convention() {
        let sample = r##"
class OrderManager {
    private _id: number = 0;
    private _name: string = "";
    private _active: boolean = false;
    private _items: Array<Item> = [];

    load(raw: any): void {
        this._id = raw.id;
        this._name = raw.name;
        this._active = raw.active;
        this._items = raw.items;
        this._refresh();
        this._notify();
        this._updateUi();
        this._saveState();
        this._log();
        this._render();
        this._persist();
    }
}
"##;
        let bullets = static_analyze_file_style(sample, "OrderManager.ts");
        let joined = bullets.join(" | ");
        assert!(
            joined.contains("Private fields") && joined.contains("_underscorePrefix"),
            "`_underscorePrefix` convention must fire on heavy `private _x` usage, got: {joined}"
        );
    }

    #[test]
    fn ts_static_analyzer_detects_angular_di_ctor() {
        let sample = r##"
import { Component } from "@angular/core";

@Component({ selector: "app-root", template: "" })
export class AppComponent {
    constructor(
        private readonly userService: UserService,
        private logger: LoggerService,
        public router: Router,
    ) {}
}
"##;
        let bullets = static_analyze_file_style(sample, "app.component.ts");
        let joined = bullets.join(" | ");
        assert!(
            joined.contains("Dependency injection") && joined.contains("parameter-property DI"),
            "Angular constructor DI must be flagged, got: {joined}"
        );
    }

    #[test]
    fn ts_static_analyzer_detects_i_prefix_interface_convention() {
        let sample = r##"
interface IUser { id: number; name: string; }
interface IOrder { id: number; total: number; }
interface IProduct { id: number; sku: string; }
interface IAddress { street: string; city: string; }
"##;
        let bullets = static_analyze_file_style(sample, "types.ts");
        let joined = bullets.join(" | ");
        assert!(
            joined.contains("`I`-prefix convention"),
            "I-prefix interface convention must be detected, got: {joined}"
        );
    }

    // ── ASPX / ASCX / Master ─────────────────────────────────────────────
    // Representative ASPX with Page directive, MasterPageFile, codebehind,
    // UpdatePanel, GridView, validators, Register directives.
    const ASPX_SAMPLE: &str = r#"
<%@ Page Language="VB" MasterPageFile="~/site.Master" CodeBehind="Order.aspx.vb" Inherits="App.Order" %>
<%@ Register TagPrefix="uc" TagName="Pager" Src="~/ctrls/Pager.ascx" %>
<%@ Register Assembly="AjaxControlToolkit" Namespace="AjaxControlToolkit" TagPrefix="ajax" %>

<asp:Content ID="c1" ContentPlaceHolderID="main" runat="server">
    <asp:ScriptManager ID="sm" runat="server" />
    <asp:UpdatePanel ID="up1" runat="server">
        <ContentTemplate>
            <asp:GridView ID="gv" runat="server" DataSourceID="ods1" AutoGenerateColumns="false">
                <Columns>
                    <asp:BoundField DataField="Name" HeaderText="Name" />
                </Columns>
            </asp:GridView>
            <asp:Repeater ID="rp" runat="server" DataSourceID="ods2">
                <ItemTemplate><%# Eval("Title") %></ItemTemplate>
            </asp:Repeater>
            <asp:RequiredFieldValidator ID="rfv" runat="server" ControlToValidate="txtName" />
            <asp:RegularExpressionValidator ID="rxv" runat="server" ControlToValidate="txtEmail"
                ValidationExpression="^[^@]+@[^@]+$" />
        </ContentTemplate>
    </asp:UpdatePanel>
    <script src="/scripts/order.js"></script>
</asp:Content>
"#;

    #[test]
    fn aspx_static_analyzer_detects_ajax_and_data_controls() {
        let bullets = static_analyze_file_style(ASPX_SAMPLE, "Order.aspx");
        assert!(
            bullets.len() >= 5,
            "expected ≥5 ASPX bullets, got {}: {bullets:#?}",
            bullets.len()
        );
        let joined = bullets.join(" | ");
        assert!(
            joined.contains("Page"),
            "Page directive must be reported, got: {joined}"
        );
        assert!(
            joined.contains("Master page binding") && joined.contains("site.Master"),
            "MasterPageFile must be extracted verbatim, got: {joined}"
        );
        assert!(
            joined.contains("codebehind"),
            "CodeBehind= split must be reported, got: {joined}"
        );
        assert!(
            joined.contains("UpdatePanel"),
            "UpdatePanel count must be reported, got: {joined}"
        );
        assert!(
            joined.contains("GridView") || joined.contains("Data-display control"),
            "data-display control detector must fire, got: {joined}"
        );
        assert!(
            joined.contains("Validation"),
            "validator rollup must fire, got: {joined}"
        );
    }

    // ── SQL ──────────────────────────────────────────────────────────────
    // T-SQL-flavored sample with prefixed names, UPPER keywords, dbo. schema,
    // BEGIN TRY, CREATE OR ALTER, @params.
    const SQL_SAMPLE: &str = r#"
CREATE OR ALTER PROCEDURE [dbo].[fj_GetFiberjobb]
    @jobbId INT,
    @userId INT
AS
BEGIN
    BEGIN TRY
        SELECT f.*, u.Name
        FROM [dbo].[fj_fiberjobb] f
        INNER JOIN [dbo].[aspnet_Users] u ON u.UserId = f.OwnerId
        WHERE f.Id = @jobbId AND f.OwnerId = @userId;
    END TRY
    BEGIN CATCH
        THROW;
    END CATCH
END
GO

CREATE OR ALTER PROCEDURE [dbo].[fj_SaveFiberjobb]
    @jobbId INT,
    @data NVARCHAR(MAX)
AS
BEGIN
    BEGIN TRANSACTION
        UPDATE [dbo].[fj_fiberjobb] SET Data = @data WHERE Id = @jobbId;
    COMMIT
END
GO

CREATE TABLE [dbo].[pr_profile] (Id INT, Name NVARCHAR(100));
CREATE VIEW [dbo].[pr_profile_active] AS SELECT * FROM [dbo].[pr_profile];
"#;

    #[test]
    fn sql_static_analyzer_detects_prefix_and_tsql_patterns() {
        let bullets = static_analyze_file_style(SQL_SAMPLE, "fj_procs.sql");
        assert!(
            bullets.len() >= 5,
            "expected ≥5 SQL bullets, got {}: {bullets:#?}",
            bullets.len()
        );
        let joined = bullets.join(" | ");
        assert!(
            joined.contains("Prefix namespaces") && joined.contains("fj_"),
            "prefix-namespace rollup must surface `fj_`, got: {joined}"
        );
        assert!(
            joined.contains("UPPERCASE") || joined.contains("Keyword casing"),
            "keyword-casing detector must fire, got: {joined}"
        );
        assert!(
            joined.contains("Schema qualification") || joined.contains("dbo."),
            "schema-qualification detector must fire, got: {joined}"
        );
        assert!(
            joined.contains("Transactions") || joined.contains("TRAN"),
            "transaction detector must fire, got: {joined}"
        );
        assert!(
            joined.contains("CREATE OR ALTER") || joined.contains("Proc style"),
            "CREATE-OR-ALTER idempotency must be called out, got: {joined}"
        );
    }

    // ── Python ───────────────────────────────────────────────────────────
    const PYTHON_SAMPLE: &str = r#"
from typing import Optional
import json

class OrderService:
    def __init__(self, db: Database) -> None:
        self.db = db

    def get_order(self, order_id: int) -> Optional[dict]:
        try:
            row = self.db.query(order_id)
            if row is None:
                return None
            return {"id": row.id, "total": row.total}
        except ValueError as exc:
            raise RuntimeError(f"bad order: {exc}") from exc

    async def save_order(self, order: dict) -> bool:
        try:
            await self.db.write(order)
            return True
        except Exception:
            return False

def format_total(total: float) -> str:
    return f"${total:.2f}"
"#;

    #[test]
    fn python_static_analyzer_reports_type_hints_and_naming() {
        let bullets = static_analyze_file_style(PYTHON_SAMPLE, "orders.py");
        assert!(
            bullets.len() >= 3,
            "expected ≥3 Python bullets, got {}: {bullets:#?}",
            bullets.len()
        );
        let joined = bullets.join(" | ");
        assert!(
            joined.contains("snake_case"),
            "snake_case function naming must be detected, got: {joined}"
        );
        assert!(
            joined.contains("Type hints"),
            "type-hint detector must fire, got: {joined}"
        );
        assert!(
            joined.contains("Async") || joined.contains("await"),
            "async detector must fire, got: {joined}"
        );
    }

    // ── Rust ─────────────────────────────────────────────────────────────
    const RUST_SAMPLE: &str = r#"
use std::collections::HashMap;
use tokio::sync::RwLock;
use anyhow::Result;

/// Top-level user service.
/// Uses `?` everywhere, no unwraps in hot paths.
#[derive(Debug, Clone)]
pub struct UserService {
    users: RwLock<HashMap<u64, User>>,
}

#[derive(Debug, Clone)]
pub struct User {
    pub id: u64,
    pub name: String,
}

#[derive(Debug)]
pub enum UserError {
    Missing,
    Invalid,
}

pub trait UserRepository {
    fn fetch(&self, id: u64) -> Option<User>;
}

impl UserService {
    pub async fn get_user(&self, id: u64) -> Result<User> {
        let guard = self.users.read().await;
        let u = guard.get(&id).ok_or_else(|| anyhow::anyhow!("missing"))?;
        Ok(u.clone())
    }

    pub async fn save_user(&self, user: User) -> Result<()> {
        let mut guard = self.users.write().await;
        guard.insert(user.id, user);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roundtrip() {
        let svc = UserService { users: RwLock::new(HashMap::new()) };
        svc.save_user(User { id: 1, name: "a".into() }).await.unwrap();
        let u = svc.get_user(1).await.unwrap();
        assert_eq!(u.id, 1);
    }
}
"#;

    #[test]
    fn rust_static_analyzer_reports_fn_casing_and_error_handling() {
        let bullets = static_analyze_file_style(RUST_SAMPLE, "user_service.rs");
        assert!(
            bullets.len() >= 3,
            "expected ≥3 Rust bullets, got {}: {bullets:#?}",
            bullets.len()
        );
        let joined = bullets.join(" | ");
        assert!(
            joined.contains("snake_case"),
            "snake_case fn naming must be detected, got: {joined}"
        );
        assert!(
            joined.contains("PascalCase"),
            "PascalCase type naming must be detected, got: {joined}"
        );
        assert!(
            joined.contains("Error handling"),
            "error-handling detector must fire, got: {joined}"
        );
        assert!(
            joined.contains("tokio") || joined.contains("Async runtime"),
            "async-runtime detector must fire, got: {joined}"
        );
    }

    // ── Go ───────────────────────────────────────────────────────────────
    const GO_SAMPLE: &str = r#"
package orders

import (
    "context"
    "errors"
    "fmt"
)

type Service struct {
    db Database
}

func NewService(db Database) *Service {
    return &Service{db: db}
}

func (s *Service) GetOrder(ctx context.Context, id int64) (*Order, error) {
    row, err := s.db.Query(ctx, id)
    if err != nil {
        return nil, fmt.Errorf("get: %w", err)
    }
    if row == nil {
        return nil, errors.New("not found")
    }
    return row, nil
}

func (s *Service) worker(ch chan int) {
    for id := range ch {
        go s.process(id)
    }
}
"#;

    #[test]
    fn go_static_analyzer_reports_err_checks_and_concurrency() {
        let bullets = static_analyze_file_style(GO_SAMPLE, "service.go");
        assert!(
            bullets.len() >= 3,
            "expected ≥3 Go bullets, got {}: {bullets:#?}",
            bullets.len()
        );
        let joined = bullets.join(" | ");
        assert!(
            joined.contains("err != nil"),
            "`if err != nil` detector must fire, got: {joined}"
        );
        assert!(
            joined.contains("Imports") || joined.contains("import"),
            "grouped imports must be reported, got: {joined}"
        );
    }

    // ── Java ─────────────────────────────────────────────────────────────
    const JAVA_SAMPLE: &str = r#"
package com.example.order;

import java.util.List;
import java.util.Optional;
import java.util.stream.Collectors;
import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.stereotype.Component;

@Component
public class OrderService {

    @Autowired
    private OrderRepository repo;

    public Optional<Order> findOrder(long id) throws OrderNotFoundException {
        try {
            Optional<Order> opt = repo.findById(id);
            return opt;
        } catch (RuntimeException ex) {
            throw new OrderNotFoundException("missing", ex);
        }
    }

    public List<String> names(List<Order> orders) {
        return orders.stream()
            .map(o -> o.getName())
            .collect(Collectors.toList());
    }
}
"#;

    #[test]
    fn java_static_analyzer_reports_streams_and_di() {
        let bullets = static_analyze_file_style(JAVA_SAMPLE, "OrderService.java");
        assert!(
            bullets.len() >= 3,
            "expected ≥3 Java bullets, got {}: {bullets:#?}",
            bullets.len()
        );
        let joined = bullets.join(" | ");
        assert!(
            joined.contains("stream") || joined.contains("lambda"),
            "modern-Java detector must fire, got: {joined}"
        );
        assert!(
            joined.contains("Autowired") || joined.contains("DI annotations"),
            "Spring DI detector must fire, got: {joined}"
        );
    }

    // ── Improved generic fallback ────────────────────────────────────────
    #[test]
    fn generic_static_analyzer_covers_multiple_universals() {
        // An unknown-extension file big enough to trip the file-length +
        // comment-density + indentation detectors simultaneously.
        let mut content = String::new();
        content.push_str("# header comment\n");
        content.push_str("# another header comment\n");
        for _ in 0..120 {
            content.push_str("    foo_bar = 1\n");
            content.push_str("    # comment line that keeps density high\n");
        }
        let bullets = static_analyze_file_style(&content, "data.toml");
        assert!(
            bullets.len() >= 3,
            "generic fallback must emit ≥3 bullets on substantial content, got {}: {bullets:#?}",
            bullets.len()
        );
        let joined = bullets.join(" | ");
        assert!(
            joined.contains("Indentation"),
            "indentation bullet must fire, got: {joined}"
        );
        assert!(
            joined.contains("Comment density"),
            "comment-density bullet must fire, got: {joined}"
        );
    }
}
