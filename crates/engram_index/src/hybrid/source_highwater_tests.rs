//! Recovery generations must preserve published readers and abandoned evidence.
use super::*;

fn document(generation: u64, id: &str) -> IndexDoc {
    IndexDoc {
        generation, chunk_id: generation, path: RelPath::new("source.rs"), language: "rust".into(),
        content: format!("source sentinel {id}"), namespace: "memory".into(), author: None,
        timestamp: None, start_line: 1, end_line: 1, doc_id: id.into(), content_hash: id.into(),
    }
}

fn scope(root: &Path) -> PathBuf {
    root.join("tantivy/source-snapshots")
        .join(blake3::hash(b"project\0memory").to_hex().as_str())
}

async fn engine(root: &Path) -> anyhow::Result<HybridSearchEngine> {
    let config = engram_core::Config { embedding_backend: "fts_only".into(), ..Default::default() };
    HybridSearchEngine::new(root.join("tantivy"), root.join("vectors"), &config).await
}

#[tokio::test]
async fn source_recovery_highwater_preserves_held_reader_and_abandoned_empty_generation() -> anyhow::Result<()> {
    let owned = tempfile::tempdir()?;
    let result = async {
        let index = engine(owned.path()).await?;
        let cancel = CancellationToken::new();
        index.index_docs("project", &[document(1, "published")], &cancel).await?;
        index.prepare_full_source_snapshot("project", 1, 2, &[RelPath::new("source.rs")], &cancel).await?;
        index.index_docs("project", &[document(2, "reader")], &cancel).await?;
        index.seal_source_snapshot("project", 2)?;
        let held = index.pin_source_generation("project", 2)?;
        index.index_docs("project", &[document(9, "abandoned-payload")], &cancel).await?;
        index.prepare_full_source_snapshot("project", 2, 12, &[], &cancel).await?;
        let abandoned = std::fs::read(scope(owned.path()).join("12.json"))?;
        anyhow::ensure!(index.source_generation_high_water("project", &cancel).await? == 12);
        index.prepare_recovered_source_snapshot("project", 12, 13, &[RelPath::new("source.rs")], &cancel).await?;
        anyhow::ensure!(std::fs::read(scope(owned.path()).join("12.json"))? == abandoned,
            "Recovery preparation must preserve the empty abandoned attempt");
        index.index_docs("project", &[document(13, "recovered")], &cancel).await?;
        index.seal_source_snapshot("project", 13)?;
        index.purge_old_generations("project", 13).await?;
        anyhow::ensure!(index.count_docs_in_generation("project", "memory", 2)? == 1,
            "Recovery and collection must preserve the old reader's selected source");
        anyhow::ensure!(held._snapshot.contains("source.rs", 2));
        anyhow::ensure!(index.count_docs_in_generation("project", "memory", 13)? == 1);
        Ok::<_, anyhow::Error>(())
    }.await;
    owned.close()?;
    result
}

#[tokio::test]
async fn source_recovery_highwater_supersedes_corrupt_manifest_without_overwriting_it() -> anyhow::Result<()> {
    let owned = tempfile::tempdir()?;
    let result = async {
        let index = engine(owned.path()).await?;
        let cancel = CancellationToken::new();
        index.prepare_full_source_snapshot("project", 1, 8, &[], &cancel).await?;
        let path = scope(owned.path()).join("8.json");
        std::fs::write(&path, b"{broken manifest")?;
        let unsupported = scope(owned.path()).join("7.json");
        let unsupported_bytes = br#"{"format":999,"project":"project","namespace":"memory","generation":7,"parent":null,"pending":false,"files":{},"changes":{}}"#;
        std::fs::write(&unsupported, unsupported_bytes)?;
        anyhow::ensure!(index.pin_source_generation("project", 8).is_err(),
            "Direct reads of malformed active metadata must remain strict");
        anyhow::ensure!(index.source_generation_high_water("project", &cancel).await? == 8);
        anyhow::ensure!(index.prepare_recovered_source_snapshot("project", 7, 8, &[], &cancel).await.is_err());
        anyhow::ensure!(std::fs::read(&path)? == b"{broken manifest");
        index.prepare_recovered_source_snapshot("project", 8, 9, &[], &cancel).await?;
        index.seal_source_snapshot("project", 9)?;
        anyhow::ensure!(index.pin_source_generation("project", 9).is_ok());
        anyhow::ensure!(std::fs::read(&path)? == b"{broken manifest");
        let alias = scope(owned.path()).join("09.json");
        std::fs::write(&alias, b"unowned noncanonical file")?;
        index.purge_old_generations("project", 9).await?;
        anyhow::ensure!(index.count_docs_in_generation("project", "memory", 9)? == 0);
        index.prepare_source_snapshot("project", 9, 10, &[RelPath::new("source.rs")], &[], &cancel).await?;
        index.index_docs("project", &[document(10, "next-edit")], &cancel).await?;
        index.seal_source_snapshot("project", 10)?;
        index.purge_old_generations("project", 10).await?;
        anyhow::ensure!(index.count_docs_in_generation("project", "memory", 10)? == 1,
            "The edit following damaged-manifest recovery must remain readable after collection");
        anyhow::ensure!(std::fs::read(&path)? == b"{broken manifest",
            "Collection must preserve unvalidated abandoned evidence");
        anyhow::ensure!(std::fs::read(&unsupported)? == unsupported_bytes,
            "Collection must preserve unsupported old manifest formats");
        anyhow::ensure!(std::fs::read(&alias)? == b"unowned noncanonical file",
            "Validating 9.json does not authorize deleting its unvalidated 09.json alias");
        Ok::<_, anyhow::Error>(())
    }.await;
    owned.close()?;
    result
}

#[tokio::test]
async fn source_recovery_highwater_refuses_corrupt_boundaries_exhaustion_and_cancellation() -> anyhow::Result<()> {
    let owned = tempfile::tempdir()?;
    let result = async {
        let index = engine(owned.path()).await?;
        let cancel = CancellationToken::new();
        index.prepare_full_source_snapshot("project", 1, 3, &[], &cancel).await?;
        let boundary = scope(owned.path()).join("legacy-through.json");
        std::fs::write(&boundary, b"not a generation")?;
        anyhow::ensure!(index.source_generation_high_water("project", &cancel).await.is_err());
        anyhow::ensure!(std::fs::read(&boundary)? == b"not a generation");
        std::fs::write(&boundary, b"1")?;
        let retired = scope(owned.path()).join("retired-before.json");
        std::fs::write(&retired, b"55")?;
        anyhow::ensure!(index.source_generation_high_water("project", &cancel).await? == 55,
            "Durable retirement bounds must survive missing physical generations");
        std::fs::write(&retired, b"broken floor")?;
        anyhow::ensure!(index.source_generation_high_water("project", &cancel).await.is_err());
        std::fs::write(&retired, b"3")?;
        let mut budget = source_highwater::WorkBudget::with_work_limit(&cancel, 0);
        anyhow::ensure!(index.source_high_water_inner("project", &mut budget).await.is_err());
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        anyhow::ensure!(index.source_generation_high_water("project", &cancelled).await.is_err());
        anyhow::ensure!(index.prepare_recovered_source_snapshot("project", 3, 4, &[], &cancelled).await.is_err());
        anyhow::ensure!(!scope(owned.path()).join("4.json").exists());
        std::fs::write(scope(owned.path()).join("unknown.json"), b"{}")?;
        anyhow::ensure!(index.source_generation_high_water("project", &cancel).await.is_err());
        Ok::<_, anyhow::Error>(())
    }.await;
    owned.close()?;
    result
}

#[tokio::test]
async fn source_recovery_highwater_scopes_live_lexical_rows_and_retains_leased_metadata() -> anyhow::Result<()> {
    let owned = tempfile::tempdir()?;
    let result = async {
        let index = engine(owned.path()).await?;
        let cancel = CancellationToken::new();
        index.index_docs("project", &[document(7, "live"), document(99, "deleted")], &cancel).await?;
        index.index_docs("other-project", &[document(500, "other")], &cancel).await?;
        let mut writer = index.acquire_writer("highwater deleted-row fixture", &cancel).await?;
        writer.delete_term(Term::from_field_u64(index.fields.generation, 99));
        writer.commit()?;
        writer.wait_merging_threads()?;
        anyhow::ensure!(index.source_generation_high_water("project", &cancel).await? == 7);
        // A lease can outlive the catalog file that originally described it.
        index.prepare_full_source_snapshot("project", 7, 20, &[], &cancel).await?;
        index.seal_source_snapshot("project", 20)?;
        let _held = index.pin_source_generation("project", 20)?;
        std::fs::remove_file(scope(owned.path()).join("20.json"))?;
        anyhow::ensure!(index.source_generation_high_water("project", &cancel).await? == 20);
        anyhow::ensure!(index.prepare_recovered_source_snapshot("project", 19, 20, &[], &cancel).await.is_err(),
            "A missing manifest does not authorize replacing a cached reader view");
        Ok::<_, anyhow::Error>(())
    }.await;
    owned.close()?;
    result
}

#[cfg(feature = "vector")]
#[tokio::test]
async fn source_recovery_highwater_includes_vector_only_abandoned_rows() -> anyhow::Result<()> {
    let owned = tempfile::tempdir()?;
    let result = async {
        let index = HybridSearchEngine::new(owned.path().join("tantivy"), owned.path().join("vectors"),
            &engram_core::Config::default()).await?;
        let cancel = CancellationToken::new();
        index.index_docs("project", &[document(7, "live"), document(31, "vector-only")], &cancel).await?;
        let mut writer = index.acquire_writer("highwater vector-only fixture", &cancel).await?;
        writer.delete_term(Term::from_field_u64(index.fields.generation, 31));
        writer.commit()?;
        writer.wait_merging_threads()?;
        anyhow::ensure!(index.source_generation_high_water("project", &cancel).await? == 31,
            "A vector-only abandoned generation must prevent staging collisions");
        Ok::<_, anyhow::Error>(())
    }.await;
    owned.close()?;
    result
}

#[tokio::test]
async fn source_recovery_highwater_refuses_source_rows_without_generation() -> anyhow::Result<()> {
    let owned = tempfile::tempdir()?;
    let result = async {
        let index = engine(owned.path()).await?;
        let cancel = CancellationToken::new();
        let mut writer = index.acquire_writer("highwater missing-generation fixture", &cancel).await?;
        writer.add_document(doc!(index.fields.project_id => "project", index.fields.namespace => "memory"))?;
        writer.commit()?;
        writer.wait_merging_threads()?;
        anyhow::ensure!(index.source_generation_high_water("project", &cancel).await.is_err(),
            "A source row without generation cannot establish a safe staging floor");
        Ok::<_, anyhow::Error>(())
    }.await;
    owned.close()?;
    result
}

#[cfg(not(feature = "vector"))]
#[tokio::test]
async fn source_recovery_highwater_no_vector_refuses_existing_native_data() -> anyhow::Result<()> {
    let owned = tempfile::tempdir()?;
    let result = async {
        let index = engine(owned.path()).await?;
        std::fs::create_dir_all(owned.path().join("vectors/project_project.lance"))?;
        anyhow::ensure!(index.source_generation_high_water("project", &CancellationToken::new()).await.is_err());
        Ok::<_, anyhow::Error>(())
    }.await;
    owned.close()?;
    result
}

#[tokio::test]
async fn source_snapshot_future_delta_cannot_expose_unpublished_source_and_full_recovery_keeps_old_reader()
-> anyhow::Result<()> {
    let owned = tempfile::tempdir()?;
    let result = async {
        let index = engine(owned.path()).await?;
        let cancel = CancellationToken::new();
        index.index_docs("project", &[document(1, "published-old")], &cancel).await?;
        let held = index.pin_source_generation("project", 1)?;
        let query = |generation| HybridQuery {
            project_id: "project".into(), namespace: "memory".into(), generation,
            text: "sentinel".into(), top_k: 10, fts_mode: "loose".into(),
            include_path_prefixes: None, exclude_path_prefixes: None,
            include_path_suffixes: None, language_filters: None, author_filter: None,
            date_after: None, date_before: None, use_mmr: false,
        };
        let old = index.lexical_search(&query(1))?.pop()
            .ok_or_else(|| anyhow::anyhow!("Published source missing"))?;
        index.prepare_source_snapshot("project", 1, 2, &[RelPath::new("source.rs")], &[], &cancel).await?;
        index.index_docs("project", &[document(3, "unpublished-future")], &cancel).await?;
        let path = scope(owned.path()).join("2.json");
        let mut record: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        record["pending"] = serde_json::json!(false);
        record["changes"]["source.rs"] = serde_json::json!(3);
        let corrupted = serde_json::to_vec(&record)?;
        std::fs::write(&path, &corrupted)?;

        let error = index.lexical_search(&query(2)).err()
            .ok_or_else(|| anyhow::anyhow!("Damaged delta exposed unpublished future source"))?;
        anyhow::ensure!(error.to_string().contains("delta references a future revision"), "{error:#}");
        anyhow::ensure!(std::fs::read(&path)? == corrupted, "Read refusal must preserve damaged evidence");
        anyhow::ensure!(index.lexical_search(&query(1))?.iter().any(|hit| hit.pk == old.pk));

        let floor = index.source_generation_high_water("project", &cancel).await?;
        anyhow::ensure!(floor == 3, "Recovery must still observe unpublished physical rows");
        index.prepare_recovered_source_snapshot("project", floor, 4, &[RelPath::new("source.rs")], &cancel).await?;
        index.index_docs("project", &[document(4, "recovered-current")], &cancel).await?;
        index.seal_source_snapshot("project", 4)?;
        index.purge_old_generations("project", 4).await?;
        let current = index.lexical_search(&query(4))?;
        anyhow::ensure!(current.len() == 1
            && current[0].doc_id == "recovered-current"
            && current[0].pk == "project:memory:4:recovered-current",
            "Recovery must select the new complete revision, not an abandoned sentinel");
        anyhow::ensure!(index.get_doc_by_pk(&current[0].pk)?
            .ok_or_else(|| anyhow::anyhow!("Recovered body missing"))?.2
                == "source sentinel recovered-current");
        anyhow::ensure!(index.lexical_search(&query(1))?.iter().any(|hit| hit.pk == old.pk),
            "Recovery must preserve the old reader's valid source");
        anyhow::ensure!(std::fs::read(&path)? == corrupted);
        drop(held);
        index.purge_old_generations("project", 4).await?;
        anyhow::ensure!(index.get_doc_by_pk(&old.pk)?.is_none());
        Ok::<_, anyhow::Error>(())
    }.await;
    owned.close()?;
    result
}
