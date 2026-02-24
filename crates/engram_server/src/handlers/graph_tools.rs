use crate::models::{
    FindReferencesRequest, GraphSearchRequest, QueryGraphNodesRequest, TraverseGraphRequest,
};
use crate::tools::Engram;
use engram_graph::EdgeKind;
use engram_index::HybridQuery;
use rmcp::{
    ErrorData as McpError,
    model::{CallToolResult, Content},
};

/// Graph tool helper methods on Engram.
impl Engram {
    pub async fn handle_query_graph_nodes(
        &self,
        req: QueryGraphNodesRequest,
    ) -> Result<CallToolResult, McpError> {
        let graph = self.state.graph.clone();
        let out = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let nodes = graph
                .query_nodes(
                    &req.project_id,
                    Some(&req.node_type),
                    Some(&req.name_pattern),
                    Some(&req.file_path),
                    req.sanitized_limit(),
                )
                .map_err(|e| e.to_string())?;

            if nodes.is_empty() {
                return Ok(String::new());
            }

            let mut out = String::new();
            for n in nodes {
                out.push_str(&format!(
                    "- {} | {} | {} (lines {}-{} | gen {})\n",
                    n.node_id, n.node_type, n.file_path, n.start_line, n.end_line, n.generation
                ));
            }
            Ok(out)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        if out.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No matching nodes.",
            )]));
        }

        Ok(CallToolResult::success(vec![Content::text(
            out.trim().to_string(),
        )]))
    }

    pub async fn handle_find_references(
        &self,
        req: FindReferencesRequest,
    ) -> Result<CallToolResult, McpError> {
        let kind = match req.edge_kind.as_deref() {
            Some("co_occurrence") => Some(EdgeKind::CoOccurrence),
            Some("temporal_coupling") => Some(EdgeKind::TemporalCoupling),
            Some("insight") => Some(EdgeKind::Insight),
            Some("dependency") => Some(EdgeKind::Dependency),
            Some("anti_pattern") => Some(EdgeKind::AntiPattern),
            Some("contains") => Some(EdgeKind::Contains),
            Some("imports") => Some(EdgeKind::Imports),
            _ => None,
        };

        let graph = self.state.graph.clone();
        let edge_kind_str = req.edge_kind.clone();
        let out = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let mut out = String::new();

            if req.direction == "in" || req.direction == "both" {
                let incoming = graph
                    .find_incoming_edges(&req.project_id, kind.clone(), &req.node_id, 100)
                    .map_err(|e| e.to_string())?;
                if !incoming.is_empty() {
                    let header = match edge_kind_str.as_deref() {
                        Some("contains") => "Containers (Incoming 'contains'):\n",
                        Some("imports") => "Imported by (Incoming 'imports'):\n",
                        Some(k) => &format!("Incoming references (kind='{}'):\n", k),
                        None => "Incoming references (all kinds):\n",
                    };
                    out.push_str(header);
                    for (n, w) in incoming {
                        out.push_str(&format!("- {} (weight={})\n", n, w));
                    }
                }
            }

            if req.direction == "out" || req.direction == "both" {
                let search_kind = kind.unwrap_or(EdgeKind::Dependency);
                let outgoing = graph
                    .neighbors(&req.project_id, search_kind, &req.node_id, 100)
                    .map_err(|e| e.to_string())?;
                if !outgoing.is_empty() {
                    let header = match edge_kind_str.as_deref() {
                        Some("contains") => "Members (Outgoing 'contains'):\n",
                        Some("imports") => "Imports (Outgoing 'imports'):\n",
                        Some(k) => &format!("Outgoing references (kind='{}'):\n", k),
                        None => "Outgoing references (dependencies):\n",
                    };
                    out.push_str(header);
                    for (n, w) in outgoing {
                        out.push_str(&format!("- {} (weight={})\n", n, w));
                    }
                }
            }

            Ok(out)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        if out.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No references found.",
            )]));
        }

        Ok(CallToolResult::success(vec![Content::text(
            out.trim().to_string(),
        )]))
    }

    pub async fn handle_graph_search(
        &self,
        req: GraphSearchRequest,
    ) -> Result<CallToolResult, McpError> {
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;
        let max_results = req.sanitized_max_results();
        let hop_depth = req.sanitized_hop_depth();
        let max_content_chars = req.sanitized_max_content_chars();

        // Validate fts_mode
        let fts_mode = match req.fts_mode.as_str() {
            "strict" | "loose" | "regex" => req.fts_mode.clone(),
            _ => "strict".into(),
        };

        // 1. Hybrid text search for initial candidates
        let hits = ps
            .search
            .search(
                &HybridQuery {
                    project_id: req.project_id.clone(),
                    namespace: req.namespace.clone(),
                    generation: gen_,
                    text: req.query.clone(),
                    top_k: max_results * 2, // oversample for graph expansion
                    fts_mode,
                    include_path_prefixes: None,
                    exclude_path_prefixes: None,
                    language_filters: None,
                    author_filter: None,
                    date_after: None,
                    date_before: None,
                    use_mmr: req.use_mmr,
                },
                None,
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // 2. Graph symbol name lookup: find symbol nodes whose name matches the query
        let symbol_nodes = self
            .state
            .graph
            .query_nodes(&req.project_id, None, Some(&req.query), None, 30)
            .unwrap_or_default();

        // Build score map: node_id -> (score, label, path_for_content)
        let mut scores: std::collections::HashMap<String, (f32, Option<String>, Option<String>)> =
            std::collections::HashMap::with_capacity(max_results * 2);

        // Seed from text search hits (file-level nodes)
        for h in &hits {
            let node_id = format!("file:{}", h.path);
            scores.insert(node_id, (h.score, None, Some(h.path.as_str().to_string())));
        }

        // Seed from symbol name matches with a symbol boost
        let base_score = hits.first().map(|h| h.score).unwrap_or(1.0);
        let query_lower = req.query.to_lowercase();
        for node in &symbol_nodes {
            let name_lower = node.name.to_lowercase();
            let match_ratio = if name_lower == query_lower {
                1.0f32
            } else if name_lower.contains(&query_lower) || query_lower.contains(&name_lower) {
                0.6
            } else {
                0.3
            };

            let sym_score = base_score * (0.5 + req.symbol_boost * 10.0 * match_ratio);
            let label = Some(format!("{} ({})", node.node_type, node.name));
            let file_path = if node.file_path.as_str().is_empty() {
                None
            } else {
                Some(node.file_path.as_str().to_string())
            };
            let entry = scores
                .entry(node.node_id.clone())
                .or_insert((0.0, None, None));
            if sym_score > entry.0 {
                *entry = (sym_score, label, file_path.clone());
            }

            // Also boost the parent file node
            if let Some(fp) = &file_path {
                let file_node_id = format!("file:{}", fp);
                let file_entry = scores.entry(file_node_id).or_insert((0.0, None, None));
                let file_boost = sym_score * 0.8;
                if file_boost > file_entry.0 {
                    file_entry.0 = file_boost;
                    file_entry.2 = file_path.clone();
                }
            }
        }

        // 3. Determine expansion edge kinds
        let default_expansion_kinds = vec![
            EdgeKind::Dependency,
            EdgeKind::Contains,
            EdgeKind::Imports,
            EdgeKind::SqlCalls,
            EdgeKind::ApiCall,
        ];
        let expansion_kinds = if let Some(ref filter) = req.expansion_edge_kinds {
            let mut kinds = Vec::new();
            for s in filter {
                if let Some(k) = EdgeKind::parse(s) {
                    kinds.push(k);
                }
            }
            if kinds.is_empty() {
                default_expansion_kinds
            } else {
                kinds
            }
        } else {
            default_expansion_kinds
        };

        // 4. Multi-hop graph expansion with configurable depth
        for _hop in 0..hop_depth {
            let seed_nodes: Vec<(String, f32)> = scores
                .iter()
                .map(|(k, (s, _, _))| (k.clone(), *s))
                .collect();

            for (node_id, parent_score) in &seed_nodes {
                let neighbors_per_kind = 5.max(10 / expansion_kinds.len());
                for kind in &expansion_kinds {
                    if let Ok(neighbors) = self.state.graph.neighbors(
                        &req.project_id,
                        kind.clone(),
                        node_id,
                        neighbors_per_kind,
                    ) {
                        for (neigh_id, weight) in neighbors {
                            let hop_decay = 0.7f32.powi((_hop + 1) as i32);
                            let weight_factor =
                                0.5 + (weight.min(10) as f32 * req.symbol_boost * 0.05);
                            let neigh_score = parent_score * weight_factor.min(0.90) * hop_decay;
                            let entry = scores.entry(neigh_id).or_insert((0.0, None, None));
                            if neigh_score > entry.0 {
                                entry.0 = neigh_score;
                            }
                        }
                    }
                }
            }
        }

        if scores.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No graph matches found.",
            )]));
        }

        // 5. Sort and format
        let mut sorted: Vec<_> = scores.into_iter().collect();
        sorted.sort_by(|a, b| {
            (b.1)
                .0
                .partial_cmp(&(a.1).0)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut out = String::with_capacity(4096);
        out.push_str(&format!(
            "Graph search results for '{}' (ns={}, {} text hits, {} symbol matches, {} hops):\n",
            req.query,
            req.namespace,
            hits.len(),
            symbol_nodes.len(),
            hop_depth
        ));

        for (id, (score, label, _path)) in sorted.iter().take(max_results) {
            if let Some(lbl) = label {
                out.push_str(&format!("- {} [{}] (score={:.3})\n", id, lbl, score));
            } else {
                out.push_str(&format!("- {} (score={:.3})\n", id, score));
            }

            // Include content preview if requested
            if req.include_content && max_content_chars > 0 {
                // Try to fetch content from search hits first
                if let Some(hit) = hits
                    .iter()
                    .find(|h| id == &format!("file:{}", h.path) || id.contains(h.path.as_str()))
                    && let Ok(Some((_, _, content, start_line, _))) = ps.search.get_doc_by_doc_id(
                        &req.project_id,
                        &req.namespace,
                        gen_,
                        &hit.doc_id,
                    )
                {
                    let preview: String = content.chars().take(max_content_chars).collect();
                    out.push_str(&format!("  L{}: {}", start_line, preview));
                    if content.chars().count() > max_content_chars {
                        out.push_str("...");
                    }
                    out.push('\n');
                }
            }
        }

        Ok(CallToolResult::success(vec![Content::text(
            out.trim().to_string(),
        )]))
    }

    pub async fn handle_traverse_graph(
        &self,
        req: TraverseGraphRequest,
    ) -> Result<CallToolResult, McpError> {
        let kinds = req.edge_kinds.as_ref().map(|v| {
            v.iter()
                .filter_map(|s| EdgeKind::parse(s.as_str()))
                .collect::<Vec<_>>()
        });

        let results = self
            .state
            .graph
            .traverse(
                &req.project_id,
                &req.node_id,
                req.sanitized_max_hops(),
                kinds,
                &req.direction,
            )
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if results.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No connected nodes found within constraints.",
            )]));
        }

        let mut out = String::new();
        out.push_str(&format!(
            "Traversal results from {} (max_hops={}):\n",
            req.node_id, req.max_hops
        ));
        for (n, dist) in results {
            out.push_str(&format!(
                "- [{}] {} | {} | {}\n",
                dist, n.node_id, n.node_type, n.file_path
            ));
        }

        Ok(CallToolResult::success(vec![Content::text(
            out.trim().to_string(),
        )]))
    }
}
