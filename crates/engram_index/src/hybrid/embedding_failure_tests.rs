use super::*;
use std::collections::BTreeSet;
#[cfg(feature = "vector")]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[cfg(feature = "vector")]
struct IntermittentProvider {
    failing: AtomicBool,
    middle_calls: AtomicUsize,
    cancel: Option<CancellationToken>,
    different_space: bool,
}

#[cfg(feature = "vector")]
#[async_trait::async_trait]
impl engram_ml::Embedder for IntermittentProvider {
    async fn embed(&self, _: &str) -> anyhow::Result<engram_ml::Embedding> {
        Ok(vec![1.0; 384])
    }
    async fn embed_batch(&self, texts: &[&str]) -> anyhow::Result<Vec<engram_ml::Embedding>> {
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
            anyhow::bail!("provider interrupted by caller");
        }
        if self.failing.load(Ordering::SeqCst) && texts.iter().any(|text| text.contains("__FAIL__"))
        {
            // At least one successful provider sub-batch is discarded with
            // the failed outer write. Count missing rows, not failed requests.
            if self.middle_calls.fetch_add(1, Ordering::SeqCst) > 0 {
                anyhow::bail!("owned unavailable provider");
            }
        }
        Ok(vec![vec![1.0; 384]; texts.len()])
    }
    fn dimension(&self) -> usize {
        384
    }
    fn model_tag(&self) -> String {
        if self.different_space {
            "different-space".into()
        } else {
            engram_ml::Embedder::model_tag(&engram_ml::LocalEmbedder)
        }
    }
}

fn identities(index: &HybridSearchEngine) -> anyhow::Result<BTreeSet<(String, String, String)>> {
    index
        .list_docs_in_namespace("project", "memory")?
        .into_iter()
        .map(|doc| {
            let content = index
                .get_doc_by_doc_id("project", "memory", 1, &doc.doc_id)?
                .ok_or_else(|| anyhow::anyhow!("Missing retained source chunk"))?
                .2;
            Ok((doc.path, doc.doc_id, content))
        })
        .collect()
}

#[tokio::test]
async fn embedding_failure_keeps_all_source_batches_and_recovers_vectors() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let result = async {
        let root = temp.path().join("source"); std::fs::create_dir(&root)?;
        let mut files = Vec::new();
        for number in 0..150 {
            let marker = if (50..100).contains(&number) { "__FAIL__" } else { "healthy" };
            let mut text = format!("pub fn marker_{number}() -> u32 {{ {number} }} // {marker}\n");
            for line in 0..80 {
                text.push_str(&format!("// {marker} file {number} line {line}: retained independently authored source content\n"));
            }
            let path = root.join(format!("file{number:03}.rs")); std::fs::write(&path, text)?; files.push(path);
        }
        let cfg = engram_core::Config { embedding_backend: "local".into(), ..Default::default() };
        #[allow(unused_mut)]
        let mut index = HybridSearchEngine::new(temp.path().join("text"), temp.path().join("vectors"), &cfg).await?;
        #[cfg(feature = "vector")]
        let provider = Arc::new(IntermittentProvider { failing: AtomicBool::new(true), middle_calls: AtomicUsize::new(0), cancel: None, different_space: false });
        #[cfg(feature = "vector")]
        { index.embedder = provider.clone(); }
        let cancel = CancellationToken::new();
        let stats = index.index_files("project", "memory", 1, &root, files, 256, &cancel, |_, _| {}).await?;
        assert_eq!(stats.all_files.len(), 150);
        for number in 0..150 {
            assert!(stats.symbols.iter().any(|(_, symbol)| symbol.name == format!("marker_{number}")), "Missing declaration {number}");
        }
        let before = identities(&index)?;
        assert_eq!(before.len(), stats.chunks);
        let paths: BTreeSet<_> = before.iter().map(|row| row.0.clone()).collect();
        assert_eq!(paths, (0..150).map(|n| format!("file{n:03}.rs")).collect());
        #[cfg(feature = "vector")]
        {
            let missing = before.iter().filter(|row| (50..100).any(|n| row.0 == format!("file{n:03}.rs"))).count();
            assert!(missing > 512, "Fixture must exercise separate source flushes");
            assert_eq!(stats.unembedded_chunks, missing);
            assert_eq!(stats.failed_embedding_batches, 1);
            let (rows, present) = index.vector_generation_inventory("project", Some("memory"), 1).await?;
            assert_eq!(rows, before.len()-missing);
            let expected: BTreeSet<_> = (0..150).filter(|n| !(50..100).contains(n)).map(|n| format!("file{n:03}.rs")).collect();
            assert_eq!(present, expected, "Both successful surrounding batches must remain searchable by vector");
            provider.failing.store(false, Ordering::SeqCst);
            assert_eq!(index.recover_vectors("project", &cancel).await?, before.len());
            assert_eq!(index.vector_generation_inventory("project", Some("memory"), 1).await?, (before.len(), paths));
            assert_eq!(identities(&index)?, before, "Vector repair changed retained source identity/content");
        }
        #[cfg(not(feature = "vector"))]
        {
            assert_eq!(stats.unembedded_chunks, 0);
            assert_eq!(stats.failed_embedding_batches, 0);
            assert!(stats.embedding_warning().is_none());
        }
        drop(index);
        Ok(())
    }.await;
    temp.close()?;
    result
}

#[cfg(feature = "vector")]
#[tokio::test]
async fn embedding_failure_does_not_swallow_cancellation_or_storage_contract_errors()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let result = async {
        let root = temp.path().join("source");
        std::fs::create_dir(&root)?;
        let path = root.join("one.rs");
        std::fs::write(&path, "pub fn authored_marker() {}\n")?;
        let cfg = engram_core::Config {
            embedding_backend: "local".into(),
            ..Default::default()
        };
        let mut index =
            HybridSearchEngine::new(temp.path().join("text"), temp.path().join("vectors"), &cfg)
                .await?;
        let cancel = CancellationToken::new();
        index.embedder = Arc::new(IntermittentProvider {
            failing: AtomicBool::new(false),
            middle_calls: AtomicUsize::new(0),
            cancel: Some(cancel.clone()),
            different_space: false,
        });
        let error = index
            .index_files(
                "project",
                "memory",
                1,
                &root,
                vec![path.clone()],
                2048,
                &cancel,
                |_, _| {},
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cancel"), "{error:#}");
        // Existing table belongs to this exact provider space. A changed
        // model contract must be refused, even though lexical degradation is
        // permitted for an unavailable matching provider.
        index.embedder = Arc::new(IntermittentProvider {
            failing: AtomicBool::new(false),
            middle_calls: AtomicUsize::new(0),
            cancel: None,
            different_space: true,
        });
        let error = index
            .index_files(
                "project",
                "memory",
                2,
                &root,
                vec![path],
                2048,
                &CancellationToken::new(),
                |_, _| {},
            )
            .await
            .unwrap_err();
        assert!(
            !error.is::<EmbeddingProviderFailure>(),
            "Storage contract refusal mislabeled as provider failure: {error:#}"
        );
        assert!(
            error.to_string().contains("vector") || error.to_string().contains("embedding"),
            "{error:#}"
        );
        drop(index);
        Ok(())
    }
    .await;
    temp.close()?;
    result
}
