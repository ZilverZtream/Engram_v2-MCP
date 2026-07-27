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
use engram_ml::llm_provider::LlmError;
use regex::Regex;
use serde::{Deserialize, Serialize};

use super::full_project_migration_service::{
    MethodInfo, MethodKind, extract_cs_method_body, extract_ml_method_body, extract_vb_method_body,
};

// ── Prompt Template ──────────────────────────────────────────────────────────

const METHOD_ANALYSIS_PROMPT: &str = r#"You are a business analyst reverse-engineering a legacy {language} WebForms application. Extract the TESTABLE business rules from this method. A developer must be able to re-implement each rule in a new stack without reading the original code.

File: {file_path}
Class: {class_name}
Method: {method_name}

Method body (with real source line numbers):
```{language_tag}
{method_body}
```

Respond with STRICT JSON only (no markdown, no prose, no backticks).
Use exactly these keys and keep them stable:
{
  "purpose": "<one sentence: what this method does for the business>",
  "steps": ["<first action>", "<next action>"],
  "business_rules": [
    {
      "when": "<the exact triggering condition, quoting the real field/control/column>",
      "then": "<the exact consequence>",
      "source_line": <line number where the condition is checked>,
      "refs": ["<DB table.column, Session key, control ID, or config key involved>"]
    }
  ],
  "data_flow": "<what data it reads/writes - table and field names>",
  "error_handling": "<error handling behavior>",
  "side_effects_detail": "<state changes: DB writes, session, UI, redirects>"
}

Rules for business_rules:
- Each entry must be a testable WHEN/THEN pair anchored to a source_line from the numbered body above.
- Quote concrete artifacts in refs: database columns (Orders.Total), session keys (Session("CartID")), control IDs (btnSave, txtQty), stored procedures, config keys.
- Validation checks, permission/role gates, visibility toggles, price/date/limit calculations, and status transitions are business rules. Null checks and logging are not.
- If the method contains no business rules, return "business_rules": [] — never invent one.
- Use "" for any other field that does not apply — never guess.

Example entry (from a different method):
{"when": "Session(\"UserRole\") <> \"Admin\" And chkShowAll.Checked", "then": "results are filtered to CustomerId = Session(\"CustomerId\") before binding gvOrders", "source_line": 214, "refs": ["Session(\"UserRole\")", "Customers.CustomerId", "gvOrders"]}

Do not include any keys other than the six keys above."#;

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
    /// Diagnostic text stored when strict JSON parsing fails.
    #[serde(default)]
    pub parse_diagnostic: String,
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

#[derive(Debug, Default, Deserialize)]
struct LlmMethodAnalysis {
    #[serde(default)]
    purpose: String,
    #[serde(default)]
    steps: Vec<String>,
    #[serde(default)]
    business_rules: Vec<RuleEntry>,
    #[serde(default)]
    data_flow: String,
    #[serde(default)]
    error_handling: String,
    #[serde(default)]
    side_effects_detail: String,
}

/// A business rule as returned by the LLM. The current prompt asks for
/// anchored WHEN/THEN objects; older prompts (and weaker models) return
/// plain strings — accept both so a schema drift never zeroes out rules.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RuleEntry {
    Structured {
        #[serde(default)]
        when: String,
        #[serde(default)]
        then: String,
        #[serde(default)]
        source_line: Option<u32>,
        #[serde(default)]
        refs: Vec<String>,
        // Some models emit a free-text "rule" alongside/instead.
        #[serde(default)]
        rule: String,
    },
    Plain(String),
}

impl RuleEntry {
    /// Flatten to the rich one-line form consumers store and search:
    /// `IF <when> THEN <then> [line N] {refs: a, b}`.
    fn to_display(&self) -> String {
        match self {
            RuleEntry::Plain(s) => s.trim().to_string(),
            RuleEntry::Structured {
                when,
                then,
                source_line,
                refs,
                rule,
            } => {
                let mut out = if !when.trim().is_empty() || !then.trim().is_empty() {
                    format!("IF {} THEN {}", when.trim(), then.trim())
                } else {
                    rule.trim().to_string()
                };
                if let Some(line) = source_line {
                    out.push_str(&format!(" [line {line}]"));
                }
                if !refs.is_empty() {
                    out.push_str(&format!(" {{refs: {}}}", refs.join(", ")));
                }
                out
            }
        }
    }
}

/// Cut the model's response down to the JSON object it was asked for.
/// Reasoning models wrap output in `<think>…</think>`; many models add
/// ```json fences or prose around the object. Any of those made
/// `serde_json::from_str` fail and silently DISCARDED every extracted
/// rule (falling back to a one-sentence summary) — so be liberal here.
fn extract_json_object(raw: &str) -> Option<&str> {
    let mut s = raw;
    if let Some(end) = s.find("</think>") {
        s = &s[end + "</think>".len()..];
    }
    let start = s.find('{')?;
    let end = s.rfind('}')?;
    (end > start).then(|| &s[start..=end])
}

/// Parse a structured LLM response into a `MethodBusinessLogic`.
pub fn parse_llm_response(
    raw: &str,
    file_path: &str,
    method_name: &str,
    fqn: &str,
    content_hash: &str,
) -> MethodBusinessLogic {
    let candidate = extract_json_object(raw).unwrap_or(raw);
    let (parsed, parse_diagnostic) = match serde_json::from_str::<LlmMethodAnalysis>(candidate) {
        Ok(parsed) => (parsed, String::new()),
        Err(_) => (
            deterministic_summary_from_raw(raw),
            truncate_for_diagnostic(raw, 1000),
        ),
    };

    let business_rules: Vec<String> = parsed
        .business_rules
        .iter()
        .map(RuleEntry::to_display)
        .filter(|r| !r.is_empty())
        .collect();

    MethodBusinessLogic {
        file_path: file_path.to_string(),
        method_name: method_name.to_string(),
        fqn: fqn.to_string(),
        purpose: parsed.purpose,
        steps: parsed.steps,
        business_rules,
        data_flow: parsed.data_flow,
        error_handling: parsed.error_handling,
        side_effects_detail: parsed.side_effects_detail,
        content_hash: content_hash.to_string(),
        confidence: String::new(),
        validation_warnings: vec![],
        parse_diagnostic,
    }
}

fn truncate_for_diagnostic(raw: &str, max_len: usize) -> String {
    let mut truncated: String = raw.chars().take(max_len).collect();
    if raw.chars().count() > max_len {
        truncated.push('…');
    }
    truncated
}

fn deterministic_summary_from_raw(raw: &str) -> LlmMethodAnalysis {
    let cleaned = raw.replace(['\n', '\r'], " ");
    let normalized = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let purpose = if normalized.is_empty() {
        String::new()
    } else {
        normalized
            .split_terminator(['.', '!', '?'])
            .find_map(|sentence| {
                let s = sentence.trim();
                (!s.is_empty()).then(|| truncate_for_diagnostic(s, 220))
            })
            .unwrap_or_else(|| truncate_for_diagnostic(&normalized, 220))
    };

    LlmMethodAnalysis {
        purpose,
        ..LlmMethodAnalysis::default()
    }
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
        parse_diagnostic: String::new(),
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
///
/// `start_line` is the 1-based line of the method's first body line in the
/// source file; the body is sent to the model with REAL line numbers so the
/// extracted rules carry usable `file:line` anchors.
pub async fn analyze_method_logic(
    dreaming: &DreamingEngine,
    file_path: &str,
    method_name: &str,
    method_body: &str,
    class_name: &str,
    language: &str,
    start_line: u32,
) -> MethodBusinessLogic {
    let body_hash = ContentHash::compute(method_body.as_bytes()).0;
    let fqn = format!("{class_name}.{method_name}");

    let (lang_tag, lang_full) = match language {
        "vb" => ("vb.net", "VB.NET"),
        "ml" => ("minilang", "MiniLang"),
        _ => ("csharp", "C#"),
    };

    let numbered_body: String = method_body
        .lines()
        .enumerate()
        .map(|(i, l)| format!("{}: {l}", start_line.max(1) as usize + i))
        .collect::<Vec<_>>()
        .join("\n");

    let prompt = METHOD_ANALYSIS_PROMPT
        .replace("{language}", lang_full)
        .replace("{language_tag}", lang_tag)
        .replace("{file_path}", file_path)
        .replace("{class_name}", class_name)
        .replace("{method_name}", method_name)
        .replace("{method_body}", &numbered_body);

    // 3072 tokens: the old 1024 ceiling silently truncated the JSON on any
    // sizeable Page_Load, which failed the strict parse and threw away every
    // extracted rule.
    let raw = match dreaming
        .generate_text(&prompt, 3072, Duration::from_secs(120))
        .await
    {
        Ok(raw) => raw,
        Err(err) => {
            log_llm_failure("business_logic.method_analysis", &fqn, &err);
            String::new()
        }
    };

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
            parse_diagnostic: String::new(),
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

/// Detect language: file EXTENSION first (authoritative), content heuristic only
/// as a fallback for extensionless input. The old content-only sniff classified
/// any file without `End Sub`/`End Function` as C# — so a VB file made only of
/// properties / module-level / `ReadOnly Property … End Property` was treated as
/// C#, skipping VB body extraction and telling the LLM the wrong language. VB is
/// the PRIMARY language here, so that was a correctness bug.
///
/// `.ml`/`.mlinc` (MiniLang) is checked before `.vb`: without this branch, a
/// `.ml` file falls through to the content sniff, which also finds
/// `End Function` (MiniLang shares that terminator with VB) and misclassifies
/// it as `"vb"` — the exact defect this branch fixes.
pub fn detect_language(file_path: &str, content: &str) -> &'static str {
    let p = file_path.to_ascii_lowercase();
    if p.ends_with(".ml") || p.ends_with(".mlinc") {
        return "ml";
    }
    if p.ends_with(".vb") {
        // covers .vb, .aspx.vb, .ascx.vb, .designer.vb
        return "vb";
    }
    if p.ends_with(".cs") {
        return "cs";
    }
    // Fallback for extensionless / unknown input.
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
    let language = detect_language(file_path, content);

    // Extract method names and bodies
    let method_names = extract_method_names_for_language(content, language);
    let mut methods = Vec::new();
    let mut analyzed_count = 0usize;
    let mut skipped_count = 0usize;

    for name in &method_names {
        let body_opt = match language {
            "ml" => extract_ml_method_body(content, name),
            "vb" => extract_vb_method_body(content, name),
            _ => extract_cs_method_body(content, name),
        };

        let Some((body, start, _end, _lines)) = body_opt else {
            continue;
        };

        let body_hash = ContentHash::compute(body.as_bytes()).0;
        let fqn = format!("{class_name}.{name}");

        // Check cache
        if let Some(cached_hash) = cached_hashes.get(&fqn)
            && *cached_hash == body_hash
        {
            skipped_count += 1;
            continue;
        }

        let result = analyze_method_logic(
            dreaming,
            file_path,
            name,
            &body,
            &class_name,
            language,
            start as u32,
        )
        .await;
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

        let lang_full = match language {
            "vb" => "VB.NET",
            "ml" => "MiniLang",
            _ => "C#",
        };
        let prompt = FILE_PURPOSE_PROMPT
            .replace("{language}", lang_full)
            .replace("{method_list}", &method_list);

        match dreaming
            .generate_text(&prompt, 128, Duration::from_secs(30))
            .await
        {
            Ok(text) => text,
            Err(err) => {
                log_llm_failure("business_logic.file_purpose", file_path, &err);
                String::new()
            }
        }
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

fn log_llm_failure(operation: &str, target: &str, err: &LlmError) {
    tracing::warn!(
        operation = operation,
        target = target,
        provider = err.provider().unwrap_or("unknown"),
        status_code = err.status_code(),
        retry_exhausted = err.retry_exhausted(),
        error = %err,
        "LLM generation failed; using fallback"
    );
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
            let Ok(_permit) = sem.acquire().await else {
                // Semaphore was closed (can occur during shutdown); skip this file.
                return (
                    FileBusinessLogic {
                        file_path: path_owned,
                        class_name: String::new(),
                        file_purpose: String::new(),
                        methods: Vec::new(),
                        analyzed_at: String::new(),
                    },
                    0usize,
                    1usize,
                );
            };
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
// MiniLang declarations. Access modifiers are optional, and the name may be
// followed by an ` Of …` generic clause instead of an immediate `(` —
// demanding a paren would miss every generic declaration in the stdlib.
// Anchoring on Function/Sub as the first significant token keeps type
// annotations such as `Mapper As Function(T) As R` from matching.
static ML_METHOD_NAME_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)^\s*(?:(?:Public|Private)\s+)?(?:Function|Sub)\s+(\w+)")
        .expect("ML_METHOD_NAME_RE")
});

/// Extract method names from file content, guessing the language from
/// content alone. Kept for callers that don't have a file path handy; when
/// the language is already known (e.g. from `detect_language`), prefer
/// `extract_method_names_for_language` — MiniLang and VB cannot be told
/// apart by content alone, since both use `End Sub`/`End Function`.
fn extract_method_names(content: &str) -> Vec<String> {
    let is_vb = content.contains("End Sub") || content.contains("End Function");
    extract_method_names_for_language(content, if is_vb { "vb" } else { "cs" })
}

/// Language-explicit method-name extraction. Split out from
/// `extract_method_names` so MiniLang callers (which cannot be told apart
/// from VB by content alone — both use `End Function`) can select the right
/// pattern from the file extension instead.
pub(crate) fn extract_method_names_for_language(content: &str, language: &str) -> Vec<String> {
    let re = match language {
        "ml" => &*ML_METHOD_NAME_RE,
        "vb" => &*VB_METHOD_NAME_RE,
        _ => &*CS_METHOD_NAME_RE,
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
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_llm_response_valid_json() {
        let raw = r#"{
  "purpose": "Loads customer data from the database and populates the grid control.",
  "steps": [
    "Check if the page is a postback; if so, exit early",
    "Read the current user's role from Session[\"UserRole\"]",
    "Query the Customers table filtered by region"
  ],
  "business_rules": [
    "If user is not authenticated, redirect to Login.aspx",
    "Admin users see all customers; regular users see only their region"
  ],
  "data_flow": "Reads from Customers table and Session values.",
  "error_handling": "On database exception, log error and show user-friendly message.",
  "side_effects_detail": "Writes Session[\"LastViewedRegion\"], updates grid datasource."
}"#;

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
        assert_eq!(result.steps.len(), 3);
        assert_eq!(result.business_rules.len(), 2);
        assert!(result.data_flow.contains("Customers table"));
        assert!(result.error_handling.contains("database exception"));
        assert!(result.side_effects_detail.contains("Session"));
        assert!(result.parse_diagnostic.is_empty());
    }

    #[test]
    fn test_parse_llm_response_missing_optional_fields() {
        let raw = r#"{
  "purpose": "Saves the form data to the database.",
  "steps": ["Validates input fields", "Calls SaveCustomer stored procedure"],
  "side_effects_detail": "Writes to Customers table"
}"#;

        let result =
            parse_llm_response(raw, "edit.vb", "btnSave_Click", "Edit.btnSave_Click", "h2");

        assert!(result.purpose.contains("Saves the form data"));
        assert_eq!(result.steps.len(), 2);
        assert!(result.business_rules.is_empty());
        assert!(result.data_flow.is_empty());
        assert!(result.error_handling.is_empty());
        assert!(result.side_effects_detail.contains("Customers table"));
        assert!(result.parse_diagnostic.is_empty());
    }

    #[test]
    fn test_parse_llm_response_malformed_json_fallback() {
        let raw = r#"{"purpose": "This method loads customer data","steps":["#;
        let result = parse_llm_response(raw, "test.vb", "DoStuff", "Test.DoStuff", "hash1");

        assert!(result.purpose.contains("This method loads customer data"));
        assert!(result.steps.is_empty());
        assert!(result.business_rules.is_empty());
        assert!(result.data_flow.is_empty());
        assert_eq!(result.parse_diagnostic, raw);
    }

    #[test]
    fn test_parse_llm_response_structured_rules_render_anchored() {
        let raw = r#"{
  "purpose": "Filters the order grid by the caller's role.",
  "steps": ["Read role", "Bind grid"],
  "business_rules": [
    {"when": "Session(\"UserRole\") <> \"Admin\"",
     "then": "grid is filtered to CustomerId = Session(\"CustomerId\")",
     "source_line": 214,
     "refs": ["Session(\"UserRole\")", "Customers.CustomerId", "gvOrders"]},
    "Plain legacy-style rule survives too"
  ],
  "data_flow": "Reads Customers",
  "error_handling": "",
  "side_effects_detail": ""
}"#;
        let result = parse_llm_response(raw, "Orders.aspx.vb", "BindGrid", "Orders.BindGrid", "h");
        assert_eq!(
            result.business_rules.len(),
            2,
            "{:?}",
            result.business_rules
        );
        let anchored = &result.business_rules[0];
        assert!(anchored.starts_with("IF "), "{anchored}");
        assert!(anchored.contains("THEN"), "{anchored}");
        assert!(anchored.contains("[line 214]"), "{anchored}");
        assert!(anchored.contains("Customers.CustomerId"), "{anchored}");
        assert_eq!(
            result.business_rules[1],
            "Plain legacy-style rule survives too"
        );
        assert!(result.parse_diagnostic.is_empty());
    }

    #[test]
    fn test_parse_llm_response_strips_think_and_fences() {
        // Reasoning models (deepseek etc.) wrap output; previously this
        // failed strict parsing and silently discarded every rule.
        let raw = "<think>Let me analyze the method...\n{not the answer}\n</think>\n```json\n{\"purpose\": \"Validates the coupon code.\", \"business_rules\": [\"If coupon expired, reject checkout\"]}\n```";
        let result = parse_llm_response(raw, "c.vb", "Validate", "C.Validate", "h");
        assert_eq!(result.purpose, "Validates the coupon code.");
        assert_eq!(result.business_rules.len(), 1);
        assert!(
            result.parse_diagnostic.is_empty(),
            "{}",
            result.parse_diagnostic
        );
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
        // Extension is authoritative.
        assert_eq!(detect_language("Foo.vb", ""), "vb");
        assert_eq!(detect_language("modules/x.aspx.vb", ""), "vb");
        assert_eq!(detect_language("Services/Bar.cs", ""), "cs");
        // A VB file with NO Sub/Function (property/module only) must still be vb
        // by extension — the old content sniff returned cs here (the bug).
        assert_eq!(
            detect_language("Settings.vb", "Public ReadOnly Property Foo As String"),
            "vb"
        );
        // Content fallback only when extensionless.
        assert_eq!(detect_language("", "Public Sub Page_Load()\nEnd Sub"), "vb");
        assert_eq!(
            detect_language(
                "",
                "protected void Page_Load(object sender, EventArgs e) { }"
            ),
            "cs"
        );
    }

    #[test]
    fn detect_language_recognises_minilang() {
        assert_eq!(detect_language("Std.Collections.List.ml", ""), "ml");
        assert_eq!(detect_language("shared.mlinc", ""), "ml");
        // VB and C# are unaffected.
        assert_eq!(detect_language("Form1.vb", ""), "vb");
        assert_eq!(detect_language("Program.cs", ""), "cs");
    }

    #[test]
    fn extract_method_names_finds_minilang_declarations() {
        let src = "\
Namespace Std
    Function BTreeMap_Get Of K, V(tree As Int, key As K) As V
        Return key
    End Function
    Public Sub Install(target As Int)
        Say target
    End Sub
End Namespace
Type Cursor Of T, R
    Mapper As Function(T) As R
End Type
";
        let names = extract_method_names_for_language(src, "ml");
        assert!(names.contains(&"BTreeMap_Get".to_string()), "got {names:?}");
        assert!(names.contains(&"Install".to_string()), "got {names:?}");
        assert!(
            !names.contains(&"Mapper".to_string()),
            "a field of function type must not be a method name, got {names:?}"
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
            parse_diagnostic: String::new(),
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
                    parse_diagnostic: String::new(),
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
                    parse_diagnostic: String::new(),
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
            parse_diagnostic: String::new(),
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
            parse_diagnostic: String::new(),
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
            parse_diagnostic: String::new(),
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
            parse_diagnostic: String::new(),
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
            parse_diagnostic: String::new(),
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
            parse_diagnostic: String::new(),
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
                    parse_diagnostic: String::new(),
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
