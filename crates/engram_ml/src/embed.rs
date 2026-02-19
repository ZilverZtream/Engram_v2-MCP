use ahash::AHasher;
use async_trait::async_trait;
use std::hash::{BuildHasher, Hash, Hasher};

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
