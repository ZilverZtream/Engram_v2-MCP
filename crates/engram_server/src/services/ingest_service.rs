use crate::state::AppState;
use crate::utils::now_ms;
use engram_core::memory::{AllocationGuard, Subsystem};

fn is_safe_project_relative_path(path: &str) -> bool {
    let p = std::path::Path::new(path);
    if path.is_empty() || p.is_absolute() || path.contains('\0') {
        return false;
    }

    for component in p.components() {
        match component {
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => return false,
            _ => {}
        }
    }
    true
}

fn metadata_to_json(
    metadata: &Option<std::collections::HashMap<String, String>>,
) -> Option<serde_json::Value> {
    metadata.as_ref().map(|m| {
        let mut obj = serde_json::Map::with_capacity(m.len());
        for (k, v) in m {
            obj.insert(k.clone(), serde_json::Value::String(v.clone()));
        }
        serde_json::Value::Object(obj)
    })
}

/// Process ingest stats: create graph nodes and edges from parsed symbols.
/// This is a large function that builds the graph from the AST extraction results.
pub async fn process_ingest_stats(
    state: &AppState,
    project_id: &str,
    generation: u64,
    stats: &engram_index::IngestStats,
) -> anyhow::Result<()> {
    let mut input_edge_kind_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for (_, edge) in &stats.edges {
        *input_edge_kind_counts.entry(edge.kind.clone()).or_insert(0) += 1;
    }
    if !input_edge_kind_counts.is_empty() {
        tracing::debug!(
            project_id = %project_id,
            edge_kind_counts = ?input_edge_kind_counts,
            "process_ingest_stats: extracted edge kinds before graph mapping"
        );
    }

    let graph_estimate_bytes = ((stats.symbols.len() + stats.edges.len() + stats.all_files.len())
        as u64)
        .saturating_mul(512);
    let _graph_build_guard = AllocationGuard::try_new(
        state.memory_budget.as_ref(),
        graph_estimate_bytes.max(1),
        Subsystem::Graph,
        "graph build phase",
    )?;

    let mut nodes = Vec::with_capacity(stats.symbols.len() + stats.all_files.len());

    let fp_map: std::collections::HashMap<_, _> = stats
        .fingerprints
        .iter()
        .map(|fp| (fp.rel_path.as_str(), fp))
        .collect();

    let mut seen_virtual_node_ids = std::collections::HashSet::new();

    for rel_path in &stats.all_files {
        if !is_safe_project_relative_path(rel_path.as_str()) {
            let reason = if std::path::Path::new(rel_path.as_str()).is_absolute() {
                "absolute path rejected"
            } else {
                "unsafe relative path (traversal or null byte)"
            };
            anyhow::bail!(
                "process_ingest_stats: {} in all_files: {}",
                reason,
                rel_path.as_str()
            );
        }
        let language = engram_core::guess_language(std::path::Path::new(rel_path.as_str()));

        let mut metadata = None;

        if let Some(fp) = fp_map.get(rel_path.as_str()) {
            metadata = Some(serde_json::json!({
                "mtime": fp.mtime_ms / 1000,
                "size": fp.size,
                "file_hash": fp.file_hash,
            }));
        }

        nodes.push(engram_graph::Node {
            node_id: engram_core::ids::NodeId::file(rel_path.as_str()).0,
            node_type: "file".into(),
            name: rel_path
                .file_name()
                .unwrap_or_else(|| rel_path.as_str())
                .to_string(),
            namespace: engram_core::namespaces::NAMESPACE_MEMORY.into(),
            language: language.into(),
            file_path: rel_path.clone(),
            start_line: 0,
            end_line: 0,
            generation,
            metadata,
        });
    }

    // node_id → index of the fingerprint-carrying file nodes pushed above,
    // so per-file status symbols can MERGE into them instead of minting a
    // shadow node.
    let file_node_index: std::collections::HashMap<String, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.node_id.clone(), i))
        .collect();

    let mut file_contains_edges: Vec<engram_graph::Edge> = Vec::new();
    for (rel_path, sym) in &stats.symbols {
        if !is_safe_project_relative_path(rel_path.as_str()) {
            anyhow::bail!(
                "process_ingest_stats: unsafe relative path in symbols: {}",
                rel_path.as_str()
            );
        }

        if sym.kind == "file" {
            // The sidecar's per-file parse-status symbol (parse_success,
            // parse_error_count, is_designer). It used to mint a SECOND
            // node_type="file" node (`sym:file:…`) for the same path;
            // readers that key file nodes by path then saw the parse-status
            // metadata instead of the fingerprint metadata (redb key order
            // puts `sym:` after `file:`), so the incremental change scan
            // read stored=(0,0) for every sidecar-parsed file and re-indexed
            // all of them on every update, forever. Merge into the real
            // file node instead.
            let fid = engram_core::ids::NodeId::file(rel_path.as_str()).0;
            if let (Some(&i), Some(m)) = (file_node_index.get(&fid), &sym.metadata) {
                let mut obj = match nodes[i].metadata.take() {
                    Some(serde_json::Value::Object(o)) => o,
                    _ => serde_json::Map::new(),
                };
                for (k, v) in m.iter() {
                    obj.insert(k.clone(), serde_json::Value::String(v.clone()));
                }
                nodes[i].metadata = Some(serde_json::Value::Object(obj));
            }
            continue;
        }

        let language = engram_core::guess_language(std::path::Path::new(rel_path.as_str()));

        let (metadata, fqn) = if let Some(m) = &sym.metadata {
            let fqn_val = m.get("fqn").map(|v| v.as_str().to_string());
            let mut map: serde_json::Map<String, serde_json::Value> = m
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                .collect();
            if let Some(fqn) = &fqn_val {
                map.entry("fqn".to_string())
                    .or_insert_with(|| serde_json::Value::String(fqn.clone()));
            }
            (Some(serde_json::Value::Object(map)), fqn_val)
        } else {
            (None, None)
        };
        let fqn = fqn.as_deref();

        let (node_id, final_kind) = if sym.kind == "page" {
            (
                engram_core::ids::NodeId::page(rel_path.as_str()).0,
                sym.kind.to_string(),
            )
        } else if sym.kind == "control" {
            let control_id = sym
                .metadata
                .as_ref()
                .and_then(|m| m.get("control_id"))
                .map(|s| s.as_str())
                .unwrap_or(sym.name.as_str());
            (
                engram_core::ids::NodeId::control(rel_path.as_str(), control_id).0,
                "control".to_string(),
            )
        } else if sym.kind == "control_ref" {
            let path_str = rel_path.as_str();
            let page_path = if let Some(idx) = path_str.find(".designer.") {
                &path_str[..idx]
            } else if let Some(idx) = path_str.find(".aspx.") {
                &path_str[..idx + 5]
            } else if let Some(idx) = path_str.find(".ascx.") {
                &path_str[..idx + 5]
            } else {
                path_str
            };
            (
                engram_core::ids::NodeId::control(page_path, &sym.name).0,
                "control".to_string(),
            )
        } else if sym.kind == "db_table" {
            (
                engram_core::ids::NodeId::table(&sym.name).0,
                "db_table".to_string(),
            )
        } else if sym.kind == "db_column" {
            let table = sym
                .metadata
                .as_ref()
                .and_then(|m| m.get("table"))
                .map(|s| s.as_str())
                .unwrap_or("unknown");
            (
                engram_core::ids::NodeId::column(table, &sym.name).0,
                "db_column".to_string(),
            )
        } else if sym.kind == "global_state" {
            let state_type = sym
                .metadata
                .as_ref()
                .and_then(|m| m.get("state_type"))
                .map(|s| s.as_str())
                .unwrap_or("Session");
            let state_key = sym
                .metadata
                .as_ref()
                .and_then(|m| m.get("state_key"))
                .map(|s| s.as_str())
                .unwrap_or(&sym.name);
            (
                engram_core::ids::NodeId::state(state_type, state_key).0,
                "global_state".to_string(),
            )
        } else if sym.kind == "ui_container" {
            (
                engram_core::ids::NodeId::ui_container(rel_path.as_str(), &sym.name).0,
                "ui_container".to_string(),
            )
        } else if sym.kind == "control_layout" {
            // control_layout is metadata-enriched view of an existing control;
            // use the same control NodeId so it merges with the base control node.
            let control_id = sym
                .metadata
                .as_ref()
                .and_then(|m| m.get("control_id"))
                .map(|s| s.as_str())
                .unwrap_or(sym.name.as_str());
            (
                engram_core::ids::NodeId::control(rel_path.as_str(), control_id).0,
                "control_layout".to_string(),
            )
        } else {
            (
                engram_core::ids::NodeId::symbol(
                    sym.kind.as_str(),
                    fqn,
                    rel_path.as_str(),
                    &sym.name,
                    sym.start_line,
                )
                .0,
                sym.kind.to_string(),
            )
        };

        // TODO-16: file-level containment. Extractors emit Contains only as
        // namespace->class and class->function, so nothing links a file node
        // to the symbols inside it; blast radius had to re-derive membership
        // by comparing file_path strings. Emit an explicit file->symbol edge
        // for location-based symbols (the generic branch above). Tagged so
        // scoring passes can tell synthesized containment from extractor
        // Contains edges.
        let is_location_symbol = node_id.starts_with("sym:");
        if is_location_symbol {
            file_contains_edges.push(engram_graph::Edge {
                source_id: engram_core::ids::NodeId::file(rel_path.as_str()).0,
                target_id: node_id.clone(),
                namespace: engram_core::namespaces::NAMESPACE_MEMORY.into(),
                language: language.into(),
                edge_kind: engram_graph::EdgeKind::Contains,
                weight: 1,
                generation,
                metadata: Some(serde_json::json!({"containment": "file"})),
                updated_at_ms: now_ms(),
            });
        }

        nodes.push(engram_graph::Node {
            node_id,
            node_type: final_kind,
            name: sym.name.clone(),
            namespace: engram_core::namespaces::NAMESPACE_MEMORY.into(),
            language: language.into(),
            file_path: (**rel_path).clone(),
            start_line: sym.start_line,
            end_line: sym.end_line,
            generation,
            metadata,
        });
    }

    // ── GRAPH-WIRE: batch symbol lookup for edge endpoints ───────────────────
    // Edge endpoints MUST reproduce the exact node IDs minted in the symbol
    // loop above (location-based: sym:{kind}:{path}:{name}:{line}). Extractors
    // frequently put an FQN in source/target names and a call-site (or no)
    // line on the edge; rebuilding an ID from those fields alone mints
    // phantom endpoints that match no node and silently disconnect calls,
    // SQL, and event wiring (regression introduced when node IDs switched to
    // the location-based scheme). This lookup finds the declaring
    // (path, kind, name, line) for a short name or an FQN's terminal segment.
    const NON_SYMBOL_KINDS: [&str; 8] = [
        "page",
        "control",
        "control_ref",
        "db_table",
        "db_column",
        "global_state",
        "ui_container",
        "control_layout",
    ];
    // (path, kind, name, line, arity, language of the declaring file)
    type SymEntry<'a> = (&'a str, &'a str, &'a str, u32, Option<u32>, &'static str);
    let mut symbols_by_name: std::collections::HashMap<&str, Vec<SymEntry>> =
        std::collections::HashMap::new();
    for (sym_path, sym) in &stats.symbols {
        if NON_SYMBOL_KINDS.contains(&sym.kind.as_str()) {
            continue;
        }
        let arity = sym
            .metadata
            .as_ref()
            .and_then(|m| m.get("arity"))
            .and_then(|s| s.parse::<u32>().ok());
        let sym_lang = engram_core::guess_language(std::path::Path::new(sym_path.as_str()));
        let entry: SymEntry = (
            sym_path.as_str(),
            sym.kind.as_str(),
            sym.name.as_str(),
            sym.start_line,
            arity,
            sym_lang,
        );
        symbols_by_name
            .entry(sym.name.as_str())
            .or_default()
            .push(entry);
        // FQN-named symbols are also reachable via their terminal segment.
        if let Some(term) = sym.name.rsplit('.').next()
            && term != sym.name
        {
            symbols_by_name.entry(term).or_default().push(entry);
        }
    }
    let rebuild_symbol_id =
        |e: SymEntry| -> String { engram_core::ids::NodeId::symbol(e.1, None, e.0, e.2, e.3).0 };
    // TODO-13: call-site argument count, when the extractor recorded it.
    let edge_call_arity = |edge: &engram_index::ExtractedEdge| -> Option<u32> {
        edge.metadata
            .as_ref()
            .and_then(|m| m.get("args"))
            .and_then(|s| s.parse::<u32>().ok())
    };
    // Resolve a possibly-qualified name to a real symbol node in this batch:
    // same-file exact/terminal match first, then unique kind-matching
    // cross-file candidate, then unique candidate of any kind. None means
    // absent-or-ambiguous; the caller then emits a "::name" placeholder so
    // the post-ingest resolve_symbol_edges pass (which sees the whole graph,
    // not just this batch) can rewire it.
    // TODO-12: every resolution carries (confidence, method) so consumers
    // can tell an FQN-verified binding from a bare-name guess. Terminal-
    // segment keys and any-kind matches are progressively less trustworthy —
    // a JS `Map` call binding to a class named `ConfigSettings.Map` is the
    // canonical false positive this exposes.
    let resolve_batch_symbol = |raw: &str,
                                prefer_path: &str,
                                prefer_kind: Option<&str>,
                                prefer_arity: Option<u32>,
                                caller_lang: &str|
     -> Option<(String, f32, &'static str, bool)> {
        // Defensive: Roslyn-bound call targets can arrive signature-shaped
        // ("Ns.Cls.Save(Integer, String)") while definition names are bare —
        // strip the parameter list before deriving lookup keys (arity
        // already travels separately in metadata["args"]).
        let raw = raw.split('(').next().unwrap_or(raw).trim_end();
        let terminal = raw.rsplit('.').next().unwrap_or(raw);
        for key in [raw, terminal] {
            let via_terminal = !std::ptr::eq(key, raw) && key != raw;
            let Some(cands) = symbols_by_name.get(key) else {
                continue;
            };
            let kind_ok = |c: &SymEntry| prefer_kind.is_none_or(|k| c.1.eq_ignore_ascii_case(k));
            // TODO-13: a candidate matches arity when both sides know it.
            let arity_ok = |c: &SymEntry| match (prefer_arity, c.4) {
                (Some(want), Some(have)) => want == have,
                _ => false,
            };
            let same_file: Vec<&SymEntry> = cands.iter().filter(|c| c.0 == prefer_path).collect();
            // Same-file overloads: an arity match beats the first name hit.
            let crosses = |c: &SymEntry| -> bool { c.5 != caller_lang };
            if same_file.len() > 1
                && let Some(c) = same_file.iter().find(|c| kind_ok(c) && arity_ok(c))
            {
                return Some((
                    rebuild_symbol_id(**c),
                    0.92,
                    "batch_same_file_arity",
                    crosses(c),
                ));
            }
            if let Some(c) = same_file
                .iter()
                .find(|c| kind_ok(c))
                .or_else(|| same_file.first())
            {
                let (conf, method) = if via_terminal {
                    (0.85, "batch_same_file_terminal")
                } else {
                    (0.9, "batch_same_file")
                };
                return Some((rebuild_symbol_id(**c), conf, method, crosses(c)));
            }
            let kind_matches: Vec<&SymEntry> = cands.iter().filter(|c| kind_ok(c)).collect();
            // Cross-file: exactly one arity-matching candidate wins over an
            // otherwise-ambiguous set.
            if kind_matches.len() > 1 {
                let arity_matches: Vec<&&SymEntry> =
                    kind_matches.iter().filter(|c| arity_ok(c)).collect();
                if arity_matches.len() == 1 {
                    return Some((
                        rebuild_symbol_id(**arity_matches[0]),
                        0.75,
                        "batch_arity_match",
                        crosses(arity_matches[0]),
                    ));
                }
                // TODO-19: among otherwise-ambiguous candidates, exactly one
                // sharing the caller's LANGUAGE beats the bare-name tie —
                // a VB call resolving into VB is far likelier than into JS.
                let lang_matches: Vec<&&SymEntry> =
                    kind_matches.iter().filter(|c| !crosses(c)).collect();
                if lang_matches.len() == 1 {
                    return Some((
                        rebuild_symbol_id(**lang_matches[0]),
                        0.65,
                        "batch_same_lang",
                        false,
                    ));
                }
            }
            if kind_matches.len() == 1 {
                let (conf, method) = if via_terminal {
                    (0.55, "batch_unique_kind_terminal")
                } else {
                    (0.7, "batch_unique_kind")
                };
                return Some((
                    rebuild_symbol_id(*kind_matches[0]),
                    conf,
                    method,
                    crosses(kind_matches[0]),
                ));
            }
            if cands.len() == 1 {
                let (conf, method) = if via_terminal {
                    (0.35, "batch_unique_any_terminal")
                } else {
                    (0.5, "batch_unique_any")
                };
                return Some((
                    rebuild_symbol_id(cands[0]),
                    conf,
                    method,
                    crosses(&cands[0]),
                ));
            }
        }
        None
    };

    let mut edges = Vec::with_capacity(stats.edges.len());
    for (rel_path, edge) in &stats.edges {
        if !is_safe_project_relative_path(rel_path.as_str()) {
            anyhow::bail!(
                "process_ingest_stats: unsafe relative path in edges: {}",
                rel_path.as_str()
            );
        }

        let language = engram_core::guess_language(std::path::Path::new(&format!(
            "dummy.{}",
            edge.source_language
        )));

        let source_id = if edge.source_name == "file" || edge.source_kind == "file" {
            let path = if edge.source_name == "file" {
                rel_path.as_str()
            } else {
                &edge.source_name
            };
            if !is_safe_project_relative_path(path) {
                anyhow::bail!(
                    "process_ingest_stats: unsafe path in edge source: {} (file: {})",
                    path,
                    rel_path.as_str()
                );
            }
            if edge.source_kind == "page" {
                engram_core::ids::NodeId::page(path).0
            } else {
                engram_core::ids::NodeId::file(path).0
            }
        } else if edge.source_kind == "page" {
            engram_core::ids::NodeId::page(rel_path.as_str()).0
        } else if edge.source_kind == "ui_container" {
            engram_core::ids::NodeId::ui_container(rel_path.as_str(), &edge.source_name).0
        } else if edge.source_kind == "control" {
            let control_id = edge
                .metadata
                .as_ref()
                .and_then(|m| m.get("control_id"))
                .map(|s| s.as_str())
                .unwrap_or(edge.source_name.as_str());

            let path_str = rel_path.as_str();
            let page_path = if path_str.ends_with(".cs") || path_str.ends_with(".vb") {
                if let Some(idx) = path_str.find(".aspx.") {
                    &path_str[..idx + 5]
                } else if let Some(idx) = path_str.find(".ascx.") {
                    &path_str[..idx + 5]
                } else {
                    path_str
                }
            } else {
                path_str
            };
            if !is_safe_project_relative_path(page_path) {
                anyhow::bail!(
                    "process_ingest_stats: unsafe control source page path: {} (file: {})",
                    page_path,
                    rel_path.as_str()
                );
            }
            engram_core::ids::NodeId::control(page_path, control_id).0
        } else {
            // GRAPH-WIRE: prefer the real declaring node from this batch —
            // extractors put FQNs (e.g. "MyApp.Data.LoadData") and call-site
            // lines here, which otherwise rebuild into phantom endpoints that
            // match no node minted in the symbol loop.
            resolve_batch_symbol(
                &edge.source_name,
                rel_path.as_str(),
                Some(edge.source_kind.as_str()),
                None,
                language,
            )
            .map(|(id, _, _, _)| id)
            .unwrap_or_else(|| {
                let fqn = if edge.source_name.contains('.') {
                    Some(edge.source_name.as_str())
                } else {
                    edge.metadata
                        .as_ref()
                        .and_then(|m| m.get("source_fqn"))
                        .map(|s| s.as_str())
                };
                engram_core::ids::NodeId::symbol(
                    edge.source_kind.as_str(),
                    fqn,
                    rel_path.as_str(),
                    &edge.source_name,
                    edge.source_start_line,
                )
                .0
            })
        };

        // TODO-12: how the TARGET endpoint was bound (None = file/page/etc.
        // structural ids, trusted-line rebuilds, or placeholders resolved in
        // the post-pass).
        let mut target_resolution: Option<(f32, &'static str)> = None;
        let mut target_crossed_language = false;
        let target_id = if edge.target_name == "file" || edge.target_kind.as_deref() == Some("file")
        {
            let path = if edge.target_name == "file" {
                rel_path.as_str()
            } else {
                &edge.target_name
            };
            if !is_safe_project_relative_path(path) {
                anyhow::bail!(
                    "process_ingest_stats: unsafe path in edge target: {} (file: {})",
                    path,
                    rel_path.as_str()
                );
            }
            if edge.target_kind.as_deref() == Some("page") {
                engram_core::ids::NodeId::page(path).0
            } else {
                engram_core::ids::NodeId::file(path).0
            }
        } else if edge.target_kind.as_deref() == Some("page") {
            engram_core::ids::NodeId::page(rel_path.as_str()).0
        } else if edge.target_kind.as_deref() == Some("control") {
            let control_id = edge
                .metadata
                .as_ref()
                .and_then(|m| m.get("control_id"))
                .map(|s| s.as_str())
                .unwrap_or(edge.target_name.as_str());
            // GRAPH-WIRE: control nodes are keyed by their PAGE path. Map
            // codebehind/designer files to the page; for non-page sources
            // (JS/TS bridge edges like __doPostBack) the owning page is
            // unknown here — emit a ::placeholder for resolve_symbol_edges
            // instead of a phantom control ID keyed by the referencing file.
            let path_str = rel_path.as_str();
            let page_path = if let Some(idx) = path_str.find(".aspx.") {
                &path_str[..idx + 5]
            } else if let Some(idx) = path_str.find(".ascx.") {
                &path_str[..idx + 5]
            } else if let Some(idx) = path_str.find(".master.") {
                &path_str[..idx + 7]
            } else {
                path_str
            };
            let lower = page_path.to_ascii_lowercase();
            let is_page_file =
                lower.ends_with(".aspx") || lower.ends_with(".ascx") || lower.ends_with(".master");
            let sanitized_control = control_id.trim().replace('\0', "");
            if is_page_file || sanitized_control.is_empty() {
                engram_core::ids::NodeId::control(page_path, control_id).0
            } else {
                format!("::{}", sanitized_control)
            }
        } else if edge.target_kind.as_deref() == Some("control_ref") {
            let path_str = rel_path.as_str();
            let page_path = if let Some(idx) = path_str.find(".designer.") {
                &path_str[..idx]
            } else {
                path_str
            };
            if !is_safe_project_relative_path(page_path) {
                anyhow::bail!(
                    "process_ingest_stats: unsafe control target page path: {} (file: {})",
                    page_path,
                    rel_path.as_str()
                );
            }
            let simple_name = edge
                .target_name
                .split('.')
                .next_back()
                .unwrap_or(&edge.target_name);
            engram_core::ids::NodeId::control(page_path, simple_name).0
        } else if edge.target_name.starts_with("sql:")
            || edge.target_name.starts_with("state:")
            || edge.target_name.starts_with("binding_field:")
            || edge.target_name.starts_with("gis_config:")
            || edge.target_name.starts_with("column:")
        {
            edge.target_name.clone()
        } else if edge.target_kind.as_deref() == Some("endpoint") {
            // Web API / route targets get a stable virtual route node
            // (parity with sql: targets) — there is no source-file
            // declaration for them to bind to.
            format!("route:{}", edge.target_name.trim())
        } else if edge.target_kind.as_deref() == Some("db_table") {
            engram_core::ids::NodeId::table(&edge.target_name).0
        } else if edge.target_kind.as_deref() == Some("db_column") {
            let table = edge
                .metadata
                .as_ref()
                .and_then(|m| m.get("local_table").or_else(|| m.get("table")))
                .map(|s| s.as_str())
                .unwrap_or("unknown");
            engram_core::ids::NodeId::column(table, &edge.target_name).0
        } else if let Some(kind) = &edge.target_kind {
            // GRAPH-WIRE: precedence for symbol targets —
            //  1. exact same-file (name, line) batch match: overload-precise
            //     when the extractor knew the declaration site AND named the
            //     symbol exactly as its node;
            //  2. batch resolution: handles FQN-vs-short-name mismatches
            //     (tree-sitter contains edges carry FQN target names while
            //     nodes are short-named; the VB fallback is the inverse);
            //  3. trusted-line mint: same-file targets that are not regular
            //     symbols in this batch;
            //  4. ::placeholder for the post-ingest resolver — never a
            //     phantom location ID under the SOURCE file's path.
            let exact_same_file = edge.target_start_line.and_then(|line| {
                symbols_by_name
                    .get(edge.target_name.as_str())
                    .and_then(|cands| {
                        cands
                            .iter()
                            .find(|c| {
                                c.0 == rel_path.as_str() && c.2 == edge.target_name && c.3 == line
                            })
                            .map(|c| rebuild_symbol_id(*c))
                    })
            });
            if let Some(id) = exact_same_file {
                target_resolution = Some((0.98, "exact_same_file"));
                id
            } else if let Some((id, conf, method, crossed)) = resolve_batch_symbol(
                &edge.target_name,
                rel_path.as_str(),
                Some(kind.as_str()),
                edge_call_arity(edge),
                language,
            ) {
                // TODO-19: a binding that crosses a language boundary on a
                // bare name is the weakest signal class — discount it and
                // record the crossing.
                let conf = if crossed { (conf * 0.8).max(0.2) } else { conf };
                target_resolution = Some((conf, method));
                target_crossed_language = crossed;
                id
            } else if let Some(line) = edge.target_start_line {
                let fqn = if edge.target_name.contains('.') {
                    Some(edge.target_name.as_str())
                } else {
                    edge.metadata
                        .as_ref()
                        .and_then(|m| m.get("fqn"))
                        .map(|s| s.as_str())
                };
                engram_core::ids::NodeId::symbol(
                    kind.as_str(),
                    fqn,
                    rel_path.as_str(),
                    &edge.target_name,
                    line,
                )
                .0
            } else {
                let sanitized = edge.target_name.trim().replace('\0', "");
                if sanitized.is_empty() {
                    anyhow::bail!("process_ingest_stats: empty symbol target name");
                }
                format!("::{}", sanitized)
            }
        } else {
            let sanitized = edge.target_name.trim().replace('\0', "");
            if sanitized.is_empty() {
                anyhow::bail!("process_ingest_stats: empty unresolved target name");
            }
            format!("::{}", sanitized)
        };

        // Virtual nodes for route targets (Web API endpoints hit from JS).
        if target_id.starts_with("route:") && seen_virtual_node_ids.insert(target_id.clone()) {
            nodes.push(engram_graph::Node {
                node_id: target_id.clone(),
                node_type: "route_handler".into(),
                name: edge.target_name.clone(),
                namespace: engram_core::namespaces::NAMESPACE_MEMORY.into(),
                language: language.into(),
                file_path: (**rel_path).clone(),
                start_line: edge.target_start_line.unwrap_or(0),
                end_line: edge.target_start_line.unwrap_or(0),
                generation,
                metadata: metadata_to_json(&edge.metadata),
            });
        }

        // Virtual nodes for SQL targets
        if target_id.starts_with("sql:") && seen_virtual_node_ids.insert(target_id.clone()) {
            let sql_name = target_id.split(':').next_back().unwrap_or(&target_id);
            let sql_kind = if target_id.contains(":stored_proc:") {
                "stored_proc"
            } else {
                "inline_sql"
            };
            nodes.push(engram_graph::Node {
                node_id: target_id.clone(),
                node_type: sql_kind.into(),
                name: sql_name.to_string(),
                namespace: engram_core::namespaces::NAMESPACE_MEMORY.into(),
                language: "sql".into(),
                file_path: (**rel_path).clone(),
                start_line: edge.target_start_line.unwrap_or(0),
                end_line: edge.target_start_line.unwrap_or(0),
                generation,
                metadata: metadata_to_json(&edge.metadata),
            });
        }

        // Virtual nodes for binding_field targets (Eval/Bind expressions)
        if target_id.starts_with("binding_field:")
            && seen_virtual_node_ids.insert(target_id.clone())
        {
            let field_name = target_id
                .strip_prefix("binding_field:")
                .unwrap_or(&target_id);
            nodes.push(engram_graph::Node {
                node_id: target_id.clone(),
                node_type: "binding_field".into(),
                name: field_name.to_string(),
                namespace: engram_core::namespaces::NAMESPACE_MEMORY.into(),
                language: "text".into(),
                file_path: (**rel_path).clone(),
                start_line: 0,
                end_line: 0,
                generation,
                metadata: Some(serde_json::json!({
                    "field_name": field_name,
                })),
            });
        }

        // Virtual nodes for global state targets
        if target_id.starts_with("state:") && seen_virtual_node_ids.insert(target_id.clone()) {
            let parts: Vec<&str> = target_id.splitn(3, ':').collect();
            if parts.len() == 3 {
                let state_type = parts[1];
                let state_key = parts[2];
                nodes.push(engram_graph::Node {
                    node_id: target_id.clone(),
                    node_type: "global_state".into(),
                    name: format!("{}:{}", state_type, state_key),
                    namespace: engram_core::namespaces::NAMESPACE_MEMORY.into(),
                    language: "text".into(),
                    file_path: (**rel_path).clone(),
                    start_line: 0,
                    end_line: 0,
                    generation,
                    metadata: Some(serde_json::json!({
                        "state_type": state_type,
                        "state_key": state_key,
                    })),
                });
            }
        }

        // Virtual nodes for GIS config targets (API keys, zoom levels, center points)
        if target_id.starts_with("gis_config:") && seen_virtual_node_ids.insert(target_id.clone()) {
            let parts: Vec<&str> = target_id.splitn(3, ':').collect();
            if parts.len() == 3 {
                let page_path = parts[1];
                let config_key = parts[2];
                nodes.push(engram_graph::Node {
                    node_id: target_id.clone(),
                    node_type: "gis_config".into(),
                    name: format!("{}:{}", page_path, config_key),
                    namespace: engram_core::namespaces::NAMESPACE_MEMORY.into(),
                    language: "javascript".into(),
                    file_path: (**rel_path).clone(),
                    start_line: 0,
                    end_line: 0,
                    generation,
                    metadata: Some(serde_json::json!({
                        "page_path": page_path,
                        "config_key": config_key,
                    })),
                });
            }
        }

        let raw_kind = edge.kind.as_str();
        let edge_kind = match raw_kind {
            "contains" | "cb_defines" | "inherits" | "codebehind_file" | "codebehind_class" => {
                engram_graph::EdgeKind::Contains
            }
            "calls" => engram_graph::EdgeKind::Calls,
            "imports" => engram_graph::EdgeKind::Imports,
            "sql_calls" => engram_graph::EdgeKind::SqlCalls,
            "has_column" => engram_graph::EdgeKind::HasColumn,
            "foreign_key" => engram_graph::EdgeKind::ForeignKey,
            "queries_table" => engram_graph::EdgeKind::QueriesTable,
            "reads_state" => engram_graph::EdgeKind::ReadsState,
            "writes_state" => engram_graph::EdgeKind::WritesState,
            "reads_setting" => engram_graph::EdgeKind::ReadsSetting,
            "inherits_from" => engram_graph::EdgeKind::InheritsFrom,
            "implements_interface" => engram_graph::EdgeKind::Implements,
            "data_binding" => engram_graph::EdgeKind::DataBinding,
            "registers_control" => engram_graph::EdgeKind::RegistersControl,
            "includes_file" => engram_graph::EdgeKind::IncludesFile,
            "unresolved_state_read" => engram_graph::EdgeKind::UnresolvedStateRead,
            "unresolved_state_write" => engram_graph::EdgeKind::UnresolvedStateWrite,
            "exposes_web_service" => engram_graph::EdgeKind::ExposesWebService,
            "exposes_http_handler" => engram_graph::EdgeKind::ExposesHttpHandler,
            "exposes_wcf_service" => engram_graph::EdgeKind::ExposesWcfService,
            "contains_ui" => engram_graph::EdgeKind::ContainsUi,
            "ui_layout_neighbor" => engram_graph::EdgeKind::UiLayoutNeighbor,
            "reads_column" => engram_graph::EdgeKind::ReadsColumn,
            "registers_module" => engram_graph::EdgeKind::RegistersModule,
            "registers_handler" => engram_graph::EdgeKind::RegistersHandler,
            "manipulates_dom" => engram_graph::EdgeKind::ManipulatesDom,
            "triggers_postback" => engram_graph::EdgeKind::TriggersPostback,
            "api_call" => engram_graph::EdgeKind::ApiCall,
            "parameter_binding" => engram_graph::EdgeKind::ParameterBinding,
            "spatial_call" => engram_graph::EdgeKind::SpatialCall,
            "state_affinity" => engram_graph::EdgeKind::StateAffinity,
            "injects_script" => engram_graph::EdgeKind::InjectsScript,
            _ => engram_graph::EdgeKind::Dependency,
        };

        // Diagnostic: log the raw-kind → graph-kind mapping for the
        // `event_wiring` family so we can see exactly which source/target
        // IDs land as Dependency edges. This was requested to diagnose
        // why `trace_ui_event` from `control:…:linqSource` returned zero
        // outgoing Dependency edges on OciusX despite the extractor
        // emitting 1571 `event_wiring` edges — a log here makes the
        // resolved node-id shape visible without requiring a custom
        // graph dump.
        if raw_kind == "event_wiring" {
            tracing::debug!(
                raw_kind = raw_kind,
                source_id = %source_id,
                target_id = %target_id,
                "event_wiring → Dependency: ingest-side id mapping"
            );
        }

        let metadata = {
            let mut obj =
                serde_json::Map::with_capacity(edge.metadata.as_ref().map_or(0, |m| m.len()) + 3);
            if let Some(m) = edge.metadata.as_ref() {
                for (k, v) in m {
                    obj.insert(k.clone(), serde_json::Value::String(v.clone()));
                }
            }
            // Call-SITE anchor: the extractor's source_start_line is the line
            // the reference occurs on (line-scanned VB fallback exactly; the
            // enclosing-capture line otherwise). Persisting it lets
            // find_symbol_references show WHERE each caller references the
            // symbol instead of only who the caller is.
            if edge.source_start_line > 0 {
                obj.insert(
                    "src_line".into(),
                    serde_json::Value::String(edge.source_start_line.to_string()),
                );
            }
            if let Some((conf, method)) = target_resolution {
                obj.insert(
                    "resolution".into(),
                    serde_json::Value::String(method.into()),
                );
                obj.insert(
                    "confidence".into(),
                    serde_json::Value::String(format!("{conf:.2}")),
                );
                if target_crossed_language {
                    obj.insert(
                        "cross_language".into(),
                        serde_json::Value::String("true".into()),
                    );
                }
            }
            if obj.is_empty() {
                None
            } else {
                Some(serde_json::Value::Object(obj))
            }
        };
        edges.push(engram_graph::Edge {
            source_id,
            target_id,
            namespace: engram_core::namespaces::NAMESPACE_MEMORY.into(),
            language: language.into(),
            edge_kind,
            weight: 1,
            generation,
            metadata,
            updated_at_ms: now_ms(),
        });
    }

    edges.append(&mut file_contains_edges);

    if !edges.is_empty() {
        let mut mapped_edge_kind_counts: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for edge in &edges {
            *mapped_edge_kind_counts
                .entry(edge.edge_kind.as_str().to_string())
                .or_insert(0) += 1;
        }
        tracing::debug!(
            project_id = %project_id,
            edge_kind_counts = ?mapped_edge_kind_counts,
            "process_ingest_stats: graph edge kinds after mapping"
        );
    }

    if !nodes.is_empty() || !edges.is_empty() {
        let graph = state.graph.clone();
        let pid = project_id.to_string();
        match tokio::task::spawn_blocking(move || {
            graph.upsert_nodes_and_edges(&pid, &nodes, &edges)
        })
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::error!("graph upsert_nodes_and_edges failed for {project_id}: {e}");
                return Err(e);
            }
            Err(e) => {
                tracing::error!("graph upsert_nodes_and_edges task panicked for {project_id}: {e}");
                anyhow::bail!("graph upsert_nodes_and_edges task panicked: {e}");
            }
        }
    }

    // P0-5: record when this project's index last completed so every read
    // tool (and get_index_freshness) can report staleness. Both index_project
    // and update_project flow through here, making it the single choke point.
    {
        let reg = state.registry.clone();
        let pid = project_id.to_string();
        let completed_ms = now_ms().to_string();
        let files_count = stats.all_files.len().to_string();
        match tokio::task::spawn_blocking(move || {
            reg.set_meta(&pid, "last_index_completed_ms", &completed_ms)?;
            reg.set_meta(&pid, "last_index_files", &files_count)
        })
        .await
        {
            Ok(Ok(())) => {}
            // Freshness metadata is advisory — never fail an ingest over it.
            Ok(Err(e)) => tracing::warn!(
                project_id = %project_id,
                "failed to record last_index_completed_ms: {e}"
            ),
            Err(e) => tracing::warn!(
                project_id = %project_id,
                "last_index_completed_ms write task panicked: {e}"
            ),
        }
    }

    Ok(())
}
