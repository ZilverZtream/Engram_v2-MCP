use async_trait::async_trait;

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

    /// Embed multiple texts with cancellation support.
    /// Implementations should check `cancel.is_cancelled()` between batches/retries.
    /// Default impl delegates to embed_batch ignoring the token (sub-optimal but safe).
    async fn embed_batch_cancellable(
        &self,
        texts: &[&str],
        cancel: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<Vec<Embedding>> {
        if cancel.is_cancelled() {
            anyhow::bail!("embedding cancelled before start");
        }
        self.embed_batch(texts).await
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
        // Guard: dim=0 would cause integer divide-by-zero in the projection loop.
        if self.dim == 0 {
            anyhow::bail!("ProjectionEmbedder dim must be > 0 (got 0)");
        }
        // Fix #3: empty / whitespace-only text returns a stable unit vector anchored
        // at dimension 0 rather than an all-zero vector.  An all-zero vector causes
        // division-by-zero in cosine-similarity databases (LanceDB).
        if text.trim().is_empty() {
            let mut vec = vec![0.0f32; self.dim];
            vec[0] = 1.0;
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

    state.hash_one(data)
}

/// Hash family B — used for projection slot 1 (independent of A).
fn ahash_u64_fixed_b(data: &[u8]) -> u64 {
    let state = ahash::RandomState::with_seeds(
        0xdeadbabe_f00dcafe,
        0x01234567_89abcdef,
        0xfedcba98_76543210,
        0xc0ffee11_deadbeef,
    );

    state.hash_one(data)
}

/// Parse a JSON array into a Vec<f32>, failing closed on non-numeric elements.
/// ENG-AUD-2026-S12-0001: consistent with batch-path semantics.
fn parse_embedding_array(arr: &[serde_json::Value]) -> anyhow::Result<Vec<f32>> {
    arr.iter()
        .enumerate()
        .map(|(i, v)| {
            v.as_f64()
                .ok_or_else(|| anyhow::anyhow!(
                    "ENG-AUD-2026-S12-0001: non-numeric element at index {i}: {:?}", v
                ))
                .map(|f| f as f32)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests for ProjectionEmbedder
// ---------------------------------------------------------------------------

#[cfg(test)]
mod projection_tests {
    use super::*;

    /// Gate ENG-AUD-2026-0008: dim=0 must return Err, not panic.
    #[tokio::test]
    async fn projection_dim_zero_returns_err_not_panic() {
        let embedder = ProjectionEmbedder::new(0);
        let result = embedder.embed("hello world").await;
        assert!(
            result.is_err(),
            "embed() with dim=0 must return Err to avoid divide-by-zero"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("dim"),
            "error message should mention dim; got: {msg}"
        );
    }

    /// dim=0 with empty string must also return Err (not short-circuit to Ok).
    #[tokio::test]
    async fn projection_dim_zero_empty_text_returns_err() {
        let embedder = ProjectionEmbedder::new(0);
        let result = embedder.embed("").await;
        assert!(
            result.is_err(),
            "embed() with dim=0 and empty text must return Err"
        );
    }

    /// Gate ENG-AUD-2026-0009: whitespace-only text must not panic and must
    /// return a unit vector (not all-zeros, which would break cosine similarity).
    #[tokio::test]
    async fn projection_whitespace_only_returns_unit_vector() {
        let embedder = ProjectionEmbedder::new(128);
        let result = embedder.embed("   \t\n  ").await.unwrap();
        assert_eq!(result.len(), 128, "must return vector of correct dimension");
        // Must not be all-zero (would cause cosine-similarity NaN in LanceDB).
        let norm: f32 = result.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            norm > 0.9,
            "whitespace-only text must produce a non-zero unit vector; norm={norm}"
        );
    }

    /// Truly empty string also returns a unit vector.
    #[tokio::test]
    async fn projection_empty_string_returns_unit_vector() {
        let embedder = ProjectionEmbedder::new(64);
        let result = embedder.embed("").await.unwrap();
        assert_eq!(result.len(), 64);
        let norm: f32 = result.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(norm > 0.9, "empty string must produce unit vector; norm={norm}");
    }

    /// Normal text produces a vector of the correct dimension, normalized.
    #[tokio::test]
    async fn projection_normal_text_correct_dim_and_normalized() {
        let embedder = ProjectionEmbedder::new(384);
        let result = embedder.embed("fn main() { println!(\"hello\"); }").await.unwrap();
        assert_eq!(result.len(), 384);
        let norm: f32 = result.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-4,
            "output must be L2-normalized; got norm={norm}"
        );
    }

    /// Embeddings must be deterministic (fixed seeds).
    #[tokio::test]
    async fn projection_is_deterministic() {
        let embedder = ProjectionEmbedder::new(256);
        let a = embedder.embed("deterministic test").await.unwrap();
        let b = embedder.embed("deterministic test").await.unwrap();
        assert_eq!(a, b, "ProjectionEmbedder must produce identical output across calls");
    }
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
    ///
    /// Returns `Err` if the HTTP client cannot be built (ENG-AUD-2026-0007).
    pub fn new(model: impl Into<String>, url: impl Into<String>, dim: usize) -> anyhow::Result<Self> {
        // EMB3: reject dim=0 at construction time; hybrid.rs also catches this but
        // fail-fast here avoids deferred surprises.
        if dim == 0 {
            anyhow::bail!("EMB-AUD: embedder dim must be > 0 (got 0); check ollama_embed_dim/openai_embed_dim config");
        }
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| anyhow::anyhow!("ENG-AUD-2026-0007: failed to build Ollama HTTP client: {e}"))?;
        Ok(Self {
            model: model.into(),
            url: url.into().trim_end_matches('/').to_string(),
            dim,
            client,
        })
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

    /// EMB1: cancellable batch embed — checks cancel before sending and between retries.
    async fn embed_batch_cancellable(
        &self,
        texts: &[&str],
        cancel: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<Vec<Embedding>> {
        if cancel.is_cancelled() {
            anyhow::bail!("embedding cancelled before start");
        }
        embed_batch_via_ollama_cancellable(&self.client, &self.url, &self.model, texts, self.dim, cancel).await
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
    // EMB2: normalize empty/whitespace text to unit vector matching ProjectionEmbedder behavior
    if text.trim().is_empty() {
        if expected_dim == 0 {
            anyhow::bail!("cannot embed empty text with dim=0");
        }
        let mut v = vec![0.0f32; expected_dim];
        v[0] = 1.0;
        return Ok(v);
    }
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
                let arr = data["embeddings"][0]
                    .as_array()
                    .ok_or_else(|| anyhow::anyhow!("Ollama response missing embeddings[0]"))?;
                let vec = parse_embedding_array(arr)?;
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
                    let emb_arr = emb
                        .as_array()
                        .ok_or_else(|| anyhow::anyhow!("Ollama batch: embedding is not array"))?;
                    let vec = parse_embedding_array(emb_arr)?;
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

/// EMB1: cancellable variant of embed_batch_via_ollama — checks cancel before each retry sleep.
async fn embed_batch_via_ollama_cancellable(
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
    texts: &[&str],
    expected_dim: usize,
    cancel: &tokio_util::sync::CancellationToken,
) -> anyhow::Result<Vec<Embedding>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    if texts.len() == 1 {
        if cancel.is_cancelled() {
            anyhow::bail!("embedding cancelled before request");
        }
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
        if cancel.is_cancelled() {
            anyhow::bail!("embedding cancelled during retry loop");
        }
        if attempt > 0 {
            let backoff = std::time::Duration::from_millis(500 * (1 << attempt));
            tokio::time::sleep(backoff).await;
            if cancel.is_cancelled() {
                anyhow::bail!("embedding cancelled after backoff sleep");
            }
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
                    let emb_arr = emb
                        .as_array()
                        .ok_or_else(|| anyhow::anyhow!("Ollama batch: embedding is not array"))?;
                    let vec = parse_embedding_array(emb_arr)?;
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
    /// Returns `Err` if the HTTP client cannot be built (ENG-AUD-2026-0007).
    pub fn new(
        model: impl Into<String>,
        api_key: impl Into<String>,
        api_base: impl Into<String>,
        dim: usize,
    ) -> anyhow::Result<Self> {
        // EMB3: reject dim=0 at construction time; fail-fast before any HTTP call.
        if dim == 0 {
            anyhow::bail!("EMB-AUD: embedder dim must be > 0 (got 0); check ollama_embed_dim/openai_embed_dim config");
        }
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| anyhow::anyhow!("ENG-AUD-2026-0007: failed to build OpenAI HTTP client: {e}"))?;
        Ok(Self {
            model: model.into(),
            api_key: api_key.into(),
            api_base: api_base.into().trim_end_matches('/').to_string(),
            dim,
            client,
        })
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

    /// EMB1: cancellable batch embed — checks cancel before sending and between retries.
    async fn embed_batch_cancellable(
        &self,
        texts: &[&str],
        cancel: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<Vec<Embedding>> {
        if cancel.is_cancelled() {
            anyhow::bail!("embedding cancelled before start");
        }
        embed_batch_via_openai_cancellable(
            &self.client,
            &self.api_base,
            &self.api_key,
            &self.model,
            texts,
            self.dim,
            cancel,
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
    // EMB2: normalize empty/whitespace text to unit vector matching ProjectionEmbedder behavior
    if text.trim().is_empty() {
        if expected_dim == 0 {
            anyhow::bail!("cannot embed empty text with dim=0");
        }
        let mut v = vec![0.0f32; expected_dim];
        v[0] = 1.0;
        return Ok(v);
    }
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
                let arr = data["data"][0]["embedding"]
                    .as_array()
                    .ok_or_else(|| anyhow::anyhow!("OpenAI response missing data[0].embedding"))?;
                let vec = parse_embedding_array(arr)?;
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
                    let idx = item["index"]
                        .as_u64()
                        .ok_or_else(|| anyhow::anyhow!("OpenAI batch: item missing or invalid 'index' field"))?
                        as usize;
                    let vec: Vec<f32> = item["embedding"]
                        .as_array()
                        .ok_or_else(|| anyhow::anyhow!("OpenAI batch: missing embedding"))?
                        .iter()
                        .map(|v| {
                            v.as_f64()
                                .ok_or_else(|| anyhow::anyhow!("OpenAI batch: non-numeric embedding element"))
                                .map(|f| f as f32)
                        })
                        .collect::<Result<Vec<f32>, _>>()?;
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

/// EMB1: cancellable variant of embed_batch_via_openai — checks cancel before each retry sleep.
async fn embed_batch_via_openai_cancellable(
    client: &reqwest::Client,
    api_base: &str,
    api_key: &str,
    model: &str,
    texts: &[&str],
    expected_dim: usize,
    cancel: &tokio_util::sync::CancellationToken,
) -> anyhow::Result<Vec<Embedding>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    if texts.len() == 1 {
        if cancel.is_cancelled() {
            anyhow::bail!("embedding cancelled before request");
        }
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
        if cancel.is_cancelled() {
            anyhow::bail!("embedding cancelled during retry loop");
        }
        if attempt > 0 {
            let backoff = std::time::Duration::from_millis(500 * (1 << attempt));
            tokio::time::sleep(backoff).await;
            if cancel.is_cancelled() {
                anyhow::bail!("embedding cancelled after backoff sleep");
            }
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
                let mut indexed: Vec<(usize, Vec<f32>)> = Vec::with_capacity(arr.len());
                for item in arr {
                    let idx = item["index"]
                        .as_u64()
                        .ok_or_else(|| anyhow::anyhow!("OpenAI batch: item missing 'index' field"))?
                        as usize;
                    let vec: Vec<f32> = item["embedding"]
                        .as_array()
                        .ok_or_else(|| anyhow::anyhow!("OpenAI batch: missing embedding"))?
                        .iter()
                        .map(|v| {
                            v.as_f64()
                                .ok_or_else(|| anyhow::anyhow!("OpenAI batch: non-numeric element"))
                                .map(|f| f as f32)
                        })
                        .collect::<Result<Vec<f32>, _>>()?;
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
    /// Returns `Err` if the HTTP client cannot be built (ENG-AUD-2026-0007).
    pub fn ollama(model: impl Into<String>, url: impl Into<String>, dim: usize) -> anyhow::Result<Self> {
        Ok(Self {
            backend: RemoteBackend::Ollama(OllamaEmbedder::new(model, url, dim)?),
        })
    }

    /// Create an OpenAI-compatible remote embedder.
    /// `dim` should match the model's output dimensionality (e.g. 1536 for text-embedding-3-small).
    /// Returns `Err` if the HTTP client cannot be built (ENG-AUD-2026-0007).
    pub fn openai(
        model: impl Into<String>,
        api_key: impl Into<String>,
        api_base: impl Into<String>,
        dim: usize,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            backend: RemoteBackend::OpenAI(OpenAIEmbedder::new(model, api_key, api_base, dim)?),
        })
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

    /// EMB1: dispatch cancellable batch to the correct backend.
    async fn embed_batch_cancellable(
        &self,
        texts: &[&str],
        cancel: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<Vec<Embedding>> {
        match &self.backend {
            RemoteBackend::Ollama(e) => e.embed_batch_cancellable(texts, cancel).await,
            RemoteBackend::OpenAI(e) => e.embed_batch_cancellable(texts, cancel).await,
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
            // ENG-AUD-2026-S12-0004: use the operator-configured dimension so that
            // models whose output size differs from nomic-embed-text (768) don't
            // produce silent dimension-mismatch failures at embed time.
            // Operator sets ollama_embed_dim in engram_mcp.yaml; we fall back to
            // 768 only when the field is absent for backward compatibility.
            let dim = cfg.ollama_embed_dim.unwrap_or(768);
            Ok(Box::new(OllamaEmbedder::new(model, url, dim)?))
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
            // ENG-AUD-2026-S12-0004: use operator-configured dimension.
            // Defaults to 1536 (text-embedding-3-small) for backward compatibility.
            let dim = cfg.openai_embed_dim.unwrap_or(1536);
            Ok(Box::new(OpenAIEmbedder::new(model, api_key, api_base, dim)?))
        }
        // Known local-mode backends that all resolve to LocalEmbedder.
        "local" | "candle" | "fts_only" => Ok(Box::new(LocalEmbedder)),
        _ => anyhow::bail!(
            "unknown embedding_backend '{}': must be one of: local, candle, fts_only, ollama, openai",
            cfg.embedding_backend
        ),
    }
}

#[cfg(test)]
mod embed_factory_tests {
    use super::*;

    fn make_cfg(backend: &str) -> engram_core::Config {
        engram_core::Config {
            embedding_backend: backend.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn build_embedder_rejects_unknown_backend() {
        let cfg = make_cfg("bad_backend_xyz");
        let result = build_embedder(&cfg);
        assert!(result.is_err(), "build_embedder must reject unknown backend");
        // Map to the error string without requiring Debug on Box<dyn Embedder>.
        let msg = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(
            msg.contains("unknown"),
            "error message should contain 'unknown', got: {msg}"
        );
    }

    #[test]
    fn build_embedder_accepts_known_backends() {
        for backend in &["local", "candle", "fts_only"] {
            let cfg = make_cfg(backend);
            let result = build_embedder(&cfg);
            let err_msg = result.err().map(|e| e.to_string()).unwrap_or_default();
            assert!(
                err_msg.is_empty(),
                "build_embedder must accept known backend '{}', got error: {}",
                backend,
                err_msg
            );
        }
    }

    /// ENG-AUD-2026-S12-0004: when ollama_embed_dim is set the embedder must
    /// report that exact dimension, not the hardcoded 768 fallback.
    #[test]
    fn ollama_config_uses_configured_dimension() {
        // ENG-AUD-2026-S12-0004
        let cfg = engram_core::Config {
            embedding_backend: "ollama".to_string(),
            ollama_embed_dim: Some(1024),
            ..Default::default()
        };
        let embedder = build_embedder(&cfg).expect("build_embedder must succeed for ollama");
        assert_eq!(
            embedder.dimension(),
            1024,
            "ENG-AUD-2026-S12-0004: embedder dimension must match ollama_embed_dim=1024"
        );
    }

    /// ENG-AUD-2026-S12-0004: when ollama_embed_dim is absent the embedder must
    /// fall back to 768 for backward compatibility.
    #[test]
    fn ollama_config_defaults_to_768_when_unset() {
        // ENG-AUD-2026-S12-0004
        let cfg = engram_core::Config {
            embedding_backend: "ollama".to_string(),
            ollama_embed_dim: None,
            ..Default::default()
        };
        let embedder = build_embedder(&cfg).expect("build_embedder must succeed for ollama");
        assert_eq!(
            embedder.dimension(),
            768,
            "ENG-AUD-2026-S12-0004: embedder must default to dim=768 when ollama_embed_dim is not set"
        );
    }

    /// ENG-AUD-2026-S12-0004: when openai_embed_dim is set the embedder must
    /// report that exact dimension, not the hardcoded 1536 fallback.
    #[test]
    fn openai_config_uses_configured_dimension() {
        // ENG-AUD-2026-S12-0004
        let cfg = engram_core::Config {
            embedding_backend: "openai".to_string(),
            openai_api_key: Some("test-key".to_string()),
            openai_embed_dim: Some(3072),
            ..Default::default()
        };
        let embedder = build_embedder(&cfg).expect("build_embedder must succeed for openai");
        assert_eq!(
            embedder.dimension(),
            3072,
            "ENG-AUD-2026-S12-0004: embedder dimension must match openai_embed_dim=3072"
        );
    }

    /// ENG-AUD-2026-S12-0004: when openai_embed_dim is absent the embedder must
    /// fall back to 1536 for backward compatibility.
    #[test]
    fn openai_config_defaults_to_1536_when_unset() {
        // ENG-AUD-2026-S12-0004
        let cfg = engram_core::Config {
            embedding_backend: "openai".to_string(),
            openai_api_key: Some("test-key".to_string()),
            openai_embed_dim: None,
            ..Default::default()
        };
        let embedder = build_embedder(&cfg).expect("build_embedder must succeed for openai");
        assert_eq!(
            embedder.dimension(),
            1536,
            "ENG-AUD-2026-S12-0004: embedder must default to dim=1536 when openai_embed_dim is not set"
        );
    }

    /// EMB3: build_embedder with ollama_embed_dim=0 must fail at construction time
    /// so misconfiguration is detected at startup, not deferred to first embed call.
    #[test]
    fn build_embedder_ollama_rejects_zero_dim() {
        let cfg = engram_core::Config {
            embedding_backend: "ollama".to_string(),
            ollama_embed_dim: Some(0),
            ..Default::default()
        };
        let result = build_embedder(&cfg);
        assert!(
            result.is_err(),
            "EMB3: build_embedder must reject ollama_embed_dim=0 at construction time"
        );
        let msg = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(
            msg.contains("dim") || msg.contains("EMB-AUD"),
            "error must mention dim config; got: {msg}"
        );
    }

    /// EMB3: build_embedder with openai_embed_dim=0 must fail at construction time.
    #[test]
    fn build_embedder_openai_rejects_zero_dim() {
        let cfg = engram_core::Config {
            embedding_backend: "openai".to_string(),
            openai_api_key: Some("test-key".to_string()),
            openai_embed_dim: Some(0),
            ..Default::default()
        };
        let result = build_embedder(&cfg);
        assert!(
            result.is_err(),
            "EMB3: build_embedder must reject openai_embed_dim=0 at construction time"
        );
        let msg = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(
            msg.contains("dim") || msg.contains("EMB-AUD"),
            "error must mention dim config; got: {msg}"
        );
    }

    /// EMB1: embed_batch_cancellable called with a pre-cancelled token must return Err
    /// immediately without making any HTTP requests.
    #[tokio::test]
    async fn ollama_embed_batch_cancellable_respects_pre_cancel() {
        let embedder = OllamaEmbedder::new("nomic-embed-text", "http://127.0.0.1:19999", 768)
            .expect("OllamaEmbedder::new must succeed with valid params");
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        let result = embedder.embed_batch_cancellable(&["hello"], &cancel).await;
        assert!(
            result.is_err(),
            "EMB1: embed_batch_cancellable must return Err when token is pre-cancelled"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("cancel"),
            "error must mention cancellation; got: {msg}"
        );
    }

    /// EMB1: embed_batch_cancellable on OpenAI with pre-cancelled token must return Err immediately.
    #[tokio::test]
    async fn openai_embed_batch_cancellable_respects_pre_cancel() {
        let embedder = OpenAIEmbedder::new(
            "text-embedding-3-small",
            "test-api-key",
            "http://127.0.0.1:19999/v1",
            1536,
        )
        .expect("OpenAIEmbedder::new must succeed with valid params");
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        let result = embedder.embed_batch_cancellable(&["hello"], &cancel).await;
        assert!(
            result.is_err(),
            "EMB1: openai embed_batch_cancellable must return Err when token is pre-cancelled"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("cancel"),
            "error must mention cancellation; got: {msg}"
        );
    }
}

#[cfg(test)]
mod provider_parity_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Spawn a minimal HTTP server that accepts one connection and returns a canned response.
    /// Returns the ephemeral port it is listening on, plus a JoinHandle.
    async fn mock_http_once(response_body: &str) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = response_body.to_string();
        let handle = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 8192];
                let _ = stream.read(&mut buf).await;
                let http_response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(http_response.as_bytes()).await;
            }
        });
        (port, handle)
    }

    /// Mock server that returns an HTTP error status for one connection.
    async fn mock_http_error_once(status: u16) -> (u16, tokio::task::JoinHandle<()>) {
        mock_http_error_n(status, 1).await
    }

    /// Mock server that returns an HTTP error status for `n` consecutive connections.
    /// Use this when the caller retries — otherwise the retry hangs on a closed port.
    async fn mock_http_error_n(status: u16, n: usize) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            for _ in 0..n {
                if let Ok((mut stream, _)) = listener.accept().await {
                    let mut buf = vec![0u8; 8192];
                    let _ = stream.read(&mut buf).await;
                    let resp = format!(
                        "HTTP/1.1 {status} Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                }
            }
        });
        (port, handle)
    }

    // ── OpenAI provider ──────────────────────────────────────────────────

    /// S1-0007: Verifies the OpenAI response schema `data[0].embedding` is correctly parsed.
    #[tokio::test]
    async fn openai_valid_response_schema_parsed() {
        let embedding: Vec<f32> = vec![0.1, 0.2, 0.3, 0.4];
        let body = serde_json::json!({
            "data": [{"embedding": embedding, "index": 0}],
            "model": "text-embedding-3-small",
            "usage": {"prompt_tokens": 1, "total_tokens": 1}
        })
        .to_string();

        let (port, _handle) = mock_http_once(&body).await;
        let url = format!("http://127.0.0.1:{port}/v1");
        let embedder = OpenAIEmbedder::new("text-embedding-3-small", "test-key", url, 4)
            .expect("OpenAIEmbedder::new must succeed in test environment");
        let result = embedder.embed("hello").await;
        assert!(result.is_ok(), "valid OpenAI response must parse correctly: {:?}", result);
        let vec = result.unwrap();
        assert_eq!(vec.len(), 4, "embedding dimension must match");
        assert!((vec[0] - 0.1_f32).abs() < 1e-5);
    }

    /// S1-0007: OpenAI schema mismatch — wrong field name `vectors` instead of `embedding`.
    #[tokio::test]
    async fn openai_schema_mismatch_returns_error() {
        let body = serde_json::json!({
            "data": [{"vectors": [0.1, 0.2, 0.3], "index": 0}]
        })
        .to_string();

        let (port, _handle) = mock_http_once(&body).await;
        let url = format!("http://127.0.0.1:{port}/v1");
        let embedder = OpenAIEmbedder::new("text-embedding-3-small", "test-key", url, 3)
            .expect("OpenAIEmbedder::new must succeed in test environment");
        let result = embedder.embed("hello").await;
        assert!(result.is_err(), "missing data[0].embedding must return error");
        let msg = format!("{:?}", result.unwrap_err());
        assert!(
            msg.contains("missing") || msg.contains("embedding"),
            "error message must mention missing embedding field, got: {msg}"
        );
    }

    /// S1-0007: OpenAI dimension mismatch is rejected.
    #[tokio::test]
    async fn openai_dimension_mismatch_returns_error() {
        let body = serde_json::json!({
            "data": [{"embedding": [0.1, 0.2, 0.3], "index": 0}]  // dim=3
        })
        .to_string();

        let (port, _handle) = mock_http_once(&body).await;
        let url = format!("http://127.0.0.1:{port}/v1");
        let embedder = OpenAIEmbedder::new("text-embedding-3-small", "test-key", url, 1536) // expects 1536
            .expect("OpenAIEmbedder::new must succeed in test environment");
        let result = embedder.embed("hello").await;
        assert!(result.is_err(), "dimension mismatch must return error");
        let msg = format!("{:?}", result.unwrap_err());
        assert!(
            msg.contains("dim") || msg.contains("dimension") || msg.contains("1536") || msg.contains("expected"),
            "error must mention dimension, got: {msg}"
        );
    }

    /// S1-0007: OpenAI 4xx client error is returned as Err.
    #[tokio::test]
    async fn openai_client_error_status_returns_error() {
        let (port, _handle) = mock_http_error_once(401).await;
        let url = format!("http://127.0.0.1:{port}/v1");
        let embedder = OpenAIEmbedder::new("text-embedding-3-small", "test-key", url, 3)
            .expect("OpenAIEmbedder::new must succeed in test environment");
        let result = embedder.embed("hello").await;
        assert!(result.is_err(), "HTTP 401 must return Err, not empty embedding");
    }

    // ── Ollama provider ───────────────────────────────────────────────────

    /// S1-0007: Verifies the Ollama response schema `embeddings[0]` is correctly parsed.
    #[tokio::test]
    async fn ollama_valid_response_schema_parsed() {
        let embedding: Vec<f32> = vec![0.5, 0.6, 0.7];
        let body = serde_json::json!({
            "embeddings": [embedding],
            "model": "nomic-embed-text"
        })
        .to_string();

        let (port, _handle) = mock_http_once(&body).await;
        let url = format!("http://127.0.0.1:{port}");
        let embedder = OllamaEmbedder::new("nomic-embed-text", url, 3)
            .expect("OllamaEmbedder::new must succeed in test environment");
        let result = embedder.embed("hello").await;
        assert!(result.is_ok(), "valid Ollama response must parse correctly: {:?}", result);
        let vec = result.unwrap();
        assert_eq!(vec.len(), 3);
        assert!((vec[0] - 0.5_f32).abs() < 1e-5);
    }

    /// S1-0007: Ollama schema mismatch — wrong field name `vectors` instead of `embeddings`.
    #[tokio::test]
    async fn ollama_schema_mismatch_returns_error() {
        let body = serde_json::json!({
            "vectors": [[0.1, 0.2, 0.3]]
        })
        .to_string();

        let (port, _handle) = mock_http_once(&body).await;
        let url = format!("http://127.0.0.1:{port}");
        let embedder = OllamaEmbedder::new("nomic-embed-text", url, 3)
            .expect("OllamaEmbedder::new must succeed in test environment");
        let result = embedder.embed("hello").await;
        assert!(result.is_err(), "missing embeddings[0] must return error");
    }

    /// S1-0007: Ollama dimension mismatch is rejected.
    #[tokio::test]
    async fn ollama_dimension_mismatch_returns_error() {
        let body = serde_json::json!({
            "embeddings": [[0.1, 0.2, 0.3]]  // dim=3
        })
        .to_string();

        let (port, _handle) = mock_http_once(&body).await;
        let url = format!("http://127.0.0.1:{port}");
        let embedder = OllamaEmbedder::new("nomic-embed-text", url, 768) // expects 768
            .expect("OllamaEmbedder::new must succeed in test environment");
        let result = embedder.embed("hello").await;
        assert!(result.is_err(), "dimension mismatch (3 vs 768) must return error");
    }

    /// ENG-AUD-2026-EXH-0006: Ollama 5xx server error must return Err within
    /// a deterministic timeout — timeout is NOT an acceptable outcome.
    /// The mock services all 3 retry attempts (attempt 0 + 2 retries) so the
    /// retry loop can complete without hanging on a closed port.
    /// Backoff: 0s + 1s + 2s = 3s max; 10s timeout gives ample headroom.
    #[tokio::test]
    async fn ollama_server_error_returns_error() {
        // 3 connections = initial attempt + 2 retries (embed_via_ollama loops 0..3)
        let (port1, _h1) = mock_http_error_n(503, 3).await;
        let url = format!("http://127.0.0.1:{port1}");
        let embedder = OllamaEmbedder::new("nomic-embed-text", url, 3)
            .expect("OllamaEmbedder::new must succeed in test environment");
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            embedder.embed("hello"),
        )
        .await;
        match result {
            Ok(Err(_)) => {} // expected: error returned after all retries exhausted
            Ok(Ok(_)) => panic!("503 response must not produce a successful embedding"),
            Err(_timeout) => panic!(
                "ENG-AUD-2026-EXH-0006: Ollama 503 retry loop did not complete within 10 s — \
                 infinite backoff or hang regression detected"
            ),
        }
    }

    // ── RemoteEmbedder dispatch ────────────────────────────────────────────

    /// S1-0007: RemoteEmbedder::openai() dispatches to OpenAI schema.
    #[tokio::test]
    async fn remote_embedder_openai_path_dispatches_correctly() {
        let embedding = vec![0.1_f32, 0.2_f32];
        let body = serde_json::json!({
            "data": [{"embedding": embedding, "index": 0}]
        })
        .to_string();

        let (port, _handle) = mock_http_once(&body).await;
        let url = format!("http://127.0.0.1:{port}/v1");
        let embedder = RemoteEmbedder::openai("text-embedding-3-small", "test-key", url, 2)
            .expect("RemoteEmbedder::openai must succeed in test environment");
        let result = embedder.embed("test").await;
        assert!(result.is_ok(), "RemoteEmbedder::openai must parse OpenAI schema: {:?}", result);
        assert_eq!(result.unwrap().len(), 2);
    }

    /// S1-0007: RemoteEmbedder::ollama() dispatches to Ollama schema.
    #[tokio::test]
    async fn remote_embedder_ollama_path_dispatches_correctly() {
        let embedding = vec![0.3_f32, 0.4_f32];
        let body = serde_json::json!({
            "embeddings": [embedding]
        })
        .to_string();

        let (port, _handle) = mock_http_once(&body).await;
        let url = format!("http://127.0.0.1:{port}");
        let embedder = RemoteEmbedder::ollama("nomic-embed-text", url, 2)
            .expect("RemoteEmbedder::ollama must succeed in test environment");
        let result = embedder.embed("test").await;
        assert!(result.is_ok(), "RemoteEmbedder::ollama must parse Ollama schema: {:?}", result);
        assert_eq!(result.unwrap().len(), 2);
    }
}

#[cfg(test)]
mod audit_0007_tests {
    /// ENG-AUD-2026-0007: constructors must return Result, not infallible Self.
    /// This test compiles only if OllamaEmbedder::new returns Result.
    #[test]
    fn embedder_constructors_return_result() {
        let source = include_str!("embed.rs");
        assert!(
            source.contains("ENG-AUD-2026-0007"),
            "embed.rs must contain ENG-AUD-2026-0007 tag"
        );
        // Verify the map_err pattern (not unwrap_or_else fallback).
        // Split across variables so this assertion string doesn't itself trigger the check.
        let forbidden = ["unwrap_or_else", "|_|", "reqwest::Client::new()"];
        let has_fallback = source.lines().any(|line| {
            // Skip lines that are comments or part of this test's string literals.
            let trimmed = line.trim();
            !trimmed.starts_with("//")
                && !trimmed.starts_with("\"")
                && !trimmed.contains("forbidden")
                && forbidden.iter().all(|f| line.contains(f))
        });
        assert!(
            !has_fallback,
            "HTTP client must not fall back to default via unwrap_or_else — must propagate error"
        );
    }
}

// ---------------------------------------------------------------------------
// Tests for parse_embedding_array — ENG-AUD-2026-S12-0001 / S16-0001
// ---------------------------------------------------------------------------

#[cfg(test)]
mod parse_embedding_array_tests {
    use super::*;

    #[test]
    fn parse_embedding_array_rejects_null_element() {
        let arr = serde_json::json!([0.1, null, 0.3]);
        let result = parse_embedding_array(arr.as_array().unwrap());
        assert!(result.is_err(), "null element must return Err");
        assert!(result.unwrap_err().to_string().contains("non-numeric"));
    }

    #[test]
    fn parse_embedding_array_rejects_string_element() {
        let arr = serde_json::json!([0.1, "abc", 0.3]);
        let result = parse_embedding_array(arr.as_array().unwrap());
        assert!(result.is_err(), "string element must return Err");
    }

    #[test]
    fn parse_embedding_array_accepts_valid_floats() {
        let arr = serde_json::json!([0.1_f64, 0.2_f64, 0.3_f64]);
        let result = parse_embedding_array(arr.as_array().unwrap());
        assert!(result.is_ok());
        let v = result.unwrap();
        assert_eq!(v.len(), 3);
        assert!((v[0] - 0.1f32).abs() < 1e-5);
    }

    #[test]
    fn parse_embedding_array_rejects_boolean() {
        let arr = serde_json::json!([0.1, true, 0.3]);
        let result = parse_embedding_array(arr.as_array().unwrap());
        assert!(result.is_err(), "boolean element must return Err");
    }
}
