//! A scheduled sweep may not collect a registered but unproven legacy source.
use super::*;
use anyhow::{Result, ensure};
use engram_core::{Config, ContentHash, ProjectRecord, RelPath};
use engram_graph::{Edge, EdgeKind, Node};
use engram_index::{HybridSearchEngine, IndexDoc};
#[cfg(feature = "vector")]
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

const PID: &str = "gc-unproven-legacy";

fn source(generation: u64, path: &str, content: &str) -> IndexDoc {
    let hash = ContentHash::compute(content.as_bytes());
    IndexDoc {
        generation, chunk_id: generation, path: RelPath::new(path),
        language: "rust".into(), content: content.into(), namespace: "memory".into(),
        author: None, timestamp: None, start_line: 1, end_line: 1,
        doc_id: format!("{path}-{generation}"), content_hash: hash.0,
    }
}

#[cfg(feature = "vector")]
fn native_files(root: &std::path::Path) -> Result<BTreeMap<String, String>> {
    fn walk(root: &std::path::Path, current: &std::path::Path, out: &mut BTreeMap<String, String>) -> Result<()> {
        for entry in std::fs::read_dir(current)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            ensure!(!kind.is_symlink(), "Owned native fixture unexpectedly contains a link");
            if kind.is_dir() { walk(root, &entry.path(), out)?; }
            else {
                ensure!(kind.is_file() && entry.metadata()?.len() <= 4 * 1024 * 1024);
                let raw = std::fs::read(entry.path())?;
                out.insert(entry.path().strip_prefix(root)?.to_string_lossy().into(), blake3::hash(&raw).to_hex().to_string());
                ensure!(out.len() <= 256, "Owned canonical table exceeded its fixture bound");
            }
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    walk(root, root, &mut files)?;
    Ok(files)
}

async fn case(root: &std::path::Path, cached: bool) -> Result<()> {
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo)?;
    let docs = vec![
        source(1, "source.rs", "pub fn previous_complete_source() {}\n"),
        source(5, "source.rs", "pub fn current_complete_source() {}\n"),
        source(5, "sibling.rs", "pub fn retained_complete_sibling() {}\n"),
    ];
    std::fs::write(repo.join("source.rs"), &docs[1].content)?;
    std::fs::write(repo.join("sibling.rs"), &docs[2].content)?;
    let cfg = Config {
        data_dir: root.join("data"), allowed_roots: vec![repo.clone()],
        embedding_backend: if cfg!(feature = "vector") { "local" } else { "fts_only" }.into(),
        llm_backend: "none".into(), ..Default::default()
    };
    let (state, _) = AppState::new(cfg.clone())?;
    state.registry.put_project(&ProjectRecord {
        project_id: PID.into(), project_name: PID.into(), directory: repo.to_string_lossy().into(),
        project_type: "rust".into(), created_at_ms: 1, updated_at_ms: 1, reindex_required_since_ms: None,
    })?;
    let ps = crate::services::project_service::ensure_project_runtime(&state, PID).await?;
    // Write actual flat-format lexical documents without source catalog/route
    // creation. This is an old producer fixture, not a fabricated managed view.
    {
        // Seed flat lexical rows through an FTS-only writer, without creating
        // vector identity. The runtime under test still uses the native config.
        let mut lexical_cfg = cfg.clone();
        lexical_cfg.embedding_backend = "fts_only".into();
        let directory = cfg.data_dir.join("projects").join(PID).join("tantivy");
        let native = cfg.data_dir.join("projects").join(PID).join("lancedb");
        let lexical = HybridSearchEngine::new_with_budget(
            directory, native, &lexical_cfg, Some(state.memory_budget.clone()),
        ).await?;
        let mut writer = lexical.create_bulk_writer()?;
        HybridSearchEngine::write_docs_to_writer(&lexical.fields(), &mut writer, PID, &docs)?;
        writer.finish()?;
    }
    ensure!(!ps.search.has_managed_source_snapshots(PID)?);
    #[cfg(feature = "vector")]
    let (canonical, before_native) = {
        let directory = cfg.data_dir.join("projects").join(PID).join("lancedb");
        let connection = engram_index::vector::connect(&directory).await?;
        let name = format!("project_{}", PID.replace('-', "_"));
        let (table, _) = engram_index::vector::open_or_create_table(&connection, &name, 384).await?;
        let batch = engram_index::vector::create_record_batch_with_gens(
            PID, "memory",
            &docs.iter().map(|doc| doc.generation).collect::<Vec<_>>(),
            &docs.iter().map(|doc| engram_core::build_pk(PID, "memory", doc.generation, &doc.doc_id)).collect::<Vec<_>>(),
            &docs.iter().map(|doc| doc.doc_id.clone()).collect::<Vec<_>>(),
            &docs.iter().map(|doc| doc.content_hash.clone()).collect::<Vec<_>>(),
            &docs.iter().map(|doc| doc.chunk_id).collect::<Vec<_>>(),
            &docs.iter().map(|doc| doc.path.as_str().to_owned()).collect::<Vec<_>>(),
            &docs.iter().map(|doc| doc.language.clone()).collect::<Vec<_>>(),
            &docs.iter().map(|doc| doc.author.clone()).collect::<Vec<_>>(),
            &docs.iter().map(|doc| doc.timestamp).collect::<Vec<_>>(),
            &vec![vec![0.1; 384]; docs.len()], 384)?;
        engram_index::vector::upsert_vectors(&table, vec![batch]).await?;
        ensure!(table.count_rows(None).await? == docs.len());
        drop(table); drop(connection);
        let canonical = directory.join(format!("{name}.lance"));
        let before = native_files(&canonical)?;
        (canonical, before)
    };
    // A real old positive numeric HEAD has no new absence/managed authority.
    state.registry.set_meta(PID, "active_generation", "5")?;
    state.registry.set_meta(PID, "last_full_index_generation", "5")?;
    state.registry.set_meta(PID, "purge_pending", "5")?;
    let nodes = [1, 5].map(|generation| Node {
        node_id: format!("source-{generation}"), node_type: "function".into(),
        name: format!("source-{generation}"), namespace: "memory".into(), language: "rust".into(),
        file_path: RelPath::new("source.rs"), start_line: 1, end_line: 1, generation,
        metadata: Some(serde_json::json!({"owned_fixture":"preserve until explicit repair"})),
    });
    let edge = Edge {
        source_id: "source-1".into(), target_id: "source-5".into(),
        namespace: "memory".into(), language: "rust".into(), edge_kind: EdgeKind::Calls,
        weight: 7, generation: 1, metadata: None, updated_at_ms: 17,
    };
    state.graph.upsert_nodes(PID, &nodes)?;
    state.graph.upsert_edges(PID, &[edge.clone()])?;
    let before_nodes = serde_json::to_value(&nodes)?;
    let before_edge = serde_json::to_value(&edge)?;
    drop(ps);
    if !cached { state.projects.remove(PID); }
    ensure!(state.get_project_cached(PID).is_some() == cached);
    let shutdown = CancellationToken::new();
    let mut task = tokio::spawn(run_gc_scheduler_with_delay(state.clone(), shutdown.clone(), Duration::from_secs(3600)));
    let check: Result<()> = async {
        state.gc_nudge.notify_one();
        tokio::time::timeout(Duration::from_secs(15), async {
            while state.gc_sweeps_completed.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await?;
        let actual_nodes = nodes.iter().map(|node| state.graph.get_node(PID, &node.node_id))
            .collect::<Result<Vec<_>>>()?;
        ensure!(actual_nodes.iter().all(Option::is_some),
            "Scheduled GC collected legacy graph before admitting source; cached={cached}");
        let actual_nodes = actual_nodes.into_iter().flatten().collect::<Vec<_>>();
        ensure!(serde_json::to_value(actual_nodes)? == before_nodes);
        let edges = state.graph.get_edges_by_endpoints(PID, &[("source-1".into(), EdgeKind::Calls, "source-5".into())])?;
        ensure!(edges.len() == 1 && serde_json::to_value(&edges[0])? == before_edge,
            "Scheduled GC changed unproven legacy edge");
        ensure!(state.get_project_cached(PID).is_some() == cached,
            "Refused scheduled GC changed runtime cache admission");
        let reopened = crate::services::project_service::ensure_project_runtime(&state, PID).await?;
        ensure!(reopened.search.count_docs(PID)? == docs.len(), "Scheduled GC collected legacy source rows");
        for doc in &docs {
            let pk = engram_core::build_pk(PID, "memory", doc.generation, &doc.doc_id);
            let (path, _, body, first, last) = reopened.search.get_doc_by_pk(&pk)?
                .ok_or_else(|| anyhow::anyhow!("Missing retained legacy physical source {pk}"))?;
            ensure!(path == doc.path && body == doc.content && first == 1 && last == 1);
        }
        ensure!(!reopened.search.has_managed_source_snapshots(PID)?);
        ensure!(state.registry.get_meta(PID, "active_generation")?.as_deref() == Some("5"));
        ensure!(state.registry.get_meta(PID, "purge_pending")?.as_deref() == Some("5"),
            "Skipped GC must not claim a pending purge was settled");
        ensure!(state.registry.get_meta(PID, engram_core::registry::SOURCE_SNAPSHOTS_REQUIRED_KEY)?.is_none());
        ensure!(state.registry.list_jobs(Some(PID))?.is_empty());
        #[cfg(feature = "vector")]
        ensure!(native_files(&canonical)? == before_native,
            "Scheduled GC changed unknown old canonical native table bytes or membership");
        Ok(())
    }.await;
    shutdown.cancel();
    match tokio::time::timeout(Duration::from_secs(5), &mut task).await {
        Ok(joined) => { joined?; }
        Err(_) => {
            task.abort();
            let _ = task.await;
            anyhow::bail!("Scheduled GC did not stop; owning runtime must finish before fixture teardown");
        }
    }
    check?;
    drop(state);
    Ok(())
}

#[test]
fn nudged_gc_preserves_unproven_legacy_graph_source_and_canonical_vectors() -> Result<()> {
    for cached in [false, true] {
        let owned = tempfile::tempdir()?;
        // Runtime is declared after TempDir and explicitly closed before
        // TempDir.close, including ordinary Result failures.
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
        let result = runtime.block_on(case(owned.path(), cached));
        drop(runtime);
        owned.close()?;
        result?;
    }
    Ok(())
}
