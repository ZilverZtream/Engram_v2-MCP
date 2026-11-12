use anyhow::{Result, ensure};
use engram_core::{Config, RelPath};
use engram_index::{HybridQuery, HybridSearchEngine, SourceExtractionExpectation, SourceExtractionIdentity};
use tokio_util::sync::CancellationToken;

#[test]
fn vendor_assets_publish_and_remain_searchable_without_semantic_facts() -> Result<()> {
    let owned = tempfile::tempdir()?;
    let result = (|| -> Result<()> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2).enable_all().build()?;
        runtime.block_on(async {
            let root = owned.path().join("repo");
            let sources = [
                ("Site/bower_components/font-awesome/metadata/icon-families.json", r#"{"icon":"vendorjsonneedle"}"#),
                ("Site/bower_components/font-awesome/js/icons.js", "function vendorjsneedle() { appHandler(); }"),
                ("Site/app/settings.json", r#"{"setting":"applicationjsonneedle"}"#),
                ("Site/app/main.js", "function applicationjsneedle() { appHandler(); }"),
            ];
            for (path, source) in sources {
                let file = root.join(path);
                std::fs::create_dir_all(file.parent().ok_or_else(|| anyhow::anyhow!("Missing fixture parent"))?)?;
                std::fs::write(file, source)?;
            }
            let index = HybridSearchEngine::new(owned.path().join("tantivy"), owned.path().join("lance"),
                &Config { embedding_backend: "fts_only".into(), ..Default::default() }).await?;
            index.prepare_full_source_snapshot("project", 0, 1,
                &sources.iter().map(|(path, _)| RelPath::new(*path)).collect::<Vec<_>>(),
                &CancellationToken::new()).await?;
            let stats = index.index_files("project", "memory", 1, &root,
                sources.iter().map(|(path, _)| root.join(path)).collect(), 2000,
                &CancellationToken::new(), |_, _| {}).await?;
            let expected = SourceExtractionExpectation::for_paths(2000, sources.iter().map(|(path, _)| *path))?;
            ensure!(stats.files == 4 && stats.all_files.len() == 4);
            ensure!(stats.symbols.iter().all(|(path, _)| !path.as_str().contains("bower_components"))
                && stats.edges.iter().all(|(path, _)| !path.as_str().contains("bower_components")),
                "Vendor assets must not introduce semantic symbols or app dependency edges");
            expected.verify_produced(&stats)?;
            index.seal_source_snapshot("project", 1)?;
            for (path, needle) in [
                (sources[0].0, "vendorjsonneedle"), (sources[1].0, "vendorjsneedle"),
                (sources[2].0, "applicationjsonneedle"), (sources[3].0, "applicationjsneedle"),
            ] {
                let query = HybridQuery { project_id: "project".into(), namespace: "memory".into(),
                    generation: 1, text: needle.into(), top_k: 10, fts_mode: "loose".into(),
                    include_path_prefixes: None, exclude_path_prefixes: None, include_path_suffixes: None,
                    language_filters: None, author_filter: None, date_after: None, date_before: None, use_mmr: false };
                ensure!(index.lexical_search(&query)?.iter().any(|hit| hit.path.as_str() == path),
                    "Published source must remain searchable: {path}");
            }
            let mut missing_facts = stats;
            missing_facts.symbols.clear();
            ensure!(expected.verify_produced(&missing_facts).is_err(),
                "First-party config facts remain mandatory");
            missing_facts.all_files = vec![RelPath::new(sources[1].0)];
            missing_facts.script_call_contracts.clear();
            ensure!(expected.verify_produced(&missing_facts).is_err(),
                "Vendor suppression does not bypass script contract receipts");
            Ok(())
        })
    })();
    let cleanup = owned.close();
    match (result, cleanup) {
        (Err(error), Err(cleanup)) => Err(error.context(format!("Owned vendor fixture cleanup failed: {cleanup}"))),
        (Err(error), _) => Err(error),
        (Ok(()), cleanup) => cleanup.map_err(Into::into),
    }
}

#[test]
fn vendor_reuse_preserves_source_identity_and_suppression_contract_checks() -> Result<()> {
    let json = "Site/bower_components/font-awesome/metadata/icon-families.json";
    let js = "Site/bower_components/font-awesome/js/icons.js";
    let expected = SourceExtractionExpectation::for_paths(2000, [json, js])?;
    let source = serde_json::json!({"source_index_version": engram_index::SOURCE_INDEX_VERSION,
        "source_extraction_identity": SourceExtractionIdentity::for_chunk_target(2000)});
    ensure!(expected.metadata_is_compatible(&source, json));
    ensure!(!expected.metadata_is_compatible(&source, js));
    ensure!(!expected.metadata_is_compatible(&source, "Site/app/settings.json"));
    let mut script = source.clone();
    script["script_call_contract"] = serde_json::json!(engram_index::source_contract::SCRIPT_CALL_CONTRACT);
    ensure!(expected.metadata_is_compatible(&script, js));
    for path in [json, js] {
        let mut stale = script.clone();
        stale["source_index_version"] = serde_json::json!(0);
        ensure!(!expected.metadata_is_compatible(&stale, path));
        let mut changed_chunking = script.clone();
        changed_chunking["source_extraction_identity"]["target_chars_per_chunk"] = serde_json::json!(1000);
        ensure!(!expected.metadata_is_compatible(&changed_chunking, path));
        ensure!(!expected.metadata_is_compatible(&serde_json::json!({}), path));
    }
    Ok(())
}
