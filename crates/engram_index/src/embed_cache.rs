//! Content-hash embedding cache (16b groundwork).
//!
//! Embedding is the dominant cost of indexing with a real model: a full
//! pilot-corpus reindex re-embeds 28k unchanged chunks (~30 min via Ollama), and
//! the incremental-update copy-forward path funnels unchanged docs back
//! through `index_docs`, re-embedding the whole corpus per update.
//!
//! `CachedEmbedder` wraps any [`engram_ml::Embedder`] with a redb table
//! keyed `(model_tag, blake3(text))` — shared across projects and project
//! generations, so delete+recreate cycles and copy-forward both become
//! cache hits. The cache file lives in the server data dir (one per
//! machine), opened once per process.

use engram_ml::{Embedder, Embedding};
use redb::{Database, TableDefinition};
use std::path::Path;
use std::sync::{Arc, OnceLock};

const EMBED_CACHE: TableDefinition<&str, &[u8]> = TableDefinition::new("embed_cache_v1");

/// One database handle per process: redb allows a single writer per file,
/// and every project's engine shares the same cache.
static CACHE_DB: OnceLock<anyhow::Result<Arc<Database>>> = OnceLock::new();

fn open_cache_db(path: &Path) -> anyhow::Result<Arc<Database>> {
    match CACHE_DB.get_or_init(|| {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Arc::new(Database::create(path)?))
    }) {
        Ok(db) => Ok(db.clone()),
        Err(e) => anyhow::bail!("embed cache open failed: {e:#}"),
    }
}

pub struct CachedEmbedder {
    inner: Arc<dyn Embedder>,
    db: Arc<Database>,
    tag: String,
}

impl CachedEmbedder {
    /// Wrap `inner` with the on-disk cache at `cache_path`. Errors if the
    /// cache database cannot be created/opened — callers may then fall back
    /// to the uncached embedder.
    pub fn new(inner: Arc<dyn Embedder>, cache_path: &Path) -> anyhow::Result<Self> {
        let db = open_cache_db(cache_path)?;
        let tag = inner.model_tag();
        Ok(Self { inner, db, tag })
    }

    fn key(&self, text: &str) -> String {
        format!("{}\0{}", self.tag, blake3::hash(text.as_bytes()).to_hex())
    }

    fn get_many(&self, keys: &[String]) -> Vec<Option<Embedding>> {
        let Ok(rtx) = self.db.begin_read() else {
            return vec![None; keys.len()];
        };
        let Ok(table) = rtx.open_table(EMBED_CACHE) else {
            // Table absent on a fresh cache file: every key is a miss.
            return vec![None; keys.len()];
        };
        keys.iter()
            .map(|k| {
                table.get(k.as_str()).ok().flatten().and_then(|v| {
                    let bytes = v.value();
                    if bytes.len() % 4 != 0 {
                        return None;
                    }
                    let dim = self.inner.dimension();
                    let vecf: Vec<f32> = bytes
                        .chunks_exact(4)
                        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                        .collect();
                    if vecf.len() == dim { Some(vecf) } else { None }
                })
            })
            .collect()
    }

    fn put_many(&self, entries: &[(String, &Embedding)]) {
        if entries.is_empty() {
            return;
        }
        // Best-effort: a failed cache write must never fail the embed call.
        let write = || -> anyhow::Result<()> {
            let wtx = self.db.begin_write()?;
            {
                let mut table = wtx.open_table(EMBED_CACHE)?;
                for (k, v) in entries {
                    let bytes: Vec<u8> = v.iter().flat_map(|f| f.to_le_bytes()).collect();
                    table.insert(k.as_str(), bytes.as_slice())?;
                }
            }
            wtx.commit()?;
            Ok(())
        };
        if let Err(e) = write() {
            tracing::warn!("embed cache write failed (continuing uncached): {e:#}");
        }
    }
}

#[async_trait::async_trait]
impl Embedder for CachedEmbedder {
    async fn embed(&self, text: &str) -> anyhow::Result<Embedding> {
        let key = self.key(text);
        if let Some(hit) = self.get_many(std::slice::from_ref(&key)).pop().flatten() {
            return Ok(hit);
        }
        let v = self.inner.embed(text).await?;
        self.put_many(&[(key, &v)]);
        Ok(v)
    }

    fn dimension(&self) -> usize {
        self.inner.dimension()
    }

    fn model_tag(&self) -> String {
        self.inner.model_tag()
    }

    async fn embed_batch(&self, texts: &[&str]) -> anyhow::Result<Vec<Embedding>> {
        self.embed_batch_cancellable(texts, &tokio_util::sync::CancellationToken::new())
            .await
    }

    async fn embed_batch_cancellable(
        &self,
        texts: &[&str],
        cancel: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<Vec<Embedding>> {
        let keys: Vec<String> = texts.iter().map(|t| self.key(t)).collect();
        let cached = self.get_many(&keys);

        let miss_idx: Vec<usize> = cached
            .iter()
            .enumerate()
            .filter_map(|(i, c)| c.is_none().then_some(i))
            .collect();
        if miss_idx.is_empty() {
            return Ok(cached.into_iter().map(|c| c.expect("all hits")).collect());
        }

        let miss_texts: Vec<&str> = miss_idx.iter().map(|&i| texts[i]).collect();
        let fresh = self
            .inner
            .embed_batch_cancellable(&miss_texts, cancel)
            .await?;
        anyhow::ensure!(
            fresh.len() == miss_texts.len(),
            "inner embedder returned {} vectors for {} texts",
            fresh.len(),
            miss_texts.len()
        );

        let entries: Vec<(String, &Embedding)> = miss_idx
            .iter()
            .zip(&fresh)
            .map(|(&i, v)| (keys[i].clone(), v))
            .collect();
        self.put_many(&entries);

        let mut fresh_iter = fresh.into_iter();
        Ok(cached
            .into_iter()
            .map(|c| match c {
                Some(v) => v,
                None => fresh_iter.next().expect("one fresh vector per miss"),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingEmbedder {
        calls: AtomicUsize,
        dim: usize,
    }

    #[async_trait::async_trait]
    impl Embedder for CountingEmbedder {
        async fn embed(&self, text: &str) -> anyhow::Result<Embedding> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut v = vec![0.0f32; self.dim];
            v[0] = text.len() as f32;
            Ok(v)
        }

        fn dimension(&self) -> usize {
            self.dim
        }

        fn model_tag(&self) -> String {
            "counting-test".into()
        }
    }

    fn cache_path() -> std::path::PathBuf {
        // OnceLock holds one DB per process: all tests share one file.
        std::env::temp_dir().join("engram_embed_cache_test.redb")
    }

    #[tokio::test]
    async fn second_batch_is_served_from_cache() {
        let inner = Arc::new(CountingEmbedder {
            calls: AtomicUsize::new(0),
            dim: 8,
        });
        let cached = CachedEmbedder::new(inner.clone(), &cache_path()).unwrap();

        let salt = std::process::id();
        let a = format!("alpha-cache-test-{salt}");
        let b = format!("beta-cache-test-{salt}");
        let texts = [a.as_str(), b.as_str()];
        let first = cached.embed_batch(&texts).await.unwrap();
        assert_eq!(first.len(), 2);
        let calls_after_first = inner.calls.load(Ordering::SeqCst);
        assert!(calls_after_first >= 2, "first pass embeds for real");

        let second = cached.embed_batch(&texts).await.unwrap();
        assert_eq!(first, second, "cached vectors identical");
        assert_eq!(
            inner.calls.load(Ordering::SeqCst),
            calls_after_first,
            "second pass must not call the inner embedder"
        );
    }

    #[tokio::test]
    async fn partial_hits_only_embed_misses_in_order() {
        let inner = Arc::new(CountingEmbedder {
            calls: AtomicUsize::new(0),
            dim: 8,
        });
        let cached = CachedEmbedder::new(inner.clone(), &cache_path()).unwrap();

        let salt = std::process::id();
        let gamma = format!("gamma-partial-{salt}");
        let delta = format!("delta-partial-x-{salt}");
        let epsilon = format!("epsilon-partial-xy-{salt}");
        cached.embed_batch(&[gamma.as_str()]).await.unwrap();
        let baseline = inner.calls.load(Ordering::SeqCst);

        let out = cached
            .embed_batch(&[delta.as_str(), gamma.as_str(), epsilon.as_str()])
            .await
            .unwrap();
        assert_eq!(out.len(), 3);
        // gamma served from cache: only delta + epsilon embed.
        assert_eq!(inner.calls.load(Ordering::SeqCst), baseline + 2);
        // Order preserved: each vector encodes its text length.
        assert_eq!(out[0][0], delta.len() as f32);
        assert_eq!(out[1][0], gamma.len() as f32);
        assert_eq!(out[2][0], epsilon.len() as f32);
    }
}
