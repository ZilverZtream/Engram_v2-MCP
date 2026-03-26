use crate::llm_provider::{
    LlmError, LlmGenerateOptions, LlmProvider, OllamaProvider, OpenAiCompatibleProvider,
    OpenRouterProvider,
};
use regex::Regex;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;

fn get_compiled_regex<'a>(
    lock: &'a OnceLock<Regex>,
    pattern: &str,
    label: &str,
) -> Option<&'a Regex> {
    if let Some(re) = lock.get() {
        return Some(re);
    }
    match Regex::new(pattern) {
        Ok(re) => Some(lock.get_or_init(|| re)),
        Err(err) => {
            tracing::error!("failed to compile {label} regex: {err}");
            None
        }
    }
}
use std::time::Duration;
use tokio::time::timeout;

// ---------------------------------------------------------------------------
// Prompt templates (ported from v1 generation.py)
// ---------------------------------------------------------------------------

const INSIGHT_PROMPT: &str = r#"You are an insight engine that connects two related contexts.

Context A:
{context_a}

Context B:
{context_b}

Task:
Write a single, concise insight that links the two contexts. Be specific and avoid repetition.
If the snippets are unrelated or the connection is trivial, respond with ONLY "NO_INSIGHT".

Insight:
"#;

const CLUSTER_SUMMARY_PROMPT: &str = r#"You are a code insight engine. You have been given several related code snippets that appear together frequently in searches.

Code snippets:
{snippets}

Task:
1. Write a short title (one line, max 80 chars) summarising what links these snippets.
2. Write a brief markdown explanation (3-5 bullet points) of WHY these areas are related and what an engineer should know.
3. List up to 5 key terms / identifiers.

Respond in this exact format:
TITLE: <title>
SUMMARY:
<markdown bullets>
TERMS: <comma-separated terms>
"#;

const MAX_SNIPPETS_FOR_CLUSTER_SUMMARY: usize = 24;
const MAX_SNIPPET_CHARS: usize = 8_000;
const MAX_CONTEXT_BYTES_FOR_INSIGHT: usize = 16_000;

const MIGRATION_BOUNDARY_PROMPT: &str = r#"You are a software architect analyzing a legacy monolithic ASP.NET WebForms application for modernization.

You have been given clusters of files that frequently change together (temporal couplings) along with their shared state accesses and database dependencies.

Clusters:
{clusters}

Shared State Keys (Session/ViewState/Application used across clusters):
{shared_state}

Shared Database Tables:
{shared_tables}

Task:
1. Identify 2-5 potential bounded contexts (microservice candidates) from these clusters.
2. For each bounded context, specify:
   - A descriptive name
   - The key files that belong to it
   - The data it owns (tables, state keys)
   - Dependencies on other bounded contexts
   - Migration risk (LOW/MEDIUM/HIGH) based on shared state and coupling
3. Identify any "seam" files that sit between contexts and will need refactoring.

Respond in this exact format (repeat for each context, separated by ---):
CONTEXT: <name>
FILES: <comma-separated file paths>
DATA: <comma-separated tables and state keys>
DEPENDS_ON: <comma-separated context names, or NONE>
RISK: <LOW|MEDIUM|HIGH>
SEAM_FILES: <comma-separated files at boundaries, or NONE>
---
"#;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DreamInsight {
    pub title: String,
    pub summary_markdown: String,
    pub key_terms: Vec<String>,
}

/// Proposed microservice / bounded-context boundary from the Architecture Mimicry pipeline.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MigrationBoundary {
    pub context_name: String,
    pub files: Vec<String>,
    pub owned_data: Vec<String>,
    pub depends_on: Vec<String>,
    pub risk: String,
    pub seam_files: Vec<String>,
    /// Other context names that share state or data tables with this boundary.
    /// Populated by cross-cluster dependency analysis.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub shared_across: Vec<String>,
}

// ---------------------------------------------------------------------------
// LLM backend config (mirrors embed.rs pattern)
// ---------------------------------------------------------------------------

/// Which LLM backend to use for text generation.
#[derive(Clone, Default)]
pub struct LlmBackend {
    provider: Option<Arc<dyn LlmProvider>>,
}

impl LlmBackend {
    fn resolve_openai_headers(cfg: &engram_core::Config, mut headers: HeaderMap) -> HeaderMap {
        if let Some(referer) = &cfg.llm_http_referer
            && let Ok(value) = HeaderValue::from_str(referer)
        {
            headers.insert(HeaderName::from_static("http-referer"), value);
        }

        if let Some(title) = &cfg.llm_x_title
            && let Ok(value) = HeaderValue::from_str(title)
        {
            headers.insert(HeaderName::from_static("x-title"), value);
        }

        if let Some(extra) = &cfg.llm_extra_headers {
            for (key, value) in extra {
                if let (Ok(name), Ok(value)) = (
                    HeaderName::from_bytes(key.as_bytes()),
                    HeaderValue::from_str(value),
                ) {
                    headers.insert(name, value);
                }
            }
        }

        headers
    }

    /// Build an `LlmBackend` from the project `Config`.
    ///
    /// Returns `Err` for any unrecognised backend/provider string so that
    /// mis-configurations are caught eagerly rather than silently degrading to
    /// no-LLM mode.  Use `"none"` or leave the field empty to explicitly
    /// disable the LLM.
    pub fn from_config(cfg: &engram_core::Config) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        let backend = cfg
            .llm_provider
            .as_deref()
            .unwrap_or(cfg.llm_backend.as_str());

        match backend {
            "ollama" => {
                let url = cfg
                    .llm_ollama_url
                    .clone()
                    .or_else(|| cfg.ollama_url.clone())
                    .unwrap_or_else(|| "http://localhost:11434".into());
                let model = cfg.llm_model.clone().unwrap_or_else(|| "llama3.2".into());
                Ok(Self {
                    provider: Some(Arc::new(OllamaProvider::new(client, url, model))),
                })
            }
            "openai" => {
                let api_key = cfg
                    .llm_openai_api_key
                    .clone()
                    .or_else(|| cfg.openai_api_key.clone())
                    .unwrap_or_default();
                let api_base = cfg
                    .llm_openai_api_base
                    .clone()
                    .or_else(|| cfg.openai_api_base.clone())
                    .unwrap_or_else(|| "https://api.openai.com/v1".into());
                let model = cfg
                    .llm_model
                    .clone()
                    .unwrap_or_else(|| "gpt-4o-mini".into());
                let headers = Self::resolve_openai_headers(cfg, HeaderMap::new());
                Ok(Self {
                    provider: Some(Arc::new(
                        OpenAiCompatibleProvider::new(client, api_key, api_base, model)
                            .with_headers(headers),
                    )),
                })
            }
            "openrouter" => {
                let api_key = cfg
                    .llm_openai_api_key
                    .clone()
                    .or_else(|| cfg.openai_api_key.clone())
                    .unwrap_or_default();
                let api_base = cfg
                    .llm_openai_api_base
                    .clone()
                    .or_else(|| cfg.openai_api_base.clone());
                let model = cfg
                    .llm_model
                    .clone()
                    .unwrap_or_else(|| "openai/gpt-4o-mini".into());
                let headers =
                    Self::resolve_openai_headers(cfg, OpenRouterProvider::default_headers());
                Ok(Self {
                    provider: Some(Arc::new(OpenRouterProvider::new(
                        client, api_key, api_base, model, headers,
                    ))),
                })
            }
            // Explicit "disable LLM" values.
            "none" | "" => Ok(Self::default()),
            _ => anyhow::bail!(
                "unknown llm_backend/llm_provider '{}': must be one of: none, ollama, openai, openrouter",
                backend
            ),
        }
    }

    fn provider(&self) -> Option<Arc<dyn LlmProvider>> {
        self.provider.as_ref().map(Arc::clone)
    }
}

// ---------------------------------------------------------------------------
// DreamingEngine
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
pub struct DreamingEngine {
    llm: Option<LlmBackendHandle>,
}

/// Inner handle so the engine is Clone/Default even when holding the config.
#[derive(Clone)]
struct LlmBackendHandle {
    provider: Arc<dyn LlmProvider>,
}

impl DreamingEngine {
    pub fn new() -> Self {
        Self { llm: None }
    }

    /// Create a dreaming engine with a real LLM backend configured from Config.
    ///
    /// If the config contains an unknown backend string the error is logged and
    /// the engine falls back to no-LLM mode so that the server can still start.
    /// Callers that want hard failures should use `LlmBackend::from_config`
    /// directly.
    pub fn with_config(cfg: &engram_core::Config) -> Self {
        let backend = match LlmBackend::from_config(cfg) {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "DreamingEngine: invalid LLM config — falling back to no-LLM mode"
                );
                LlmBackend::default()
            }
        };
        let llm = backend
            .provider()
            .map(|provider| LlmBackendHandle { provider });
        Self { llm }
    }

    /// Public entry point with timeout and fallback logic.
    pub async fn summarize_cluster(
        &self,
        context_blobs: &[String],
        max_wait: Duration,
    ) -> DreamInsight {
        match timeout(max_wait, self.llm_summarize(context_blobs)).await {
            Ok(Ok(insight)) => insight,
            Ok(Err(e)) => {
                tracing::debug!("LLM summarize failed (using deterministic fallback): {e:#}");
                self.deterministic_summarize(context_blobs)
            }
            Err(_) => {
                tracing::debug!("LLM summarize timed out (using deterministic fallback)");
                self.deterministic_summarize(context_blobs)
            }
        }
    }

    /// Generate an insight linking two contexts (used by v1's `generate_insight`).
    pub async fn generate_insight(
        &self,
        context_a: &str,
        context_b: &str,
        max_wait: Duration,
    ) -> String {
        let safe_a = truncate_for_prompt(
            &redact_control_chars(context_a),
            MAX_CONTEXT_BYTES_FOR_INSIGHT,
        );
        let safe_b = truncate_for_prompt(
            &redact_control_chars(context_b),
            MAX_CONTEXT_BYTES_FOR_INSIGHT,
        );
        let prompt = INSIGHT_PROMPT
            .replace("{context_a}", &safe_a)
            .replace("{context_b}", &safe_b);

        match timeout(max_wait, self.call_llm(&prompt, 256)).await {
            Ok(Ok(text)) => {
                let trimmed = text.trim().to_string();
                if trimmed == "NO_INSIGHT" || trimmed.is_empty() {
                    String::new()
                } else {
                    trimmed
                }
            }
            Ok(Err(e)) => {
                tracing::debug!("LLM insight generation failed: {e:#}");
                String::new()
            }
            Err(_) => {
                tracing::debug!("LLM insight generation timed out");
                String::new()
            }
        }
    }

    /// Send a free-form prompt to the configured LLM backend and return the response.
    /// Returns a typed `LlmError` if no backend is configured, the call fails, or it times out.
    pub async fn generate_text(
        &self,
        prompt: &str,
        max_tokens: u32,
        max_wait: Duration,
    ) -> Result<String, LlmError> {
        match timeout(max_wait, self.call_llm(prompt, max_tokens)).await {
            Ok(Ok(text)) => Ok(text.trim().to_string()),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(LlmError::Timeout {
                provider: self.llm.as_ref().map(|h| h.provider.name().to_string()),
                status_code: None,
                retry_exhausted: false,
                message: format!(
                    "LLM text generation timed out after {} ms",
                    max_wait.as_millis()
                ),
            }),
        }
    }

    /// Suggest microservice/bounded-context boundaries from temporal coupling clusters.
    ///
    /// Calls the LLM with the migration boundary prompt. Falls back to directory-based
    /// grouping when no LLM is configured or the call fails.
    pub async fn suggest_boundaries(
        &self,
        clusters_text: &str,
        shared_state_text: &str,
        shared_tables_text: &str,
        max_wait: Duration,
    ) -> Vec<MigrationBoundary> {
        let prompt = MIGRATION_BOUNDARY_PROMPT
            .replace("{clusters}", &truncate_for_prompt(clusters_text, 12_000))
            .replace(
                "{shared_state}",
                &truncate_for_prompt(shared_state_text, 4_000),
            )
            .replace(
                "{shared_tables}",
                &truncate_for_prompt(shared_tables_text, 4_000),
            );

        match timeout(max_wait, self.call_llm(&prompt, 2048)).await {
            Ok(Ok(raw)) => {
                let boundaries = parse_boundary_response(&raw);
                if boundaries.is_empty() {
                    tracing::debug!("LLM returned unparseable boundary response, using fallback");
                    deterministic_boundaries_with_data(
                        clusters_text,
                        shared_state_text,
                        shared_tables_text,
                    )
                } else {
                    boundaries
                }
            }
            Ok(Err(e)) => {
                tracing::debug!("LLM boundary suggestion failed (using fallback): {e:#}");
                deterministic_boundaries_with_data(
                    clusters_text,
                    shared_state_text,
                    shared_tables_text,
                )
            }
            Err(_) => {
                tracing::debug!("LLM boundary suggestion timed out (using fallback)");
                deterministic_boundaries_with_data(
                    clusters_text,
                    shared_state_text,
                    shared_tables_text,
                )
            }
        }
    }

    /// Call the configured LLM backend and return its raw text output.
    async fn llm_summarize(&self, context_blobs: &[String]) -> anyhow::Result<DreamInsight> {
        let Some(handle) = &self.llm else {
            anyhow::bail!("no LLM backend configured");
        };

        let snippets = context_blobs
            .iter()
            .take(MAX_SNIPPETS_FOR_CLUSTER_SUMMARY)
            .enumerate()
            .map(|(i, b)| {
                let sanitized = redact_control_chars(b);
                let compact = truncate_for_prompt(&sanitized, MAX_SNIPPET_CHARS);
                format!("--- Snippet {} ---\n{}", i + 1, compact)
            })
            .collect::<Vec<_>>()
            .join("\n\n");

        let prompt = CLUSTER_SUMMARY_PROMPT.replace("{snippets}", &snippets);
        let raw = self
            .call_llm_with_backend(&handle.provider, &prompt, 512)
            .await?;

        Ok(parse_llm_cluster_response(&raw, context_blobs))
    }

    /// Low-level LLM call — uses whatever backend is configured.
    async fn call_llm(&self, prompt: &str, max_tokens: u32) -> Result<String, LlmError> {
        let Some(handle) = &self.llm else {
            return Err(LlmError::Transport {
                provider: None,
                status_code: None,
                retry_exhausted: false,
                message: "no LLM backend configured".into(),
            });
        };
        self.call_llm_with_backend(&handle.provider, prompt, max_tokens)
            .await
    }

    async fn call_llm_with_backend(
        &self,
        provider: &Arc<dyn LlmProvider>,
        prompt: &str,
        max_tokens: u32,
    ) -> Result<String, LlmError> {
        let response = provider
            .generate(prompt, LlmGenerateOptions::new(max_tokens))
            .await?;
        Ok(response.text)
    }

    /// Deterministic, local "dream" summarizer — always available as fallback.
    pub fn deterministic_summarize(&self, context_blobs: &[String]) -> DreamInsight {
        if context_blobs.is_empty() {
            return DreamInsight {
                title: "Insight".into(),
                summary_markdown: "No context was available to summarize.".into(),
                key_terms: Vec::new(),
            };
        }

        static TOKEN_RE: OnceLock<Regex> = OnceLock::new();
        let Some(re) = get_compiled_regex(&TOKEN_RE, r"[A-Za-z_][A-Za-z0-9_]{2,}", "dream_token")
        else {
            return DreamInsight {
                title: "Insight".into(),
                summary_markdown:
                    "Unable to compute deterministic summary due to regex initialization failure."
                        .into(),
                key_terms: Vec::new(),
            };
        };
        let mut counts: HashMap<String, usize> = HashMap::new();

        for blob in context_blobs {
            for m in re.find_iter(blob) {
                let t = m.as_str();
                if t.len() > 40 {
                    continue;
                }
                // Downweight very common noise.
                if matches!(t, "self" | "this" | "that" | "None" | "Some" | "Result") {
                    continue;
                }
                *counts.entry(t.to_string()).or_insert(0) += 1;
            }
        }

        let mut top: Vec<(String, usize)> = counts.into_iter().collect();
        top.sort_by(|a, b| b.1.cmp(&a.1));
        top.truncate(8);

        let key_terms: Vec<String> = top.iter().map(|(t, _)| t.clone()).collect();
        let title = if key_terms.len() >= 2 {
            format!("Insight: {} ↔ {}", key_terms[0], key_terms[1])
        } else if let Some((t, _)) = top.first() {
            format!("Insight: {t}")
        } else {
            "Insight".into()
        };

        let mut summary = String::new();
        summary.push_str("### Why these areas seem linked (Deterministic Fallback)\n");
        summary.push_str(
            "- Repeated concepts and identifiers show up together across your recent context.\n",
        );

        if !key_terms.is_empty() {
            summary.push_str("- Shared signals: ");
            for (i, t) in key_terms.iter().enumerate() {
                if i > 0 {
                    summary.push_str(", ");
                }
                summary.push_str(&format!("`{t}`"));
            }
            summary.push_str(".\n");
        }

        summary.push_str("\n### Suggested follow-ups\n");
        summary.push_str("- Search for call sites / tests that mention the shared signals.\n");
        summary.push_str("- If this spans multiple modules, consider extracting an explicit interface boundary.\n");

        DreamInsight {
            title,
            summary_markdown: summary,
            key_terms,
        }
    }
}

fn truncate_for_prompt(input: &str, max_chars: usize) -> String {
    input.chars().take(max_chars).collect()
}

fn redact_control_chars(input: &str) -> String {
    input
        .chars()
        .map(|c| {
            if c == '\n' || c == '\t' || !c.is_control() {
                c
            } else {
                ' '
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Parse the structured LLM cluster response
// ---------------------------------------------------------------------------

fn parse_llm_cluster_response(raw: &str, context_blobs: &[String]) -> DreamInsight {
    let mut title = String::new();
    let mut summary_lines: Vec<&str> = Vec::new();
    let mut terms_line = String::new();
    let mut in_summary = false;

    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("TITLE:") {
            title = rest.trim().to_string();
            in_summary = false;
        } else if line.trim() == "SUMMARY:" {
            in_summary = true;
        } else if let Some(rest) = line.strip_prefix("TERMS:") {
            terms_line = rest.trim().to_string();
            in_summary = false;
        } else if in_summary {
            summary_lines.push(line);
        }
    }

    let key_terms: Vec<String> = if !terms_line.is_empty() {
        let mut terms: Vec<String> = Vec::new();
        for term in terms_line
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            if terms.iter().any(|t| t.eq_ignore_ascii_case(term)) {
                continue;
            }
            terms.push(term.to_string());
            if terms.len() >= 12 {
                break;
            }
        }
        terms
    } else {
        Vec::new()
    };

    // If parsing failed, fall back to building a minimal insight.
    if title.is_empty() && summary_lines.is_empty() {
        // Use the raw LLM output verbatim.
        let fallback_engine = DreamingEngine::new();
        let mut det = fallback_engine.deterministic_summarize(context_blobs);
        if !raw.trim().is_empty() {
            det.summary_markdown = format!("{}\n\n---\n{}", raw.trim(), det.summary_markdown);
        }
        return det;
    }

    let summary_markdown = if summary_lines.is_empty() {
        let mut det = DreamingEngine::new().deterministic_summarize(context_blobs);
        if !title.is_empty() {
            det.title = title.clone();
        }
        det.summary_markdown
    } else {
        let joined = summary_lines.join("\n");
        joined.trim().to_string()
    };

    DreamInsight {
        title: if title.is_empty() {
            "Code Insight".into()
        } else {
            title
        },
        summary_markdown,
        key_terms,
    }
}

// ---------------------------------------------------------------------------
// Migration Boundary parsing + deterministic fallback
// ---------------------------------------------------------------------------

/// Parse an LLM response into `MigrationBoundary` structs.
///
/// Expected format per boundary (separated by `---`):
/// ```text
/// CONTEXT: <name>
/// FILES: <comma-separated>
/// DATA: <comma-separated>
/// DEPENDS_ON: <comma-separated or NONE>
/// RISK: <LOW|MEDIUM|HIGH>
/// SEAM_FILES: <comma-separated or NONE>
/// ```
fn parse_boundary_response(raw: &str) -> Vec<MigrationBoundary> {
    let mut boundaries = Vec::new();
    let mut seen_names = std::collections::HashSet::new();

    for block in raw.split("---") {
        let block = block.trim();
        if block.is_empty() {
            continue;
        }

        let mut context_name = String::new();
        let mut files = Vec::new();
        let mut owned_data = Vec::new();
        let mut depends_on = Vec::new();
        let mut risk = String::new();
        let mut seam_files = Vec::new();

        for line in block.lines() {
            let line = line.trim();
            if let Some(val) = line.strip_prefix("CONTEXT:") {
                context_name = val.trim().to_string();
            } else if let Some(val) = line.strip_prefix("FILES:") {
                files = split_csv(val);
            } else if let Some(val) = line.strip_prefix("DATA:") {
                owned_data = split_csv(val);
            } else if let Some(val) = line.strip_prefix("DEPENDS_ON:") {
                let items = split_csv(val);
                depends_on = items
                    .into_iter()
                    .filter(|s| !s.eq_ignore_ascii_case("none"))
                    .collect();
            } else if let Some(val) = line.strip_prefix("RISK:") {
                risk = val.trim().to_uppercase();
            } else if let Some(val) = line.strip_prefix("SEAM_FILES:") {
                let items = split_csv(val);
                seam_files = items
                    .into_iter()
                    .filter(|s| !s.eq_ignore_ascii_case("none"))
                    .collect();
            }
        }

        // Validate RISK: must be LOW/MEDIUM/HIGH; default to MEDIUM on garbage.
        if !matches!(risk.as_str(), "LOW" | "MEDIUM" | "HIGH") {
            risk = "MEDIUM".into();
        }

        // Deduplicate context names: skip if we've already seen this name.
        if !context_name.is_empty() && !files.is_empty() && seen_names.insert(context_name.clone())
        {
            boundaries.push(MigrationBoundary {
                context_name,
                files,
                owned_data,
                depends_on,
                risk,
                seam_files,
                shared_across: Vec::new(),
            });
        }
    }

    boundaries
}

/// Split a comma-separated string into trimmed, non-empty strings.
fn split_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Deterministic fallback: group files by directory prefix.
///
/// When `state_text` and `tables_text` are provided (non-empty, not "(none detected)"),
/// the function tries to assign data ownership to clusters and detects cross-cluster
/// data sharing to set appropriate risk levels.
pub fn deterministic_boundaries(clusters_text: &str) -> Vec<MigrationBoundary> {
    deterministic_boundaries_with_data(clusters_text, "", "")
}

/// Extended deterministic fallback that incorporates state/table data for smarter ownership.
pub fn deterministic_boundaries_with_data(
    clusters_text: &str,
    state_text: &str,
    tables_text: &str,
) -> Vec<MigrationBoundary> {
    let mut dir_groups: HashMap<String, Vec<String>> = HashMap::new();

    for line in clusters_text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('-') {
            continue;
        }
        // Extract file paths from the cluster text (simple heuristic).
        for token in line.split_whitespace() {
            let token = token.trim_matches(|c: char| c == ',' || c == '"' || c == '\'');
            if token.contains('/') || token.contains('\\') || token.contains('.') {
                let dir = token
                    .rsplit_once(['/', '\\'])
                    .map(|(d, _)| d.to_string())
                    .unwrap_or_else(|| "root".to_string());
                dir_groups.entry(dir).or_default().push(token.to_string());
            }
        }
    }

    // Parse data references to assign ownership.
    let state_keys: Vec<&str> =
        if state_text.is_empty() || state_text == "(none detected)" || state_text == "(none)" {
            Vec::new()
        } else {
            state_text
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect()
        };

    let table_refs: Vec<&str> =
        if tables_text.is_empty() || tables_text == "(none detected)" || tables_text == "(none)" {
            Vec::new()
        } else {
            tables_text
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect()
        };

    let has_data = !state_keys.is_empty() || !table_refs.is_empty();

    let mut boundaries: Vec<MigrationBoundary> = dir_groups
        .into_iter()
        .map(|(dir, files)| {
            let mut owned_data = Vec::new();
            if has_data {
                // Assign data to this cluster if any of its files are in a matching dir.
                let dir_lower = dir.to_lowercase();
                for key in &state_keys {
                    if key.to_lowercase().contains(&dir_lower)
                        || dir_lower.contains(&key.to_lowercase())
                    {
                        owned_data.push(format!("state:{key}"));
                    }
                }
                for table in &table_refs {
                    if table.to_lowercase().contains(&dir_lower)
                        || dir_lower.contains(&table.to_lowercase())
                    {
                        owned_data.push(format!("table:{table}"));
                    }
                }
            }
            MigrationBoundary {
                context_name: format!("{} Module", dir.split('/').next_back().unwrap_or(&dir)),
                files,
                owned_data,
                depends_on: Vec::new(),
                risk: "MEDIUM".into(),
                seam_files: Vec::new(),
                shared_across: Vec::new(),
            }
        })
        .collect();

    // Detect cross-cluster data sharing: if any data item appears in 2+ clusters,
    // mark risk as HIGH and populate shared_across.
    if has_data && boundaries.len() > 1 {
        // Build map: data_item -> list of context_names that own it.
        let mut data_owners: HashMap<String, Vec<String>> = HashMap::new();
        for b in &boundaries {
            for d in &b.owned_data {
                data_owners
                    .entry(d.clone())
                    .or_default()
                    .push(b.context_name.clone());
            }
        }
        // Mark boundaries that share data.
        for b in &mut boundaries {
            let mut shared: std::collections::HashSet<String> = std::collections::HashSet::new();
            for d in &b.owned_data {
                if let Some(owners) = data_owners.get(d)
                    && owners.len() > 1
                {
                    for o in owners {
                        if o != &b.context_name {
                            shared.insert(o.clone());
                        }
                    }
                }
            }
            if !shared.is_empty() {
                b.risk = "HIGH".into();
                b.shared_across = shared.into_iter().collect();
                b.shared_across.sort();
            }
        }
    }

    boundaries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_boundary_response() {
        let raw = r#"
CONTEXT: Order Management
FILES: Orders.aspx, Orders.aspx.vb, OrderDAL.vb
DATA: Orders table, Session:CartId
DEPENDS_ON: Customer Management
RISK: HIGH
SEAM_FILES: SharedUtils.vb
---
CONTEXT: Customer Management
FILES: Customers.aspx, CustomerService.vb
DATA: Customers table
DEPENDS_ON: NONE
RISK: LOW
SEAM_FILES: NONE
---
"#;
        let boundaries = parse_boundary_response(raw);
        assert_eq!(boundaries.len(), 2);

        assert_eq!(boundaries[0].context_name, "Order Management");
        assert_eq!(boundaries[0].files.len(), 3);
        assert_eq!(boundaries[0].risk, "HIGH");
        assert_eq!(boundaries[0].depends_on, vec!["Customer Management"]);
        assert_eq!(boundaries[0].seam_files, vec!["SharedUtils.vb"]);

        assert_eq!(boundaries[1].context_name, "Customer Management");
        assert!(boundaries[1].depends_on.is_empty());
        assert!(boundaries[1].seam_files.is_empty());
    }

    #[test]
    fn test_deterministic_fallback() {
        let clusters = r#"
Cluster 1: Orders/OrderPage.aspx, Orders/OrderDAL.vb
Cluster 2: Customers/CustomerPage.aspx, Customers/CustomerService.vb
"#;
        let boundaries = deterministic_boundaries(clusters);
        assert!(
            !boundaries.is_empty(),
            "Should produce at least 1 boundary group"
        );
        for b in &boundaries {
            assert!(!b.context_name.is_empty());
            assert!(!b.files.is_empty());
        }
    }

    fn make_llm_cfg(backend: &str) -> engram_core::Config {
        engram_core::Config {
            llm_backend: backend.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn llm_backend_from_config_rejects_unknown_provider() {
        let cfg = make_llm_cfg("groq_v99");
        let result = LlmBackend::from_config(&cfg);
        assert!(
            result.is_err(),
            "from_config must return Err for unknown provider"
        );
        // Use map_err to extract the message without requiring Debug on LlmBackend.
        let msg = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(
            msg.contains("unknown"),
            "error message should contain 'unknown', got: {msg}"
        );
    }

    #[test]
    fn llm_backend_from_config_accepts_known_providers() {
        for backend in &["none", "", "ollama", "openai", "openrouter"] {
            let cfg = make_llm_cfg(backend);
            let result = LlmBackend::from_config(&cfg);
            let err_msg = result.err().map(|e| e.to_string()).unwrap_or_default();
            assert!(
                err_msg.is_empty(),
                "from_config must accept known backend '{}', got error: {}",
                backend,
                err_msg
            );
        }
    }
}
