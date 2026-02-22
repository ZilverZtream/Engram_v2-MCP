//! Phase 36: LLM-Powered Business Logic Comprehension
//!
//! Uses the local LLM (Qwen 2.5 Coder 14B via Ollama) to analyze extracted method
//! bodies and produce queryable natural-language business logic summaries.
//!
//! The developer should never need to open the legacy codebase — just query Engram.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

use engram_core::ids::ContentHash;
use engram_ml::DreamingEngine;
use regex::Regex;
use serde::Serialize;

use super::full_project_migration_service::{
    MethodInfo, MethodKind, extract_cs_method_body, extract_vb_method_body,
};

// ── Prompt Template ──────────────────────────────────────────────────────────

const METHOD_ANALYSIS_PROMPT: &str = r#"Analyze this {language} method and describe its business logic in plain English.

Class: {class_name}
Method: {method_name}

```{language_tag}
{method_body}
```

Respond in this exact format (plain text only, no markdown, no backticks, no bold):
PURPOSE: <one sentence explaining what this method does>
STEPS:
1. <first action>
2. <next action>
RULES:
- <business rule or condition>
DATA: <what data it reads/writes - table and field names>
ERRORS: <error handling behavior>
EFFECTS: <state changes: DB writes, session, UI, redirects>"#;

const FILE_PURPOSE_PROMPT: &str = r#"This {language} class has the following methods:
{method_list}

In ONE sentence, describe the overall purpose of this page/class from a business perspective.
Respond with only the sentence, no prefix."#;

// ── Structs ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct MethodBusinessLogic {
    pub file_path: String,
    pub method_name: String,
    pub fqn: String,
    pub purpose: String,
    pub steps: Vec<String>,
    pub business_rules: Vec<String>,
    pub data_flow: String,
    pub error_handling: String,
    pub side_effects_detail: String,
    pub content_hash: String,
    /// Confidence level from LLM validation (High / Medium / Low / empty if not validated).
    #[serde(default)]
    pub confidence: String,
    /// Warnings from cross-validation of LLM output against deterministic effects.
    #[serde(default)]
    pub validation_warnings: Vec<String>,
}

// ── Ticket 37.2: LLM Validation Gate ─────────────────────────────────────────

/// Result of cross-validating LLM output against deterministic extraction.
#[derive(Debug, Clone, Serialize)]
pub struct ValidationResult {
    pub confidence: Confidence,
    pub warnings: Vec<String>,
}

/// Confidence level assigned after cross-validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Confidence {
    High,
    Medium,
    Low,
}

impl std::fmt::Display for Confidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::High => write!(f, "High"),
            Self::Medium => write!(f, "Medium"),
            Self::Low => write!(f, "Low"),
        }
    }
}

/// Cross-validate LLM output against deterministic effects.
///
/// Compares what the LLM reported vs what static analysis already found.
/// Checks both directions: effects the LLM missed, and tables the LLM
/// hallucinated. Returns a confidence score and list of discrepancy warnings.
pub fn validate_llm_output(
    llm: &MethodBusinessLogic,
    deterministic: &MethodBusinessLogic,
    effects: &[String],
) -> ValidationResult {
    let mut warnings = Vec::new();
    let llm_data_lower = llm.data_flow.to_lowercase();
    let llm_effects_lower = llm.side_effects_detail.to_lowercase();
    let llm_all_lower = format!(
        "{} {} {} {} {}",
        llm_data_lower,
        llm_effects_lower,
        llm.purpose.to_lowercase(),
        llm.steps.join(" ").to_lowercase(),
        llm.business_rules.join(" ").to_lowercase(),
    );

    // Category checkers: (keyword_in_effect, keyword_in_llm, description)
    let categories: &[(&str, &str, &str)] = &[
        ("sql:", "sql", "database access"),
        ("session", "session", "Session usage"),
        ("redirect", "redirect", "Redirect"),
        ("viewstate", "viewstate", "ViewState usage"),
        ("cache", "cache", "Cache usage"),
        ("application", "application[", "Application state usage"),
        ("cookie", "cookie", "Cookie usage"),
        ("email", "email", "Email sending"),
        ("file", "file", "File I/O"),
    ];

    for effect in effects {
        let eff_lower = effect.to_lowercase();

        for &(eff_keyword, llm_keyword, desc) in categories {
            if eff_lower.contains(eff_keyword) && !llm_all_lower.contains(llm_keyword) {
                warnings.push(format!(
                    "LLM missed {desc} detected by static analysis: {effect}"
                ));
                break; // one warning per effect is enough
            }
        }
    }

    // Check if deterministic found error handling but LLM said none
    if deterministic.error_handling.contains("Has error handling")
        && (llm.error_handling.is_empty()
            || llm.error_handling.to_lowercase().contains("no error")
            || llm.error_handling.to_lowercase().contains("none"))
    {
        warnings.push(
            "LLM reports no error handling, but static analysis found Try/Catch or On Error"
                .to_string(),
        );
    }

    // Check if LLM mentions tables not found in deterministic analysis
    static TABLE_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\b(?:FROM|INTO|UPDATE|JOIN)\s+\[?(\w+)\]?").expect("TABLE_RE")
    });
    static SQL_KEYWORDS: LazyLock<std::collections::HashSet<&'static str>> = LazyLock::new(|| {
        [
            "select", "where", "set", "values", "table", "dbo", "sys", "null", "not", "and", "or",
            "as", "on", "into", "inner", "outer", "left", "right", "cross", "top", "distinct",
            "case", "when", "then", "else", "end", "begin", "declare", "cursor", "fetch",
            "inserted", "deleted",
        ]
        .into_iter()
        .collect()
    });

    let effects_joined = effects.join(" ").to_lowercase();
    let det_all_lower = format!(
        "{} {}",
        deterministic.data_flow.to_lowercase(),
        deterministic.side_effects_detail.to_lowercase()
    );
    for cap in TABLE_RE.captures_iter(&llm.data_flow) {
        let table = cap[1].to_lowercase();
        if SQL_KEYWORDS.contains(table.as_str()) {
            continue;
        }
        // Check both effects and deterministic data_flow
        if !effects_joined.contains(&table) && !det_all_lower.contains(&table) {
            warnings.push(format!(
                "LLM mentioned table '{table}' not found in static analysis — verify"
            ));
        }
    }

    let confidence = match warnings.len() {
        0 => Confidence::High,
        1 | 2 => Confidence::Medium,
        _ => Confidence::Low,
    };

    ValidationResult {
        confidence,
        warnings,
    }
}

/// Return the confidence badge emoji for rendering in markdown.
pub fn confidence_badge(confidence: &str) -> &'static str {
    match confidence {
        "High" => "✅ High",
        "Medium" => "⚠️ Medium",
        "Low" => "❌ Low",
        _ => "",
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FileBusinessLogic {
    pub file_path: String,
    pub class_name: String,
    pub file_purpose: String,
    pub methods: Vec<MethodBusinessLogic>,
    pub analyzed_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectBusinessLogicReport {
    pub project_id: String,
    pub files_analyzed: usize,
    pub methods_analyzed: usize,
    pub methods_skipped_cached: usize,
    pub llm_failures: usize,
    pub file_summaries: Vec<FileBusinessLogic>,
}

// ── LLM Response Parsing ─────────────────────────────────────────────────────

static PURPOSE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?im)^PURPOSE:\s*(.+)$").expect("PURPOSE_RE"));
static STEPS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?im)^\d+\.\s*(.+)$").expect("STEPS_RE"));
static RULES_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?im)^-\s*(.+)$").expect("RULES_RE"));
// Note: DATA/ERRORS/EFFECTS use extract_section_text() instead of single-line regex
// to support multi-line LLM responses for these sections.

/// Parse a structured LLM response into a `MethodBusinessLogic`.
pub fn parse_llm_response(
    raw: &str,
    file_path: &str,
    method_name: &str,
    fqn: &str,
    content_hash: &str,
) -> MethodBusinessLogic {
    let purpose = PURPOSE_RE
        .captures(raw)
        .map(|c| c[1].trim().to_string())
        .unwrap_or_default();

    // Extract steps: numbered lines between STEPS: and the next section header
    let steps = extract_section_items(raw, "STEPS:", &STEPS_RE);
    // Extract rules: bulleted lines between RULES: and the next section header
    let rules = extract_section_items(raw, "RULES:", &RULES_RE);

    // Extract DATA/ERRORS/EFFECTS as section text (supports multi-line LLM output)
    let data_flow = extract_section_text(raw, "DATA:");
    let error_handling = extract_section_text(raw, "ERRORS:");
    let side_effects_detail = extract_section_text(raw, "EFFECTS:");

    // If parsing got nothing useful, store the raw text as purpose (truncated)
    let final_purpose = if purpose.is_empty() && !raw.trim().is_empty() {
        let truncated: String = raw.chars().take(300).collect();
        if raw.chars().count() > 300 {
            format!("{truncated}…")
        } else {
            truncated
        }
    } else {
        purpose
    };

    MethodBusinessLogic {
        file_path: file_path.to_string(),
        method_name: method_name.to_string(),
        fqn: fqn.to_string(),
        purpose: final_purpose,
        steps,
        business_rules: rules,
        data_flow,
        error_handling,
        side_effects_detail,
        content_hash: content_hash.to_string(),
        confidence: String::new(),
        validation_warnings: vec![],
    }
}

/// Extract items (numbered or bulleted) from a labeled section of the LLM response.
fn extract_section_items(raw: &str, section_label: &str, item_re: &Regex) -> Vec<String> {
    // Find section start
    let lower = raw.to_lowercase();
    let label_lower = section_label.to_lowercase();
    let Some(start) = lower.find(&label_lower) else {
        return vec![];
    };
    let section_start = start + section_label.len();

    // Find next section header (PURPOSE:, STEPS:, RULES:, DATA:, ERRORS:, EFFECTS:)
    let section_end = find_next_section(raw, section_start);
    let section = &raw[section_start..section_end];

    item_re
        .captures_iter(section)
        .map(|c| c[1].trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Find the start of the next labeled section after `offset`.
fn find_next_section(raw: &str, offset: usize) -> usize {
    static SECTION_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?im)^(?:PURPOSE|STEPS|RULES|DATA|ERRORS|EFFECTS):").expect("SECTION_RE")
    });
    let tail = &raw[offset..];
    SECTION_RE
        .find(tail)
        .map(|m| offset + m.start())
        .unwrap_or(raw.len())
}

/// Extract all text from a labeled section (e.g. DATA:, ERRORS:, EFFECTS:) until the next section.
/// Supports both single-line and multi-line content.
fn extract_section_text(raw: &str, section_label: &str) -> String {
    let lower = raw.to_lowercase();
    let label_lower = section_label.to_lowercase();
    let Some(start) = lower.find(&label_lower) else {
        return String::new();
    };
    let content_start = start + section_label.len();
    let section_end = find_next_section(raw, content_start);
    let text = raw[content_start..section_end].trim();
    // Strip any markdown formatting the LLM might add
    text.replace("**", "").replace("```", "").to_string()
}

// ── Deterministic Fallback ───────────────────────────────────────────────────

/// Generate a deterministic business logic summary from method metadata.
/// Used when no LLM is available.
pub fn deterministic_method_summary(
    file_path: &str,
    method: &MethodInfo,
    class_name: &str,
) -> MethodBusinessLogic {
    let kind_desc = match &method.method_kind {
        MethodKind::Lifecycle => "ASP.NET page lifecycle handler",
        MethodKind::ControlEvent => "UI control event handler",
        MethodKind::WebMethod => "AJAX-callable WebMethod",
        MethodKind::DataAccess => "data access method",
        MethodKind::Helper => "helper/utility method",
        MethodKind::Unknown => "method",
    };

    // Include Handles clause info for VB event handlers (e.g., "Handles btnSave.Click")
    let handles_info = if !method.handles_clause.is_empty() {
        format!(" [Handles {}]", method.handles_clause.join(", "))
    } else {
        String::new()
    };

    let purpose = if method.effects.is_empty() {
        format!(
            "{}{handles_info} (complexity: {})",
            capitalize_first(kind_desc),
            method.complexity_score
        )
    } else {
        format!(
            "{}{handles_info} with {} (complexity: {})",
            capitalize_first(kind_desc),
            method.effects.join(", "),
            method.complexity_score
        )
    };

    let steps = method
        .effects
        .iter()
        .map(|e| format!("Performs: {e}"))
        .collect();

    let data_flow = method
        .effects
        .iter()
        .filter(|e| {
            e.contains("SQL")
                || e.contains("Session")
                || e.contains("ViewState")
                || e.contains("Redirect")
        })
        .cloned()
        .collect::<Vec<_>>()
        .join("; ");

    let error_handling = if method
        .effects
        .iter()
        .any(|e| e.contains("Error") || e.contains("Try"))
    {
        "Has error handling".to_string()
    } else {
        "No explicit error handling detected".to_string()
    };

    let body_hash = method
        .body_preview
        .as_ref()
        .map(|b| ContentHash::compute(b.as_bytes()).0)
        .unwrap_or_default();

    MethodBusinessLogic {
        file_path: file_path.to_string(),
        method_name: method.name.clone(),
        fqn: format!("{class_name}.{}", method.name),
        purpose,
        steps,
        business_rules: vec![],
        data_flow,
        error_handling,
        side_effects_detail: method.effects.join(", "),
        content_hash: body_hash,
        confidence: String::new(),
        validation_warnings: vec![],
    }
}

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

// ── LLM-Powered Analysis ────────────────────────────────────────────────────

/// Analyze a single method's business logic using the LLM.
pub async fn analyze_method_logic(
    dreaming: &DreamingEngine,
    file_path: &str,
    method_name: &str,
    method_body: &str,
    class_name: &str,
    language: &str,
) -> MethodBusinessLogic {
    let body_hash = ContentHash::compute(method_body.as_bytes()).0;
    let fqn = format!("{class_name}.{method_name}");

    let lang_tag = if language == "vb" { "vb.net" } else { "csharp" };
    let lang_full = if language == "vb" { "VB.NET" } else { "C#" };

    let prompt = METHOD_ANALYSIS_PROMPT
        .replace("{language}", lang_full)
        .replace("{language_tag}", lang_tag)
        .replace("{class_name}", class_name)
        .replace("{method_name}", method_name)
        .replace("{method_body}", method_body);

    let raw = dreaming
        .generate_text(&prompt, 1024, Duration::from_secs(120))
        .await;

    if raw.is_empty() {
        tracing::warn!("LLM returned empty response for {fqn}");
        return MethodBusinessLogic {
            file_path: file_path.to_string(),
            method_name: method_name.to_string(),
            fqn,
            purpose: String::new(),
            steps: vec![],
            business_rules: vec![],
            data_flow: String::new(),
            error_handling: String::new(),
            side_effects_detail: String::new(),
            content_hash: body_hash,
            confidence: String::new(),
            validation_warnings: vec![],
        };
    }

    parse_llm_response(&raw, file_path, method_name, &fqn, &body_hash)
}

/// Detect the class name from file content.
pub fn detect_class_name(content: &str) -> String {
    static VB_CLASS_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?im)(?:Partial\s+)?(?:Public\s+)?Class\s+(\w+)").expect("VB_CLASS_RE")
    });
    static CS_CLASS_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)(?:public\s+)?(?:partial\s+)?class\s+(\w+)").expect("CS_CLASS_RE")
    });

    let is_vb = content.contains("End Sub") || content.contains("End Function");
    if is_vb {
        VB_CLASS_RE
            .captures(content)
            .map(|c| c[1].to_string())
            .unwrap_or_else(|| "UnknownClass".to_string())
    } else {
        CS_CLASS_RE
            .captures(content)
            .map(|c| c[1].to_string())
            .unwrap_or_else(|| "UnknownClass".to_string())
    }
}

/// Detect language from content.
pub fn detect_language(content: &str) -> &'static str {
    if content.contains("End Sub") || content.contains("End Function") {
        "vb"
    } else {
        "cs"
    }
}

/// Analyze all methods in a single file using the LLM.
pub async fn analyze_file_logic(
    dreaming: &DreamingEngine,
    file_path: &str,
    content: &str,
    cached_hashes: &HashMap<String, String>,
) -> (FileBusinessLogic, usize, usize) {
    let class_name = detect_class_name(content);
    let language = detect_language(content);

    // Extract method names and bodies
    let method_names = extract_method_names(content);
    let mut methods = Vec::new();
    let mut analyzed_count = 0usize;
    let mut skipped_count = 0usize;

    for name in &method_names {
        let body_opt = if language == "vb" {
            extract_vb_method_body(content, name)
        } else {
            extract_cs_method_body(content, name)
        };

        let Some((body, _start, _end, _lines)) = body_opt else {
            continue;
        };

        let body_hash = ContentHash::compute(body.as_bytes()).0;
        let fqn = format!("{class_name}.{name}");

        // Check cache
        if let Some(cached_hash) = cached_hashes.get(&fqn) {
            if *cached_hash == body_hash {
                skipped_count += 1;
                continue;
            }
        }

        let result =
            analyze_method_logic(dreaming, file_path, name, &body, &class_name, language).await;
        analyzed_count += 1;
        methods.push(result);
    }

    // Generate file-level purpose from method summaries
    let file_purpose = if methods.is_empty() {
        String::new()
    } else {
        let method_list: String = methods
            .iter()
            .map(|m| format!("- {}: {}", m.method_name, m.purpose))
            .collect::<Vec<_>>()
            .join("\n");

        let lang_full = if language == "vb" { "VB.NET" } else { "C#" };
        let prompt = FILE_PURPOSE_PROMPT
            .replace("{language}", lang_full)
            .replace("{method_list}", &method_list);

        dreaming
            .generate_text(&prompt, 128, Duration::from_secs(30))
            .await
    };

    let file_logic = FileBusinessLogic {
        file_path: file_path.to_string(),
        class_name,
        file_purpose,
        methods,
        analyzed_at: now_utc_string(),
    };

    (file_logic, analyzed_count, skipped_count)
}

/// Analyze all code-behind files in a project with caching.
pub async fn analyze_project_logic(
    dreaming: &DreamingEngine,
    project_id: &str,
    code_files: &[(&str, &str)],
    cached_hashes: &HashMap<String, String>,
    max_concurrent: usize,
) -> ProjectBusinessLogicReport {
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(max_concurrent));
    let dreaming = std::sync::Arc::new(dreaming.clone());
    let cached = std::sync::Arc::new(cached_hashes.clone());

    let mut handles = Vec::new();

    for &(path, content) in code_files {
        let sem = semaphore.clone();
        let dream = dreaming.clone();
        let cache = cached.clone();
        let path_owned = path.to_string();
        let content_owned = content.to_string();

        let handle = tokio::spawn(async move {
            let _permit = sem.acquire().await.expect("semaphore closed");
            analyze_file_logic(&dream, &path_owned, &content_owned, &cache).await
        });
        handles.push(handle);
    }

    let mut file_summaries = Vec::new();
    let mut total_analyzed = 0usize;
    let mut total_skipped = 0usize;
    let mut total_failures = 0usize;

    for handle in handles {
        match handle.await {
            Ok((file_logic, analyzed, skipped)) => {
                total_analyzed += analyzed;
                total_skipped += skipped;
                // Count failures: methods with empty purpose after LLM analysis
                total_failures += file_logic
                    .methods
                    .iter()
                    .filter(|m| m.purpose.is_empty())
                    .count();
                file_summaries.push(file_logic);
            }
            Err(e) => {
                tracing::warn!("Business logic analysis task failed: {e}");
                total_failures += 1;
            }
        }
    }

    ProjectBusinessLogicReport {
        project_id: project_id.to_string(),
        files_analyzed: file_summaries.len(),
        methods_analyzed: total_analyzed,
        methods_skipped_cached: total_skipped,
        llm_failures: total_failures,
        file_summaries,
    }
}

/// Render a `MethodBusinessLogic` as a markdown document suitable for DocStore storage.
pub fn render_method_as_doc(m: &MethodBusinessLogic) -> String {
    let mut md = String::with_capacity(1024);
    md.push_str(&format!("# {}\n\n", m.fqn));
    md.push_str(&format!("**Purpose**: {}\n\n", m.purpose));

    if !m.steps.is_empty() {
        md.push_str("## Steps\n");
        for (i, step) in m.steps.iter().enumerate() {
            md.push_str(&format!("{}. {step}\n", i + 1));
        }
        md.push('\n');
    }

    if !m.business_rules.is_empty() {
        md.push_str("## Business Rules\n");
        for rule in &m.business_rules {
            md.push_str(&format!("- {rule}\n"));
        }
        md.push('\n');
    }

    if !m.data_flow.is_empty() {
        md.push_str(&format!("## Data Flow\n{}\n\n", m.data_flow));
    }

    if !m.error_handling.is_empty() {
        md.push_str(&format!("## Error Handling\n{}\n\n", m.error_handling));
    }

    if !m.side_effects_detail.is_empty() {
        md.push_str(&format!("## Side Effects\n{}\n\n", m.side_effects_detail));
    }

    md
}

/// Render the full project report as compact markdown (for embedding in full migration report).
pub fn render_compact_markdown(report: &ProjectBusinessLogicReport) -> String {
    let mut md = String::with_capacity(32_000);
    md.push_str("## Business Logic Summary\n\n");
    md.push_str(&format!(
        "- **Files analyzed**: {}\n- **Methods analyzed**: {}\n- **Cached (skipped)**: {}\n- **LLM failures**: {}\n\n",
        report.files_analyzed,
        report.methods_analyzed,
        report.methods_skipped_cached,
        report.llm_failures
    ));

    for file in &report.file_summaries {
        if file.methods.is_empty() {
            continue;
        }
        md.push_str(&format!("### {} — {}\n", file.class_name, file.file_path));
        if !file.file_purpose.is_empty() {
            let safe_purpose = file.file_purpose.replace('*', r"\*");
            md.push_str(&format!("*{safe_purpose}*\n\n"));
        }
        // Use confidence column when any method has confidence data
        let has_confidence = file.methods.iter().any(|m| !m.confidence.is_empty());
        if has_confidence {
            md.push_str("| Method | Purpose | Key Rules | Confidence |\n|---|---|---|---|\n");
        } else {
            md.push_str("| Method | Purpose | Key Rules |\n|---|---|---|\n");
        }
        for m in &file.methods {
            let rules_summary = if m.business_rules.is_empty() {
                "—".to_string()
            } else {
                m.business_rules
                    .iter()
                    .take(2)
                    .map(|r| escape_pipe(r))
                    .collect::<Vec<_>>()
                    .join("; ")
            };
            if has_confidence {
                md.push_str(&format!(
                    "| {} | {} | {} | {} |\n",
                    escape_pipe(&m.method_name),
                    escape_pipe(&m.purpose),
                    rules_summary,
                    confidence_badge(&m.confidence),
                ));
            } else {
                md.push_str(&format!(
                    "| {} | {} | {} |\n",
                    escape_pipe(&m.method_name),
                    escape_pipe(&m.purpose),
                    rules_summary
                ));
            }
        }
        md.push('\n');
    }

    md
}

fn escape_pipe(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

fn now_utc_string() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = secs / 86400;
    let time_secs = secs % 86400;
    let h = time_secs / 3600;
    let m = (time_secs % 3600) / 60;
    let s = time_secs % 60;
    let (y, mo, d) = super::full_project_migration_service::epoch_days_to_date(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

// ── Helper: Extract Method Names ─────────────────────────────────────────────

static VB_METHOD_NAME_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)^\s*(?:(?:Public|Private|Protected|Friend)\s+)?(?:(?:Shared|Overrides|Overridable|MustOverride|NotOverridable|Overloads)\s+)*(?:Async\s+)?(?:Sub|Function)\s+(\w+)")
        .expect("VB_METHOD_NAME_RE")
});
static CS_METHOD_NAME_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)^\s*(?:(?:public|private|protected|internal)\s+)?(?:static\s+)?(?:override\s+)?(?:virtual\s+)?(?:async\s+)?(?:\w[\w.<>\[\],]*)\s+(\w+)\s*\(")
        .expect("CS_METHOD_NAME_RE")
});

/// Extract method names from file content.
fn extract_method_names(content: &str) -> Vec<String> {
    let is_vb = content.contains("End Sub") || content.contains("End Function");
    let re = if is_vb {
        &*VB_METHOD_NAME_RE
    } else {
        &*CS_METHOD_NAME_RE
    };

    let skip_keywords = [
        "if",
        "else",
        "for",
        "foreach",
        "while",
        "switch",
        "catch",
        "using",
        "lock",
        "return",
        "new",
        "class",
        "struct",
        "interface",
        "enum",
        "namespace",
        "get",
        "set",
        "var",
        "typeof",
    ];

    let mut seen = std::collections::HashSet::new();
    re.captures_iter(content)
        .map(|c| c[1].to_string())
        .filter(|name| !skip_keywords.contains(&name.as_str()))
        .filter(|name| seen.insert(name.clone()))
        .collect()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_llm_response_well_formed() {
        let raw = r#"PURPOSE: Loads customer data from the database and populates the grid control based on the current user's role.
STEPS:
1. Check if the page is a postback; if so, exit early
2. Read the current user's role from Session["UserRole"]
3. Query the Customers table filtered by region
4. Bind the results to grdCustomers DataGrid
5. Set lblStatus.Text to show the record count
RULES:
- If user is not authenticated, redirect to Login.aspx
- Admin users see all customers; regular users see only their region
- If no records found, show "No customers match your criteria" message
DATA: Reads from Customers table (CustomerID, Name, Region, Status), filtered by UserRegion from Session
ERRORS: On database exception, logs error and shows "Unable to load customer data" in lblError
EFFECTS: Writes to Session["LastViewedRegion"], updates grdCustomers.DataSource, sets lblStatus.Text"#;

        let result = parse_llm_response(
            raw,
            "CustomerList.aspx.vb",
            "Page_Load",
            "CustomerList.Page_Load",
            "abc123",
        );

        assert_eq!(result.method_name, "Page_Load");
        assert_eq!(result.fqn, "CustomerList.Page_Load");
        assert!(result.purpose.contains("Loads customer data"));
        assert_eq!(result.steps.len(), 5);
        assert!(result.steps[0].contains("postback"));
        assert_eq!(result.business_rules.len(), 3);
        assert!(result.business_rules[0].contains("authenticated"));
        assert!(result.data_flow.contains("Customers table"));
        assert!(result.error_handling.contains("database exception"));
        assert!(result.side_effects_detail.contains("Session"));
    }

    #[test]
    fn test_parse_malformed_response_raw_text() {
        let raw = "This method loads customer data and displays it in a grid.";
        let result = parse_llm_response(raw, "test.vb", "DoStuff", "Test.DoStuff", "hash1");

        // Should fall back to storing raw text as purpose
        assert!(result.purpose.contains("loads customer data"));
        assert!(result.steps.is_empty());
        assert!(result.business_rules.is_empty());
    }

    #[test]
    fn test_parse_partial_response_missing_sections() {
        let raw = r#"PURPOSE: Saves the form data to the database.
STEPS:
1. Validates input fields
2. Calls SaveCustomer stored procedure
EFFECTS: Writes to Customers table"#;

        let result =
            parse_llm_response(raw, "edit.vb", "btnSave_Click", "Edit.btnSave_Click", "h2");

        assert!(result.purpose.contains("Saves the form data"));
        assert_eq!(result.steps.len(), 2);
        assert!(result.business_rules.is_empty()); // RULES section missing
        assert!(result.data_flow.is_empty()); // DATA section missing
        assert!(result.error_handling.is_empty()); // ERRORS section missing
        assert!(result.side_effects_detail.contains("Customers table"));
    }

    #[test]
    fn test_deterministic_fallback_lifecycle_with_effects() {
        let method = MethodInfo {
            name: "Page_Load".to_string(),
            signature: "Protected Sub Page_Load(sender, e)".to_string(),
            return_type: "Sub".to_string(),
            access_level: "Protected".to_string(),
            line_range: (10, 40),
            line_count: 30,
            method_kind: MethodKind::Lifecycle,
            effects: vec![
                "SQL: SELECT Customers".to_string(),
                "Session write: UserRole".to_string(),
            ],
            calls_methods: vec![],
            called_by: vec![],
            body_preview: Some("Protected Sub Page_Load(...)\n  ...\nEnd Sub".to_string()),
            complexity_score: 8,
            handles_clause: vec![],
        };

        let result = deterministic_method_summary("CustomerList.aspx.vb", &method, "CustomerList");

        assert_eq!(result.fqn, "CustomerList.Page_Load");
        assert!(result.purpose.contains("ASP.NET page lifecycle handler"));
        assert!(result.purpose.contains("SQL: SELECT Customers"));
        assert!(result.purpose.contains("complexity: 8"));
        assert_eq!(result.steps.len(), 2);
        assert!(result.data_flow.contains("SQL: SELECT Customers"));
    }

    #[test]
    fn test_deterministic_fallback_no_effects() {
        let method = MethodInfo {
            name: "FormatDate".to_string(),
            signature: "Private Function FormatDate(d As Date) As String".to_string(),
            return_type: "String".to_string(),
            access_level: "Private".to_string(),
            line_range: (50, 55),
            line_count: 5,
            method_kind: MethodKind::Helper,
            effects: vec![],
            calls_methods: vec![],
            called_by: vec![],
            body_preview: Some("Private Function FormatDate(...)\nEnd Function".to_string()),
            complexity_score: 1,
            handles_clause: vec![],
        };

        let result = deterministic_method_summary("Utils.vb", &method, "Utils");

        assert!(result.purpose.contains("Helper/utility method"));
        assert!(result.purpose.contains("complexity: 1"));
        assert!(result.steps.is_empty());
    }

    #[test]
    fn test_content_hash_for_caching() {
        let body1 = "Protected Sub Page_Load()\n  lblTitle.Text = \"Hello\"\nEnd Sub";
        let body2 = "Protected Sub Page_Load()\n  lblTitle.Text = \"World\"\nEnd Sub";
        let body1_copy = "Protected Sub Page_Load()\n  lblTitle.Text = \"Hello\"\nEnd Sub";

        let hash1 = ContentHash::compute(body1.as_bytes()).0;
        let hash2 = ContentHash::compute(body2.as_bytes()).0;
        let hash1_copy = ContentHash::compute(body1_copy.as_bytes()).0;

        assert_ne!(
            hash1, hash2,
            "Different bodies should have different hashes"
        );
        assert_eq!(hash1, hash1_copy, "Same bodies should have same hash");
    }

    #[test]
    fn test_detect_class_name_vb() {
        let content = r#"
Imports System
Public Partial Class CustomerList
    Inherits System.Web.UI.Page
    Protected Sub Page_Load(sender As Object, e As EventArgs)
    End Sub
End Class"#;
        assert_eq!(detect_class_name(content), "CustomerList");
    }

    #[test]
    fn test_detect_class_name_cs() {
        let content = r#"
using System;
public partial class OrderEntry : System.Web.UI.Page
{
    protected void Page_Load(object sender, EventArgs e) { }
}"#;
        assert_eq!(detect_class_name(content), "OrderEntry");
    }

    #[test]
    fn test_detect_language() {
        assert_eq!(detect_language("Public Sub Page_Load()\nEnd Sub"), "vb");
        assert_eq!(
            detect_language("protected void Page_Load(object sender, EventArgs e) { }"),
            "cs"
        );
    }

    #[test]
    fn test_extract_method_names_vb() {
        let content = r#"
Public Class CustomerList
    Protected Sub Page_Load(sender As Object, e As EventArgs)
    End Sub
    Private Sub LoadGrid()
    End Sub
    Protected Sub btnSave_Click(sender As Object, e As EventArgs) Handles btnSave.Click
    End Sub
End Class"#;
        let names = extract_method_names(content);
        assert!(names.contains(&"Page_Load".to_string()));
        assert!(names.contains(&"LoadGrid".to_string()));
        assert!(names.contains(&"btnSave_Click".to_string()));
    }

    #[test]
    fn test_extract_method_names_cs() {
        let content = r#"
public class OrderEntry : Page
{
    protected void Page_Load(object sender, EventArgs e) { }
    private void BindGrid() { }
    public static string FormatCurrency(decimal amount) { }
}"#;
        let names = extract_method_names(content);
        assert!(names.contains(&"Page_Load".to_string()));
        assert!(names.contains(&"BindGrid".to_string()));
        assert!(names.contains(&"FormatCurrency".to_string()));
    }

    #[test]
    fn test_render_method_as_doc() {
        let m = MethodBusinessLogic {
            file_path: "CustomerList.aspx.vb".to_string(),
            method_name: "Page_Load".to_string(),
            fqn: "CustomerList.Page_Load".to_string(),
            purpose: "Loads and displays customer data".to_string(),
            steps: vec!["Check postback".to_string(), "Query database".to_string()],
            business_rules: vec!["Admins see all records".to_string()],
            data_flow: "Reads Customers table".to_string(),
            error_handling: "Shows error message on failure".to_string(),
            side_effects_detail: "Binds grid, updates Session".to_string(),
            content_hash: "abc".to_string(),
            confidence: String::new(),
            validation_warnings: vec![],
        };

        let doc = render_method_as_doc(&m);
        assert!(doc.contains("# CustomerList.Page_Load"));
        assert!(doc.contains("**Purpose**: Loads and displays customer data"));
        assert!(doc.contains("1. Check postback"));
        assert!(doc.contains("2. Query database"));
        assert!(doc.contains("- Admins see all records"));
        assert!(doc.contains("## Data Flow"));
        assert!(doc.contains("## Error Handling"));
        assert!(doc.contains("## Side Effects"));
    }

    #[test]
    fn test_render_compact_markdown() {
        let report = ProjectBusinessLogicReport {
            project_id: "test-project".to_string(),
            files_analyzed: 1,
            methods_analyzed: 2,
            methods_skipped_cached: 0,
            llm_failures: 0,
            file_summaries: vec![FileBusinessLogic {
                file_path: "Default.aspx.vb".to_string(),
                class_name: "_Default".to_string(),
                file_purpose: "Main landing page for the application".to_string(),
                methods: vec![MethodBusinessLogic {
                    file_path: "Default.aspx.vb".to_string(),
                    method_name: "Page_Load".to_string(),
                    fqn: "_Default.Page_Load".to_string(),
                    purpose: "Initializes the dashboard".to_string(),
                    steps: vec![],
                    business_rules: vec!["Auth required".to_string()],
                    data_flow: String::new(),
                    error_handling: String::new(),
                    side_effects_detail: String::new(),
                    content_hash: "h1".to_string(),
                    confidence: String::new(),
                    validation_warnings: vec![],
                }],
                analyzed_at: "2026-02-22T00:00:00Z".to_string(),
            }],
        };

        let md = render_compact_markdown(&report);
        assert!(md.contains("## Business Logic Summary"));
        assert!(md.contains("_Default"));
        assert!(md.contains("Initializes the dashboard"));
        assert!(md.contains("Auth required"));
    }

    #[test]
    fn test_parse_multiline_data_section() {
        let raw = r#"PURPOSE: Saves customer and order data.
STEPS:
1. Validate inputs
2. Save to database
DATA: Reads from Customers table (CustomerID, Name)
Writes to Orders table (OrderID, Amount, CustomerID)
Also updates AuditLog table
ERRORS: Shows error on failure
EFFECTS: Updates Customers, Orders, AuditLog tables"#;

        let result = parse_llm_response(raw, "test.vb", "Save", "Test.Save", "hash");
        // Multi-line DATA should be captured fully
        assert!(result.data_flow.contains("Customers table"));
        assert!(result.data_flow.contains("Orders table"));
        assert!(result.data_flow.contains("AuditLog"));
    }

    #[test]
    fn test_parse_purpose_truncation_indicator() {
        // A raw response with no recognizable sections and >300 chars
        let long_text = "A".repeat(400);
        let result = parse_llm_response(&long_text, "t.vb", "M", "T.M", "h");
        assert!(
            result.purpose.ends_with('…'),
            "Truncated purpose should end with ellipsis"
        );
        assert!(result.purpose.len() < 400);
    }

    #[test]
    fn test_extract_method_names_dedup() {
        // Simulates an overloaded method appearing twice in C# code
        let content = r#"
public class Foo : Page
{
    public void DoWork(int x) { }
    public void DoWork(string s) { }
    private void Other() { }
}"#;
        let names = extract_method_names(content);
        let do_work_count = names.iter().filter(|n| *n == "DoWork").count();
        assert_eq!(do_work_count, 1, "Duplicate method names should be deduped");
        assert!(names.contains(&"Other".to_string()));
    }

    #[test]
    fn test_deterministic_fallback_with_handles_clause() {
        let method = MethodInfo {
            name: "btnSave_Click".to_string(),
            signature: "Protected Sub btnSave_Click(sender, e) Handles btnSave.Click".to_string(),
            return_type: "Sub".to_string(),
            access_level: "Protected".to_string(),
            line_range: (10, 30),
            line_count: 20,
            method_kind: MethodKind::ControlEvent,
            effects: vec!["SQL: INSERT Orders".to_string()],
            calls_methods: vec![],
            called_by: vec![],
            body_preview: Some("Protected Sub btnSave_Click(...)".to_string()),
            complexity_score: 5,
            handles_clause: vec!["btnSave.Click".to_string()],
        };

        let result = deterministic_method_summary("Edit.aspx.vb", &method, "EditPage");
        assert!(
            result.purpose.contains("Handles btnSave.Click"),
            "Purpose should mention Handles clause: {}",
            result.purpose
        );
    }

    #[test]
    fn test_escape_pipe_handles_newlines() {
        let input = "Line one\nLine two | with pipe";
        let escaped = escape_pipe(input);
        assert!(!escaped.contains('\n'), "Newlines should be replaced");
        assert!(escaped.contains("\\|"), "Pipes should be escaped");
    }

    #[test]
    fn test_file_purpose_star_escaping() {
        let report = ProjectBusinessLogicReport {
            project_id: "test".to_string(),
            files_analyzed: 1,
            methods_analyzed: 1,
            methods_skipped_cached: 0,
            llm_failures: 0,
            file_summaries: vec![FileBusinessLogic {
                file_path: "page.vb".to_string(),
                class_name: "MyPage".to_string(),
                file_purpose: "Uses *asterisks* in purpose text".to_string(),
                methods: vec![MethodBusinessLogic {
                    file_path: "page.vb".to_string(),
                    method_name: "Load".to_string(),
                    fqn: "MyPage.Load".to_string(),
                    purpose: "Loads data".to_string(),
                    steps: vec![],
                    business_rules: vec![],
                    data_flow: String::new(),
                    error_handling: String::new(),
                    side_effects_detail: String::new(),
                    content_hash: "h".to_string(),
                    confidence: String::new(),
                    validation_warnings: vec![],
                }],
                analyzed_at: "2026-01-01T00:00:00Z".to_string(),
            }],
        };

        let md = render_compact_markdown(&report);
        // Asterisks in file_purpose should be escaped so they don't break italic formatting
        assert!(
            md.contains(r"\*"),
            "Asterisks in file_purpose should be escaped"
        );
    }

    // ── Phase 37: Validation Gate Tests ──────────────────────────────────

    #[test]
    fn validate_llm_perfect_agreement() {
        let llm = MethodBusinessLogic {
            file_path: "Page.aspx.vb".to_string(),
            method_name: "Load".to_string(),
            fqn: "Page.Load".to_string(),
            purpose: "Loads customer data from database".to_string(),
            steps: vec!["Query Customers table".to_string()],
            business_rules: vec![],
            data_flow: "Reads Customers table via SQL SELECT".to_string(),
            error_handling: String::new(),
            side_effects_detail: "Writes Session[\"UserRole\"]".to_string(),
            content_hash: "h1".to_string(),
            confidence: String::new(),
            validation_warnings: vec![],
        };
        let det = llm.clone();
        let effects = vec![
            "SQL: SELECT Customers".to_string(),
            "Session write: UserRole".to_string(),
        ];

        let result = validate_llm_output(&llm, &det, &effects);
        assert_eq!(result.confidence, Confidence::High);
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn validate_llm_misses_sql_effect() {
        let llm = MethodBusinessLogic {
            file_path: "Page.aspx.vb".to_string(),
            method_name: "Load".to_string(),
            fqn: "Page.Load".to_string(),
            purpose: "Initializes the page".to_string(),
            steps: vec![],
            business_rules: vec![],
            data_flow: String::new(), // LLM missed the SQL
            error_handling: String::new(),
            side_effects_detail: String::new(),
            content_hash: "h1".to_string(),
            confidence: String::new(),
            validation_warnings: vec![],
        };
        let det = llm.clone();
        let effects = vec!["SQL: SELECT Customers".to_string()];

        let result = validate_llm_output(&llm, &det, &effects);
        assert_eq!(result.confidence, Confidence::Medium);
        assert!(result.warnings[0].contains("missed database access"));
    }

    #[test]
    fn validate_llm_misses_session_and_redirect() {
        let llm = MethodBusinessLogic {
            file_path: "Page.aspx.vb".to_string(),
            method_name: "Load".to_string(),
            fqn: "Page.Load".to_string(),
            purpose: "Does something".to_string(),
            steps: vec![],
            business_rules: vec![],
            data_flow: String::new(),
            error_handling: String::new(),
            side_effects_detail: String::new(),
            content_hash: "h1".to_string(),
            confidence: String::new(),
            validation_warnings: vec![],
        };
        let det = llm.clone();
        let effects = vec![
            "SQL: SELECT Orders".to_string(),
            "Session write: CartID".to_string(),
            "Redirect: Checkout.aspx".to_string(),
        ];

        let result = validate_llm_output(&llm, &det, &effects);
        assert_eq!(result.confidence, Confidence::Low);
        assert!(result.warnings.len() >= 3);
    }

    #[test]
    fn validate_llm_mentions_unknown_table() {
        let llm = MethodBusinessLogic {
            file_path: "Page.aspx.vb".to_string(),
            method_name: "Save".to_string(),
            fqn: "Page.Save".to_string(),
            purpose: "Saves data".to_string(),
            steps: vec![],
            business_rules: vec![],
            data_flow: "Reads FROM UnknownTable, writes INTO AnotherTable".to_string(),
            error_handling: String::new(),
            side_effects_detail: String::new(),
            content_hash: "h1".to_string(),
            confidence: String::new(),
            validation_warnings: vec![],
        };
        // Deterministic version only knows about Customers — LLM hallucinated the other tables
        let det = MethodBusinessLogic {
            file_path: "Page.aspx.vb".to_string(),
            method_name: "Save".to_string(),
            fqn: "Page.Save".to_string(),
            purpose: "Saves data".to_string(),
            steps: vec![],
            business_rules: vec![],
            data_flow: "SQL: SELECT Customers".to_string(),
            error_handling: String::new(),
            side_effects_detail: String::new(),
            content_hash: "h1".to_string(),
            confidence: String::new(),
            validation_warnings: vec![],
        };
        let effects = vec!["SQL: SELECT Customers".to_string()];

        let result = validate_llm_output(&llm, &det, &effects);
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("not found in static analysis")),
            "Should flag tables not found in deterministic analysis: {:?}",
            result.warnings
        );
    }

    #[test]
    fn validate_empty_llm_no_crash() {
        let llm = MethodBusinessLogic {
            file_path: "t.vb".to_string(),
            method_name: "M".to_string(),
            fqn: "T.M".to_string(),
            purpose: String::new(),
            steps: vec![],
            business_rules: vec![],
            data_flow: String::new(),
            error_handling: String::new(),
            side_effects_detail: String::new(),
            content_hash: "h".to_string(),
            confidence: String::new(),
            validation_warnings: vec![],
        };
        let det = llm.clone();
        let result = validate_llm_output(&llm, &det, &[]);
        assert_eq!(result.confidence, Confidence::High);
    }

    #[test]
    fn confidence_badge_renders_in_compact_markdown() {
        let report = ProjectBusinessLogicReport {
            project_id: "test".to_string(),
            files_analyzed: 1,
            methods_analyzed: 1,
            methods_skipped_cached: 0,
            llm_failures: 0,
            file_summaries: vec![FileBusinessLogic {
                file_path: "page.vb".to_string(),
                class_name: "MyPage".to_string(),
                file_purpose: "Test page".to_string(),
                methods: vec![MethodBusinessLogic {
                    file_path: "page.vb".to_string(),
                    method_name: "Load".to_string(),
                    fqn: "MyPage.Load".to_string(),
                    purpose: "Loads data".to_string(),
                    steps: vec![],
                    business_rules: vec![],
                    data_flow: String::new(),
                    error_handling: String::new(),
                    side_effects_detail: String::new(),
                    content_hash: "h".to_string(),
                    confidence: "High".to_string(),
                    validation_warnings: vec![],
                }],
                analyzed_at: "2026-01-01T00:00:00Z".to_string(),
            }],
        };

        let md = render_compact_markdown(&report);
        assert!(
            md.contains("Confidence"),
            "Should have confidence column header"
        );
        assert!(md.contains("High"), "Should show confidence badge");
    }
}
