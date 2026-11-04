//! User-visible backend compatibility and query-cache isolation.
mod filter_experiment;
mod process_exclusion;
mod compaction;
use super::*;
use engram_ml::Embedder;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};

fn document(path: &str, generation: u64, id: &str, text: &str) -> IndexDoc {
    IndexDoc {
        generation,
        chunk_id: generation,
        path: RelPath::new(path),
        language: "rust".into(),
        content: text.into(),
        namespace: "memory".into(),
        author: None,
        timestamp: None,
        start_line: 1,
        end_line: 1,
        doc_id: id.into(),
        content_hash: ContentHash::compute(text.as_bytes()).0,
    }
}

fn query(generation: u64, text: &str) -> HybridQuery {
    HybridQuery {
        project_id: "project".into(),
        namespace: "memory".into(),
        generation,
        text: text.into(),
        top_k: 10,
        fts_mode: "loose".into(),
        include_path_prefixes: None,
        exclude_path_prefixes: None,
        include_path_suffixes: None,
        language_filters: None,
        author_filter: None,
        date_after: None,
        date_before: None,
        use_mmr: false,
    }
}

async fn engine(root: &Path, cfg: &engram_core::Config) -> anyhow::Result<HybridSearchEngine> {
    HybridSearchEngine::new(root.join("tantivy"), root.join("lancedb"), cfg).await
}

#[tokio::test]
async fn default_vectors_survive_cold_reopen_and_reuse_unchanged_content_after_edit()
-> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let result = async {
        let cfg = engram_core::Config::default();
        let cancel = CancellationToken::new();
        let index = engine(temporary.path(), &cfg).await?;
        anyhow::ensure!(
            index.semantic_quality() == SemanticQuality::DegradedTrigram,
            "Default projection vectors must disclose their degraded quality"
        );
        index
            .index_docs(
                "project",
                &[
                    document("a.rs", 1, "old-a", "obsolete source"),
                    document("b.rs", 1, "stable-b", "stable source"),
                ],
                &cancel,
            )
            .await?;
        let before = index.vector_search(&query(1, "source"), &cancel).await?;
        anyhow::ensure!(
            before.len() == 2,
            "Default configuration must create searchable vectors"
        );
        let stable_pk = before
            .iter()
            .find(|h| h.doc_id == "stable-b")
            .ok_or_else(|| anyhow::anyhow!("Missing unchanged source vector"))?
            .pk
            .clone();
        drop(index);

        let reopened = engine(temporary.path(), &cfg).await?;
        anyhow::ensure!(
            reopened
                .vector_search(&query(1, "source"), &cancel)
                .await?
                .len()
                == 2,
            "A cold reopen must preserve default vector search"
        );
        reopened
            .prepare_source_snapshot("project", 1, 2, &[RelPath::new("a.rs")], &[], &cancel)
            .await?;
        reopened
            .index_docs(
                "project",
                &[document("a.rs", 2, "new-a", "replacement source")],
                &cancel,
            )
            .await?;
        reopened.seal_source_snapshot("project", 2)?;
        let after = reopened.vector_search(&query(2, "source"), &cancel).await?;
        anyhow::ensure!(
            after
                .iter()
                .map(|h| h.doc_id.as_str())
                .collect::<BTreeSet<_>>()
                == BTreeSet::from(["new-a", "stable-b"]),
            "An edit must select the new and inherited vectors"
        );
        anyhow::ensure!(
            after
                .iter()
                .any(|h| h.doc_id == "stable-b" && h.pk == stable_pk),
            "Unchanged content must keep its physical vector identity"
        );
        Ok(())
    }
    .await;
    temporary.close()?;
    result
}

#[tokio::test]
async fn explicit_lexical_mode_preserves_vectors_and_default_can_initialize_stored_text()
-> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let result = async {
        let cancel = CancellationToken::new();
        let lexical_cfg = engram_core::Config {
            embedding_backend: "fts_only".into(),
            ..Default::default()
        };
        let lexical = engine(temporary.path(), &lexical_cfg).await?;
        lexical
            .index_docs(
                "project",
                &[document("a.rs", 1, "seed", "stored source")],
                &cancel,
            )
            .await?;
        anyhow::ensure!(
            lexical.native_store.capture().expect("owned native fixture").connection.table_names().execute().await?.is_empty(),
            "Explicit lexical mode must not create vector tables"
        );
        anyhow::ensure!(
            lexical
                .vector_search(&query(1, "source"), &cancel)
                .await?
                .is_empty()
        );
        let refused = lexical
            .initialize_vectors("project", &cancel)
            .await
            .unwrap_err();
        anyhow::ensure!(matches!(
            refused.downcast_ref::<VectorInitializationRejection>(),
            Some(VectorInitializationRejection::NoBackend)
        ));
        drop(lexical);

        let index = engine(temporary.path(), &engram_core::Config::default()).await?;
        let missing = index
            .vector_search(&query(1, "source"), &cancel)
            .await
            .unwrap_err();
        anyhow::ensure!(
            format!("{missing:#}").contains("stored documents have no identified vector store"),
            "Missing vector coverage must be disclosed before initialization: {missing:#}"
        );
        anyhow::ensure!(index.count_docs("project")? == 1);
        anyhow::ensure!(
            index.initialize_vectors("project", &cancel).await? == 1,
            "Default backend must initialize vectors from existing stored text"
        );
        anyhow::ensure!(
            index
                .vector_search(&query(1, "source"), &cancel)
                .await?
                .len()
                == 1
        );
        let tables = index.native_store.capture().expect("owned native fixture").connection.table_names().execute().await?;
        anyhow::ensure!(!tables.is_empty());
        let vector_count = index.count_vectors("project").await?;
        drop(index);

        let lexical = engine(temporary.path(), &lexical_cfg).await?;
        anyhow::ensure!(
            lexical
                .vector_search(&query(1, "source"), &cancel)
                .await?
                .is_empty()
        );
        anyhow::ensure!(lexical.lexical_search(&query(1, "source"))?.len() == 1);
        anyhow::ensure!(
            lexical.native_store.capture().expect("owned native fixture").connection.table_names().execute().await? == tables,
            "Choosing lexical mode must preserve historical vector tables"
        );
        drop(lexical);
        let reopened = engine(temporary.path(), &engram_core::Config::default()).await?;
        anyhow::ensure!(reopened.count_vectors("project").await? == vector_count);
        anyhow::ensure!(
            reopened
                .vector_search(&query(1, "source"), &cancel)
                .await?
                .len()
                == 1,
            "Switching back from lexical mode must retain searchable vectors"
        );
        Ok(())
    }
    .await;
    temporary.close()?;
    result
}

struct DirectoryRedirect {
    live: PathBuf,
    saved: PathBuf,
    linked: bool,
    restored: bool,
}

impl DirectoryRedirect {
    fn new(fixture: &Path, live: PathBuf, destination: &Path) -> anyhow::Result<Self> {
        let fixture = fixture.canonicalize()?;
        let saved = live.with_file_name("owned-original");
        anyhow::ensure!(
            live.canonicalize()?.starts_with(&fixture)
                && destination.canonicalize()?.starts_with(&fixture)
                && saved
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("Missing backup parent"))?
                    .canonicalize()?
                    .starts_with(&fixture),
            "Redirect test must remain inside its owned fixture"
        );
        std::fs::rename(&live, &saved)?;
        let mut redirect = Self {
            live,
            saved,
            linked: false,
            restored: false,
        };
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            let status = std::process::Command::new("cmd.exe")
                .args(["/D", "/C", "mklink", "/J"])
                .arg(&redirect.live)
                .arg(destination)
                .creation_flags(0x08000000)
                .output()?;
            anyhow::ensure!(
                status.status.success(),
                "Could not create owned test junction: {}",
                String::from_utf8_lossy(&status.stderr)
            );
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(destination, &redirect.live)?;
        redirect.linked = true;
        Ok(redirect)
    }

    fn restore(&mut self) -> anyhow::Result<()> {
        if self.restored {
            return Ok(());
        }
        if self.linked {
            // Remove this link only, never traverse or recursively remove its target.
            #[cfg(windows)]
            std::fs::remove_dir(&self.live)?;
            #[cfg(unix)]
            std::fs::remove_file(&self.live)?;
            self.linked = false;
        }
        std::fs::rename(&self.saved, &self.live)?;
        self.restored = true;
        Ok(())
    }
}

impl Drop for DirectoryRedirect {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[tokio::test]
async fn redirected_ownership_metadata_is_refused_before_lexical_mutation() -> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let result = async {
        let cancel = CancellationToken::new();
        let index = engine(temporary.path(), &engram_core::Config::default()).await?;
        index
            .index_docs(
                "project",
                &[document("a.rs", 1, "seed", "original source")],
                &cancel,
            )
            .await?;
        let owned = temporary
            .path()
            .join("tantivy")
            .join("source-snapshots")
            .join("vector-routes")
            .join("owned");
        let receipts = std::fs::read_dir(&owned)?.collect::<Result<Vec<_>, _>>()?;
        anyhow::ensure!(receipts.len() == 1, "Expected one owned test table");
        let receipt = receipts[0].file_name();
        let original = std::fs::read(owned.join(&receipt))?;
        let outside = temporary.path().join("redirected-receipts");
        std::fs::create_dir(&outside)?;
        std::fs::write(outside.join(&receipt), &original)?;
        let mut redirect = DirectoryRedirect::new(temporary.path(), owned, &outside)?;
        let refused = index.create_bulk_writer_async("project", &cancel).await;
        // Restore even when the operation unexpectedly succeeds, before checking it.
        redirect.restore()?;
        let was_refused = refused.is_err();
        drop(refused);
        anyhow::ensure!(
            was_refused,
            "An identical ownership receipt reached through a redirected directory must be refused"
        );
        anyhow::ensure!(
            std::fs::read(outside.join(&receipt))? == original,
            "Refusing redirected metadata must preserve the external evidence"
        );
        anyhow::ensure!(
            index.count_docs("project")? == 1
                && index.lexical_search(&query(1, "original"))?.len() == 1,
            "Rejected preflight must preserve existing lexical documents"
        );
        let writer = index.create_bulk_writer_async("project", &cancel).await?;
        writer.finish()?;
        anyhow::ensure!(
            index
                .vector_search(&query(1, "source"), &cancel)
                .await?
                .len()
                == 1,
            "Restoring valid ownership metadata must leave the original vectors usable"
        );
        Ok(())
    }
    .await;
    temporary.close()?;
    result
}

struct CountingProvider {
    calls: AtomicUsize,
    tag: &'static str,
}

struct RecordingLocalProvider {
    contents: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl Embedder for RecordingLocalProvider {
    async fn embed(&self, text: &str) -> anyhow::Result<engram_ml::Embedding> {
        self.contents.lock().map_err(|_| anyhow::anyhow!("recording provider poisoned"))?
            .push(ContentHash::compute(text.as_bytes()).0);
        engram_ml::embed::LocalEmbedder.embed(text).await
    }

    fn dimension(&self) -> usize {
        engram_ml::embed::LocalEmbedder.dimension()
    }

    fn model_tag(&self) -> String {
        engram_ml::embed::LocalEmbedder.model_tag()
    }
}

#[tokio::test]
async fn editing_and_deleting_files_embed_only_new_content_and_preserve_inherited_vectors()
-> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let result = async {
        let cfg = engram_core::Config { embedding_backend: "local".into(), ..Default::default() };
        let cancel = CancellationToken::new();
        let contents = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = || -> anyhow::Result<Vec<String>> {
            Ok(contents.lock().map_err(|_| anyhow::anyhow!("recording provider poisoned"))?.clone())
        };
        let hash = |text: &str| ContentHash::compute(text.as_bytes()).0;
        let mut index = engine(temporary.path(), &cfg).await?;
        // Query embedding remains the original provider: these observations
        // count document work, not the separate cost of asking a question.
        index.embedder = Arc::new(RecordingLocalProvider { contents: contents.clone() });
        let old_text = ["original_first_a source", "original_second_a source",
            "untouched_b source", "removed_c source"];
        let initial = [
            document("a.rs", 1, "a-first", old_text[0]),
            document("a.rs", 1, "a-second", old_text[1]),
            document("b.rs", 1, "stable-b", old_text[2]),
            document("c.rs", 1, "removed-c", old_text[3]),
        ];
        index.index_docs("project", &initial, &cancel).await?;
        let initial_calls = recorded()?;
        anyhow::ensure!(initial_calls.len() == 4 && initial_calls.into_iter().collect::<BTreeSet<_>>()
            == old_text.iter().map(|text| hash(text)).collect(), "Each initial document must be embedded once");
        let old = index.pin_source_generation("project", 1)?;
        let old_vectors = index.vector_search(&query(1, "source"), &cancel).await?;
        let stable_pk = old_vectors.iter().find(|hit| hit.doc_id == "stable-b")
            .ok_or_else(|| anyhow::anyhow!("Initial unchanged vector missing"))?.pk.clone();
        index.prepare_source_snapshot("project", 1, 2, &[RelPath::new("a.rs")], &[RelPath::new("c.rs")], &cancel).await?;
        anyhow::ensure!(recorded()?.len() == 4, "Preparing an edit/deletion must not embed inherited contents");
        let replacement = "unique_replacement_a source";
        index.index_docs("project", &[document("a.rs", 2, "new-a", replacement)], &cancel).await?;
        anyhow::ensure!(recorded()?.get(4..) == Some([hash(replacement)].as_slice()),
            "Shrinking an edited file must embed only its new content, never its unchanged sibling");
        index.seal_source_snapshot("project", 2)?;
        let selected = index.vector_search(&query(2, "source"), &cancel).await?;
        anyhow::ensure!(selected.iter().map(|hit| hit.doc_id.as_str()).collect::<BTreeSet<_>>()
            == BTreeSet::from(["new-a", "stable-b"]), "Only complete changed and inherited revisions are selected");
        anyhow::ensure!(selected.iter().any(|hit| hit.doc_id == "stable-b" && hit.pk == stable_pk),
            "An unchanged vector retains its physical row identity");
        let lexical = index.lexical_search(&query(2, "source"))?;
        anyhow::ensure!(lexical.iter().map(|hit| hit.doc_id.as_str()).collect::<BTreeSet<_>>()
            == BTreeSet::from(["new-a", "stable-b"]));
        anyhow::ensure!(lexical.iter().any(|hit| hit.doc_id == "stable-b" && hit.pk == stable_pk));
        index.purge_old_generations("project", 2).await?;
        anyhow::ensure!(index.vector_search(&query(1, "source"), &cancel).await?.len() == 4
            && index.lexical_search(&query(1, "source"))?.len() == 4, "Existing readers retain the complete original multi-chunk view");

        // Deletion alone must retain B without any document embedding work.
        index.prepare_source_snapshot("project", 2, 3, &[], &[RelPath::new("a.rs")], &cancel).await?;
        index.seal_source_snapshot("project", 3)?;
        anyhow::ensure!(recorded()?.len() == 5, "Sealing, reads, GC and deletion alone must not re-embed content");
        let remaining = index.vector_search(&query(3, "source"), &cancel).await?;
        anyhow::ensure!(remaining.len() == 1 && remaining[0].pk == stable_pk);
        drop(old);
        index.purge_old_generations("project", 3).await?;
        anyhow::ensure!(index.count_docs("project")? == 1 && index.count_vectors("project").await? == 1);
        anyhow::ensure!(recorded()?.len() == 5);
        drop(index);

        let mut reopened = engine(temporary.path(), &cfg).await?;
        let post_open = Arc::new(std::sync::Mutex::new(Vec::new()));
        reopened.embedder = Arc::new(RecordingLocalProvider { contents: post_open.clone() });
        let recovered = reopened.vector_search(&query(3, "source"), &cancel).await?;
        anyhow::ensure!(recovered.len() == 1 && recovered[0].pk == stable_pk,
            "Engine reopen must preserve the inherited selected vector");
        let next_text = "unique_post_reopen_b source";
        reopened.prepare_source_snapshot("project", 3, 4, &[RelPath::new("b.rs")], &[], &cancel).await?;
        reopened.index_docs("project", &[document("b.rs", 4, "new-b", next_text)], &cancel).await?;
        reopened.seal_source_snapshot("project", 4)?;
        anyhow::ensure!(*post_open.lock().map_err(|_| anyhow::anyhow!("recording provider poisoned"))?
            == [hash(next_text)], "The next edit after reopen embeds only the new document");
        let current = reopened.vector_search(&query(4, "source"), &cancel).await?;
        anyhow::ensure!(current.len() == 1 && current[0].doc_id == "new-b");
        Ok(())
    }.await;
    temporary.close()?;
    result
}

#[async_trait::async_trait]
impl Embedder for CountingProvider {
    async fn embed(&self, _text: &str) -> anyhow::Result<engram_ml::Embedding> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![0.3; 8])
    }
    fn dimension(&self) -> usize {
        8
    }
    fn model_tag(&self) -> String {
        self.tag.into()
    }
}

#[tokio::test]
async fn bulk_writes_do_not_put_interactive_queries_in_persistent_embedding_cache()
-> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let result = async {
        let cancel = CancellationToken::new();
        let cfg = engram_core::Config::default();
        let raw_a = Arc::new(CountingProvider {
            calls: AtomicUsize::new(0),
            tag: "space-a",
        });
        let bulk_a = Arc::new(crate::embed_cache::CachedEmbedder::new_lazy(
            raw_a.clone(),
            &temporary.path().join("bulk.redb"),
        ));
        let mut a = engine(temporary.path(), &cfg).await?;
        a.embedder = bulk_a.clone();
        a.query_embedder = Arc::new(crate::embed_cache::QueryEmbedder::new(raw_a.clone()));
        let seed = document("a.rs", 1, "seed", "stored source");
        a.index_docs("project", std::slice::from_ref(&seed), &cancel)
            .await?;

        // Exercise actual bulk preflight and commit using an already-covered
        // physical document, so this does not introduce missing vector rows.
        let mut writer = a.create_bulk_writer_async("project", &cancel).await?;
        HybridSearchEngine::write_docs_to_writer(&a.fields(), &mut writer, "project", &[seed])?;
        writer.finish()?;

        let raw_b = Arc::new(CountingProvider {
            calls: AtomicUsize::new(0),
            tag: "space-b",
        });
        let mut b = engine(temporary.path(), &cfg).await?;
        b.embedder = raw_b.clone();
        b.query_embedder = Arc::new(crate::embed_cache::QueryEmbedder::new(raw_b.clone()));
        let text = "interactive query absent from the bulk corpus";
        let before_query = raw_a.calls.load(Ordering::SeqCst);
        let hits = b.vector_search(&query(1, text), &cancel).await?;
        anyhow::ensure!(
            hits.len() == 1 && hits[0].doc_id == "seed",
            "A live old epoch must remain searchable in its original embedding space"
        );
        let after_query = raw_a.calls.load(Ordering::SeqCst);
        anyhow::ensure!(
            after_query == before_query + 1,
            "The first novel query must actually call the epoch's query provider once"
        );
        b.vector_search(&query(1, text), &cancel).await?;
        anyhow::ensure!(
            raw_a.calls.load(Ordering::SeqCst) == after_query,
            "Repeated interactive text must use the bounded query cache"
        );
        anyhow::ensure!(
            raw_b.calls.load(Ordering::SeqCst) == 0,
            "A mismatched engine must not embed queries in another vector space"
        );
        bulk_a.embed(text).await?;
        anyhow::ensure!(
            raw_a.calls.load(Ordering::SeqCst) == after_query + 1,
            "Interactive query text must not have populated the persistent bulk cache"
        );
        Ok(())
    }
    .await;
    temporary.close()?;
    result
}
