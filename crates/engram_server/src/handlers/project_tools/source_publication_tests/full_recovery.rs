//! Explicit full recovery must preserve complete readers and unrelated knowledge.
use super::*;
use anyhow::{Result, ensure};
use engram_index::{HybridSearchEngine, SourceSnapshotGuard};
use redb::ReadableTable;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

const WATERMARKS: [(&str, &str); 4] = [
    ("last_git_oid", "retained-s13-history-tip"),
    ("oldest_indexed_git_oid", "retained-s13-history-start"),
    ("git_backfill_complete", "true"),
    ("pr_ingest_watermark", "retained-s13-pr-cursor"),
];

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Row {
    path: String,
    pk: String,
    doc_id: String,
    first: u32,
    last: u32,
    text: String,
}

fn source(version: &str, lines: usize) -> String {
    (0..lines).map(|n| format!(
        "pub fn {version}_{n:03}() -> usize {{ {n} }} // s13marker complete immutable source line {version}_{n:03}\n"
    )).collect()
}

fn configuration(root: &Path) -> Config {
    Config {
        data_dir: root.join("data"),
        allowed_roots: vec![root.join("repo")],
        embedding_backend: if cfg!(feature = "vector") {
            "local"
        } else {
            "fts_only"
        }
        .into(),
        max_chunks_per_file: 80,
        llm_backend: "none".into(),
        ..Default::default()
    }
}

fn reopen(cfg: &Config) -> Result<Engram> {
    Ok(Engram::new(AppState::new(cfg.clone())?.0))
}

async fn repair(app: &Engram) -> Result<()> {
    app.handle_repair_project(serde_json::from_value(json!({
        "project_id":PROJECT,"scope":"full","wipe_and_reindex":false,
        "max_commits":1,"index_antipatterns":false
    }))?)
    .await
    .map_err(|error| anyhow::anyhow!("{error}"))?;
    Ok(())
}

async fn stage(
    app: &Engram,
    search: &HybridSearchEngine,
    generation: u64,
    base: u64,
) -> Result<()> {
    let root = PathBuf::from(
        app.state
            .registry
            .get_project(PROJECT)?
            .ok_or_else(|| anyhow::anyhow!("Missing fixture project"))?
            .directory,
    );
    let stats = app
        .index_full_files_with_parse_guard(
            search,
            PROJECT,
            "memory",
            generation,
            base,
            &root,
            vec![root.join("source.rs"), root.join("sibling.rs")],
            80,
            &engram_index::SourceExtractionExpectation::for_paths(80, ["source.rs", "sibling.rs"] )?,
            &CancellationToken::new(),
            |_, _| {},
        )
        .await?;
    app.process_ingest_stats(PROJECT, generation, &stats)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

async fn source_rows(
    search: &HybridSearchEngine,
    generation: u64,
    view: &SourceSnapshotGuard,
    source_text: &str,
) -> Result<Vec<Row>> {
    let sibling = source("sibling", 1);
    let expected = BTreeMap::from([("source.rs", source_text), ("sibling.rs", sibling.as_str())]);
    let mut request = query(generation, "s13marker");
    request.top_k = 128;
    let hits = search.lexical_search(&request)?;
    ensure!(
        hits.len() == search.count_docs_in_generation(PROJECT, "memory", generation)?,
        "Every selected source chunk must participate in the complete group oracle"
    );
    let keys: BTreeSet<_> = hits.iter().map(|hit| hit.pk.clone()).collect();
    ensure!(keys.len() == hits.len(), "Duplicate source physical key");
    #[cfg(feature = "vector")]
    {
        let vectors = search
            .vector_search(&request, &CancellationToken::new())
            .await?;
        ensure!(
            vectors.len() == hits.len()
                && vectors
                    .iter()
                    .map(|hit| hit.pk.clone())
                    .collect::<BTreeSet<_>>()
                    == keys,
            "Complete selected native and lexical source groups differ"
        );
    }
    let mut rows = Vec::new();
    let mut covered: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
    for hit in hits {
        let (path, _, text, first, last) = search
            .recover_selected_source_hit(PROJECT, view, &hit)?
            .ok_or_else(|| anyhow::anyhow!("Missing exact selected body"))?;
        let known = expected
            .get(path.as_str())
            .ok_or_else(|| anyhow::anyhow!("Unexpected source path {path}"))?;
        let lines: Vec<_> = known.lines().collect();
        ensure!(first > 0 && first <= last && last as usize <= lines.len());
        ensure!(
            text == lines[first as usize - 1..last as usize].join("\n") + "\n",
            "Selected bytes differ from the fixture's complete source lines"
        );
        covered
            .entry(path.as_str().into())
            .or_default()
            .extend(first..=last);
        rows.push(Row {
            path: path.as_str().into(),
            pk: hit.pk,
            doc_id: hit.doc_id.clone(),
            first,
            last,
            text,
        });
    }
    ensure!(covered.len() == expected.len());
    for (path, text) in expected {
        ensure!(
            covered.get(path) == Some(&(1..=text.lines().count() as u32).collect()),
            "Selected revision has missing source lines: {path}"
        );
    }
    ensure!(
        rows.iter().filter(|row| row.path == "source.rs").count() >= 2,
        "Recovery fixture must retain a genuine multi-chunk file"
    );
    rows.sort();
    Ok(rows)
}

async fn current_rows(app: &Engram, text: &str) -> Result<(u64, Vec<Row>)> {
    let generation = app.state.registry.active_source_generation(PROJECT)?;
    let ps = app
        .ensure_project_runtime(PROJECT)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let held = ps.search.pin_source_generation(PROJECT, generation)?;
    Ok((
        generation,
        source_rows(&ps.search, generation, &held, text).await?,
    ))
}

async fn foreign_rows(search: &HybridSearchEngine) -> Result<Vec<(String, Row)>> {
    let mut rows = Vec::new();
    for namespace in ["memory_bank", "history"] {
        let mut request = query(0, "s13foreign");
        request.namespace = namespace.into();
        // Real Git ingestion also owns history vectors. Compare the exact
        // fixture-owned retained group on both legs, not every semantic
        // neighbor in the otherwise legitimately growing namespace.
        request.include_path_prefixes = Some(vec!["retained-".into()]);
        let hits = search.lexical_search(&request)?;
        ensure!(
            hits.len() == 1,
            "Retained {namespace} document disappeared or was duplicated"
        );
        #[cfg(feature = "vector")]
        {
            let vectors = search
                .vector_search(&request, &CancellationToken::new())
                .await?;
            ensure!(
                vectors.len() == 1 && vectors[0].pk == hits[0].pk,
                "Retained native {namespace} identity changed"
            );
        }
        let hit = &hits[0];
        let (path, _, text, first, last) = search
            .get_doc_by_pk(&hit.pk)?
            .ok_or_else(|| anyhow::anyhow!("Retained {namespace} body missing"))?;
        rows.push((
            namespace.into(),
            Row {
                path: path.as_str().into(),
                pk: hit.pk.clone(),
                doc_id: hit.doc_id.clone(),
                first,
                last,
                text,
            },
        ));
    }
    Ok(rows)
}

fn watermarks(app: &Engram) -> Result<()> {
    for (key, wanted) in WATERMARKS {
        ensure!(
            app.state.registry.get_meta(PROJECT, key)?.as_deref() == Some(wanted),
            "Full source recovery changed unrelated ingestion watermark {key}"
        );
    }
    Ok(())
}

struct Previous {
    search: Arc<HybridSearchEngine>,
    held: SourceSnapshotGuard,
    rows: Vec<Row>,
    foreign: Vec<(String, Row)>,
}

async fn seeded(root: &Path, cfg: &Config, corrupt_pending: bool) -> Result<(Engram, Previous)> {
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo)?;
    drop(git2::Repository::init(&repo)?);
    std::fs::write(repo.join("source.rs"), source("original", 8))?;
    std::fs::write(repo.join("sibling.rs"), source("sibling", 1))?;
    let app = reopen(cfg)?;
    app.state.registry.put_project(&ProjectRecord {
        project_id: PROJECT.into(),
        project_name: "Full recovery".into(),
        project_type: "general".into(),
        directory: repo.to_string_lossy().into(),
        created_at_ms: 1,
        updated_at_ms: 1,
        reindex_required_since_ms: None,
    })?;
    app.state.registry.set_meta(PROJECT, "active_generation", "1")?;
    let ps = app
        .ensure_project_runtime(PROJECT)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let base = app
        .capture_source_head(PROJECT, &ps.info.directory, &ps.info.project_type)
        .await?;
    stage(&app, &ps.search, 500, base.generation()).await?;
    let admission = ps.search.begin_project_mutation(PROJECT)?;
    app.publish_source_generation(&admission, PROJECT, base, 500, None,
        engram_index::SourceExtractionExpectation::for_paths(80, ["source.rs", "sibling.rs"] )?)
        .await?;
    drop(admission);
    for namespace in ["memory_bank", "history"] {
        let text = format!("s13foreign retained {namespace} bytes");
        ps.search
            .index_docs(
                PROJECT,
                &[IndexDoc {
                    generation: 0,
                    chunk_id: 1,
                    path: RelPath::new("retained-note.txt"),
                    language: "text".into(),
                    content: text.clone(),
                    namespace: namespace.into(),
                    author: None,
                    timestamp: None,
                    start_line: 1,
                    end_line: 1,
                    doc_id: format!("retained-{namespace}"),
                    content_hash: ContentHash::compute(text.as_bytes()).0,
                }],
                &CancellationToken::new(),
            )
            .await?;
    }
    for (key, value) in WATERMARKS {
        app.state.registry.set_meta(PROJECT, key, value)?;
    }
    let held = ps.search.pin_source_generation(PROJECT, 500)?;
    let rows = source_rows(&ps.search, 500, &held, &source("original", 8)).await?;
    let foreign = foreign_rows(&ps.search).await?;
    let count = ps.search.count_docs(PROJECT)?;
    ps.search
        .prepare_source_snapshot(
            PROJECT,
            500,
            501,
            &[RelPath::new("source.rs")],
            &[],
            &CancellationToken::new(),
        )
        .await?;
    ensure!(
        ps.search.count_docs(PROJECT)? == count,
        "Pending generation fixture must own no replacement chunks"
    );
    if corrupt_pending {
        let identity = blake3::hash(format!("{PROJECT}\0memory").as_bytes())
            .to_hex()
            .to_string();
        let canonical = ps
            .info
            .tantivy_dir
            .join("source-snapshots")
            .join(identity)
            .join("501.json");
        ensure!(
            canonical.is_file(),
            "Expected an owned canonical pending record"
        );
        std::fs::write(canonical, b"{interrupted owned pending record")?;
    }
    let previous = Previous {
        search: ps.search.clone(),
        held,
        rows,
        foreign,
    };
    drop(ps);
    finish_owned_jobs(&app).await;
    Ok((app, previous))
}

fn remove_required_head(cfg: &Config) -> Result<()> {
    // Caller has released every AppState/Registry owner. Only source readers
    // remain alive; they do not own this independently opened registry file.
    let database = redb::Database::open(cfg.data_dir.join("registry/registry.redb"))?;
    let write = database.begin_write()?;
    {
        let mut table = write.open_table(redb::TableDefinition::<&str, &[u8]>::new("meta"))?;
        ensure!(
            table
                .get(format!("{PROJECT}\0source_head_required_v1").as_str())?
                .is_some_and(|value| value.value() == b"1"),
            "Missing-head fixture must retain the authoritative presence requirement"
        );
        let removed = table
            .remove(format!("{PROJECT}\0active_generation").as_str())?
            .is_some();
        ensure!(
            removed,
            "The fixture must remove a genuinely required published head"
        );
    }
    write.commit()?;
    drop(database);
    Ok(())
}

async fn recover_and_continue(root: PathBuf, missing: bool) -> Result<()> {
    let cfg = configuration(&root);
    let (app, previous) = seeded(&root, &cfg, missing).await?;
    let mut current_root = root.join("repo");
    let baseline_jobs: BTreeSet<_> = app
        .state
        .registry
        .list_jobs(Some(PROJECT))?
        .into_iter()
        .map(|job| job.job_id)
        .collect();
    let app = if missing {
        finish_owned_jobs(&app).await;
        drop(app);
        remove_required_head(&cfg)?;
        reopen(&cfg)?
    } else {
        let relocated = current_root.join("relocated");
        std::fs::create_dir_all(&relocated)?;
        drop(git2::Repository::init(&relocated)?);
        std::fs::write(relocated.join("sibling.rs"), source("sibling", 1))?;
        let mut project = app
            .state
            .registry
            .get_project(PROJECT)?
            .ok_or_else(|| anyhow::anyhow!("Missing fixture project"))?;
        project.directory = relocated.to_string_lossy().into_owned();
        project.project_type = "rust".into();
        app.state.registry.put_project(&project)?;
        ensure!(
            app.state
                .projects
                .get(PROJECT)
                .is_some_and(|cached| Arc::ptr_eq(&cached.search, &previous.search)
                    && cached.info.directory != project.directory
                    && cached.info.project_type != project.project_type),
            "Fixture must retain the old cached engine and stale source identity"
        );
        current_root = relocated;
        app.state
            .registry
            .set_meta(PROJECT, "active_generation", "corrupt-owned-head")?;
        app
    };
    ensure!(
        app.state.registry.capture_source_head(PROJECT).is_err(),
        "Fixture must enter explicit damaged-head recovery"
    );
    let repaired_text = source("recovered", 8);
    std::fs::write(current_root.join("source.rs"), &repaired_text)?;
    repair(&app).await?;
    finish_owned_jobs(&app).await;
    if !missing {
        ensure!(
            app.state
                .projects
                .get(PROJECT)
                .is_some_and(|cached| Arc::ptr_eq(&cached.search, &previous.search)
                    && cached.info.directory == current_root.to_string_lossy().as_ref()
                    && cached.info.project_type == "rust"),
            "Damaged-head repair must use the corrected directory/type with the same engine"
        );
    }
    let (generation, rows) = current_rows(&app, &repaired_text).await?;
    ensure!(
        generation > 501,
        "Recovery must clear neither held500 nor the canonical pending501 reservation"
    );
    ensure!(
        source_rows(
            &previous.search,
            500,
            &previous.held,
            &source("original", 8)
        )
        .await?
            == previous.rows,
        "Explicit recovery cleared or changed a complete already-held source revision"
    );
    ensure!(foreign_rows(&previous.search).await? == previous.foreign);
    watermarks(&app)?;
    ensure!(
        app.state
            .registry
            .list_jobs(Some(PROJECT))?
            .into_iter()
            .map(|job| job.job_id)
            .collect::<BTreeSet<_>>()
            == baseline_jobs,
        "Source recovery must not enqueue an unrelated history rebuild"
    );
    let foreign = previous.foreign.clone();
    drop(previous);
    drop(app);

    // A genuine cold AppState/engine open, with no fixture reinitialization.
    let cold = reopen(&cfg)?;
    ensure!(current_rows(&cold, &repaired_text).await? == (generation, rows));
    watermarks(&cold)?;
    let ps = cold
        .ensure_project_runtime(PROJECT)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    ensure!(foreign_rows(&ps.search).await? == foreign);
    drop(ps);
    let next = source("after_restart", 8);
    std::fs::write(current_root.join("source.rs"), &next)?;
    cold.handle_update_project(serde_json::from_value(json!({
        "project_id":PROJECT,"wait":true,"max_commits":1,"index_antipatterns":false
    }))?)
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    ensure!(
        current_rows(&cold, &next).await?.0 > generation,
        "The next real source edit must publish after recovery/reopen"
    );
    let ps = cold
        .ensure_project_runtime(PROJECT)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    ensure!(foreign_rows(&ps.search).await? == foreign);
    drop(ps);
    finish_owned_jobs(&cold).await;
    drop(cold);
    Ok(())
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
    // Runtime teardown joins owned blocking work, including error unwinding,
    // before the only fixture directory is removed.
    owned.close()?;
    result
}

#[test]
fn corrupt_head_full_repair_uses_relocated_source_without_reopening_engine_or_losing_held_revision()
-> Result<()> {
    run(|root| Box::pin(recover_and_continue(root, false)))
}

#[test]
fn missing_required_head_full_repair_skips_corrupt_pending_filename_and_preserves_held_source()
-> Result<()> {
    run(|root| Box::pin(recover_and_continue(root, true)))
}

#[test]
fn stale_full_recovery_cannot_publish_over_an_already_complete_winner() -> Result<()> {
    run(|root| {
        Box::pin(async move {
            let cfg = configuration(&root);
            let (app, previous) = seeded(&root, &cfg, false).await?;
            let winner = source("winner", 8);
            std::fs::write(root.join("repo/source.rs"), &winner)?;
            stage(&app, &previous.search, 700, 500).await?;
            let winner_held = previous.search.pin_source_generation(PROJECT, 700)?;
            let winner_rows = source_rows(&previous.search, 700, &winner_held, &winner).await?;
            let candidate = source("stale_candidate", 8);
            std::fs::write(root.join("repo/source.rs"), &candidate)?;
            app.state
                .registry
                .set_meta(PROJECT, "active_generation", "corrupt-owned-head")?;
            let pause = PublicationPause {
                project: PROJECT.into(),
                after_commit: false,
                fail_cleanup: false,
                entered: Arc::new(tokio::sync::Notify::new()),
                resume: Arc::new(tokio::sync::Notify::new()),
            };
            let registry_id = Arc::as_ptr(&app.state.registry) as usize;
            BACKGROUND_PUBLICATION_PAUSES
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(registry_id, pause.clone());
            let release = ResumePublication {
                registry_id,
                resume: pause.resume.clone(),
            };
            let control = async {
                let outcome = tokio::time::timeout(Duration::from_secs(30), async {
                    pause.entered.notified().await;
                    let candidate_gen = previous
                        .search
                        .source_generation_high_water(PROJECT, &CancellationToken::new())
                        .await?;
                    ensure!(
                        candidate_gen > 700,
                        "The public repair must actually seal a fresh candidate"
                    );
                    let refused = previous.search.pin_source_generation(PROJECT, candidate_gen)
                        .err().ok_or_else(|| anyhow::anyhow!("A fresh source reader bypassed the deliberately corrupt published HEAD"))?;
                    ensure!(format!("{refused:#}").contains("Invalid active_generation"),
                        "Candidate inspection did not refuse the damaged source authority: {refused:#}");
                    // Install the complete direct Registry winner while the
                    // public repair remains paused before its exact CAS. Only
                    // then may a fresh reader inspect the already-sealed
                    // candidate using the now-valid published authority.
                    app.state
                        .registry
                        .set_meta(PROJECT, "active_generation", "700")?;
                    let held = previous
                        .search
                        .pin_source_generation(PROJECT, candidate_gen)?;
                    source_rows(&previous.search, candidate_gen, &held, &candidate).await?;
                    drop(held);
                    Ok::<_, anyhow::Error>(())
                })
                .await;
                // Release the actual handler on success, timeout and observation errors.
                drop(release);
                outcome?
            };
            let (repaired, changed) = tokio::join!(repair(&app), control);
            changed?;
            let error = repaired
                .err()
                .ok_or_else(|| anyhow::anyhow!("Stale recovery published over the winner"))?;
            ensure!(
                format!("{error:#}").contains("publication"),
                "Failure did not reach publication CAS: {error:#}"
            );
            ensure!(app.state.registry.active_source_generation(PROJECT)? == 700);
            ensure!(
                source_rows(&previous.search, 700, &winner_held, &winner).await? == winner_rows
            );
            ensure!(
                source_rows(
                    &previous.search,
                    500,
                    &previous.held,
                    &source("original", 8)
                )
                .await?
                    == previous.rows
            );
            ensure!(foreign_rows(&previous.search).await? == previous.foreign);
            watermarks(&app)?;
            drop(winner_held);
            drop(previous);
            finish_owned_jobs(&app).await;
            drop(app);
            Ok(())
        })
    })
}

#[derive(Clone, Copy)]
enum DurableDamage { MissingCheckpoint, TruncatedCheckpoint, MissingLegacyBoundary, MissingRetirementBoundary, TruncatedRetirementBoundary }

fn durable_scope(cfg: &Config) -> PathBuf {
    cfg.data_dir.join("projects").join(PROJECT).join("tantivy/source-snapshots")
        .join(blake3::hash(format!("{PROJECT}\0memory").as_bytes()).to_hex().as_str())
}

async fn durable_warm_case(root: &Path, cfg: &Config, damage: DurableDamage) -> Result<(u64, Vec<Row>)> {
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo)?;
    let before_text = source("durable_before", 6);
    let repaired_text = source("durable_repaired", 6);
    std::fs::write(repo.join("source.rs"), &before_text)?;
    std::fs::write(repo.join("sibling.rs"), source("sibling", 1))?;
    {
        let git = git2::Repository::init(&repo)?;
        let mut index = git.index()?;
        index.add_path(Path::new("source.rs"))?; index.add_path(Path::new("sibling.rs"))?;
        let tree = git.find_tree(index.write_tree()?)?;
        let signature = git2::Signature::now("Owned durable source fixture", "fixture@example.invalid")?;
        git.commit(Some("HEAD"), &signature, &signature, "owned source baseline", &tree, &[])?;
    }
    let app = reopen(cfg)?;
    app.state.registry.put_project(&ProjectRecord {
        project_id: PROJECT.into(), project_name: "Durable source ancestry".into(),
        project_type: "general".into(), directory: repo.to_string_lossy().into_owned(),
        created_at_ms: 1, updated_at_ms: 1, reindex_required_since_ms: None,
    })?;
    app.state.registry.set_meta(PROJECT, "active_generation", "1")?;
    repair(&app).await?;
    finish_owned_jobs(&app).await;
    let base = app.state.registry.capture_source_head(PROJECT)?;
    ensure!(base.generation() == 2, "The public full repair must publish the genuine first managed checkpoint");
    let ps = app.ensure_project_runtime(PROJECT).await.map_err(|error| anyhow::anyhow!("{error}"))?;
    let held = ps.search.pin_source_generation(PROJECT, base.generation())?;
    let before = source_rows(&ps.search, base.generation(), &held, &before_text).await?;
    // Persist and warm a real retirement boundary through public maintenance,
    // while the actual already-held complete source reader remains valid.
    app.handle_incremental_indexing_gc(serde_json::from_value(json!({
        "project_id": PROJECT, "compact_vectors": false
    }))?).await.map_err(|error| anyhow::anyhow!("{error}"))?;
    let scope = durable_scope(cfg);
    let damaged = match damage {
        DurableDamage::MissingCheckpoint | DurableDamage::TruncatedCheckpoint => scope.join("2.json"),
        DurableDamage::MissingLegacyBoundary => scope.join("legacy-through.json"),
        DurableDamage::MissingRetirementBoundary | DurableDamage::TruncatedRetirementBoundary => scope.join("retired-before.json"),
    };
    let original = std::fs::read(&damaged)?;
    match damage {
        DurableDamage::MissingCheckpoint | DurableDamage::MissingLegacyBoundary | DurableDamage::MissingRetirementBoundary => std::fs::remove_file(&damaged)?,
        _ => std::fs::write(&damaged, b"{truncated-owned-durable-metadata")?,
    }
    std::fs::write(repo.join("source.rs"), &repaired_text)?;
    let refused = app.handle_update_project(serde_json::from_value(json!({
        "project_id": PROJECT, "wait": true, "max_commits": 1, "index_antipatterns": false
    }))?).await;
    ensure!(refused.is_err(), "A warm cached parent/floor must not certify a newly published durable source head");
    ensure!(app.state.registry.source_head_is_current(&base)?, "A refused update changed source authority");
    ensure!(source_rows(&ps.search, base.generation(), &held, &before_text).await? == before,
        "Refusal must preserve the complete already-held lexical/native source view");
    if let Ok(raw) = std::fs::read(scope.join("3.json")) {
        let record: serde_json::Value = serde_json::from_slice(&raw)?;
        ensure!(record["pending"] == true, "Failed durable validation made the candidate eligible for publication");
    }
    if matches!(damage, DurableDamage::TruncatedRetirementBoundary) {
        // Unknown malformed boundary authority is not permission to guess a
        // new floor. Keep that distinct from parentless checkpoint recovery.
        ensure!(repair(&app).await.is_err(), "Malformed retirement authority was silently reset by full repair");
        ensure!(app.state.registry.source_head_is_current(&base)?
            && std::fs::read(&damaged)? == b"{truncated-owned-durable-metadata");
        ensure!(source_rows(&ps.search, base.generation(), &held, &before_text).await? == before);
        // This fixture owns the exact pre-damage bytes, not an inferred reset.
        std::fs::write(&damaged, &original)?;
    }
    // Non-wiping public full repair writes a parentless checkpoint; it must
    // not require loading/replacing the damaged old parent or its held view.
    repair(&app).await?;
    let generation = app.state.registry.active_source_generation(PROJECT)?;
    ensure!(generation > base.generation());
    let repaired = source_rows(&ps.search, generation,
        &ps.search.pin_source_generation(PROJECT, generation)?, &repaired_text).await?;
    ensure!(source_rows(&ps.search, base.generation(), &held, &before_text).await? == before,
        "Parentless full repair must preserve the genuine held old source reader");
    drop(held); drop(ps);
    finish_owned_jobs(&app).await;
    drop(app);
    Ok((generation, repaired))
}

fn durable_public_case(damage: DurableDamage) -> Result<()> {
    let owned = tempfile::tempdir()?;
    let outcome = (|| -> Result<()> {
        let cfg = configuration(owned.path());
        let (generation, repaired) = {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            runtime.block_on(durable_warm_case(owned.path(), &cfg, damage))?
        };
        // The first runtime, AppState, engine and sole held view are all gone.
        // This cold reopen must reconstruct the published selection from disk.
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        runtime.block_on(async {
            let app = reopen(&cfg)?;
            let recovered_text = source("durable_repaired", 6);
            ensure!(current_rows(&app, &recovered_text).await? == (generation, repaired.clone()),
                "Cold reopen could not reconstruct the complete repaired source");
            let next = source("durable_next_edit", 6);
            std::fs::write(owned.path().join("repo/source.rs"), &next)?;
            app.handle_update_project(serde_json::from_value(json!({
                "project_id": PROJECT, "wait": true, "max_commits": 1, "index_antipatterns": false
            }))?).await.map_err(|error| anyhow::anyhow!("{error}"))?;
            let (current, next_rows) = current_rows(&app, &next).await?;
            ensure!(current > generation
                && next_rows.iter().filter(|row| row.path == "sibling.rs").collect::<Vec<_>>()
                    == repaired.iter().filter(|row| row.path == "sibling.rs").collect::<Vec<_>>(),
                "The next ordinary edit must retain the repaired unchanged sibling group");
            finish_owned_jobs(&app).await;
            drop(app);
            Ok(())
        })
    })();
    let cleanup = owned.close();
    match (outcome, cleanup) {
        (Err(error), Err(cleanup)) => Err(error.context(format!("Owned durable-source cleanup failed: {cleanup}"))),
        (Err(error), _) => Err(error), (Ok(()), cleanup) => cleanup.map_err(Into::into),
    }
}

#[test]
fn warm_missing_or_truncated_parent_refuses_publication_then_parentless_repair_survives_restart() -> Result<()> {
    for damage in [DurableDamage::MissingCheckpoint, DurableDamage::TruncatedCheckpoint] {
        durable_public_case(damage)?;
    }
    Ok(())
}

#[test]
fn warm_missing_or_corrupt_boundaries_refuse_without_guessing_and_preserve_held_source() -> Result<()> {
    for damage in [DurableDamage::MissingLegacyBoundary, DurableDamage::MissingRetirementBoundary, DurableDamage::TruncatedRetirementBoundary] {
        durable_public_case(damage)?;
    }
    Ok(())
}


async fn cold_retirement_seed(root: &Path, cfg: &Config) -> Result<(u64, Vec<Row>)> {
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo)?;
    let initial = source("cold_retired", 6);
    std::fs::write(repo.join("source.rs"), &initial)?;
    std::fs::write(repo.join("sibling.rs"), source("sibling", 1))?;
    {
        let git = git2::Repository::init(&repo)?;
        let mut index = git.index()?;
        index.add_path(Path::new("source.rs"))?; index.add_path(Path::new("sibling.rs"))?;
        let tree = git.find_tree(index.write_tree()?)?;
        let signature = git2::Signature::now("Owned cold boundary fixture", "fixture@example.invalid")?;
        git.commit(Some("HEAD"), &signature, &signature, "owned source baseline", &tree, &[])?;
    }
    let app = reopen(cfg)?;
    app.state.registry.put_project(&ProjectRecord {
        project_id: PROJECT.into(), project_name: "Cold retirement boundary".into(),
        project_type: "general".into(), directory: repo.to_string_lossy().into_owned(),
        created_at_ms: 1, updated_at_ms: 1, reindex_required_since_ms: None,
    })?;
    app.state.registry.set_meta(PROJECT, "active_generation", "1")?;
    repair(&app).await?;
    let (old_generation, old_rows) = current_rows(&app, &initial).await?;
    ensure!(old_generation == 2);
    // The helper's only source lease is gone before publication/collection:
    // real collection must release the changed physical source2 group.
    let current = source("cold_current", 6);
    std::fs::write(repo.join("source.rs"), &current)?;
    app.handle_update_project(serde_json::from_value(json!({
        "project_id": PROJECT, "wait": true, "max_commits": 1, "index_antipatterns": false
    }))?).await.map_err(|error| anyhow::anyhow!("{error}"))?;
    let (generation, rows) = current_rows(&app, &current).await?;
    ensure!(generation == 3);
    app.handle_incremental_indexing_gc(serde_json::from_value(json!({
        "project_id": PROJECT, "compact_vectors": false
    }))?).await.map_err(|error| anyhow::anyhow!("{error}"))?;
    let ps = app.ensure_project_runtime(PROJECT).await.map_err(|error| anyhow::anyhow!("{error}"))?;
    for row in old_rows.iter().filter(|row| row.path == "source.rs") {
        ensure!(ps.search.get_doc_by_pk(&row.pk)?.is_none(), "Real GC retained the changed retired physical source2 group");
    }
    ensure!(rows.iter().filter(|row| row.path == "sibling.rs").collect::<Vec<_>>()
        == old_rows.iter().filter(|row| row.path == "sibling.rs").collect::<Vec<_>>(),
        "The live third source must actually inherit its unchanged physical source2 sibling");
    ensure!(durable_scope(cfg).join("2.json").is_file(), "GC must retain checkpoint2 as active3 ancestry");
    let floor: u64 = serde_json::from_slice(&std::fs::read(durable_scope(cfg).join("retired-before.json"))?)?;
    ensure!(floor == generation);
    for namespace in ["memory_bank", "history"] {
        let content = format!("s13foreign retained cold-layout {namespace} bytes");
        ps.search.index_docs(PROJECT, &[IndexDoc {
            generation: 0, chunk_id: 1, path: RelPath::new("retained-layout-note.txt"), language: "text".into(),
            content_hash: ContentHash::compute(content.as_bytes()).0, content, namespace: namespace.into(),
            author: None, timestamp: None, start_line: 1, end_line: 1, doc_id: format!("layout-{namespace}"),
        }], &CancellationToken::new()).await?;
    }
    drop(ps);
    finish_owned_jobs(&app).await;
    drop(app);
    Ok((generation, rows))
}

async fn cold_retirement_refusal_and_repair(root: &Path, cfg: &Config, base_generation: u64,
    before_rows: &[Row]) -> Result<(u64, Vec<Row>)> {
    let app = reopen(cfg)?;
    let base = app.state.registry.capture_source_head(PROJECT)?;
    ensure!(base.generation() == base_generation);
    let ps = app.ensure_project_runtime(PROJECT).await.map_err(|error| anyhow::anyhow!("{error}"))?;
    ensure!(ps.search.pin_source_generation(PROJECT, 2).is_err(),
        "Cold missing retirement authority must not resurrect retained ancestry2 with collected source2 payload");
    ensure!(ps.search.pin_source_generation(PROJECT, base_generation).is_err(),
        "A managed cold read must not guess a zero retirement boundary");
    let native_count = ps.search.count_vectors(PROJECT).await?;
    let ancestry = std::fs::read(durable_scope(cfg).join("2.json"))?;
    let edited = source("cold_repaired", 6);
    std::fs::write(root.join("repo/source.rs"), &edited)?;
    ensure!(app.handle_update_project(serde_json::from_value(json!({
        "project_id": PROJECT, "wait": true, "max_commits": 1, "index_antipatterns": false
    }))?).await.is_err(), "Ordinary cold update must refuse missing retirement authority");
    ensure!(app.state.registry.source_head_is_current(&base)?
        && !durable_scope(cfg).join("retired-before.json").exists()
        && std::fs::read(durable_scope(cfg).join("2.json"))? == ancestry,
        "Refusal changed the head, reconstructed unknown authority, or mutated retained ancestry");
    for row in before_rows {
        let (path, _, text, first, last) = ps.search.get_doc_by_pk(&row.pk)?
            .ok_or_else(|| anyhow::anyhow!("Refusal lost existing complete physical source payload"))?;
        ensure!(path.as_str() == row.path && text == row.text && first == row.first && last == row.last);
    }
    ensure!(ps.search.count_vectors(PROJECT).await? == native_count, "Refusal changed native source payload count");
    // A damaged HEAD has no trustworthy logical anchor. Its physical floor
    // must not be used to recreate this missing boundary and retire a winner.
    app.state.registry.set_meta(PROJECT, "active_generation", "damaged-owned-head")?;
    ensure!(repair(&app).await.is_err(), "Damaged head plus missing logical floor was silently reconstructed from physical high-water");
    ensure!(!durable_scope(cfg).join("retired-before.json").exists());
    app.state.registry.set_meta(PROJECT, "active_generation", &base_generation.to_string())?;
    // The exact original valid head is the recovery anchor, not a guessed
    // physical maximum or the as-yet-unpublished candidate generation.
    repair(&app).await?;
    let (generation, rows) = current_rows(&app, &edited).await?;
    ensure!(generation > base_generation);
    ensure!(ps.search.pin_source_generation(PROJECT, 2).is_err(), "Full repair reopened a collected old logical root");
    drop(ps);
    finish_owned_jobs(&app).await;
    drop(app);
    Ok((generation, rows))
}

#[test]
fn cold_missing_retirement_boundary_cannot_resurrect_collected_ancestry_and_full_repair_recovers() -> Result<()> {
    let owned = tempfile::tempdir()?;
    let outcome = (|| -> Result<()> {
        let cfg = configuration(owned.path());
        let (base, before_rows) = {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            runtime.block_on(cold_retirement_seed(owned.path(), &cfg))?
        };
        // Every prior AppState/catalog/cache/lease/runtime is gone. Remove ONLY
        // the real floor, leaving the ancestor required by the current delta.
        std::fs::remove_file(durable_scope(&cfg).join("retired-before.json"))?;
        let (generation, repaired) = {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            runtime.block_on(cold_retirement_refusal_and_repair(owned.path(), &cfg, base, &before_rows))?
        };
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        runtime.block_on(async {
            let app = reopen(&cfg)?;
            ensure!(current_rows(&app, &source("cold_repaired", 6)).await? == (generation, repaired.clone()),
                "Cold reopen must reconstruct the parentless repair without resurrecting retired ancestry");
            std::fs::write(owned.path().join("repo/source.rs"), source("cold_next", 6))?;
            app.handle_update_project(serde_json::from_value(json!({
                "project_id": PROJECT, "wait": true, "max_commits": 1, "index_antipatterns": false
            }))?).await.map_err(|error| anyhow::anyhow!("{error}"))?;
            let (next, rows) = current_rows(&app, &source("cold_next", 6)).await?;
            ensure!(next > generation && rows.iter().filter(|row| row.path == "sibling.rs").collect::<Vec<_>>()
                == repaired.iter().filter(|row| row.path == "sibling.rs").collect::<Vec<_>>(),
                "The edit after cold recovery must retain the repaired complete sibling group");
            finish_owned_jobs(&app).await;
            drop(app);
            Ok(())
        })
    })();
    let cleanup = owned.close();
    match (outcome, cleanup) {
        (Err(error), Err(cleanup)) => Err(error.context(format!("Owned cold-boundary cleanup failed: {cleanup}"))),
        (Err(error), _) => Err(error), (Ok(()), cleanup) => cleanup.map_err(Into::into),
    }
}


async fn first_managed_publication(root: PathBuf, full: bool) -> Result<()> {
    let cfg = configuration(&root);
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo)?;
    let old_text = source("first_legacy", 6);
    std::fs::write(repo.join("source.rs"), &old_text)?;
    std::fs::write(repo.join("sibling.rs"), source("sibling", 1))?;
    {
        let git = git2::Repository::init(&repo)?;
        let mut index = git.index()?;
        index.add_path(Path::new("source.rs"))?; index.add_path(Path::new("sibling.rs"))?;
        let tree = git.find_tree(index.write_tree()?)?;
        let signature = git2::Signature::now("Owned first transition fixture", "fixture@example.invalid")?;
        git.commit(Some("HEAD"), &signature, &signature, "owned legacy baseline", &tree, &[])?;
    }
    let app = reopen(&cfg)?;
    app.state.registry.put_project(&ProjectRecord {
        project_id: PROJECT.into(), project_name: "First managed publication".into(),
        project_type: "general".into(), directory: repo.to_string_lossy().into_owned(),
        created_at_ms: 1, updated_at_ms: 1, reindex_required_since_ms: None,
    })?;
    let ps = app.ensure_project_runtime(PROJECT).await.map_err(|error| anyhow::anyhow!("{error}"))?;
    // Produce genuine legacy lexical/native data and its normal graph stats,
    // without any source catalog metadata or fabricated receipt markers.
    let stats = ps.search.index_files(PROJECT, "memory", 1, &repo,
        vec![repo.join("source.rs"), repo.join("sibling.rs")], 80,
        &CancellationToken::new(), |_, _| {}).await?;
    app.process_ingest_stats(PROJECT, 1, &stats).await.map_err(|error| anyhow::anyhow!("{error}"))?;
    ensure!(!ps.search.has_managed_source_snapshots(PROJECT)?);
    // The actual producer has just completed every admitted legacy file.
    let legacy = app.state.registry.capture_source_head(PROJECT)?;
    ensure!(app.state.registry.attest_legacy_source_head(&legacy)?);
    // Legacy GC must refuse before creating floor-only metadata which would
    // invalidate its own still-published legacy source.
    ensure!(app.handle_incremental_indexing_gc(serde_json::from_value(json!({
        "project_id": PROJECT, "compact_vectors": false
    }))?).await.is_err());
    ensure!(!ps.search.has_managed_source_snapshots(PROJECT)?);
    let held = ps.search.pin_source_generation(PROJECT, 1)?;
    let old_rows = source_rows(&ps.search, 1, &held, &old_text).await?;
    let base = app.capture_source_head(PROJECT, &ps.info.directory, &ps.info.project_type).await?;
    let next_text = source("first_managed", 6);
    std::fs::write(repo.join("source.rs"), &next_text)?;
    let entered = Arc::new(tokio::sync::Notify::new());
    let resume = Arc::new(tokio::sync::Notify::new());
    let registry_id = Arc::as_ptr(&app.state.registry) as usize;
    let (foreground_release, background_release) = if full {
        BACKGROUND_PUBLICATION_PAUSES.lock().unwrap_or_else(|error| error.into_inner()).insert(registry_id,
            PublicationPause { project: PROJECT.into(), after_commit: false, fail_cleanup: false,
                entered: entered.clone(), resume: resume.clone() });
        (None, Some(ResumePublication { registry_id, resume }))
    } else {
        FOREGROUND_PUBLICATION_PAUSES.lock().unwrap_or_else(|error| error.into_inner()).insert(registry_id,
            ForegroundPublicationPause { project: PROJECT.into(), after_commit: false, cancel: false,
                fail_step: None, entered: entered.clone(), resume: resume.clone() });
        (Some(ResumeForegroundPublication { registry_id, resume }), None)
    };
    let inspect_before_publication = async {
        let checked = tokio::time::timeout(Duration::from_secs(30), async {
            entered.notified().await;
            ensure!(app.state.registry.source_head_is_current(&base)?, "First managed preparation retired the published legacy head");
            ensure!(source_rows(&ps.search, 1, &held, &old_text).await? == old_rows,
                "First managed preparation invalidated its actual held legacy source view");
            let fresh = ps.search.pin_source_generation(PROJECT, 1)?;
            ensure!(source_rows(&ps.search, 1, &fresh, &old_text).await? == old_rows,
                "A new request cannot read the still-published complete legacy source before CAS");
            Ok::<_, anyhow::Error>(())
        }).await;
        // The real public RPC is released even after an observation failure.
        drop(foreground_release); drop(background_release);
        checked?
    };
    let publish = async {
        if full { repair(&app).await }
        else {
            app.handle_update_project(serde_json::from_value(json!({
                "project_id": PROJECT, "wait": true, "max_commits": 1, "index_antipatterns": false
            }))?).await.map_err(|error| anyhow::anyhow!("{error}"))?;
            Ok(())
        }
    };
    let (published, inspected) = tokio::join!(publish, inspect_before_publication);
    inspected?; published?;
    // Full repair intentionally returns after scheduling owned Git history
    // work. Let that actual admitted worker settle before requesting separate
    // maintenance, while the real old source reader remains held throughout.
    finish_owned_jobs(&app).await;
    let (generation, new_rows) = current_rows(&app, &next_text).await?;
    ensure!(generation == 2);
    if !full {
        ensure!(new_rows.iter().filter(|row| row.path == "sibling.rs").collect::<Vec<_>>()
            == old_rows.iter().filter(|row| row.path == "sibling.rs").collect::<Vec<_>>(),
            "The first ordinary managed update must inherit the complete legacy sibling group");
    }
    ensure!(source_rows(&ps.search, 1, &held, &old_text).await? == old_rows,
        "Successful first publication must retain the real old reader's complete physical groups");
    drop(held);
    ps.search.purge_old_generations(PROJECT, generation).await?;
    ensure!(ps.search.pin_source_generation(PROJECT, 1).is_err(), "Unheld retired legacy source reopened after collection");
    drop(ps);
    finish_owned_jobs(&app).await;
    drop(app);
    Ok(())
}

#[test]
fn first_managed_update_and_full_repair_keep_legacy_readers_valid_until_publication() -> Result<()> {
    for full in [false, true] {
        run(|root| Box::pin(first_managed_publication(root, full)))?;
    }
    Ok(())
}


async fn cold_scope_loss_refusal_and_repair(root: &Path, cfg: &Config, base_generation: u64,
    before_rows: &[Row], unproven_absent: bool) -> Result<(u64, Vec<Row>)> {
    let app = reopen(cfg)?;
    let base = app.state.registry.capture_source_head(PROJECT)?;
    ensure!(base.generation() == base_generation || (unproven_absent && base.generation_metadata_absent()));
    // Runtime creation remains available: only source Snapshot use is gated.
    let ps = app.ensure_project_runtime(PROJECT).await.map_err(|error| anyhow::anyhow!("{error}"))?;
    let foreign = foreign_rows(&ps.search).await?;
    let knowledge = app.handle_search_memory(serde_json::from_value(json!({
        "project_id":PROJECT,"query":"s13foreign","search_scope":"knowledge","semantic":false,"include_content":true
    }))?).await.map_err(|error| anyhow::anyhow!("{error}"))?;
    ensure!(knowledge.content.iter().filter_map(|item| item.as_text()).any(|item| item.text.contains("s13foreign")),
        "Unrelated knowledge was blocked by damaged source metadata");
    let native_count = ps.search.count_vectors(PROJECT).await?;
    let graph_before = serde_json::to_value(app.state.graph.list_file_node_metadata_with_generation(PROJECT)?)?;
    let sibling = before_rows.iter().find(|row| row.path == "sibling.rs")
        .ok_or_else(|| anyhow::anyhow!("Missing inherited sibling fixture"))?;
    ensure!(app.handle_search_memory(serde_json::from_value(json!({
        "project_id":PROJECT,"query":"sibling","semantic":false,"include_content":true
    }))?).await.is_err(), "Cold source search silently selected only the newest physical group");
    let chunk = app.handle_get_chunk(serde_json::from_value(json!({
        "project_id":PROJECT,"doc_id":sibling.doc_id,"namespace":"memory"
    }))?).await.err().ok_or_else(|| anyhow::anyhow!("Cold get_chunk accepted a partial source layout"))?;
    // Absent authority is refused by get_active_generation before the pin
    // wrapper; a readable numeric head reaches the strict layout pin instead.
    // Neither path may misreport an inherited physical chunk as not found.
    let explicit_metadata_refusal = if unproven_absent {
        chunk.message == "Source generation metadata is absent for an existing snapshot store without a legacy absence proof; explicit source recovery is required"
    } else {
        chunk.message.starts_with("source snapshot unavailable: ")
            && chunk.message.contains("Source snapshot layout is managed or unproven, but required metadata is missing; full repair required")
    };
    ensure!(explicit_metadata_refusal && !chunk.message.contains("not found"),
        "Expected an explicit source authority/layout refusal before document lookup, received: {}", chunk.message);
    // Exercise both the no-op and real-edit paths. Neither may certify or
    // publish a selection reconstructed only from physical generation3.
    ensure!(app.handle_update_project(serde_json::from_value(json!({
        "project_id":PROJECT,"wait":true,"max_commits":1,"index_antipatterns":false
    }))?).await.is_err(), "An unchanged update silently certified the partial source view");
    let next_text = source("scope_repaired", 6);
    std::fs::write(root.join("repo/source.rs"), &next_text)?;
    ensure!(app.handle_update_project(serde_json::from_value(json!({
        "project_id":PROJECT,"wait":true,"max_commits":1,"index_antipatterns":false
    }))?).await.is_err(), "A real edit silently published loss of the inherited sibling");
    ensure!(app.handle_incremental_indexing_gc(serde_json::from_value(json!({
        "project_id":PROJECT,"compact_vectors":false
    }))?).await.is_err(), "GC admitted an unproven partial source layout");
    ensure!(app.state.registry.source_head_is_current(&base)?
        && std::fs::read_dir(durable_scope(cfg))?.next().is_none()
        && serde_json::to_value(app.state.graph.list_file_node_metadata_with_generation(PROJECT)?)? == graph_before,
        "Refusal changed exact source authority, recreated false metadata, or mutated file evidence");
    for row in before_rows {
        let (path, _, content, first, last) = ps.search.get_doc_by_pk(&row.pk)?
            .ok_or_else(|| anyhow::anyhow!("Refusal collected an existing selected physical source chunk"))?;
        ensure!(path.as_str() == row.path && content == row.text && first == row.first && last == row.last);
    }
    ensure!(ps.search.count_vectors(PROJECT).await? == native_count && foreign_rows(&ps.search).await? == foreign);
    // Healthy numeric full repair bypasses the missing base view and publishes
    // a genuinely complete parentless checkpoint with the independent marker.
    repair(&app).await?;
    let (generation, rows) = current_rows(&app, &next_text).await?;
    ensure!(generation > base_generation
        && app.state.registry.get_meta(PROJECT, engram_core::registry::SOURCE_SNAPSHOTS_REQUIRED_KEY)?.as_deref() == Some("1")
        && foreign_rows(&ps.search).await? == foreign);
    drop(ps); finish_owned_jobs(&app).await; drop(app);
    Ok((generation, rows))
}

fn cold_total_scope_loss_case(pre_marker: bool, unproven_absent: bool) -> Result<()> {
    let owned = tempfile::tempdir()?;
    let outcome = (|| -> Result<()> {
        let cfg = configuration(owned.path());
        let (base, rows) = {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            runtime.block_on(cold_retirement_seed(owned.path(), &cfg))?
        };
        // All Registry/AppState/runtime/cache/reader owners are gone. Remove
        // only this owned scope's metadata, retaining real inherited payload.
        for entry in std::fs::read_dir(durable_scope(&cfg))? { std::fs::remove_file(entry?.path())?; }
        if pre_marker {
            // Represent the old numeric-head format without pretending to know
            // whether its now-lost scope was managed or legacy.
            let database = redb::Database::open(cfg.data_dir.join("registry/registry.redb"))?;
            let tx = database.begin_write()?;
            tx.open_table(redb::TableDefinition::<&str, &[u8]>::new("meta"))?
                .remove(format!("{PROJECT}\0{}", engram_core::registry::SOURCE_SNAPSHOTS_REQUIRED_KEY).as_str())?;
            if unproven_absent {
                let mut table = tx.open_table(redb::TableDefinition::<&str, &[u8]>::new("meta"))?;
                ensure!(table.get(format!("{PROJECT}\0source_legacy_absence_attested_v1").as_str())?.is_none());
                table.remove(format!("{PROJECT}\0active_generation").as_str())?;
                table.remove(format!("{PROJECT}\0source_head_required_v1").as_str())?;
            }
            tx.commit()?; drop(database);
        }
        let (generation, repaired) = {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            runtime.block_on(cold_scope_loss_refusal_and_repair(owned.path(), &cfg, base, &rows, unproven_absent))?
        };
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        runtime.block_on(async {
            let app = reopen(&cfg)?;
            ensure!(current_rows(&app, &source("scope_repaired", 6)).await? == (generation, repaired.clone()),
                "Cold reopen lost the repaired inherited-source completeness");
            std::fs::write(owned.path().join("repo/source.rs"), source("scope_next", 6))?;
            app.handle_update_project(serde_json::from_value(json!({
                "project_id":PROJECT,"wait":true,"max_commits":1,"index_antipatterns":false
            }))?).await.map_err(|error| anyhow::anyhow!("{error}"))?;
            let (next, next_rows) = current_rows(&app, &source("scope_next", 6)).await?;
            ensure!(next > generation && next_rows.iter().filter(|row| row.path == "sibling.rs").collect::<Vec<_>>()
                == repaired.iter().filter(|row| row.path == "sibling.rs").collect::<Vec<_>>(),
                "The next ordinary edit lost the repaired unchanged sibling group");
            let ps = app.ensure_project_runtime(PROJECT).await.map_err(|error| anyhow::anyhow!("{error}"))?;
            ensure!(foreign_rows(&ps.search).await?.len() == 2); drop(ps);
            finish_owned_jobs(&app).await; drop(app);
            Ok(())
        })
    })();
    let cleanup = owned.close();
    match (outcome, cleanup) {
        (Err(error), Err(cleanup)) => Err(error.context(format!("Owned layout fixture cleanup failed: {cleanup}"))),
        (Err(error), _) => Err(error), (Ok(()), cleanup) => cleanup.map_err(Into::into),
    }
}

#[test]
fn cold_total_scope_loss_refuses_partial_public_reads_updates_and_gc_then_repair_survives_restart() -> Result<()> {
    cold_total_scope_loss_case(false, false)
}

#[test]
fn unproven_old_numeric_layout_never_adopts_legacy_from_empty_scope_and_explicit_repair_recovers() -> Result<()> {
    cold_total_scope_loss_case(true, false)
}

#[test]
fn unproven_absent_layout_uses_explicit_fresh_recovery_above_real_payload_and_survives_restart() -> Result<()> {
    cold_total_scope_loss_case(true, true)
}
