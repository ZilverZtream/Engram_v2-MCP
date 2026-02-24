use async_trait::async_trait;
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
        let body = serde_json::json!({
            "model": self.model,
            "messages": [{"role": "user", "content": prompt}],
            "max_tokens": options.max_tokens,
            "temperature": options.temperature,
        });

        with_retry(async || {
            let mut request = self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body);
            for (name, value) in &self.extra_headers {
                request = request.header(name, value);
            }

            let resp = request
                .send()
                .await
                .map_err(|e| map_reqwest_error("openai", e))?;
            let status = resp.status();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                return Err(LlmError::RateLimited {
                    provider: Some("openai".into()),
                    status_code: Some(status.as_u16()),
                    retry_exhausted: false,
                    message: format!("HTTP {status}"),
                });
            }
            if status.is_server_error() {
                return Err(LlmError::Upstream5xx {
                    provider: Some("openai".into()),
                    status_code: Some(status.as_u16()),
                    retry_exhausted: false,
                    message: format!("HTTP {status}"),
                });
            }
            if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                return Err(LlmError::Auth {
                    provider: Some("openai".into()),
                    status_code: Some(status.as_u16()),
                    retry_exhausted: false,
                    message: format!("HTTP {status}"),
                });
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
            let text = data["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or("")
                .trim()
                .to_string();
            if text.is_empty() {
                return Err(LlmError::InvalidResponse {
                    provider: Some("openai".into()),
                    status_code: Some(status.as_u16()),
                    retry_exhausted: false,
                    message: "returned empty choices[0].message.content".into(),
                });
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
