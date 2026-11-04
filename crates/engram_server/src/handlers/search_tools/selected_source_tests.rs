//! A real paused search owns the only old source lease during publication/GC.
use super::*;
use anyhow::{Result, ensure};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use tokio::sync::Notify;

#[derive(Default)]
struct Pause { entered: Notify, release: Notify }
static PAUSES: LazyLock<Mutex<HashMap<String, Arc<Pause>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) async fn after_selection(pid: &str) {
    let pause = PAUSES.lock().expect("pause registry").get(pid).cloned();
    if let Some(pause) = pause {
        pause.entered.notify_one();
        pause.release.notified().await;
    }
}

struct Registered { pid: String, pause: Arc<Pause> }
impl Drop for Registered {
    fn drop(&mut self) {
        PAUSES.lock().expect("pause registry").remove(&self.pid);
        self.pause.release.notify_one();
    }
}

/// Each real RPC owns the only old source lease. Graph nodes stay live; these
/// cases test the source selection used by the request, not graph isolation.
fn cognitive_request_source_lifetime(kind: &'static str) -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let result = runtime.block_on(async {
    let mut requests = tokio::task::JoinSet::new();
    let result = async {
        let root = temporary.path().join("repo");
        std::fs::create_dir_all(root.join("src"))?;
        let paths = [engram_core::RelPath::new("src/target.rs"), engram_core::RelPath::new("src/companion.rs")];
        let source = |generation: u64, id: &str, path: &engram_core::RelPath, marker: &str| {
            let content = format!("pub fn {marker}() {{}}\n");
            engram_index::IndexDoc { generation, chunk_id: generation, path: path.clone(),
                language: "rust".into(), content_hash: blake3::hash(content.as_bytes()).to_hex().to_string(),
                content, namespace: "memory".into(), author: None, timestamp: None,
                start_line: 1, end_line: 1, doc_id: id.into() }
        };
        let old = [source(1, "old-target", &paths[0], "requestoldmarker"),
            source(1, "old-companion", &paths[1], "requestsecondmarker")];
        for doc in &old { std::fs::write(root.join(doc.path.as_str()), &doc.content)?; }
        let (state, _) = crate::state::AppState::new(engram_core::Config {
            data_dir: temporary.path().join("data"), allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(), llm_backend: "none".into(), ..Default::default()
        })?;
        let pid = format!("request-source-{}", uuid::Uuid::new_v4());
        state.registry.register_source_project(&engram_core::ProjectRecord {
            project_id: pid.clone(), project_name: "Owned request source".into(), project_type: "rust".into(),
            directory: root.to_string_lossy().into(), created_at_ms: 1, updated_at_ms: 1,
            reindex_required_since_ms: None,
        })?;
        let engram = Engram::new(state);
        let ps = engram.ensure_project_runtime(&pid).await?;
        let cancel = tokio_util::sync::CancellationToken::new();
        let initial = engram.state.registry.capture_source_head(&pid)?;
        ps.search.prepare_full_source_snapshot(&pid, 0, 1, &paths, &cancel).await?;
        ps.search.index_docs(&pid, &old, &cancel).await?;
        ps.search.seal_source_snapshot(&pid, 1)?;
        ensure!(engram.state.registry.publish_source_head(&initial, 1)?);
        let mut nodes = paths.iter().map(|path| engram_graph::Node {
            node_id: format!("file:{}", path.as_str()), node_type: "file".into(),
            name: path.as_str().into(), namespace: "memory".into(), language: "rust".into(),
            file_path: path.clone(), start_line: 1, end_line: 1, generation: 1, metadata: None,
        }).collect::<Vec<_>>();
        for (id, node_type) in [("control:old-request-control", "control"), ("handler:old-request-handler", "function")] {
            nodes.push(engram_graph::Node { node_id: id.into(), node_type: node_type.into(), name: id.into(),
                namespace: "memory".into(), language: "rust".into(), file_path: paths[0].clone(),
                start_line: 1, end_line: 1, generation: 1, metadata: None });
        }
        engram.state.graph.upsert_nodes(&pid, &nodes)?;
        engram.state.graph.upsert_edges(&pid, &[engram_graph::Edge {
            source_id: "control:old-request-control".into(), target_id: "handler:old-request-handler".into(),
            namespace: "memory".into(), language: "rust".into(), edge_kind: EdgeKind::Calls,
            weight: 1, generation: 1, metadata: None, updated_at_ms: 1,
        }])?;
        let request = serde_json::json!({"project_id":pid,"query":"requestoldmarker","output_json":true});
        let benchmark = serde_json::json!({"project_id":pid,"output_json":true,"custom_queries":[
            {"query":"requestoldmarker","relevant_paths":["src/target.rs"]},
            {"query":"requestsecondmarker","relevant_paths":["src/companion.rs"]}
        ]});
        let pause = Arc::new(Pause::default());
        PAUSES.lock().expect("pause registry").insert(pid.clone(), pause.clone());
        let registered = Registered { pid: pid.clone(), pause: pause.clone() };
        let server = Engram::new(engram.state.clone());
        let request_pid = pid.clone();
        let centrality_request = request.clone();
        let benchmark_request = benchmark.clone();
        requests.spawn(async move {
            match kind {
                "trace" => server.handle_trace_ui_action(serde_json::from_value(serde_json::json!({
                    "project_id":request_pid,"query":"requestoldmarker","max_depth":3,"max_paths":3,
                })).unwrap()).await,
                "centrality" => server.handle_graph_centrality_rerank(serde_json::from_value(centrality_request).unwrap()).await,
                "benchmark" => server.handle_benchmark_retrieval(serde_json::from_value(benchmark_request).unwrap()).await,
                "health" => server.handle_project_health(crate::models::ProjectIdRequest { project_id: request_pid }).await,
                "freshness" => server.handle_get_index_freshness(crate::models::GetIndexFreshnessRequest {
                    project_id: request_pid, check_disk: false, expected_git_commit: None,
                }).await,
                _ => unreachable!(),
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(15), pause.entered.notified()).await?;
        let old_pk = format!("{pid}:memory:1:old-target");
        let base = engram.state.registry.capture_source_head(&pid)?;
        let new = [source(2, "new-target", &paths[0], "requestnewmarker"),
            source(2, "new-target-extra", &paths[0], "requestadditionalmarker"),
            source(2, "new-companion", &paths[1], "requestnewsecondmarker")];
        for doc in &new { std::fs::write(root.join(doc.path.as_str()), &doc.content)?; }
        ps.search.prepare_full_source_snapshot(&pid, 1, 2, &paths, &cancel).await?;
        ps.search.index_docs(&pid, &new, &cancel).await?;
        ps.search.seal_source_snapshot(&pid, 2)?;
        if kind == "freshness" {
            let future_files = paths.iter().map(|path| engram_graph::Node {
                node_id: format!("file:{path}"), node_type: "file".into(), name: path.as_str().into(),
                namespace: "memory".into(), language: "rust".into(), file_path: path.clone(),
                start_line: 1, end_line: 1, generation: 2, metadata: None,
            }).collect::<Vec<_>>();
            engram.state.graph.upsert_nodes(&pid, &future_files)?;
        }
        ensure!(engram.state.registry.publish_source_head(&base, 2)?);
        ps.search.purge_old_generations(&pid, 2).await?;
        ensure!(ps.search.get_doc_by_pk(&old_pk)?.is_some(), "{kind} RPC lost its captured source during publication/GC");
        drop(registered);
        let response = tokio::time::timeout(std::time::Duration::from_secs(15), requests.join_next()).await?
            .ok_or_else(|| anyhow::anyhow!("Request task missing"))???;
        let body = &response.content[0].as_text().ok_or_else(|| anyhow::anyhow!("Request response missing"))?.text;
        match kind {
            "trace" => ensure!(body.contains("control:old-request-control") && body.contains("handler:old-request-handler"),
                "Trace lost its selected source results: {body}"),
            "centrality" => {
                let rows: serde_json::Value = serde_json::from_str(body)?;
                ensure!(rows.as_array().is_some_and(|rows| rows.iter().any(|row| row["file_path"] == "src/target.rs")),
                    "Centrality search lost its selected source results: {body}");
            }
            "benchmark" => {
                let result: serde_json::Value = serde_json::from_str(body)?;
                ensure!(result["query_count"] == 2 && result["recall_at_10"] == 1.0,
                    "Benchmark lost source across its query loop: {body}");
                for (n, path) in ["src/target.rs", "src/companion.rs"].iter().enumerate() {
                    ensure!(result["per_query_results"][n]["actual_top_paths"] == serde_json::json!([path]));
                }
            }
            "health" => ensure!(body.contains("active_generation: 1\n")
                && body.contains("generation completeness: generation 1") && body.contains("tantivy: 2")
                && body.contains("chunks 2 / vector rows 0"), "Health mixed its selected generation/counts: {body}"),
            "freshness" => ensure!(body.contains("active_generation: 1\n")
                && body.contains("disk_check: not_run (check_disk=false)")
                && body.contains("generation_complete: unknown")
                && body.contains("file records newer than selected source generation 1"),
                "Freshness lost its selected generation: {body}"),
            _ => unreachable!(),
        }
        ps.search.purge_old_generations(&pid, 2).await?;
        ensure!(ps.search.get_doc_by_pk(&old_pk)?.is_none(), "Completed {kind} RPC still retains old source");
        Ok(())
    }.await;
    let drained = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        while requests.join_next().await.is_some() {}
    }).await;
    if drained.is_err() {
        requests.abort_all();
        while requests.join_next().await.is_some() {}
    }
    drained?;
    result
    });
    // Runtime shutdown joins native blocking work even when a request's async
    // future was cancelled. Database handles must close before fixture removal.
    drop(runtime);
    match (result, temporary.close()) {
        (Err(error), Err(cleanup)) => Err(error.context(format!("Fixture cleanup also failed: {cleanup}"))),
        (Err(error), _) => Err(error),
        (Ok(()), cleanup) => cleanup.map_err(Into::into),
    }
}

#[test]
fn cognitive_request_source_lifetimes_trace_keeps_its_selected_search() -> Result<()> {
    cognitive_request_source_lifetime("trace")
}

#[test]
fn cognitive_request_source_lifetimes_centrality_keeps_its_selected_search() -> Result<()> {
    cognitive_request_source_lifetime("centrality")
}

#[test]
fn cognitive_request_source_lifetimes_benchmark_keeps_source_across_all_queries() -> Result<()> {
    cognitive_request_source_lifetime("benchmark")
}

#[test]
fn cognitive_request_source_lifetimes_health_keeps_its_selected_completeness() -> Result<()> {
    cognitive_request_source_lifetime("health")
}

#[test]
fn cognitive_request_source_lifetimes_freshness_keeps_its_selected_source() -> Result<()> {
    cognitive_request_source_lifetime("freshness")
}

fn query(pid: &str) -> Result<SearchMemoryRequest> {
    Ok(serde_json::from_value(serde_json::json!({"project_id":pid,
        "query":"requestviewmarker","max_results":3,"include_content":true,
        "max_content_chars_per_result":4000,"semantic":false}))?)
}

/// Both graph handlers must keep their captured source readable across later
/// awaits. The graph itself remains live; only source identity/body is asserted.
fn graph_request_source_lifetimes(resolve_doc: bool) -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let result = runtime.block_on(async {
    let mut requests = tokio::task::JoinSet::new();
    let result = async {
        let root = temporary.path().join("repo");
        std::fs::create_dir_all(root.join("src"))?;
        let old_source = format!("pub fn graphrequestviewmarker() -> &'static str {{ \"old_body_{}old_tail\" }}\n", "old_padding_".repeat(90));
        let new_source = format!("\n\npub fn graphrequestviewmarker() -> &'static str {{ \"new_body_{}new_tail\" }}\n", "new_padding_".repeat(90));
        std::fs::write(root.join("src/target.rs"), &old_source)?;
        std::fs::write(root.join("src/companion.rs"), "pub fn stablecompanion() {}\n")?;
        {
            let repo = git2::Repository::init(&root)?;
            let mut index = repo.index()?;
            index.add_all(["src"], git2::IndexAddOption::DEFAULT, None)?;
            let tree = repo.find_tree(index.write_tree()?)?;
            let signature = git2::Signature::now("Fixture", "fixture@example.invalid")?;
            repo.commit(Some("HEAD"), &signature, &signature, "owned graph request baseline", &tree, &[])?;
        }
        let (state, _) = crate::state::AppState::new(engram_core::Config {
            data_dir: temporary.path().join("data"), allowed_roots: vec![root.clone()],
            embedding_backend: if cfg!(feature = "vector") { "local" } else { "fts_only" }.into(),
            llm_backend: "none".into(),
            ..Default::default()
        })?;
        let engram = Engram::new(state);
        let indexed = engram.handle_index_project(serde_json::from_value(serde_json::json!({
            "directory":root,"project_name":"Graph request source lifetime","project_type":"rust","wait":true
        }))?).await?;
        let text = &indexed.content[0].as_text().ok_or_else(|| anyhow::anyhow!("Missing index response"))?.text;
        let pid = text.split("project_id: ").nth(1).and_then(|s| s.lines().next())
            .ok_or_else(|| anyhow::anyhow!("Missing project ID"))?.trim().to_string();
        let ps = engram.state.get_project_cached(&pid).ok_or_else(|| anyhow::anyhow!("Runtime missing"))?;
        let old_generation: u64 = engram.state.registry.get_meta(&pid, "active_generation")?.ok_or_else(|| anyhow::anyhow!("Missing head"))?.parse()?;
        let mut search_query = HybridQuery { project_id: pid.clone(), namespace: "memory".into(), generation: old_generation,
            text: "graphrequestviewmarker".into(), top_k: 3, fts_mode: "loose".into(), include_path_prefixes: None,
            exclude_path_prefixes: None, include_path_suffixes: None, language_filters: None,
            author_filter: None, date_after: None, date_before: None, use_mmr: false };
        let old_hits = ps.search.lexical_search(&search_query)?;
        ensure!(old_hits.len() == 1, "Expected one owned source document");
        let old_hit = old_hits[0].clone();
        let old_content = ps.search.get_doc_by_pk(&old_hit.pk)?.ok_or_else(|| anyhow::anyhow!("Old body missing"))?.2;
        let old_body = old_content.trim_end();
        ensure!(old_body.len() > 500 && old_body.len() < 4000 && old_body.contains("old_tail"));
        let graph_request = || -> Result<crate::models::GraphSearchRequest> {
            Ok(serde_json::from_value(serde_json::json!({"project_id":pid,"query":"graphrequestviewmarker",
                "include_content":true,"max_content_chars":4000,"max_results":10}))?)
        };
        let resolve_request = |id: &str| -> Result<crate::models::ResolveIdRequest> {
            Ok(serde_json::from_value(serde_json::json!({"project_id":pid,"id":id}))?)
        };
        let pause = Arc::new(Pause::default());
        PAUSES.lock().expect("pause registry").insert(pid.clone(), pause.clone());
        let registered = Registered { pid: pid.clone(), pause: pause.clone() };
        let request_engram = Engram::new(engram.state.clone());
        if resolve_doc {
            let request = resolve_request(&old_hit.doc_id)?;
            requests.spawn(async move { request_engram.handle_resolve_id(request).await });
        } else {
            let request = graph_request()?;
            requests.spawn(async move { request_engram.handle_graph_search(request).await });
        }
        tokio::time::timeout(std::time::Duration::from_secs(15), pause.entered.notified()).await?;

        std::fs::write(root.join("src/target.rs"), &new_source)?;
        let updated = engram.handle_update_project(serde_json::from_value(serde_json::json!({
            "project_id":pid,"wait":true,"max_commits":1,"index_antipatterns":false
        }))?).await?;
        ensure!(updated.is_error != Some(true));
        let new_generation: u64 = engram.state.registry.get_meta(&pid, "active_generation")?.ok_or_else(|| anyhow::anyhow!("Missing new head"))?.parse()?;
        ensure!(new_generation > old_generation);
        ps.search.purge_old_generations(&pid, new_generation).await?;
        ensure!(ps.search.get_doc_by_pk(&old_hit.pk)?.is_some(), "GC removed the real paused graph request's captured source");
        drop(registered);
        let response = tokio::time::timeout(std::time::Duration::from_secs(15), requests.join_next()).await?
            .ok_or_else(|| anyhow::anyhow!("Graph request task missing"))???;
        let body = &response.content[0].as_text().ok_or_else(|| anyhow::anyhow!("Missing graph response"))?.text;
        if resolve_doc {
            ensure!(body.contains("resolved: search doc (chunk)") && body.contains(&format!("doc_id: {}", old_hit.doc_id))
                && body.contains(&format!("file: {} (lines {}-{})", old_hit.path, old_hit.start_line, old_hit.end_line)),
                "Paused doc fallback lost its captured document identity/coordinates: {body}");
        } else {
            let content = body.split(&format!("  L{}: ", old_hit.start_line)).nth(1)
                .ok_or_else(|| anyhow::anyhow!("Selected source preview missing: {body}"))?;
            ensure!(body.contains(&format!("file:{}", old_hit.path)) && content.starts_with(old_body)
                && !body.contains("new_tail"), "Graph search substituted or truncated its selected body");
            ensure!(blake3::hash(&content.as_bytes()[..old_body.len()]) == blake3::hash(old_body.as_bytes()));
        }

        search_query.generation = new_generation;
        let new_hits = ps.search.lexical_search(&search_query)?;
        ensure!(new_hits.len() == 1 && new_hits[0].doc_id != old_hit.doc_id);
        if resolve_doc {
            // Even while its physical row still exists, an old ID must not
            // make a later request select an obsolete source view.
            let stale = engram.handle_resolve_id(resolve_request(&old_hit.doc_id)?).await?;
            let stale_body = &stale.content[0].as_text().ok_or_else(|| anyhow::anyhow!("Missing old-ID response"))?.text;
            ensure!(stale_body.contains("resolved: NOT FOUND"), "A stale ID forced an old source view");
            let fresh = engram.handle_resolve_id(resolve_request(&new_hits[0].doc_id)?).await?;
            let fresh_body = &fresh.content[0].as_text().ok_or_else(|| anyhow::anyhow!("Missing fresh resolve response"))?.text;
            ensure!(fresh_body.contains(&format!("doc_id: {}", new_hits[0].doc_id))
                && fresh_body.contains(&format!("file: {} (lines {}-{})", new_hits[0].path, new_hits[0].start_line, new_hits[0].end_line)));
        } else {
            let fresh = engram.handle_graph_search(graph_request()?).await?;
            let fresh_body = &fresh.content[0].as_text().ok_or_else(|| anyhow::anyhow!("Missing fresh graph response"))?.text;
            ensure!(fresh_body.contains("new_tail") && !fresh_body.contains("old_tail"));
        }
        ps.search.purge_old_generations(&pid, new_generation).await?;
        ensure!(ps.search.get_doc_by_pk(&old_hit.pk)?.is_none(), "Completed graph request still retains its obsolete source row");
        Ok(())
    }.await;
    // Registered's Drop releases the pause on every error path. Drain before
    // closing TempDir so no request task can retain runtime/database handles.
    let drained = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        while requests.join_next().await.is_some() {}
    }).await;
    if drained.is_err() {
        requests.abort_all();
        while requests.join_next().await.is_some() {}
    }
    drained?;
    result
    });
    // Initial indexing can enqueue a blocking PageRank warmup. Shutting down
    // this fixture's runtime waits for it, including during early-error cleanup.
    drop(runtime);
    match (result, temporary.close()) {
        (Err(error), Err(cleanup)) => Err(error.context(format!("Fixture cleanup also failed: {cleanup}"))),
        (Err(error), _) => Err(error),
        (Ok(()), cleanup) => cleanup.map_err(Into::into),
    }
}

#[test]
fn graph_request_source_lifetimes_search_keeps_its_complete_selected_body() -> Result<()> {
    graph_request_source_lifetimes(false)
}

#[test]
fn graph_request_source_lifetimes_resolve_doc_keeps_its_selected_identity() -> Result<()> {
    graph_request_source_lifetimes(true)
}

#[tokio::test]
async fn a_search_recovers_its_selected_old_body_during_publication_and_gc_then_releases_it() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let mut requests = tokio::task::JoinSet::new();
    let result = async {
        let root = temporary.path().join("repo");
        std::fs::create_dir_all(root.join("src"))?;
        let old_source = format!("pub fn requestviewmarker() -> &'static str {{ \"old_body_{}old_tail\" }}\n", "old_padding_".repeat(90));
        let new_source = format!("pub fn requestviewmarker() -> &'static str {{ \"new_body_{}new_tail\" }}\n", "new_padding_".repeat(90));
        std::fs::write(root.join("src/target.rs"), &old_source)?;
        std::fs::write(root.join("src/companion.rs"), "pub fn stablecompanion() {}\n")?;
        {
            let repo = git2::Repository::init(&root)?;
            let mut index = repo.index()?;
            index.add_all(["src"], git2::IndexAddOption::DEFAULT, None)?;
            let tree = repo.find_tree(index.write_tree()?)?;
            let signature = git2::Signature::now("Fixture", "fixture@example.invalid")?;
            repo.commit(Some("HEAD"), &signature, &signature, "baseline", &tree, &[])?;
        }
        let (state, _) = crate::state::AppState::new(engram_core::Config {
            data_dir: temporary.path().join("data"), allowed_roots: vec![root.clone()],
            embedding_backend: if cfg!(feature = "vector") { "local" } else { "fts_only" }.into(),
            ..Default::default()
        })?;
        let engram = Engram::new(state);
        let indexed = engram.handle_index_project(serde_json::from_value(serde_json::json!({
            "directory":root,"project_name":"Request-held source recovery","project_type":"rust","wait":true
        }))?).await?;
        let index_text = &indexed.content[0].as_text().ok_or_else(|| anyhow::anyhow!("Missing index response"))?.text;
        let pid = index_text.split("project_id: ").nth(1).and_then(|s| s.lines().next())
            .ok_or_else(|| anyhow::anyhow!("Missing project ID"))?.trim().to_string();
        let ps = engram.state.get_project_cached(&pid).ok_or_else(|| anyhow::anyhow!("Runtime missing"))?;
        let old_generation: u64 = engram.state.registry.get_meta(&pid, "active_generation")?.ok_or_else(|| anyhow::anyhow!("Missing head"))?.parse()?;
        let search_query = HybridQuery { project_id: pid.clone(), namespace: "memory".into(), generation: old_generation,
            text: "requestviewmarker".into(), top_k: 3, fts_mode: "loose".into(), include_path_prefixes: None,
            exclude_path_prefixes: None, include_path_suffixes: None, language_filters: None,
            author_filter: None, date_after: None, date_before: None, use_mmr: false };
        let old_hits = ps.search.lexical_search(&search_query)?;
        ensure!(old_hits.len() == 1);
        let old_hit = old_hits[0].clone();
        let old_content = ps.search.get_doc_by_pk(&old_hit.pk)?.ok_or_else(|| anyhow::anyhow!("Old body missing"))?.2;
        ensure!(old_content.len() > 500 && old_content.len() < 4000 && old_content.contains("old_tail"));
        let old_hash = blake3::hash(old_content.as_bytes());
        let pause = Arc::new(Pause::default());
        PAUSES.lock().expect("pause registry").insert(pid.clone(), pause.clone());
        let registered = Registered { pid: pid.clone(), pause: pause.clone() };
        let request_engram = Engram::new(engram.state.clone());
        let request = query(&pid)?;
        requests.spawn(async move { request_engram.handle_search_memory(request).await });
        tokio::time::timeout(std::time::Duration::from_secs(15), pause.entered.notified()).await?;

        std::fs::write(root.join("src/target.rs"), &new_source)?;
        let updated = engram.handle_update_project(serde_json::from_value(serde_json::json!({
            "project_id":pid,"wait":true,"index_antipatterns":false
        }))?).await?;
        ensure!(updated.is_error != Some(true));
        let new_generation: u64 = engram.state.registry.get_meta(&pid, "active_generation")?.ok_or_else(|| anyhow::anyhow!("Missing head"))?.parse()?;
        ensure!(new_generation > old_generation);
        ps.search.purge_old_generations(&pid, new_generation).await?;
        ensure!(ps.search.get_doc_by_pk(&old_hit.pk)?.is_some(), "GC removed the real paused request's selected revision");
        drop(registered);
        let response = tokio::time::timeout(std::time::Duration::from_secs(15), requests.join_next()).await?
            .ok_or_else(|| anyhow::anyhow!("Search task missing"))???;
        let body = &response.content[0].as_text().ok_or_else(|| anyhow::anyhow!("Missing search response"))?.text;
        let content = body.split("content:\n").nth(1).ok_or_else(|| anyhow::anyhow!("Full selected content unavailable"))?;
        ensure!(content.starts_with(&old_content) && !body.contains("new_tail") && body.contains(&old_hit.doc_id),
            "Search substituted a newer body or fell back to a truncated snippet");
        ensure!(blake3::hash(&content.as_bytes()[..old_content.len()]) == old_hash);

        let fresh = engram.handle_search_memory(query(&pid)?).await?;
        let fresh_body = &fresh.content[0].as_text().ok_or_else(|| anyhow::anyhow!("Missing fresh response"))?.text;
        ensure!(fresh_body.contains("new_tail") && !fresh_body.contains("old_tail"));
        ps.search.purge_old_generations(&pid, new_generation).await?;
        ensure!(ps.search.get_doc_by_pk(&old_hit.pk)?.is_none(), "Completed request still pins its obsolete physical row");
        Ok(())
    }.await;
    requests.abort_all();
    while requests.join_next().await.is_some() {}
    temporary.close()?;
    result
}

fn visible_hits(response: &CallToolResult) -> Result<Vec<Vec<String>>> {
    let text = &response.content[0].as_text().ok_or_else(|| anyhow::anyhow!("Missing search text"))?.text;
    Ok(text.split("\n#").skip(1).map(|block| {
        block.lines().filter(|line| ["doc_id: ", "chunk_id: ", "path: ", "lines: ", "score: "]
            .iter().any(|prefix| line.starts_with(prefix)) || line.starts_with("pub fn pageitem"))
            .map(str::to_owned).collect::<Vec<_>>()
    }).collect())
}

#[tokio::test]
async fn selected_source_small_pages_preserve_the_full_window_and_cap_notices() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let result = async {
        let root = temporary.path().join("repo");
        std::fs::create_dir_all(root.join("src"))?;
        for i in 0..211 {
            std::fs::write(root.join(format!("src/file{i:03}.rs")),
                format!("pub fn pageitem{i:03}() -> &'static str {{ \"needlepage original{i:03}\" }}\n"))?;
        }
        // History and ignore discovery belong to this fixture's own repository.
        {
            let repo = git2::Repository::init(&root)?;
            let mut index = repo.index()?;
            index.add_all(["src"], git2::IndexAddOption::DEFAULT, None)?;
            let tree = repo.find_tree(index.write_tree()?)?;
            let signature = git2::Signature::now("Fixture", "fixture@example.invalid")?;
            repo.commit(Some("HEAD"), &signature, &signature, "owned pagination baseline", &tree, &[])?;
        }
        let (state, _) = crate::state::AppState::new(engram_core::Config {
            data_dir: temporary.path().join("data"), allowed_roots: vec![root.clone()],
            embedding_backend: if cfg!(feature = "vector") { "local" } else { "fts_only" }.into(),
            ..Default::default()
        })?;
        let engram = Engram::new(state);
        let indexed = engram.handle_index_project(serde_json::from_value(serde_json::json!({
            "directory":root,"project_name":"Public source pagination","project_type":"rust","wait":true
        }))?).await?;
        let text = &indexed.content[0].as_text().ok_or_else(|| anyhow::anyhow!("Missing index text"))?.text;
        let pid = text.split("project_id: ").nth(1).and_then(|s| s.lines().next())
            .ok_or_else(|| anyhow::anyhow!("Missing project ID"))?.trim().to_string();
        std::fs::write(root.join("src/file000.rs"), "pub fn pageitem000() -> &'static str { \"needlepage changed000\" }\n")?;
        engram.handle_update_project(serde_json::from_value(serde_json::json!({
            "project_id":pid,"wait":true,"index_antipatterns":false
        }))?).await?;
        let generation = engram.state.registry.get_meta(&pid, "active_generation")?.ok_or_else(|| anyhow::anyhow!("Missing head"))?;
        // Hold ranking inputs constant so this test concerns pagination rather
        // than the documented mutability of PageRank between requests.
        let generation = generation.parse::<u64>()?;
        let cache = engram.state.projects.get(&pid).ok_or_else(|| anyhow::anyhow!("Missing runtime"))?.pagerank_cache.clone();
        let permit = engram.state.pagerank_workers.clone().try_acquire_owned()?;
        cache.start(generation, permit).ok_or_else(|| anyhow::anyhow!("Ranking fixture was already computing"))?
            .complete(engram_graph::analysis::CentralityMetrics { pagerank: HashMap::new() });
        let request = |offset, max_results, use_mmr| -> Result<SearchMemoryRequest> {
            Ok(serde_json::from_value(serde_json::json!({"project_id":pid,"query":"needlepage",
                "offset":offset,"max_results":max_results,"use_mmr":use_mmr,"semantic":true,
                "include_content":true,"max_content_chars_per_result":500}))?)
        };
        for use_mmr in [true, false] {
            let full = engram.handle_search_memory(request(0, 200, use_mmr)?).await?;
            let expected = visible_hits(&full)?;
            ensure!(expected.len() == 200);
            let mut collected = Vec::new();
            for offset in (0..200).step_by(17) {
                let response = engram.handle_search_memory(request(offset, 17, use_mmr)?).await?;
                let hits = visible_hits(&response)?;
                ensure!(hits == expected[offset..(offset+17).min(200)],
                    "Small page changed source identity, order, score, lines or full content");
                let text = &response.content[0].as_text().ok_or_else(|| anyhow::anyhow!("Missing page text"))?.text;
                let next = offset + hits.len();
                ensure!(if next == 200 { text.contains("result_cap_reached: 200") }
                    else { text.contains(&format!("next_page: offset={next};")) });
                collected.extend(hits);
            }
            ensure!(collected == expected, "Concatenated pages do not reconstruct the full bounded result window");
            for offset in [200, usize::MAX] {
                let response = engram.handle_search_memory(request(offset, 3, use_mmr)?).await?;
                ensure!(visible_hits(&response)?.is_empty(), "Offset beyond the cap rewound to earlier results");
            }
        }
        Ok(())
    }.await;
    temporary.close()?;
    result
}

#[cfg(windows)]
#[tokio::test]
async fn selected_source_metadata_sharing_failure_keeps_the_exact_head_and_retry_converges() -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    let temporary = tempfile::tempdir()?;
    let result = async {
        let root = temporary.path().join("repo");
        // A path containing a vector diagnostic token is not a vector fault.
        let data = temporary.path().join("VEC1-lexical-data");
        std::fs::create_dir_all(root.join("src"))?;
        std::fs::write(root.join("src/target.rs"), "pub fn failureviewmarker() -> &'static str { \"old_complete_body\" }\n")?;
        std::fs::write(root.join("src/companion.rs"), "pub fn stablecompanion() {}\n")?;
        {
            let repo = git2::Repository::init(&root)?;
            let mut index = repo.index()?;
            index.add_all(["src"], git2::IndexAddOption::DEFAULT, None)?;
            let tree = repo.find_tree(index.write_tree()?)?;
            let signature = git2::Signature::now("Fixture", "fixture@example.invalid")?;
            repo.commit(Some("HEAD"), &signature, &signature, "owned sharing fixture", &tree, &[])?;
        }
        let (state, _) = crate::state::AppState::new(engram_core::Config {
            data_dir: data.clone(), allowed_roots: vec![root.clone()],
            embedding_backend: if cfg!(feature = "vector") { "local" } else { "fts_only" }.into(),
            ..Default::default()
        })?;
        let engram = Engram::new(state);
        let indexed = engram.handle_index_project(serde_json::from_value(serde_json::json!({
            "directory":root,"project_name":"Metadata sharing recovery","project_type":"rust","wait":true
        }))?).await?;
        let text = &indexed.content[0].as_text().ok_or_else(|| anyhow::anyhow!("Missing index text"))?.text;
        let pid = text.split("project_id: ").nth(1).and_then(|s| s.lines().next())
            .ok_or_else(|| anyhow::anyhow!("Missing project ID"))?.trim().to_string();
        let before = engram.state.registry.capture_source_head(&pid)?;
        let ps = engram.state.get_project_cached(&pid).ok_or_else(|| anyhow::anyhow!("Runtime missing"))?;
        #[cfg(feature = "vector")]
        let physical_vectors_before = ps.search.count_vectors(&pid).await?;
        let metadata = data.join("projects").join(&pid).join("tantivy/meta.json");
        ensure!(metadata.is_file());
        // This owned read handle allows reads/writes but denies replacement.
        // It makes the prepublication metadata commit fail deterministically.
        let replacement_lock = std::fs::OpenOptions::new().read(true).share_mode(0x1 | 0x2).open(&metadata)?;
        std::fs::write(root.join("src/target.rs"), "pub fn failureviewmarker() -> &'static str { \"unpublished_attempt_body\" }\n")?;
        let failed = engram.handle_update_project(serde_json::from_value(serde_json::json!({
            "project_id":pid,"wait":true,"index_antipatterns":false
        }))?).await;
        let error = failed.err().ok_or_else(|| anyhow::anyhow!("Locked lexical metadata unexpectedly published"))?;
        let message = error.to_string();
        ensure!(message.contains("lexical stage=commit") && message.contains("commit=failed_ambiguous")
            && message.contains(&pid) && message.contains("index_directory="),
            "Commit failure omitted its actual phase/project/store context: {message}");
        ensure!(engram.state.registry.source_head_is_current(&before)?, "A failed prepublication commit changed the exact authoritative head");
        ensure!(engram.state.registry.get_project(&pid)?.ok_or_else(|| anyhow::anyhow!("Project missing"))?
            .reindex_required_since_ms.is_none(), "A lexical IO failure/path token incorrectly requested vector recovery");
        #[cfg(feature = "vector")]
        ensure!(ps.search.count_vectors(&pid).await? > physical_vectors_before,
            "Fixture did not exercise native rows committed before the lexical failure");
        let request = || -> Result<SearchMemoryRequest> { Ok(serde_json::from_value(serde_json::json!({
            "project_id":pid,"query":"failureviewmarker","include_content":true,"semantic":false,"max_results":3
        }))?) };
        let old = engram.handle_search_memory(request()?).await?;
        let old_text = &old.content[0].as_text().ok_or_else(|| anyhow::anyhow!("Old response missing"))?.text;
        ensure!(old_text.contains("old_complete_body") && !old_text.contains("unpublished_attempt_body"));
        #[cfg(feature = "vector")]
        ensure!(ps.search.count_vectors_in_generation(&pid, before.generation()).await?
            == ps.search.count_docs_in_generation(&pid, "memory", before.generation())?, "Failure broke old source/vector coverage");
        drop(replacement_lock);
        std::fs::write(root.join("src/target.rs"), "pub fn failureviewmarker() -> &'static str { \"retry_complete_body\" }\n")?;
        let retried = engram.handle_update_project(serde_json::from_value(serde_json::json!({
            "project_id":pid,"wait":true,"index_antipatterns":false
        }))?).await?;
        ensure!(retried.is_error != Some(true));
        let after = engram.state.registry.capture_source_head(&pid)?;
        ensure!(after.generation() > before.generation());
        let fresh = engram.handle_search_memory(request()?).await?;
        let fresh_text = &fresh.content[0].as_text().ok_or_else(|| anyhow::anyhow!("Retry response missing"))?.text;
        ensure!(fresh_text.contains("retry_complete_body") && !fresh_text.contains("old_complete_body")
            && !fresh_text.contains("unpublished_attempt_body"));
        ensure!(ps.search.paths_in_generation(&pid, Some("memory"), after.generation())?
            == std::collections::BTreeSet::from(["src/target.rs".to_string(),"src/companion.rs".to_string()]));
        #[cfg(feature = "vector")]
        ensure!(ps.search.count_vectors_in_generation(&pid, after.generation()).await?
            == ps.search.count_docs_in_generation(&pid, "memory", after.generation())?, "Retry source/vector coverage differs");
        Ok(())
    }.await;
    temporary.close()?;
    result
}
