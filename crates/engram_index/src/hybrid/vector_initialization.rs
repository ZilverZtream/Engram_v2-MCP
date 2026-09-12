//! Initialize an absent vector table from stored text, without re-ingestion.
use super::*;
#[cfg(feature = "vector")]
use tantivy::query::Query;

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
        anyhow::ensure!(
            self.embedding_backend != "fts_only" && !self.embedding_backend.is_empty(),
            "Configure an embedding backend before initializing vectors"
        );
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
        let canonical = format!("project_{}", project_id.replace('-', "_"));
        let root = self._lance_dir.canonicalize()?;
        let destination = root.join(format!("{canonical}.lance"));
        anyhow::ensure!(
            !destination.exists()
                && !self
                    .lance_conn
                    .table_names()
                    .execute()
                    .await?
                    .contains(&canonical),
            "Vector table already exists; initialize_vectors refuses to replace existing vectors"
        );

        // Prevent text writes while a stable reader snapshot is embedded. This
        // writer is never used to add/delete documents or publish a generation.
        let _writer = self.acquire_writer_blocking("vector initialization")?;
        anyhow::ensure!(
            !destination.exists()
                && !self
                    .lance_conn
                    .table_names()
                    .execute()
                    .await?
                    .contains(&canonical),
            "Vector table appeared while waiting for the index writer; refusing replacement"
        );
        let reader = self.tantivy_index.reader()?;
        let searcher = reader.searcher();
        let query = TermQuery::new(
            Term::from_field_text(self.fields.project_id, project_id),
            IndexRecordOption::Basic,
        );
        let expected = searcher.search(&query, &tantivy::collector::Count)?;
        anyhow::ensure!(expected > 0, "No stored documents to initialize");

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let stage = format!("{canonical}_initializing_{}_{stamp}", std::process::id());
        anyhow::ensure!(
            !self
                .lance_conn
                .table_names()
                .execute()
                .await?
                .iter()
                .any(|name| name.starts_with(&format!("{canonical}_initializing_"))),
            "An unfinished vector initialization table exists. Inspect that staging table before retrying; existing staging data will not be deleted or duplicated"
        );
        let stage_path = root.join(format!("{stage}.lance"));
        anyhow::ensure!(
            !stage_path.exists(),
            "Vector initialization staging path already exists"
        );
        let mut stage_owned = false;
        let result: anyhow::Result<usize> = async {
            // Creation is exclusive at the table level; never reuse a prior
            // failed attempt's data or touch another table on cleanup.
            let table = crate::vector::create_new_table(&self.lance_conn, &stage, self.embedder.dimension()).await?;
            stage_owned = true;
            drop(table);
            let mut processed = 0usize;
            // Walk postings once. TopDocs with a growing offset retains every
            // previous match and repeatedly scans the entire corpus.
            let weight = query.weight(tantivy::query::EnableScoring::disabled_from_searcher(&searcher))?;
            for (segment_ord, segment) in searcher.segment_readers().iter().enumerate() {
              let mut scorer = weight.scorer(segment, 1.0)?;
              while scorer.doc() != tantivy::TERMINATED {
                anyhow::ensure!(!cancel.is_cancelled(), "Vector initialization cancelled");
                let mut page = Vec::with_capacity(128);
                while page.len() < 128 && scorer.doc() != tantivy::TERMINATED {
                    if !segment.is_deleted(scorer.doc()) {
                        page.push(DocAddress::new(segment_ord as u32, scorer.doc()));
                    }
                    scorer.advance();
                }
                let mut groups = std::collections::BTreeMap::<String, Vec<IndexDoc>>::new();
                for address in &page {
                    let stored: tantivy::TantivyDocument = searcher.doc(*address)?;
                    let string = |field| -> anyhow::Result<String> {
                        stored.get_first(field).and_then(|v| v.as_str()).map(str::to_owned)
                            .ok_or_else(|| anyhow::anyhow!("Stored document is missing a required text field"))
                    };
                    let number = |field| -> anyhow::Result<u64> {
                        stored.get_first(field).and_then(|v| v.as_u64())
                            .ok_or_else(|| anyhow::anyhow!("Stored document is missing a required numeric field"))
                    };
                    let doc = IndexDoc {
                        generation: number(self.fields.generation)?,
                        chunk_id: number(self.fields.chunk_id)?,
                        path: RelPath::new(&string(self.fields.path)?),
                        language: string(self.fields.language)?,
                        content: string(self.fields.content)?,
                        namespace: string(self.fields.namespace)?,
                        author: stored.get_first(self.fields.author).and_then(|v| v.as_str()).map(str::to_owned),
                        timestamp: stored.get_first(self.fields.timestamp).and_then(|v| v.as_u64()),
                        start_line: number(self.fields.start_line)?.try_into()?,
                        end_line: number(self.fields.end_line)?.try_into()?,
                        doc_id: string(self.fields.doc_id)?,
                        content_hash: string(self.fields.content_hash)?,
                    };
                    anyhow::ensure!(string(self.fields.pk)? == build_pk(project_id, &doc.namespace, doc.generation, &doc.doc_id),
                        "Stored primary key does not match its identity fields");
                    groups.entry(doc.namespace.clone()).or_default().push(doc);
                }
                for docs in groups.values() {
                    self.embed_and_upsert_vectors_to_table(project_id, docs, cancel, Some(&stage)).await?;
                    anyhow::ensure!(!cancel.is_cancelled(), "Vector initialization cancelled");
                    processed += docs.len();
                }
              }
            }
            let table = self.lance_conn.open_table(&stage).execute().await?;
            let rows = table.count_rows(None).await?;
            drop(table);
            anyhow::ensure!(processed == expected && rows as usize == expected,
                "Vector initialization incomplete: expected {expected}, processed {processed}, stored {rows}");
            anyhow::ensure!(!cancel.is_cancelled(), "Vector initialization cancelled before publication");
            anyhow::ensure!(!destination.exists(), "Vector destination appeared during initialization; refusing replacement");
            anyhow::ensure!(stage_path.canonicalize()?.parent() == Some(root.as_path()) && destination.parent() == Some(root.as_path()),
                "Vector publication paths must remain inside the configured index directory");
            // Local Lance manifests use relative data paths. No index exists
            // at the destination; rename publishes the completed table only.
            std::fs::rename(&stage_path, &destination)?;
            stage_owned = false;
            Ok(processed)
        }.await;
        if stage_owned {
            // A failed/cancelled provider never publishes partial vectors.
            // Cleanup is limited to this call's newly-created staging table.
            if let Err(error) = self.lance_conn.drop_table(&stage, &[]).await {
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
        anyhow::bail!("Vector initialization requires a build with the vector feature")
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
            engine.lance_conn.table_names().execute().await.unwrap(),
            vec!["project_selected"]
        );
        let table = engine
            .lance_conn
            .open_table("project_selected")
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
                    .lance_conn
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
        let table = crate::vector::create_new_table(&engine.lance_conn, "reserved", 8)
            .await
            .unwrap();
        assert!(
            crate::vector::create_new_table(&engine.lance_conn, "reserved", 384)
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
            &engine.lance_conn,
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
                .lance_conn
                .table_names()
                .execute()
                .await
                .unwrap()
                .len(),
            2
        );
    }
}
