use crate::handlers::validate_project_id;
use crate::models::{
    Direction, FindReferencesRequest, GraphSearchRequest, QueryGraphNodesRequest,
    TraverseGraphRequest,
};
use crate::tools::Engram;
use engram_graph::EdgeKind;
use engram_index::HybridQuery;
use rmcp::{
    ErrorData as McpError,
    model::{CallToolResult, Content},
};

/// Render one node as an identity card with follow-up tool hints.
fn render_node_identity(node: &engram_graph::Node) -> String {
    let mut out = String::with_capacity(256);
    out.push_str(&format!("node_id: {}\n", node.node_id));
    out.push_str(&format!("name: {}\n", node.name));
    out.push_str(&format!("type: {}\n", node.node_type));
    if !node.file_path.as_str().is_empty() {
        out.push_str(&format!(
            "file: {} (lines {}-{})\n",
            node.file_path, node.start_line, node.end_line
        ));
    }
    if !node.language.is_empty() {
        out.push_str(&format!("language: {}\n", node.language));
    }
    out.push_str(&format!(
        "use with: find_symbol_references(symbol_name=\"{}\"), compute_blast_radius, traverse_graph(start_node_id=\"{}\")\n",
        node.name, node.node_id
    ));
    out
}

/// Graph tool helper methods on Engram.
impl Engram {
    /// P0-8: one tool that accepts any identifier kind (node_id, name/FQN,
    /// doc_id) and returns every identity Engram knows for it, so agents can
    /// chain search output into graph tools without guessing formats.
    pub async fn handle_resolve_id(
        &self,
        req: crate::models::ResolveIdRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;

        // 1. Graph resolution: node_id / exact name / FQN / short name.
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let id = req.id.clone();
        let resolved =
            tokio::task::spawn_blocking(move || graph.resolve_symbol(&pid, &id, None, None))
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        match resolved {
            engram_graph::ResolveResult::Unique(node) => {
                let mut out = String::from("resolved: graph node (unique)\n");
                out.push_str(&render_node_identity(&node));
                return Ok(CallToolResult::success(vec![Content::text(out)]));
            }
            engram_graph::ResolveResult::Ambiguous(nodes) => {
                let mut out = format!(
                    "resolved: AMBIGUOUS — {} graph nodes match '{}'. \
                     Pass an exact node_id or a more specific name:\n\n",
                    nodes.len(),
                    req.id
                );
                for node in nodes.iter().take(10) {
                    out.push_str(&format!(
                        "- {} ({}) node_id={} file={} lines {}-{}\n",
                        node.name,
                        node.node_type,
                        node.node_id,
                        node.file_path,
                        node.start_line,
                        node.end_line
                    ));
                }
                if nodes.len() > 10 {
                    out.push_str(&format!("... and {} more\n", nodes.len() - 10));
                }
                return Ok(CallToolResult::success(vec![Content::text(out)]));
            }
            engram_graph::ResolveResult::NotFound => {}
        }

        // 2. doc_id resolution (search-layer identity).
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;
        if let Ok(Some((path, lang, _content, start_line, end_line))) =
            ps.search
                .get_doc_by_doc_id(&req.project_id, &req.namespace, gen_, &req.id)
        {
            let mut out = String::from("resolved: search doc (chunk)\n");
            out.push_str(&format!("doc_id: {}\n", req.id));
            out.push_str(&format!(
                "file: {path} (lines {start_line}-{end_line})\nlanguage: {lang}\n"
            ));

            // Symbols covering this chunk → the node_id bridge.
            let graph = self.state.graph.clone();
            let pid = req.project_id.clone();
            let path_s = path.as_str().to_string();
            let nodes = tokio::task::spawn_blocking(move || {
                graph
                    .query_nodes(&pid, None, None, Some(&path_s), 200)
                    .unwrap_or_default()
            })
            .await
            .unwrap_or_default();
            let overlapping: Vec<String> = nodes
                .iter()
                .filter(|n| {
                    n.node_type != "file"
                        && !(n.start_line == 0 && n.end_line == 0)
                        && n.start_line <= end_line
                        && start_line <= n.end_line
                })
                .take(5)
                .map(|n| format!("{} ({}) node_id={}", n.name, n.node_type, n.node_id))
                .collect();
            if !overlapping.is_empty() {
                out.push_str(&format!("symbols: {}\n", overlapping.join("; ")));
            }
            out.push_str("use with: get_chunk(doc_id) for full text\n");
            return Ok(CallToolResult::success(vec![Content::text(out)]));
        }

        Ok(CallToolResult::success(vec![Content::text(format!(
            "resolved: NOT FOUND — '{}' is not a known node_id, symbol name, FQN, \
             or doc_id (namespace '{}', generation {}). hints: search_memory to find \
             the symbol; doc_ids are generation-scoped and expire on reindex.",
            req.id, req.namespace, gen_
        ))]))
    }

    pub async fn handle_query_graph_nodes(
        &self,
        req: QueryGraphNodesRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
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
        validate_project_id(&req.project_id)?;
        let kind = req.edge_kind.as_deref().and_then(EdgeKind::parse);

        let graph = self.state.graph.clone();
        let edge_kind_str = req.edge_kind.clone();
        let out = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let mut out = String::new();

            if matches!(req.direction, Direction::In | Direction::Both) {
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

            if matches!(req.direction, Direction::Out | Direction::Both) {
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
        validate_project_id(&req.project_id)?;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;
        let max_results = req.sanitized_max_results();
        let hop_depth = req.sanitized_hop_depth();
        let max_content_chars = req.sanitized_max_content_chars();

        // fts_mode is now a validated enum — invalid values are rejected by serde
        // at the request boundary, so no runtime guard is needed here.
        let fts_mode = req.fts_mode.as_str().to_owned();

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
                &tokio_util::sync::CancellationToken::new(),
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
            EdgeKind::Calls,
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
        validate_project_id(&req.project_id)?;
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
                req.direction.as_str(),
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

impl Engram {
    /// TODO-14: "how does A reach B?" — BFS shortest path between two
    /// resolvable identifiers, directed first, undirected fallback.
    pub async fn handle_find_connection_path(
        &self,
        req: crate::models::FindConnectionPathRequest,
    ) -> Result<CallToolResult, McpError> {
        crate::handlers::validate_project_id(&req.project_id)?;
        let _ = self.ensure_project_record(&req.project_id).await?;

        let resolve = |label: &'static str, input: String| {
            let graph = self.state.graph.clone();
            let pid = req.project_id.clone();
            async move {
                let r = tokio::task::spawn_blocking(move || {
                    graph.resolve_symbol(&pid, &input, None, None)
                })
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                match r {
                    engram_graph::ResolveResult::Unique(n) => Ok(n.node_id),
                    engram_graph::ResolveResult::Ambiguous(nodes) => {
                        let mut msg = format!(
                            "{label} '{}' is AMBIGUOUS — {} candidates. Re-call with an exact node_id:\n",
                            nodes.first().map(|n| n.name.as_str()).unwrap_or(""),
                            nodes.len()
                        );
                        for n in nodes.iter().take(8) {
                            msg.push_str(&format!(
                                "- {} ({}) node_id={}\n",
                                n.name, n.file_path, n.node_id
                            ));
                        }
                        Err(McpError::invalid_params(msg, None))
                    }
                    engram_graph::ResolveResult::NotFound => Err(McpError::invalid_params(
                        format!(
                            "{label} not found in the graph. Use resolve_id or query_graph_nodes first."
                        ),
                        None,
                    )),
                }
            }
        };
        let from_id = resolve("from", req.from.clone()).await?;
        let to_id = resolve("to", req.to.clone()).await?;

        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let max_depth = req.max_depth.clamp(1, 12);
        let (from_c, to_c) = (from_id.clone(), to_id.clone());
        let found = tokio::task::spawn_blocking(move || {
            engram_graph::analysis::find_path(&graph, &pid, &from_c, &to_c, max_depth, &[])
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let mut out = String::new();
        match found {
            Some(path) => {
                out.push_str(&format!(
                    "# Connection path ({} hop(s), {})\n\n",
                    path.hops.len(),
                    if path.directed {
                        "directed"
                    } else {
                        "undirected — includes reversed edges"
                    }
                ));
                out.push_str(&format!("{from_id}\n"));
                for hop in &path.hops {
                    let arrow = if hop.reversed { "<--" } else { "-->" };
                    out.push_str(&format!(
                        "  {arrow} [{}] {}\n",
                        hop.edge_kind.as_str(),
                        hop.node_id
                    ));
                }
                out.push_str(
                    "\nnext: blast_radius on intermediate nodes before editing them; \
                     trace_data_flow for value-level detail.\n",
                );
            }
            None => {
                out.push_str(&format!(
                    "No path within {max_depth} hops between\n  {from_id}\n  {to_id}\n\
                     (searched directed then undirected; synthesized file-membership edges excluded).\n\
                     Try a larger max_depth, or check both endpoints with resolve_id.\n"
                ));
            }
        }
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }
}
