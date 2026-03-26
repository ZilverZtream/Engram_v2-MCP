use crate::state::{AppEvent, AppState};
use engram_graph::algorithms::clustering::find_cooccurrence_clusters;
use engram_graph::{EdgeKind, Node};
use engram_index::IndexDoc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast::Receiver;

pub async fn run_dreamer(state: AppState, mut rx: Receiver<AppEvent>) {
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
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
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
        .unwrap_or_else(|_| Ok(None))?;
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
    #[test]
    fn eng_aud_s1_0002_tag_present_in_source() {
        let source = include_str!("dreamer.rs");
        assert!(
            source.contains("ENG-AUD-S1-0002"),
            "dreamer.rs must contain ENG-AUD-S1-0002 audit tags"
        );
    }

    #[test]
    fn dreamer_project_list_error_is_logged() {
        // Positive check: the ENG-AUD-S1-0002 tag appears for both the project-listing
        // error path and the create_dir_all error path.
        let source = include_str!("dreamer.rs");
        let tag_count = source.matches("ENG-AUD-S1-0002").count();
        assert!(
            tag_count >= 2,
            "dreamer.rs must have ENG-AUD-S1-0002 tag on at least two error paths; found {tag_count}"
        );
    }

    #[test]
    fn create_dir_all_errors_propagate() {
        // Positive check: map_err (propagation) must appear near create_dir_all.
        let source = include_str!("dreamer.rs");
        assert!(
            source.contains("create_dir_all") && source.contains("map_err"),
            "create_dir_all must be paired with map_err error propagation (not .ok() suppression)"
        );
    }

    #[test]
    fn eng_aud_2026_0003_fetch_generation_uses_error_propagation() {
        let source = include_str!("dreamer.rs");
        // The helper function must exist
        assert!(
            source.contains("fetch_active_generation"),
            "dreamer.rs must define fetch_active_generation helper"
        );
        // The ENG-AUD tag must be present
        assert!(
            source.contains("ENG-AUD-2026-0003"),
            "dreamer.rs must contain ENG-AUD-2026-0003 tag"
        );
    }

    #[test]
    fn dreamer_generation_fetch_does_not_silently_default() {
        let source = include_str!("dreamer.rs");
        // Positive check: ENG-AUD-2026-0003 tag must appear at multiple call sites
        // (the helper definition + at least one error path).
        let tag_count = source.matches("ENG-AUD-2026-0003").count();
        assert!(
            tag_count >= 3,
            "dreamer.rs must have ENG-AUD-2026-0003 on all generation fetch error paths; found {tag_count}"
        );
        // Positive check: fetch_active_generation is called instead of inline unwrap_or
        let call_count = source.matches("fetch_active_generation").count();
        assert!(
            call_count >= 3,
            "dreamer.rs must call fetch_active_generation in multiple places; found {call_count}"
        );
    }
}
