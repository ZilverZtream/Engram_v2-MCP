use async_trait::async_trait;
use blake3;

pub type Embedding = Vec<f32>;

#[async_trait]
pub trait Embedder: Send + Sync {
    async fn embed(&self, text: &str) -> anyhow::Result<Embedding>;
    fn dimension(&self) -> usize;
}

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
        let mut vec = vec![0.0f32; self.dim];
        if text.is_empty() {
            return Ok(vec);
        }

        // Tokenize into trigrams
        let chars: Vec<char> = text.chars().collect();
        if chars.len() < 3 {
            // Fallback for very short text
            let h = blake3::hash(text.as_bytes());
            let idx =
                (u32::from_le_bytes(h.as_bytes()[0..4].try_into().unwrap()) as usize) % self.dim;
            vec[idx] = 1.0;
            return Ok(vec);
        }

        for i in 0..chars.len() - 2 {
            let trigram: String = chars[i..i + 3].iter().collect();
            let h = blake3::hash(trigram.as_bytes());
            let bytes = h.as_bytes();

            // Map each trigram to 2 indices with +1 or -1 (SimHash style projection)
            for j in 0..2 {
                let chunk = &bytes[j * 4..(j + 1) * 4];
                let val = u32::from_le_bytes(chunk.try_into().unwrap());
                let idx = (val as usize) % self.dim;
                let sign = if (val >> 31) == 0 { 1.0 } else { -1.0 };
                vec[idx] += sign;
            }
        }

        // L2 Normalize
        let norm = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in vec.iter_mut() {
                *x /= norm;
            }
        }

        Ok(vec)
    }
}

/// Placeholder local embedder (Candle / ONNX / etc.)
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

/// Placeholder remote embedder (Ollama/OpenAI/etc.)
#[derive(Clone)]
pub struct RemoteEmbedder;

#[async_trait]
impl Embedder for RemoteEmbedder {
    fn dimension(&self) -> usize {
        1536
    }
    async fn embed(&self, _text: &str) -> anyhow::Result<Embedding> {
        Err(anyhow::anyhow!(
            "RemoteEmbedder is not yet implemented. Please use 'local' or 'fts_only'."
        ))
    }
}
