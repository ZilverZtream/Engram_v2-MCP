//! Initialize an absent vector table from stored text, without re-ingestion.
use super::*;

/// Refusals before any initialization work. These do not establish a new
/// defect in an already-good table and are distinct from admitted failures.
#[derive(Debug, thiserror::Error)]
pub enum VectorInitializationRejection {
    #[error("Configure an embedding backend before initializing vectors")]
    NoBackend,
    #[error("Vector table already exists; initialize_vectors refuses to replace existing vectors")]
    ExistingTable,
    #[error("No stored documents to initialize")]
    NoStoredDocuments,
    #[error("Vector initialization requires a build with the vector feature")]
    FeatureDisabled,
    #[error("vector recovery or mutation is active; retry the operation")]
    InProgress,
}

impl HybridSearchEngine {
    /// Maintenance operation for an index originally built with `fts_only`.
    /// Existing vector tables are never replaced. Readers see no new vectors
    /// until every stored document has been embedded and the table is published.
    #[cfg(feature = "vector")]
    pub async fn initialize_vectors(
        &self,
        project_id: &str,
        cancel: &CancellationToken,
    ) -> anyhow::Result<usize> {
        self.run_vector_rebuild(project_id, cancel, false).await
    }

    /// Recover every retained physical document in every namespace without
    /// reading project files or changing the published source generation.
    #[cfg(feature = "vector")]
    pub async fn recover_vectors(
        &self,
        project_id: &str,
        cancel: &CancellationToken,
    ) -> anyhow::Result<usize> {
        self.run_vector_rebuild(project_id, cancel, true).await
    }

    #[cfg(feature = "vector")]
    async fn run_vector_rebuild(
        &self,
        project_id: &str,
        cancel: &CancellationToken,
        replace: bool,
    ) -> anyhow::Result<usize> {
        let worker = self.clone();
        let project = project_id.to_owned();
        let cancel = cancel.child_token();
        let _cancel_on_drop = cancel.clone().drop_guard();
        // Dropping the request cancels cooperatively, but never drops native
        // writes or their exclusive permit before the backend actually settles.
        tokio::spawn(async move {
            if replace {
                if let Err(error) = worker.collect_retired_vectors_inner(&project, &cancel).await {
                    if error.is::<vector_epochs::RetiredCleanupDeferred>() {
                        tracing::warn!(project_id = %project,
                            "Retired cleanup deferred before recovery: {error:#}");
                    } else if !matches!(error.downcast_ref::<mutation::CoordinationError>(), Some(mutation::CoordinationError::Busy)) {
                        return Err(error);
                    }
                }
            }
            let rows = worker.rebuild_vectors(&project, &cancel, replace).await?;
            if let Err(error) = worker.collect_retired_vectors_inner(&project, &cancel).await {
                tracing::warn!(project_id = %project, "Vector recovery completed; retired cleanup deferred: {error:#}");
            }
            Ok(rows)
        })
            .await
            .map_err(|e| anyhow::anyhow!("Vector recovery worker did not complete: {e}"))?
    }

    #[cfg(feature = "vector")]
    async fn rebuild_vectors(
        &self,
        project_id: &str,
        cancel: &CancellationToken,
        replace: bool,
    ) -> anyhow::Result<usize> {
        if self.is_lexical_only() {
            return Err(VectorInitializationRejection::NoBackend.into());
        }
        anyhow::ensure!(
            !project_id.is_empty()
                && project_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "Invalid project identity for vector initialization"
        );
        anyhow::ensure!(
            !cancel.is_cancelled(),
            "Vector initialization cancelled before start"
        );
        let recovery = self
            .vector_routes
            .reserve_recovery(&self.snapshots.mutations, project_id)
            .map_err(|error| {
                // Classify refusal at the admission boundary. A nested Busy
                // after staging must never be interpreted as no work started.
                if matches!(
                    error.downcast_ref::<mutation::CoordinationError>(),
                    Some(mutation::CoordinationError::Busy)
                ) {
                    anyhow::Error::from(VectorInitializationRejection::InProgress)
                } else {
                    error
                }
            })?;
        // Keep exclusion through error cleanup too. Dropping the permit as the
        // inner future returns would let another recovery overlap native drop.
        let mut recovery = Some(recovery);
        let previous_epoch = self.vector_routes.pin_recovery(
            project_id,
            &self.query_embedder,
            self.snapshots.clone(),
        )?;
        anyhow::ensure!(
            previous_epoch
                .record
                .as_ref()
                .is_none_or(|r| r.retired.len() < 64),
            "Vector retirement capacity exhausted; collect unleased owned epochs before recovery"
        );
        let canonical = format!("project_{}", project_id.replace('-', "_"));
        let root = self._lance_dir.canonicalize()?;
        let destination = root.join(format!("{canonical}.lance"));
        if !(replace
            || (!destination.exists()
                && !self
                    .native_store.capture()?.connection
                    .table_names()
                    .execute()
                    .await?
                    .contains(&canonical)
                && previous_epoch.record.is_none()))
        {
            return Err(VectorInitializationRejection::ExistingTable.into());
        }

        // Prevent text writes while a stable reader snapshot is embedded. This
        // writer is never used to add/delete documents or publish a generation.
        let _writer_allocation = self
            .memory_budget
            .as_ref()
            .map(|budget| {
                AllocationGuard::try_new(
                    budget,
                    self.tantivy_writer_memory as u64,
                    Subsystem::Tantivy,
                    "vector recovery stable source writer",
                )
            })
            .transpose()?;
        let _writer = self.acquire_writer_blocking("vector initialization")?;
        if !(replace
            || (!destination.exists()
                && !self
                    .native_store.capture()?.connection
                    .table_names()
                    .execute()
                    .await?
                    .contains(&canonical)
                && previous_epoch.record.is_none()))
        {
            return Err(VectorInitializationRejection::ExistingTable.into());
        }
        let reader = self.tantivy_index.reader()?;
        let searcher = reader.searcher();
        let query = TermQuery::new(
            Term::from_field_text(self.fields.project_id, project_id),
            IndexRecordOption::Basic,
        );
        let expected = searcher.search(&query, &tantivy::collector::Count)?;
        if !replace && expected == 0 {
            return Err(VectorInitializationRejection::NoStoredDocuments.into());
        }

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let stage = format!(
            "{canonical}_{}_{}_{stamp}",
            if replace {
                "recovering"
            } else {
                "initializing"
            },
            std::process::id()
        );
        anyhow::ensure!(
            !self
                .native_store.capture()?.connection
                .table_names()
                .execute()
                .await?
                .iter()
                .any(
                    |name| (name.starts_with(&format!("{canonical}_initializing_"))
                        || name.starts_with(&format!("{canonical}_recovering_")))
                        && name != &previous_epoch.table
                        && !previous_epoch
                            .record
                            .as_ref()
                            .is_some_and(|r| r.retired.iter().any(|owner| &owner.table == name))
                ),
            "An unfinished vector initialization table exists. Inspect that staging table before retrying; existing staging data will not be deleted or duplicated"
        );
        let stage_path = root.join(format!("{stage}.lance"));
        anyhow::ensure!(
            !stage_path.exists(),
            "Vector initialization staging path already exists"
        );
        let mut stage_owned = false;
        let stage_owner = vector_epochs::OwnedTable::new(project_id, &stage, &self.embedder)?;
        self.vector_routes.write_owned(&stage_owner)?;
        let result: anyhow::Result<usize> = async {
            // Creation is exclusive at the table level; never reuse a prior
            // failed attempt's data or touch another table on cleanup.
            let table = crate::vector::create_owned_table(&self.native_store.capture()?.connection, &stage, self.embedder.dimension(), stage_owner.metadata()).await?;
            stage_owned = true;
            stage_owner.validate_table(&table).await?;
            drop(table);
            let (processed, coverage_digest) = self.scan_retained_vectors(
                project_id, &stage, &searcher, &query, cancel).await?;
            let table = self.native_store.capture()?.connection.open_table(&stage).execute().await?;
            let rows = table.count_rows(None).await?;
            drop(table);
            anyhow::ensure!(processed == expected && rows as usize == expected,
                "Vector initialization incomplete: expected {expected}, processed {processed}, stored {rows}");
            anyhow::ensure!(!cancel.is_cancelled(), "Vector initialization cancelled before publication");
            anyhow::ensure!(replace || !destination.exists(), "Vector destination appeared during initialization; refusing replacement");
            anyhow::ensure!(stage_path.canonicalize()?.parent() == Some(root.as_path()) && destination.parent() == Some(root.as_path()),
                "Vector publication paths must remain inside the configured index directory");
            // Keep the physical identity immutable. Both initialization and
            // replacement publish the exclusively created table through routing.
            let active_table = stage.clone();
            let reopened = self.native_store.capture()?.connection.open_table(&active_table).execute().await?;
            anyhow::ensure!(reopened.count_rows(None).await? as usize == expected, "Published vector destination is incomplete");
            stage_owner.validate_table(&reopened).await?;
            drop(reopened);
            let mut retired = previous_epoch.record.as_ref().map(|r| r.retired.clone()).unwrap_or_default();
            if let Some(previous) = &previous_epoch.record {
                if previous_epoch.table != active_table { retired.push(previous.ownership()); }
            }
            self.vector_routes.mark_publication(project_id, recovery.as_mut().expect("live recovery permit"))?;
            // Publication errors require authoritative reconciliation. If disk
            // or cache names the target, or either cannot be checked, preserve
            // it. Otherwise this call may discard only its owned stage, under
            // the still-live exclusive permit. Admission remains uncertain
            // until a later complete recovery successfully activates.
            stage_owned = false;
            if let Err(error) = self.vector_routes.publish(vector_epochs::Record {
                version: 1, project: project_id.to_owned(), table: active_table,
                model_tag: self.embedder.model_tag(), dimension: self.embedder.dimension(),
                creation_token: stage_owner.creation_token.clone(),
                baseline_rows: expected, baseline_digest: coverage_digest, retired,
            }, self.query_embedder.clone()) {
                stage_owned = self.vector_routes.unpublished_after_failure(&stage_owner)
                    .unwrap_or(false);
                return Err(error);
            }
            self.vector_routes.finish_activation(recovery.take().expect("live recovery permit"))?;
            Ok(processed)
        }.await;
        if stage_owned {
            // A failed/cancelled provider never publishes partial vectors.
            // Cleanup is limited to this call's newly-created staging table.
            if let Err(error) = self.drop_owned_vector_table(&stage_owner).await {
                return Err(anyhow::anyhow!(
                    "Vector initialization failed ({:?}); owned staging cleanup failed at {}: {error}",
                    result.as_ref().err(),
                    stage_path.display()
                ));
            }
        }
        result
    }

    #[cfg(not(feature = "vector"))]
    pub async fn initialize_vectors(
        &self,
        _project_id: &str,
        _cancel: &CancellationToken,
    ) -> anyhow::Result<usize> {
        Err(VectorInitializationRejection::FeatureDisabled.into())
    }

    #[cfg(not(feature = "vector"))]
    pub async fn recover_vectors(
        &self,
        _project_id: &str,
        _cancel: &CancellationToken,
    ) -> anyhow::Result<usize> {
        Err(VectorInitializationRejection::FeatureDisabled.into())
    }

    #[cfg(not(feature = "vector"))]
    pub async fn collect_retired_vectors(
        &self,
        _project: &str,
        _cancel: &CancellationToken,
    ) -> anyhow::Result<usize> {
        Ok(0)
    }

    #[cfg(feature = "vector")]
    pub(super) async fn verify_recovered_vector_rows(
        &self,
        project: &str,
        table: &str,
        docs: &[IndexDoc],
    ) -> anyhow::Result<()> {
        use futures::TryStreamExt;
        use lancedb::query::{ExecutableQuery, QueryBase, Select};
        let verification_bytes = docs
            .iter()
            .try_fold(0u64, |sum, doc| {
                let pk = (project.len() as u64)
                    .checked_add(doc.namespace.len() as u64)
                    .and_then(|n| n.checked_add(doc.doc_id.len() as u64))
                    .and_then(|n| n.checked_add(24));
                pk.and_then(|n| n.checked_mul(8))
                    .and_then(|n| n.checked_add((doc.content_hash.len() as u64).checked_mul(2)?))
                    .and_then(|n| n.checked_add(1024))
                    .and_then(|n| sum.checked_add(n))
            })
            .ok_or_else(|| anyhow::anyhow!("Recovery verification size overflow"))?;
        // Keys, comparison map, SQL filter and projected result buffers remain
        // live together after the embedding reservation has been released.
        let _verification_allocation = self
            .memory_budget
            .as_ref()
            .map(|budget| {
                AllocationGuard::try_new(
                    budget,
                    verification_bytes,
                    Subsystem::LanceDb,
                    "vector recovery identity verification",
                )
            })
            .transpose()?;
        let mut expected = std::collections::BTreeMap::new();
        for doc in docs {
            let pk = build_pk(project, &doc.namespace, doc.generation, &doc.doc_id);
            anyhow::ensure!(
                expected.insert(pk, doc.content_hash.as_str()).is_none(),
                "Duplicate stored document identity during recovery"
            );
        }
        let keys = expected.keys().cloned().collect::<Vec<_>>();
        let table = self.native_store.capture()?.connection.open_table(table).execute().await?;
        let mut rows = table
            .query()
            .select(Select::Columns(vec!["pk".into(), "content_hash".into()]))
            .only_if(crate::vector::build_pk_filter(&keys))
            .execute()
            .await?;
        while let Some(batch) = rows.try_next().await? {
            let strings = |name| {
                batch
                    .column_by_name(name)
                    .and_then(|array| array.as_any().downcast_ref::<arrow_array::StringArray>())
                    .ok_or_else(|| anyhow::anyhow!("Recovered vector table lacks {name}"))
            };
            let pks = strings("pk")?;
            let hashes = strings("content_hash")?;
            for n in 0..batch.num_rows() {
                anyhow::ensure!(
                    expected.remove(pks.value(n)) == Some(hashes.value(n)),
                    "Recovered vector identity/content mismatch"
                );
            }
        }
        anyhow::ensure!(
            expected.is_empty(),
            "Recovered vector table is missing required document identities"
        );
        Ok(())
    }
}

#[cfg(all(test, feature = "vector"))]
mod tests {
    use super::*;

    struct TestEmbedder {
        cancel: Option<CancellationToken>,
        fail: bool,
        calls: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl engram_ml::Embedder for TestEmbedder {
        fn dimension(&self) -> usize {
            384
        }
        async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            let calls = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if calls >= 65 {
                if self.fail {
                    anyhow::bail!("deliberate provider failure after a completed batch");
                }
                if let Some(cancel) = &self.cancel {
                    cancel.cancel();
                }
            }
            Ok(vec![0.1; 384])
        }
    }

    async fn fixture() -> (tempfile::TempDir, HybridSearchEngine) {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = engram_core::Config {
            embedding_backend: "fts_only".into(),
            ..Default::default()
        };
        let engine =
            HybridSearchEngine::new(tmp.path().join("fts"), tmp.path().join("vectors"), &cfg)
                .await
                .unwrap();
        for pid in ["selected", "foreign"] {
            for ns in ["memory", "history", "memory_bank"] {
                let docs: Vec<_> = (0..150)
                    .map(|n| IndexDoc {
                        generation: if ns == "memory" { 3 } else { 0 },
                        chunk_id: n,
                        path: RelPath::new(&format!("folder/{n}.txt")),
                        language: "text".into(),
                        content: format!("{pid} {ns} exact text {n}"),
                        namespace: ns.into(),
                        author: Some("author".into()),
                        timestamp: Some(123),
                        start_line: 1,
                        end_line: 1,
                        doc_id: format!("{ns}-{n}"),
                        content_hash: format!("hash-{n}"),
                    })
                    .collect();
                engine
                    .index_docs(pid, &docs, &CancellationToken::new())
                    .await
                    .unwrap();
            }
        }
        drop(engine);
        let mut engine =
            HybridSearchEngine::new(tmp.path().join("fts"), tmp.path().join("vectors"), &cfg)
                .await
                .unwrap();
        engine.embedding_backend = "local".into();
        engine.embedder = Arc::new(TestEmbedder {
            cancel: None,
            fail: false,
            calls: Default::default(),
        });
        (tmp, engine)
    }

    fn stored_snapshot(engine: &HybridSearchEngine) -> Vec<String> {
        use tantivy::schema::Document;
        let searcher = engine.tantivy_index.reader().unwrap().searcher();
        let mut docs = Vec::new();
        for (ord, segment) in searcher.segment_readers().iter().enumerate() {
            for id in 0..segment.max_doc() {
                if !segment.is_deleted(id) {
                    let doc: tantivy::TantivyDocument =
                        searcher.doc(DocAddress::new(ord as u32, id)).unwrap();
                    docs.push(doc.to_json(&engine.tantivy_index.schema()));
                }
            }
        }
        docs.sort();
        docs
    }

    #[tokio::test]
    async fn vector_initialization_waits_for_publication_and_bulk_writers_across_engine_reload() {
        for bulk in [false, true] {
            let (temporary, engine) = fixture().await;
            let before = stored_snapshot(&engine);
            let publication = (!bulk).then(|| engine.begin_mutation().unwrap());
            let writer = bulk.then(|| engine.create_bulk_writer().unwrap());
            drop(engine);
            let reopened = HybridSearchEngine::new(
                temporary.path().join("fts"),
                temporary.path().join("vectors"),
                &engram_core::Config {
                    embedding_backend: "local".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            let busy = reopened
                .initialize_vectors("selected", &CancellationToken::new())
                .await
                .unwrap_err();
            assert!(matches!(
                busy.downcast_ref::<VectorInitializationRejection>(),
                Some(VectorInitializationRejection::InProgress)
            ));
            assert!(
                reopened
                    .native_store.capture().expect("owned native fixture").connection
                    .table_names()
                    .execute()
                    .await
                    .unwrap()
                    .is_empty()
            );
            drop(writer);
            drop(publication);
            assert_eq!(
                reopened
                    .initialize_vectors("selected", &CancellationToken::new())
                    .await
                    .unwrap(),
                450
            );
            assert_eq!(stored_snapshot(&reopened), before);
        }
    }

    struct PausedEmbedder {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        started: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl engram_ml::Embedder for PausedEmbedder {
        fn dimension(&self) -> usize {
            384
        }

        async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            if !self.started.swap(true, std::sync::atomic::Ordering::SeqCst) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            Ok(vec![0.1; 384])
        }
    }

    #[tokio::test]
    async fn vector_initialization_excludes_another_engines_writes_during_provider_await() {
        use futures::FutureExt;
        let (temporary, mut engine) = fixture().await;
        let before = stored_snapshot(&engine);
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        engine.embedder = Arc::new(PausedEmbedder {
            entered: entered.clone(),
            release: release.clone(),
            started: Default::default(),
        });
        let other = HybridSearchEngine::new(
            temporary.path().join("fts"),
            temporary.path().join("vectors"),
            &engram_core::Config {
                embedding_backend: "local".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let initialization = tokio::spawn(async move {
            engine
                .initialize_vectors("selected", &CancellationToken::new())
                .await
        });
        if tokio::time::timeout(std::time::Duration::from_secs(10), entered.notified())
            .await
            .is_err()
        {
            initialization.abort();
            let result = initialization.await;
            panic!("initialization did not reach the provider: {result:?}");
        }
        let attempt = std::panic::AssertUnwindSafe(async {
            let doc = IndexDoc {
                generation: 9,
                chunk_id: 999,
                path: RelPath::new("unexpected.txt"),
                language: "text".into(),
                content: "must not be written during recovery".into(),
                namespace: "memory".into(),
                author: None,
                timestamp: None,
                start_line: 1,
                end_line: 1,
                doc_id: "unexpected".into(),
                content_hash: "unexpected".into(),
            };
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                other.index_docs("selected", &[doc], &CancellationToken::new()),
            )
            .await
        })
        .catch_unwind()
        .await;
        // Release and join before asserting, including unwinding in the attempted
        // mutation. No paused provider or database task may outlive this fixture.
        release.notify_one();
        let completed = initialization.await;
        let attempt = match attempt {
            Ok(attempt) => attempt,
            Err(panic) => std::panic::resume_unwind(panic),
        };
        assert_eq!(completed.unwrap().unwrap(), 450);
        match attempt {
            Ok(Err(error)) => assert!(
                error
                    .downcast_ref::<mutation::CoordinationError>()
                    .is_some()
            ),
            other => panic!("a concurrent write did not fail retryably: {other:?}"),
        }
        assert_eq!(stored_snapshot(&other), before);
        assert_eq!(other.count_vectors("selected").await.unwrap(), 450);
    }

    #[tokio::test]
    async fn vector_recovery_repairs_equal_count_corruption_and_survives_restart_without_source_changes()
     {
        let (temporary, engine) = fixture().await;
        engine
            .initialize_vectors("selected", &CancellationToken::new())
            .await
            .unwrap();
        let before = stored_snapshot(&engine);
        let held_previous = engine.pin_vector_epoch("selected").unwrap();
        let old = engine
            .native_store.capture().expect("owned native fixture").connection
            .open_table(&engine.pin_vector_epoch("selected").unwrap().table)
            .execute()
            .await
            .unwrap();
        old.delete("namespace = 'history' AND doc_id = 'history-0'")
            .await
            .unwrap();
        let stray = IndexDoc {
            generation: 0,
            chunk_id: 99999,
            path: RelPath::new("obsolete.txt"),
            language: "text".into(),
            content: "obsolete vector without source".into(),
            namespace: "history".into(),
            author: None,
            timestamp: None,
            start_line: 1,
            end_line: 1,
            doc_id: "obsolete".into(),
            content_hash: "obsolete-hash".into(),
        };
        engine
            .embed_and_upsert_vectors_to_table(
                "selected",
                &[stray],
                &CancellationToken::new(),
                Some(&engine.pin_vector_epoch("selected").unwrap().table),
            )
            .await
            .unwrap();
        // Independent Lance handles retain their opened version. Inspect the
        // current store after the intentionally separate corruption write.
        drop(old);
        let old = engine
            .native_store.capture().expect("owned native fixture").connection
            .open_table(&engine.pin_vector_epoch("selected").unwrap().table)
            .execute()
            .await
            .unwrap();
        assert_eq!(
            old.count_rows(None).await.unwrap(),
            450,
            "counts alone cannot prove complete recovery"
        );
        assert_eq!(
            engine
                .recover_vectors("selected", &CancellationToken::new())
                .await
                .unwrap(),
            450
        );
        let epoch = engine.pin_vector_epoch("selected").unwrap();
        let recovered = engine
            .native_store.capture().expect("owned native fixture").connection
            .open_table(&epoch.table)
            .execute()
            .await
            .unwrap();
        assert_eq!(
            recovered
                .count_rows(Some("doc_id = 'obsolete'".into()))
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            recovered
                .count_rows(Some(
                    "namespace = 'history' AND doc_id = 'history-0' AND content_hash = 'hash-0'"
                        .into()
                ))
                .await
                .unwrap(),
            1
        );
        for namespace in ["memory", "history", "memory_bank"] {
            assert_eq!(
                recovered
                    .count_rows(Some(format!("namespace = '{namespace}'")))
                    .await
                    .unwrap(),
                150
            );
        }
        assert_eq!(
            old.count_rows(None).await.unwrap(),
            450,
            "recovery must preserve the previous table for readers"
        );
        assert_eq!(stored_snapshot(&engine), before);
        drop(recovered);
        drop(old);
        drop(epoch);
        drop(held_previous);
        drop(engine);
        let reopened = HybridSearchEngine::new(
            temporary.path().join("fts"),
            temporary.path().join("vectors"),
            &engram_core::Config {
                embedding_backend: "local".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(reopened.count_vectors("selected").await.unwrap(), 450);
        assert_eq!(stored_snapshot(&reopened), before);
    }

    struct DifferentSpace;

    #[async_trait::async_trait]
    impl engram_ml::Embedder for DifferentSpace {
        fn dimension(&self) -> usize {
            384
        }
        fn model_tag(&self) -> String {
            "different-weights-same-dimension".into()
        }
        async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            Ok(vec![0.2; 384])
        }
    }

    #[tokio::test]
    async fn vector_recovery_keeps_inflight_query_on_its_original_provider_and_rejects_stale_writers()
     {
        let (temporary, mut engine) = fixture().await;
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        engine.query_embedder = Arc::new(PausedEmbedder {
            entered: entered.clone(),
            release: release.clone(),
            started: Default::default(),
        });
        engine
            .initialize_vectors("selected", &CancellationToken::new())
            .await
            .unwrap();
        let before = stored_snapshot(&engine);
        let engine = Arc::new(engine);
        let mut replacement = HybridSearchEngine::new(
            temporary.path().join("fts"),
            temporary.path().join("vectors"),
            &engram_core::Config {
                embedding_backend: "local".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let provider = Arc::new(DifferentSpace);
        replacement.embedder = provider.clone();
        replacement.query_embedder = provider;
        let query = HybridQuery {
            project_id: "selected".into(),
            namespace: "memory".into(),
            generation: 3,
            text: "retained source".into(),
            top_k: 5,
            fts_mode: "strict".into(),
            include_path_prefixes: None,
            exclude_path_prefixes: None,
            include_path_suffixes: None,
            language_filters: None,
            author_filter: None,
            date_after: None,
            date_before: None,
            use_mmr: false,
        };
        let task_engine = engine.clone();
        let task_query = query.clone();
        let task = tokio::spawn(async move {
            task_engine
                .vector_search(&task_query, &CancellationToken::new())
                .await
        });
        if tokio::time::timeout(std::time::Duration::from_secs(10), entered.notified())
            .await
            .is_err()
        {
            task.abort();
            let outcome = task.await;
            panic!("query did not reach original provider: {outcome:?}");
        }
        let recovery = replacement
            .recover_vectors("selected", &CancellationToken::new())
            .await;
        release.notify_one();
        let original_query = task.await;
        // Settle the paused request before assertions or fixture teardown.
        assert_eq!(recovery.unwrap(), 450);
        assert_eq!(original_query.unwrap().unwrap().len(), 5);
        assert_eq!(
            replacement
                .vector_search(&query, &CancellationToken::new())
                .await
                .unwrap()
                .len(),
            5
        );
        let stale = IndexDoc {
            generation: 4,
            chunk_id: 777,
            path: RelPath::new("wrong-model.txt"),
            language: "text".into(),
            content: "must never be written".into(),
            namespace: "memory".into(),
            author: None,
            timestamp: None,
            start_line: 1,
            end_line: 1,
            doc_id: "stale-model".into(),
            content_hash: "stale".into(),
        };
        assert!(
            engine
                .index_docs("selected", &[stale], &CancellationToken::new())
                .await
                .unwrap_err()
                .to_string()
                .contains("VEC1")
        );
        assert_eq!(stored_snapshot(&engine), before);
    }

    #[tokio::test]
    async fn missing_managed_vectors_and_unidentified_legacy_content_cannot_be_silently_recreated()
    {
        let (_temporary, engine) = fixture().await;
        let before = stored_snapshot(&engine);
        // An absent vector directory beside retained source/history is recovery,
        // not an empty new project, even before an identity receipt exists.
        assert!(
            engine
                .vector_write_table("selected")
                .await
                .unwrap_err()
                .to_string()
                .contains("VEC1")
        );
        engine
            .initialize_vectors("selected", &CancellationToken::new())
            .await
            .unwrap();
        engine
            .native_store.capture().expect("owned native fixture").connection
            .drop_table(&engine.pin_vector_epoch("selected").unwrap().table, &[])
            .await
            .unwrap();
        let doc = IndexDoc {
            generation: 4,
            chunk_id: 42,
            path: RelPath::new("new.txt"),
            language: "text".into(),
            content: "must not appear on failure".into(),
            namespace: "memory".into(),
            author: None,
            timestamp: None,
            start_line: 1,
            end_line: 1,
            doc_id: "unpublished".into(),
            content_hash: "new-hash".into(),
        };
        assert!(
            engine
                .index_docs("selected", &[doc], &CancellationToken::new())
                .await
                .unwrap_err()
                .to_string()
                .contains("VEC1")
        );
        assert_eq!(stored_snapshot(&engine), before);
        assert!(
            engine
                .native_store.capture().expect("owned native fixture").connection
                .table_names()
                .execute()
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            engine
                .recover_vectors("selected", &CancellationToken::new())
                .await
                .unwrap(),
            450
        );
        assert_eq!(stored_snapshot(&engine), before);
    }

    #[tokio::test]
    async fn vector_recovery_resumes_uncertain_publication_without_admitting_writes() {
        let (_temporary, engine) = fixture().await;
        engine
            .initialize_vectors("selected", &CancellationToken::new())
            .await
            .unwrap();
        let before = stored_snapshot(&engine);
        engine.vector_routes.fail_next_activation();
        assert!(
            engine
                .recover_vectors("selected", &CancellationToken::new())
                .await
                .unwrap_err()
                .to_string()
                .contains("after durable")
        );
        assert!(
            engine.begin_mutation().is_err(),
            "ambiguous activation must keep write admission closed"
        );
        assert_eq!(
            engine
                .recover_vectors("selected", &CancellationToken::new())
                .await
                .unwrap(),
            450
        );
        assert!(engine.begin_mutation().is_ok());
        assert_eq!(engine.count_vectors("selected").await.unwrap(), 450);
        assert_eq!(stored_snapshot(&engine), before);
    }

    #[tokio::test]
    async fn a_vector_directory_cannot_be_shared_by_independent_reader_registries() {
        let (temporary, engine) = fixture().await;
        engine
            .initialize_vectors("selected", &CancellationToken::new())
            .await
            .unwrap();
        let held = engine.pin_vector_epoch("selected").unwrap();
        #[cfg(windows)]
        for lock in [
            temporary
                .path()
                .join("fts")
                .join(".source-snapshot-catalog.lock"),
            temporary
                .path()
                .join("vectors")
                .join(".engram-vector-owner.lock"),
        ] {
            assert!(lock.is_file());
            assert!(
                std::fs::remove_file(&lock).is_err(),
                "a live ownership lock must resist replacement"
            );
            assert!(lock.is_file());
        }
        let table_name = held.table.clone();
        let config = engram_core::Config {
            embedding_backend: "local".into(),
            ..Default::default()
        };
        let incompatible_source = temporary.path().join("another-fts");
        let vectors = temporary.path().join("vectors");
        let attempt =
            HybridSearchEngine::new(incompatible_source.clone(), vectors.clone(), &config).await;
        assert!(
            attempt
                .err()
                .expect("independent index must not share vector storage")
                .to_string()
                .contains("Vector directory")
        );
        let attempt = HybridSearchEngine::new(
            temporary.path().join("fts"),
            temporary.path().join("other-vectors"),
            &config,
        )
        .await;
        assert!(
            attempt
                .err()
                .expect("one source index must retain its vector binding")
                .to_string()
                .contains("different vector directories")
        );
        drop(engine);
        let attempt =
            HybridSearchEngine::new(incompatible_source.clone(), vectors.clone(), &config).await;
        assert!(
            attempt.is_err(),
            "a retained reader must retain process and root ownership"
        );
        drop(held);
        let reopened = HybridSearchEngine::new(incompatible_source, vectors, &config)
            .await
            .unwrap();
        let preserved = reopened
            .native_store.capture().expect("owned native fixture").connection
            .open_table(&table_name)
            .execute()
            .await
            .unwrap();
        assert_eq!(
            preserved.count_rows(None).await.unwrap(),
            450,
            "failed bindings cannot modify existing vector data"
        );
    }

    #[tokio::test]
    async fn retired_cleanup_waits_for_all_provider_views_across_engine_reload() {
        let (temporary, engine) = fixture().await;
        engine
            .initialize_vectors("selected", &CancellationToken::new())
            .await
            .unwrap();
        let before = stored_snapshot(&engine);
        let first_view = engine.pin_vector_epoch("selected").unwrap();
        let old_name = first_view.table.clone();
        let config = engram_core::Config {
            embedding_backend: "local".into(),
            ..Default::default()
        };
        let other = HybridSearchEngine::new(
            temporary.path().join("fts"),
            temporary.path().join("vectors"),
            &config,
        )
        .await
        .unwrap();
        let second_view = other.pin_vector_epoch("selected").unwrap();
        engine
            .recover_vectors("selected", &CancellationToken::new())
            .await
            .unwrap();
        drop(first_view);
        drop(other);
        drop(engine);
        // The remaining reader owns the shared Catalog/process lock even when
        // all original engine objects have been released.
        let reopened = HybridSearchEngine::new(
            temporary.path().join("fts"),
            temporary.path().join("vectors"),
            &config,
        )
        .await
        .unwrap();
        assert_eq!(
            reopened
                .collect_retired_vectors("selected", &CancellationToken::new())
                .await
                .unwrap(),
            0
        );
        assert!(
            reopened
                .native_store.capture().expect("owned native fixture").connection
                .table_names()
                .execute()
                .await
                .unwrap()
                .contains(&old_name)
        );
        drop(second_view);
        assert_eq!(
            reopened
                .collect_retired_vectors("selected", &CancellationToken::new())
                .await
                .unwrap(),
            1
        );
        assert!(
            !reopened
                .native_store.capture().expect("owned native fixture").connection
                .table_names()
                .execute()
                .await
                .unwrap()
                .contains(&old_name)
        );
        assert_eq!(reopened.count_vectors("selected").await.unwrap(), 450);
        assert_eq!(stored_snapshot(&reopened), before);
    }

    #[tokio::test]
    async fn retired_cleanup_preserves_tables_with_wrong_creation_identity() {
        for changed in ["token", "project", "table", "model"] {
            let (_temporary, engine) = fixture().await;
            engine
                .initialize_vectors("selected", &CancellationToken::new())
                .await
                .unwrap();
            let before = stored_snapshot(&engine);
            let held = engine.pin_vector_epoch("selected").unwrap();
            let owner = held.record.as_ref().unwrap().ownership();
            engine
                .recover_vectors("selected", &CancellationToken::new())
                .await
                .unwrap();
            drop(held);
            engine
                .native_store.capture().expect("owned native fixture").connection
                .drop_table(&owner.table, &[])
                .await
                .unwrap();
            let mut occupant = owner.clone();
            match changed {
                "token" => occupant.creation_token = "f".repeat(64),
                "project" => occupant.project = "another-project".into(),
                "table" => occupant.table = "copied_table_identity".into(),
                _ => occupant.model_tag = "other-model-same-dimension".into(),
            }
            let table = crate::vector::create_owned_table(
                &engine.native_store.capture().expect("owned native fixture").connection,
                &owner.table,
                owner.dimension,
                occupant.metadata(),
            )
            .await
            .unwrap();
            drop(table);
            engine
                .embed_and_upsert_vectors_to_table(
                    "selected",
                    &[new_doc("sentinel")],
                    &CancellationToken::new(),
                    Some(&owner.table),
                )
                .await
                .unwrap();
            let error = engine
                .collect_retired_vectors("selected", &CancellationToken::new())
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("ownership"),
                "{changed}: {error:#}"
            );
            let preserved = engine
                .native_store.capture().expect("owned native fixture").connection
                .open_table(&owner.table)
                .execute()
                .await
                .unwrap();
            assert_eq!(
                preserved.count_rows(None).await.unwrap(),
                1,
                "{changed}: wrong occupant must survive"
            );
            assert!(
                engine.begin_mutation().is_ok(),
                "an invalid retired occupant must not disable current writes"
            );
            assert_eq!(engine.count_vectors("selected").await.unwrap(), 450);
            assert_eq!(stored_snapshot(&engine), before);
        }
    }

    #[tokio::test]
    async fn wrong_active_table_token_blocks_writes_before_lexical_mutation() {
        let (_temporary, engine) = fixture().await;
        engine
            .initialize_vectors("selected", &CancellationToken::new())
            .await
            .unwrap();
        let before = stored_snapshot(&engine);
        let epoch = engine.pin_vector_epoch("selected").unwrap();
        let mut occupant = epoch.record.as_ref().unwrap().ownership();
        engine
            .native_store.capture().expect("owned native fixture").connection
            .drop_table(&epoch.table, &[])
            .await
            .unwrap();
        occupant.creation_token = "0".repeat(64);
        let table = crate::vector::create_owned_table(
            &engine.native_store.capture().expect("owned native fixture").connection,
            &epoch.table,
            occupant.dimension,
            occupant.metadata(),
        )
        .await
        .unwrap();
        assert!(
            engine
                .index_docs(
                    "selected",
                    &[new_doc("unpublished")],
                    &CancellationToken::new()
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("ownership")
        );
        assert_eq!(table.count_rows(None).await.unwrap(), 0);
        assert_eq!(stored_snapshot(&engine), before);
    }

    #[tokio::test]
    async fn vector_recovery_never_claims_or_collects_unreceipted_legacy_tables() {
        let (_temporary, engine) = fixture().await;
        let before = stored_snapshot(&engine);
        let legacy_name = "project_selected";
        drop(
            crate::vector::create_new_table(&engine.native_store.capture().expect("owned native fixture").connection, legacy_name, 384)
                .await
                .unwrap(),
        );
        engine
            .embed_and_upsert_vectors_to_table(
                "selected",
                &[new_doc("legacy-sentinel")],
                &CancellationToken::new(),
                Some(legacy_name),
            )
            .await
            .unwrap();
        assert_eq!(
            engine
                .recover_vectors("selected", &CancellationToken::new())
                .await
                .unwrap(),
            450
        );
        assert_eq!(
            engine
                .collect_retired_vectors("selected", &CancellationToken::new())
                .await
                .unwrap(),
            0
        );
        let legacy = engine
            .native_store.capture().expect("owned native fixture").connection
            .open_table(legacy_name)
            .execute()
            .await
            .unwrap();
        assert_eq!(legacy.count_rows(None).await.unwrap(), 1);
        assert_eq!(engine.count_vectors("selected").await.unwrap(), 450);
        assert_eq!(stored_snapshot(&engine), before);
    }

    #[tokio::test]
    async fn dropping_recovery_request_keeps_exclusion_until_provider_and_cleanup_settle() {
        let (temporary, mut engine) = fixture().await;
        let before = stored_snapshot(&engine);
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        engine.embedder = Arc::new(PausedEmbedder {
            entered: entered.clone(),
            release: release.clone(),
            started: Default::default(),
        });
        let other = HybridSearchEngine::new(
            temporary.path().join("fts"),
            temporary.path().join("vectors"),
            &engram_core::Config {
                embedding_backend: "local".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let request = tokio::spawn(async move {
            engine
                .initialize_vectors("selected", &CancellationToken::new())
                .await
        });
        if tokio::time::timeout(std::time::Duration::from_secs(10), entered.notified())
            .await
            .is_err()
        {
            request.abort();
            let _ = request.await;
            release.notify_one();
            panic!("recovery did not reach paused provider");
        }
        request.abort();
        let outcome = request.await;
        let excluded = other.begin_mutation().is_err();
        release.notify_one();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if other.begin_mutation().is_ok() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "cancelled owned worker did not settle"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(outcome.unwrap_err().is_cancelled());
        assert!(
            excluded,
            "request cancellation must not release exclusion while the provider still runs"
        );
        assert!(
            other
                .native_store.capture().expect("owned native fixture").connection
                .table_names()
                .execute()
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(stored_snapshot(&other), before);
    }

    async fn tiny_ready_store() -> (tempfile::TempDir, HybridSearchEngine) {
        let temporary = tempfile::tempdir().unwrap();
        std::fs::create_dir(temporary.path().join(".git")).unwrap();
        let config = engram_core::Config {
            embedding_backend: "fts_only".into(),
            ..Default::default()
        };
        let mut first = HybridSearchEngine::new(
            temporary.path().join("fts"),
            temporary.path().join("vectors"),
            &config,
        )
        .await
        .unwrap();
        let mut seed = new_doc("seed");
        seed.namespace = "memory_bank".into();
        first
            .index_docs("selected", &[seed], &CancellationToken::new())
            .await
            .unwrap();
        first.embedding_backend = "local".into();
        first.embedder = Arc::new(TestEmbedder {
            cancel: None,
            fail: false,
            calls: Default::default(),
        });
        first
            .initialize_vectors("selected", &CancellationToken::new())
            .await
            .unwrap();

        (temporary, first)
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn recovery_retry_after_locked_route_preserves_the_old_view() {
        use std::os::windows::fs::OpenOptionsExt;
        let (temporary, engine) = tiny_ready_store().await;
        let before = stored_snapshot(&engine);
        let held = engine.pin_vector_epoch("selected").unwrap();
        let old_table = held.table.clone();
        let route = temporary
            .path()
            .join("fts/source-snapshots/vector-routes")
            .join(format!("{}.json", blake3::hash(b"selected").to_hex()));
        let old_bytes = std::fs::read(&route).unwrap();
        // Read sharing permits authoritative validation, but excludes the
        // rename/delete required to replace this real routing file.
        let locked = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&route)
            .unwrap();
        let failed = engine
            .recover_vectors("selected", &CancellationToken::new())
            .await;
        let unchanged_route = std::fs::read(&route).unwrap() == old_bytes;
        drop(locked);
        let old_rows = engine
            .open_vector_epoch_table(&held)
            .await
            .unwrap()
            .count_rows(None)
            .await
            .unwrap();
        let retry = engine
            .recover_vectors("selected", &CancellationToken::new())
            .await;
        let names = engine.native_store.capture().expect("owned native fixture").connection.table_names().execute().await.unwrap();
        let current = engine.pin_vector_epoch("selected").unwrap();
        let admission_restored = engine.begin_mutation().is_ok();
        let after = stored_snapshot(&engine);
        let current_rows = engine.count_vectors("selected").await;
        let current_table = current.table.clone();
        drop(current);
        drop(held);
        drop(engine);
        temporary.close().unwrap();
        assert!(
            failed.is_err(),
            "a locked durable route must reject publication"
        );
        assert!(unchanged_route);
        assert_eq!(old_rows, 1, "the held old reader must remain usable");
        assert_eq!(retry.unwrap(), 1, "unlocking must permit complete recovery");
        assert!(admission_restored);
        assert_ne!(old_table, current_table);
        assert_eq!(
            names.len(),
            2,
            "only the held old and current tables remain"
        );
        assert_eq!(current_rows.unwrap(), 1);
        assert_eq!(after, before);
    }

    #[tokio::test]
    async fn recovery_retry_preserves_invalid_retired_data_and_collects_other_epochs() {
        let (temporary, engine) = tiny_ready_store().await;
        let before = stored_snapshot(&engine);
        let first = engine.pin_vector_epoch("selected").unwrap();
        let invalid_owner = first.record.as_ref().unwrap().ownership();
        engine
            .recover_vectors("selected", &CancellationToken::new())
            .await
            .unwrap();
        let second = engine.pin_vector_epoch("selected").unwrap();
        let valid_retired = second.table.clone();
        engine
            .recover_vectors("selected", &CancellationToken::new())
            .await
            .unwrap();
        drop(first);
        drop(second);
        // A different occupant at the retired path is never ours to remove.
        engine
            .native_store.capture().expect("owned native fixture").connection
            .drop_table(&invalid_owner.table, &[])
            .await
            .unwrap();
        let mut occupant = invalid_owner.clone();
        occupant.creation_token = "f".repeat(64);
        let table = crate::vector::create_owned_table(
            &engine.native_store.capture().expect("owned native fixture").connection,
            &invalid_owner.table,
            occupant.dimension,
            occupant.metadata(),
        )
        .await
        .unwrap();
        drop(table);
        engine
            .embed_and_upsert_vectors_to_table(
                "selected",
                &[new_doc("sentinel")],
                &CancellationToken::new(),
                Some(&invalid_owner.table),
            )
            .await
            .unwrap();
        let cleanup = engine
            .collect_retired_vectors("selected", &CancellationToken::new())
            .await;
        let names_after_cleanup = engine.native_store.capture().expect("owned native fixture").connection.table_names().execute().await.unwrap();
        let retry = engine
            .recover_vectors("selected", &CancellationToken::new())
            .await;
        let sentinel = engine
            .native_store.capture().expect("owned native fixture").connection
            .open_table(&invalid_owner.table)
            .execute()
            .await
            .unwrap();
        let sentinel_rows = sentinel.count_rows(None).await.unwrap();
        let current_rows = engine.count_vectors("selected").await;
        let receipt_retained = engine.vector_routes.read_owned(&invalid_owner).is_ok();
        let after = stored_snapshot(&engine);
        drop(sentinel);
        drop(engine);
        temporary.close().unwrap();
        assert!(cleanup.is_err(), "deferred cleanup must be reported");
        assert!(
            !names_after_cleanup.contains(&valid_retired),
            "one invalid owner must not prevent collecting other verified unleased epochs"
        );
        assert_eq!(
            retry.unwrap(),
            1,
            "retired cleanup must not disable fresh recovery below capacity"
        );
        assert_eq!(sentinel_rows, 1);
        assert!(receipt_retained);
        assert_eq!(current_rows.unwrap(), 1);
        assert_eq!(after, before);
    }

    #[tokio::test]
    async fn scoped_writes_reject_foreign_projects_and_roots_and_keep_recovery_excluded() {
        let (temporary, first) = tiny_ready_store().await;
        let context = first.begin_project_mutation("selected").unwrap();
        let scoped = first.with_mutation(&context).unwrap();
        let foreign = HybridSearchEngine::new(
            temporary.path().join("foreign-fts"),
            temporary.path().join("foreign-vectors"),
            &engram_core::Config {
                embedding_backend: "fts_only".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let wrong_root = foreign.with_mutation(&context).is_err();
        let wrong_project = scoped
            .index_docs(
                "foreign",
                &[new_doc("forbidden")],
                &CancellationToken::new(),
            )
            .await;
        let foreign_rows = first.count_docs("foreign");
        let writer = scoped.create_bulk_writer().unwrap();
        let bulk_wrong_root = foreign.with_bulk_writer(&writer, "selected").is_err();
        let bulk_wrong_project = first.with_bulk_writer(&writer, "foreign").is_err();
        let bulk_view = first.with_bulk_writer(&writer, "selected").unwrap();
        let finished = writer.finish();
        drop(context);
        drop(scoped);
        // The derived bulk view is an owned lease. It alone must keep
        // recovery excluded even after the writer and original scope finish.
        let excluded = first
            .recover_vectors("selected", &CancellationToken::new())
            .await;
        drop(bulk_view);
        let resumed = first
            .recover_vectors("selected", &CancellationToken::new())
            .await;
        drop(foreign);
        drop(first);
        temporary.close().unwrap();
        assert!(wrong_root);
        assert!(wrong_project.is_err());
        assert!(bulk_wrong_root && bulk_wrong_project);
        finished.unwrap();
        assert_eq!(foreign_rows.unwrap(), 0);
        assert!(excluded.is_err());
        assert_eq!(resumed.unwrap(), 1);
    }

    #[tokio::test]
    async fn overlapping_same_document_writes_keep_lexical_and_vector_content_identity_equal() {
        overlapping_write_contract(true, false).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_bulk_flush_cannot_resume_ordinary_writes_without_ownership() {
        let (temporary, base) = tiny_ready_store().await;
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let completed = Arc::new(tokio::sync::Notify::new());
        let mut a = base.clone();
        a.native_write_pause = Some(Arc::new(super::super::vector_mutations::NativeWritePause {
            entered: entered.clone(),
            release: release.clone(),
            started: Default::default(),
        }));
        a.native_commit_completed = Some(completed.clone());
        let cancel = CancellationToken::new();
        let mut writer = a
            .create_bulk_writer_async("selected", &cancel)
            .await
            .unwrap();
        let mut doc = new_doc("committed-after-disconnect");
        doc.namespace = "memory_bank".into();
        HybridSearchEngine::write_docs_to_writer(
            &a.fields(),
            &mut writer,
            "selected",
            &[doc.clone()],
        )
        .unwrap();
        let docs = [doc];
        let reached = {
            let flushing =
                a.embed_and_upsert_vectors_with_writer(&mut writer, "selected", &docs, &cancel);
            tokio::pin!(flushing);
            tokio::select! {
                _ = entered.notified() => true,
                _ = &mut flushing => false,
            }
            // Dropping this awaiting future does not return native authority
            // to the still-live caller-owned writer.
        };
        let mut forbidden = new_doc("forbidden-resume");
        forbidden.namespace = "memory_bank".into();
        let commit_refused = writer.commit().is_err();
        let add_refused = HybridSearchEngine::write_docs_to_writer(
            &a.fields(),
            &mut writer,
            "selected",
            &[forbidden.clone()],
        )
        .is_err();
        let embed_refused = a
            .embed_and_upsert_vectors_with_writer(&mut writer, "selected", &[forbidden], &cancel)
            .await
            .is_err();
        let finish_refused = writer.finish().is_err();
        release.notify_one();
        let native_done =
            tokio::time::timeout(std::time::Duration::from_secs(10), completed.notified())
                .await
                .is_ok();
        let mut next = new_doc("after-settlement");
        next.namespace = "memory_bank".into();
        let retried = base.index_docs("selected", &[next], &cancel).await;
        let forbidden_absent = base
            .stored_doc_by_pk(&build_pk("selected", "memory_bank", 0, "forbidden-resume"))
            .unwrap()
            .is_none();
        let lexical = base.count_docs("selected");
        let vectors = base.count_vectors("selected").await;
        drop(a);
        drop(base);
        temporary.close().unwrap();
        assert!(reached && native_done);
        assert!(commit_refused && add_refused && embed_refused && finish_refused);
        assert!(forbidden_absent);
        retried.unwrap();
        assert_eq!(lexical.unwrap(), 3);
        assert_eq!(vectors.unwrap(), 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_bulk_finalization_keeps_write_ownership_after_native_completion() {
        let (temporary, base) = tiny_ready_store().await;
        let native_entered = Arc::new(tokio::sync::Notify::new());
        let native_release = Arc::new(tokio::sync::Notify::new());
        let native_completed = Arc::new(tokio::sync::Notify::new());
        let finalizing = Arc::new(tokio::sync::Notify::new());
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let mut a = base.clone();
        a.native_write_pause = Some(Arc::new(super::super::vector_mutations::NativeWritePause {
            entered: native_entered.clone(),
            release: native_release.clone(),
            started: Default::default(),
        }));
        a.native_commit_completed = Some(native_completed.clone());
        a.bulk_finalize_pause = Some(Arc::new(super::super::commit::FinalizePause {
            entered: finalizing.clone(),
            release: std::sync::Mutex::new(finish_rx),
            started: Default::default(),
        }));
        let document = |label: &str| {
            let mut doc = new_doc("shared");
            doc.namespace = "memory_bank".into();
            doc.content = format!("finalization version {label}");
            doc.content_hash = engram_core::ContentHash::compute(doc.content.as_bytes()).0;
            doc
        };
        let doc_a = document("A");
        let request = tokio::spawn(async move {
            let cancel = CancellationToken::new();
            let mut writer = a.create_bulk_writer_async("selected", &cancel).await?;
            HybridSearchEngine::write_docs_to_writer(
                &a.fields(),
                &mut writer,
                "selected",
                &[doc_a.clone()],
            )?;
            a.embed_and_upsert_vectors_with_writer(&mut writer, "selected", &[doc_a], &cancel)
                .await?;
            writer.finish()
        });
        let entered = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            native_entered.notified(),
        )
        .await
        .is_ok();
        request.abort();
        // Never await the cancelled parent while its real Drop is paused.
        let paused =
            tokio::time::timeout(std::time::Duration::from_secs(10), finalizing.notified())
                .await
                .is_ok();
        native_release.notify_one();
        let native_done = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            native_completed.notified(),
        )
        .await
        .is_ok();
        // No test-owned A scope/authority survives. Probe recovery before B
        // owns any admission, so B cannot mask premature release of A's lease.
        let recovery_excluded = base
            .recover_vectors("selected", &CancellationToken::new())
            .await
            .is_err();
        let b_context = base.begin_project_mutation("selected").unwrap();
        let b = base.with_mutation(&b_context).unwrap();
        let cancel = CancellationToken::new();
        let (pending_after_native, cancelled) = {
            let lane = b.acquire_commit(&cancel);
            tokio::pin!(lane);
            let pending = match futures::poll!(&mut lane) {
                std::task::Poll::Pending => true,
                std::task::Poll::Ready(result) => {
                    drop(result);
                    false
                }
            };
            let _ = finish_tx.send(());
            let cancelled = request.await;
            if pending {
                drop(lane.await);
            }
            (pending, cancelled)
        };
        let doc_b = document("B");
        let expected = doc_b.content_hash.clone();
        let written = b.index_docs("selected", &[doc_b], &cancel).await;
        drop(b);
        drop(b_context);
        let epoch = base.pin_vector_epoch("selected").unwrap();
        let table = base.open_vector_epoch_table(&epoch).await.unwrap();
        let matching = table
            .count_rows(Some(format!(
                "doc_id = 'shared' AND content_hash = '{expected}'"
            )))
            .await;
        let lexical = base.count_docs("selected");
        let vectors = base.count_vectors("selected").await;
        drop(table);
        drop(epoch);
        drop(base);
        temporary.close().unwrap();
        assert!(
            entered && paused && native_done,
            "owned cancellation boundaries did not settle"
        );
        assert!(cancelled.unwrap_err().is_cancelled());
        assert!(recovery_excluded);
        assert!(
            pending_after_native,
            "native completion must not release another owner's unfinished lexical finalizer"
        );
        written.unwrap();
        assert_eq!(lexical.unwrap(), 2);
        assert_eq!(vectors.unwrap(), 2);
        assert_eq!(matching.unwrap(), 1);
    }

    async fn overlapping_write_contract(same_document: bool, bulk_b: bool) {
        let (temporary, base) = tiny_ready_store().await;
        let entered_a = Arc::new(tokio::sync::Notify::new());
        let entered_b = Arc::new(tokio::sync::Notify::new());
        let release_a = Arc::new(tokio::sync::Notify::new());
        let release_b = Arc::new(tokio::sync::Notify::new());
        let waiting_a = Arc::new(tokio::sync::Notify::new());
        let native_b = Arc::new(tokio::sync::Notify::new());
        let release_native_b = Arc::new(tokio::sync::Notify::new());
        let mut a = base.clone();
        let mut b = base.clone();
        a.embedder = Arc::new(PausedEmbedder {
            entered: entered_a.clone(),
            release: release_a.clone(),
            started: Default::default(),
        });
        b.embedder = Arc::new(PausedEmbedder {
            entered: entered_b.clone(),
            release: release_b.clone(),
            started: Default::default(),
        });
        a.commit_waiting = Some(waiting_a.clone());
        b.native_write_pause = Some(Arc::new(super::super::vector_mutations::NativeWritePause {
            entered: native_b.clone(),
            release: release_native_b.clone(),
            started: Default::default(),
        }));
        let document = |label: &str| {
            let mut doc = new_doc(if same_document { "shared" } else { label });
            doc.namespace = "memory_bank".into();
            doc.content = format!("source version {label}");
            doc.content_hash = engram_core::ContentHash::compute(doc.content.as_bytes()).0;
            doc
        };
        let doc_b = document("B");
        let mut task_b = tokio::spawn(async move {
            let cancel = CancellationToken::new();
            if bulk_b {
                let mut writer = b.create_bulk_writer_async("selected", &cancel).await?;
                HybridSearchEngine::write_docs_to_writer(
                    &b.fields(),
                    &mut writer,
                    "selected",
                    &[doc_b.clone()],
                )?;
                b.embed_and_upsert_vectors_with_writer(&mut writer, "selected", &[doc_b], &cancel)
                    .await?;
                writer.finish()
            } else {
                b.index_docs("selected", &[doc_b], &cancel).await
            }
        });
        let mut task_a = None;
        let mut early_a = None;
        let schedule = tokio::time::timeout(std::time::Duration::from_secs(15), async {
            entered_b.notified().await;
            let doc_a = document("A");
            task_a = Some(tokio::spawn(async move {
                a.index_docs("selected", &[doc_a], &CancellationToken::new())
                    .await
            }));
            tokio::select! {
                _ = waiting_a.notified() => {
                    // Serialization before embedding: finish B so A can enter.
                    release_b.notify_one();
                    native_b.notified().await;
                    release_native_b.notify_one();
                    entered_a.notified().await;
                    release_a.notify_one();
                }
                _ = entered_a.notified() => {
                    release_b.notify_one();
                    native_b.notified().await;
                    release_a.notify_one();
                    tokio::select! {
                        result = task_a.as_mut().unwrap() => { early_a = Some(result); }
                        _ = waiting_a.notified() => {}
                    }
                    // Serialization after embedding is also legitimate. A
                    // storage wait is an event, never inferred from a sleep.
                    release_native_b.notify_one();
                }
            }
        })
        .await;
        // Release every dependency before joining or reporting any failure.
        release_a.notify_one();
        release_b.notify_one();
        release_native_b.notify_one();
        if schedule.is_err() {
            if let Some(task) = &task_a {
                task.abort();
            }
            task_b.abort();
        }
        let result_a = match early_a {
            Some(result) => Some(result),
            None => match task_a {
                Some(task) => Some(task.await),
                None => None,
            },
        };
        let result_b = (&mut task_b).await;
        let lexical_hash = stored_snapshot(&base)
            .into_iter()
            .filter_map(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .find(|doc| {
                doc["doc_id"][0].as_str() == Some(if same_document { "shared" } else { "A" })
            })
            .and_then(|doc| doc["content_hash"][0].as_str().map(str::to_owned));
        let epoch = base.pin_vector_epoch("selected").unwrap();
        let table = base.open_vector_epoch_table(&epoch).await.unwrap();
        let matching = match &lexical_hash {
            Some(hash) => {
                table
                    .count_rows(Some(format!(
                        "doc_id = '{}' AND content_hash = '{hash}'",
                        if same_document { "shared" } else { "A" }
                    )))
                    .await
            }
            None => Ok(0),
        };
        let lexical = base.count_docs("selected");
        let vectors = base.count_vectors("selected").await;
        drop(table);
        drop(epoch);
        drop(base);
        temporary.close().unwrap();
        assert!(schedule.is_ok(), "controlled write schedule stalled");
        result_a.unwrap().unwrap().unwrap();
        result_b.unwrap().unwrap();
        assert_eq!(lexical.unwrap(), if same_document { 2 } else { 3 });
        assert_eq!(vectors.unwrap(), if same_document { 2 } else { 3 });
        assert!(lexical_hash.is_some());
        assert_eq!(
            matching.unwrap(),
            1,
            "the selected vector must identify the same content as the lexical row"
        );
    }

    #[tokio::test]
    async fn admitted_bulk_embedding_queues_without_rejection_or_store_skew() {
        overlapping_write_contract(false, true).await;
    }

    #[tokio::test]
    async fn already_admitted_distinct_writes_complete_without_lexical_vector_skew() {
        overlapping_write_contract(false, false).await;
    }

    #[tokio::test]
    async fn cancelled_native_upsert_cannot_be_overtaken_by_an_ordinary_retry() {
        let (temporary, mut engine) = fixture().await;
        engine
            .initialize_vectors("selected", &CancellationToken::new())
            .await
            .unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        engine.native_write_pause =
            Some(Arc::new(super::super::vector_mutations::NativeWritePause {
                entered: entered.clone(),
                release: release.clone(),
                started: Default::default(),
            }));
        let other = HybridSearchEngine::new(
            temporary.path().join("fts"),
            temporary.path().join("vectors"),
            &engram_core::Config {
                embedding_backend: "local".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let mut abandoned = new_doc("abandoned");
        abandoned.path = RelPath::new("new.txt");
        abandoned.generation = 4;
        let request = tokio::spawn(async move {
            engine
                .index_docs("selected", &[abandoned], &CancellationToken::new())
                .await
        });
        if tokio::time::timeout(std::time::Duration::from_secs(10), entered.notified())
            .await
            .is_err()
        {
            request.abort();
            let result = request.await;
            release.notify_one();
            panic!("upsert did not reach the native boundary: {result:?}");
        }
        request.abort();
        let outcome = request.await;
        let rejected_retry = other
            .prepare_source_snapshot(
                "selected",
                3,
                4,
                &[RelPath::new("new.txt")],
                &[],
                &CancellationToken::new(),
            )
            .await;
        let rejected_recovery = other
            .recover_vectors("selected", &CancellationToken::new())
            .await;
        release.notify_one();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if other.begin_mutation().is_ok() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "owned native upsert did not settle"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(outcome.unwrap_err().is_cancelled());
        assert!(
            rejected_retry.is_err(),
            "retry must not reuse generation 4 while an old write can still finish"
        );
        assert!(rejected_recovery.is_err());
        assert_eq!(
            other
                .count_docs_in_generation("selected", "memory", 3)
                .unwrap(),
            150
        );
        assert_eq!(
            other
                .count_vectors_in_generation("selected", 3)
                .await
                .unwrap(),
            150
        );
        other
            .prepare_source_snapshot(
                "selected",
                3,
                4,
                &[RelPath::new("new.txt")],
                &[],
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let mut replacement = new_doc("replacement");
        replacement.path = RelPath::new("new.txt");
        replacement.generation = 4;
        other
            .index_docs("selected", &[replacement], &CancellationToken::new())
            .await
            .unwrap();
        other.seal_source_snapshot("selected", 4).unwrap();
        assert!(
            other
                .stored_doc_by_pk(&build_pk("selected", "memory", 4, "abandoned"))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            other
                .count_docs_in_generation("selected", "memory", 4)
                .unwrap(),
            151
        );
        assert_eq!(
            other
                .count_vectors_in_generation("selected", 4)
                .await
                .unwrap(),
            151
        );
    }

    fn retained_history(start: u64) -> Vec<IndexDoc> {
        (start..start + 20)
            .map(|n| IndexDoc {
                generation: 0,
                chunk_id: n,
                path: RelPath::new(&format!("folder/{n}.txt")),
                language: "text".into(),
                content: format!("selected history exact text {n}"),
                namespace: "history".into(),
                author: Some("author".into()),
                timestamp: Some(123),
                start_line: 1,
                end_line: 1,
                doc_id: format!("history-{n}"),
                content_hash: format!("hash-{n}"),
            })
            .collect()
    }

    struct CountedProvider {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl engram_ml::Embedder for CountedProvider {
        fn dimension(&self) -> usize {
            384
        }
        async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(vec![0.1; 384])
        }
    }

    #[tokio::test]
    async fn live_embedding_buffers_prevent_overcommit_and_release_budget_after_native_completion()
    {
        let (temporary, mut first) = fixture().await;
        first
            .initialize_vectors("selected", &CancellationToken::new())
            .await
            .unwrap();
        let before_first = stored_snapshot(&first);
        let first_observer = first.clone();
        let budget = Arc::new(MemoryBudget::new(128 * 1024));
        first.memory_budget = Some(budget.clone());
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        first.embedder = Arc::new(PausedEmbedder {
            entered: entered.clone(),
            release: release.clone(),
            started: Default::default(),
        });
        let provider = Arc::new(CountedProvider {
            calls: Default::default(),
        });
        // Different index roots may write concurrently while sharing one
        // process budget; same-root writes now queue before allocating.
        let mut second = HybridSearchEngine::new(
            temporary.path().join("second-fts"),
            temporary.path().join("second-vectors"),
            &engram_core::Config {
                embedding_backend: "fts_only".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let mut history = retained_history(0);
        history.extend(retained_history(20));
        second
            .index_docs("selected", &history, &CancellationToken::new())
            .await
            .unwrap();
        second.embedding_backend = "local".into();
        second.embedder = provider.clone();
        second
            .initialize_vectors("selected", &CancellationToken::new())
            .await
            .unwrap();
        provider.calls.store(0, std::sync::atomic::Ordering::SeqCst);
        second.memory_budget = Some(budget.clone());
        let before_second = stored_snapshot(&second);
        let work = tokio::spawn(async move {
            first
                .embed_and_upsert_vectors(
                    "selected",
                    &retained_history(0),
                    &CancellationToken::new(),
                )
                .await
        });
        let reached = tokio::time::timeout(std::time::Duration::from_secs(10), entered.notified())
            .await
            .is_ok();
        let used_during_request = budget.used();
        let rejected = second
            .embed_and_upsert_vectors("selected", &retained_history(20), &CancellationToken::new())
            .await;
        let calls_before_release = provider.calls.load(std::sync::atomic::Ordering::SeqCst);
        release.notify_one();
        let completed = work.await;
        let used_after_first = budget.used();
        let retried = second
            .embed_and_upsert_vectors("selected", &retained_history(20), &CancellationToken::new())
            .await;
        let after_first = stored_snapshot(&first_observer);
        let after_second = stored_snapshot(&second);
        let vectors = second.count_vectors("selected").await;
        let calls = provider.calls.load(std::sync::atomic::Ordering::SeqCst);
        let final_used = budget.used();
        drop(first_observer);
        drop(second);
        temporary.close().unwrap();
        assert!(reached);
        assert!(used_during_request > 0 && used_during_request <= budget.budget());
        assert!(rejected.unwrap_err().to_string().contains("memory budget"));
        assert_eq!(
            calls_before_release, 0,
            "over-budget work must not call the provider"
        );
        completed.unwrap().unwrap();
        assert_eq!(used_after_first, 0);
        retried.unwrap();
        assert_eq!(calls, 20);
        assert_eq!(final_used, 0);
        assert_eq!(vectors.unwrap(), 40);
        assert_eq!(after_first, before_first);
        assert_eq!(after_second, before_second);
    }

    struct InvalidProvider {
        kind: u8,
    }

    #[async_trait::async_trait]
    impl engram_ml::Embedder for InvalidProvider {
        fn dimension(&self) -> usize {
            384
        }
        async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            anyhow::bail!("unexpected scalar request")
        }
        async fn embed_batch(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
            let count = if self.kind == 0 {
                texts.len() - 1
            } else {
                texts.len()
            };
            let dimension = if self.kind == 1 { 383 } else { 384 };
            let mut result = vec![vec![0.9; dimension]; count];
            if self.kind == 2 {
                result[0][0] = f32::NAN;
            }
            Ok(result)
        }
    }

    #[tokio::test]
    async fn invalid_provider_batches_fail_without_changing_existing_vectors_or_leaking_budget() {
        use futures::TryStreamExt;
        let (_temporary, mut engine) = fixture().await;
        engine
            .initialize_vectors("selected", &CancellationToken::new())
            .await
            .unwrap();
        let budget = Arc::new(MemoryBudget::new(128 * 1024));
        engine.memory_budget = Some(budget.clone());
        for kind in 0..3 {
            engine.embedder = Arc::new(InvalidProvider { kind });
            let error = engine
                .embed_and_upsert_vectors(
                    "selected",
                    &retained_history(0),
                    &CancellationToken::new(),
                )
                .await
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("Embedding provider returned invalid")
            );
            assert_eq!(budget.used(), 0);
            let epoch = engine.pin_vector_epoch("selected").unwrap();
            let table = engine.open_vector_epoch_table(&epoch).await.unwrap();
            let mut rows = table
                .query()
                .only_if("namespace = 'history' AND doc_id = 'history-0'")
                .execute()
                .await
                .unwrap();
            let row = rows.try_next().await.unwrap().unwrap();
            let vectors = row
                .column_by_name("vector")
                .unwrap()
                .as_any()
                .downcast_ref::<arrow_array::FixedSizeListArray>()
                .unwrap();
            let value = vectors.value(0);
            let values = value
                .as_any()
                .downcast_ref::<arrow_array::Float32Array>()
                .unwrap();
            assert!(values.values().iter().all(|v| *v == 0.1));
            assert_eq!(table.count_rows(None).await.unwrap(), 450);
        }
    }

    async fn stored_corpus_fixture() -> (tempfile::TempDir, HybridSearchEngine) {
        let temporary = tempfile::tempdir().unwrap();
        let mut engine = HybridSearchEngine::new(
            temporary.path().join("fts"),
            temporary.path().join("vectors"),
            &engram_core::Config {
                embedding_backend: "fts_only".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        // Exercise native compressed stored records without indexing millions
        // of padding trigrams. Normal disk/schema behavior has separate cases.
        let mut schema = tantivy::schema::Schema::builder();
        for (_, field) in engine.tantivy_index.schema().fields() {
            if field.name() == "content" {
                schema.add_text_field("content", tantivy::schema::STORED);
            } else {
                schema.add_field(field.clone());
            }
        }
        engine.tantivy_index = tantivy::Index::create_in_ram(schema.build());
        engine.tantivy_index.tokenizers().register(
            crate::word_tokenizer::WORDS_TOKENIZER,
            crate::word_tokenizer::words_analyzer(),
        );
        (temporary, engine)
    }

    struct BoundedRecoveryProvider {
        seen: std::sync::atomic::AtomicUsize,
        largest_request: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl engram_ml::Embedder for BoundedRecoveryProvider {
        fn dimension(&self) -> usize {
            384
        }
        async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            anyhow::bail!("unexpected scalar request")
        }
        async fn embed_batch(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
            let bytes = texts.iter().map(|text| text.len()).sum::<usize>();
            anyhow::ensure!(
                bytes <= 32 * 1024 * 1024,
                "provider request exceeds retained-text limit"
            );
            self.seen
                .fetch_add(texts.len(), std::sync::atomic::Ordering::SeqCst);
            self.largest_request
                .fetch_max(bytes, std::sync::atomic::Ordering::SeqCst);
            Ok(vec![vec![0.1; 384]; texts.len()])
        }
    }

    #[tokio::test]
    async fn recovery_streams_large_records_with_one_global_byte_budget_across_namespaces() {
        for namespaces in [1, 3] {
            let (_temporary, mut engine) = stored_corpus_fixture().await;
            let count = if namespaces == 1 { 34 } else { 66 };
            let docs = (0..count)
                .map(|n| {
                    let namespace = ["memory", "history", "memory_bank"][n % namespaces];
                    let content = format!("large body {n} {}", " ".repeat(1024 * 1024));
                    IndexDoc {
                        generation: if namespace == "memory" { 3 } else { 0 },
                        chunk_id: n as u64,
                        path: RelPath::new(&format!("large/{n}.txt")),
                        language: "text".into(),
                        content_hash: blake3::hash(content.as_bytes()).to_hex().to_string(),
                        content,
                        namespace: namespace.into(),
                        author: None,
                        timestamp: None,
                        start_line: 1,
                        end_line: 1,
                        doc_id: format!("large-{n}"),
                    }
                })
                .collect::<Vec<_>>();
            let mut writer = engine.create_bulk_writer().unwrap();
            HybridSearchEngine::write_docs_to_writer(&engine.fields(), &mut writer, "large", &docs)
                .unwrap();
            writer.finish().unwrap();
            drop(docs);
            // Put the corpus in one native segment, so count-only paging cannot
            // accidentally appear byte-bounded because of small source segments.
            let segments = engine.tantivy_index.searchable_segment_ids().unwrap();
            if segments.len() > 1 {
                let mut writer = engine
                    .acquire_writer_blocking("recovery corpus compaction")
                    .unwrap();
                writer.merge(&segments).wait().unwrap();
                writer.wait_merging_threads().unwrap();
            }
            let budget = Arc::new(MemoryBudget::new(256 * 1024 * 1024));
            engine.memory_budget = Some(budget.clone());
            engine.embedding_backend = "local".into();
            let provider = Arc::new(BoundedRecoveryProvider {
                seen: Default::default(),
                largest_request: Default::default(),
            });
            engine.embedder = provider.clone();
            assert_eq!(
                engine
                    .initialize_vectors("large", &CancellationToken::new())
                    .await
                    .unwrap(),
                count
            );
            assert_eq!(
                provider.seen.load(std::sync::atomic::Ordering::SeqCst),
                count
            );
            assert!(
                provider
                    .largest_request
                    .load(std::sync::atomic::Ordering::SeqCst)
                    <= 32 * 1024 * 1024
            );
            assert_eq!(budget.used(), 0);
            let epoch = engine.pin_vector_epoch("large").unwrap();
            let table = engine.open_vector_epoch_table(&epoch).await.unwrap();
            assert_eq!(table.count_rows(None).await.unwrap(), count);
            for namespace in ["memory", "history", "memory_bank"]
                .into_iter()
                .take(namespaces)
            {
                assert_eq!(
                    table
                        .count_rows(Some(format!("namespace = '{namespace}'")))
                        .await
                        .unwrap(),
                    count / namespaces
                );
            }
            assert_eq!(engine.count_docs("large").unwrap(), count);
        }
    }

    #[tokio::test]
    async fn recovery_rejects_oversized_retained_record_without_publishing_a_partial_table() {
        let (_temporary, mut engine) = stored_corpus_fixture().await;
        let mut original = new_doc("original");
        original.generation = 3;
        engine
            .index_docs("selected", &[original], &CancellationToken::new())
            .await
            .unwrap();
        engine.embedding_backend = "local".into();
        engine
            .initialize_vectors("selected", &CancellationToken::new())
            .await
            .unwrap();
        let old_table = engine.pin_vector_epoch("selected").unwrap().table.clone();
        engine.embedding_backend = "fts_only".into();
        let mut large = new_doc("oversized-retained");
        large.generation = 4;
        large.content = " ".repeat(32 * 1024 * 1024 + 8192);
        engine
            .index_docs("selected", &[large], &CancellationToken::new())
            .await
            .unwrap();
        engine.embedding_backend = "local".into();
        let count_before = engine.count_docs("selected").unwrap();
        let error = engine
            .recover_vectors("selected", &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("exceeds vector recovery record limit"),
            "{error:#}"
        );
        assert_eq!(
            engine.pin_vector_epoch("selected").unwrap().table,
            old_table
        );
        assert_eq!(engine.count_vectors("selected").await.unwrap(), 1);
        assert_eq!(engine.count_docs("selected").unwrap(), count_before);
        assert_eq!(
            engine
                .count_docs_in_generation("selected", "memory", 3)
                .unwrap(),
            1
        );
        assert_eq!(
            engine
                .native_store.capture().expect("owned native fixture").connection
                .table_names()
                .execute()
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn recovery_read_admission_failure_preserves_active_data_and_releases_reservations() {
        let (_temporary, mut engine) = fixture().await;
        engine
            .initialize_vectors("selected", &CancellationToken::new())
            .await
            .unwrap();
        let before = stored_snapshot(&engine);
        let old_table = engine.pin_vector_epoch("selected").unwrap().table.clone();
        let budget = Arc::new(MemoryBudget::new(64 * 1024 * 1024));
        engine.memory_budget = Some(budget.clone());
        let error = engine
            .recover_vectors("selected", &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("memory budget"), "{error:#}");
        assert_eq!(budget.used(), 0);
        assert_eq!(
            engine.pin_vector_epoch("selected").unwrap().table,
            old_table
        );
        assert_eq!(engine.count_vectors("selected").await.unwrap(), 450);
        assert_eq!(stored_snapshot(&engine), before);
        assert_eq!(
            engine
                .native_store.capture().expect("owned native fixture").connection
                .table_names()
                .execute()
                .await
                .unwrap()
                .len(),
            1
        );
    }

    fn new_doc(name: &str) -> IndexDoc {
        IndexDoc {
            generation: 1,
            chunk_id: if name == "a" { 1 } else { 2 },
            path: RelPath::new(name),
            language: "text".into(),
            content: format!("source {name}"),
            namespace: "memory".into(),
            author: None,
            timestamp: None,
            start_line: 1,
            end_line: 1,
            doc_id: name.into(),
            content_hash: format!("hash-{name}"),
        }
    }

    #[tokio::test]
    async fn concurrent_first_writers_share_one_complete_store() {
        let temporary = tempfile::tempdir().unwrap();
        let config = engram_core::Config {
            embedding_backend: "local".into(),
            ..Default::default()
        };
        let first = HybridSearchEngine::new(
            temporary.path().join("fts"),
            temporary.path().join("vectors"),
            &config,
        )
        .await
        .unwrap();
        let second = HybridSearchEngine::new(
            temporary.path().join("fts"),
            temporary.path().join("vectors"),
            &config,
        )
        .await
        .unwrap();
        let a = [new_doc("a")];
        let b = [new_doc("b")];
        let cancel = CancellationToken::new();
        let (left, right) = tokio::join!(
            first.index_docs("new", &a, &cancel),
            second.index_docs("new", &b, &cancel)
        );
        // A contender can observe the durable pending attempt and fail closed;
        // after the owner finishes, retry must use its existing ready route.
        if left.is_err() {
            first.index_docs("new", &a, &cancel).await.unwrap();
        }
        if right.is_err() {
            second.index_docs("new", &b, &cancel).await.unwrap();
        }
        assert_eq!(first.count_docs("new").unwrap(), 2);
        assert_eq!(second.count_vectors("new").await.unwrap(), 2);
        assert_eq!(
            first
                .native_store.capture().expect("owned native fixture").connection
                .table_names()
                .execute()
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn buffered_lexical_only_writer_prevents_false_empty_vector_bootstrap() {
        let temporary = tempfile::tempdir().unwrap();
        let lexical = HybridSearchEngine::new(
            temporary.path().join("fts"),
            temporary.path().join("vectors"),
            &engram_core::Config {
                embedding_backend: "fts_only".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let vector = HybridSearchEngine::new(
            temporary.path().join("fts"),
            temporary.path().join("vectors"),
            &engram_core::Config {
                embedding_backend: "local".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let mut buffered = lexical.create_bulk_writer().unwrap();
        HybridSearchEngine::write_docs_to_writer(
            &lexical.fields(),
            &mut buffered,
            "new",
            &[new_doc("a")],
        )
        .unwrap();
        let attempt = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            vector.index_docs("new", &[new_doc("b")], &CancellationToken::new()),
        )
        .await;
        buffered.finish().unwrap();
        assert!(
            attempt.is_err(),
            "uncommitted source must keep bootstrap waiting"
        );
        assert!(
            vector
                .index_docs("new", &[new_doc("b")], &CancellationToken::new())
                .await
                .unwrap_err()
                .to_string()
                .contains("VEC1")
        );
        assert_eq!(lexical.count_docs("new").unwrap(), 1);
        assert!(
            vector
                .native_store.capture().expect("owned native fixture").connection
                .table_names()
                .execute()
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn lexical_only_search_does_not_depend_on_damaged_vector_routing() {
        let (temporary, mut engine) = fixture().await;
        engine.embedding_backend = "fts_only".into();
        let routing = temporary
            .path()
            .join("fts")
            .join("source-snapshots")
            .join("vector-routes");
        std::fs::create_dir_all(&routing).unwrap();
        std::fs::write(
            routing.join(format!("{}.json", blake3::hash(b"selected").to_hex())),
            b"interrupted-vector-metadata",
        )
        .unwrap();
        let query = HybridQuery {
            project_id: "selected".into(),
            namespace: "memory".into(),
            generation: 3,
            text: "exact".into(),
            top_k: 5,
            fts_mode: "strict".into(),
            include_path_prefixes: None,
            exclude_path_prefixes: None,
            include_path_suffixes: None,
            language_filters: None,
            author_filter: None,
            date_after: None,
            date_before: None,
            use_mmr: true,
        };
        let cancel = CancellationToken::new();
        assert_eq!(engine.search(&query, None, &cancel).await.unwrap().len(), 5);
        assert!(
            engine
                .vector_search(&query, &cancel)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            engine
                .pure_vector_search(&query, 1000, &cancel)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            engine
                .native_store.capture().expect("owned native fixture").connection
                .table_names()
                .execute()
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn vector_initialization_publishes_complete_table_and_refuses_replacement() {
        use futures::TryStreamExt;
        use lancedb::query::{ExecutableQuery, QueryBase};
        let (_tmp, engine) = fixture().await;
        let before = stored_snapshot(&engine);
        assert_eq!(
            engine
                .initialize_vectors("selected", &CancellationToken::new())
                .await
                .unwrap(),
            450
        );
        assert_eq!(
            engine
                .native_store.capture().expect("owned native fixture").connection
                .table_names()
                .execute()
                .await
                .unwrap()
                .len(),
            1
        );
        let table = engine
            .native_store.capture().expect("owned native fixture").connection
            .open_table(&engine.pin_vector_epoch("selected").unwrap().table)
            .execute()
            .await
            .unwrap();
        assert_eq!(table.count_rows(None).await.unwrap(), 450);
        for ns in ["memory", "history", "memory_bank"] {
            assert_eq!(table.count_rows(Some(format!("namespace = '{ns}' AND project_id = 'selected' AND author = 'author' AND timestamp = 123"))).await.unwrap(), 150);
        }
        let batches: Vec<_> = table
            .vector_search(vec![0.1; 384])
            .unwrap()
            .limit(5)
            .execute()
            .await
            .unwrap()
            .try_collect()
            .await
            .unwrap();
        assert_eq!(batches.iter().map(|b| b.num_rows()).sum::<usize>(), 5);
        assert!(
            engine
                .initialize_vectors("selected", &CancellationToken::new())
                .await
                .unwrap_err()
                .to_string()
                .contains("already exists")
        );
        assert_eq!(table.count_rows(None).await.unwrap(), 450);
        assert_eq!(stored_snapshot(&engine), before);
    }

    #[tokio::test]
    async fn vector_initialization_failure_and_cancellation_leave_text_and_no_vectors() {
        let (_tmp, mut engine) = fixture().await;
        let before = stored_snapshot(&engine);
        for mode in 0..3 {
            let cancel = CancellationToken::new();
            if mode == 0 {
                cancel.cancel();
            }
            engine.embedder = Arc::new(TestEmbedder {
                cancel: (mode == 2).then(|| cancel.clone()),
                fail: mode == 1,
                calls: Default::default(),
            });
            assert!(
                engine
                    .initialize_vectors("selected", &cancel)
                    .await
                    .is_err()
            );
            assert!(
                engine
                    .native_store.capture().expect("owned native fixture").connection
                    .table_names()
                    .execute()
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(stored_snapshot(&engine), before);
        }
    }

    #[tokio::test]
    async fn vector_initialization_exclusive_creation_preserves_incompatible_existing_table() {
        let (_tmp, engine) = fixture().await;
        let table = crate::vector::create_new_table(&engine.native_store.capture().expect("owned native fixture").connection, "reserved", 8)
            .await
            .unwrap();
        assert!(
            crate::vector::create_new_table(&engine.native_store.capture().expect("owned native fixture").connection, "reserved", 384)
                .await
                .is_err()
        );
        assert_eq!(
            table
                .schema()
                .await
                .unwrap()
                .field_with_name("vector")
                .unwrap()
                .data_type(),
            &arrow_schema::DataType::FixedSizeList(
                Arc::new(arrow_schema::Field::new(
                    "item",
                    arrow_schema::DataType::Float32,
                    true
                )),
                8
            )
        );
        crate::vector::create_new_table(
            &engine.native_store.capture().expect("owned native fixture").connection,
            "project_selected_initializing_interrupted",
            384,
        )
        .await
        .unwrap();
        assert!(
            engine
                .initialize_vectors("selected", &CancellationToken::new())
                .await
                .unwrap_err()
                .to_string()
                .contains("unfinished")
        );
        assert_eq!(
            engine
                .native_store.capture().expect("owned native fixture").connection
                .table_names()
                .execute()
                .await
                .unwrap()
                .len(),
            2
        );
    }
}
