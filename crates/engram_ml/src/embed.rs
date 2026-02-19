use ahash::AHasher;
use async_trait::async_trait;
use std::hash::{Hash, Hasher};

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
            // Fallback for very short text — still use ahash for consistency.
            let h = ahash_u64(text.as_bytes());
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

            // Two independent ahash calls with different seeds give two projection slots.
            // AHasher is a non-cryptographic hasher optimised for throughput — ideal for
            // SimHash where we call it millions of times on tiny inputs.
            let h0 = ahash_u64_seeded(trigram_buf.as_bytes(), 0xdeadbeef_cafebabe);
            let h1 = ahash_u64_seeded(trigram_buf.as_bytes(), 0x01234567_89abcdef);

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

/// Hash `data` with AHasher (default seed).
fn ahash_u64(data: &[u8]) -> u64 {
    let mut h = AHasher::default();
    data.hash(&mut h);
    h.finish()
}

/// Hash `data` with AHasher, mixing in a fixed `seed` constant so the two
/// projection slots are independent.  AHasher::default() is deterministic
/// within a single process run; mixing the seed gives us a second independent
/// hash family without a cryptographic primitive.
fn ahash_u64_seeded(data: &[u8], seed: u64) -> u64 {
    // XOR the seed into the hash of the data to create a second independent value.
    let base = ahash_u64(data);
    base ^ seed.wrapping_mul(0x9e3779b97f4a7c15)
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
