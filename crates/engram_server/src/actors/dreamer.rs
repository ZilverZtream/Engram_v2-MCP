use crate::state::{AppEvent, AppState};
use engram_graph::algorithms::clustering::find_cooccurrence_clusters;
use engram_graph::{EdgeKind, Node};
use engram_index::IndexDoc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast::Receiver;

pub async fn run_dreamer(state: AppState, mut rx: Receiver<AppEvent>) {
    let mut last_event = Instant::now();

    // Defaults; override via config fields later.
    let idle_after = Duration::from_secs(20);
    let tick = Duration::from_secs(2);
    let min_edge_weight = 2u32;
    let min_cluster_size = 3usize;
    let max_clusters = 2usize;

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
                    let project_ids: Vec<String> = tokio::task::spawn_blocking(move || {
                        registry
                            .list_projects()
                            .map(|v| v.into_iter().map(|p| p.project_id).collect())
                            .unwrap_or_default()
                    }).await.unwrap_or_default();

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
    if let Some(p) = state.get_project_cached(project_id).await {
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
    std::fs::create_dir_all(&tantivy_dir).ok();
    std::fs::create_dir_all(&lancedb_dir).ok();

    let search = engram_index::HybridSearchEngine::new(
        tantivy_dir.clone(),
        lancedb_dir.clone(),
        state.cfg.embedding_backend.clone(),
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
pub async fn record_cooccurrence(
    state: &AppState,
    project_id: &str,
    hits: &[crate::state::SearchHitLite],
) -> anyhow::Result<()> {
    // Fetch active generation
    let active_gen: u64 = {
        let reg = state.registry.clone();
        let pid = project_id.to_string();
        tokio::task::spawn_blocking(move || {
            reg.get_meta(&pid, "active_generation")
                .ok()
                .flatten()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(1)
        })
        .await
        .unwrap_or(1)
    };

    // Limit per query so we don't explode O(n^2) updates.
    let mut h = hits.to_vec();
    h.truncate(12);

    // Ensure chunk + file nodes exist.
    let mut nodes: Vec<Node> = Vec::new();
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
        state.graph.increment_undirected_edge(
            project_id,
            engram_core::namespaces::NAMESPACE_MEMORY,
            language,
            EdgeKind::Dependency,
            &file_node_id,
            &chunk_node_id,
            1,
            active_gen,
        )?;
    }
    state.graph.upsert_nodes(project_id, &nodes)?;

    // Update co-occurrence weights for all pairs.
    for i in 0..h.len() {
        for j in (i + 1)..h.len() {
            let a = format!("pk:{}", h[i].pk);
            let b = format!("pk:{}", h[j].pk);
            state.graph.increment_undirected_edge(
                project_id,
                engram_core::namespaces::NAMESPACE_MEMORY,
                "text", // Co-occurrence is cross-language/generic
                EdgeKind::CoOccurrence,
                &a,
                &b,
                1,
                active_gen,
            )?;
        }
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
        let active_generation: u64 = {
            let reg = state.registry.clone();
            let pid = project_id.to_string();
            tokio::task::spawn_blocking(move || {
                reg.get_meta(&pid, "active_generation")
                    .ok()
                    .flatten()
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(1)
            })
            .await
            .unwrap_or(1)
        };

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
        let mut summary = insight.summary_markdown;
        if is_antipattern {
            summary = format!("{}\n\n---\n{}", anti_msg, summary);
        }

        let insight_id = format!("insight:{}", uuid::Uuid::new_v4());

        // Store first 3 context snippets as evidence.
        let evidence = ctx.iter().take(3).cloned().collect();

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

        // Also index the insight text so it is searchable.
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
        let chunk_id = {
            let h = blake3::hash(insight_id.as_bytes());
            let mut b = [0u8; 8];
            b.copy_from_slice(&h.as_bytes()[..8]);
            u64::from_le_bytes(b)
        };

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
