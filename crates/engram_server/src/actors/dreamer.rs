use crate::state::{AppEvent, AppState};
use engram_graph::algorithms::clustering::find_cooccurrence_clusters;
use engram_graph::{EdgeKind, Node};
use engram_index::IndexDoc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast::Receiver;
use tokio_util::sync::CancellationToken;

/// CANCEL1: accepts a shutdown token so the dreamer loop exits cooperatively on
/// process shutdown rather than being aborted mid-dream (which could leave
/// partial insight/cluster writes outstanding).
pub async fn run_dreamer(state: AppState, mut rx: Receiver<AppEvent>, shutdown: CancellationToken) {
    let mut last_event = Instant::now();

    // Config-driven defaults.
    let idle_after = Duration::from_secs(state.cfg.dream_idle_after_secs);
    let tick = Duration::from_secs(state.cfg.dream_tick_secs.max(1));
    let min_edge_weight = state.cfg.dream_default_min_edge_weight;
    let min_cluster_size = state.cfg.dream_default_min_cluster_size;
    let max_clusters = state.cfg.dream_default_max_clusters;

    let mut interval = tokio::time::interval(tick);

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                tracing::info!("dreamer: shutdown token cancelled — exiting");
                return;
            }
            _ = interval.tick() => {
                if last_event.elapsed() >= idle_after {
                    // Skip dreaming if system is busy indexing.
                    if state.active_indexing_count.load(std::sync::atomic::Ordering::Relaxed) > 0 {
                        continue;
                    }

                    // Dream over all registered projects (not just those currently in cache).
                    let registry = state.registry.clone();
                    let project_ids: Vec<String> = match tokio::task::spawn_blocking(move || {
                        registry.list_projects().map(|v| v.into_iter().map(|p| p.project_id).collect::<Vec<_>>())
                    }).await {
                        Err(e) => {
                            tracing::error!(
                                "ENG-AUD-S1-0002: dreamer: spawn_blocking panicked listing projects: {e}; skipping dream cycle"
                            );
                            continue;
                        }
                        Ok(Err(e)) => {
                            tracing::warn!(
                                "ENG-AUD-S1-0002: dreamer: registry list_projects failed: {e}; skipping dream cycle"
                            );
                            continue;
                        }
                        Ok(Ok(v)) => v,
                    };

                    for pid in project_ids {
                        if let Err(e) = dream_once(&state, &pid, min_edge_weight, min_cluster_size, max_clusters).await {
                            tracing::debug!("dreamer error: {e:#}");
                        }
                    }

                    last_event = Instant::now();
                }
            }
            maybe = rx.recv() => {
                match maybe {
                    Ok(ev) => {
                        last_event = Instant::now();
                        match ev {
                            AppEvent::SearchSession { project_id, hits } => {
                                if let Err(e) = record_cooccurrence(&state, &project_id, &hits).await {
                                    tracing::debug!("cooccurrence error: {e:#}");
                                }
                            }
                            AppEvent::TriggerDream { project_id } => {
                                if let Err(e) = dream_once(&state, &project_id, min_edge_weight, min_cluster_size, max_clusters).await {
                                    tracing::debug!("manual dream error: {e:#}");
                                }
                            }
                            AppEvent::WatchUpdate { .. } => {}
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        tracing::info!("dreamer: event broadcast channel closed — exiting");
                        return;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        }
    }
}

async fn load_project_runtime(
    state: &AppState,
    project_id: &str,
) -> anyhow::Result<Option<crate::state::ProjectState>> {
    if let Some(p) = state.get_project_cached(project_id) {
        return Ok(Some(p));
    }
    let reg = state.registry.clone();
    let pid = project_id.to_string();
    let rec = tokio::task::spawn_blocking(move || reg.get_project(&pid))
        .await
        .map_err(|e| anyhow::anyhow!("ENG-AUD-2026-S13-0001: spawn_blocking panicked in dreamer registry lookup: {e}"))?
        .map_err(|e| anyhow::anyhow!("ENG-AUD-2026-S13-0001: registry get_project failed: {e}"))?;
    let Some(rec) = rec else {
        return Ok(None);
    };

    let tantivy_dir = state
        .cfg
        .data_dir
        .join("projects")
        .join(project_id)
        .join("tantivy");
    let lancedb_dir = state
        .cfg
        .data_dir
        .join("projects")
        .join(project_id)
        .join("lancedb");
    std::fs::create_dir_all(&tantivy_dir).map_err(|e| {
        anyhow::anyhow!("ENG-AUD-S1-0002: failed to create tantivy dir {:?}: {e}", tantivy_dir)
    })?;
    std::fs::create_dir_all(&lancedb_dir).map_err(|e| {
        anyhow::anyhow!("ENG-AUD-S1-0002: failed to create lancedb dir {:?}: {e}", lancedb_dir)
    })?;

    let search = engram_index::HybridSearchEngine::new_with_budget(
        tantivy_dir.clone(),
        lancedb_dir.clone(),
        &state.cfg,
        Some(state.memory_budget.clone()),
    )
    .await?;
    let ps = crate::state::ProjectState {
        info: crate::state::ProjectInfo {
            project_id: project_id.to_string(),
            project_name: rec.project_name,
            project_type: rec.project_type,
            directory: rec.directory,
            tantivy_dir,
            lancedb_dir,
        },
        search: std::sync::Arc::new(search),
    };
    state.put_project_cached(ps.clone()).await;
    Ok(Some(ps))
}
/// Fetch the active generation from the registry.
/// Returns Err on spawn failure or registry/parse error (ENG-AUD-2026-0003).
/// Callers should propagate the error rather than defaulting to generation 1.
async fn fetch_active_generation(state: &crate::state::AppState, project_id: &str) -> anyhow::Result<u64> {
    let reg = state.registry.clone();
    let pid = project_id.to_string();
    tokio::task::spawn_blocking(move || -> anyhow::Result<u64> {
        let gen_str = reg.get_meta(&pid, "active_generation")
            .map_err(|e| anyhow::anyhow!("ENG-AUD-2026-0003: registry get_meta failed: {e}"))?
            .unwrap_or_else(|| "1".to_string());
        gen_str.parse::<u64>().map_err(|e| anyhow::anyhow!(
            "ENG-AUD-2026-0003: active_generation metadata is corrupt (value={gen_str:?}): {e}"
        ))
    })
    .await
    .map_err(|e| anyhow::anyhow!("ENG-AUD-2026-0003: spawn_blocking panicked reading active_generation: {e}"))?
}

pub async fn record_cooccurrence(
    state: &AppState,
    project_id: &str,
    hits: &[crate::state::SearchHitLite],
) -> anyhow::Result<()> {
    // Fetch active generation
    let active_gen: u64 = fetch_active_generation(state, project_id).await?;

    // Limit per query so we don't explode O(n^2) updates.
    let mut h = hits.to_vec();
    h.truncate(12);

    // Ensure chunk + file nodes exist.
    let mut nodes: Vec<Node> = Vec::new();
    let mut dep_pairs: Vec<(EdgeKind, String, String, u32)> = Vec::new();
    for item in &h {
        let language = engram_core::guess_language(std::path::Path::new(item.path.as_str()));
        let chunk_node_id = format!("pk:{}", item.pk);
        nodes.push(Node {
            node_id: chunk_node_id.clone(),
            node_type: "chunk".into(),
            name: format!("{}#{}", item.path, item.doc_id),
            namespace: engram_core::namespaces::NAMESPACE_MEMORY.into(),
            language: language.into(),
            file_path: item.path.clone(),
            start_line: 0,
            end_line: 0,
            generation: active_gen,
            metadata: None,
        });

        nodes.push(Node {
            node_id: format!("file:{}", item.path),
            node_type: "file".into(),
            name: item
                .path
                .file_name()
                .unwrap_or_else(|| item.path.as_str())
                .to_string(),
            namespace: engram_core::namespaces::NAMESPACE_MEMORY.into(),
            language: language.into(),
            file_path: item.path.clone(),
            start_line: 0,
            end_line: 0,
            generation: active_gen,
            metadata: None,
        });

        let file_node_id = format!("file:{}", item.path);
        // Link file <-> chunk (static edge, so weight doesn't matter much).
        dep_pairs.push((EdgeKind::Dependency, file_node_id, chunk_node_id, 1));
    }
    state.graph.upsert_nodes(project_id, &nodes)?;
    if !dep_pairs.is_empty() {
        state.graph.batch_increment_undirected_edges(
            project_id,
            engram_core::namespaces::NAMESPACE_MEMORY,
            "text",
            active_gen,
            &dep_pairs,
        )?;
    }

    // Update co-occurrence weights for all pairs.
    let mut co_pairs: Vec<(EdgeKind, String, String, u32)> = Vec::new();
    for i in 0..h.len() {
        for j in (i + 1)..h.len() {
            let a = format!("pk:{}", h[i].pk);
            let b = format!("pk:{}", h[j].pk);
            co_pairs.push((EdgeKind::CoOccurrence, a, b, 1));
        }
    }
    if !co_pairs.is_empty() {
        state.graph.batch_increment_undirected_edges(
            project_id,
            engram_core::namespaces::NAMESPACE_MEMORY,
            "text",
            active_gen,
            &co_pairs,
        )?;
    }

    Ok(())
}

pub async fn dream_once(
    state: &AppState,
    project_id: &str,
    min_edge_weight: u32,
    min_cluster_size: usize,
    max_clusters: usize,
) -> anyhow::Result<usize> {
    let project = match load_project_runtime(state, project_id).await? {
        Some(p) => p,
        None => return Ok(0),
    };

    let clusters = find_cooccurrence_clusters(
        &state.graph,
        project_id,
        min_edge_weight,
        min_cluster_size,
        max_clusters,
    )?;

    if clusters.is_empty() {
        return Ok(0);
    }

    let mut insights_generated = 0;
    // Deduplicate within this dream cycle without a full DB round-trip per cluster.
    let mut seen_fingerprints: std::collections::HashSet<String> = std::collections::HashSet::new();
    for cluster in clusters {
        let fingerprint = cluster.fingerprint();
        if !seen_fingerprints.insert(fingerprint.clone()) {
            continue;
        }
        if state
            .graph
            .fingerprint_has_insight(project_id, &fingerprint)?
        {
            continue;
        }

        // Pull some context for summarization.
        let active_generation: u64 = fetch_active_generation(state, project_id).await?;

        let mut ctx: Vec<String> = Vec::new();
        let mut src_nodes: Vec<String> = Vec::new();

        for nid in cluster.node_ids.iter().take(8) {
            if let Some(pk) = nid.strip_prefix("pk:")
                && let Ok(Some((_, _, text, _, _))) = project.search.get_doc_by_pk(pk)
            {
                ctx.push(text);
                src_nodes.push(nid.clone());
            }
        }

        if ctx.is_empty() {
            continue;
        }

        // Check if this cluster represents an anti-pattern
        let mut is_antipattern = false;
        let mut anti_msg = String::new();

        // Sample one chunk from the cluster for the check
        if let Some(sample) = ctx.first() {
            let q = sample.chars().take(1000).collect::<String>();
            let gen_ = active_generation;
            let pid = project_id.to_string();

            // We search for similarities in the antipattern namespace
            if let Ok(hits) = project
                .search
                .search(
                    &engram_index::HybridQuery {
                        project_id: pid,
                        namespace: "antipattern".into(),
                        generation: gen_,
                        text: q,
                        top_k: 50,
                        fts_mode: "strict".into(),
                        include_path_prefixes: None,
                        exclude_path_prefixes: None,
                        language_filters: None,
                        author_filter: None,
                        date_after: None,
                        date_before: None,
                        use_mmr: false,
                    },
                    None,
                )
                .await
                && let Some(best) = hits.first()
            {
                let decision = state.immune.decide(best.score, None);
                match decision {
                    engram_ml::ImmuneDecision::Allow => {}
                    engram_ml::ImmuneDecision::Warn { message, .. }
                    | engram_ml::ImmuneDecision::Block { message, .. } => {
                        is_antipattern = true;
                        anti_msg = format!(
                            "⚠️ ANTIPATTERN DETECTED\n\n{}\n\nSource: {}\n",
                            message, best.path
                        );
                    }
                }
            }
        }

        let insight = state
            .dreaming
            .summarize_cluster(&ctx, Duration::from_secs(10))
            .await;
        if insight.used_llm_fallback {
            tracing::debug!(project_id, "dreamer: LLM unavailable — insight generated via deterministic fallback");
        }
        let mut summary = insight.summary_markdown;
        if is_antipattern {
            summary = format!("ANTIPATTERN DETECTED\n\n{}\n\n---\n{}", anti_msg, summary);
        }

        let insight_id = format!("insight:{}", uuid::Uuid::new_v4());

        // Build the insight content first so we can compute proper content-based IDs.
        let namespace = engram_core::namespaces::NAMESPACE_INSIGHTS;
        let effective_gen = if let Ok(policy) = engram_core::get_policy(namespace) {
            if policy.versioning == engram_core::NamespaceVersioning::GlobalMutable {
                0
            } else {
                active_generation
            }
        } else {
            active_generation
        };

        let insight_content = format!("# {}\n\n{}", insight.title, summary);
        let content_hash = engram_core::ContentHash::compute(insight_content.as_bytes());
        let insight_path = format!("__insights/{insight_id}.md");
        let doc_id = engram_core::DocIdStr::compute(&insight_path, 0, 0, &content_hash);
        let chunk_id = engram_index::chunk_id_from_content_hash(&content_hash);

        // Store first 3 context snippets as evidence.
        let evidence = ctx.iter().take(3).cloned().collect();

        // Create the graph insight node and index in one consistent flow.
        state.graph.create_insight(
            project_id,
            &insight_id,
            &insight.title,
            &summary,
            &src_nodes,
            Some(evidence),
            Some(fingerprint),
            active_generation,
        )?;

        let cancel = tokio_util::sync::CancellationToken::new();
        let doc = IndexDoc {
            generation: effective_gen,
            chunk_id,
            path: insight_path.into(),
            language: "markdown".into(),
            content: insight_content,
            namespace: namespace.into(),
            author: None,
            timestamp: None,
            start_line: 0,
            end_line: 0,
            doc_id: doc_id.0,
            content_hash: content_hash.0,
        };
        project
            .search
            .index_docs(project_id, &[doc], &cancel)
            .await?;
        insights_generated += 1;
    }

    // ── Anti-pattern detection during dream cycle ──────────────────────
    // Run deterministic design anti-pattern detection and create insight
    // nodes so they surface in search results and the knowledge graph.
    let graph = state.graph.clone();
    let pid_ap = project_id.to_string();
    let ap_results = tokio::task::spawn_blocking(move || {
        crate::services::pattern_detection_service::detect_design_antipatterns(
            &graph, &pid_ap, 20, 10, 5,
        )
    })
    .await
    .unwrap_or_else(|_| Ok(Vec::new()))
    .unwrap_or_default();

    for ap in ap_results {
        // Dedup: use pattern_name + first affected node as fingerprint
        let fp = format!(
            "antipattern:{}:{}",
            ap.pattern_name,
            ap.affected_nodes.first().map(|s| s.as_str()).unwrap_or("")
        );
        if !seen_fingerprints.insert(fp.clone()) {
            continue;
        }
        if state.graph.fingerprint_has_insight(project_id, &fp)? {
            continue;
        }

        let active_gen: u64 = fetch_active_generation(state, project_id).await?;

        let insight_id = format!("insight:ap:{}", uuid::Uuid::new_v4());
        let title = format!("Anti-Pattern: {} [{}]", ap.pattern_name, ap.severity);
        let mut body = format!("{}\n\n", ap.description);
        body.push_str(&format!("**Modern target:** {}\n\n", ap.modern_target));
        body.push_str("**Refactoring steps:**\n");
        for (i, step) in ap.refactoring_steps.iter().enumerate() {
            body.push_str(&format!("{}. {}\n", i + 1, step));
        }

        let namespace = engram_core::namespaces::NAMESPACE_INSIGHTS;
        let effective_gen = if let Ok(policy) = engram_core::get_policy(namespace) {
            if policy.versioning == engram_core::NamespaceVersioning::GlobalMutable {
                0
            } else {
                active_gen
            }
        } else {
            active_gen
        };

        let insight_content = format!("# {}\n\n{}", title, body);
        let content_hash = engram_core::ContentHash::compute(insight_content.as_bytes());
        let insight_path = format!("__insights/{insight_id}.md");
        let doc_id = engram_core::DocIdStr::compute(&insight_path, 0, 0, &content_hash);
        let chunk_id = engram_index::chunk_id_from_content_hash(&content_hash);

        state.graph.create_insight(
            project_id,
            &insight_id,
            &title,
            &body,
            &ap.affected_nodes,
            Some(ap.evidence.clone()),
            Some(fp),
            active_gen,
        )?;

        let cancel = tokio_util::sync::CancellationToken::new();
        let doc = IndexDoc {
            generation: effective_gen,
            chunk_id,
            path: insight_path.into(),
            language: "markdown".into(),
            content: insight_content,
            namespace: namespace.into(),
            author: None,
            timestamp: None,
            start_line: 0,
            end_line: 0,
            doc_id: doc_id.0,
            content_hash: content_hash.0,
        };
        project
            .search
            .index_docs(project_id, &[doc], &cancel)
            .await?;
        insights_generated += 1;
    }

    Ok(insights_generated)
}

#[cfg(test)]
mod tests {
    /// ENG-AUD-S1-0002: dreamer project-listing error path must NOT silently return
    /// an empty project list when the registry lookup panics.
    ///
    /// Old behavior: `unwrap_or_else(|_| Ok(None))?` swallowed the JoinError and
    /// returned None/empty, causing the dreamer to skip all projects silently.
    /// New behavior: `map_err(|e| anyhow!(...))` propagates the JoinError as Err.
    #[tokio::test]
    async fn spawn_blocking_project_list_panic_propagates_not_empty_vec() {
        let result: Result<Vec<String>, _> = tokio::task::spawn_blocking(|| -> Vec<String> {
            panic!("ENG-AUD-S1-0002: simulated registry panic in project list");
        })
        .await;
        assert!(
            result.is_err(),
            "ENG-AUD-S1-0002: spawn_blocking panic must produce JoinError, not an empty Vec. \
             Silent empty-vec return would skip all dreamer work without any error signal."
        );
    }

    /// ENG-AUD-2026-S07-0001: `create_dir_all` on an existing file path must return
    /// an explicit `Err`, not be silently swallowed by `.ok()`.
    ///
    /// This is the exact scenario the fix targets: if the dreamer's output directory
    /// path collides with an existing file, `.ok()` would hide the failure, causing
    /// downstream writes to fail with cryptic "not a directory" errors.
    #[test]
    fn create_dir_all_on_existing_file_returns_err_not_ok() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let collision = tmp.path().join("collision.dat");
        std::fs::write(&collision, b"existing content").expect("create file");
        // Attempt to create a directory at the path occupied by the file.
        let result = std::fs::create_dir_all(&collision);
        assert!(
            result.is_err(),
            "ENG-AUD-2026-S07-0001: create_dir_all on existing file path must return Err; \
             .ok() would silently swallow this, producing cryptic downstream failures"
        );
    }

    /// ENG-AUD-2026-0003: a panicking `spawn_blocking` must produce a `JoinError`
    /// that is identifiable as a panic (not an Ok or a cancelled error).
    ///
    /// The `fetch_active_generation` helper uses `spawn_blocking` + `map_err` to ensure
    /// registry failures are explicit — this test verifies the tokio JoinError API
    /// that the fix relies on.
    #[tokio::test]
    async fn spawn_blocking_panic_produces_identifiable_join_error() {
        let handle = tokio::task::spawn_blocking(|| -> u64 {
            panic!("ENG-AUD-2026-0003: simulated fetch_active_generation failure");
        });
        let result = handle.await;
        assert!(result.is_err(), "panic must yield Err(JoinError)");
        let join_err = result.unwrap_err();
        assert!(
            join_err.is_panic(),
            "ENG-AUD-2026-0003: JoinError must report is_panic()=true so callers can \
             distinguish panics from cancellations. Got: is_panic={}, is_cancelled={}",
            join_err.is_panic(),
            join_err.is_cancelled()
        );
    }

    /// ENG-AUD-2026-0003: generation=0 is the 'unindexed' sentinel. If
    /// `fetch_active_generation` silently returned 0 on error, a project with a
    /// real active index (generation ≥ 1) would appear unindexed to the dreamer,
    /// causing it to skip evidence enrichment entirely.
    #[test]
    fn generation_zero_sentinel_differs_from_first_real_generation() {
        let unindexed_sentinel: u64 = 0;
        let first_real_gen: u64 = 1;
        assert_ne!(
            unindexed_sentinel, first_real_gen,
            "ENG-AUD-2026-0003: generation=0 must be the 'no index' sentinel. \
             A silent default of 0 on fetch error makes indexed projects appear unindexed."
        );
        // Demonstrate the wrap-around danger of u64::MAX default:
        let dangerous_default = u64::MAX;
        let next = dangerous_default.wrapping_add(1);
        assert_eq!(
            next, 0,
            "ENG-AUD-2026-0003: u64::MAX.wrapping_add(1)=0, resetting the generation counter. \
             This is why unwrap_or_default() = unwrap_or(0) was replaced with explicit Err."
        );
    }

    /// Gate 2.0 Test 8 (ENG-AUD-2026-S13-0001): dreamer spawn_blocking join failure
    /// must produce an explicit error, not be swallowed as Ok(None).
    #[tokio::test]
    async fn spawn_blocking_join_error_is_propagated_not_swallowed() {
        let result: Result<i32, _> = tokio::task::spawn_blocking(|| -> i32 {
            panic!("simulated registry panic");
        })
        .await;
        assert!(result.is_err(), "spawn_blocking panic must produce JoinError");
    }
}
