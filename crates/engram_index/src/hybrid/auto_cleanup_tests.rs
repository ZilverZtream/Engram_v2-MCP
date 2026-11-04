//! Real legacy defaults must never prune behind Engram's reader barriers.
use super::*;
use anyhow::{Result, ensure};
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

fn doc(namespace: &str, id: &str) -> IndexDoc {
    IndexDoc { generation:1, chunk_id:1, path:RelPath::new("source.rs"), language:"rust".into(),
        content:"needle retained source".into(), namespace:namespace.into(), author:None, timestamp:None,
        start_line:1, end_line:1, doc_id:id.into(), content_hash:ContentHash::compute(b"needle retained source").0 }
}

async fn native(index: &HybridSearchEngine) -> Result<lancedb::Table> {
    let epoch = index.pin_vector_epoch("project")?;
    index.open_vector_epoch_table(&epoch).await
}

async fn legacy_store(root: &Path) -> Result<HybridSearchEngine> {
    let index = HybridSearchEngine::new(root.join("fts"), root.join("vectors"), &engram_core::Config::default()).await?;
    let source = doc("memory", "selected");
    let mut lexical = index.clone();
    lexical.embedding_backend = "fts_only".into();
    lexical.prepare_full_source_snapshot("project", 0, 1, &[RelPath::new("source.rs")], &CancellationToken::new()).await?;
    lexical.index_docs("project", std::slice::from_ref(&source), &CancellationToken::new()).await?;
    lexical.seal_source_snapshot("project", 1)?;
    drop(lexical);
    let owner = vector_epochs::OwnedTable::new("project", "project_project", &index.embedder)?;
    index.vector_routes.write_owned(&owner)?;
    let vector = index.embedder.embed(&source.content).await?;
    let batch = crate::vector::create_record_batch("project", "memory", 1,
        &[build_pk("project", "memory", 1, "selected")], &["selected".into()],
        &[source.content_hash.clone()], &[1], &["source.rs".into()], &["rust".into()],
        &[None], &[None], &[vector], index.embedder.dimension())?;
    let schema = Arc::new(batch.schema().as_ref().clone().with_metadata(owner.metadata()));
    let batch = arrow_array::RecordBatch::try_new(schema.clone(), batch.columns().to_vec())?;
    let reader = arrow_array::RecordBatchIterator::new(vec![Ok(batch)], schema);
    // Actual locked SDK creation defaults, not a manufactured config map.
    let table = index.native_store.capture().expect("owned native fixture").connection.create_table(&owner.table, reader).execute().await?;
    owner.validate_table(&table).await?;
    index.vector_routes.publish(vector_epochs::Record { version:1, project:"project".into(),
        table:owner.table.clone(), model_tag:owner.model_tag.clone(), dimension:owner.dimension,
        creation_token:owner.creation_token.clone(), baseline_rows:1, baseline_digest:String::new(), retired:Vec::new() }, index.query_embedder.clone())?;
    // Create a real native history at version19. A migration commit at20
    // would trigger default automatic cleanup if it retained the old policy.
    for _ in 1..19 {
        table.as_native().ok_or_else(|| anyhow::anyhow!("Expected owned native fixture"))?
            .update_config([("lance.auto_cleanup.interval".into(), "20".into()),
            ("lance.auto_cleanup.older_than".into(), "14days".into())]).await?;
    }
    ensure!(table.version().await? == 19);
    let mut tags = table.tags().await?;
    tags.create("held-fixture-tag", 1).await?;
    drop(tags);
    drop(table);
    Ok(index)
}

fn sentinel(root: &Path) -> Result<PathBuf> {
    let path = root.join("vectors/project_project.lance/data/aged-fixture-sentinel.lance");
    std::fs::write(&path, b"aged unknown human bytes must survive automatic cleanup migration")?;
    let file = std::fs::OpenOptions::new().write(true).open(&path)?;
    file.set_times(std::fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000)))?;
    drop(file);
    Ok(path)
}

fn inventory(root: &Path) -> Result<BTreeMap<PathBuf, String>> {
    let mut pending = vec![root.join("vectors/project_project.lance")];
    let mut result = BTreeMap::new();
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() { pending.push(entry.path()); }
            else {
                ensure!(result.len() < 512 && entry.metadata()?.len() <= 2 * 1024 * 1024);
                result.insert(entry.path().strip_prefix(root)?.to_owned(), blake3::hash(&std::fs::read(entry.path())?).to_hex().to_string());
            }
        }
    }
    Ok(result)
}

#[test]
fn native_legacy_auto_cleanup_migration_preserves_aged_unknown_files_readers_source_and_tags() -> Result<()> {
    let owned = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let result = runtime.block_on(async {
        let index = legacy_store(owned.path()).await?;
        let source = index.pin_source_generation("project", 1)?;
        let old = native(&index).await?;
        old.checkout(1).await?;
        let unknown = sentinel(owned.path())?;
        let before = inventory(owned.path())?;
        index.index_docs("project", &[doc("history", "new-history")], &CancellationToken::new()).await?;
        let current = native(&index).await?;
        ensure!(current.version().await? == 21, "Exactly one metadata migration plus the requested native merge must commit");
        let dataset = current.dataset().unwrap().get().await?;
        crate::vector::validate_history_config(&dataset.manifest().config, false)?;
        drop(dataset);
        ensure!(old.version().await? == 1 && old.count_rows(None).await? == 1);
        let mut tags = current.tags().await?;
        ensure!(tags.get_version("held-fixture-tag").await? == 1);
        index.validate_source_snapshot_guard("project", 1, &source)?;
        ensure!(index.count_docs_in_generation("project", "memory", 1)? == 1);
        let after = inventory(owned.path())?;
        ensure!(before.iter().all(|(path, bytes)| after.get(path) == Some(bytes)),
            "Migration must preserve every existing native file, including aged unknown bytes");
        index.index_docs("project", &[doc("history", "second-history")], &CancellationToken::new()).await?;
        let next = native(&index).await?;
        ensure!(next.version().await? == 22, "An ordinary next write must not repeat the metadata migration");
        tags.delete("held-fixture-tag").await?;
        drop(tags);
        let before_prune = inventory(owned.path())?;
        ensure!(matches!(index.prune_vector_history("project", &CancellationToken::new()).await?,
            NativeHistoryPruneOutcome::Deferred { reason } if reason.contains("Unverified")),
            "An aged unknown file must block explicit deletion even after safe policy migration");
        ensure!(inventory(owned.path())? == before_prune && std::fs::read(&unknown)?.starts_with(b"aged unknown human bytes"));
        drop(next); drop(current); drop(old); drop(source); drop(index);
        Ok(())
    });
    drop(runtime);
    let cleanup = owned.close();
    result?; cleanup?; Ok(())
}

#[test]
fn cancelled_native_policy_migration_keeps_backend_authority_until_metadata_and_write_complete() -> Result<()> {
    let owned = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let result = runtime.block_on(async {
        let index = legacy_store(owned.path()).await?;
        let _source = index.pin_source_generation("project", 1)?;
        let unknown = sentinel(owned.path())?;
        let pause = Arc::new(vector_mutations::NativeWritePause { entered:Default::default(), release:Default::default(), started:Default::default() });
        struct Release(Arc<vector_mutations::NativeWritePause>);
        impl Drop for Release { fn drop(&mut self) { self.0.release.notify_one(); } }
        let release = Release(pause.clone());
        let complete = Arc::new(tokio::sync::Notify::new());
        let mut worker = index.clone();
        worker.native_write_pause = Some(pause.clone());
        worker.native_commit_completed = Some(complete.clone());
        let request = tokio::spawn(async move {
            worker.index_docs("project", &[doc("history", "cancelled-waiter")], &CancellationToken::new()).await
        });
        tokio::time::timeout(Duration::from_secs(15), pause.entered.notified()).await?;
        request.abort();
        ensure!(request.await.unwrap_err().is_cancelled());
        ensure!(index.begin_project_mutation("project").is_err(), "Cancelled waiter released the pending metadata worker's authority");
        drop(release);
        tokio::time::timeout(Duration::from_secs(15), complete.notified()).await?;
        let current = native(&index).await?;
        ensure!(current.version().await? == 21 && current.count_rows(None).await? == 2);
        ensure!(std::fs::read(&unknown)?.starts_with(b"aged unknown human bytes"));
        ensure!(index.count_docs_in_generation("project", "memory", 1)? == 1);
        drop(index.begin_project_mutation("project")?);
        drop(current); drop(_source); drop(index);
        Ok(())
    });
    drop(runtime);
    let cleanup = owned.close();
    result?; cleanup?; Ok(())
}

#[test]
fn unsupported_native_manual_cleanup_policy_refuses_managed_write_without_native_mutation() -> Result<()> {
    let owned = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let result = runtime.block_on(async {
        let index = legacy_store(owned.path()).await?;
        let table = native(&index).await?;
        table.as_native().ok_or_else(|| anyhow::anyhow!("Expected owned native fixture"))?
            .update_config([("lance.auto_cleanup.interval".into(), "73".into())]).await?;
        let unknown = sentinel(owned.path())?;
        let before = inventory(owned.path())?;
        let refused = index.index_docs("project", &[doc("history", "refused")], &CancellationToken::new()).await;
        ensure!(refused.as_ref().is_err_and(|error| format!("{error:#}").contains("Unsupported native table configuration")));
        ensure!(inventory(owned.path())? == before && std::fs::read(unknown)?.starts_with(b"aged unknown human bytes"));
        // The public raw upsert path may not turn an existing unowned table
        // with the same SDK defaults into a managed migration target either.
        use futures::TryStreamExt;
        let mut rows = table.query().limit(1).execute().await?;
        let sample = rows.try_next().await?.ok_or_else(|| anyhow::anyhow!("Missing real fixture source row"))?;
        while rows.try_next().await?.is_some() {}
        drop(rows);
        let schema = Arc::new(sample.schema().as_ref().clone().with_metadata(Default::default()));
        let reader = arrow_array::RecordBatchIterator::new(vec![Ok(arrow_array::RecordBatch::new_empty(schema.clone()))], schema);
        let unowned = index.native_store.capture().expect("owned native fixture").connection.create_table("unowned_legacy", reader).execute().await?;
        let unowned_version = unowned.version().await?;
        let refused = crate::vector::upsert_vectors(&unowned, vec![sample]).await;
        ensure!(refused.as_ref().is_err_and(|error| error.to_string().contains("unowned table preserved"))
            && unowned.version().await? == unowned_version && unowned.count_rows(None).await? == 0,
            "An unowned legacy table must remain unmodified after a real attempted upsert");
        drop(unowned);
        drop(table); drop(index);
        Ok(())
    });
    drop(runtime);
    let cleanup = owned.close();
    result?; cleanup?; Ok(())
}
