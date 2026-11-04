//! Cold configuration changes must migrate produced source layout exactly once.
use anyhow::{Result, ensure};
use engram_core::{Config, ContentHash, MemorySection, RelPath};
use engram_index::{HybridQuery, HybridSearchEngine, IndexDoc, SourceSnapshotGuard};
use engram_server::{AppState, Engram};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Row {
    path: String,
    pk: String,
    first: u32,
    last: u32,
    body: String,
}

fn source(name: &str, functions: usize) -> String {
    (0..functions).map(|n|format!(
        "pub fn {name}_{n:03}() -> usize {{ {n} }} // s03marker independently identifiable unchanged source function {n:03}\n"
    )).collect()
}

fn config(root: &Path, target: usize) -> Config {
    Config {
        data_dir: root.join("data"),
        allowed_roots: vec![root.join("repo")],
        max_chunks_per_file: target,
        embedding_backend: if cfg!(feature = "vector") {
            "local"
        } else {
            "fts_only"
        }
        .into(),
        llm_backend: "none".into(),
        ..Default::default()
    }
}

fn open(cfg: &Config) -> Result<Engram> {
    Ok(Engram::new(AppState::new(cfg.clone())?.0))
}

async fn settle(app: &Engram) -> Result<()> {
    loop {
        let jobs = app
            .state
            .active_jobs
            .write()
            .await
            .drain()
            .map(|(_, job)| job)
            .collect::<Vec<_>>();
        if jobs.is_empty() {
            return Ok(());
        }
        for job in jobs {
            job.await?;
        }
    }
}

async fn initial(root: &Path, cfg: &Config) -> Result<(Engram, String)> {
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo)?;
    drop(git2::Repository::init(&repo)?);
    std::fs::write(repo.join("Source.rs"), source("original", 14))?;
    std::fs::write(repo.join("Companion.rs"), source("companion", 2))?;
    let app = open(cfg)?;
    let indexed=app.handle_index_project(serde_json::from_value(json!({
        "directory":repo,"project_name":"Source extraction compatibility","project_type":"rust","wait":true
    }))?).await;
    settle(&app).await?;
    indexed.map_err(|error| anyhow::anyhow!("{error}"))?;
    let projects = app.state.registry.list_projects()?;
    ensure!(
        projects.len() == 1,
        "Migration must retain one project identity"
    );
    let pid = projects[0].project_id.clone();
    let section: MemorySection = serde_json::from_value(json!({
        "section_id":"layout-decision","title":"Retained knowledge","content":"Preserve this authored decision across source layout migrations.","updated_at_ms":1
    }))?;
    app.state.registry.put_memory_section(&pid, &section)?;
    let ps = app
        .state
        .get_project_cached(&pid)
        .ok_or_else(|| anyhow::anyhow!("Initial runtime missing"))?;
    let body = "s03knowledge persistent authored layout decision";
    ps.search
        .index_docs(
            &pid,
            &[IndexDoc {
                generation: 0,
                chunk_id: 1,
                path: RelPath::new("authored-note.txt"),
                language: "text".into(),
                content: body.into(),
                namespace: "memory_bank".into(),
                author: None,
                timestamp: None,
                start_line: 1,
                end_line: 1,
                doc_id: "retained-layout-note".into(),
                content_hash: ContentHash::compute(body.as_bytes()).0,
            }],
            &CancellationToken::new(),
        )
        .await?;
    drop(ps);
    Ok((app, pid))
}

fn request(pid: &str, generation: u64, namespace: &str, text: &str) -> HybridQuery {
    HybridQuery {
        project_id: pid.into(),
        namespace: namespace.into(),
        generation,
        text: text.into(),
        top_k: 128,
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

async fn rows(
    search: &HybridSearchEngine,
    pid: &str,
    generation: u64,
    held: &SourceSnapshotGuard,
    companion: &str,
) -> Result<Vec<Row>> {
    let original = source("original", 14);
    let expected = BTreeMap::from([
        ("Source.rs", original.as_str()),
        ("Companion.rs", companion),
    ]);
    let query = request(pid, generation, "memory", "s03marker");
    let hits = search.lexical_search(&query)?;
    ensure!(
        hits.len() == search.count_docs_in_generation(pid, "memory", generation)?,
        "The oracle must enumerate every selected source chunk"
    );
    let keys = hits
        .iter()
        .map(|hit| hit.pk.clone())
        .collect::<BTreeSet<_>>();
    ensure!(keys.len() == hits.len());
    #[cfg(feature = "vector")]
    {
        let vectors = search
            .vector_search(&query, &CancellationToken::new())
            .await?;
        ensure!(
            vectors.len() == hits.len()
                && vectors
                    .iter()
                    .map(|hit| hit.pk.clone())
                    .collect::<BTreeSet<_>>()
                    == keys,
            "Selected native vectors must match the complete lexical source groups"
        );
    }
    let mut result = Vec::new();
    let mut covered: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
    for hit in hits {
        let (path, _, body, first, last) = search
            .recover_selected_source_hit(pid, held, &hit)?
            .ok_or_else(|| anyhow::anyhow!("Selected stored body missing"))?;
        let known = expected
            .get(path.as_str())
            .ok_or_else(|| anyhow::anyhow!("Unexpected source path {path}"))?;
        let lines = known.lines().collect::<Vec<_>>();
        ensure!(first > 0 && first <= last && last as usize <= lines.len());
        ensure!(
            body == lines[first as usize - 1..last as usize].join("\n") + "\n",
            "Stored source bytes differ from the fixture line range"
        );
        covered
            .entry(path.as_str().into())
            .or_default()
            .extend(first..=last);
        result.push(Row {
            path: path.as_str().into(),
            pk: hit.pk,
            first,
            last,
            body,
        });
    }
    ensure!(covered.len() == expected.len());
    for (path, text) in expected {
        ensure!(
            covered.get(path) == Some(&(1..=text.lines().count() as u32).collect()),
            "Incomplete source layout for {path}"
        );
    }
    result.sort();
    Ok(result)
}

async fn current(app: &Engram, pid: &str, companion: &str) -> Result<(u64, Vec<Row>)> {
    let generation = app.state.registry.active_source_generation(pid)?;
    let ps =
        engram_server::services::project_service::ensure_project_runtime(&app.state, pid).await?;
    let held = ps.search.pin_source_generation(pid, generation)?;
    Ok((
        generation,
        rows(&ps.search, pid, generation, &held, companion).await?,
    ))
}

async fn knowledge(app: &Engram, pid: &str) -> Result<Row> {
    ensure!(
        app.state
            .registry
            .get_memory_section(pid, "layout-decision")?
            .is_some_and(|section| section.content
                == "Preserve this authored decision across source layout migrations.")
    );
    let ps =
        engram_server::services::project_service::ensure_project_runtime(&app.state, pid).await?;
    let mut query = request(pid, 0, "memory_bank", "s03knowledge");
    // Public indexing also stores its report in memory_bank. Vector search
    // ranks all eligible documents, so select this owned note by its path in
    // both stores rather than treating a literal query as a vector filter.
    query.include_path_prefixes = Some(vec!["authored-note.txt".into()]);
    let hits = ps.search.lexical_search(&query)?;
    ensure!(
        hits.len() == 1,
        "Unrelated authored knowledge was removed or duplicated"
    );
    ensure!(
        hits[0].pk == engram_core::build_pk(pid, "memory_bank", 0, "retained-layout-note"),
        "Owned GlobalMutable knowledge must retain its namespace/gen0 identity: {}",
        hits[0].pk
    );
    #[cfg(feature = "vector")]
    {
        let vectors = ps
            .search
            .vector_search(&query, &CancellationToken::new())
            .await?;
        ensure!(
            vectors.len() == 1 && vectors[0].pk == hits[0].pk,
            "Owned memory_bank/gen0 note differs between stores; lexical={:?}; vectors={:?}",
            hits.iter()
                .map(|hit| (&hit.pk, hit.path.as_str()))
                .collect::<Vec<_>>(),
            vectors
                .iter()
                .map(|hit| (&hit.pk, hit.path.as_str()))
                .collect::<Vec<_>>()
        );
    }
    let (path, _, body, first, last) = ps
        .search
        .get_doc_by_pk(&hits[0].pk)?
        .ok_or_else(|| anyhow::anyhow!("Knowledge body missing"))?;
    Ok(Row {
        path: path.as_str().into(),
        pk: hits[0].pk.clone(),
        first,
        last,
        body,
    })
}

async fn update(app: &Engram, pid: &str) -> Result<String> {
    let response = app
        .handle_update_project(serde_json::from_value(json!({
            "project_id":pid,"wait":true,"max_commits":1,"index_antipatterns":false
        }))?)
        .await;
    settle(app).await?;
    let response = response.map_err(|error| anyhow::anyhow!("{error}"))?;
    Ok(response
        .content
        .iter()
        .filter_map(|content| content.as_text())
        .map(|text| text.text.as_str())
        .collect::<Vec<_>>()
        .join("\n"))
}

fn run(
    case: impl FnOnce(PathBuf) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>>>>,
) -> Result<()> {
    let owned = tempfile::tempdir()?;
    let result = {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(case(owned.path().to_path_buf()))
    };
    // Drain actual blocking/native owners before removing the one fixture tree,
    // including when a wanted-behavior assertion returned an error.
    owned.close()?;
    result
}

#[test]
fn cold_chunk_target_change_reextracts_unchanged_multifunction_source_and_preserves_old_reader_and_knowledge()
-> Result<()> {
    run(|root| {
        Box::pin(async move {
            let wide = config(&root, 600);
            let (app, pid) = initial(&root, &wide).await?;
            let companion = source("companion", 2);
            let (old_generation, before) = current(&app, &pid, &companion).await?;
            let previous =
                engram_server::services::project_service::ensure_project_runtime(&app.state, &pid)
                    .await?
                    .search;
            let held = previous.pin_source_generation(&pid, old_generation)?;
            let foreign = knowledge(&app, &pid).await?;
            let path = root.join("repo/Source.rs");
            let bytes = std::fs::read(&path)?;
            let before_stat = std::fs::metadata(&path)?;
            ensure!(
                before.iter().filter(|row| row.path == "Source.rs").count() > 1,
                "Initial source must already be multi-chunk"
            );
            settle(&app).await?;
            drop(app);
            let narrow = config(&root, 80);
            let cold = open(&narrow)?;
            let outdated = engram_server::services::project_service::outdated_source_index_paths(
                &cold.state,
                &pid,
            )
            .await?;
            ensure!(
                outdated.into_iter().collect::<BTreeSet<_>>()
                    == BTreeSet::from(["Source.rs".into(), "Companion.rs".into()]),
                "A chunk-target mismatch must enter source-format migration before stat/hash reuse"
            );
            let (changed, deleted) =
                engram_server::services::project_service::get_incremental_changes(
                    &cold.state,
                    &pid,
                    &root.join("repo"),
                    &["rs"],
                )
                .await?;
            ensure!(
                changed.into_iter().collect::<BTreeSet<_>>()
                    == BTreeSet::from([
                        root.join("repo/Source.rs"),
                        root.join("repo/Companion.rs")
                    ])
                    && deleted.is_empty()
            );
            let footprint = cold
                .handle_get_concept_footprint(serde_json::from_value(json!({
                    "project_id":pid,"concept":"s03marker","max_per_group":128
                }))?)
                .await
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            let footprint = footprint
                .content
                .iter()
                .filter_map(|content| content.as_text())
                .map(|text| text.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            ensure!(
                footprint.contains("- literal: failed")
                    && footprint.contains("literal pass failed:")
                    && footprint.contains("update_project"),
                "Incompatible stored layout must report failed literal coverage and migration guidance: {footprint}"
            );
            update(&cold, &pid).await?;
            let (new_generation, after) = current(&cold, &pid, &companion).await?;
            ensure!(new_generation > old_generation);
            ensure!(
                after.iter().filter(|row| row.path == "Source.rs").count()
                    > before.iter().filter(|row| row.path == "Source.rs").count(),
                "Changing only the chunk target must actually produce the narrower layout"
            );
            ensure!(
                rows(&previous, &pid, old_generation, &held, &companion).await? == before,
                "Already-held old chunk bodies and identities must survive migration and collection"
            );
            ensure!(knowledge(&cold, &pid).await? == foreign);
            ensure!(cold.state.registry.list_projects()?.len() == 1);
            ensure!(
                engram_server::services::project_service::outdated_source_index_paths(
                    &cold.state,
                    &pid
                )
                .await?
                .is_empty()
            );
            let after_stat = std::fs::metadata(&path)?;
            ensure!(
                std::fs::read(&path)? == bytes
                    && before_stat.len() == after_stat.len()
                    && before_stat.modified()? == after_stat.modified()?,
                "Migration must be caused by configuration, not a source edit or timestamp workaround"
            );
            drop(held);
            drop(previous);
            settle(&cold).await?;
            drop(cold);
            Ok(())
        })
    })
}

#[test]
fn same_target_cold_reopen_is_noop_and_companion_edit_inherits_the_migrated_physical_chunks()
-> Result<()> {
    run(|root| {
        Box::pin(async move {
            let (app, pid) = initial(&root, &config(&root, 600)).await?;
            let companion = source("companion", 2);
            let foreign = knowledge(&app, &pid).await?;
            let old_generation = app.state.registry.active_source_generation(&pid)?;
            settle(&app).await?;
            drop(app);
            let narrow = config(&root, 80);
            let migrated = open(&narrow)?;
            update(&migrated, &pid).await?;
            let (generation, baseline) = current(&migrated, &pid, &companion).await?;
            ensure!(
                generation > old_generation
                    && baseline
                        .iter()
                        .filter(|row| row.path == "Source.rs")
                        .count()
                        > 2
            );
            settle(&migrated).await?;
            drop(migrated);
            let cold = open(&narrow)?;
            ensure!(
                engram_server::services::project_service::outdated_source_index_paths(
                    &cold.state,
                    &pid
                )
                .await?
                .is_empty()
            );
            let (changed, deleted) =
                engram_server::services::project_service::get_incremental_changes(
                    &cold.state,
                    &pid,
                    &root.join("repo"),
                    &["rs"],
                )
                .await?;
            ensure!(
                changed.is_empty() && deleted.is_empty(),
                "Matching cold extraction identity must reuse the published layout"
            );
            let no_change = update(&cold, &pid).await?;
            ensure!(
                no_change.contains("no_changes_detected"),
                "Matching identity should report a source noop: {no_change}"
            );
            ensure!(
                current(&cold, &pid, &companion).await? == (generation, baseline.clone()),
                "A cold noop rewrote physical source chunks"
            );
            let changed_companion = source("changed_companion", 2);
            std::fs::write(root.join("repo/Companion.rs"), &changed_companion)?;
            update(&cold, &pid).await?;
            let (current_generation, after) = current(&cold, &pid, &changed_companion).await?;
            ensure!(current_generation > generation);
            ensure!(
                after
                    .iter()
                    .filter(|row| row.path == "Source.rs")
                    .collect::<Vec<_>>()
                    == baseline
                        .iter()
                        .filter(|row| row.path == "Source.rs")
                        .collect::<Vec<_>>(),
                "An unrelated edit must inherit the migrated physical chunk identities and bodies"
            );
            ensure!(
                after
                    .iter()
                    .filter(|row| row.path == "Companion.rs")
                    .all(|row| !baseline.iter().any(|old| old.pk == row.pk)),
                "The companion edit must actually publish new content"
            );
            ensure!(knowledge(&cold, &pid).await? == foreign);
            ensure!(
                engram_server::services::project_service::outdated_source_index_paths(
                    &cold.state,
                    &pid
                )
                .await?
                .is_empty()
            );
            let (changed, deleted) =
                engram_server::services::project_service::get_incremental_changes(
                    &cold.state,
                    &pid,
                    &root.join("repo"),
                    &["rs"],
                )
                .await?;
            ensure!(
                changed.is_empty() && deleted.is_empty(),
                "Extraction migration must not repeat after an ordinary edit"
            );
            settle(&cold).await?;
            drop(cold);
            Ok(())
        })
    })
}
