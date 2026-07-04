//! Settings intelligence — the "PO's brain as a database".
//!
//! Legacy apps accumulate hundreds of settings (web.config appSettings,
//! settings-table rows surfaced through store classes, Session/Application
//! keys) whose purpose and interactions live in one person's head. These
//! tools turn the graph's existing knowledge (app_setting /
//! connection_string / global_state nodes, reads_setting / ReadsState /
//! WritesState edges, settings-store accessor symbols) into a queryable
//! catalog: WHICH settings exist, WHERE each is used (file:function:line),
//! and — per setting — what to probe when testing.

use crate::handlers::validate_project_id;
use crate::tools::Engram;
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use std::collections::BTreeMap;

/// Is this node a settings-store accessor: a property/function/field whose
/// class/namespace or name marks it as part of a settings/config store
/// (ConfigSettings.*, SystemSettingStore.*, …)? Generic token match — no
/// per-repo names.
fn is_store_accessor(node: &engram_graph::Node) -> bool {
    if !matches!(node.node_type.as_str(), "property" | "function" | "field") {
        return false;
    }
    let ns = node.namespace.to_lowercase();
    if ns.contains("setting")
        || ns.contains("configsetting")
        || ns.ends_with("config")
        || ns.contains("useraccess")
    {
        return true;
    }
    // VB fallback symbols carry the WHOLE dotted path in `name` with an
    // empty/default namespace (ConfigSettings.Multitenant.IsMaster) — accept
    // when the ROOT segment carries the settings/config token.
    // Check the whole dotted PATH before the terminal segment - store
    // classes often sit under a namespace alias (_us.UserAccessObject.x,
    // ConfigSettings.Multitenant.y), so the token can be in ANY parent
    // segment, not only the first.
    let name = node.name.to_lowercase();
    if let Some((path, _last)) = name.rsplit_once('.') {
        return path.contains("setting")
            || path.contains("config")
            || path.contains("useraccess")
            || path.contains("permission");
    }
    false
}

/// Category of a catalog entry, in render order.
fn category(node: &engram_graph::Node) -> Option<&'static str> {
    match node.node_type.as_str() {
        "app_setting" => Some("web.config appSettings"),
        "connection_string" => Some("connection strings"),
        "global_state" => Some("shared state keys (Session/Application/Cache)"),
        _ if is_store_accessor(node) => Some("settings-store accessors (code)"),
        _ => None,
    }
}

impl Engram {
    pub async fn handle_list_settings(
        &self,
        req: crate::models::ListSettingsRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let _rec = self.ensure_project_record(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let scope = req
            .scope
            .as_deref()
            .map(|s| s.replace('\\', "/").to_lowercase());

        type Entry = (String, String, usize, Vec<String>); // name, node_id, reader_count, top readers
        let (groups, truncated) = tokio::task::spawn_blocking(move || {
            let nodes = graph
                .query_nodes(&pid, None, None, None, crate::handlers::NODE_SCAN_LIMIT)
                .unwrap_or_default();
            let truncated = nodes.len() >= crate::handlers::NODE_SCAN_LIMIT;
            // node_id -> (name, file, line) for reader labelling.
            let by_id: std::collections::HashMap<&str, (&str, &str, u32)> = nodes
                .iter()
                .map(|n| {
                    (
                        n.node_id.as_str(),
                        (n.name.as_str(), n.file_path.as_str(), n.start_line),
                    )
                })
                .collect();

            let mut groups: BTreeMap<&'static str, Vec<Entry>> = BTreeMap::new();
            for n in &nodes {
                let Some(cat) = category(n) else { continue };
                if let Some(sc) = &scope
                    && !n.file_path.as_str().to_lowercase().contains(sc.as_str())
                {
                    continue;
                }
                let readers = graph
                    .find_incoming_edges_with_kind(&pid, None, &n.node_id, 200)
                    .unwrap_or_default();
                let count = readers.len();
                // Catalog rows show TWO exemplar readers (name:line only) —
                // the full reader list is get_setting's job. Four full-path
                // labels per row made the catalog a 58K-char dump.
                let mut top: Vec<String> = readers
                    .iter()
                    .filter_map(|(src, _kind, _w)| {
                        by_id
                            .get(src.as_str())
                            .map(|(name, _file, line)| format!("{name}:{line}"))
                    })
                    .take(2)
                    .collect();
                top.dedup();
                groups.entry(cat).or_default().push((
                    n.name.clone(),
                    n.node_id.clone(),
                    count,
                    top,
                ));
            }
            // Most-read settings first inside each category — those are the
            // minefield the PO worries about.
            for list in groups.values_mut() {
                list.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
            }
            (groups, truncated)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let total: usize = groups.values().map(Vec::len).sum();
        let mut out = format!(
            "# Settings catalog — {} setting(s){}\n",
            total,
            req.scope
                .as_deref()
                .map(|s| format!(" (scope: {s})"))
                .unwrap_or_default()
        );
        if truncated {
            out.push_str("⚠ node scan hit the node-scan cap — catalog may be incomplete.\n");
        }
        if total == 0 {
            out.push_str(
                "\nNo settings found. Either the project has none indexed (run \
                 update_project) or the scope filter excluded everything.\n",
            );
        }
        let per_cat = req.max_per_category.clamp(5, 500);
        for (cat, list) in &groups {
            out.push_str(&format!("\n## {cat} — {}\n", list.len()));
            for (name, _id, count, top) in list.iter().take(per_cat) {
                out.push_str(&format!("- **{name}** — {count} reader(s)"));
                if !top.is_empty() {
                    out.push_str(&format!(": {}", top.join("; ")));
                }
                out.push('\n');
            }
            if list.len() > per_cat {
                out.push_str(&format!("  ... and {} more\n", list.len() - per_cat));
            }
        }
        out.push_str(
            "\nnext: get_setting(name=<setting>) for every usage site with lines + test \
             guidance; map_guards_and_settings(scope=<area>) for the role/permission axis.\n",
        );
        out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_get_setting(
        &self,
        req: crate::models::GetSettingRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let _rec = self.ensure_project_record(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let name_q = req.name.clone();

        let result = tokio::task::spawn_blocking(move || {
            let mut nodes = graph
                .query_nodes(&pid, None, Some(&name_q), None, 50_000)
                .unwrap_or_default();
            // Store-read settings live as PROPERTY nodes named by their
            // terminal segment (IsMaster) inside the store class namespace
            // (ConfigSettings.Multitenant) — a dotted query like
            // `ConfigSettings.Multitenant.IsMaster` matches no node NAME.
            // Retry on the terminal segment and keep only nodes whose
            // namespace+name composite equals the dotted query.
            if nodes.iter().all(|n| category(n).is_none()) && name_q.contains('.') {
                let terminal = name_q.rsplit('.').next().unwrap_or(&name_q);
                let dotted_lower = name_q.to_lowercase();
                nodes = graph
                    .query_nodes(&pid, None, Some(terminal), None, 50_000)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|n| {
                        let composite = format!("{}.{}", n.namespace, n.name).to_lowercase();
                        composite == dotted_lower
                            || composite.ends_with(&format!(".{dotted_lower}"))
                    })
                    .collect();
            }
            let mut candidates: Vec<&engram_graph::Node> =
                nodes.iter().filter(|n| category(n).is_some()).collect();
            // Exact name beats substring hits.
            if candidates.len() > 1 {
                let exact: Vec<&engram_graph::Node> = candidates
                    .iter()
                    .copied()
                    .filter(|n| n.name.eq_ignore_ascii_case(&name_q))
                    .collect();
                if !exact.is_empty() {
                    candidates = exact;
                }
            }
            if candidates.is_empty() {
                return Err(format!(
                    "No setting matching '{name_q}'. list_settings shows the catalog; \
                     the name must match a web.config key, connection string, state key, \
                     or settings-store member."
                ));
            }
            if candidates.len() > 1 {
                let mut msg = format!(
                    "AMBIGUOUS: {} settings match '{name_q}'. Re-call with the exact name:\n",
                    candidates.len()
                );
                for n in candidates.iter().take(10) {
                    msg.push_str(&format!(
                        "- {} [{}] ({})\n",
                        n.name, n.node_type, n.file_path
                    ));
                }
                return Err(msg);
            }
            let node = candidates[0];

            // All usage sites, labelled with the containing symbol + line.
            let by_id: std::collections::HashMap<&str, (&str, &str, u32)> = nodes
                .iter()
                .map(|n| {
                    (
                        n.node_id.as_str(),
                        (n.name.as_str(), n.file_path.as_str(), n.start_line),
                    )
                })
                .collect();
            let incoming = graph
                .find_incoming_edges_with_kind(&pid, None, &node.node_id, 500)
                .unwrap_or_default();
            let mut readers: Vec<(String, String)> = Vec::new(); // (kind, label)
            for (src, kind, _w) in &incoming {
                let label = match by_id.get(src.as_str()) {
                    Some((name, file, line)) => format!("{name} — {file}:{line}"),
                    None => match graph.get_node(&pid, src) {
                        Ok(Some(n)) => {
                            format!("{} — {}:{}", n.name, n.file_path, n.start_line)
                        }
                        _ => src.clone(),
                    },
                };
                readers.push((kind.as_str().to_string(), label));
            }
            readers.sort();
            readers.dedup();
            Ok((
                node.name.clone(),
                node.node_type.clone(),
                node.file_path.as_str().to_string(),
                node.start_line,
                readers,
            ))
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let (name, node_type, file, line, readers) =
            result.map_err(|e| McpError::invalid_params(e, None))?;

        let mut out = format!(
            "# Setting: `{name}`\n\ntype: {node_type} | declared: {file}:{line} | usage sites: {}\n",
            readers.len()
        );
        if readers.is_empty() {
            out.push_str(
                "\nNo indexed usage sites. It may be read via patterns the extractor \
                 doesn't model yet — grep_project(\"<name>\") for a literal sweep.\n",
            );
        } else {
            let mut by_kind: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
            for (kind, label) in &readers {
                by_kind.entry(kind.as_str()).or_default().push(label);
            }
            for (kind, labels) in &by_kind {
                out.push_str(&format!("\n## {kind} — {}\n", labels.len()));
                for l in labels.iter().take(30) {
                    out.push_str(&format!("- {l}\n"));
                }
                if labels.len() > 30 {
                    out.push_str(&format!("  ... and {} more\n", labels.len() - 30));
                }
            }
            out.push_str(&format!(
                "\n## Test guidance\n\
                 - Toggle `{name}` and exercise EVERY usage site above — the behaviour \
                 fork lives at those lines.\n\
                 - Check the role/permission axis for each usage file: \
                 map_guards_and_settings(scope=<file's area>) — settings and guards \
                 frequently gate the SAME code path.\n\
                 - grep_project(\"{name}\") to catch string-built reads the graph missed.\n"
            ));
        }
        out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }
}

impl Engram {
    /// QA/test-plan intelligence: given the files a change touches, derive
    /// WHAT TO TEST — the settings that fork behaviour in that code, the
    /// roles/permissions that gate it, and the shared-state keys that
    /// couple it to other pages. This is the axis knowledge that otherwise
    /// lives only in the PO's head.
    pub async fn handle_derive_test_matrix(
        &self,
        req: crate::models::DeriveTestMatrixRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let _rec = self.ensure_project_record(&req.project_id).await?;
        if req.files.is_empty() {
            return Err(McpError::invalid_params(
                "files must contain at least one changed/planned file path",
                None,
            ));
        }
        let gen_ = self.get_active_generation(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let files: Vec<String> = req.files.iter().map(|f| f.replace('\\', "/")).collect();

        type Axis = BTreeMap<String, Vec<String>>; // axis value -> methods
        let (settings_axis, roles_axis, state_axis, unresolved) =
            tokio::task::spawn_blocking(move || {
                let mut settings_axis: Axis = BTreeMap::new();
                let mut roles_axis: Axis = BTreeMap::new();
                let mut state_axis: Axis = BTreeMap::new();
                let mut unresolved: Vec<String> = Vec::new();

                for file in &files {
                    let symbols: Vec<engram_graph::Node> = graph
                        .query_nodes(&pid, None, None, Some(file), 2_000)
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|n| {
                            n.file_path
                                .as_str()
                                .replace('\\', "/")
                                .eq_ignore_ascii_case(file)
                        })
                        .collect();
                    if symbols.is_empty() {
                        unresolved.push(file.clone());
                        continue;
                    }
                    for n in &symbols {
                        if n.node_type != "function" {
                            continue;
                        }
                        let label =
                            format!("{} ({}:{})", n.name, n.file_path.as_str(), n.start_line);

                        // Settings axis: what this method reads.
                        if let Ok(neigh) = graph.neighbors(
                            &pid,
                            engram_graph::EdgeKind::ReadsSetting,
                            &n.node_id,
                            20,
                        ) {
                            for (target, _) in neigh {
                                let key = graph
                                    .get_node(&pid, &target)
                                    .ok()
                                    .flatten()
                                    .map(|t| t.name)
                                    .unwrap_or(target);
                                settings_axis.entry(key).or_default().push(label.clone());
                            }
                        }

                        // Role axis: guard metadata the extractor recorded.
                        if let Some(meta) = n.metadata.as_ref() {
                            let roles = meta
                                .get("guard_roles")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            for r in roles.split(';').filter(|r| !r.trim().is_empty()) {
                                roles_axis
                                    .entry(r.trim().to_string())
                                    .or_default()
                                    .push(label.clone());
                            }
                            let checks = meta
                                .get("permission_checks")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            for c in checks.split(';').filter(|c| !c.trim().is_empty()) {
                                // Permission checks without an explicit role
                                // list still define a testable gate.
                                roles_axis
                                    .entry(format!("[gate] {}", c.trim()))
                                    .or_default()
                                    .push(label.clone());
                            }
                        }

                        // Shared-state axis: cross-page coupling.
                        for kind in [
                            engram_graph::EdgeKind::ReadsState,
                            engram_graph::EdgeKind::WritesState,
                        ] {
                            if let Ok(neigh) = graph.neighbors(&pid, kind, &n.node_id, 20) {
                                for (target, _) in neigh {
                                    if let Some(key) = target.strip_prefix("state:") {
                                        state_axis
                                            .entry(key.to_string())
                                            .or_default()
                                            .push(label.clone());
                                    }
                                }
                            }
                        }
                    }
                }
                for axis in [&mut settings_axis, &mut roles_axis, &mut state_axis] {
                    for v in axis.values_mut() {
                        v.sort();
                        v.dedup();
                    }
                }
                (settings_axis, roles_axis, state_axis, unresolved)
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let mut out = format!("# Test matrix — {} changed file(s)\n", req.files.len());
        if !unresolved.is_empty() {
            out.push_str(&format!(
                "⚠ {} file(s) not found in the index ({}) — matrix may be incomplete; \
                 run update_project if they are new.\n",
                unresolved.len(),
                unresolved
                    .iter()
                    .take(5)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }

        let render_axis = |title: &str, instruction: &str, axis: &Axis, out: &mut String| {
            if axis.is_empty() {
                return;
            }
            out.push_str(&format!("\n## {title} — {}\n{instruction}\n", axis.len()));
            for (value, methods) in axis.iter().take(40) {
                out.push_str(&format!(
                    "- **{value}** → {}\n",
                    methods
                        .iter()
                        .take(4)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("; ")
                ));
                if methods.len() > 4 {
                    out.push_str(&format!("  (+{} more sites)\n", methods.len() - 4));
                }
            }
        };
        render_axis(
            "Settings axis",
            "Test with each setting ON and OFF — the behaviour fork lives at these sites:",
            &settings_axis,
            &mut out,
        );
        render_axis(
            "Role / permission axis",
            "Run the changed flows as EACH of these roles (and one role NOT listed, expecting denial):",
            &roles_axis,
            &mut out,
        );
        render_axis(
            "Shared-state axis",
            "These Session/Application keys couple the change to OTHER pages — retest the listed sites after cross-page navigation:",
            &state_axis,
            &mut out,
        );

        if settings_axis.is_empty() && roles_axis.is_empty() && state_axis.is_empty() {
            out.push_str(
                "\nNo setting/role/state gates found for these files in the graph. \
                 Either the change is gate-free (plain rendering logic) or the gating \
                 happens in helpers: run get_method_edit_context on the changed \
                 methods and derive_test_matrix on the helper files it names.\n",
            );
        } else {
            out.push_str(&format!(
                "\n## Suggested priority\nStart with the combination: most-read setting × \
                 most-privileged role listed above, then the denial case. Full matrix: \
                 {} setting value-pairs × {} roles.\n",
                settings_axis.len().max(1) * 2,
                roles_axis.len().max(1)
            ));
            out.push_str(
                "\nnext: get_setting(name=<setting>) for every other usage of a setting \
                 before assuming a toggle is safe; map_guards_and_settings(scope=<area>) \
                 for gates in files you did NOT change but that share these keys.\n",
            );
        }
        out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }
}

/// Read 1-based inclusive line range from a file (best effort).
fn read_line_range(path: &std::path::Path, start: u32, end: u32) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let s = (start.max(1) as usize) - 1;
    let e = (end as usize).min(text.lines().count());
    if s >= e {
        return None;
    }
    Some(
        text.lines()
            .skip(s)
            .take(e - s)
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

impl Engram {
    /// LLM-authored wiki entry for one setting: what it controls, ON/OFF
    /// behaviour, user-type interactions, and test implications — built
    /// from the actual reader-method bodies and persisted to the
    /// business_logic namespace (path-stable `__settings/<name>.md`, so
    /// query_business_logic finds it by domain terms).
    pub async fn handle_describe_setting(
        &self,
        req: crate::models::DescribeSettingRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let rec = self.ensure_project_record(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let name_q = req.name.clone();
        let project_dir = rec.directory.clone();

        // Resolve the setting + top reader excerpts (blocking graph + disk IO).
        let ctx = tokio::task::spawn_blocking(move || {
            let mut nodes = graph
                .query_nodes(
                    &pid,
                    None,
                    Some(&name_q),
                    None,
                    crate::handlers::NODE_SCAN_LIMIT,
                )
                .unwrap_or_default();
            if nodes.iter().all(|n| category(n).is_none()) && name_q.contains('.') {
                let terminal = name_q.rsplit('.').next().unwrap_or(&name_q);
                let dotted_lower = name_q.to_lowercase();
                nodes = graph
                    .query_nodes(
                        &pid,
                        None,
                        Some(terminal),
                        None,
                        crate::handlers::NODE_SCAN_LIMIT,
                    )
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|n| {
                        let composite = format!("{}.{}", n.namespace, n.name).to_lowercase();
                        composite == dotted_lower
                            || composite.ends_with(&format!(".{dotted_lower}"))
                            || n.name.eq_ignore_ascii_case(&name_q)
                    })
                    .collect();
            }
            let node = nodes
                .iter()
                .find(|n| category(n).is_some() && n.name.eq_ignore_ascii_case(&name_q))
                .or_else(|| nodes.iter().find(|n| category(n).is_some()))
                .cloned()
                .ok_or_else(|| {
                    format!("No setting matching '{name_q}'. list_settings shows the catalog.")
                })?;

            let readers = graph
                .find_incoming_edges_with_kind(&pid, None, &node.node_id, 200)
                .unwrap_or_default();
            let mut excerpts: Vec<(String, String)> = Vec::new(); // (label, code)
            for (src, _kind, _w) in readers.iter().take(30) {
                if excerpts.len() >= 5 {
                    break;
                }
                let Ok(Some(r)) = graph.get_node(&pid, src) else {
                    continue;
                };
                if r.node_type != "function" {
                    continue;
                }
                let Ok(abs) = engram_core::safe_join(
                    std::path::Path::new(&project_dir),
                    r.file_path.as_str(),
                ) else {
                    continue;
                };
                // Cap each excerpt so one giant method doesn't eat the prompt.
                let end = r.end_line.min(r.start_line + 60);
                if let Some(code) = read_line_range(&abs, r.start_line, end) {
                    excerpts.push((
                        format!("{} ({}:{})", r.name, r.file_path, r.start_line),
                        code,
                    ));
                }
            }
            Ok::<_, String>((node.name.clone(), readers.len(), excerpts))
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::invalid_params(e, None))?;
        let (setting_name, reader_count, excerpts) = ctx;

        if excerpts.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "Setting '{setting_name}' has no readable reader-method bodies to describe \
                 from. get_setting(name=\"{setting_name}\") lists its raw usage sites."
            ))]));
        }

        let mut prompt = format!(
            "You are documenting the setting `{setting_name}` for a team wiki so that \
             developers and testers no longer depend on one person's memory. It has \
             {reader_count} usage sites; excerpts from the most important readers follow.\n\n"
        );
        for (label, code) in &excerpts {
            prompt.push_str(&format!("### {label}\n```\n{code}\n```\n\n"));
        }
        prompt.push_str(
            "From THESE excerpts only (never invent behaviour you cannot see), write:\n\
              1. WHAT IT CONTROLS: 1-2 sentences.\n\
             2. WHEN ENABLED vs DISABLED (or per value): the concrete behaviour difference, \
             citing the function names above.\n\
             3. USER-TYPE INTERACTIONS: roles/permissions checked in the same code paths, if any.\n\
             4. TEST IMPLICATIONS: what to toggle and which flows to exercise.\n\
             Plain markdown, max ~250 words. Write 'not visible in these excerpts' where true.",
        );

        let raw = self
            .state
            .dreaming
            .generate_text(&prompt, 2048, std::time::Duration::from_secs(120))
            .await
            .map_err(|e| {
                McpError::internal_error(format!("LLM unavailable for describe_setting: {e}"), None)
            })?;

        // Persist (path-stable upsert) so query_business_logic finds it.
        let doc_body = format!("# Setting: {setting_name}\n\n{raw}\n");
        {
            use engram_core::{ContentHash, DocIdStr, RelPath};
            let ps = self.ensure_project_runtime(&req.project_id).await?;
            let synthetic_path = format!("__settings/{setting_name}.md");
            let path_hash = ContentHash::compute(synthetic_path.as_bytes());
            let doc_id = DocIdStr::compute(&synthetic_path, 0, 0, &path_hash);
            let chunk_id = {
                let h = blake3::hash(synthetic_path.as_bytes());
                let mut b = [0u8; 8];
                b.copy_from_slice(&h.as_bytes()[..8]);
                u64::from_le_bytes(b)
            };
            let content_hash = ContentHash::compute(doc_body.as_bytes());
            let doc = engram_index::IndexDoc {
                generation: 0,
                chunk_id,
                path: RelPath::new(&synthetic_path),
                language: "markdown".into(),
                content: doc_body.clone(),
                namespace: engram_core::namespaces::NAMESPACE_BUSINESS_LOGIC.into(),
                author: None,
                timestamp: None,
                start_line: 0,
                end_line: 0,
                doc_id: doc_id.0,
                content_hash: content_hash.0,
            };
            ps.search
                .index_docs(
                    &req.project_id,
                    std::slice::from_ref(&doc),
                    &tokio_util::sync::CancellationToken::new(),
                )
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        }

        let mut out = doc_body;
        out.push_str(
            "\n_(persisted — retrieve later with query_business_logic; raw sites via get_setting)_\n",
        );
        out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(node_type: &str, name: &str, ns: &str) -> engram_graph::Node {
        engram_graph::Node {
            node_id: format!("test:{name}"),
            name: name.into(),
            node_type: node_type.into(),
            namespace: ns.into(),
            file_path: "a/b.vb".into(),
            language: "vbnet".into(),
            start_line: 1,
            end_line: 2,
            generation: 1,
            metadata: None,
        }
    }

    #[test]
    fn categories_cover_all_setting_sources() {
        assert_eq!(
            category(&node("app_setting", "MaxUpload", "")),
            Some("web.config appSettings")
        );
        assert_eq!(
            category(&node("connection_string", "MainDb", "")),
            Some("connection strings")
        );
        assert!(
            category(&node("global_state", "Session:CartID", ""))
                .unwrap()
                .contains("state")
        );
        assert!(
            category(&node("property", "IsMaster", "ConfigSettings.Multitenant"))
                .unwrap()
                .contains("store")
        );
        // VB fallback shape: full dotted path in name, empty namespace.
        assert!(
            category(&node("property", "ConfigSettings.Multitenant.IsMaster", ""))
                .unwrap()
                .contains("store")
        );
        assert_eq!(
            category(&node("property", "Customer.Name", "")),
            None,
            "non-settings dotted properties must not classify as settings"
        );
        assert_eq!(category(&node("function", "SaveUser", "UserService")), None);
        assert_eq!(category(&node("class", "Foo", "settings")), None);
    }
}
