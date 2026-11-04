use super::*;
use anyhow::{Result, ensure};
use std::time::Duration;

fn doc(namespace: &str, text: &str) -> IndexDoc {
    IndexDoc {
        namespace: namespace.into(),
        generation: 1,
        chunk_id: 1,
        path: RelPath::new("source.rs"),
        language: "rust".into(),
        content: text.into(),
        content_hash: ContentHash::compute(text.as_bytes()).0,
        doc_id: "stable".into(),
        author: None,
        timestamp: None,
        start_line: 1,
        end_line: 1,
    }
}

async fn engine(root: &Path) -> Result<HybridSearchEngine> {
    HybridSearchEngine::new(
        root.join("fts"),
        root.join("vectors"),
        &engram_core::Config::default(),
    )
    .await
}

async fn prepare(index: &HybridSearchEngine) -> Result<()> {
    let cancel = CancellationToken::new();
    index
        .prepare_full_source_snapshot("project", 0, 1, &[RelPath::new("source.rs")], &cancel)
        .await?;
    index
        .index_docs(
            "project",
            &[doc("memory", "needle current source")],
            &cancel,
        )
        .await?;
    index.seal_source_snapshot("project", 1)?;
    for step in 0..12 {
        index
            .index_docs(
                "project",
                &[doc("history", &format!("retained history update {step}"))],
                &cancel,
            )
            .await?;
    }
    Ok(())
}

async fn verify_selected_source(index: &HybridSearchEngine) -> Result<()> {
    use futures::TryStreamExt;
    use lancedb::query::{ExecutableQuery, QueryBase, Select};
    let body = "needle current source";
    let pk = build_pk("project", "memory", 1, "stable");
    let lexical = index
        .get_doc_by_pk(&pk)?
        .ok_or_else(|| anyhow::anyhow!("Selected source PK vanished"))?;
    ensure!(
        lexical.0.as_str() == "source.rs" && lexical.2 == body,
        "Selected physical source bytes changed"
    );
    let expected_hash = ContentHash::compute(body.as_bytes()).0;
    let epoch = index.pin_vector_epoch("project")?;
    let worker = index.clone();
    index
        .run_native_vector_query(epoch.clone(), &CancellationToken::new(), async move {
            let table = worker.open_vector_epoch_table(&epoch).await?;
            let mut rows = table
                .query()
                .only_if("namespace = 'memory' AND doc_id = 'stable'")
                .select(Select::Columns(vec![
                    "pk".into(),
                    "path".into(),
                    "content_hash".into(),
                    "generation".into(),
                ]))
                .limit(2)
                .execute()
                .await?;
            let mut seen = 0;
            while let Some(batch) = rows.try_next().await? {
                let strings = |name| {
                    batch
                        .column_by_name(name)
                        .and_then(|column| {
                            column.as_any().downcast_ref::<arrow_array::StringArray>()
                        })
                        .ok_or_else(|| anyhow::anyhow!("Missing native source column {name}"))
                };
                let generations = batch
                    .column_by_name("generation")
                    .and_then(|column| column.as_any().downcast_ref::<arrow_array::UInt64Array>())
                    .ok_or_else(|| anyhow::anyhow!("Missing native generation"))?;
                for row in 0..batch.num_rows() {
                    ensure!(
                        strings("pk")?.value(row) == pk
                            && strings("path")?.value(row) == "source.rs"
                            && strings("content_hash")?.value(row) == expected_hash
                            && generations.value(row) == 1,
                        "Native source physical identity/hash differs from selected lexical bytes"
                    );
                    seen += 1;
                }
            }
            ensure!(
                seen == 1,
                "Selected native physical PK must occur exactly once"
            );
            Ok(())
        })
        .await
}

fn rotation(outcome: NativeHistoryPruneOutcome) -> Result<NativeSessionRotationOutcome> {
    match outcome {
        NativeHistoryPruneOutcome::Pruned { cache_rotation, .. } => Ok(cache_rotation),
        other => anyhow::bail!("Expected completed physical maintenance: {other:?}"),
    }
}

fn private_case(
    test: impl for<'a> FnOnce(
        &'a Path,
    )
        -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + 'a>>,
) -> Result<()> {
    let owned = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let result = runtime.block_on(test(owned.path()));
    drop(runtime);
    let cleanup = owned.close();
    match (result, cleanup) {
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("Owned fixture cleanup failed: {cleanup}")))
        }
        (Err(error), _) => Err(error),
        (Ok(()), cleanup) => cleanup.map_err(Into::into),
    }
}

#[test]
fn explicit_history_rotation_releases_obsolete_sdk_session_for_all_clones_preserving_source_and_reopen()
-> Result<()> {
    private_case(|root| {
        Box::pin(async move {
            let index = engine(root).await?;
            prepare(&index).await?;
            let alias = index.clone();
            verify_selected_source(&index).await?;
            let selected = index.pin_source_generation("project", 1)?;
            index
                .index_docs(
                    "peer",
                    &[doc("memory", "peer current source")],
                    &CancellationToken::new(),
                )
                .await?;
            let held_epoch = index.pin_vector_epoch("peer")?;
            let held_native = index.open_vector_epoch_table(&held_epoch).await?;
            held_native.checkout(held_native.version().await?).await?;
            let old_session = {
                let current = index.native_store.capture()?;
                Arc::downgrade(&current.session)
            };
            let before = index.retention_observation("project");
            ensure!(
                matches!(
                    index
                        .prune_vector_history("project", &CancellationToken::new())
                        .await?,
                    NativeHistoryPruneOutcome::Deferred { .. }
                ),
                "A native reader of another table shares this cache owner"
            );
            ensure!(held_native.count_rows(None).await? == 1);
            ensure!(
                index.retention_observation("project")["native_cache"]["session_generation"]
                    == before["native_cache"]["session_generation"]
            );
            drop(held_native);
            drop(held_epoch);
            let rotated = rotation(
                alias
                    .prune_vector_history("project", &CancellationToken::new())
                    .await?,
            )?;
            ensure!(
                matches!(
                    rotated,
                    NativeSessionRotationOutcome::Rotated { generation: 1, .. }
                ),
                "{rotated:?}"
            );
            ensure!(
                old_session.upgrade().is_none(),
                "An engine clone retained the obsolete SDK session"
            );
            verify_selected_source(&alias).await?;
            ensure!(
                index.count_vectors("project").await? == 2
                    && alias.count_vectors("peer").await? == 1
            );
            index.validate_source_snapshot_guard("project", 1, &selected)?;
            ensure!(
                index.paths_in_generation("project", Some("memory"), 1)?
                    == std::collections::BTreeSet::from(["source.rs".into()])
            );
            alias
                .index_docs(
                    "project",
                    &[doc("history", "next actual write")],
                    &CancellationToken::new(),
                )
                .await?;
            ensure!(index.count_vectors("project").await? == 2);
            drop(selected);
            drop(alias);
            drop(index);
            let reopened = engine(root).await?;
            verify_selected_source(&reopened).await?;
            ensure!(
                reopened.count_vectors("project").await? == 2
                    && reopened.count_vectors("peer").await? == 1
            );
            reopened
                .index_docs(
                    "project",
                    &[doc("history", "write after cold reopen")],
                    &CancellationToken::new(),
                )
                .await?;
            ensure!(reopened.count_vectors("project").await? == 2);
            Ok(())
        })
    })
}

#[test]
fn candidate_failure_and_retained_sdk_owner_preserve_data_and_bound_cache_generations() -> Result<()>
{
    private_case(|root| {
        Box::pin(async move {
            let index = engine(root).await?;
            prepare(&index).await?;
            let retained_sdk_session = index.native_store.capture()?.session.clone();
            let old = Arc::downgrade(&retained_sdk_session);
            index
                .native_store
                .fail_candidate
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let failed = rotation(
                index
                    .prune_vector_history("project", &CancellationToken::new())
                    .await?,
            )?;
            ensure!(matches!(
                failed,
                NativeSessionRotationOutcome::Deferred { .. }
            ));
            ensure!(
                index.retention_observation("project")["native_cache"]["session_generation"] == 0
            );
            ensure!(index.count_vectors("project").await? == 2 && old.upgrade().is_some());
            index
                .index_docs(
                    "project",
                    &[doc("history", "next write after candidate failure")],
                    &CancellationToken::new(),
                )
                .await?;
            // This error occurs only AFTER a real candidate SDK connection and
            // table open. Retain its actual Session as an independent SDK owner.
            let failed_owner = Arc::new(std::sync::Mutex::new(None::<Arc<lancedb::Session>>));
            let captured = failed_owner.clone();
            *index.native_store.post_open.lock().unwrap() = Some(Box::new(move |session| {
                *captured.lock().unwrap() = Some(session);
                anyhow::bail!("Controlled post-open candidate validation refusal")
            }));
            let post_open_failure = rotation(
                index
                    .prune_vector_history("project", &CancellationToken::new())
                    .await?,
            )?;
            ensure!(matches!(
                post_open_failure,
                NativeSessionRotationOutcome::Deferred { .. }
            ));
            ensure!(
                index.retention_observation("project")["native_cache"]["session_generation"] == 0
            );
            let failed_weak = {
                let owner = failed_owner.lock().unwrap();
                Arc::downgrade(owner.as_ref().unwrap())
            };
            ensure!(
                failed_weak.upgrade().is_some()
                    && index.retention_observation("project")["native_cache"]["candidate_session_wrapper_owners"]
                        == 1
            );
            verify_selected_source(&index).await?;
            ensure!(
                matches!(index.prune_vector_history("project",&CancellationToken::new()).await?,
            NativeHistoryPruneOutcome::Deferred { reason } if reason.contains("failed native candidate"))
            );
            drop(failed_owner.lock().unwrap().take());
            ensure!(
                failed_weak.upgrade().is_none(),
                "SDK candidate owner did not drain after legitimate owner release"
            );
            verify_selected_source(&index).await?;
            index
                .index_docs(
                    "project",
                    &[doc("history", "next write after actual candidate release")],
                    &CancellationToken::new(),
                )
                .await?;
            ensure!(matches!(
                rotation(
                    index
                        .prune_vector_history("project", &CancellationToken::new())
                        .await?
                )?,
                NativeSessionRotationOutcome::Rotated { generation: 1, .. }
            ));
            ensure!(
                index.retention_observation("project")["native_cache"]["retired_session_wrapper_owners"]
                    == 1
            );
            index
                .index_docs(
                    "project",
                    &[doc("history", "another current owner write")],
                    &CancellationToken::new(),
                )
                .await?;
            ensure!(
                matches!(rotation(index.prune_vector_history("project", &CancellationToken::new()).await?)?,
            NativeSessionRotationOutcome::Deferred { reason, .. } if reason.contains("prior native session"))
            );
            ensure!(
                index.retention_observation("project")["native_cache"]["session_generation"] == 1
            );
            drop(retained_sdk_session);
            ensure!(old.upgrade().is_none());
            index
                .index_docs(
                    "project",
                    &[doc("history", "fresh write after real owner drop")],
                    &CancellationToken::new(),
                )
                .await?;
            ensure!(matches!(
                rotation(
                    index
                        .prune_vector_history("project", &CancellationToken::new())
                        .await?
                )?,
                NativeSessionRotationOutcome::Rotated { generation: 2, .. }
            ));
            ensure!(index.count_vectors("project").await? == 2);
            let before = index.retention_observation("project");
            ensure!(matches!(
                rotation(
                    index
                        .prune_vector_history("project", &CancellationToken::new())
                        .await?
                )?,
                NativeSessionRotationOutcome::Skipped { .. }
            ));
            ensure!(
                index.retention_observation("project")["native_cache"]["session_generation"]
                    == before["native_cache"]["session_generation"],
                "A no-history maintenance call must not cold caches again"
            );
            Ok(())
        })
    })
}

#[test]
fn cancelled_reader_and_maintenance_waiters_keep_global_native_exclusion_until_worker_finishes()
-> Result<()> {
    private_case(|root| {
        Box::pin(async move {
            let index = engine(root).await?;
            prepare(&index).await?;
            index
                .index_docs(
                    "peer",
                    &[doc("memory", "peer current source")],
                    &CancellationToken::new(),
                )
                .await?;
            let read_pause = Arc::new(vector_reads::NativeReadPause {
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
            let release_read = ReleaseRead(read_pause.clone());
            let reader = {
                let mut worker = index.clone();
                worker.native_read_pause = Some(read_pause.clone());
                tokio::spawn(async move { worker.count_vectors("peer").await })
            };
            tokio::time::timeout(Duration::from_secs(15), read_pause.entered.notified()).await?;
            reader.abort();
            ensure!(reader.await.unwrap_err().is_cancelled());
            ensure!(
                matches!(
                    index
                        .prune_vector_history("project", &CancellationToken::new())
                        .await?,
                    NativeHistoryPruneOutcome::Deferred { .. }
                ),
                "Cancelled peer's backend owner still needs its cache generation"
            );
            drop(release_read);
            tokio::time::timeout(Duration::from_secs(15), read_pause.completed.notified()).await?;
            let prune_pause = Arc::new(vector_history::HistoryPrunePause {
                entered: Default::default(),
                release: Default::default(),
                completed: Default::default(),
            });
            struct ReleasePrune(Arc<vector_history::HistoryPrunePause>);
            impl Drop for ReleasePrune {
                fn drop(&mut self) {
                    self.0.release.notify_one();
                }
            }
            let release_prune = ReleasePrune(prune_pause.clone());
            let old = {
                let current = index.native_store.capture()?;
                Arc::downgrade(&current.session)
            };
            let maintenance = {
                let mut worker = index.clone();
                worker.history_prune_pause = Some(prune_pause.clone());
                tokio::spawn(async move {
                    worker
                        .prune_vector_history("project", &CancellationToken::new())
                        .await
                })
            };
            tokio::time::timeout(Duration::from_secs(15), prune_pause.entered.notified()).await?;
            maintenance.abort();
            ensure!(maintenance.await.unwrap_err().is_cancelled());
            ensure!(
                index.count_vectors("peer").await.is_err(),
                "A peer reader bypassed the global cache-generation fence"
            );
            ensure!(index.begin_project_mutation("project").is_err());
            drop(release_prune);
            tokio::time::timeout(Duration::from_secs(15), prune_pause.completed.notified()).await?;
            ensure!(old.upgrade().is_none());
            ensure!(
                index.count_vectors("peer").await? == 1
                    && index.count_vectors("project").await? == 2
            );
            Ok(())
        })
    })
}
