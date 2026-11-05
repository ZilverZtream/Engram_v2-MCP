//! Empty cleanup must not manufacture native versions; real cleanup still owns
//! its current observation and preserves selected source and held readers.
use super::*;
use anyhow::{Result, ensure};
use std::collections::BTreeMap;

fn document(generation: u64, path: &str, text: &str) -> IndexDoc {
    IndexDoc {
        generation, chunk_id: 1, path: RelPath::new(path), language: "rust".into(),
        content: text.into(), namespace: "memory".into(), author: None, timestamp: None,
        start_line: 1, end_line: 1, doc_id: path.into(),
        content_hash: ContentHash::compute(text.as_bytes()).0,
    }
}

async fn engine(root: &Path) -> Result<HybridSearchEngine> {
    // The empty backend uses the local deterministic projection embedder, so
    // these native-store behaviors require neither model files nor a service.
    HybridSearchEngine::new(root.join("fts"), root.join("vectors"),
        &engram_core::Config::default()).await
}

async fn table(engine: &HybridSearchEngine) -> Result<lancedb::Table> {
    let epoch = engine.pin_vector_epoch("project")?;
    engine.open_vector_epoch_table(&epoch).await
}

fn inventory(root: &Path, table_name: &str) -> Result<BTreeMap<PathBuf, (u64, String)>> {
    let table_root = root.join("vectors").join(format!("{table_name}.lance"));
    let mut result = BTreeMap::new();
    let mut pending = vec![table_root.join("_versions"), table_root.join("_transactions")];
    let mut total_bytes = 0u64;
    let mut entries = 0;
    while let Some(directory) = pending.pop() {
        let children = match std::fs::read_dir(directory) {
            Ok(children) => children,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        for child in children {
            let child = child?;
            entries += 1;
            ensure!(entries <= 1024, "Owned native metadata inventory exceeded its entry budget");
            let metadata = child.metadata()?;
            if metadata.is_dir() {
                pending.push(child.path());
            } else {
                ensure!(metadata.is_file() && metadata.len() <= 1024 * 1024);
                total_bytes += metadata.len();
                ensure!(total_bytes <= 16 * 1024 * 1024,
                    "Owned native metadata inventory exceeded its byte budget");
                result.insert(child.path().strip_prefix(&table_root)?.to_owned(),
                    (metadata.len(), blake3::hash(&std::fs::read(child.path())?).to_hex().to_string()));
            }
        }
    }
    ensure!(result.keys().any(|path| path.starts_with("_versions")),
        "A native version manifest must exist for the durable inventory assertion");
    Ok(result)
}

async fn native_state(root: &Path, engine: &HybridSearchEngine)
-> Result<(u64, BTreeMap<PathBuf, (u64, String)>)> {
    let table = table(engine).await?;
    Ok((table.version().await?, inventory(root, table.name())?))
}

async fn has_revision(table: &lancedb::Table, generation: u64, text: &str) -> Result<bool> {
    let hash = ContentHash::compute(text.as_bytes()).0;
    Ok(table.count_rows(Some(format!("generation = {generation} AND content_hash = '{hash}'"))).await? == 1)
}

#[test]
fn wide_managed_cleanup_preserves_inherited_files_all_reader_kinds_and_future_rows() -> Result<()> {
    let owned = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let result = runtime.block_on(async {
        let index = engine(owned.path()).await?;
        let cancel = CancellationToken::new();
        let hot = "src/O'Brien-å.rs";
        let mut docs = (0..2300).map(|i| document(1, &format!("stable/{i}.rs"), "inherited source"))
            .collect::<Vec<_>>();
        docs.push(document(1, hot, "original hot source"));
        let mut extra = document(1, hot, "second original chunk");
        extra.doc_id.push_str("#second");extra.chunk_id = 2;docs.push(extra);
        index.index_docs("project", &docs, &cancel).await?;
        let legacy_reader = index.pin_source_generation("project", 1)?;
        let native_reader = table(&index).await?;
        let reader_version = native_reader.version().await?;
        native_reader.checkout(reader_version).await?;
        index.prepare_source_snapshot("project", 1, 2, &[], &[], &cancel).await?;
        index.seal_source_snapshot("project", 2)?;
        let files_reader = index.pin_source_generation("project", 2)?;
        index.prepare_source_snapshot("project", 2, 3, &[RelPath::new(hot)], &[], &cancel).await?;
        index.index_docs("project", &[document(3, hot, "current hot source")], &cancel).await?;
        index.seal_source_snapshot("project", 3)?;
        let mut global = document(0, "knowledge", "authored global note");
        global.namespace = "memory_bank".into();
        index.embed_and_upsert_vectors("project", &[
            document(1, "legacy-only.rs", "native-only legacy row"),
            document(4, "future.rs", "unpublished future source"),
        ], &cancel).await?;
        index.embed_and_upsert_vectors("project", &[global], &cancel).await?;
        let baseline = native_state(owned.path(), &index).await?;
        index.purge_old_generations("project", 3).await?;
        ensure!(native_state(owned.path(), &index).await? == baseline,
            "Protected old revisions must not create native cleanup versions or metadata files");
        drop(legacy_reader);
        index.purge_old_generations("project", 3).await?;
        let latest = table(&index).await?;
        ensure!(!has_revision(&latest, 1, "native-only legacy row").await?);
        ensure!(has_revision(&latest, 1, "original hot source").await?
            && has_revision(&latest, 1, "second original chunk").await?,
            "The separately held Files selection must still protect both original chunks");
        drop(latest);drop(files_reader);
        index.purge_old_generations("project", 3).await?;
        let cleaned = native_state(owned.path(), &index).await?;
        let latest = table(&index).await?;
        ensure!(latest.count_rows(Some("namespace = 'memory' AND generation = 1".into())).await? == 2300);
        ensure!(!has_revision(&latest, 1, "original hot source").await?
            && !has_revision(&latest, 1, "second original chunk").await?);
        ensure!(has_revision(&latest, 3, "current hot source").await?
            && has_revision(&latest, 4, "unpublished future source").await?);
        ensure!(latest.count_rows(Some("namespace = 'memory_bank'".into())).await? == 1);
        ensure!(native_reader.version().await? == reader_version
            && has_revision(&native_reader, 1, "original hot source").await?);
        index.purge_old_generations("project", 3).await?;
        ensure!(native_state(owned.path(), &index).await? == cleaned,
            "Repeated completed cleanup must remain a physical native no-op");
        drop(latest);drop(native_reader);drop(index);
        let reopened = engine(owned.path()).await?;
        ensure!(native_state(owned.path(), &reopened).await? == cleaned);
        ensure!(reopened.count_docs_in_generation("project", "memory", 3)? == 2301);
        let latest = table(&reopened).await?;
        ensure!(latest.count_rows(None).await? == 2303);
        Ok(())
    });
    drop(runtime);owned.close()?;result
}

#[test]
fn managed_cleanup_queued_behind_native_only_write_observes_latest_and_preserves_readers() -> Result<()> {
    let owned = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let result = runtime.block_on(async {
        let index = engine(owned.path()).await?;
        let cancel = CancellationToken::new();
        index.index_docs("project", &[document(1, "source.rs", "selected source")], &cancel).await?;
        index.prepare_source_snapshot("project", 1, 2, &[], &[], &cancel).await?;
        index.seal_source_snapshot("project", 2)?;
        let stale = table(&index).await?;
        let original = stale.version().await?;
        let reader = table(&index).await?;reader.checkout(original).await?;
        let waiting = Arc::new(tokio::sync::Notify::new());
        let mut cleanup_index = index.clone();cleanup_index.commit_waiting = Some(waiting.clone());
        let mut ahead = index.acquire_commit(&cancel).await?;
        let cleanup = tokio::spawn(async move { cleanup_index.purge_old_generations("project", 2).await });
        let queued = tokio::time::timeout(std::time::Duration::from_secs(10), waiting.notified()).await;
        let written = if queued.is_ok() {
            index.embed_vectors_in_commit("project", &[
                document(1, "queued-only.rs", "queued obsolete native row"),
                document(3, "future.rs", "queued future native row"),
            ], &cancel, None, Some(&mut ahead)).await
        } else { Err(anyhow::anyhow!("Managed cleanup did not queue behind the writer")) };
        let stale_count = stale.count_rows(Some("path = 'queued-only.rs'".into())).await;
        let postwrite = async {
            let latest = table(&index).await?;
            ensure!(has_revision(&latest, 1, "queued obsolete native row").await?
                && has_revision(&latest, 3, "queued future native row").await?,
                "The fresh native table must contain the real write queued ahead of cleanup");
            Ok::<_, anyhow::Error>(latest.version().await?)
        }.await;
        drop(ahead);
        let cleaned = cleanup.await; // Always join before propagating errors/tearing down.
        written?;cleaned??;
        let postwrite_version = postwrite?;
        ensure!(stale_count? == 0 && stale.version().await? == original);
        let latest = table(&index).await?;
        ensure!(latest.version().await? > postwrite_version, "Real managed cleanup must commit a deletion");
        ensure!(!has_revision(&latest, 1, "queued obsolete native row").await?);
        ensure!(has_revision(&latest, 1, "selected source").await?
            && has_revision(&latest, 3, "queued future native row").await?);
        ensure!(reader.version().await? == original && has_revision(&reader, 1, "selected source").await?);
        let before = native_state(owned.path(), &index).await?;
        index.purge_old_generations("project", 2).await?;
        ensure!(native_state(owned.path(), &index).await? == before);
        Ok(())
    });
    drop(runtime);owned.close()?;result
}

#[test]
fn native_empty_cleanup_preserves_versions_and_real_cleanup_preserves_readers_and_reopen()
-> Result<()> {
    let owned = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let result = runtime.block_on(async {
        let index = engine(owned.path()).await?;
        let cancel = CancellationToken::new();
        let changed = [RelPath::new("source.rs")];
        index.index_docs("project", &[
            document(1, "source.rs", "needle original source"),
            document(1, "unchanged.rs", "needle inherited source"),
        ], &cancel).await?;
        let source_reader = index.pin_source_generation("project", 1)?;
        let native_reader = table(&index).await?;
        let reader_version = native_reader.version().await?;
        native_reader.checkout(reader_version).await?;

        let baseline = native_state(owned.path(), &index).await?;
        index.purge_old_generations_with_cutoff("project", 1, 0).await?;
        ensure!(native_state(owned.path(), &index).await? == baseline,
            "A zero-cutoff native GC must not create versions or transaction/manifest files");
        index.prepare_source_snapshot("project", 1, 2, &changed, &[], &cancel).await?;
        ensure!(native_state(owned.path(), &index).await? == baseline,
            "Preparing a genuinely absent native generation must not commit a delete");

        // Retry the same unpublished generation after a real abandoned write.
        index.index_docs("project", &[document(2, "source.rs", "abandoned payload")], &cancel).await?;
        let staged = native_state(owned.path(), &index).await?;
        index.prepare_source_snapshot("project", 1, 2, &changed, &[], &cancel).await?;
        ensure!(native_state(owned.path(), &index).await?.0 > staged.0,
            "Real abandoned native rows must still be deleted");
        ensure!(table(&index).await?.count_rows(Some("generation = 2".into())).await? == 0);
        index.index_docs("project", &[document(2, "source.rs", "needle current source")], &cancel).await?;
        index.seal_source_snapshot("project", 2)?;
        let both_selected = native_state(owned.path(), &index).await?;
        index.purge_old_generations("project", 2).await?;
        ensure!(native_state(owned.path(), &index).await? == both_selected,
            "GC with all physical revisions selected by current or held source must be a native no-op");
        ensure!(source_reader._snapshot.contains("source.rs", 1));
        ensure!(has_revision(&native_reader, 1, "needle original source").await?);
        ensure!(native_reader.version().await? == reader_version,
            "Mutation preflight must not refresh a held reader's checkout");

        drop(source_reader);
        index.purge_old_generations("project", 2).await?;
        let collected = native_state(owned.path(), &index).await?;
        ensure!(collected.0 > both_selected.0, "Unselected old source must still be collected");
        let current = table(&index).await?;
        ensure!(!has_revision(&current, 1, "needle original source").await?);
        ensure!(has_revision(&current, 1, "needle inherited source").await?);
        ensure!(has_revision(&current, 2, "needle current source").await?);
        ensure!(has_revision(&native_reader, 1, "needle original source").await?,
            "Deleting current rows must preserve an independently checked-out native reader");
        index.purge_old_generations("project", 2).await?;
        ensure!(native_state(owned.path(), &index).await? == collected,
            "Repeated cleanup after a real deletion must not manufacture another native version");
        drop(current);
        drop(native_reader);
        drop(index);

        let reopened = engine(owned.path()).await?;
        ensure!(native_state(owned.path(), &reopened).await? == collected);
        ensure!(reopened.count_docs_in_generation("project", "memory", 2)? == 2);
        ensure!(has_revision(&table(&reopened).await?, 2, "needle current source").await?);
        reopened.prepare_source_snapshot("project", 2, 3, &changed, &[], &cancel).await?;
        ensure!(native_state(owned.path(), &reopened).await? == collected,
            "The next edit's empty staging cleanup must remain a no-op after reopening");
        reopened.index_docs("project", &[document(3, "source.rs", "needle next edit")], &cancel).await?;
        reopened.seal_source_snapshot("project", 3)?;
        reopened.purge_old_generations("project", 3).await?;
        ensure!(reopened.count_docs_in_generation("project", "memory", 3)? == 2);
        let next = table(&reopened).await?;
        ensure!(has_revision(&next, 3, "needle next edit").await?);
        ensure!(has_revision(&next, 1, "needle inherited source").await?);
        ensure!(!has_revision(&next, 2, "needle current source").await?);
        Ok(())
    });
    drop(runtime);
    owned.close()?;
    result
}

#[test]
fn native_cleanup_queued_behind_real_write_uses_latest_without_moving_stale_reader() -> Result<()> {
    let owned = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let result = runtime.block_on(async {
        let index = engine(owned.path()).await?;
        let cancel = CancellationToken::new();
        index.index_docs("project", &[document(1, "source.rs", "needle selected")], &cancel).await?;
        let stale = table(&index).await?;
        let original_version = stale.version().await?;
        let reader = table(&index).await?;
        reader.checkout(original_version).await?;
        let waiting = Arc::new(tokio::sync::Notify::new());
        let mut cleanup_index = index.clone();
        cleanup_index.commit_waiting = Some(waiting.clone());
        let stale_for_cleanup = stale.clone();
        let mut ahead = index.acquire_commit(&cancel).await?;
        let cleanup = tokio::spawn(async move {
            let mut commit = cleanup_index.acquire_commit(&CancellationToken::new()).await?;
            cleanup_index.delete_vector_rows(&mut commit, &stale_for_cleanup,
                "namespace = 'memory' AND generation = 2".into()).await
        });
        let queued = tokio::time::timeout(std::time::Duration::from_secs(10), waiting.notified()).await;
        let written = if queued.is_ok() {
            // Deliberately native-only: lexical absence cannot prove native absence.
            index.embed_vectors_in_commit("project", &[document(2, "source.rs", "real queued leftover")],
                &cancel, None, Some(&mut ahead)).await
        } else {
            Err(anyhow::anyhow!("Cleanup did not queue behind the earlier writer"))
        };
        let stale_observation = stale.count_rows(Some("generation = 2".into())).await;
        let lexical_absence = index.count_docs_in_generation("project", "memory", 2);
        drop(ahead);
        // Join on every ordinary error path before any fixture teardown.
        let cleaned = cleanup.await;
        written?;
        ensure!(lexical_absence? == 0, "The real queued leftover must have no lexical counterpart");
        ensure!(stale_observation? == 0 && stale.version().await? == original_version,
            "The supplied preopened handle must actually be stale for this regression");
        cleaned??;
        let latest = table(&index).await?;
        ensure!(latest.count_rows(Some("generation = 2".into())).await? == 0,
            "A real leftover committed ahead of cleanup must not be skipped using the stale handle");
        ensure!(latest.version().await? >= original_version + 2,
            "Both the queued write and its real cleanup must commit");
        ensure!(has_revision(&latest, 1, "needle selected").await?);
        ensure!(reader.version().await? == original_version
            && has_revision(&reader, 1, "needle selected").await?);
        ensure!(stale.version().await? == original_version,
            "Reopening the mutation view must not move the caller's shared stale Table");
        Ok(())
    });
    drop(runtime);
    owned.close()?;
    result
}
