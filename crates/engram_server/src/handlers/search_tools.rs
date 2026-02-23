use crate::models::{GetChunkRequest, SearchMemoryRequest, VectorSearchRequest};
use crate::state::{AppEvent, SearchHitLite};
use crate::tools::Engram;
use engram_index::HybridQuery;
use rmcp::model::{CallToolResult, Content};
use rmcp::ErrorData as McpError;

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

            if req.include_content {
                if let Ok(Some((_, _, content, _, _))) = ps.search.get_doc_by_pk(&h.pk) {
                    if content.chars().count() > max_chars {
                        out.push_str(&content.chars().take(max_chars).collect::<String>());
                        out.push_str("... (truncated)\n");
                    } else {
                        out.push_str(&content);
                        out.push('\n');
                    }
                }
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_get_chunk(
        &self,
        req: GetChunkRequest,
    ) -> Result<CallToolResult, McpError> {
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
        if let Some(ref slice_type) = req.logical_slice {
            if slice_type != "all" && !slice_type.is_empty() {
                display_content = crate::services::slice_service::apply_logical_slice(
                    &display_content,
                    slice_type,
                    &lang,
                );
            }
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
}
