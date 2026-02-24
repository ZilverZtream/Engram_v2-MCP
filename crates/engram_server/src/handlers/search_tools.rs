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

impl Engram {
    pub async fn handle_search_memory(
        &self,
        req: SearchMemoryRequest,
    ) -> Result<CallToolResult, McpError> {
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;

        // 1. Fetch PageRank centrality for boosting (project-wide)
        let graph = self.state.graph.clone();
        let pid_for_centrality = req.project_id.clone();
        let active_gen = gen_;
        let centrality = tokio::task::spawn_blocking(move || {
            engram_graph::analysis::compute_pagerank(&graph, &pid_for_centrality, active_gen)
        })
        .await
        .ok()
        .and_then(|r| r.ok());

        // 2. Perform Hybrid Search with Boost
        let hits = ps
            .search
            .search(
                &HybridQuery {
                    project_id: req.project_id.clone(),
                    namespace: req.namespace.clone(),
                    generation: gen_,
                    text: req.query.clone(),
                    top_k: req.sanitized_max_results(),
                    fts_mode: req.fts_mode.clone(),
                    include_path_prefixes: req.include_path_prefixes.clone(),
                    exclude_path_prefixes: req.exclude_path_prefixes.clone(),
                    language_filters: req.language_filters.clone(),
                    author_filter: None,
                    date_after: None,
                    date_before: None,
                    use_mmr: req.use_mmr,
                },
                centrality.as_ref().map(|c| &c.pagerank),
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

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
            return Ok(CallToolResult::success(vec![Content::text("No hits.")]));
        }

        let mut out = String::new();
        out.push_str(&format!("active_generation: {gen_}\n"));
        for (i, h) in hits.iter().enumerate() {
            out.push_str(&format!(
                "\n#{}\ndoc_id: {}\nchunk_id: {}\npath: {}\nscore: {:.3}\n",
                i + 1,
                h.doc_id,
                h.chunk_id,
                h.path,
                h.score
            ));

            if req.include_content {
                if let Ok(Some((_, _, content, _, _))) =
                    ps.search
                        .get_doc_by_doc_id(&req.project_id, &req.namespace, gen_, &h.doc_id)
                {
                    out.push_str("content:\n");
                    let limit = req.sanitized_max_content_chars_per_result();
                    if content.chars().count() > limit {
                        out.push_str(&content.chars().take(limit).collect::<String>());
                        out.push_str("... (truncated)");
                    } else {
                        out.push_str(&content);
                    }
                    out.push('\n');
                }
            } else if let Some(sn) = &h.snippet {
                out.push_str("snippet:\n");
                out.push_str(sn);
                out.push('\n');
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_vector_search(
        &self,
        req: VectorSearchRequest,
    ) -> Result<CallToolResult, McpError> {
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
            .pure_vector_search(&q, timeout_ms)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if hits.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No vector search results. Ensure the project is indexed with a vector-capable embedding backend (not fts_only).",
            )]));
        }

        let mut out = String::with_capacity(4096);
        out.push_str(&format!(
            "Vector search results (namespace={}, top_k={}, mmr={}):\n\n",
            req.namespace, top_k, req.use_mmr
        ));

        for (i, h) in hits.iter().enumerate() {
            out.push_str(&format!(
                "[{}] similarity={:.4} path={} chunk_id={}\n",
                i + 1,
                h.score,
                h.path,
                h.chunk_id
            ));

            if req.include_content
                && let Ok(Some((_, _, content, _, _))) = ps.search.get_doc_by_pk(&h.pk) {
                    if content.chars().count() > max_chars {
                        out.push_str(&content.chars().take(max_chars).collect::<String>());
                        out.push_str("... (truncated)\n");
                    } else {
                        out.push_str(&content);
                        out.push('\n');
                    }
                }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_get_chunk(&self, req: GetChunkRequest) -> Result<CallToolResult, McpError> {
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;

        let doc = ps
            .search
            .get_doc_by_doc_id(&req.project_id, &req.namespace, gen_, &req.doc_id)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let Some((path, lang, content, start_line, end_line)) = doc else {
            return Ok(CallToolResult::success(vec![Content::text("Not found.")]));
        };

        // Inject repo rules if requested.
        let mut display_content = if req.inject_rules {
            self.inject_repo_rules(&req.project_id, &path, &content)
                .await
        } else {
            content.to_string()
        };

        // Apply logical slice if requested.
        if let Some(ref slice_type) = req.logical_slice
            && slice_type != "all" && !slice_type.is_empty() {
                display_content = crate::services::slice_service::apply_logical_slice(
                    &display_content,
                    slice_type,
                    &lang,
                );
            }

        // Compute confidence footer for WebForms files.
        let confidence_footer = self.confidence_footer(&path, &lang);

        let mut output = format!(
            "path: {}\ndoc_id: {}\nnamespace: {}\nlanguage: {}\nlines: {}-{}\nactive_generation: {}\n\n{}",
            path, req.doc_id, req.namespace, lang, start_line, end_line, gen_, display_content
        );
        output.push_str(&confidence_footer);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    pub async fn handle_find_symbol_references(
        &self,
        req: FindSymbolReferencesRequest,
    ) -> Result<CallToolResult, McpError> {
        let max_incoming = req.sanitized_max_incoming();
        let max_outgoing_per_kind = req.sanitized_max_outgoing_per_kind();
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;
        let needle = &req.symbol_name;

        // Parse edge kind filter
        let edge_kind_filter: Option<Vec<EdgeKind>> = req
            .edge_kind_filter
            .as_ref()
            .map(|f| f.iter().filter_map(|s| EdgeKind::parse(s)).collect());

        // 1. Find matching symbol nodes — exact name match and FQN suffix match
        let nodes = self
            .state
            .graph
            .query_nodes(&req.project_id, None, Some(needle), None, 50)
            .unwrap_or_default();

        let mut out = String::with_capacity(4096);
        let mut found_in_graph = false;

        for node in &nodes {
            // Multi-strategy name match: exact, FQN suffix, node-id suffix
            let name_lower = node.name.to_lowercase();
            let needle_lower = needle.to_lowercase();
            let name_matches = name_lower == needle_lower
                || name_lower.ends_with(&format!(".{}", needle_lower))
                || name_lower.ends_with(&format!("::{}", needle_lower))
                || node
                    .node_id
                    .to_lowercase()
                    .ends_with(&format!(":{}", needle_lower));
            if !name_matches {
                continue;
            }

            // Apply file scope filter
            if let Some(ref scope) = req.file_scope
                && !node.file_path.as_str().is_empty()
                    && !node.file_path.as_str().starts_with(scope.as_str())
                {
                    continue;
                }

            // Query incoming edge kinds (filtered if specified)
            let incoming_kind_filter = edge_kind_filter.as_ref().map(|_| ()).and(None);
            let mut incoming = self
                .state
                .graph
                .find_incoming_edges_with_kind(
                    &req.project_id,
                    incoming_kind_filter, // None = all kinds
                    &node.node_id,
                    max_incoming,
                )
                .unwrap_or_default();

            // Apply edge kind filter post-query if specified
            if let Some(ref filter) = edge_kind_filter {
                incoming.retain(|(_, kind, _)| filter.contains(kind));
            }

            // Apply file scope filter to incoming edges
            if let Some(ref scope) = req.file_scope {
                incoming.retain(|(src_id, _, _)| {
                    // src_id may be like "file:path" or "sym:type:path:name"
                    src_id.contains(scope.as_str())
                });
            }

            // Outgoing edges — filter to requested kinds only
            let outgoing_kinds: &[EdgeKind] = if let Some(ref filter) = edge_kind_filter {
                filter
            } else {
                EdgeKind::ALL
            };

            let mut outgoing: Vec<(String, EdgeKind, u32)> = Vec::new();
            for kind in outgoing_kinds {
                if let Ok(neighbors) = self.state.graph.neighbors(
                    &req.project_id,
                    kind.clone(),
                    &node.node_id,
                    max_outgoing_per_kind,
                ) {
                    for (target_id, weight) in neighbors {
                        // Apply file scope filter to outgoing
                        if let Some(ref scope) = req.file_scope
                            && !target_id.contains(scope.as_str()) {
                                continue;
                            }
                        outgoing.push((target_id, kind.clone(), weight));
                    }
                }
            }

            if !incoming.is_empty() || !outgoing.is_empty() {
                found_in_graph = true;
                out.push_str(&format!(
                    "Symbol: {} ({}) in {}\n  node_id: {}\n",
                    node.name, node.node_type, node.file_path, node.node_id
                ));

                if !incoming.is_empty() {
                    out.push_str(&format!("  Incoming references ({}):\n", incoming.len()));
                    let mut by_kind: std::collections::HashMap<String, Vec<(&str, u32)>> =
                        std::collections::HashMap::new();
                    for (src_id, kind, weight) in &incoming {
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
                            out.push_str(&format!("      <- {} (w={})\n", src, w));
                        }
                        if refs.len() > 20 {
                            out.push_str(&format!("      ... and {} more\n", refs.len() - 20));
                        }
                    }
                }

                if !outgoing.is_empty() {
                    out.push_str(&format!("  Outgoing dependencies ({}):\n", outgoing.len()));
                    let mut by_kind: std::collections::HashMap<String, Vec<(&str, u32)>> =
                        std::collections::HashMap::new();
                    for (tgt_id, kind, weight) in &outgoing {
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
                            out.push_str(&format!("      -> {} (w={})\n", tgt, w));
                        }
                        if refs.len() > 20 {
                            out.push_str(&format!("      ... and {} more\n", refs.len() - 20));
                        }
                    }
                }
                out.push('\n');
            }
        }

        if found_in_graph {
            return Ok(CallToolResult::success(vec![Content::text(
                out.trim().to_string(),
            )]));
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
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if hits.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No references found.",
            )]));
        }

        let mut out = String::new();
        out.push_str(&format!(
            "No graph symbol found for '{}'; lexical references:\n",
            req.symbol_name
        ));
        for h in hits {
            out.push_str(&format!(
                "- {} (chunk_id={}, score={:.3})\n",
                h.path, h.chunk_id, h.score
            ));
        }
        Ok(CallToolResult::success(vec![Content::text(
            out.trim().to_string(),
        )]))
    }

    pub async fn handle_analyze_error_stack(
        &self,
        req: AnalyzeErrorStackRequest,
    ) -> Result<CallToolResult, McpError> {
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
