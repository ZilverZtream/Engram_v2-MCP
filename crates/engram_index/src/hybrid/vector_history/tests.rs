use super::*;
use anyhow::{Result, ensure};
use futures::TryStreamExt;
use std::collections::BTreeMap;
use std::time::Duration;

pub(super) fn document(generation: u64, path: &str, text: &str) -> IndexDoc {
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
        doc_id: path.into(),
        content_hash: ContentHash::compute(text.as_bytes()).0,
    }
}

pub(super) async fn engine(root: &Path) -> Result<HybridSearchEngine> {
    HybridSearchEngine::new(
        root.join("fts"),
        root.join("vectors"),
        &engram_core::Config::default(),
    )
    .await
}

pub(super) async fn table(index: &HybridSearchEngine) -> Result<lancedb::Table> {
    let epoch = index.pin_vector_epoch("project")?;
    index.open_vector_epoch_table(&epoch).await
}

fn query() -> HybridQuery {
    HybridQuery {
        project_id: "project".into(),
        namespace: "memory".into(),
        generation: 1,
        text: "needle".into(),
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

fn string_column<'a>(
    batch: &'a arrow_array::RecordBatch,
    name: &str,
) -> Result<&'a arrow_array::StringArray> {
    batch
        .column_by_name(name)
        .and_then(|a| a.as_any().downcast_ref())
        .ok_or_else(|| anyhow::anyhow!("Missing fixture column {name}"))
}

struct FailedProvider {
    dimension: usize,
    tag: String,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl engram_ml::Embedder for FailedProvider {
    async fn embed(&self, _: &str) -> Result<Vec<f32>> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        anyhow::bail!("fixture provider unavailable before native query")
    }
    fn dimension(&self) -> usize {
        self.dimension
    }
    fn model_tag(&self) -> String {
        self.tag.clone()
    }
}

async fn rows(table: &lancedb::Table) -> Result<BTreeMap<String, String>> {
    let mut stream = table.query().limit(100).execute().await?;
    let mut result = BTreeMap::new();
    while let Some(batch) = stream.try_next().await? {
        let pks = string_column(&batch, "pk")?;
        let hashes = string_column(&batch, "content_hash")?;
        let paths = string_column(&batch, "path")?;
        let vectors = batch
            .column_by_name("vector")
            .and_then(|a| a.as_any().downcast_ref::<arrow_array::FixedSizeListArray>())
            .ok_or_else(|| anyhow::anyhow!("Missing fixture vector"))?;
        for row in 0..batch.num_rows() {
            let values = vectors.value(row);
            let values = values
                .as_any()
                .downcast_ref::<arrow_array::Float32Array>()
                .ok_or_else(|| anyhow::anyhow!("Wrong fixture vector type"))?;
            let mut digest = blake3::Hasher::new();
            digest.update(hashes.value(row).as_bytes());
            digest.update(paths.value(row).as_bytes());
            for value in values.values() {
                digest.update(&value.to_bits().to_le_bytes());
            }
            ensure!(
                result
                    .insert(
                        pks.value(row).to_owned(),
                        digest.finalize().to_hex().to_string()
                    )
                    .is_none()
            );
        }
    }
    ensure!(result.len() < 100, "Fixture inspection must be complete");
    Ok(result)
}

pub(super) fn inventory(root: &Path) -> Result<BTreeMap<PathBuf, String>> {
    let mut pending = vec![root.to_owned()];
    let mut result = BTreeMap::new();
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                pending.push(entry.path());
            } else {
                ensure!(result.len() < 2048 && entry.metadata()?.len() <= 16 * 1024 * 1024);
                result.insert(
                    entry.path().strip_prefix(root)?.to_owned(),
                    blake3::hash(&std::fs::read(entry.path())?)
                        .to_hex()
                        .to_string(),
                );
            }
        }
    }
    Ok(result)
}

fn deferred(outcome: NativeHistoryPruneOutcome) -> Result<String> {
    match outcome {
        NativeHistoryPruneOutcome::Deferred { reason } => Ok(reason),
        other => anyhow::bail!("Expected preserved history, got {other:?}"),
    }
}

#[test]
fn history_prune_preserves_native_readers_selected_revisions_vector_bits_and_reopen() -> Result<()>
{
    let owned = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        let index = engine(owned.path()).await?;
        let cancel = CancellationToken::new();
        index.index_docs("project", &[document(1, "hot.rs", "needle old"), document(1, "stable.rs", "needle inherited")], &cancel).await?;
        let mut knowledge = document(1, "knowledge", "durable knowledge");
        knowledge.namespace = "memory_bank".into();
        index.index_docs("project", &[knowledge], &cancel).await?;
        let source = index.pin_source_generation("project", 1)?;
        let physical = index.pin_vector_epoch("project")?;
        let old = index.open_vector_epoch_table(&physical).await?;
        let old_version = old.version().await?;
        old.checkout(old_version).await?;
        let old_rows = rows(&old).await?;
        index.prepare_source_snapshot("project", 1, 2, &[RelPath::new("hot.rs")], &[], &cancel).await?;
        index.index_docs("project", &[document(2, "hot.rs", "needle current")], &cancel).await?;
        index.seal_source_snapshot("project", 2)?;
        index.purge_old_generations("project", 2).await?;
        index.compact_vector_fragments("project").await?;
        let before = rows(&table(&index).await?).await?;
        ensure!(deferred(index.prune_vector_history("project", &cancel).await?)?.contains("readers"));
        ensure!(rows(&old).await? == old_rows, "Held native version changed during deferred maintenance");
        drop(old);
        drop(physical);
        let outcome = index.prune_vector_history("project", &cancel).await?;
        ensure!(matches!(outcome, NativeHistoryPruneOutcome::Pruned { old_versions, .. } if old_versions > 0), "Expected actual history removal: {outcome:?}");
        let current = table(&index).await?;
        ensure!(rows(&current).await? == before, "Pruning changed current source/knowledge values or vector bits");
        ensure!(source._snapshot.contains("hot.rs", 1));
        ensure!(index.count_docs_in_generation("project", "memory", 1)? == 2);
        ensure!(index.count_docs_in_generation("project", "memory", 2)? == 2);
        ensure!(current.list_versions().await?.len() == 1, "Old native manifest directory must actually shrink");
        ensure!(current.checkout(old_version).await.is_err(), "An unleased pruned native version must no longer be available");
        drop(current);
        drop(source);
        drop(index);
        let reopened = engine(owned.path()).await?;
        ensure!(rows(&table(&reopened).await?).await? == before);
        reopened.prepare_source_snapshot("project", 2, 3, &[RelPath::new("hot.rs")], &[], &cancel).await?;
        reopened.index_docs("project", &[document(3, "hot.rs", "needle next edit")], &cancel).await?;
        reopened.seal_source_snapshot("project", 3)?;
        reopened.purge_old_generations("project", 3).await?;
        reopened.compact_vector_fragments("project").await?;
        ensure!(matches!(reopened.prune_vector_history("project", &cancel).await?, NativeHistoryPruneOutcome::Pruned { .. }));
        ensure!(reopened.count_docs_in_generation("project", "memory", 3)? == 2);
        let next = rows(&table(&reopened).await?).await?;
        ensure!(next.len() == 3 && next.keys().any(|pk| pk.contains(":memory_bank:")));
        Ok(())
    });
    drop(runtime);
    owned.close()?;
    result
}

#[test]
fn cancelled_readers_and_prune_waiters_keep_backend_exclusion_until_completion() -> Result<()> {
    let owned = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        let index = engine(owned.path()).await?;
        let cancel = CancellationToken::new();
        index
            .index_docs(
                "project",
                &[document(1, "hot.rs", "needle selected")],
                &cancel,
            )
            .await?;
        let pause = Arc::new(vector_reads::NativeReadPause {
            entered: Default::default(),
            release: Default::default(),
            completed: Default::default(),
            started: Default::default(),
        });
        struct ReleaseRead(Arc<vector_reads::NativeReadPause>);
        impl Drop for ReleaseRead {
            fn drop(&mut self) {
                self.0.release.notify_one();
            }
        }
        let release_read = ReleaseRead(pause.clone());
        let mut reader = index.clone();
        reader.native_read_pause = Some(pause.clone());
        let request = tokio::spawn(async move {
            reader
                .pure_vector_search(&query(), 30_000, &CancellationToken::new())
                .await
        });
        tokio::time::timeout(Duration::from_secs(10), pause.entered.notified()).await?;
        request.abort();
        ensure!(request.await.unwrap_err().is_cancelled());
        ensure!(
            deferred(index.prune_vector_history("project", &cancel).await?)?.contains("readers"),
            "Cancelled caller released the actual native worker's epoch"
        );
        drop(release_read);
        tokio::time::timeout(Duration::from_secs(10), pause.completed.notified()).await?;
        ensure!(index.vector_routes.read_slots.available_permits() == 4);
        index
            .index_docs("project", &[document(2, "new.rs", "needle later")], &cancel)
            .await?;
        let prune_pause = Arc::new(HistoryPrunePause {
            entered: Default::default(),
            release: Default::default(),
            completed: Default::default(),
        });
        struct ReleasePrune(Arc<HistoryPrunePause>);
        impl Drop for ReleasePrune {
            fn drop(&mut self) {
                self.0.release.notify_one();
            }
        }
        let release_prune = ReleasePrune(prune_pause.clone());
        let mut maintenance = index.clone();
        maintenance.history_prune_pause = Some(prune_pause.clone());
        let waiter = tokio::spawn(async move {
            maintenance
                .prune_vector_history("project", &CancellationToken::new())
                .await
        });
        tokio::time::timeout(Duration::from_secs(10), prune_pause.entered.notified()).await?;
        ensure!(
            index.count_vectors("project").await.is_err(),
            "New native reader bypassed the atomic prune marker"
        );
        ensure!(
            index.begin_project_mutation("project").is_err(),
            "Writer bypassed exclusive native maintenance"
        );
        waiter.abort();
        ensure!(waiter.await.unwrap_err().is_cancelled());
        ensure!(
            index.begin_project_mutation("project").is_err(),
            "Cancelling maintenance's waiter released its backend authority"
        );
        drop(release_prune);
        tokio::time::timeout(Duration::from_secs(10), prune_pause.completed.notified()).await?;
        ensure!(index.count_vectors("project").await? == 2);
        drop(index.begin_project_mutation("project")?);

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut failed_provider = index.clone();
        failed_provider.query_embedder = Arc::new(FailedProvider {
            dimension: index.query_embedder.dimension(),
            tag: index.query_embedder.model_tag(),
            calls: calls.clone(),
        });
        ensure!(
            failed_provider
                .vector_search(&query(), &cancel)
                .await
                .is_err()
        );
        ensure!(calls.load(std::sync::atomic::Ordering::SeqCst) == 1);
        drop(failed_provider);
        ensure!(
            matches!(
                index.prune_vector_history("project", &cancel).await?,
                NativeHistoryPruneOutcome::Pruned { .. }
            ),
            "A failed embedding before native work must not disable later history maintenance"
        );

        // A genuine native query failure cannot certify provider task joins.
        let epoch = index.pin_vector_epoch("project")?;
        let native = index.open_vector_epoch_table(&epoch).await?;
        let failed = index
            .run_native_vector_query(epoch, &cancel, async move {
                let mut stream = native
                    .query()
                    .only_if("missing_fixture_column = 1")
                    .execute()
                    .await?;
                while stream.try_next().await?.is_some() {}
                Ok(())
            })
            .await;
        ensure!(failed.is_err());
        ensure!(
            deferred(index.prune_vector_history("project", &cancel).await?)?.contains("unproven")
        );
        Ok(())
    });
    drop(runtime);
    owned.close()?;
    result
}

#[test]
fn tags_aged_unknown_files_corrupt_manifests_and_budget_limits_preserve_whole_history() -> Result<()>
{
    let owned = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        let mut index = engine(owned.path()).await?;
        let cancel = CancellationToken::new();
        index
            .index_docs("project", &[document(1, "a.rs", "needle one")], &cancel)
            .await?;
        index
            .index_docs("project", &[document(2, "b.rs", "needle two")], &cancel)
            .await?;
        let native = table(&index).await?;
        let root = owned
            .path()
            .join("vectors")
            .join(format!("{}.lance", native.name()));
        let mut tags = native.tags().await?;
        tags.create("held-history", native.version().await? - 1)
            .await?;
        drop(tags);
        drop(native);
        let before = inventory(&root)?;
        ensure!(deferred(index.prune_vector_history("project", &cancel).await?)?.contains("tags"));
        ensure!(inventory(&root)? == before);
        let native = table(&index).await?;
        let mut tags = native.tags().await?;
        ensure!(tags.list().await?.contains_key("held-history"));
        tags.delete("held-history").await?;
        drop(tags);
        drop(native);

        let unknown = root.join("data/human-sentinel.lance");
        std::fs::write(&unknown, b"unowned bytes must survive native maintenance")?;
        let file = std::fs::OpenOptions::new().write(true).open(&unknown)?;
        file.set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000)),
        )?;
        drop(file);
        let before = inventory(&root)?;
        ensure!(
            deferred(index.prune_vector_history("project", &cancel).await?)?.contains("Unverified")
        );
        ensure!(
            inventory(&root)? == before
                && std::fs::read(&unknown)? == b"unowned bytes must survive native maintenance"
        );
        std::fs::remove_file(&unknown)?; // Exact sentinel created by this fixture.

        let native = table(&index).await?;
        let dataset = native.dataset().unwrap().get().await?;
        let path =
            lance_table::io::commit::ManifestNamingScheme::V2.manifest_path(dataset.refs.base(), 1);
        let corrupt = root.join("_versions").join(path.filename().unwrap());
        drop(dataset);
        drop(native);
        let original = std::fs::read(&corrupt)?;
        std::fs::write(&corrupt, b"malformed owned old manifest")?;
        let before = inventory(&root)?;
        deferred(index.prune_vector_history("project", &cancel).await?)?;
        ensure!(
            inventory(&root)? == before,
            "Malformed old metadata must not be skipped before native deletion"
        );
        std::fs::write(&corrupt, original)?;
        index.history_preflight_work_limit = Some(0);
        let before = inventory(&root)?;
        ensure!(
            deferred(index.prune_vector_history("project", &cancel).await?)?.contains("budget")
        );
        ensure!(inventory(&root)? == before);
        index.history_preflight_work_limit = None;
        let native = table(&index).await?;
        native
            .create_index(&["pk"], lancedb::index::Index::BTree(Default::default()))
            .execute()
            .await?;
        drop(native);
        let before = inventory(&root)?;
        let reason = deferred(index.prune_vector_history("project", &cancel).await?)?;
        ensure!(
            reason.contains("index") && inventory(&root)? == before,
            "Unsupported native index data must defer pruning and remain byte-identical"
        );
        Ok(())
    });
    drop(runtime);
    owned.close()?;
    result
}
