use regex::Regex;
use std::collections::HashMap;
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

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DreamInsight {
    pub title: String,
    pub summary_markdown: String,
    pub key_terms: Vec<String>,
}

// ---------------------------------------------------------------------------
// LLM backend config (mirrors embed.rs pattern)
// ---------------------------------------------------------------------------

/// Which LLM backend to use for text generation.
#[derive(Debug, Clone)]
pub enum LlmBackend {
    /// No LLM — use deterministic fallback only.
    None,
    /// Local Ollama server (default: http://localhost:11434).
    Ollama { url: String, model: String },
    /// OpenAI-compatible API.
    OpenAI {
        api_key: String,
        api_base: String,
        model: String,
    },
}

impl LlmBackend {
    /// Build an `LlmBackend` from the project `Config`.
    pub fn from_config(cfg: &engram_core::Config) -> Self {
        match cfg.llm_backend.as_str() {
            "ollama" => {
                let url = cfg
                    .llm_ollama_url
                    .clone()
                    .or_else(|| cfg.ollama_url.clone())
                    .unwrap_or_else(|| "http://localhost:11434".into());
                let model = cfg.llm_model.clone().unwrap_or_else(|| "llama3.2".into());
                LlmBackend::Ollama { url, model }
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
                LlmBackend::OpenAI {
                    api_key,
                    api_base,
                    model,
                }
            }
            _ => LlmBackend::None,
        }
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
    backend: std::sync::Arc<LlmBackend>,
}

impl DreamingEngine {
    pub fn new() -> Self {
        Self { llm: None }
    }

    /// Create a dreaming engine with a real LLM backend configured from Config.
    pub fn with_config(cfg: &engram_core::Config) -> Self {
        let backend = LlmBackend::from_config(cfg);
        let llm = match &backend {
            LlmBackend::None => None,
            _ => Some(LlmBackendHandle {
                backend: std::sync::Arc::new(backend),
            }),
        };
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
        let prompt = INSIGHT_PROMPT
            .replace("{context_a}", context_a)
            .replace("{context_b}", context_b);

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

    /// Call the configured LLM backend and return its raw text output.
    async fn llm_summarize(&self, context_blobs: &[String]) -> anyhow::Result<DreamInsight> {
        let Some(handle) = &self.llm else {
            anyhow::bail!("no LLM backend configured");
        };

        let snippets = context_blobs
            .iter()
            .enumerate()
            .map(|(i, b)| format!("--- Snippet {} ---\n{}", i + 1, b))
            .collect::<Vec<_>>()
            .join("\n\n");

        let prompt = CLUSTER_SUMMARY_PROMPT.replace("{snippets}", &snippets);
        let raw = self
            .call_llm_with_backend(&handle.backend, &prompt, 512)
            .await?;

        Ok(parse_llm_cluster_response(&raw, context_blobs))
    }

    /// Low-level LLM call — uses whatever backend is configured.
    async fn call_llm(&self, prompt: &str, max_tokens: u32) -> anyhow::Result<String> {
        let Some(handle) = &self.llm else {
            anyhow::bail!("no LLM backend configured");
        };
        self.call_llm_with_backend(&handle.backend, prompt, max_tokens)
            .await
    }

    async fn call_llm_with_backend(
        &self,
        backend: &LlmBackend,
        prompt: &str,
        max_tokens: u32,
    ) -> anyhow::Result<String> {
        match backend {
            LlmBackend::None => anyhow::bail!("LLM backend is None"),
            LlmBackend::Ollama { url, model } => {
                call_ollama_generate(url, model, prompt, max_tokens).await
            }
            LlmBackend::OpenAI {
                api_key,
                api_base,
                model,
            } => call_openai_chat(api_base, api_key, model, prompt, max_tokens).await,
        }
    }

    /// Deterministic, local "dream" summarizer — always available as fallback.
    pub fn deterministic_summarize(&self, context_blobs: &[String]) -> DreamInsight {
        static TOKEN_RE: OnceLock<Regex> = OnceLock::new();
        let Some(re) = get_compiled_regex(&TOKEN_RE, r"[A-Za-z_][A-Za-z0-9_]{2,}", "dream_token")
        else {
            return DreamInsight {
                title: "Insight".into(),
                summary:
                    "Unable to compute deterministic summary due to regex initialization failure."
                        .into(),
                actions: Vec::new(),
                confidence: 0.0,
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
        terms_line
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
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

    let summary_markdown = summary_lines.join("\n");

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
// HTTP helpers — Ollama chat and OpenAI chat completions
// ---------------------------------------------------------------------------

/// Call Ollama's /api/generate endpoint (non-streaming).
async fn call_ollama_generate(
    base_url: &str,
    model: &str,
    prompt: &str,
    max_tokens: u32,
) -> anyhow::Result<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .connect_timeout(Duration::from_secs(10))
        .build()?;

    let url = format!("{}/api/generate", base_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": model,
        "prompt": prompt,
        "stream": false,
        "options": {
            "num_predict": max_tokens,
            "temperature": 0.3,
        }
    });

    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..3u32 {
        if attempt > 0 {
            let backoff = Duration::from_millis(500 * (1 << attempt));
            tokio::time::sleep(backoff).await;
        }
        match client.post(&url).json(&body).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_server_error()
                    || status == reqwest::StatusCode::TOO_MANY_REQUESTS
                    || status == reqwest::StatusCode::REQUEST_TIMEOUT
                {
                    last_err = Some(anyhow::anyhow!("Ollama generate HTTP {status}"));
                    continue;
                }
                let data: serde_json::Value = resp
                    .error_for_status()?
                    .json()
                    .await
                    .map_err(|e| anyhow::anyhow!("Ollama generate JSON parse: {e}"))?;
                let text = data["response"].as_str().unwrap_or("").to_string();
                return Ok(text);
            }
            Err(e) => {
                last_err = Some(anyhow::anyhow!("Ollama generate request: {e}"));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("Ollama generate failed after retries")))
}

/// Call OpenAI's /chat/completions endpoint.
async fn call_openai_chat(
    api_base: &str,
    api_key: &str,
    model: &str,
    prompt: &str,
    max_tokens: u32,
) -> anyhow::Result<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .connect_timeout(Duration::from_secs(10))
        .build()?;

    let url = format!("{}/chat/completions", api_base.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": model,
        "messages": [
            {"role": "user", "content": prompt}
        ],
        "max_tokens": max_tokens,
        "temperature": 0.3,
    });

    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..3u32 {
        if attempt > 0 {
            let backoff = Duration::from_millis(500 * (1 << attempt));
            tokio::time::sleep(backoff).await;
        }
        match client
            .post(&url)
            .bearer_auth(api_key)
            .json(&body)
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status();
                if status.is_server_error()
                    || status == reqwest::StatusCode::TOO_MANY_REQUESTS
                    || status == reqwest::StatusCode::REQUEST_TIMEOUT
                {
                    last_err = Some(anyhow::anyhow!("OpenAI chat HTTP {status}"));
                    continue;
                }
                let data: serde_json::Value = resp
                    .error_for_status()?
                    .json()
                    .await
                    .map_err(|e| anyhow::anyhow!("OpenAI chat JSON parse: {e}"))?;
                let text = data["choices"][0]["message"]["content"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                return Ok(text);
            }
            Err(e) => {
                last_err = Some(anyhow::anyhow!("OpenAI chat request: {e}"));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("OpenAI chat failed after retries")))
}
