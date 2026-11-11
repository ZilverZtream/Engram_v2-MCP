use super::*;
use std::collections::BTreeMap;

fn stored_bytes(root: &Path) -> anyhow::Result<BTreeMap<PathBuf, String>> {
    let mut pending = vec![root.to_path_buf()];
    let mut result = BTreeMap::new();
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                pending.push(entry.path());
            } else {
                let path = entry.path();
                let bytes = std::fs::read(&path).map_err(|error|
                    anyhow::anyhow!("Cannot inspect closed storage file {}: {error}", path.display()))?;
                result.insert(path.strip_prefix(root)?.to_path_buf(),
                    blake3::hash(&bytes).to_hex().to_string());
            }
        }
    }
    Ok(result)
}

#[cfg(feature = "vector")]
struct ForbiddenEmbedder(std::sync::atomic::AtomicUsize);

#[cfg(feature = "vector")]
#[async_trait::async_trait]
impl engram_ml::Embedder for ForbiddenEmbedder {
    async fn embed(&self, _: &str) -> anyhow::Result<engram_ml::Embedding> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        anyhow::bail!("Graph extraction must not call an embedding provider")
    }
    fn dimension(&self) -> usize { 384 }
    fn model_tag(&self) -> String { "unavailable-rebuild-provider".into() }
}

#[tokio::test]
async fn graph_rebuild_extraction_preserves_storage_with_an_unavailable_embedder()
-> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let result = async {
        let source = temporary.path().join("source");
        let data = temporary.path().join("data");
        std::fs::create_dir_all(&source)?;
        let authored = [
            ("Caller.cs", "public class Caller {\n public int Run() { return Callee.Next(); }\n}\n"),
            ("Callee.cs", "public class Callee {\n public static int Next() { return 7; }\n}\n"),
            ("empty.cs", ""),
            ("actions.js", "function executeAction() { return 7; }\n"),
            ("schema.sql", "CREATE TABLE FixtureTable (Id INT PRIMARY KEY);\n"),
        ];
        let files = authored.iter().map(|(name, _)| source.join(name)).collect::<Vec<_>>();
        for (name, text) in authored { std::fs::write(source.join(name), text)?; }
        let cfg = engram_core::Config { embedding_backend: "local".into(), ..Default::default() };
        let engine = HybridSearchEngine::new(data.join("tantivy"), data.join("vectors"), &cfg).await?;
        let cancel = CancellationToken::new();
        engine.index_files("project", "memory", 1, &source, files.clone(), 2048, &cancel, |_, _| {}).await?;
        drop(engine);
        // Windows storage locks forbid raw reads through another handle.
        // Compare complete durable bytes only after the owning engine closes.
        let before = stored_bytes(&data)?;
        #[allow(unused_mut)]
        let mut engine = HybridSearchEngine::new(data.join("tantivy"), data.join("vectors"), &cfg).await?;
        #[cfg(feature = "vector")]
        let forbidden = Arc::new(ForbiddenEmbedder(std::sync::atomic::AtomicUsize::new(0)));
        #[cfg(feature = "vector")]
        { engine.embedder = forbidden.clone(); }
        let stats = engine.extract_graph_files("project", &source, files.clone(), 2048, &cancel, |_, _| {}).await?;
        assert_eq!(stats.files, 5);
        assert_eq!(stats.chunks, 0, "Graph reconstruction must not produce new search documents");
        assert_eq!(stats.all_files.len(), 5, "Empty-symbol files are still part of the complete extraction");
        for name in ["Caller", "Callee", "Run", "Next", "executeAction", "FixtureTable"] {
            assert!(stats.symbols.iter().any(|(_, symbol)| symbol.name == name), "Missing authored declaration: {name}");
        }
        for (name, text) in authored {
            let fingerprint = stats.fingerprints.iter().find(|fp| fp.rel_path == name).unwrap();
            assert_eq!(fingerprint.file_hash, blake3::hash(text.as_bytes()).to_hex().as_str());
        }
        assert!(stats.edges.iter().any(|(_, edge)| edge.target_name.contains("Next")), "The authored cross-file call must be extracted");
        drop(engine);
        assert_eq!(stored_bytes(&data)?, before, "Extraction changed search/vector files");

        #[allow(unused_mut)]
        let mut engine = HybridSearchEngine::new(data.join("tantivy"), data.join("vectors"), &cfg).await?;
        #[cfg(feature = "vector")]
        { engine.embedder = forbidden.clone(); }
        let cancelled = CancellationToken::new(); cancelled.cancel();
        assert!(engine.extract_graph_files("project", &source, files.clone(), 2048, &cancelled, |_, _| {}).await.is_err());
        std::fs::remove_file(source.join("Callee.cs"))?;
        assert!(engine.extract_graph_files("project", &source, files, 2048, &cancel, |_, _| {}).await.is_err());
        drop(engine);
        assert_eq!(stored_bytes(&data)?, before, "Cancelled/failed extraction changed stored content");
        #[cfg(feature = "vector")]
        assert_eq!(forbidden.0.load(std::sync::atomic::Ordering::SeqCst), 0);
        Ok(())
    }.await;
    temporary.close()?;
    result
}
