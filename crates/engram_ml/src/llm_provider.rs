use anyhow::Context;
use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::time::Duration;

pub type LlmError = anyhow::Error;

#[derive(Debug, Clone)]
pub struct LlmGenerateOptions {
    pub max_tokens: u32,
    pub temperature: f32,
}

impl LlmGenerateOptions {
    pub fn new(max_tokens: u32) -> Self {
        Self {
            max_tokens,
            temperature: 0.3,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub text: String,
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn generate(
        &self,
        prompt: &str,
        options: LlmGenerateOptions,
    ) -> Result<LlmResponse, LlmError>;
}

#[derive(Clone)]
pub struct OllamaProvider {
    client: reqwest::Client,
    base_url: String,
    model: String,
}

impl OllamaProvider {
    pub fn new(client: reqwest::Client, base_url: String, model: String) -> Self {
        Self {
            client,
            base_url,
            model,
        }
    }
}

#[async_trait]
impl LlmProvider for OllamaProvider {
    async fn generate(
        &self,
        prompt: &str,
        options: LlmGenerateOptions,
    ) -> Result<LlmResponse, LlmError> {
        if self.model.trim().is_empty() {
            anyhow::bail!("Ollama model cannot be empty");
        }
        if self.base_url.trim().is_empty() {
            anyhow::bail!("Ollama base URL cannot be empty");
        }

        let url = format!("{}/api/generate", self.base_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "model": self.model,
            "prompt": prompt,
            "stream": false,
            "options": {
                "num_predict": options.max_tokens,
                "temperature": options.temperature,
            }
        });

        with_retry("Ollama generate", async || {
            let resp = self.client.post(&url).json(&body).send().await?;
            let status = resp.status();
            if status.is_server_error()
                || status == reqwest::StatusCode::TOO_MANY_REQUESTS
                || status == reqwest::StatusCode::REQUEST_TIMEOUT
            {
                anyhow::bail!("HTTP {status}");
            }
            let data: serde_json::Value = resp
                .error_for_status()?
                .json()
                .await
                .context("JSON parse")?;
            let text = data["response"].as_str().unwrap_or("").trim().to_string();
            if text.is_empty() {
                anyhow::bail!("returned empty response payload");
            }
            Ok(LlmResponse { text })
        })
        .await
    }
}

#[derive(Clone)]
pub struct OpenAiCompatibleProvider {
    client: reqwest::Client,
    api_key: String,
    api_base: String,
    model: String,
    extra_headers: HeaderMap,
}

impl OpenAiCompatibleProvider {
    pub fn new(client: reqwest::Client, api_key: String, api_base: String, model: String) -> Self {
        Self {
            client,
            api_key,
            api_base,
            model,
            extra_headers: HeaderMap::new(),
        }
    }

    pub fn with_headers(mut self, headers: HeaderMap) -> Self {
        self.extra_headers = headers;
        self
    }
}

#[async_trait]
impl LlmProvider for OpenAiCompatibleProvider {
    async fn generate(
        &self,
        prompt: &str,
        options: LlmGenerateOptions,
    ) -> Result<LlmResponse, LlmError> {
        if self.api_key.trim().is_empty() {
            anyhow::bail!("OpenAI API key cannot be empty when llm_backend=openai");
        }
        if self.model.trim().is_empty() {
            anyhow::bail!("OpenAI model cannot be empty");
        }
        if self.api_base.trim().is_empty() {
            anyhow::bail!("OpenAI API base URL cannot be empty");
        }

        let url = format!("{}/chat/completions", self.api_base.trim_end_matches('/'));
        let body = serde_json::json!({
            "model": self.model,
            "messages": [{"role": "user", "content": prompt}],
            "max_tokens": options.max_tokens,
            "temperature": options.temperature,
        });

        with_retry("OpenAI chat", async || {
            let mut request = self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body);
            for (name, value) in &self.extra_headers {
                request = request.header(name, value);
            }

            let resp = request.send().await?;
            let status = resp.status();
            if status.is_server_error()
                || status == reqwest::StatusCode::TOO_MANY_REQUESTS
                || status == reqwest::StatusCode::REQUEST_TIMEOUT
            {
                anyhow::bail!("HTTP {status}");
            }

            let data: serde_json::Value = resp
                .error_for_status()?
                .json()
                .await
                .context("JSON parse")?;
            let text = data["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or("")
                .trim()
                .to_string();
            if text.is_empty() {
                anyhow::bail!("returned empty choices[0].message.content");
            }
            Ok(LlmResponse { text })
        })
        .await
    }
}

#[derive(Clone)]
pub struct OpenRouterProvider {
    inner: OpenAiCompatibleProvider,
}

impl OpenRouterProvider {
    pub fn new(
        client: reqwest::Client,
        api_key: String,
        api_base: Option<String>,
        model: String,
        extra_headers: HeaderMap,
    ) -> Self {
        let base = api_base.unwrap_or_else(|| "https://openrouter.ai/api/v1".to_string());
        Self {
            inner: OpenAiCompatibleProvider::new(client, api_key, base, model)
                .with_headers(extra_headers),
        }
    }

    pub fn default_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("http-referer"),
            HeaderValue::from_static("https://engram.local"),
        );
        headers.insert(
            HeaderName::from_static("x-title"),
            HeaderValue::from_static("Engram"),
        );
        headers
    }
}

#[async_trait]
impl LlmProvider for OpenRouterProvider {
    async fn generate(
        &self,
        prompt: &str,
        options: LlmGenerateOptions,
    ) -> Result<LlmResponse, LlmError> {
        self.inner.generate(prompt, options).await
    }
}

async fn with_retry<T, F, Fut>(label: &str, mut op: F) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..3u32 {
        if attempt > 0 {
            let backoff = Duration::from_millis(500_u64.saturating_mul(1_u64 << attempt));
            tokio::time::sleep(backoff).await;
        }
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => last_err = Some(anyhow::anyhow!("{label} {e}")),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("{label} failed after retries")))
}
