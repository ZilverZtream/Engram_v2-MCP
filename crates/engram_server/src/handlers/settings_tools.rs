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

fn setting_label(node: &engram_graph::Node) -> String {
    if is_store_accessor(node) && !node.namespace.is_empty() && !node.name.contains('.') {
        format!("{}.{}", node.namespace, node.name)
    } else {
        node.name.clone()
    }
}

/// Resolve catalog identity before consulting reader evidence; never choose an
/// arbitrary first substring match, including in generated descriptions.
fn resolve_setting(
    graph: &engram_graph::GraphStore,
    pid: &str,
    query: &str,
) -> Result<engram_graph::Node, String> {
    if query.trim().is_empty() {
        return Err("setting name must not be blank".into());
    }
    let terminal = query.rsplit('.').next().unwrap_or(query);
    let nodes = graph
        .query_nodes(pid, None, Some(terminal), None, 50_001)
        .map_err(|e| format!("Setting lookup failed: {e}"))?;
    if nodes.len() > 50_000 {
        return Err("Setting candidate scan reached 50000 nodes; lookup is incomplete and cannot establish unique identity".into());
    }
    let mut candidates: Vec<_> = nodes
        .into_iter()
        .filter(|n| category(n).is_some())
        .filter(|n| {
            !query.contains('.')
                || n.name.eq_ignore_ascii_case(query)
                || format!("{}.{}", n.namespace, n.name)
                    .to_lowercase()
                    .ends_with(&format!(".{}", query.to_lowercase()))
                || format!("{}.{}", n.namespace, n.name).eq_ignore_ascii_case(query)
        })
        .collect();
    let exact: Vec<_> = candidates
        .iter()
        .filter(|n| {
            n.name.eq_ignore_ascii_case(query)
                || format!("{}.{}", n.namespace, n.name).eq_ignore_ascii_case(query)
        })
        .cloned()
        .collect();
    if !exact.is_empty() {
        candidates = exact;
    }
    if candidates.len() != 1 {
        return Err(format!(
            "{}: {} settings match '{query}'. Use list_settings and supply an exact qualified name. Candidates: {}",
            if candidates.is_empty() {
                "NOT_FOUND"
            } else {
                "AMBIGUOUS"
            },
            candidates.len(),
            candidates
                .iter()
                .take(10)
                .map(|n| format!("{}.{} ({})", n.namespace, n.name, n.file_path))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    Ok(candidates.remove(0))
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
            let node = resolve_setting(&graph, &pid, &name_q)?;
            let mut warnings = vec!["Locations are indexed containing-symbol declarations; current source positions and runtime enforcement are unverified.".to_string()];
            let incoming = graph
                .find_incoming_edges_with_kind(&pid, None, &node.node_id, 501)
                .map_err(|e| format!("Setting usage lookup failed: {e}"))?;
            if incoming.len() > 500 { warnings.push("Usage traversal truncated at 500 sites.".into()); }
            let mut readers: Vec<(String, String)> = Vec::new(); // (kind, label)
            // Role/permission co-occurrence: the guards on the READER
            // methods tell testers which user types interact with this
            // setting/permission — the exact "settings × roles" knowledge
            // the PO otherwise carries alone.
            let mut co_roles: std::collections::BTreeMap<String, usize> = Default::default();
            for (src, kind, _w) in incoming.iter().take(500) {
                let reader_node = graph.get_node(&pid, src).map_err(|e| format!("Usage node lookup failed: {e}"))?;
                let label = match &reader_node {
                    Some(n) => format!("{} ? {}:{}", n.name, n.file_path, n.start_line),
                    None => { warnings.push(format!("Usage node unavailable: {src}")); src.clone() },
                };
                if let Some(meta) = reader_node.as_ref().and_then(|n| n.metadata.as_ref()) {
                    for key in ["guard_roles", "permission_checks"] {
                        if let Some(v) = meta.get(key).and_then(|v| v.as_str()) {
                            for g in v.split(';').filter(|g| !g.trim().is_empty()) {
                                *co_roles.entry(g.trim().to_string()).or_default() += 1;
                            }
                        }
                    }
                }
                readers.push((kind.as_str().to_string(), label));
            }
            readers.sort();
            readers.dedup();
            Ok::<_, String>((
                setting_label(&node),
                node.node_type.clone(),
                node.file_path.as_str().to_string(),
                node.start_line,
                readers,
                co_roles,
                warnings,
            ))
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let (name, node_type, file, line, readers, co_roles, warnings) =
            result.map_err(|e| McpError::invalid_params(e, None))?;

        let mut out = format!(
            "# Setting: `{name}`\n\ntype: {node_type} | declared: {file}:{line} | usage sites: {}\n",
            readers.len()
        );
        for warning in warnings {
            out.push_str(&format!("\nCoverage: {warning}\n"));
        }
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
            if !co_roles.is_empty() {
                out.push_str(&format!(
                    "\n## Role / permission co-occurrence — {}\n\
                     These checks co-occur in indexed reader metadata. This does not prove \
                     that they guard this setting or enforce authorization:\n",
                    co_roles.len()
                ));
                let mut rows: Vec<(usize, &String)> =
                    co_roles.iter().map(|(g, n)| (*n, g)).collect();
                rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
                for (n, g) in rows.into_iter().take(12) {
                    out.push_str(&format!("- `{g}` (co-occurs in {n} reader(s))\n"));
                }
            }
            out.push_str(&format!(
                "\n## Test guidance\n\
                 - Inspect the value type and source conditions for `{name}` before choosing \
                 test values; do not assume a boolean setting.\n\
                 - Check the role/permission axis for each usage file: \
                 map_guards_and_settings(scope=<file's area>) — settings and guards \
                 must be checked in source to establish which branches they govern.\n\
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
        if req.files.is_empty()
            || req.files.len() > 100
            || req.files.iter().any(|file| file.trim().is_empty())
        {
            return Err(McpError::invalid_params(
                "files must contain 1-100 nonblank changed/planned file paths",
                None,
            ));
        }
        let gen_ = self.get_active_generation(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let files: Vec<String> = req.files.iter().map(|f| f.replace('\\', "/")).collect();
        let rule_files = files.clone();
        let axis_root = std::path::PathBuf::from(&_rec.directory);

        type Axis = BTreeMap<String, Vec<String>>; // axis value -> methods
        let (settings_axis, setting_labels, roles_axis, state_axis, unresolved, coverage_notes, companion_context, member_observations) =
            tokio::task::spawn_blocking(move || {
                let mut settings_axis: Axis = BTreeMap::new();
                let mut setting_labels: BTreeMap<String, String> = BTreeMap::new();
                let mut roles_axis: Axis = BTreeMap::new();
                let mut state_axis: Axis = BTreeMap::new();
                let mut unresolved: Vec<String> = Vec::new();
                let mut coverage_notes = Vec::new();
                let mut source_bytes = 0;
                let mut companion_context = Vec::new();
                let mut member_observations = std::collections::BTreeSet::new();
                let mut seen = std::collections::HashSet::new();
                let mut pending = std::collections::VecDeque::new();
                for file in files {
                    // Graph file identities preserve case, even on Windows.
                    // Folding here can hide distinct files or suppress the
                    // diagnostic for a request that has no indexed identity.
                    if seen.insert(file.clone()) {
                        pending.push_back((file, true));
                    }
                }

                while let Some((file, discover_companion)) = pending.pop_front() {
                    let (usable, note, snapshot) = super::test_derivation::axis_source(
                        &graph,
                        &pid,
                        &axis_root,
                        &file,
                        &mut source_bytes,
                    );
                    if let Some(note) = note {
                        coverage_notes.push(note);
                    }
                    if !usable {
                        continue;
                    }
                    if discover_companion && let Some(snapshot) = snapshot.as_deref() {
                        match super::test_derivation::declared_axis_companion(&axis_root, &file, snapshot) {
                            Ok(Some(companion)) => {
                                companion_context.push(format!("{file} -> {companion}"));
                                if seen.insert(companion.clone()) {
                                    pending.push_back((companion, false));
                                }
                            }
                            Ok(None) => {}
                            Err(error) => coverage_notes.push(format!("{file}: {error}")),
                        }
                    }
                    let mut symbols = match graph.query_nodes_in_file(&pid, None, &file, 2_001) {
                        Ok(v) => v,
                        Err(e) => {
                            coverage_notes.push(format!("{file}: symbol lookup failed: {e}"));
                            continue;
                        }
                    };
                    if symbols.len() > 2_000 {
                        coverage_notes.push(format!("{file}: symbols truncated at 2000"));
                        symbols.truncate(2_000);
                    }
                    if symbols.is_empty() {
                        unresolved.push(file.clone());
                        continue;
                    }
                    // The already verified file snapshot is masked once per file,
                    // not re-read/scanned across the project for each graph edge.
                    let source_code = snapshot.as_deref()
                        .and_then(|bytes| std::str::from_utf8(bytes).ok())
                        .map(|source| crate::services::business_outcome_dependencies::executable_lines(source.trim_start_matches('\u{feff}'), true));
                    let mut nullable_observations = BTreeMap::new();
                    for n in &symbols {
                        if !matches!(
                            n.node_type.as_str(),
                            "function" | "property" | "field" | "file"
                        ) {
                            continue;
                        }
                        let label = if n.start_line == 0 {
                            format!("{} ({}: source use line unknown)", n.name, n.file_path.as_str())
                        } else {
                            format!("{} ({}:{})", n.name, n.file_path.as_str(), n.start_line)
                        };

                        // Settings axis: what this method reads.
                        match graph.neighbors(
                            &pid,
                            engram_graph::EdgeKind::ReadsSetting,
                            &n.node_id,
                            21,
                        ) {
                            Ok(mut neigh) => {
                                if neigh.len() > 20 {
                                    coverage_notes
                                        .push(format!("{label}: settings truncated at 20"));
                                    neigh.truncate(20);
                                }
                                for (target, _) in neigh {
                                    let setting_name = match graph.get_node(&pid, &target) {
                                        Ok(Some(t)) => setting_label(&t),
                                        Ok(None) => {
                                            let observation = nullable_observations.entry(target.clone()).or_insert_with(||
                                                super::test_derivation::nullable_member_observation(source_code.as_deref(), &file, &target));
                                            if let Some(observation) = observation {
                                                member_observations.insert(observation.clone());
                                                continue;
                                            }
                                            let recovery = super::test_derivation::setting_use_recovery(&pid, &file, &target);
                                            coverage_notes.push(format!("{label}: setting target {target} unavailable; its identity/value contract is incomplete. {recovery}"));
                                            "unresolved setting target".to_string()
                                        },
                                        Err(e) => {
                                            coverage_notes.push(format!(
                                                "{label}: setting lookup failed: {e}"
                                            ));
                                            "unresolved setting target".to_string()
                                        }
                                    };
                                    setting_labels.insert(target.clone(), setting_name);
                                    settings_axis.entry(target).or_default().push(label.clone());
                                }
                            }
                            Err(e) => {
                                coverage_notes.push(format!("{label}: settings lookup failed: {e}"))
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
                            match graph.neighbors(&pid, kind.clone(), &n.node_id, 21) {
                                Ok(mut neigh) => {
                                    if n.node_type == "file" && !neigh.is_empty() {
                                        let note = format!("{file}: state references have file-level ownership; no enclosing member was established for these sites");
                                        if !coverage_notes.contains(&note) {
                                            coverage_notes.push(note);
                                        }
                                    }
                                    if neigh.len() > 20 {
                                        coverage_notes.push(format!(
                                            "{label}: {} truncated at 20",
                                            kind.as_str()
                                        ));
                                        neigh.truncate(20);
                                    }
                                    for (target, _) in neigh {
                                        if let Some(key) = target.strip_prefix("state:") {
                                            state_axis
                                                .entry(key.to_string())
                                                .or_default()
                                                .push(label.clone());
                                        }
                                    }
                                }
                                Err(e) => coverage_notes
                                    .push(format!("{label}: {} lookup failed: {e}", kind.as_str())),
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
                (
                    settings_axis,
                    setting_labels,
                    roles_axis,
                    state_axis,
                    unresolved,
                    coverage_notes,
                    companion_context,
                    member_observations,
                )
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let (rule_cases, rule_notes) = match self.ensure_project_runtime(&req.project_id).await {
            Ok(runtime) => {
                let search = runtime.search.clone();
                let pid = req.project_id.clone();
                let root = std::path::PathBuf::from(_rec.directory);
                match tokio::task::spawn_blocking(move || {
                    super::test_derivation::collect(&search, &pid, gen_, &rule_files, &root)
                })
                .await
                {
                    Ok(Ok(result)) => result,
                    Ok(Err(error)) => (Vec::new(), vec![format!("rule provider failed: {error}")]),
                    Err(error) => (
                        Vec::new(),
                        vec![format!("rule provider task failed: {error}")],
                    ),
                }
            }
            Err(error) => (
                Vec::new(),
                vec![format!("rule provider unavailable: {error}")],
            ),
        };

        let mut out = format!("# Test matrix — {} changed file(s)\n", req.files.len());
        out.push_str("Evidence scope: indexed settings, permission and state references. Test discovery: not_run. Test execution: not_run. These are proposed cases, not verified outcomes.\n");
        if !companion_context.is_empty() {
            out.push_str("Direct code-behind context: declarations from source-verified markup. A companion is context, not a changed file; its indexed axes are separately checked below. No transitive helpers are inferred. Business-rule cases remain scoped to the explicitly requested files.\n");
            for relation in &companion_context {
                out.push_str(&format!("- {relation}\n"));
            }
        }
        if !member_observations.is_empty() {
            out.push_str("\n## Local nullable state observations\nSource presence checks, not configuration settings or proven shared-state effects.\n");
            for observation in &member_observations {
                out.push_str(&format!("- {observation}\n"));
            }
        }
        for note in coverage_notes.iter().chain(rule_notes.iter()) {
            out.push_str(&format!("INCOMPLETE: {note}\n"));
        }
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

        let render_axis = |title: &str,
                           instruction: &str,
                           axis: &Axis,
                           labels: Option<&BTreeMap<String, String>>,
                           out: &mut String| {
            if axis.is_empty() {
                return;
            }
            out.push_str(&format!("\n## {title} — {}\n{instruction}\n", axis.len()));
            if axis.len() > 40 {
                out.push_str(&format!(
                    "INCOMPLETE: showing 40 of {} axes; narrow the requested files.\n",
                    axis.len()
                ));
            }
            for (value, methods) in axis.iter().take(40) {
                let display = labels
                    .and_then(|labels| labels.get(value))
                    .map(|label| format!("{label} [node_id: {value}]"))
                    .unwrap_or_else(|| value.clone());
                out.push_str(&format!(
                    "- **{display}** → {}\n",
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
            "Inspect each referenced value at the listed sites before choosing cases: true/false for booleans, relevant enum values, and valid/missing/boundary values for paths, cultures, or other data. References alone do not establish a boolean switch or behaviour fork:",
            &settings_axis,
            Some(&setting_labels),
            &mut out,
        );
        render_axis(
            "Role / permission axis",
            "Exercise allowed and denied identities for each listed role or guard. Entries prefixed [gate] are guard functions, not role names; inspect those functions to determine roles, ownership, and tenant conditions before choosing identities:",
            &roles_axis,
            None,
            &mut out,
        );
        render_axis(
            "Shared-state axis",
            "These state references were found in the requested files. The listed sites do not establish cross-page consumers. Use trace_state_usage(state_type=<type>, state_key=<key>) to discover indexed readers/writers elsewhere before choosing cross-page scenarios:",
            &state_axis,
            None,
            &mut out,
        );

        out.push_str("\n## Source-linked proposed cases\nExpected outcomes below are inferred business rules whose method hashes match current source; confirm the requirements before implementing the tests.\n");
        render_rule_cases(&mut out, &rule_cases);
        if rule_cases.is_empty() {
            out.push_str("No source-verified rule cases available; run analyze_business_logic for the requested files to populate or refresh them.\n");
        }

        if settings_axis.is_empty() && roles_axis.is_empty() && state_axis.is_empty() {
            out.push_str(
                "\nNo usable setting/role/state axes were emitted. This does not establish \
                 that the change is gate-free; check incomplete evidence above and \
                 helper paths: run get_method_edit_context on the changed \
                 methods and derive_test_matrix on the helper files it names.\n",
            );
        } else {
            out.push_str(&format!(
                "\n## Suggested priority\nStart with the changed flow's normal case, then \
                 denied access and relevant boundary values. Discovered axes: \
                 {} setting/value references and {} role/guard entries. The graph does \
                 not establish their value types, privilege ordering, or a complete \
                 Cartesian test matrix; confirm those in the cited methods.\n",
                settings_axis.len(),
                roles_axis.len()
            ));
            out.push_str(
                "\nnext: get_setting(name=<qualified setting label>) for other indexed usage. Node IDs distinguish separate graph targets; get_setting accepts names, not node IDs. If identical qualified labels remain ambiguous, inspect their cited source sites with grep_project/get_chunk instead of selecting an arbitrary setting. map_guards_and_settings(scope=<area>) \
                 for gates in files you did NOT change but that share these keys.\n",
            );
        }
        out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }
}

/// Factor only byte-identical document provenance and repeated reaching text.
/// Case numbering and raw inferred requirements remain in their original order.
fn render_rule_cases(out: &mut String, cases: &[super::test_derivation::RuleCase]) {
    let mut documents: Vec<(&str, &str)> = Vec::new();
    for case in cases {
        let identity = (case.doc_id.as_str(), case.source_status.as_str());
        if !documents.contains(&identity) {
            documents.push(identity);
        }
    }
    let mut shared_reaching: Vec<Vec<&str>> = Vec::new();
    for (document_index, &(doc_id, source_status)) in documents.iter().enumerate() {
        let matching: Vec<_> = cases.iter().filter(|case| {
            case.doc_id == doc_id && case.source_status == source_status
        }).collect();
        let mut warnings: Vec<&str> = Vec::new();
        let mut qualifications: Vec<(&str, usize)> = Vec::new();
        for case in &matching {
            for warning in &case.source_warnings {
                if !warnings.contains(&warning.as_str()) { warnings.push(warning); }
            }
            if let Some((_, count)) = qualifications.iter_mut().find(|(text, _)| *text == case.reaching_qualification.as_str()) {
                *count += 1;
            } else {
                qualifications.push((&case.reaching_qualification, 1));
            }
        }
        let shared: Vec<&str> = qualifications.into_iter().filter_map(|(text, count)| {
            (count > 1 && !text.is_empty()).then_some(text)
        }).collect();
        let evidence = document_index + 1;
        out.push_str(&format!("\n### Matrix evidence D{evidence}\nSource: {source_status}\nEvidence: get_chunk(doc_id=\"{doc_id}\", namespace=\"business_logic\")\n"));
        out.push_str("Applies only to cases explicitly referencing this evidence ID. Method hashes do not verify inferred rules. Recovery returns the full source business document, not this generated matrix.\n");
        if !warnings.is_empty() {
            out.push_str(&format!("Analysis source checks requiring review \u{2014} document `{doc_id}`. Warning rule numbers refer to this document, not matrix case numbers. A matching method hash does not clear these warnings.\n"));
            for warning in warnings { out.push_str(&format!("- {warning}\n")); }
        }
        for (index, qualification) in shared.iter().enumerate() {
            out.push_str(&format!("Shared reaching qualification D{evidence}-Q{}: {qualification}\n", index + 1));
        }
        shared_reaching.push(shared);
    }
    for (index, case) in cases.iter().enumerate() {
        let document_index = documents.iter().position(|&(id, status)| {
            id == case.doc_id && status == case.source_status
        }).expect("every case has document evidence");
        let evidence = document_index + 1;
        let outcome_label = if case.expected_outcome_status == "blocked_pending_source_validation" {
            "Proposed rule: expected outcome BLOCKED pending source-validation review"
        } else if case.expected_outcome_status == "blocked_pending_helper_outcome" {
            "Proposed rule: expected outcome BLOCKED pending helper review"
        } else if case.reaching_prerequisites_require_review {
            "Proposed rule: expected outcome BLOCKED pending reaching-prerequisite review"
        } else { "Expected outcome (inferred)" };
        out.push_str(&format!("\n{}. {}: {}\n   Outcome status: {}\n   Evidence reference: [D{evidence}](#matrix-evidence-d{evidence})\n", index + 1, outcome_label, case.requirement, case.expected_outcome_status));
        out.push_str(&format!("   {}\n", case.source_diagnostic_qualification));
        if case.other_outcome_status != case.expected_outcome_status {
            out.push_str(&format!("   Other outcome qualification: {}\n", case.other_outcome_status));
        } else {
            out.push_str("   Other outcome qualification: same status as above.\n");
        }
        if let Some(shared_index) = shared_reaching[document_index].iter().position(|&text| text == case.reaching_qualification) {
            out.push_str(&format!("   Reaching qualification: D{evidence}-Q{} (above); prerequisites require review: {}.\n", shared_index + 1, case.reaching_prerequisites_require_review));
        } else {
            out.push_str(&format!("   {}\n", case.reaching_qualification));
        }
        if !case.outcome_dependencies.is_empty() {
            out.push_str(&format!("   Outcome dependencies: `{}`\n   Do not use the claimed return as an unconditional test oracle; inspect normal completion of the cited helper call.\n", crate::services::business_outcome_dependencies::render_dependencies(&case.outcome_dependencies)));
        }
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
    /// from the actual reader-method bodies. Read-only by default; explicit
    /// persist=true publishes to business_logic at `__settings/<name>.md`.
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
            let node = resolve_setting(&graph, &pid, &name_q)?;
            let readers = graph
                .find_incoming_edges_with_kind(&pid, None, &node.node_id, 201)
                .map_err(|e| format!("Setting usage lookup failed: {e}"))?;
            let mut excerpts: Vec<(String, String)> = Vec::new(); // (label, code)
            for (src, _kind, _w) in readers.iter().take(30) {
                if excerpts.len() >= 5 {
                    break;
                }
                let Some(r) = graph
                    .get_node(&pid, src)
                    .map_err(|e| format!("Reader lookup failed: {e}"))?
                else {
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
                crate::handlers::access_layer_tools::verify_indexed_source_span(
                    &graph,
                    &pid,
                    &project_dir,
                    r.file_path.as_str(),
                )?;
                // Cap each excerpt so one giant method doesn't eat the prompt.
                let end = r.end_line.min(r.start_line + 60);
                if let Some(code) = read_line_range(&abs, r.start_line, end) {
                    excerpts.push((
                        format!("{} ({}:{})", r.name, r.file_path, r.start_line),
                        code,
                    ));
                }
            }
            Ok::<_, String>((setting_label(&node), readers.len(), excerpts))
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
             developers and testers no longer depend on one person's memory. The bounded graph lookup returned \
             {reader_count} usage candidates; up to five indexed reader excerpts follow. Positions are unverified and excerpts may omit relevant conditions.\n\n"
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
             4. TEST IMPLICATIONS: appropriate values for the observed type and flows to exercise; do not assume boolean toggles.\n\
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

        // Explicit publication only; ordinary planning must not mutate the corpus.
        let doc_body = format!(
            "# Setting: {setting_name}\n\nCoverage: LLM-inferred from at most five indexed reader excerpts (up to 61 lines each). Source positions and runtime enforcement are unverified; this is not a complete usage inventory.\n\n{raw}\n"
        );
        if req.persist {
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
        if req.persist {
            out.push_str("\n_(persisted — retrieve later with query_business_logic; raw sites via get_setting)_\n");
        } else {
            out.push_str("\n_(preview only — not persisted; use persist=true to publish this inferred description; raw sites via get_setting)_\n");
        }
        out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }
}

impl Engram {
    /// LLM wiki for one DATABASE TABLE — the domain-entity layer: what the
    /// table represents, its columns, who reads/writes it, and test
    /// implications. Built from the graph (HasColumn + incoming
    /// QueriesTable/SqlCalls) plus accessor-method excerpts; persisted
    /// path-stably (__tables/<name>.md) like the setting wikis.
    pub async fn handle_describe_table(
        &self,
        req: crate::models::DescribeTableRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let rec = self.ensure_project_record(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let table = req.table.trim().to_lowercase();
        let project_dir = rec.directory.clone();

        let ctx = tokio::task::spawn_blocking(move || {
            let node_id = engram_core::ids::NodeId::table(&table).0;
            let node = graph
                .get_node(&pid, &node_id)
                .ok()
                .flatten()
                .ok_or_else(|| {
                    format!(
                        "No table '{table}' in the graph. analyze_database_intelligence \
                     lists tables; names are lowercase."
                    )
                })?;
            let mut columns: Vec<String> = graph
                .neighbors(&pid, engram_graph::EdgeKind::HasColumn, &node_id, 100)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|(cid, _)| cid.rsplit(':').next().map(str::to_string))
                .collect();
            columns.sort();
            columns.dedup();
            let incoming = graph
                .find_incoming_edges_with_kind(&pid, None, &node_id, 200)
                .unwrap_or_default();
            let mut excerpts: Vec<(String, String)> = Vec::new();
            for (src, _k, _w) in incoming.iter().take(40) {
                if excerpts.len() >= 6 {
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
                let end = r.end_line.min(r.start_line + 50);
                if let Some(code) = read_line_range(&abs, r.start_line, end) {
                    excerpts.push((
                        format!("{} ({}:{})", r.name, r.file_path, r.start_line),
                        code,
                    ));
                }
            }
            Ok::<_, String>((node.name.clone(), columns, incoming.len(), excerpts))
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::invalid_params(e, None))?;
        let (tname, columns, accessor_count, excerpts) = ctx;

        if excerpts.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "Table '{tname}' has no readable accessor bodies to describe from. \
                 get_sp_details / analyze_database_intelligence give the raw structure."
            ))]));
        }

        let mut prompt = format!(
            "You are documenting the database table `{tname}` for a team wiki. \
             It has {} known column(s): {}. {} code accessor(s); excerpts from \
             the most important follow.\n\n",
            columns.len(),
            columns.join(", "),
            accessor_count
        );
        for (label, code) in &excerpts {
            prompt.push_str(&format!("### {label}\n```\n{code}\n```\n\n"));
        }
        prompt.push_str(
            "From THESE excerpts only (never invent), write:\n\
             1. WHAT THE TABLE REPRESENTS: the domain entity, 1-2 sentences.\n\
             2. KEY COLUMNS: meanings you can actually see in the code.\n\
             3. WHO READS/WRITES IT: the workflows in the excerpts (cite functions).\n\
             4. TEST IMPLICATIONS: what to seed/verify when changes touch it.\n\
             Plain markdown, max ~250 words. Say 'not visible in these excerpts' where true.",
        );

        let raw = self
            .state
            .dreaming
            .generate_text(&prompt, 2048, std::time::Duration::from_secs(120))
            .await
            .map_err(|e| {
                McpError::internal_error(format!("LLM unavailable for describe_table: {e}"), None)
            })?;

        let doc_body = format!("# Table: {tname}\n\n{raw}\n");
        {
            use engram_core::{ContentHash, DocIdStr, RelPath};
            let ps = self.ensure_project_runtime(&req.project_id).await?;
            let synthetic_path = format!("__tables/{tname}.md");
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
        out.push_str("\n_(persisted — retrieve later with query_business_logic)_\n");
        out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;


    fn matrix_case(doc: &str, rule: &str, source: &str, reaching: &str) -> super::super::test_derivation::RuleCase {
        super::super::test_derivation::RuleCase {
            requirement: rule.into(), doc_id: doc.into(), source_status: source.into(),
            source_warnings: Vec::new(), expected_outcome_status: "inferred_unverified_outside_dependency_slice".into(),
            other_outcome_status: "inferred_unverified_outside_dependency_slice".into(),
            source_diagnostic_qualification: "association_unknown; no semantic certification".into(),
            reaching_prerequisites_require_review: false, reaching_qualification: reaching.into(),
            outcome_dependencies: Vec::new(),
        }
    }

    #[test]
    fn matrix_document_references_preserve_order_distinct_warnings_and_blocking() {
        let mut first = matrix_case("doc-a", "Rule alpha", "VERIFIED_METHOD_HASH A:12; inferred", "unknown reachability A");
        first.source_warnings = vec!["Rule 1: wrong identifier A".into()];
        first.expected_outcome_status = "blocked_pending_source_validation".into();
        first.other_outcome_status = "blocked_pending_helper_outcome".into();
        let mut second = matrix_case("doc-b", "Rule beta", "VERIFIED_METHOD_HASH B:90; inferred", "unique prerequisite B at94");
        second.source_warnings = vec!["Rule 1: different warning B".into()];
        second.expected_outcome_status = "blocked_pending_reaching_prerequisite_review".into();
        second.other_outcome_status = second.expected_outcome_status.clone();
        second.reaching_prerequisites_require_review = true;
        let mut third = matrix_case("doc-a", "Rule gamma", "VERIFIED_METHOD_HASH A:12; inferred", "unknown reachability A");
        third.source_warnings = vec!["Rule 2: later distinct warning A".into()];
        let mut out = String::new(); render_rule_cases(&mut out, &[first, second, third]);
        assert_eq!(out.matches("get_chunk(doc_id=\"doc-a\"").count(), 1);
        assert_eq!(out.matches("unknown reachability A").count(), 1);
        for warning in ["wrong identifier A", "different warning B", "later distinct warning A", "unique prerequisite B at94"] { assert!(out.contains(warning)); }
        let first = out.split("\n1. ").nth(1).unwrap().split("\n2. ").next().unwrap();
        let second = out.split("\n2. ").nth(1).unwrap().split("\n3. ").next().unwrap();
        let third = out.split("\n3. ").nth(1).unwrap();
        assert!(first.contains("BLOCKED pending source-validation") && first.contains("[D1](#matrix-evidence-d1)"));
        assert!(first.contains("Other outcome qualification: blocked_pending_helper_outcome"));
        assert!(second.contains("BLOCKED pending reaching-prerequisite") && second.contains("[D2](#matrix-evidence-d2)"));
        assert!(second.contains("Other outcome qualification: same status as above."));
        assert!(third.contains("Expected outcome (inferred): Rule gamma") && third.contains("[D1](#matrix-evidence-d1)"));
        assert!(third.contains("D1-Q1") && third.contains("prerequisites require review: false"));
    }

    #[test]
    fn matrix_same_document_different_source_status_is_not_collapsed() {
        let cases = [matrix_case("doc", "current rule", "current source", "unknown"), matrix_case("doc", "stale rule", "STALE source; withheld", "unknown")];
        let mut out = String::new(); render_rule_cases(&mut out, &cases);
        assert!(out.contains("Matrix evidence D1") && out.contains("Matrix evidence D2"));
        assert!(out.contains("current source") && out.contains("STALE source; withheld"));
        assert_eq!(out.matches("get_chunk(doc_id=\"doc\"").count(), 2);
        assert!(!out.contains("Shared reaching qualification"));
    }

    #[test]
    fn matrix_repeated_document_text_is_compact_without_losing_any_rule_or_qualification() {
        let source = "source identity, bounds and verification remain unverified semantically; ".repeat(8);
        let reaching = "unsupported tail; normal completion, scope and omissions must be reviewed; ".repeat(12);
        let cases: Vec<_> = (0..40).map(|i| matrix_case("doc", &format!("distinct rule {i}"), &source, &reaching)).collect();
        let mut out = String::new(); render_rule_cases(&mut out, &cases);
        assert_eq!(out.matches(&source).count(), 1); assert_eq!(out.matches(&reaching).count(), 1);
        for i in 0..40 { assert!(out.contains(&format!("Expected outcome (inferred): distinct rule {i}\n"))); }
        assert_eq!(out.matches("Evidence reference: [D1]").count(), 40);
        assert_eq!(out.matches("Outcome status: inferred_unverified_outside_dependency_slice").count(), 40);
        assert_eq!(out.matches("association_unknown; no semantic certification").count(), 40);
        assert_eq!(out.matches("Other outcome qualification: same status as above.").count(), 40);
        assert!(out.len() < 40 * (source.len() + reaching.len()), "repetition should be factored, not clipped");
        assert!(out.contains("not this generated matrix"));
    }

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
