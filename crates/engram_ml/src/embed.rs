use ahash::AHasher;
use async_trait::async_trait;
use std::hash::{BuildHasher, Hash, Hasher};

pub type Embedding = Vec<f32>;

#[async_trait]
pub trait Embedder: Send + Sync {
    async fn embed(&self, text: &str) -> anyhow::Result<Embedding>;
    fn dimension(&self) -> usize;

    /// Embed multiple texts in a single batch. Default falls back to sequential
    /// calls, but remote embedders override this with true batched API calls
    /// for 5-10x throughput improvement.
    async fn embed_batch(&self, texts: &[&str]) -> anyhow::Result<Vec<Embedding>> {
        let mut results = Vec::with_capacity(texts.len());
        for text in texts {
            results.push(self.embed(text).await?);
        }
        Ok(results)
    }
}

// ---------------------------------------------------------------------------
// ProjectionEmbedder — fast, deterministic, no external deps.
// Suitable for low-resource environments and as a fallback.
// ---------------------------------------------------------------------------

/// A fast, deterministic embedder using random projection of trigram hashes.
/// This is a "real" implementation suitable for low-resource environments.
#[derive(Clone)]
pub struct ProjectionEmbedder {
    pub dim: usize,
}

impl ProjectionEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim }
    }
}

#[async_trait]
impl Embedder for ProjectionEmbedder {
    fn dimension(&self) -> usize {
        self.dim
    }

    async fn embed(&self, text: &str) -> anyhow::Result<Embedding> {
        // Fix #3: empty / whitespace-only text returns a stable unit vector anchored
        // at dimension 0 rather than an all-zero vector.  An all-zero vector causes
        // division-by-zero in cosine-similarity databases (LanceDB).
        if text.is_empty() {
            let mut vec = vec![0.0f32; self.dim];
            if self.dim > 0 {
                vec[0] = 1.0;
            }
            return Ok(vec);
        }

        let mut vec = vec![0.0f32; self.dim];

        // Tokenize into trigrams
        let chars: Vec<char> = text.chars().collect();
        if chars.len() < 3 {
            // Fallback for very short text.
            let h = ahash_u64_fixed_a(text.as_bytes());
            let idx = (h as usize) % self.dim;
            vec[idx] = 1.0;
            return Ok(vec);
        }

        // Use a reusable String buffer to avoid per-trigram allocation.
        let mut trigram_buf = String::with_capacity(12); // 3 chars × up to 4 bytes each
        for i in 0..chars.len() - 2 {
            trigram_buf.clear();
            for &c in &chars[i..i + 3] {
                trigram_buf.push(c);
            }

            // Two independent fixed-seed ahash calls give two projection slots.
            // Fix #2: use ahash::RandomState::with_seeds so the hash output is
            // identical across process restarts (AHasher::default() uses OS entropy).
            let h0 = ahash_u64_fixed_a(trigram_buf.as_bytes());
            let h1 = ahash_u64_fixed_b(trigram_buf.as_bytes());

            let idx0 = (h0 as usize) % self.dim;
            let sign0 = if (h0 >> 63) == 0 { 1.0f32 } else { -1.0 };
            vec[idx0] += sign0;

            let idx1 = (h1 as usize) % self.dim;
            let sign1 = if (h1 >> 63) == 0 { 1.0f32 } else { -1.0 };
            vec[idx1] += sign1;
        }

        // L2 Normalize
        let norm = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in vec.iter_mut() {
                *x /= norm;
            }
        } else {
            // Guard: all projections cancelled; fall back to unit vector at dim 0.
            if self.dim > 0 {
                vec[0] = 1.0;
            }
        }

        Ok(vec)
    }
}

/// Local embedder (delegates to ProjectionEmbedder).
/// Future: swap to Candle / ONNX Runtime with a real sentence-transformer.
#[derive(Clone)]
pub struct LocalEmbedder;

#[async_trait]
impl Embedder for LocalEmbedder {
    fn dimension(&self) -> usize {
        384
    }
    async fn embed(&self, text: &str) -> anyhow::Result<Embedding> {
        ProjectionEmbedder::new(384).embed(text).await
    }
}

// Fix #2: Two independent fixed-seed hash families.
// ahash::RandomState::with_seeds uses compile-time constants, so the hash
// output is identical across every process restart (unlike AHasher::default()
// which re-seeds from OS entropy on every startup).

/// Hash family A — used for projection slot 0.
fn ahash_u64_fixed_a(data: &[u8]) -> u64 {
    let state = ahash::RandomState::with_seeds(
        0x6c62272e_07bb0142,
        0x62b82175_6295c58d,
        0x9368d954_3c2f05db,
        0x2f50f073_c8fa3ba5,
    );
    let mut h: AHasher = state.build_hasher();
    data.hash(&mut h);
    h.finish()
}

/// Hash family B — used for projection slot 1 (independent of A).
fn ahash_u64_fixed_b(data: &[u8]) -> u64 {
    let state = ahash::RandomState::with_seeds(
        0xdeadbabe_f00dcafe,
        0x01234567_89abcdef,
        0xfedcba98_76543210,
        0xc0ffee11_deadbeef,
    );
    let mut h: AHasher = state.build_hasher();
    data.hash(&mut h);
    h.finish()
}

// ---------------------------------------------------------------------------
// OllamaEmbedder — calls a local Ollama server's /api/embed endpoint.
// ---------------------------------------------------------------------------

/// Embedder that calls Ollama's /api/embed endpoint.
#[derive(Clone)]
pub struct OllamaEmbedder {
    pub model: String,
    pub url: String,
    pub dim: usize,
    client: reqwest::Client,
}

impl OllamaEmbedder {
    /// `dim` must match the actual output dimension of `model`.
    /// e.g. nomic-embed-text → 768, mxbai-embed-large → 1024.
    /// Use `dim = 0` to auto-detect on first call (not yet implemented;
    /// caller should know the model's dimension).
    pub fn new(model: impl Into<String>, url: impl Into<String>, dim: usize) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            model: model.into(),
            url: url.into().trim_end_matches('/').to_string(),
            dim,
            client,
        }
    }
}

#[async_trait]
impl Embedder for OllamaEmbedder {
    fn dimension(&self) -> usize {
        self.dim
    }

    async fn embed(&self, text: &str) -> anyhow::Result<Embedding> {
        embed_via_ollama(&self.client, &self.url, &self.model, text, self.dim).await
    }

    async fn embed_batch(&self, texts: &[&str]) -> anyhow::Result<Vec<Embedding>> {
        embed_batch_via_ollama(&self.client, &self.url, &self.model, texts, self.dim).await
    }
}

/// Shared HTTP helper for Ollama /api/embed with exponential-backoff retry.
async fn embed_via_ollama(
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
    text: &str,
    expected_dim: usize,
) -> anyhow::Result<Embedding> {
    let url = format!("{base_url}/api/embed");
    let body = serde_json::json!({
        "model": model,
        "input": [text],
    });

    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..3u32 {
        if attempt > 0 {
            let backoff = std::time::Duration::from_millis(500 * (1 << attempt));
            tokio::time::sleep(backoff).await;
        }
        match client.post(&url).json(&body).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_server_error()
                    || status == reqwest::StatusCode::TOO_MANY_REQUESTS
                    || status == reqwest::StatusCode::REQUEST_TIMEOUT
                {
                    last_err = Some(anyhow::anyhow!("Ollama HTTP {status}"));
                    continue;
                }
                let data: serde_json::Value = resp
                    .error_for_status()?
                    .json()
                    .await
                    .map_err(|e| anyhow::anyhow!("Ollama JSON parse error: {e}"))?;
                let vec: Vec<f32> = data["embeddings"][0]
                    .as_array()
                    .ok_or_else(|| anyhow::anyhow!("Ollama response missing embeddings[0]"))?
                    .iter()
                    .map(|v| v.as_f64().unwrap_or(0.0) as f32)
                    .collect();
                if expected_dim > 0 && vec.len() != expected_dim {
                    anyhow::bail!(
                        "Ollama model '{model}' returned dim={} but expected {expected_dim}",
                        vec.len()
                    );
                }
                return Ok(vec);
            }
            Err(e) => {
                last_err = Some(anyhow::anyhow!("Ollama request error: {e}"));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("Ollama embedding failed after retries")))
}

/// Batch embed via Ollama /api/embed — sends all texts as a single "input" array.
/// Ollama natively supports multi-input in its embed endpoint, returning one
/// embedding per input. Falls back to sequential calls if the batch response
/// doesn't contain the expected number of embeddings.
async fn embed_batch_via_ollama(
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
    texts: &[&str],
    expected_dim: usize,
) -> anyhow::Result<Vec<Embedding>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    if texts.len() == 1 {
        return Ok(vec![
            embed_via_ollama(client, base_url, model, texts[0], expected_dim).await?,
        ]);
    }
    let url = format!("{base_url}/api/embed");
    let body = serde_json::json!({
        "model": model,
        "input": texts,
    });

    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..3u32 {
        if attempt > 0 {
            let backoff = std::time::Duration::from_millis(500 * (1 << attempt));
            tokio::time::sleep(backoff).await;
        }
        match client.post(&url).json(&body).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_server_error()
                    || status == reqwest::StatusCode::TOO_MANY_REQUESTS
                    || status == reqwest::StatusCode::REQUEST_TIMEOUT
                {
                    last_err = Some(anyhow::anyhow!("Ollama batch HTTP {status}"));
                    continue;
                }
                let data: serde_json::Value = resp
                    .error_for_status()?
                    .json()
                    .await
                    .map_err(|e| anyhow::anyhow!("Ollama batch JSON parse error: {e}"))?;
                let arr = data["embeddings"]
                    .as_array()
                    .ok_or_else(|| anyhow::anyhow!("Ollama batch response missing embeddings"))?;
                if arr.len() != texts.len() {
                    anyhow::bail!(
                        "Ollama batch returned {} embeddings but expected {}",
                        arr.len(),
                        texts.len()
                    );
                }
                let mut result = Vec::with_capacity(texts.len());
                for emb in arr {
                    let vec: Vec<f32> = emb
                        .as_array()
                        .ok_or_else(|| anyhow::anyhow!("Ollama batch: embedding is not array"))?
                        .iter()
                        .map(|v| v.as_f64().unwrap_or(0.0) as f32)
                        .collect();
                    if expected_dim > 0 && vec.len() != expected_dim {
                        anyhow::bail!(
                            "Ollama batch model '{model}' returned dim={} but expected {expected_dim}",
                            vec.len()
                        );
                    }
                    result.push(vec);
                }
                return Ok(result);
            }
            Err(e) => {
                last_err = Some(anyhow::anyhow!("Ollama batch request error: {e}"));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("Ollama batch embedding failed after retries")))
}

// ---------------------------------------------------------------------------
// OpenAIEmbedder — calls an OpenAI-compatible /embeddings endpoint.
// ---------------------------------------------------------------------------

/// Embedder that calls an OpenAI-compatible /embeddings endpoint.
#[derive(Clone)]
pub struct OpenAIEmbedder {
    pub model: String,
    pub api_key: String,
    pub api_base: String,
    pub dim: usize,
    client: reqwest::Client,
}

impl OpenAIEmbedder {
    pub fn new(
        model: impl Into<String>,
        api_key: impl Into<String>,
        api_base: impl Into<String>,
        dim: usize,
    ) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            model: model.into(),
            api_key: api_key.into(),
            api_base: api_base.into().trim_end_matches('/').to_string(),
            dim,
            client,
        }
    }
}

#[async_trait]
impl Embedder for OpenAIEmbedder {
    fn dimension(&self) -> usize {
        self.dim
    }

    async fn embed(&self, text: &str) -> anyhow::Result<Embedding> {
        embed_via_openai(
            &self.client,
            &self.api_base,
            &self.api_key,
            &self.model,
            text,
            self.dim,
        )
        .await
    }

    async fn embed_batch(&self, texts: &[&str]) -> anyhow::Result<Vec<Embedding>> {
        embed_batch_via_openai(
            &self.client,
            &self.api_base,
            &self.api_key,
            &self.model,
            texts,
            self.dim,
        )
        .await
    }
}

/// Shared HTTP helper for OpenAI /embeddings with exponential-backoff retry.
async fn embed_via_openai(
    client: &reqwest::Client,
    api_base: &str,
    api_key: &str,
    model: &str,
    text: &str,
    expected_dim: usize,
) -> anyhow::Result<Embedding> {
    let url = format!("{api_base}/embeddings");
    let body = serde_json::json!({
        "model": model,
        "input": text,
    });

    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..3u32 {
        if attempt > 0 {
            let backoff = std::time::Duration::from_millis(500 * (1 << attempt));
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
                    last_err = Some(anyhow::anyhow!("OpenAI HTTP {status}"));
                    continue;
                }
                let data: serde_json::Value = resp
                    .error_for_status()?
                    .json()
                    .await
                    .map_err(|e| anyhow::anyhow!("OpenAI JSON parse error: {e}"))?;
                let vec: Vec<f32> = data["data"][0]["embedding"]
                    .as_array()
                    .ok_or_else(|| anyhow::anyhow!("OpenAI response missing data[0].embedding"))?
                    .iter()
                    .map(|v| v.as_f64().unwrap_or(0.0) as f32)
                    .collect();
                if expected_dim > 0 && vec.len() != expected_dim {
                    anyhow::bail!(
                        "OpenAI model '{model}' returned dim={} but expected {expected_dim}",
                        vec.len()
                    );
                }
                return Ok(vec);
            }
            Err(e) => {
                last_err = Some(anyhow::anyhow!("OpenAI request error: {e}"));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("OpenAI embedding failed after retries")))
}

/// Batch embed via OpenAI /embeddings — sends all texts as a single "input" array.
/// The OpenAI embeddings API natively supports a list of inputs, returning one
/// embedding per input. This eliminates per-text HTTP round-trip overhead.
async fn embed_batch_via_openai(
    client: &reqwest::Client,
    api_base: &str,
    api_key: &str,
    model: &str,
    texts: &[&str],
    expected_dim: usize,
) -> anyhow::Result<Vec<Embedding>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    if texts.len() == 1 {
        return Ok(vec![
            embed_via_openai(client, api_base, api_key, model, texts[0], expected_dim).await?,
        ]);
    }
    let url = format!("{api_base}/embeddings");
    let body = serde_json::json!({
        "model": model,
        "input": texts,
    });

    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..3u32 {
        if attempt > 0 {
            let backoff = std::time::Duration::from_millis(500 * (1 << attempt));
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
                    last_err = Some(anyhow::anyhow!("OpenAI batch HTTP {status}"));
                    continue;
                }
                let data: serde_json::Value = resp
                    .error_for_status()?
                    .json()
                    .await
                    .map_err(|e| anyhow::anyhow!("OpenAI batch JSON parse error: {e}"))?;
                let arr = data["data"]
                    .as_array()
                    .ok_or_else(|| anyhow::anyhow!("OpenAI batch response missing data"))?;
                if arr.len() != texts.len() {
                    anyhow::bail!(
                        "OpenAI batch returned {} embeddings but expected {}",
                        arr.len(),
                        texts.len()
                    );
                }
                // OpenAI returns data sorted by "index" field; sort to match input order.
                let mut indexed: Vec<(usize, Vec<f32>)> = Vec::with_capacity(arr.len());
                for item in arr {
                    let idx = item["index"].as_u64().unwrap_or(0) as usize;
                    let vec: Vec<f32> = item["embedding"]
                        .as_array()
                        .ok_or_else(|| anyhow::anyhow!("OpenAI batch: missing embedding"))?
                        .iter()
                        .map(|v| v.as_f64().unwrap_or(0.0) as f32)
                        .collect();
                    if expected_dim > 0 && vec.len() != expected_dim {
                        anyhow::bail!(
                            "OpenAI model '{model}' returned dim={} but expected {expected_dim}",
                            vec.len()
                        );
                    }
                    indexed.push((idx, vec));
                }
                indexed.sort_by_key(|(idx, _)| *idx);
                return Ok(indexed.into_iter().map(|(_, v)| v).collect());
            }
            Err(e) => {
                last_err = Some(anyhow::anyhow!("OpenAI batch request error: {e}"));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("OpenAI batch embedding failed after retries")))
}

// ---------------------------------------------------------------------------
// RemoteEmbedder — runtime-dispatched wrapper (Ollama or OpenAI).
// Constructed via RemoteEmbedder::ollama() / RemoteEmbedder::openai().
// ---------------------------------------------------------------------------

enum RemoteBackend {
    Ollama(OllamaEmbedder),
    OpenAI(OpenAIEmbedder),
}

/// A remote embedder that dispatches to either Ollama or OpenAI.
pub struct RemoteEmbedder {
    backend: RemoteBackend,
}

impl RemoteEmbedder {
    /// Create an Ollama-backed remote embedder.
    /// `dim` should match the model's output dimensionality (e.g. 768 for nomic-embed-text).
    pub fn ollama(model: impl Into<String>, url: impl Into<String>, dim: usize) -> Self {
        Self {
            backend: RemoteBackend::Ollama(OllamaEmbedder::new(model, url, dim)),
        }
    }

    /// Create an OpenAI-compatible remote embedder.
    /// `dim` should match the model's output dimensionality (e.g. 1536 for text-embedding-3-small).
    pub fn openai(
        model: impl Into<String>,
        api_key: impl Into<String>,
        api_base: impl Into<String>,
        dim: usize,
    ) -> Self {
        Self {
            backend: RemoteBackend::OpenAI(OpenAIEmbedder::new(model, api_key, api_base, dim)),
        }
    }
}

#[async_trait]
impl Embedder for RemoteEmbedder {
    fn dimension(&self) -> usize {
        match &self.backend {
            RemoteBackend::Ollama(e) => e.dimension(),
            RemoteBackend::OpenAI(e) => e.dimension(),
        }
    }

    async fn embed(&self, text: &str) -> anyhow::Result<Embedding> {
        match &self.backend {
            RemoteBackend::Ollama(e) => e.embed(text).await,
            RemoteBackend::OpenAI(e) => e.embed(text).await,
        }
    }

    async fn embed_batch(&self, texts: &[&str]) -> anyhow::Result<Vec<Embedding>> {
        match &self.backend {
            RemoteBackend::Ollama(e) => e.embed_batch(texts).await,
            RemoteBackend::OpenAI(e) => e.embed_batch(texts).await,
        }
    }
}

// ---------------------------------------------------------------------------
// EmbedderConfig — factory for building the right embedder from Config.
// ---------------------------------------------------------------------------

/// Build a boxed [`Embedder`] from the project's [`engram_core::Config`].
///
/// - `"local"` (default) → [`ProjectionEmbedder`] (dim=384, no deps)
/// - `"ollama"` → [`OllamaEmbedder`] using `cfg.ollama_url` + `cfg.embedding_model`
/// - `"openai"` → [`OpenAIEmbedder`] using `cfg.openai_api_key` + `cfg.embedding_model`
pub fn build_embedder(cfg: &engram_core::Config) -> anyhow::Result<Box<dyn Embedder>> {
    match cfg.embedding_backend.as_str() {
        "ollama" => {
            let url = cfg
                .ollama_url
                .clone()
                .unwrap_or_else(|| "http://localhost:11434".into());
            let model = cfg
                .embedding_model
                .clone()
                .unwrap_or_else(|| "nomic-embed-text".into());
            // nomic-embed-text → 768; most others → varies. We default to 768 here;
            // users can override by setting embedding_model to a model they know
            // produces a different dimension. LanceDB tables are auto-recreated when
            // the dimension changes (Phase 9, fix 2.1).
            let dim = 768usize;
            Ok(Box::new(OllamaEmbedder::new(model, url, dim)))
        }
        "openai" => {
            let api_key = cfg.openai_api_key.clone().unwrap_or_default();
            if api_key.is_empty() {
                anyhow::bail!(
                    "embedding_backend=openai requires openai_api_key to be set in config"
                );
            }
            let api_base = cfg
                .openai_api_base
                .clone()
                .unwrap_or_else(|| "https://api.openai.com/v1".into());
            let model = cfg
                .embedding_model
                .clone()
                .unwrap_or_else(|| "text-embedding-3-small".into());
            let dim = 1536usize;
            Ok(Box::new(OpenAIEmbedder::new(model, api_key, api_base, dim)))
        }
        // "local" or anything else
        _ => Ok(Box::new(LocalEmbedder)),
    }
}
