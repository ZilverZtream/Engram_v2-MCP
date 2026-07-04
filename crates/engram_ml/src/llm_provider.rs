use async_trait::async_trait;
use reqwest::StatusCode;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error(
        "LLM timeout (provider={provider:?}, status={status_code:?}, retry_exhausted={retry_exhausted}): {message}"
    )]
    Timeout {
        provider: Option<String>,
        status_code: Option<u16>,
        retry_exhausted: bool,
        message: String,
    },
    #[error(
        "LLM auth error (provider={provider:?}, status={status_code:?}, retry_exhausted={retry_exhausted}): {message}"
    )]
    Auth {
        provider: Option<String>,
        status_code: Option<u16>,
        retry_exhausted: bool,
        message: String,
    },
    #[error(
        "LLM rate limited (provider={provider:?}, status={status_code:?}, retry_exhausted={retry_exhausted}): {message}"
    )]
    RateLimited {
        provider: Option<String>,
        status_code: Option<u16>,
        retry_exhausted: bool,
        message: String,
    },
    #[error(
        "LLM invalid response (provider={provider:?}, status={status_code:?}, retry_exhausted={retry_exhausted}): {message}"
    )]
    InvalidResponse {
        provider: Option<String>,
        status_code: Option<u16>,
        retry_exhausted: bool,
        message: String,
    },
    #[error(
        "LLM upstream 5xx (provider={provider:?}, status={status_code:?}, retry_exhausted={retry_exhausted}): {message}"
    )]
    Upstream5xx {
        provider: Option<String>,
        status_code: Option<u16>,
        retry_exhausted: bool,
        message: String,
    },
    #[error(
        "LLM transport error (provider={provider:?}, status={status_code:?}, retry_exhausted={retry_exhausted}): {message}"
    )]
    Transport {
        provider: Option<String>,
        status_code: Option<u16>,
        retry_exhausted: bool,
        message: String,
    },
    #[error(
        "LLM parse error (provider={provider:?}, status={status_code:?}, retry_exhausted={retry_exhausted}): {message}"
    )]
    Parse {
        provider: Option<String>,
        status_code: Option<u16>,
        retry_exhausted: bool,
        message: String,
    },
}

impl LlmError {
    pub fn provider(&self) -> Option<&str> {
        match self {
            Self::Timeout { provider, .. }
            | Self::Auth { provider, .. }
            | Self::RateLimited { provider, .. }
            | Self::InvalidResponse { provider, .. }
            | Self::Upstream5xx { provider, .. }
            | Self::Transport { provider, .. }
            | Self::Parse { provider, .. } => provider.as_deref(),
        }
    }

    pub fn status_code(&self) -> Option<u16> {
        match self {
            Self::Timeout { status_code, .. }
            | Self::Auth { status_code, .. }
            | Self::RateLimited { status_code, .. }
            | Self::InvalidResponse { status_code, .. }
            | Self::Upstream5xx { status_code, .. }
            | Self::Transport { status_code, .. }
            | Self::Parse { status_code, .. } => *status_code,
        }
    }

    pub fn retry_exhausted(&self) -> bool {
        match self {
            Self::Timeout {
                retry_exhausted, ..
            }
            | Self::Auth {
                retry_exhausted, ..
            }
            | Self::RateLimited {
                retry_exhausted, ..
            }
            | Self::InvalidResponse {
                retry_exhausted, ..
            }
            | Self::Upstream5xx {
                retry_exhausted, ..
            }
            | Self::Transport {
                retry_exhausted, ..
            }
            | Self::Parse {
                retry_exhausted, ..
            } => *retry_exhausted,
        }
    }

    fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Timeout { .. }
                | Self::RateLimited { .. }
                | Self::Upstream5xx { .. }
                | Self::Transport { .. }
        )
    }

    fn with_retry_exhausted(self, exhausted: bool) -> Self {
        match self {
            Self::Timeout {
                provider,
                status_code,
                message,
                ..
            } => Self::Timeout {
                provider,
                status_code,
                retry_exhausted: exhausted,
                message,
            },
            Self::Auth {
                provider,
                status_code,
                message,
                ..
            } => Self::Auth {
                provider,
                status_code,
                retry_exhausted: exhausted,
                message,
            },
            Self::RateLimited {
                provider,
                status_code,
                message,
                ..
            } => Self::RateLimited {
                provider,
                status_code,
                retry_exhausted: exhausted,
                message,
            },
            Self::InvalidResponse {
                provider,
                status_code,
                message,
                ..
            } => Self::InvalidResponse {
                provider,
                status_code,
                retry_exhausted: exhausted,
                message,
            },
            Self::Upstream5xx {
                provider,
                status_code,
                message,
                ..
            } => Self::Upstream5xx {
                provider,
                status_code,
                retry_exhausted: exhausted,
                message,
            },
            Self::Transport {
                provider,
                status_code,
                message,
                ..
            } => Self::Transport {
                provider,
                status_code,
                retry_exhausted: exhausted,
                message,
            },
            Self::Parse {
                provider,
                status_code,
                message,
                ..
            } => Self::Parse {
                provider,
                status_code,
                retry_exhausted: exhausted,
                message,
            },
        }
    }
}

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
    fn name(&self) -> &'static str;

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
    fn name(&self) -> &'static str {
        "ollama"
    }

    async fn generate(
        &self,
        prompt: &str,
        options: LlmGenerateOptions,
    ) -> Result<LlmResponse, LlmError> {
        if self.model.trim().is_empty() {
            return Err(LlmError::InvalidResponse {
                provider: Some("ollama".into()),
                status_code: None,
                retry_exhausted: false,
                message: "Ollama model cannot be empty".into(),
            });
        }
        if self.base_url.trim().is_empty() {
            return Err(LlmError::InvalidResponse {
                provider: Some("ollama".into()),
                status_code: None,
                retry_exhausted: false,
                message: "Ollama base URL cannot be empty".into(),
            });
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

        with_retry(async || {
            let resp = self
                .client
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| map_reqwest_error("ollama", e))?;
            let status = resp.status();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                return Err(LlmError::RateLimited {
                    provider: Some("ollama".into()),
                    status_code: Some(status.as_u16()),
                    retry_exhausted: false,
                    message: format!("HTTP {status}"),
                });
            }
            if status.is_server_error() {
                return Err(LlmError::Upstream5xx {
                    provider: Some("ollama".into()),
                    status_code: Some(status.as_u16()),
                    retry_exhausted: false,
                    message: format!("HTTP {status}"),
                });
            }
            if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                return Err(LlmError::Auth {
                    provider: Some("ollama".into()),
                    status_code: Some(status.as_u16()),
                    retry_exhausted: false,
                    message: format!("HTTP {status}"),
                });
            }
            let resp = resp
                .error_for_status()
                .map_err(|e| map_reqwest_error("ollama", e))?;
            let data: serde_json::Value = resp.json().await.map_err(|e| LlmError::Parse {
                provider: Some("ollama".into()),
                status_code: Some(status.as_u16()),
                retry_exhausted: false,
                message: e.to_string(),
            })?;
            let text = data["response"].as_str().unwrap_or("").trim().to_string();
            if text.is_empty() {
                return Err(LlmError::InvalidResponse {
                    provider: Some("ollama".into()),
                    status_code: Some(status.as_u16()),
                    retry_exhausted: false,
                    message: "returned empty response payload".into(),
                });
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
    /// Ask the gateway to MINIMIZE hidden chain-of-thought for reasoning
    /// models (OpenRouter `reasoning: {effort: "low"}`). Without this,
    /// models like deepseek-v4 spend the entire max_tokens budget on
    /// reasoning and return an EMPTY `message.content` with HTTP 200.
    /// `enabled: false` is NOT used — endpoints with mandatory reasoning
    /// (e.g. openai/gpt-oss-120b) reject it with HTTP 400 "Reasoning is
    /// mandatory for this endpoint", while `effort: "low"` is accepted by
    /// both mandatory- and optional-reasoning models (probed 2026-07-04).
    /// Only set for OpenRouter; plain OpenAI rejects unknown params.
    disable_reasoning: bool,
}

impl OpenAiCompatibleProvider {
    pub fn new(client: reqwest::Client, api_key: String, api_base: String, model: String) -> Self {
        Self {
            client,
            api_key,
            api_base,
            model,
            extra_headers: HeaderMap::new(),
            disable_reasoning: false,
        }
    }

    pub fn with_headers(mut self, headers: HeaderMap) -> Self {
        self.extra_headers = headers;
        self
    }

    pub fn with_reasoning_disabled(mut self) -> Self {
        self.disable_reasoning = true;
        self
    }
}

const RETRY_MAX_ATTEMPTS: u32 = 3;
const RETRY_BASE_DELAY_MS: u64 = 500;
const RETRY_MAX_DELAY_MS: u64 = 8_000;

fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();

    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }

    let retry_at = httpdate::parse_http_date(value).ok()?;
    let now = std::time::SystemTime::now();
    let wait = retry_at.duration_since(now).unwrap_or_default();
    Some(wait)
}

fn is_retryable_status(status: Option<StatusCode>) -> bool {
    matches!(
        status,
        Some(StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS)
    ) || status.is_some_and(|s| s.is_server_error())
}

fn fallback_backoff_delay(attempt: u32) -> Duration {
    let exp = 1_u64 << attempt.saturating_sub(1).min(6);
    let base = RETRY_BASE_DELAY_MS.saturating_mul(exp);
    let jitter_ms = ((attempt as u64)
        .saturating_mul(1_103_515_245)
        .wrapping_add(12_345)
        % 251)
        + 25;
    Duration::from_millis(base.saturating_add(jitter_ms).min(RETRY_MAX_DELAY_MS))
}

fn select_retry_delay(
    attempt: u32,
    status: Option<StatusCode>,
    headers: Option<&HeaderMap>,
) -> Option<Duration> {
    if !is_retryable_status(status) {
        return None;
    }
    if let Some(headers) = headers
        && let Some(delay) = parse_retry_after(headers)
    {
        return Some(delay.min(Duration::from_millis(RETRY_MAX_DELAY_MS)));
    }
    Some(fallback_backoff_delay(attempt))
}

#[async_trait]
impl LlmProvider for OpenAiCompatibleProvider {
    fn name(&self) -> &'static str {
        "openai"
    }

    async fn generate(
        &self,
        prompt: &str,
        options: LlmGenerateOptions,
    ) -> Result<LlmResponse, LlmError> {
        if self.api_key.trim().is_empty() {
            return Err(LlmError::Auth {
                provider: Some("openai".into()),
                status_code: None,
                retry_exhausted: false,
                message: "OpenAI API key cannot be empty when llm_backend=openai".into(),
            });
        }
        if self.model.trim().is_empty() {
            return Err(LlmError::InvalidResponse {
                provider: Some("openai".into()),
                status_code: None,
                retry_exhausted: false,
                message: "OpenAI model cannot be empty".into(),
            });
        }
        if self.api_base.trim().is_empty() {
            return Err(LlmError::InvalidResponse {
                provider: Some("openai".into()),
                status_code: None,
                retry_exhausted: false,
                message: "OpenAI API base URL cannot be empty".into(),
            });
        }

        let url = format!("{}/chat/completions", self.api_base.trim_end_matches('/'));
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": [{"role": "user", "content": prompt}],
            "max_tokens": options.max_tokens,
            "temperature": options.temperature,
        });
        if self.disable_reasoning {
            body["reasoning"] = serde_json::json!({ "effort": "low" });
        }

        let mut last_err: Option<LlmError> = None;

        for attempt in 1..=RETRY_MAX_ATTEMPTS {
            let mut request = self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body);
            for (name, value) in &self.extra_headers {
                request = request.header(name, value);
            }

            let resp = request.send().await;
            let resp = match resp {
                Ok(resp) => resp,
                Err(err) => {
                    let mapped = map_reqwest_error("openai", err);
                    let status = mapped
                        .status_code()
                        .and_then(|code| StatusCode::from_u16(code).ok());
                    if attempt < RETRY_MAX_ATTEMPTS
                        && let Some(delay) = select_retry_delay(attempt, status, None)
                    {
                        tracing::warn!(
                            attempt,
                            status_code = status.map(|s| s.as_u16()),
                            delay_ms = delay.as_millis() as u64,
                            "retrying openai-compatible request after transport error"
                        );
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    last_err = Some(mapped);
                    break;
                }
            };
            let status = resp.status();
            if let Some(delay) = select_retry_delay(attempt, Some(status), Some(resp.headers()))
                && attempt < RETRY_MAX_ATTEMPTS
            {
                tracing::warn!(
                    attempt,
                    status_code = status.as_u16(),
                    delay_ms = delay.as_millis() as u64,
                    "retrying openai-compatible request after retryable status"
                );
                tokio::time::sleep(delay).await;
                continue;
            }

            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                last_err = Some(LlmError::RateLimited {
                    provider: Some("openai".into()),
                    status_code: Some(status.as_u16()),
                    retry_exhausted: attempt == RETRY_MAX_ATTEMPTS,
                    message: format!("HTTP {status}"),
                });
                break;
            }
            if status == reqwest::StatusCode::REQUEST_TIMEOUT {
                last_err = Some(LlmError::Timeout {
                    provider: Some("openai".into()),
                    status_code: Some(status.as_u16()),
                    retry_exhausted: attempt == RETRY_MAX_ATTEMPTS,
                    message: format!("HTTP {status}"),
                });
                break;
            }
            if status.is_server_error() {
                last_err = Some(LlmError::Upstream5xx {
                    provider: Some("openai".into()),
                    status_code: Some(status.as_u16()),
                    retry_exhausted: attempt == RETRY_MAX_ATTEMPTS,
                    message: format!("HTTP {status}"),
                });
                break;
            }
            if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                last_err = Some(LlmError::Auth {
                    provider: Some("openai".into()),
                    status_code: Some(status.as_u16()),
                    retry_exhausted: false,
                    message: format!("HTTP {status}"),
                });
                break;
            }
            let resp = resp
                .error_for_status()
                .map_err(|e| map_reqwest_error("openai", e))?;
            let data: serde_json::Value = resp.json().await.map_err(|e| LlmError::Parse {
                provider: Some("openai".into()),
                status_code: Some(status.as_u16()),
                retry_exhausted: false,
                message: e.to_string(),
            })?;
            let mut text = data["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or("")
                .trim()
                .to_string();
            if text.is_empty() {
                // Hybrid reasoning models can put their entire output in the
                // reasoning channel (OpenRouter: `reasoning`; deepseek native:
                // `reasoning_content`) and leave `content` empty. Salvage it —
                // downstream parsers extract the JSON object from prose.
                for key in ["reasoning", "reasoning_content"] {
                    if let Some(r) = data["choices"][0]["message"][key].as_str() {
                        let r = r.trim();
                        if !r.is_empty() {
                            text = r.to_string();
                            break;
                        }
                    }
                }
            }
            if text.is_empty() {
                last_err = Some(LlmError::InvalidResponse {
                    provider: Some("openai".into()),
                    status_code: Some(status.as_u16()),
                    retry_exhausted: false,
                    message: "returned empty choices[0].message.content".into(),
                });
                break;
            }
            return Ok(LlmResponse { text });
        }

        Err(last_err
            .unwrap_or_else(|| LlmError::Transport {
                provider: Some("openai".into()),
                status_code: None,
                retry_exhausted: true,
                message: "OpenAI-compatible request failed after retries".into(),
            })
            .with_retry_exhausted(true))
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
                .with_headers(extra_headers)
                .with_reasoning_disabled(),
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
    fn name(&self) -> &'static str {
        "openrouter"
    }

    async fn generate(
        &self,
        prompt: &str,
        options: LlmGenerateOptions,
    ) -> Result<LlmResponse, LlmError> {
        self.inner.generate(prompt, options).await
    }
}

fn map_reqwest_error(provider: &str, err: reqwest::Error) -> LlmError {
    if err.is_timeout() {
        return LlmError::Timeout {
            provider: Some(provider.to_string()),
            status_code: err.status().map(|s| s.as_u16()),
            retry_exhausted: false,
            message: err.to_string(),
        };
    }
    if let Some(status) = err.status() {
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return LlmError::Auth {
                provider: Some(provider.to_string()),
                status_code: Some(status.as_u16()),
                retry_exhausted: false,
                message: err.to_string(),
            };
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return LlmError::RateLimited {
                provider: Some(provider.to_string()),
                status_code: Some(status.as_u16()),
                retry_exhausted: false,
                message: err.to_string(),
            };
        }
        if status.is_server_error() {
            return LlmError::Upstream5xx {
                provider: Some(provider.to_string()),
                status_code: Some(status.as_u16()),
                retry_exhausted: false,
                message: err.to_string(),
            };
        }
    }
    LlmError::Transport {
        provider: Some(provider.to_string()),
        status_code: err.status().map(|s| s.as_u16()),
        retry_exhausted: false,
        message: err.to_string(),
    }
}

async fn with_retry<T, F, Fut>(mut op: F) -> Result<T, LlmError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, LlmError>>,
{
    let mut last_err: Option<LlmError> = None;
    for attempt in 0..3u32 {
        if attempt > 0 {
            let backoff = Duration::from_millis(500_u64.saturating_mul(1_u64 << attempt));
            tokio::time::sleep(backoff).await;
        }
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                let retryable = e.is_retryable();
                last_err = Some(e);
                if !retryable {
                    break;
                }
            }
        }
    }
    Err(last_err
        .unwrap_or_else(|| LlmError::Transport {
            provider: None,
            status_code: None,
            retry_exhausted: true,
            message: "LLM call failed after retries".into(),
        })
        .with_retry_exhausted(true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_delay_uses_retry_after_header_seconds() {
        let mut headers = HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, HeaderValue::from_static("7"));

        let delay = select_retry_delay(1, Some(StatusCode::TOO_MANY_REQUESTS), Some(&headers));

        assert_eq!(delay, Some(Duration::from_secs(7)));
    }

    #[test]
    fn retry_delay_falls_back_to_exponential_jitter_and_caps() {
        let delay_1 = select_retry_delay(1, Some(StatusCode::TOO_MANY_REQUESTS), None)
            .expect("retry delay expected");
        let delay_2 = select_retry_delay(2, Some(StatusCode::TOO_MANY_REQUESTS), None)
            .expect("retry delay expected");
        let delay_20 = select_retry_delay(20, Some(StatusCode::INTERNAL_SERVER_ERROR), None)
            .expect("retry delay expected");

        assert!(delay_2 > delay_1);
        assert!(delay_20 <= Duration::from_millis(RETRY_MAX_DELAY_MS));
    }

    #[test]
    fn retry_delay_not_selected_for_non_retryable_status() {
        let delay = select_retry_delay(1, Some(StatusCode::BAD_REQUEST), None);
        assert!(delay.is_none());
    }
}
