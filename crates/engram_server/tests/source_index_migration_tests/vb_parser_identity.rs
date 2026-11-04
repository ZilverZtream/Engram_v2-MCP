//! Explicit, serialized real-companion proof. The runner selects the built
//! companion via ENGRAM_VB_SIDECAR_PATH; this fixture never changes process env,
//! runtime assets or another client's configuration.
use anyhow::{Result, ensure};
use engram_core::{Config, MemorySection};
use engram_index::{HybridQuery, vb_extractor::{VbParserProvenance, shutdown_idle_parser}};
use engram_server::{AppState, Engram};
use serde_json::json;
use std::{collections::BTreeSet, path::Path};

struct ParserOwner;
impl Drop for ParserOwner {
    fn drop(&mut self) {
        if let Err(error) = shutdown_idle_parser() {
            eprintln!("Owned VB fixture parser cleanup failed: {error:#}");
        }
    }
}

async fn settle(app: &Engram) -> Result<()> {
    loop {
        let jobs = app.state.active_jobs.write().await.drain().map(|(_, task)| task).collect::<Vec<_>>();
        if jobs.is_empty() { return Ok(()); }
        for job in jobs { job.await?; }
    }
}

fn config(root: &Path) -> Config {
    Config { data_dir: root.join("data"), allowed_roots: vec![root.join("repo")],
        embedding_backend: "fts_only".into(), llm_backend: "none".into(), ..Default::default() }
}

async fn update(app: &Engram, pid: &str) -> Result<String> {
    let result = app.handle_update_project(serde_json::from_value(json!({
        "project_id":pid,"wait":true,"max_commits":1,"index_antipatterns":false
    }))?).await?;
    ensure!(result.is_error != Some(true));
    Ok(result.content[0].as_text().ok_or_else(|| anyhow::anyhow!("Missing update text"))?.text.clone())
}

async fn source_rows(app: &Engram, pid: &str) -> Result<BTreeSet<String>> {
    let ps = app.state.get_project_cached(pid)
        .ok_or_else(|| anyhow::anyhow!("Public indexing/update must load the project runtime"))?;
    let query = HybridQuery { project_id: pid.into(), namespace: "memory".into(),
        generation: app.state.registry.active_source_generation(pid)?, text: "parserstablemarker".into(),
        top_k: 100, fts_mode: "strict".into(), include_path_prefixes: Some(vec!["Source.vb".into()]),
        exclude_path_prefixes: None, include_path_suffixes: None, language_filters: None,
        author_filter: None, date_after: None, date_before: None, use_mmr: false };
    let hits = ps.search.lexical_search(&query)?;
    ensure!(!hits.is_empty(), "The stable VB source must remain searchable");
    Ok(hits.into_iter().map(|hit| hit.pk).collect())
}

fn warm_phase(root: &Path) -> Result<String> {
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let result = runtime.block_on(async {
        let (state, _) = AppState::new(config(root))?;
        let app = Engram::new(state);
        app.handle_index_project(serde_json::from_value(json!({
            "directory":root.join("repo"),"project_name":"Owned VB parser migration","project_type":"general","wait":true
        }))?).await?;
        let pid = app.state.registry.list_projects()?[0].project_id.clone();
        let actual = engram_index::vb_extractor::current_parser_provenance()?;
        ensure!(matches!(actual, VbParserProvenance::Roslyn { .. }), "This explicit proof requires the identity-capable real companion");
        for path in ["Source.vb", "Empty.vb"] {
            let mut file = app.state.graph.get_node(&pid, &format!("file:{path}"))?
                .ok_or_else(|| anyhow::anyhow!("Missing file {path}"))?;
            let metadata = file.metadata.as_mut().ok_or_else(|| anyhow::anyhow!("Missing fingerprint"))?;
            ensure!(metadata["vb_parser_provenance"] == serde_json::to_value(&actual)?,
                "Actual per-file parser receipt is required, including zero-symbol source");
            // Persist a previous known fallback receipt, as retained stores
            // from a companion-free deployment do. Source bytes stay intact.
            let mut prior_context = actual.context().ok_or_else(|| anyhow::anyhow!("Parser context missing"))?.clone();
            prior_context.roslyn = None;
            metadata["vb_parser_provenance"] = serde_json::to_value(VbParserProvenance::RustFallback {
                context: prior_context, reason: engram_index::vb_extractor::VbFallbackReason::Unavailable,
            })?;
            app.state.graph.upsert_nodes(&pid, &[file])?;
        }
        let section: MemorySection = serde_json::from_value(json!({
            "section_id":"parser-decision","title":"Retain authored decision","content":"Knowledge survives parser migration.","updated_at_ms":1
        }))?;
        app.state.registry.put_memory_section(&pid, &section)?;
        settle(&app).await?;
        Ok(pid)
    });
    // Indexing may launch native blocking work beyond its async task owner.
    drop(runtime);
    result
}

fn cold_phase(root: &Path, pid: &str) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let result = runtime.block_on(async {
        let (state, _) = AppState::new(config(root))?;
        let app = Engram::new(state);
        let before = app.state.registry.active_source_generation(pid)?;
        let (changed, deleted) = engram_server::services::project_service::get_incremental_changes(
            &app.state, pid, &root.join("repo"), &["vb", "rs"],
        ).await?;
        ensure!(deleted.is_empty());
        ensure!(changed.into_iter().collect::<BTreeSet<_>>() == BTreeSet::from([
            root.join("repo/Source.vb"), root.join("repo/Empty.vb"),
        ]), "A newly loaded Roslyn parser must re-extract unchanged fallback files exactly once");
        update(&app, pid).await?;
        let migrated = app.state.registry.active_source_generation(pid)?;
        ensure!(migrated > before);
        let expected = engram_index::vb_extractor::current_parser_provenance()?;
        for path in ["Source.vb", "Empty.vb"] {
            let file = app.state.graph.get_node(pid, &format!("file:{path}"))?.unwrap();
            ensure!(file.metadata.unwrap()["vb_parser_provenance"] == serde_json::to_value(&expected)?);
        }
        let baseline = source_rows(&app, pid).await?;
        ensure!(update(&app, pid).await?.contains("no_changes_detected"));
        ensure!(app.state.registry.active_source_generation(pid)? == migrated);
        std::fs::write(root.join("repo/Companion.rs"), "pub fn changed_companion() {}\n")?;
        update(&app, pid).await?;
        ensure!(app.state.registry.active_source_generation(pid)? > migrated);
        ensure!(source_rows(&app, pid).await? == baseline,
            "The next unrelated edit must inherit the migrated VB physical identities");
        let (changed, deleted) = engram_server::services::project_service::get_incremental_changes(
            &app.state, pid, &root.join("repo"), &["vb", "rs"],
        ).await?;
        ensure!(changed.is_empty() && deleted.is_empty(), "Parser migration must not repeat");
        ensure!(app.state.registry.get_memory_section(pid, "parser-decision")?.unwrap().content == "Knowledge survives parser migration.");
        settle(&app).await?;
        Ok(())
    });
    drop(runtime);
    result
}

#[test]
#[ignore = "requires identity-capable ENGRAM_VB_SIDECAR_PATH and serialized ownership of the idle global parser"]
fn cold_vb_parser_receipt_migration_runs_once_then_inherits_on_the_next_edit() -> Result<()> {
    // The explicit runner must quiesce other parser clients before selecting
    // this case. There is no runtime-path override or process-env mutation here.
    shutdown_idle_parser()?;
    let owned = tempfile::tempdir()?;
    let owner = ParserOwner;
    let source = "Public Class Stable\nPublic Sub parserstablemarker()\nEnd Sub\nEnd Class\n";
    let result = (|| -> Result<()> {
        let repo_root = owned.path().join("repo");
        std::fs::create_dir(&repo_root)?;
        std::fs::write(repo_root.join("Source.vb"), source)?;
        std::fs::write(repo_root.join("Empty.vb"), " \n")?;
        std::fs::write(repo_root.join("Companion.rs"), "pub fn companion() {}\n")?;
        {
            let repo = git2::Repository::init(&repo_root)?;
            let mut index = repo.index()?;
            index.add_all(["."], git2::IndexAddOption::DEFAULT, None)?;
            let tree = repo.find_tree(index.write_tree()?)?;
            let signature = git2::Signature::now("Fixture", "fixture@example.invalid")?;
            repo.commit(Some("HEAD"), &signature, &signature, "owned parser baseline", &tree, &[])?;
        }
        let pid = warm_phase(owned.path())?;
        shutdown_idle_parser()?;
        cold_phase(owned.path(), &pid)?;
        ensure!(std::fs::read_to_string(repo_root.join("Source.vb"))? == source);
        ensure!(std::fs::read_to_string(repo_root.join("Empty.vb"))? == " \n");
        Ok(())
    })();
    let stopped = shutdown_idle_parser();
    drop(owner);
    let cleanup = owned.close();
    result?;
    stopped?;
    cleanup?;
    Ok(())
}

#[test]
#[ignore = "requires identity-capable ENGRAM_VB_SIDECAR_PATH, cap <=4MiB, and serialized idle-parser ownership"]
fn public_unchanged_updates_reuse_actual_source_limit_fallback_without_generation_or_child_churn() -> Result<()> {
    shutdown_idle_parser()?;
    let owned = tempfile::tempdir()?;
    let owner = ParserOwner;
    let result = (|| -> Result<()> {
        let context = engram_index::vb_extractor::current_parser_context()?;
        ensure!(context.roslyn.is_some(), "This case requires the actual identity-capable Roslyn companion");
        ensure!(context.max_source_bytes <= 4 * 1024 * 1024, "Keep the actual over-cap source fixture bounded");
        let initial_pid = engram_index::vb_extractor::cached_parser_process_id()?
            .ok_or_else(|| anyhow::anyhow!("Verified parser owner missing"))?;
        let repo_root = owned.path().join("repo");
        std::fs::create_dir(&repo_root)?;
        let mut source = String::from("Public Class Stable ' fallbackinventorymarker\nPublic Sub Run() ' fallbackinventorymarker\nEnd Sub ' fallbackinventorymarker\n");
        while source.len() <= context.max_source_bytes {
            source.push_str("' fallbackinventorymarker bounded owned comment for stable degraded source reuse\n");
        }
        source.push_str("End Class ' fallbackinventorymarker\n");
        ensure!(source.len() as u64 <= engram_index::ingest::MAX_FILE_SIZE);
        std::fs::write(repo_root.join("Large.vb"), &source)?;
        std::fs::write(repo_root.join("Companion.rs"), "pub fn companion() {} // fallbackinventorymarker\n")?;
        {
            let repo = git2::Repository::init(&repo_root)?;
            let mut index = repo.index()?;
            // Only the small companion enters history. The unchanged large VB
            // source is ordinary eligible untracked working-tree input.
            index.add_path(Path::new("Companion.rs"))?;
            index.write()?;
            let tree = repo.find_tree(index.write_tree()?)?;
            let signature = git2::Signature::now("Fixture", "fixture@example.invalid")?;
            repo.commit(Some("HEAD"), &signature, &signature, "owned small history baseline", &tree, &[])?;
        }
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
        let tested = runtime.block_on(async {
            let (state, _) = AppState::new(config(owned.path()))?;
            let app = Engram::new(state);
            let indexed = app.handle_index_project(serde_json::from_value(json!({
                "directory":repo_root,"project_name":"Actual stable source-cap fallback","project_type":"general","wait":true
            }))?).await?;
            ensure!(indexed.is_error != Some(true));
            let pid = app.state.registry.list_projects()?[0].project_id.clone();
            let file = app.state.graph.get_node(&pid, "file:Large.vb")?
                .ok_or_else(|| anyhow::anyhow!("Actual produced VB metadata missing"))?;
            let actual: VbParserProvenance = serde_json::from_value(file.metadata.as_ref().unwrap()["vb_parser_provenance"].clone())?;
            ensure!(matches!(&actual, VbParserProvenance::RustFallback {
                reason: engram_index::vb_extractor::VbFallbackReason::SourceLimit, ..
            }) && actual.context() == Some(&context), "Public indexing must record actual degraded backend under available context A");
            ensure!(engram_index::vb_extractor::cached_parser_process_id()? == Some(initial_pid),
                "Source-cap fallback must keep the already-verified child healthy");
            let head = app.state.registry.capture_source_head(&pid)?;
            let revision = app.state.registry.get_meta(&pid, "source_head_revision_v1")?;
            let ps = app.state.get_project_cached(&pid)
                .ok_or_else(|| anyhow::anyhow!("Public indexing must load the project runtime"))?;
            let inventory = || -> Result<BTreeSet<String>> {
                let total = ps.search.count_docs_in_generation(&pid, "memory", head.generation())?;
                let query = HybridQuery { project_id:pid.clone(), namespace:"memory".into(), generation:head.generation(),
                    text:"fallbackinventorymarker".into(), top_k:total.saturating_add(1), fts_mode:"strict".into(),
                    include_path_prefixes:None, exclude_path_prefixes:None,
                    include_path_suffixes:None, language_filters:None, author_filter:None,
                    date_after:None, date_before:None, use_mmr:false };
                let hits = ps.search.lexical_search(&query)?;
                ensure!(!hits.is_empty() && hits.len() == total,
                    "The inventory token must enumerate every selected source chunk");
                Ok(hits.into_iter().map(|hit| hit.pk).collect())
            };
            // Every fixture source line contains the inventory token, so this
            // untruncated query enumerates all selected source physical chunks.
            let physical = inventory()?;
            for _ in 0..2 {
                let report = update(&app, &pid).await?;
                ensure!(report.contains("no_changes_detected") && report.contains("files=0 chunks=0 bytes=0"),
                    "Unchanged known degradation must remain a source no-op: {report}");
                ensure!(app.state.registry.active_source_generation(&pid)? == head.generation()
                    && app.state.registry.source_head_is_current(&head)?
                    && app.state.registry.get_meta(&pid, "source_head_revision_v1")? == revision,
                    "Unchanged fallback must not advance the published generation/revision");
                ensure!(inventory()? == physical, "Unchanged fallback rewrote selected physical source identities");
                ensure!(engram_index::vb_extractor::cached_parser_process_id()? == Some(initial_pid)
                    && engram_index::vb_extractor::current_parser_context()? == context,
                    "Unchanged source must keep the captured verified parser owner/context healthy");
            }
            // Real public admission must refuse an aggregate that exceeds the
            // compilation cap even when each peer fits its individual 10MiB
            // ingestion limit. No large peer reaches ParseText on this path.
            let over_limit = vec![b' '; 8 * 1024 * 1024];
            for number in 0..4 { std::fs::write(repo_root.join(format!("OverAggregate{number}.vb")), &over_limit)?; }
            drop(over_limit);
            let refused = app.handle_update_project(serde_json::from_value(json!({
                "project_id":pid,"wait":true,"max_commits":1,"index_antipatterns":false
            }))?).await;
            let refusal = match refused {
                Err(error) => error.to_string(),
                Ok(result) if result.is_error == Some(true) => result.content.iter().filter_map(|content|
                    content.as_text().map(|text| text.text.as_str())).collect::<Vec<_>>().join("\n"),
                Ok(_) => anyhow::bail!("Public over-cap compilation unexpectedly published"),
            };
            ensure!(refusal.contains("aggregate source-byte limit"), "Wrong public refusal: {refusal}");
            ensure!(app.state.registry.source_head_is_current(&head)?
                && app.state.registry.get_meta(&pid, "source_head_revision_v1")? == revision
                && inventory()? == physical,
                "Compilation admission failure must preserve the published source head and selected physical data");
            for number in 0..4 { std::fs::remove_file(repo_root.join(format!("OverAggregate{number}.vb")))?; }
            std::fs::write(repo_root.join("Large.vb"), format!("{source}' fallbackinventorymarker valid retry\n"))?;
            update(&app, &pid).await?;
            ensure!(app.state.registry.active_source_generation(&pid)? > head.generation(),
                "Removing the over-cap peers must allow the real changed source to publish on retry");
            let retried = app.state.graph.get_node(&pid, "file:Large.vb")?.unwrap();
            let retried: VbParserProvenance = serde_json::from_value(retried.metadata.unwrap()["vb_parser_provenance"].clone())?;
            ensure!(matches!(retried, VbParserProvenance::RustFallback {
                reason: engram_index::vb_extractor::VbFallbackReason::SourceLimit, ..
            }), "Aggregate admission must not relabel the independent per-request fallback policy");
            settle(&app).await?;
            Ok::<(), anyhow::Error>(())
        });
        drop(runtime);
        tested
    })();
    let stopped = shutdown_idle_parser();
    drop(owner);
    let cleanup = owned.close();
    result?;
    stopped?;
    cleanup?;
    Ok(())
}
