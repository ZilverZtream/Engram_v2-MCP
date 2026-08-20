// ENG-AUD-2026-S01-001
use crate::handlers::validate_project_id;
use crate::models::{
    AnalyzeErrorStackRequest, FindSymbolReferencesRequest, GetChunkRequest, SearchMemoryRequest,
    VectorSearchRequest,
};
use crate::state::{AppEvent, SearchHitLite};
use crate::tools::Engram;
use crate::utils::text::stacktrace_to_query;
use engram_graph::EdgeKind;
use engram_index::HybridQuery;
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};

/// P0-8: inclusive line-range overlap. Zero-valued ranges mean "unknown"
/// and never overlap anything.
fn line_ranges_overlap(a_start: u32, a_end: u32, b_start: u32, b_end: u32) -> bool {
    if (a_start == 0 && a_end == 0) || (b_start == 0 && b_end == 0) {
        return false;
    }
    a_start <= b_end && b_start <= a_end
}

/// Compact symbol facts attached to search hits: (name, node_type, node_id,
/// start_line, end_line).
type SymbolSpan = (String, String, String, u32, u32);

impl Engram {
    /// P0-8: bridge search → graph. For the final hit list, fetch each
    /// distinct file's symbol nodes in one blocking hop so hits can carry
    /// `symbols:` lines with node_ids agents feed straight into graph tools.
    pub(crate) async fn symbols_for_hits(
        &self,
        project_id: &str,
        hits: &[engram_index::HybridHit],
    ) -> std::collections::HashMap<String, Vec<SymbolSpan>> {
        let mut paths: Vec<String> = hits.iter().map(|h| h.path.as_str().to_string()).collect();
        paths.sort();
        paths.dedup();
        if paths.is_empty() {
            return Default::default();
        }
        let graph = self.state.graph.clone();
        let pid = project_id.to_string();
        tokio::task::spawn_blocking(move || {
            let mut map: std::collections::HashMap<String, Vec<SymbolSpan>> = Default::default();
            for path in paths {
                let nodes = graph
                    .query_nodes(&pid, None, None, Some(&path), 200)
                    .unwrap_or_default();
                let syms: Vec<SymbolSpan> = nodes
                    .into_iter()
                    .filter(|n| n.node_type != "file")
                    .map(|n| (n.name, n.node_type, n.node_id, n.start_line, n.end_line))
                    .collect();
                if !syms.is_empty() {
                    map.insert(path, syms);
                }
            }
            map
        })
        .await
        .unwrap_or_default()
    }
    pub async fn handle_search_memory(
        &self,
        req: SearchMemoryRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;

        // VEC1/D1: check if a full reindex is pending — surface degraded-mode
        // warning so callers know semantic search quality may be reduced.
        let reindex_since_ms: Option<u64> = {
            let reg = self.state.registry.clone();
            let pid = req.project_id.clone();
            tokio::task::spawn_blocking(move || {
                reg.get_project(&pid)
                    .ok()
                    .flatten()
                    .and_then(|r| r.reindex_required_since_ms)
            })
            .await
            .unwrap_or(None)
        };

        // 1. Fetch PageRank centrality for boosting (project-wide).
        //
        // Fix 5: Running compute_pagerank synchronously on every search request
        // can block Tokio's thread pool for seconds on large graphs and cause a
        // denial-of-service cascade. We instead maintain a short-lived in-memory
        // TTL cache keyed by (project_id, generation).
        //
        // Hot path: cache hit → zero I/O, zero blocking.
        // Cold path: cache miss → current request degrades gracefully (no boost),
        //   and a background task populates the cache for subsequent searches.
        const PAGERANK_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(300);
        let cache_key = format!("{}:{}", req.project_id, gen_);

        let centrality: Option<std::sync::Arc<engram_graph::analysis::CentralityMetrics>> = {
            if let Some(entry) = self.state.pagerank_cache.get(&cache_key) {
                if entry.value().0.elapsed() < PAGERANK_CACHE_TTL {
                    Some(entry.value().1.clone())
                } else {
                    // Stale: evict and trigger a background refresh below.
                    drop(entry);
                    self.state.pagerank_cache.remove(&cache_key);
                    None
                }
            } else {
                None
            }
        };

        // On cache miss launch a background task so the next search gets the
        // boost. Guard with `pagerank_inflight` so concurrent misses only ever
        // spawn one background task per (project_id, generation) key.
        if centrality.is_none() && self.state.pagerank_inflight.insert(cache_key.clone()) {
            let graph_bg = self.state.graph.clone();
            let pid_bg = req.project_id.clone();
            let gen_bg = gen_;
            let cache_bg = self.state.pagerank_cache.clone();
            let inflight_bg = self.state.pagerank_inflight.clone();
            let key_bg = cache_key;
            tokio::spawn(async move {
                if let Ok(Ok(metrics)) = tokio::task::spawn_blocking(move || {
                    engram_graph::analysis::compute_pagerank(&graph_bg, &pid_bg, gen_bg)
                })
                .await
                {
                    cache_bg.entry(key_bg.clone()).or_insert_with(|| {
                        (std::time::Instant::now(), std::sync::Arc::new(metrics))
                    });
                }
                inflight_bg.remove(&key_bg);
            });
        }

        // 2. Perform Hybrid Search — with a fast-path for literal
        //    queries. When `semantic: false` is set, we skip vector
        //    search, RRF, and MMR entirely and return pure FTS
        //    results ranked by BM25. This is what you want when the
        //    query is a literal identifier like `SubmitChanges()` and
        //    you just need to know where it lives — the semantic
        //    pipeline's vector embedding + fusion is pure overhead
        //    for that case.
        // Pagination: rank offset+max_results and slice the page out below.
        // Ranking is deterministic per generation, so pages don't overlap.
        let offset = req.sanitized_offset();
        let hybrid_q = HybridQuery {
            project_id: req.project_id.clone(),
            namespace: req.namespace.clone(),
            generation: gen_,
            text: req.query.clone(),
            top_k: req.sanitized_max_results() + offset,
            fts_mode: req.fts_mode.as_str().to_owned(),
            include_path_prefixes: req.include_path_prefixes.clone(),
            exclude_path_prefixes: req.exclude_path_prefixes.clone(),
            language_filters: req.language_filters.clone(),
            author_filter: None,
            date_after: None,
            date_before: None,
            use_mmr: req.use_mmr,
        };
        let hits = if req.semantic {
            ps.search
                .search(
                    &hybrid_q,
                    // `centrality` is `Option<Arc<CentralityMetrics>>`; deref
                    // through the Arc to obtain `Option<&CentralityMetrics>`.
                    centrality.as_deref().map(|c| &c.pagerank),
                    &tokio_util::sync::CancellationToken::new(),
                )
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
        } else {
            // Fast path: pure FTS via the existing `lexical_search`
            // entry point. No vector embedding, no RRF, no MMR.
            let engine = ps.search.clone();
            let q_clone = hybrid_q.clone();
            tokio::task::spawn_blocking(move || engine.lexical_search(&q_clone))
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
        };

        // Slice out the requested page. Done before the dreamer feed so
        // co-occurrence edges reflect what the agent actually saw.
        let hits: Vec<_> = hits.into_iter().skip(offset).collect();

        // Feed the dreamer co-occurrence graph (non-blocking).
        let lite: Vec<SearchHitLite> = hits
            .iter()
            .map(|h| SearchHitLite {
                pk: h.pk.clone(),
                doc_id: h.doc_id.clone(),
                path: h.path.clone(),
                chunk_id: Some(h.chunk_id),
            })
            .collect();
        let _ = self.state.events_tx.send(AppEvent::SearchSession {
            project_id: req.project_id.clone(),
            hits: lite,
        });

        if hits.is_empty() {
            // P0-7: an empty result with no guidance burns agent turns.
            let msg = if offset > 0 {
                format!(
                    "result: no_hits (offset {offset} is past the end of the ranked results)\n\
                     hints: lower offset, or re-run with offset=0 to see the first page."
                )
            } else {
                "result: no_hits\n\
                 hints: try fts_mode=\"loose\" (OR-of-terms) or a shorter query; \
                 check the namespace (default \"memory\"); call get_index_freshness \
                 to verify the index is current; grep_project finds exact literals."
                    .to_string()
            };
            return Ok(CallToolResult::success(vec![Content::text(msg)]));
        }

        let mut out = String::new();
        // VEC1/D1: prepend degraded-mode warning when reindex is pending.
        if let Some(since_ms) = reindex_since_ms {
            out.push_str(&format!(
                "WARNING: semantic search quality degraded — vector table was recreated at {}ms; full reindex required. Results may be incomplete.\n\n",
                since_ms
            ));
        }
        // P0-2: be honest about what the vector half of this search actually is.
        if req.semantic {
            match ps.search.semantic_quality() {
                engram_index::SemanticQuality::DegradedTrigram => out.push_str(
                    "NOTE: vector half of this search uses a non-semantic trigram-projection \
                     embedder (default install). Lexical quality is unaffected. For true \
                     semantic search set embedding_backend=ollama|openai in engram_mcp.yaml.\n",
                ),
                engram_index::SemanticQuality::Off => out.push_str(
                    "NOTE: vector search disabled (embedding_backend=fts_only) — results are \
                     lexical-only.\n",
                ),
                engram_index::SemanticQuality::Semantic => {}
            }
        }
        out.push_str(&format!("active_generation: {gen_}\n"));
        let symbols_by_path = self.symbols_for_hits(&req.project_id, &hits).await;
        for (i, h) in hits.iter().enumerate() {
            out.push_str(&format!(
                "\n#{}\ndoc_id: {}\nchunk_id: {}\npath: {}\nlines: {}-{}\nscore: {:.3}\n",
                offset + i + 1,
                h.doc_id,
                h.chunk_id,
                h.path,
                h.start_line,
                h.end_line,
                h.score
            ));

            // P0-8: name the symbols this chunk covers, with node_ids usable
            // in find_symbol_references / compute_blast_radius / resolve_id.
            if let Some(syms) = symbols_by_path.get(h.path.as_str()) {
                let overlapping: Vec<String> = syms
                    .iter()
                    .filter(|(_, _, _, s, e)| line_ranges_overlap(h.start_line, h.end_line, *s, *e))
                    .take(3)
                    .map(|(name, ty, id, _, _)| format!("{name} ({ty}) node_id={id}"))
                    .collect();
                if !overlapping.is_empty() {
                    out.push_str(&format!("symbols: {}\n", overlapping.join("; ")));
                }
            }

            if req.include_content {
                if let Ok(Some((_, _, content, _, _))) =
                    ps.search
                        .get_doc_by_doc_id(&req.project_id, &req.namespace, gen_, &h.doc_id)
                {
                    out.push_str("content:\n");
                    let limit = req.sanitized_max_content_chars_per_result();
                    if content.chars().count() > limit {
                        out.push_str(&content.chars().take(limit).collect::<String>());
                        out.push_str(&format!(
                            "... [truncated at {limit} chars — call get_chunk(doc_id) for the full chunk]"
                        ));
                    } else {
                        out.push_str(&content);
                    }
                    out.push('\n');
                }
            } else if let Some(sn) = &h.snippet {
                out.push_str("snippet:\n");
                out.push_str(sn);
                if h.snippet_truncated {
                    out.push_str(
                        " ... [snippet truncated — call get_chunk(doc_id) for the full chunk]",
                    );
                }
                out.push('\n');
            }
        }

        out.push_str(&format!(
            "next: get_chunk(doc_id) for full source; resolve_id(<symbol>) to enter \
             the graph; get_concept_footprint(<domain term>) for ALL touchpoints; \
             offset={} for the next page.\n",
            offset + hits.len()
        ));
        out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_vector_search(
        &self,
        req: VectorSearchRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;
        let top_k = req.sanitized_top_k();
        let max_chars = req.sanitized_max_content_chars();
        let timeout_ms = self.state.cfg.vector_search_timeout_ms;

        let q = HybridQuery {
            project_id: req.project_id.clone(),
            namespace: req.namespace.clone(),
            generation: gen_,
            text: req.query.clone(),
            top_k,
            fts_mode: String::new(), // unused by vector path
            include_path_prefixes: req.include_path_prefixes.clone(),
            exclude_path_prefixes: req.exclude_path_prefixes.clone(),
            language_filters: req.language_filters.clone(),
            author_filter: None,
            date_after: None,
            date_before: None,
            use_mmr: req.use_mmr,
        };

        let hits = ps
            .search
            .pure_vector_search(&q, timeout_ms, &tokio_util::sync::CancellationToken::new())
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if hits.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No vector search results. Ensure the project is indexed with a vector-capable embedding backend (not fts_only).",
            )]));
        }

        let mut out = String::with_capacity(4096);
        // P0-2: a "pure semantic" tool built on the trigram stub is misleading
        // without this label.
        if ps.search.semantic_quality() == engram_index::SemanticQuality::DegradedTrigram {
            out.push_str(
                "NOTE: this install uses the non-semantic trigram-projection embedder \
                 (default). Similarity scores reflect character-trigram overlap, not \
                 meaning. Set embedding_backend=ollama|openai in engram_mcp.yaml for \
                 true semantic search.\n\n",
            );
        }
        out.push_str(&format!(
            "Vector search results (namespace={}, top_k={}, mmr={}):\n\n",
            req.namespace, top_k, req.use_mmr
        ));

        for (i, h) in hits.iter().enumerate() {
            // doc_id is what get_chunk takes. The truncation notice below has
            // always told callers to use it, but the result line never carried
            // one — so every hit was a dead end.
            out.push_str(&format!(
                "[{}] similarity={:.4} path={} lines={}-{} doc_id={} chunk_id={}\n",
                i + 1,
                h.score,
                h.path,
                h.start_line,
                h.end_line,
                h.doc_id,
                h.chunk_id
            ));

            if req.include_content
                && let Ok(Some((_, _, content, _, _))) = ps.search.get_doc_by_pk(&h.pk)
            {
                if content.chars().count() > max_chars {
                    out.push_str(&content.chars().take(max_chars).collect::<String>());
                    out.push_str(&format!(
                        "... [truncated at {max_chars} chars — call get_chunk(doc_id) for the full chunk]\n"
                    ));
                } else {
                    out.push_str(&content);
                    out.push('\n');
                }
            }
        }

        out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_get_chunk(&self, req: GetChunkRequest) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;

        let doc = ps
            .search
            .get_doc_by_doc_id(&req.project_id, &req.namespace, gen_, &req.doc_id)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let Some((path, lang, content, start_line, end_line)) = doc else {
            return Err(McpError::invalid_params(
                format!(
                    "doc_id '{}' not found in project '{}' (namespace '{}', generation {}). \
                     doc_ids are generation-scoped — re-run search_memory to get current ones.",
                    req.doc_id, req.project_id, req.namespace, gen_
                ),
                None,
            ));
        };

        // Fix 7: Apply the logical slice FIRST on the syntactically valid source
        // before any rule injection. Rule injection prepends `[Repo Constraint]:
        // …` lines that are not valid syntax in Rust, JS, Python, etc. Passing
        // that mutated string to Tree-sitter makes the AST-based slicer fail.
        // Rules are semantic annotations and must be layered *after* parsing.
        let mut display_content: String = if let Some(ref slice_type) = req.logical_slice
            && slice_type != "all"
            && !slice_type.is_empty()
        {
            crate::services::slice_service::apply_logical_slice(&content, slice_type, &lang)
        } else {
            content.to_string()
        };

        // Inject repo rules (if requested) onto the already-sliced output.
        if req.inject_rules {
            display_content = self
                .inject_repo_rules(&req.project_id, &path, &display_content)
                .await;
        }

        // Compute confidence footer for WebForms files.
        let confidence_footer = self.confidence_footer(&path, &lang);

        let mut output = format!(
            "path: {}\ndoc_id: {}\nnamespace: {}\nlanguage: {}\nlines: {}-{}\nactive_generation: {}\n\n{}",
            path, req.doc_id, req.namespace, lang, start_line, end_line, gen_, display_content
        );
        output.push_str(&confidence_footer);
        output.push_str(&self.freshness_footer(&req.project_id, gen_).await);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    pub async fn handle_find_symbol_references(
        &self,
        req: FindSymbolReferencesRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let max_incoming = req.sanitized_max_incoming();
        let max_outgoing_per_kind = req.sanitized_max_outgoing_per_kind();
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;
        let needle = &req.symbol_name;

        // Parse edge kind filter (pure computation, no I/O).
        let edge_kind_filter: Option<Vec<EdgeKind>> = req
            .edge_kind_filter
            .as_ref()
            .map(|f| f.iter().filter_map(|s| EdgeKind::parse(s)).collect());

        // Fix 6: All GraphStore operations are synchronous Redb reads that block
        // the calling OS thread.  Move every graph query for this symbol into a
        // single `spawn_blocking` so Tokio's async executor is never stalled.
        //
        // Fix 8: The previous code had
        //   `let incoming_kind_filter = edge_kind_filter.as_ref().map(|_| ()).and(None);`
        // which *always* evaluates to `None` regardless of the filter value, so
        // `find_incoming_edges_with_kind` always returned all-kind edges.  The
        // post-query `.retain` then yielded 0 results if the fetched edges didn't
        // happen to include the requested kinds within the limit window.
        //
        // The correct approach: always fetch with `kind = None` (all kinds) but
        // over-fetch proportionally to the number of requested kinds so the
        // post-query retain has enough candidates.  Then truncate to `max_incoming`.

        // Determine the incoming over-fetch limit.
        let incoming_fetch_limit = match &edge_kind_filter {
            Some(f) if !f.is_empty() => max_incoming
                .saturating_mul(f.len())
                .min(max_incoming.saturating_mul(EdgeKind::ALL.len())),
            _ => max_incoming,
        };

        // Build the outgoing kind list as an owned Vec so it can cross the
        // spawn_blocking boundary (EdgeKind::ALL is &'static but a filter Vec<_>
        // is not; we unify them here).
        let outgoing_kinds_owned: Vec<EdgeKind> = edge_kind_filter
            .clone()
            .unwrap_or_else(|| EdgeKind::ALL.to_vec());

        // Minimal per-node result type that uses only owned, Send-safe values.
        struct NodeGraphResult {
            name: String,
            node_type: String,
            file_path: String, // RelPath rendered to string
            node_id: String,
            incoming: Vec<(String, EdgeKind, u32)>,
            outgoing: Vec<(String, EdgeKind, u32)>,
            /// (source_id \0 kind) → call-site line, from edge metadata.
            call_lines: std::collections::HashMap<String, u32>,
        }

        let graph_b = self.state.graph.clone();
        let project_id_b = req.project_id.clone();
        let needle_b = needle.to_string();
        let ekf_b = edge_kind_filter.clone();
        let file_scope_b = req.file_scope.clone();

        // Execute all blocking graph I/O in one dedicated OS thread.
        type EndpointLabels = std::collections::HashMap<String, String>;
        let (graph_results, endpoint_labels, found_in_graph): (
            Vec<NodeGraphResult>,
            EndpointLabels,
            bool,
        ) = tokio::task::spawn_blocking(move || {
            // The match ladder (exact / `.`-suffix / `::`-suffix / node-id
            // suffix) and the result cap now live TOGETHER in the store.
            // They have to: this call previously asked for 50 *substring*
            // matches and filtered them here, so the cap applied before the
            // filter and 50 neighbours like `FuzzWorkerProtocol` could crowd
            // out the exact `Worker` node — the tool answered "No graph
            // symbol found" for a node `resolve_id` confirmed existed and
            // told the agent to look up this very way.
            //
            // Do NOT reintroduce a post-hoc name filter here. A second copy
            // of the rule is precisely how the ProjectType serde alias list
            // drifted from `from_registry_str`.
            let nodes = graph_b
                .query_nodes_by_symbol_name(&project_id_b, &needle_b, file_scope_b.as_deref(), 50)
                .unwrap_or_default();

            let mut results: Vec<NodeGraphResult> = Vec::new();

            for node in nodes {
                // File scope filter on the node itself.
                let fp_str = node.file_path.as_str().to_string();
                if let Some(ref scope) = file_scope_b
                    && !fp_str.is_empty()
                    && !fp_str.starts_with(scope.as_str())
                {
                    continue;
                }

                // Fix 8: Fetch all incoming kinds unconditionally; the
                // post-query retain handles kind-level filtering.  Over-fetch
                // to ensure enough candidates survive the retain step.
                let mut incoming = graph_b
                    .find_incoming_edges_with_kind(
                        &project_id_b,
                        None, // always fetch all kinds
                        &node.node_id,
                        incoming_fetch_limit,
                    )
                    .unwrap_or_default();

                if let Some(ref filter) = ekf_b {
                    incoming.retain(|(_, kind, _)| filter.contains(kind));
                }
                incoming.truncate(max_incoming);

                if let Some(ref scope) = file_scope_b {
                    incoming.retain(|(src_id, _, _)| src_id.contains(scope.as_str()));
                }

                // Outgoing edges for each requested kind.
                let mut outgoing: Vec<(String, EdgeKind, u32)> = Vec::new();
                for kind in &outgoing_kinds_owned {
                    if let Ok(neighbors) = graph_b.neighbors(
                        &project_id_b,
                        kind.clone(),
                        &node.node_id,
                        max_outgoing_per_kind,
                    ) {
                        for (target_id, weight) in neighbors {
                            if let Some(ref scope) = file_scope_b
                                && !target_id.contains(scope.as_str())
                            {
                                continue;
                            }
                            outgoing.push((target_id, kind.clone(), weight));
                        }
                    }
                }

                if !incoming.is_empty() || !outgoing.is_empty() {
                    // Call-site anchors: the adjacency tables carry no
                    // metadata, so fetch the full edges once (O(degree))
                    // and lift each edge's `src_line` for rendering.
                    let mut call_lines: std::collections::HashMap<String, u32> =
                        std::collections::HashMap::new();
                    if let Ok(full_edges) =
                        graph_b.edges_touching(&project_id_b, &node.node_id, 1000)
                    {
                        for e in full_edges {
                            if e.target_id != node.node_id {
                                continue;
                            }
                            if let Some(line) = e
                                .metadata
                                .as_ref()
                                .and_then(|m| m.get("src_line"))
                                .and_then(|v| v.as_str())
                                .and_then(|s| s.parse::<u32>().ok())
                            {
                                call_lines.insert(
                                    format!("{}\0{}", e.source_id, e.edge_kind.as_str()),
                                    line,
                                );
                            }
                        }
                    }
                    results.push(NodeGraphResult {
                        name: node.name,
                        node_type: node.node_type,
                        file_path: fp_str,
                        node_id: node.node_id,
                        incoming,
                        outgoing,
                        call_lines,
                    });
                }
            }

            // Resolve displayed endpoints to "name (file:line)" labels so
            // callers don't need a second lookup per reference (bounded).
            let mut labels: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            for nr in &results {
                for (id, _, _) in nr.incoming.iter().chain(nr.outgoing.iter()) {
                    if labels.len() >= 400 || labels.contains_key(id) {
                        continue;
                    }
                    if let Ok(Some(n)) = graph_b.get_node(&project_id_b, id) {
                        let loc = if n.file_path.as_str().is_empty() {
                            String::new()
                        } else {
                            format!(" ({}:{})", n.file_path, n.start_line)
                        };
                        labels.insert(id.clone(), format!("{}{}", n.name, loc));
                    }
                }
            }

            let found = !results.is_empty();
            (results, labels, found)
        })
        .await
        .unwrap_or((Vec::new(), std::collections::HashMap::new(), false));

        // Format output from the non-blocking data collected above.
        let mut out = String::with_capacity(4096);

        // Same-name collisions are the norm in legacy WebForms (every page
        // has a Page_Load). Say up front how many DISTINCT symbols matched
        // so the agent scrolls with intent instead of assuming one match.
        if graph_results.len() > 1 {
            out.push_str(&format!(
                "⚠ {} distinct symbols match \"{}\" — results for each below. \
                 Narrow with file_scope, or use the node_id of the one you mean.\n\n",
                graph_results.len(),
                req.symbol_name
            ));
        }

        for nr in &graph_results {
            out.push_str(&format!(
                "Symbol: {} ({}) in {}\n  node_id: {}\n",
                nr.name, nr.node_type, nr.file_path, nr.node_id
            ));

            if !nr.incoming.is_empty() {
                out.push_str(&format!("  Incoming references ({}):\n", nr.incoming.len()));
                let mut by_kind: std::collections::HashMap<String, Vec<(&str, u32)>> =
                    std::collections::HashMap::new();
                for (src_id, kind, weight) in &nr.incoming {
                    by_kind
                        .entry(kind.as_str().to_string())
                        .or_default()
                        .push((src_id.as_str(), *weight));
                }
                let mut kinds_sorted: Vec<_> = by_kind.into_iter().collect();
                kinds_sorted.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
                for (kind, refs) in &kinds_sorted {
                    out.push_str(&format!("    [{}] ({}):\n", kind, refs.len()));
                    for (src, w) in refs.iter().take(20) {
                        // Call-SITE line (where the reference occurs), when
                        // the edge recorded one — distinct from the caller's
                        // declaration line in the label.
                        // Structural containment is not a call site — the
                        // recorded line echoes the declaration and reads as
                        // noise (@L1 on file-level edges).
                        let at = if *kind == "contains" {
                            String::new()
                        } else {
                            nr.call_lines
                                .get(&format!("{src}\0{kind}"))
                                .map(|l| format!(" @L{l}"))
                                .unwrap_or_default()
                        };
                        match endpoint_labels.get(*src) {
                            Some(label) => {
                                out.push_str(&format!("      <- {label}{at} [{src}] (w={w})\n"))
                            }
                            None => out.push_str(&format!("      <- {src}{at} (w={w})\n")),
                        }
                    }
                    if refs.len() > 20 {
                        out.push_str(&format!("      ... and {} more\n", refs.len() - 20));
                    }
                }
            }

            if !nr.outgoing.is_empty() {
                out.push_str(&format!(
                    "  Outgoing dependencies ({}):\n",
                    nr.outgoing.len()
                ));
                let mut by_kind: std::collections::HashMap<String, Vec<(&str, u32)>> =
                    std::collections::HashMap::new();
                for (tgt_id, kind, weight) in &nr.outgoing {
                    by_kind
                        .entry(kind.as_str().to_string())
                        .or_default()
                        .push((tgt_id.as_str(), *weight));
                }
                let mut kinds_sorted: Vec<_> = by_kind.into_iter().collect();
                kinds_sorted.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
                for (kind, refs) in &kinds_sorted {
                    out.push_str(&format!("    [{}] ({}):\n", kind, refs.len()));
                    for (tgt, w) in refs.iter().take(20) {
                        match endpoint_labels.get(*tgt) {
                            Some(label) => {
                                out.push_str(&format!("      -> {label} [{tgt}] (w={w})\n"))
                            }
                            None => out.push_str(&format!("      -> {tgt} (w={w})\n")),
                        }
                    }
                    if refs.len() > 20 {
                        out.push_str(&format!("      ... and {} more\n", refs.len() - 20));
                    }
                }
            }
            out.push('\n');
        }

        if found_in_graph {
            // The next-step hint used to be appended to the WRONG buffer
            // (`out`, after `text` was snapshotted) — composed, then thrown
            // away. Agents never saw the pointer into the verification loop.
            let mut text = out.trim().to_string();
            text.push_str(
                "\nnext: detect_incomplete_changes(edited_files=[...]) if you plan to edit \
             these files; check_edit_safety(<method>) before changing the symbol.\n",
            );
            text.push_str(&self.freshness_footer(&req.project_id, gen_).await);
            return Ok(CallToolResult::success(vec![Content::text(text)]));
        }

        // 2. Fallback: Lexical search (deduplicated — only runs if graph found nothing)
        let lexical_path_filter = req.file_scope.map(|s| vec![s]);
        let hits = ps
            .search
            .search(
                &HybridQuery {
                    project_id: req.project_id.clone(),
                    namespace: "memory".into(),
                    generation: gen_,
                    text: req.symbol_name.clone(),
                    top_k: 20,
                    fts_mode: "strict".into(),
                    include_path_prefixes: lexical_path_filter,
                    exclude_path_prefixes: None,
                    language_filters: None,
                    author_filter: None,
                    date_after: None,
                    date_before: None,
                    use_mmr: false,
                },
                None,
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if hits.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No references found (graph and lexical search both empty). \
                 hints: check spelling/casing of the symbol; try search_memory with \
                 fts_mode=\"loose\"; call get_index_freshness if the symbol was \
                 added recently.",
            )]));
        }

        let mut out = String::new();
        out.push_str(&format!(
            "No graph symbol found for '{}'; lexical references:\n",
            req.symbol_name
        ));
        for h in hits {
            out.push_str(&format!(
                "- {} lines {}-{} (chunk_id={}, score={:.3})\n",
                h.path, h.start_line, h.end_line, h.chunk_id, h.score
            ));
        }
        let mut text = out.trim().to_string();
        text.push_str(&self.freshness_footer(&req.project_id, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    pub async fn handle_analyze_error_stack(
        &self,
        req: AnalyzeErrorStackRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;

        // 1. Structured parsing of stack frames
        let frames = crate::utils::text::parse_stack_frames(&req.traceback);
        let query = stacktrace_to_query(&req.traceback);

        // 2. Hybrid search for initial candidates, using MMR for diversity.
        let hits = ps
            .search
            .search(
                &HybridQuery {
                    project_id: req.project_id.clone(),
                    namespace: "memory".into(),
                    generation: gen_,
                    text: query,
                    top_k: 15,
                    fts_mode: "loose".into(),
                    include_path_prefixes: None,
                    exclude_path_prefixes: None,
                    language_filters: None,
                    author_filter: None,
                    date_after: None,
                    date_before: None,
                    use_mmr: true,
                },
                None,
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let mut out = String::with_capacity(4096);
        out.push_str("Error Stacktrace Analysis\n");
        out.push_str(&format!("Frames parsed: {}\n\n", frames.len()));

        // 3. Show extracted frames summary
        if !frames.is_empty() {
            out.push_str("--- Extracted Frames ---\n");
            for (i, f) in frames.iter().enumerate().take(15) {
                let mut parts = Vec::new();
                if let Some(ref file) = f.file {
                    let basename = file.rsplit(['/', '\\']).next().unwrap_or(file);
                    if let Some(line) = f.line {
                        parts.push(format!("{}:{}", basename, line));
                    } else {
                        parts.push(basename.to_string());
                    }
                }
                if let Some(ref fqn) = f.fqn {
                    parts.push(fqn.clone());
                } else if let Some(ref func) = f.function {
                    parts.push(func.clone());
                }
                if !parts.is_empty() {
                    out.push_str(&format!("  #{}: {}\n", i + 1, parts.join(" in ")));
                }
            }
            if frames.len() > 15 {
                out.push_str(&format!("  ... and {} more frames\n", frames.len() - 15));
            }
            out.push('\n');
        }

        if hits.is_empty() {
            out.push_str("No matching codebase files found.\n");
            return Ok(CallToolResult::success(vec![Content::text(
                out.trim().to_string(),
            )]));
        }

        // 4. Boost hits that match extracted file paths from frames
        let frame_files: std::collections::HashSet<String> = frames
            .iter()
            .filter_map(|f| f.file.as_deref())
            .map(|f| f.rsplit(['/', '\\']).next().unwrap_or(f).to_lowercase())
            .collect();
        let frame_functions: std::collections::HashSet<String> = frames
            .iter()
            .filter_map(|f| f.function.as_deref())
            .map(|s| s.to_lowercase())
            .collect();

        let mut scored_hits: Vec<_> = hits
            .iter()
            .map(|h| {
                let basename = h
                    .path
                    .as_str()
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or(h.path.as_str())
                    .to_lowercase();
                let mut bonus = 0.0f32;
                // Exact file match from stack frame
                if frame_files.contains(&basename) {
                    bonus += 0.3;
                }
                // Centrality bonus
                bonus += h.centrality * 0.1;
                (h, h.score + bonus)
            })
            .collect();
        scored_hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // 5. Output ranked results
        out.push_str("--- Likely Source Files ---\n");
        out.push_str(
            "Ranked by search relevance + stack frame matching + architectural centrality.\n\n",
        );

        for (i, (h, final_score)) in scored_hits.iter().enumerate().take(8) {
            let centrality_note = if h.centrality > 0.5 {
                " [Hub]"
            } else if h.centrality > 0.2 {
                " [Utility]"
            } else {
                ""
            };

            let basename = h
                .path
                .as_str()
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(h.path.as_str())
                .to_lowercase();
            let frame_match = if frame_files.contains(&basename) {
                " [STACK MATCH]"
            } else {
                ""
            };

            // Check if any frame function matches a graph node in this file
            let mut func_matches = Vec::new();
            if !frame_functions.is_empty() {
                let file_nodes = self
                    .state
                    .graph
                    .query_nodes(
                        &req.project_id,
                        Some("function"),
                        None,
                        Some(h.path.as_str()),
                        50,
                    )
                    .unwrap_or_default();
                for node in &file_nodes {
                    let node_name_lower = node.name.to_lowercase();
                    if frame_functions.contains(&node_name_lower) {
                        func_matches.push(format!("{}:{}", node.name, node.start_line));
                    }
                }
            }

            out.push_str(&format!(
                "#{}: {}{}{} (score: {:.3})\n",
                i + 1,
                h.path,
                centrality_note,
                frame_match,
                final_score
            ));

            if !func_matches.is_empty() {
                out.push_str(&format!(
                    "   Matching functions: {}\n",
                    func_matches.join(", ")
                ));
            }

            if let Ok(Some((_, _, content, start_line, _))) =
                ps.search
                    .get_doc_by_doc_id(&req.project_id, "memory", gen_, &h.doc_id)
            {
                let snippet: String = content.lines().take(3).collect::<Vec<_>>().join("\n");
                out.push_str(&format!("   (line ~{})\n", start_line));
                out.push_str("   > ");
                out.push_str(&snippet.replace('\n', "\n   > "));
                out.push_str("\n\n");
            }
        }

        Ok(CallToolResult::success(vec![Content::text(
            out.trim().to_string(),
        )]))
    }
}

#[cfg(test)]
mod p0_line_overlap_tests {
    use super::line_ranges_overlap;

    #[test]
    fn overlapping_and_nested_ranges_overlap() {
        assert!(line_ranges_overlap(10, 20, 15, 25)); // partial
        assert!(line_ranges_overlap(10, 20, 12, 14)); // nested
        assert!(line_ranges_overlap(10, 20, 20, 30)); // touching edge (inclusive)
        assert!(line_ranges_overlap(7, 7, 7, 7)); // single-line identity
    }

    #[test]
    fn disjoint_ranges_do_not_overlap() {
        assert!(!line_ranges_overlap(10, 20, 21, 30));
        assert!(!line_ranges_overlap(21, 30, 10, 20));
    }

    #[test]
    fn unknown_zero_ranges_never_overlap() {
        assert!(!line_ranges_overlap(0, 0, 10, 20));
        assert!(!line_ranges_overlap(10, 20, 0, 0));
        assert!(!line_ranges_overlap(0, 0, 0, 0));
    }
}
