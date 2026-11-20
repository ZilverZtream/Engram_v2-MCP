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
use crate::models::requests::TestMatrixFormat;
use crate::tools::Engram;
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use std::collections::BTreeMap;

const MATRIX_AXIS_LIMIT: usize = 40;
const MATRIX_SITE_LIMIT: usize = 4;

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
        "setting_key" => Some("literal-key settings getter observations (code)"),
        "app_setting" => Some("web.config appSettings"),
        "connection_string" => Some("connection strings"),
        "global_state" => Some("shared state keys (Session/Application/Cache)"),
        _ if is_store_accessor(node) => Some("settings-store accessors (code)"),
        _ => None,
    }
}

fn setting_label(node: &engram_graph::Node) -> String {
    if node.node_type == "setting_key" {
        return super::literal_settings::axis_label(node);
    }
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
        if let Some(files) = &req.source_files {
            super::current_settings::validate(files).map_err(|e| McpError::invalid_params(e.to_string(),None))?;
            if req.scope.is_some() { return Err(McpError::invalid_params("source_files and indexed scope must not be mixed",None)); }
            if req.offset_per_category != 0 {
                return Err(McpError::invalid_params(
                    "offset_per_category applies to the indexed catalog, without source_files",
                    None,
                ));
            }
        }
        let _rec = self.ensure_project_record(&req.project_id).await?;
        if let Some(files) = req.source_files {
            let root = std::path::PathBuf::from(_rec.directory);
            let pid = req.project_id;
            let budget = self.state.memory_budget.clone();
            let report = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
                Ok(serde_json::to_string_pretty(&super::current_settings::collect(&root,&pid,&files,None,200,&budget)?)?)
            }).await.map_err(|e| McpError::internal_error(e.to_string(),None))?
                .map_err(|e| McpError::invalid_params(e.to_string(),None))?;
            return Ok(CallToolResult::success(vec![Content::text(report)]));
        }
        let gen_ = self.get_active_generation(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let scope = req.scope.clone().filter(|scope| !scope.is_empty());

        type Entry = (String, String, usize, Vec<String>); // name, node_id, reader_count, top readers
        let (groups, truncated) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let limit = crate::handlers::NODE_SCAN_LIMIT;
            let mut nodes = graph.query_nodes(&pid, None, None, scope.as_deref(), limit + 1)?;
            let truncated = nodes.len() > limit;
            nodes.truncate(limit);
            let by_id: std::collections::HashMap<&str, (&str, u32)> = nodes
                .iter()
                .map(|node| (node.node_id.as_str(), (node.name.as_str(), node.start_line)))
                .collect();

            let mut groups: BTreeMap<&'static str, Vec<Entry>> = BTreeMap::new();
            let mut literal_rows: BTreeMap<String, (usize, Vec<String>)> = BTreeMap::new();
            for n in &nodes {
                let Some(cat) = category(n) else { continue };
                if n.node_type == "setting_key" {
                    let entry = literal_rows.entry(n.name.clone()).or_default();
                    entry.0 += 1;
                    if entry.1.len() < 2 {
                        entry.1.push(format!("{}:{} ({})", n.file_path, n.start_line,
                            super::literal_settings::field(n,"literal_setting_getters")));
                    }
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
                            .map(|(name, line)| format!("{name}:{line}"))
                            .or_else(|| {
                                // The scoped inventory only contains declarations under that
                                // path. A caller in another area can still be an exemplar.
                                scope.as_ref()?;
                                graph.get_node(&pid, src).ok().flatten()
                                    .map(|node| format!("{}:{}", node.name, node.start_line))
                            })
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
            for (name, (count, sites)) in literal_rows {
                groups.entry("literal-key settings getter observations (code)").or_default()
                    .push((name, String::new(), count, sites));
            }
            // Most-read settings first inside each category — those are the
            // minefield the PO worries about.
            for list in groups.values_mut() {
                list.sort_by(|a, b| {
                    b.2.cmp(&a.2)
                        .then_with(|| a.0.cmp(&b.0))
                        .then_with(|| a.1.cmp(&b.1))
                });
            }
            Ok((groups, truncated))
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
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
        if groups.contains_key("literal-key settings getter observations (code)") {
            out.push_str("\nLiteral-key rows count indexed call observations; repeated labels do not prove a common physical store. Locations/getter spellings are indexed evidence, not current source or runtime values.\n");
        }
        if total == 0 {
            out.push_str(
                "\nNo settings found. Either the project has none indexed (run \
                 update_project) or the scope filter excluded everything.\n",
            );
        }
        let per_cat = req.max_per_category.clamp(5, 500);
        let offset = req.offset_per_category;
        let next_offset = offset.saturating_add(per_cat);
        if offset != 0 {
            out.push_str(&format!(
                "\nSkipped the first {offset} indexed settings in each category.\n"
            ));
        }
        let mut has_next_page = false;
        for (cat, list) in &groups {
            out.push_str(&format!("\n## {cat} — {}\n", list.len()));
            if offset != 0 && offset >= list.len() {
                out.push_str("- No settings in this page for this category.\n");
            }
            for (name, _id, count, top) in list.iter().skip(offset).take(per_cat) {
                out.push_str(&format!("- **{name}** — {count} reader(s)"));
                if !top.is_empty() {
                    out.push_str(&format!(": {}", top.join("; ")));
                }
                out.push('\n');
            }
            if list.len() > next_offset {
                has_next_page = true;
                out.push_str(&format!("  ... and {} more\n", list.len() - next_offset));
            }
        }
        if has_next_page {
            let arguments = serde_json::json!({
                "project_id": req.project_id,
                "scope": req.scope,
                "max_per_category": per_cat,
                "offset_per_category": next_offset,
            });
            out.push_str(&format!("\nNext catalog page: list_settings({arguments})\n"));
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
        let name = match (&req.name, &req.node_id) {
            (Some(name), None) if !name.trim().is_empty() => name.clone(),
            (None, Some(id)) if !id.trim().is_empty() && id.len() <= 4096
                && !id.chars().any(char::is_control) && req.source_files.is_none() => String::new(),
            _ => return Err(McpError::invalid_params(
                "Supply a nonblank setting name or one exact node_id. source_files requires name.",
                None,
            )),
        };
        if let Some(files) = &req.source_files {
            super::current_settings::validate(files).map_err(|e| McpError::invalid_params(e.to_string(),None))?;
        }
        let _rec = self.ensure_project_record(&req.project_id).await?;
        if let Some(files) = req.source_files {
            let root = std::path::PathBuf::from(_rec.directory);
            let pid = req.project_id;
            let budget = self.state.memory_budget.clone();
            let report = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
                Ok(serde_json::to_string_pretty(&super::current_settings::collect(&root,&pid,&files,Some(&name),200,&budget)?)?)
            }).await.map_err(|e| McpError::internal_error(e.to_string(),None))?
                .map_err(|e| McpError::invalid_params(e.to_string(),None))?;
            return Ok(CallToolResult::success(vec![Content::text(report)]));
        }
        let (gen_, source_view) = if req.node_id.is_some() {
            let runtime = self.ensure_project_runtime(&req.project_id).await?;
            let (generation, view) = self.pin_active_source_generation(&req.project_id, &runtime.search).await?;
            (generation, Some(std::sync::Arc::new(view)))
        } else {
            (self.get_active_generation(&req.project_id).await?, None)
        };
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let node_id = req.node_id;

        let literal_view = source_view.clone();
        let literal_graph = graph.clone();
        let literal_pid = pid.clone();
        let literal_query = name.clone();
        let root = std::path::PathBuf::from(&_rec.directory);
        let (literal, selected_node) = tokio::task::spawn_blocking(move || -> Result<_, String> {
            if let Some(id) = node_id {
                let node = literal_graph.get_node(&literal_pid, &id)
                    .map_err(|error| format!("Setting node lookup failed: {error}"))?
                    .filter(|node| category(node).is_some())
                    .ok_or_else(|| "The supplied node_id is not an indexed setting for this project".to_string())?;
                let revisions = literal_view.as_deref().and_then(|view| view.selected_file_revisions());
                if revisions.is_some_and(|files| files.get(node.file_path.as_str()) != Some(&node.generation))
                    || node.generation > gen_
                {
                    return Err("The setting node does not belong to its selected published source revision. Refresh the setting or QA matrix before using the node.".into());
                }
                let report = if node.node_type == "setting_key" {
                    Some(super::literal_settings::render_observations(
                        &literal_graph, &literal_pid, &root, std::slice::from_ref(&node),
                        revisions.is_some(),
                    )?)
                } else {
                    None
                };
                Ok((report, Some(node)))
            } else {
                Ok((super::literal_settings::lookup(&literal_graph, &literal_pid, &root, &literal_query)?, None))
            }
        })
            .await.map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::invalid_params(e, None))?;
        if let Some(mut text) = literal {
            text.push_str(&self.freshness_footer(&req.project_id, gen_).await);
            return Ok(CallToolResult::success(vec![Content::text(text)]));
        }

        let result = tokio::task::spawn_blocking(move || {
            let node = if let Some(node) = selected_node { node } else { resolve_setting(&graph, &pid, &name)? };
            let mut warnings = vec!["Locations are indexed containing-symbol declarations; current source positions and runtime enforcement are unverified.".to_string()];
            let revisions = source_view.as_deref().and_then(|view| view.selected_file_revisions());
            let mut withheld = 0;
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
                if let Some(revisions) = revisions
                    && reader_node.as_ref().is_some_and(|reader|
                        revisions.get(reader.file_path.as_str()) != Some(&reader.generation))
                {
                    withheld += 1;
                    continue;
                }
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
            if withheld > 0 {
                warnings.push(format!("{withheld} indexed usage entries withheld outside the selected source revision."));
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
        mut req: crate::models::DeriveTestMatrixRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let _rec = self.ensure_project_record(&req.project_id).await?;
        if req.knowledge_before.as_deref()
            .is_some_and(|date| !super::test_derivation::valid_yyyy_mm_dd(date))
        {
            return Err(McpError::invalid_params(
                "knowledge_before must be YYYY-MM-DD",
                None,
            ));
        }
        if req.files.is_empty()
            || req.files.len() > 100
            || req.files.iter().any(|file| file.trim().is_empty())
        {
            return Err(McpError::invalid_params(
                "files must contain 1-100 nonblank changed/planned file paths",
                None,
            ));
        }
        let source_runtime = self.ensure_project_runtime(&req.project_id).await;
        let pinned = match source_runtime.as_ref() {
            Ok(runtime) => self.pin_active_source_generation(&req.project_id, &runtime.search)
                .await.map_err(|error| error.message.to_string()),
            Err(error) => Err(error.message.to_string()),
        };
        let (gen_, matrix_view, source_selection_note) = match pinned {
            Ok((generation, view)) => (generation, Some(std::sync::Arc::new(view)), None),
            Err(note) => (self.get_active_generation(&req.project_id).await?, None, Some(note)),
        };
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let files: Vec<String> = req.files.iter().map(|f| f.replace('\\', "/")).collect();
        if let Some(scopes)=req.methods.as_mut() {
            for scope in scopes.iter_mut(){scope.file_path=scope.file_path.replace('\\',"/");}
            crate::services::method_scope::validate(scopes,&files).map_err(|e|McpError::invalid_params(e.to_string(),None))?;
        }
        let method_scopes=req.methods.clone();
        let rule_scopes=req.methods.clone();
        let scoped_canonical=req.methods.is_some();
        let rule_files = files.clone();
        let axis_root = std::path::PathBuf::from(&_rec.directory);
        let canonical_files = files.clone();
        let canonical_root = axis_root.clone();
        let canonical_intent = req.change_intent.clone();
        // Authored requirements remain available even when there are no usable
        // inferred rules. A historical cutoff must never borrow today's policy.
        let project_policy_sources = if req.knowledge_before.is_some() {
            crate::services::project_policy::PolicyContext::historical_unavailable()
        } else {
            let policy_root = axis_root.clone();
            let policy_files = files.clone();
            let policy_project = req.project_id.clone();
            tokio::task::spawn_blocking(move || {
                crate::services::project_policy::resolve(&policy_root, &policy_files, &[])
                    .with_source_recovery(&policy_project)
            })
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?
        };
        let (configured_risk_pack, configured_risk_notes) =
            super::test_derivation::load_configured_risk_pack_before(
                &self.state.cfg.data_dir,
                &axis_root,
                req.knowledge_before.as_deref(),
            );
        let configured_risk_count = configured_risk_pack.len();
        let source_chunk_target = self.state.cfg.max_chunks_per_file;
        let allow_current_settings = req.knowledge_before.is_none();
        let current_settings_budget = self.state.memory_budget.clone();
        let axis_view = matrix_view.clone();
        let axis_selection_note = source_selection_note.clone();

        type Axis = BTreeMap<String, Vec<String>>; // axis value -> methods
        let (settings_axis, setting_labels, indexed_setting_ids, roles_axis, state_axis, runtime_axes, sql_parameter_axes, identity_batch_axes, bulk_copy_axes, unresolved, coverage_notes, companion_context, member_observations, constructor_context) =
            tokio::task::spawn_blocking(move || {
                let mut settings_axis: Axis = BTreeMap::new();
                let mut setting_labels: BTreeMap<String, String> = BTreeMap::new();
                let mut indexed_setting_ids = std::collections::BTreeSet::new();
                let mut roles_axis: Axis = BTreeMap::new();
                let mut state_axis: Axis = BTreeMap::new();
                let mut runtime_axes: Axis = BTreeMap::new();
                let mut sql_parameter_axes: Axis = BTreeMap::new();
                let mut identity_batch_axes: Axis = BTreeMap::new();
                let mut bulk_copy_axes: BTreeMap<String, Vec<serde_json::Value>> = BTreeMap::new();
                let mut unresolved: Vec<String> = Vec::new();
                let mut coverage_notes = configured_risk_notes;
                if let Some(note) = axis_selection_note {
                    coverage_notes.push(format!("Published source unavailable; indexed axes withheld: {note}"));
                }
                let selected = axis_view.as_ref().and_then(|view| view.selected_file_revisions());
                let mut source_bytes = 0;
                let mut companion_context = Vec::new();
                let mut member_observations = std::collections::BTreeSet::new();
                let mut constructor_context = Vec::new();
                let mut seen = std::collections::HashSet::new();
                let mut pending = std::collections::VecDeque::new();
                // A discovered code-behind may introduce VB after a markup-only
                // request. Capture its actual parser once, only when needed.
                let mut vb_extraction = None;
                let explicit_files: std::collections::HashSet<_> = files.iter().cloned().collect();
                let mut current_setting_files = 0usize;
                let mut current_setting_rows = 0usize;
                let extraction = engram_index::SourceExtractionExpectation::for_project(
                    &axis_root, source_chunk_target, std::iter::empty::<&str>(),
                ).map_err(|e| e.to_string());
                for file in files {
                    // Graph file identities preserve case, even on Windows.
                    // Folding here can hide distinct files or suppress the
                    // diagnostic for a request that has no indexed identity.
                    if seen.insert(file.clone()) {
                        pending.push_back((file, true, None::<String>));
                    }
                }

                while let Some((file, discover_companion, companion_relation)) = pending.pop_front() {
                    if method_scopes.as_ref().is_some_and(|scopes|!scopes.iter().any(|scope|scope.file_path==file)){continue;}

                    let needs_vb = engram_index::vb_extractor::requires_parser_provenance(&file);
                    if vb_extraction.is_none() && needs_vb {
                        vb_extraction = Some(engram_index::SourceExtractionExpectation::for_project(
                            &axis_root, source_chunk_target, std::iter::once(file.as_str()),
                        ).map_err(|e| e.to_string()));
                    }
                    let expected = if needs_vb { vb_extraction.as_ref().expect("VB expectation captured") }
                        else { &extraction };
                    let (usable, note, snapshot) = if axis_view.is_some() { super::test_derivation::axis_source(
                        &graph,
                        &pid,
                        &axis_root,
                        gen_,
                        selected,
                        expected.as_ref().map_err(String::as_str),
                        &file,
                        &mut source_bytes,
                    ) } else { (false, None, None) };
                    if let Some(note) = note {
                        coverage_notes.push(note);
                    }
                    if method_scopes.is_some() && (!usable || snapshot.is_none()) {
                        coverage_notes.push(format!("{file}: method-scoped axes withheld because selected method source could not be admitted; file-wide fallback was not used."));
                        continue;
                    }
                    if (!usable || snapshot.is_none()) && needs_vb && explicit_files.contains(&file) {
                        if !allow_current_settings {
                            coverage_notes.push(format!("{file}: current literal-setting fallback withheld under knowledge_before; current bytes are not historical evidence"));
                        } else if current_setting_files >= 16 || current_setting_rows >= 200 {
                            coverage_notes.push(format!("{file}: current setting observations omitted at16files/200observations; coverage remains incomplete"));
                        } else {
                            current_setting_files += 1;
                            match super::current_settings::collect(&axis_root,&pid,std::slice::from_ref(&file),None,200-current_setting_rows,&current_settings_budget) {
                                Ok(report) => {
                                    current_setting_rows += report.observations.len();
                                    coverage_notes.extend(report.incomplete);
                                    for site in report.observations {
                                        let id=format!("current:{}:{}:{}:{}",site.file_path,site.observation.line,site.raw_blake3,site.observation.name);
                                        setting_labels.insert(id.clone(),format!("{} (current file literal; getter {}; compiler/store binding and value unverified)",site.observation.name,site.observation.getter));
                                        settings_axis.entry(id).or_default().push(format!("{}:{} | raw BLAKE3 {} | current code observation only; review task applicability",site.file_path,site.observation.line,site.raw_blake3));
                                    }
                                    coverage_notes.push(format!("{file}: current source setting observations were collected independently; unavailable indexed role/state/business axes remain incomplete. No compiler/project source inventory or index refresh was performed for these literal observations."));
                                }
                                Err(error) => coverage_notes.push(format!("{file}: current source setting observations unavailable: {error}")),
                            }
                        }
                    }
                    if !usable {
                        continue;
                    }
                    let selected_revision = selected.and_then(|files| files.get(&file)).copied();
                    let scoped_ranges=if let Some(scopes)=&method_scopes {
                        match graph.query_nodes_in_file(&pid,Some("function"),&file,1025)
                            .and_then(|mut nodes|{
                                anyhow::ensure!(nodes.len()<=1024,"Method inventory exceeds1024 declarations");
                                if let Some(revision) = selected_revision {
                                    nodes.retain(|node| node.generation == revision);
                                }
                                crate::services::method_scope::declarations(&nodes,&file,scopes)
                                    .map(|selected|selected.iter().map(|n|(n.start_line,n.end_line)).collect::<Vec<_>>())
                            }) {
                            Ok(ranges)=>Some(ranges),
                            Err(error)=>{coverage_notes.push(format!("{file}: method scope unresolved: {error}; no inferred axes admitted for this file."));continue;}
                        }
                    }else{None};
                    if needs_vb && allow_current_settings
                        && let (Some(ranges), Some(source)) = (
                            &scoped_ranges,
                            snapshot.as_deref().and_then(|bytes| std::str::from_utf8(bytes).ok()),
                        )
                    {
                        let starts: Vec<_> = ranges.iter().map(|(start, _)| *start).collect();
                        constructor_context.extend(crate::services::business_member_origins::collect_scoped(
                            &file, source, &starts,
                        ));
                    }
                    if let Some(relation) = companion_relation {
                        companion_context.push(relation);
                    }
                    if method_scopes.is_none() && discover_companion && let Some(snapshot) = snapshot.as_deref() {
                        match super::test_derivation::declared_axis_companion(&axis_root, &file, snapshot) {
                            Ok(Some((companion, relation_verified))) => {
                                let relation = format!("{file} -> {companion}");
                                if relation_verified {
                                    companion_context.push(relation.clone());
                                }
                                if seen.insert(companion.clone()) {
                                    pending.push_back((companion, false, (!relation_verified).then_some(relation)));
                                }
                            }
                            Ok(None) => {}
                            Err(error) => coverage_notes.push(format!("{file}: {error}")),
                        }
                    }
                    if method_scopes.is_none() { match super::test_derivation::registered_control_hosts(&graph, &pid, &file) {
                        Ok((hosts, truncated)) => {
                            if truncated {
                                coverage_notes.push(format!(
                                    "{file}: registered-control hosts truncated at 32"
                                ));
                            }
                            for host in hosts {
                                if seen.insert(host.clone()) {
                                    pending.push_back((
                                        host.clone(),
                                        true,
                                        Some(format!("{host} registers {file}")),
                                    ));
                                }
                            }
                        }
                        Err(error) => coverage_notes.push(format!("{file}: {error}")),
                    }
                    }
                    // Configured risk rules match the verified file snapshot, so
                    // they apply even when the parser emitted no symbols for a
                    // markup or client-only file.
                    if let Some(source) = snapshot
                        .as_deref()
                        .and_then(|bytes| std::str::from_utf8(bytes).ok())
                    {
                        let scoped_source=scoped_ranges.as_ref().map(|ranges|{
                            let lines:Vec<_>=source.lines().collect();ranges.iter().filter(|(start,end)|*start>0 && *end as usize<=lines.len()).map(|(start,end)|lines[*start as usize-1..*end as usize].join("\n")).collect::<Vec<_>>().join("\n")
                        });
                        for (axis, evidence) in
                            super::test_derivation::configured_risk_axes(
                                &configured_risk_pack,
                                &file,
                                scoped_source.as_deref().unwrap_or(source),
                            )
                        {
                            runtime_axes.entry(axis).or_default().push(evidence);
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
                    if let Some(revision) = selected_revision {
                        symbols.retain(|node| node.generation == revision);
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
                    if let Some(ranges)=&scoped_ranges {
                        symbols.retain(|n| n.start_line>0 && ranges.iter().any(|(start,end)|n.start_line>=*start && n.end_line<=*end));
                    }
                    let mut nullable_observations = BTreeMap::new();
                    for n in &symbols {
                        if n.node_type == "setting_key" {
                            indexed_setting_ids.insert(n.node_id.clone());
                            setting_labels.insert(n.node_id.clone(), setting_label(n));
                            settings_axis.entry(n.node_id.clone()).or_default().push(format!(
                                "{}:{} (literal-key read in requested source; whether it governs this task requires source review)",
                                n.file_path, n.start_line));
                            continue;
                        }
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

                        if needs_vb && explicit_files.contains(&file) && allow_current_settings
                            && n.node_type == "function"
                        {
                            if let Some(source) = snapshot.as_deref().and_then(|bytes| std::str::from_utf8(bytes).ok()) {
                                let lines:Vec<_>=source.lines().collect();
                                if n.start_line>0 && n.end_line>=n.start_line && n.end_line as usize<=lines.len() {
                                    let body=lines[n.start_line as usize-1..n.end_line as usize].join("\n");
                                    let batches=crate::services::request_trace::sql_commands::identity_batches(&body,n.start_line);
                                    if batches.truncated {coverage_notes.push(format!("{label}: identity-batch source observation limit reached; omitted facts remain unknown."));}
                                    for observation in batches.observations {
                                        let target=observation["batch"]["identity_target_spelling"].as_str().unwrap_or("unknown");
                                        identity_batch_axes.entry(format!("IDENTITY_INSERT {target}")).or_default().push(format!("{label}; initializer {}:{}; constructor {}:{}; execute sites {}; INSERT write/read candidates {}/{}; file BLAKE3 {}. SQL source order ON/INSERT/OFF does not prove physical binding, transaction/session identity, successful insertion or cleanup after an error.",file,observation["initializer"]["line"],file,observation["constructor_line"],observation["executions"],observation["batch"]["insert_write_candidates"],observation["batch"]["insert_read_candidates"],blake3::hash(source.as_bytes()).to_hex()));
                                    }
                                    collect_bulk_copy_axis(
                                        &pid, &file, source, &body, n.start_line,
                                        &mut bulk_copy_axes, &mut coverage_notes,
                                    );
                                }
                                if let Some(parameters) = crate::services::vb_sql_parameters::method_observations(source,n.start_line,n.end_line) {
                                    if parameters.truncated {
                                        coverage_notes.push(format!("{label}: SQL parameter observation limit reached; omitted parameter facts remain unknown."));
                                    }
                                    let hash=blake3::hash(source.as_bytes()).to_hex().to_string();
                                    for command in parameters.commands {
                                        if command.receiver_ambiguous || command.execution_sites.is_empty() {continue;}
                                        for parameter in command.parameters {
                                            let declared:Vec<_>=parameter.declarations.iter().map(|d|format!("{}:{} SqlDbType.{}{}",file,d.site.line,d.sql_type_spelling.as_deref().unwrap_or("unknown"),d.size_literal.as_ref().map(|size|format!(" size literal {size}")).unwrap_or_default())).collect();
                                            let assignments:Vec<_>=parameter.assignments.iter().map(|site|format!("{}:{} `{}`{}",file,site.line,site.source_line,if site.excerpt_truncated{" [line excerpt clipped]"}else{""})).collect();
                                            let key=format!("SQL parameter {} on {}",parameter.name,command.receiver);
                                            sql_parameter_axes.entry(key).or_default().push(format!("{label}; declarations [{}]; assignment source [{}]; file BLAKE3 {hash}. Type/size spellings and assignments are source observations; runtime values, branch reachability, nullability and physical database binding are unproven.",declared.join("; "),assignments.join("; ")));
                                        }
                                    }
                                }
                            }
                        }

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
                                        Ok(Some(t)) => {
                                            if category(&t).is_some() {
                                                indexed_setting_ids.insert(t.node_id.clone());
                                            }
                                            setting_label(&t)
                                        }
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
                for axis in [&mut settings_axis, &mut roles_axis, &mut state_axis, &mut runtime_axes, &mut sql_parameter_axes, &mut identity_batch_axes] {
                    for v in axis.values_mut() {
                        v.sort();
                        v.dedup();
                    }
                }
                (
                    settings_axis,
                    setting_labels,
                    indexed_setting_ids,
                    roles_axis,
                    state_axis,
                    runtime_axes,
                    sql_parameter_axes,
                    identity_batch_axes,
                    bulk_copy_axes,
                    unresolved,
                    coverage_notes,
                    companion_context,
                    member_observations,
                    constructor_context,
                )
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let canonical_sweep = tokio::task::spawn_blocking(move || {
            if scoped_canonical {return super::test_derivation::CanonicalCallSweep{notes:vec!["Method scope does not run the file/diff-wide residual call sweep. Companion, host and transitive helper QA remain outside this matrix; request file scope for that context.".into()],..Default::default()};}
            use crate::services::pre_commit_review_service::{
                parse_unified_diff, resolve_bounded_worktree_diff,
            };
            let resolved = resolve_bounded_worktree_diff(
                &canonical_root,
                &canonical_files,
                100,
                20_000,
                2 * 1024 * 1024,
            );
            let (diffs, diff_notes) = match resolved {
                Ok(resolved) => (
                    parse_unified_diff(&resolved.text),
                    resolved
                        .notes
                        .into_iter()
                        .map(|note| format!("canonical-call {note}"))
                        .collect::<Vec<_>>(),
                ),
                Err(error) => (
                    Vec::new(),
                    vec![format!(
                        "canonical-call bounded Git diff resolution failed: {error}"
                    )],
                ),
            };
            let mut sweep = super::test_derivation::canonical_call_migration_sweep(
                &canonical_root,
                canonical_intent.as_deref().unwrap_or_default(),
                &canonical_files,
                &diffs,
            );
            sweep.notes.extend(diff_notes);
            sweep
        })
        .await
        .map_err(|error| McpError::internal_error(error.to_string(), None))?;

        let max_source_cases = req.max_source_cases.clamp(1, 40);
        let max_coverage_notes = req.max_coverage_notes.clamp(5, 100);
        let (rule_cases, rule_notes) = match (source_runtime, matrix_view.as_ref()) {
            (Ok(runtime), Some(view)) => {
                let search = runtime.search.clone();
                let view = view.clone();
                let pid = req.project_id.clone();
                let root = std::path::PathBuf::from(_rec.directory);
                match tokio::task::spawn_blocking(move || {
                    let _selected_source = view;
                    super::test_derivation::collect_scoped(
                        &search,
                        &pid,
                        gen_,
                        &rule_files,
                        &root,
                        max_source_cases,
                        rule_scopes.as_deref(),
                    )
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
            (Err(error), _) => (
                Vec::new(),
                vec![format!("rule provider unavailable: {error}")],
            ),
            (_, None) => (Vec::new(), vec!["Published source could not be pinned; indexed rule cases withheld.".into()]),
        };

        let matrix_notes = coverage_notes
            .iter()
            .chain(&rule_notes)
            .chain(&canonical_sweep.notes)
            .collect::<Vec<_>>();
        let can_read_settings = crate::tool_surface::includes(
            self.state.cfg.effective_tool_preset(),
            "get_setting",
        );
        if matches!(req.output_format, TestMatrixFormat::Json) {
            let mut documents = BTreeMap::new();
            let cases: Vec<_> = rule_cases.iter().map(|case| {
                documents.entry(case.doc_id.as_str()).or_insert_with(|| serde_json::json!({
                    "source_status": case.source_status,
                    "supporting_context": case.supporting_context.as_deref(),
                    "read_action": {
                        "tool": "get_chunk",
                        "arguments": {
                            "project_id": req.project_id,
                            "doc_id": case.doc_id,
                            "namespace": "business_logic",
                        },
                    },
                }));
                serde_json::json!({
                    "requirement": case.requirement,
                    "document_id": case.doc_id,
                    "source_status": case.source_status,
                    "source_warnings": case.source_warnings,
                    "expected_outcome_status": case.expected_outcome_status,
                    "other_outcome_status": case.other_outcome_status,
                    "source_diagnostic_qualification": case.source_diagnostic_qualification,
                    "reaching_prerequisites_require_review": case.reaching_prerequisites_require_review,
                    "reaching_qualification": case.reaching_qualification,
                    "outcome_dependencies": case.outcome_dependencies,
                })
            }).collect();
            let constructors: Vec<_> = constructor_context.iter().map(|(method_line, context)| {
                let reads: Vec<_> = context.observations.iter().map(|item| {
                    constructor_source_read(
                        &req.project_id,
                        &context.source_file,
                        context.source_blake3.as_deref(),
                        item.line,
                    )
                }).collect();
                serde_json::json!({
                    "method_line": method_line,
                    "source_evidence": context,
                    "source_reads": reads,
                })
            }).collect();
            let report = serde_json::json!({
                "schema_version": 1,
                "project_id": req.project_id,
                "source_generation": gen_,
                "files": req.files,
                "methods": req.methods,
                "knowledge_before": req.knowledge_before,
                "authored_guidance": project_policy_sources,
                "axes": {
                    "settings": structured_matrix_axis(&settings_axis, Some(&setting_labels), |id| {
                        (can_read_settings && indexed_setting_ids.contains(id))
                            .then(|| exact_setting_read(&req.project_id, id))
                    }),
                    "roles": structured_matrix_axis(&roles_axis, None, |_| None),
                    "shared_state": structured_matrix_axis(&state_axis, None, |_| None),
                    "configured_risk": structured_matrix_axis(&runtime_axes, None, |_| None),
                    "sql_parameters": structured_matrix_axis(&sql_parameter_axes, None, |_| None),
                    "identity_insert_batches": structured_matrix_axis(&identity_batch_axes, None, |_| None),
                    "bulk_copy_writes": structured_matrix_axis(&bulk_copy_axes, None, |_| None),
                },
                "constructor_setup": constructors,
                "local_nullable_state": member_observations,
                "direct_ui_context": companion_context,
                "cases": cases,
                "evidence_documents": documents,
                "canonical_call_sweep": {
                    "attempted": canonical_sweep.attempted,
                    "summary": canonical_sweep.summary,
                    "residuals": canonical_sweep.residuals,
                },
                "coverage": {
                    "source_selection_held": matrix_view.is_some(),
                    "notes": matrix_notes.iter().take(max_coverage_notes).collect::<Vec<_>>(),
                    "omitted_notes": matrix_notes.len().saturating_sub(max_coverage_notes),
                    "unresolved_files": unresolved,
                    "configured_risk_pack_rules_loaded": configured_risk_count,
                    "test_discovery": "not_run",
                    "test_execution": "not_run",
                    "interpretation": "Cases are inferred proposals. Confirm effective settings, runtime behavior and test outcomes independently.",
                }
            });
            return Ok(CallToolResult::success(vec![Content::text(report.to_string())]));
        }

        let mut out = format!("# Test matrix — {} requested file(s)\n", req.files.len());
        out.push_str("Evidence scope: indexed settings, permission and state references plus configured risk rules matched against source-verified snapshots. Test discovery: not_run. Test execution: not_run. These are proposed cases, not verified outcomes.\n");
        out.push_str(&project_policy_sources.render_markdown());
        if let Some(scopes)=&req.methods {
            out.push_str(&format!("Method declaration scope: {}. Inferred axes and rule cases cover only these exact methods. Authored file guidance remains applicable; this is not whole-file, companion or transitive-helper QA.\n",serde_json::to_string(scopes).unwrap_or_default()));
        }
        if configured_risk_count > 0 {
            out.push_str(&format!("Configured risk packs: {configured_risk_count} validated rule(s) loaded at call time; repository rules override organization rules by stable id.\n"));
        }
        if let Some(cutoff) = req.knowledge_before.as_deref() {
            out.push_str(&format!("Historical knowledge cutoff: configured rules require dated provenance strictly before {cutoff}; undated and newer rules are excluded and reported.\n"));
        }
        if !companion_context.is_empty() {
            out.push_str("Direct UI context: code-behind declarations and one-hop user-control hosts from source-verified markup/graph edges. A companion or host is context, not a changed file; its indexed axes are separately checked below. No transitive helper calls are inferred. Business-rule cases remain scoped to the explicitly requested files.\n");
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
        if !constructor_context.is_empty() {
            out.push_str("\n## Constructor setup for selected methods\n");
            out.push_str("These name-matched source assignments are setup leads. Confirm field binding, constructor selection, relevant configuration values and applicable test expectations.\n");
            for (method_line, context) in constructor_context {
                let file = &context.source_file;
                for assignment in context.observations {
                    out.push_str(&format!(
                        "- Selected method {file}:{method_line}; constructor assignment {file}:{} for {}.\n  `{}`\n",
                        assignment.line, assignment.member_spelling, assignment.source.trim(),
                    ));
                    let action = constructor_source_read(
                        &req.project_id, file, context.source_blake3.as_deref(), assignment.line,
                    );
                    out.push_str(&format!("  Source read: {action}\n"));
                }
                for gap in context.omissions {
                    out.push_str(&format!("- {file}:{method_line}: {gap}\n"));
                }
            }
        }
        let note_limit = max_coverage_notes;
        for note in matrix_notes.iter().take(note_limit) {
            out.push_str(&format!("INCOMPLETE: {note}\n"));
        }
        if matrix_notes.len() > note_limit {
            out.push_str(&format!(
                "INCOMPLETE: {} additional coverage note(s) omitted by max_coverage_notes={note_limit}. Source-linked matrix evidence below retains each included document's get_chunk recovery pointer; narrow files or raise max_coverage_notes for diagnostics.\n",
                matrix_notes.len() - note_limit
            ));
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
            if axis.len() > MATRIX_AXIS_LIMIT {
                out.push_str(&format!(
                    "INCOMPLETE: showing {MATRIX_AXIS_LIMIT} of {} axes; narrow the requested files.\n",
                    axis.len()
                ));
            }
            for (value, methods) in axis.iter().take(MATRIX_AXIS_LIMIT) {
                let display = labels
                    .and_then(|labels| labels.get(value))
                    .map(|label| format!("{label} [node_id: {value}]"))
                    .unwrap_or_else(|| value.clone());
                out.push_str(&format!(
                    "- **{display}** → {}\n",
                    methods
                        .iter()
                        .take(MATRIX_SITE_LIMIT)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("; ")
                ));
                if methods.len() > MATRIX_SITE_LIMIT {
                    out.push_str(&format!("  (+{} more sites)\n", methods.len() - MATRIX_SITE_LIMIT));
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
        if can_read_settings {
            let mut actions = settings_axis.keys().take(MATRIX_AXIS_LIMIT)
                .filter(|id| indexed_setting_ids.contains(*id)).peekable();
            if actions.peek().is_some() {
                out.push_str("\nExact setting reads for the displayed indexed rows:\n");
                for id in actions {
                    let action = exact_setting_read(&req.project_id, id);
                    out.push_str(&format!("- {action}\n"));
                }
            }
        }
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
        render_axis(
            "Configured risk axis",
            "Organization or repository risk rules matched these source-verified files. Each case names its rule and pack; they propose tests, not proof of current behavior:",
            &runtime_axes,
            None,
            &mut out,
        );
        render_axis(
            "Database parameter axis",
            "Review the declared SQL type/size and assignment source before choosing applicable length, null and domain-boundary cases. A DBNull source assignment is a review lead, not proof of reachable null values. Only verified explicitly requested VB method ranges contribute; no application or database tests were executed:",
            &sql_parameter_axes,
            None,
            &mut out,
        );
        render_axis(
            "Identity-insert batch axis",
            "Review the supported literal ON/INSERT/OFF constructor source and its session/error-cleanup paths against actual task requirements. These are proposed review/test leads, not executed SQL or guarantees that OFF runs after failure:",
            &identity_batch_axes,
            None,
            &mut out,
        );
        let bulk_copy_display = bulk_copy_axes.iter().map(|(table, sites)| {
            let descriptions = sites.iter().map(|site| {
                format!("{}:{} writes after the literal destination assignment at line {}. Source read: {}",
                    site["file"].as_str().unwrap_or_default(), site["write_line"],
                    site["destination_assignment_line"], site["source_read"])
            }).collect();
            (table.clone(), descriptions)
        }).collect();
        render_axis(
            "Bulk-copy write axis",
            "Check the qualified destination, input rows and error handling against the source. The call sites do not establish database state or test results.",
            &bulk_copy_display,
            None,
            &mut out,
        );
        if canonical_sweep.attempted && !canonical_sweep.summary.is_empty() {
            out.push_str(&format!(
                "\n## Canonical-call residual sweep — {}\n{}\n",
                canonical_sweep.residuals.len(),
                canonical_sweep.summary,
            ));
            if canonical_sweep.residuals.is_empty() {
                out.push_str("No same-callee plain-string residual was observed within the reported bounded scan. This is not proof that the repository has no aliases, indirect calls, multiline calls, generated sources, non-UTF-8 sources, or sites outside the scan caps.\n");
            } else {
                for residual in &canonical_sweep.residuals {
                    out.push_str(&format!("- {residual}\n"));
                }
            }
        }

        out.push_str("\n## Source-linked proposed cases\nExpected outcomes below are inferred business rules whose method hashes match current source; confirm the requirements before implementing the tests.\n");
        render_rule_cases(&mut out, &rule_cases);
        if rule_cases.is_empty() {
            out.push_str("No source-verified rule cases available; run analyze_business_logic for the requested files to populate or refresh them.\n");
        }

        if settings_axis.is_empty() && roles_axis.is_empty() && state_axis.is_empty() && runtime_axes.is_empty() && sql_parameter_axes.is_empty() && identity_batch_axes.is_empty()
            && bulk_copy_axes.is_empty() {
            out.push_str(
                "\nNo usable setting/role/state/configured-risk axes were emitted. This does not establish \
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
                 Cartesian or runtime test matrix; confirm those in the cited methods.\n",
                settings_axis.len(),
                roles_axis.len()
            ));
            out.push_str(
                "\nUse the exact setting read where offered. For unindexed or unresolved references, inspect the cited source with grep_project/get_chunk. Use map_guards_and_settings(scope=<area>) \
                 for gates in files you did NOT change but that share these keys.\n",
            );
        }
        out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }
}

fn collect_bulk_copy_axis(
    project: &str,
    file: &str,
    source: &str,
    body: &str,
    first_line: u32,
    axes: &mut BTreeMap<String, Vec<serde_json::Value>>,
    notes: &mut Vec<String>,
) {
    if body.len() > 128 * 1024 {
        notes.push(format!("{file}:{first_line}: method exceeds the bulk-copy source bound."));
        return;
    }
    use crate::services::request_trace::{bulk_copy, routing};
    let tokens = routing::tokens(body, true);
    let mut writes = bulk_copy::writes(&tokens, first_line).peekable();
    if writes.peek().is_none() {
        return;
    }
    let file_hash = blake3::hash(source.as_bytes()).to_hex().to_string();
    for (index, write) in writes.enumerate() {
        if index == MATRIX_AXIS_LIMIT {
            notes.push(format!("{file}:{first_line}: bulk-copy observation limit reached."));
            break;
        }
        axes.entry(write.table).or_default().push(serde_json::json!({
            "file": file,
            "method_line": first_line,
            "declaration_line": write.declaration_line,
            "destination_assignment_line": write.assignment_line,
            "write_line": write.call_line,
            "operation": write.operation,
            "source_blake3": file_hash,
            "source_read": {
                "tool": "get_full_method_body",
                "arguments": {
                    "project_id": project,
                    "file_path": file,
                    "source_ranges": [
                        {"line_start": write.declaration_line, "line_end": write.declaration_line},
                        {"line_start": write.assignment_line, "line_end": write.assignment_line},
                        {"line_start": write.call_line, "line_end": write.call_line},
                    ],
                    "expected_file_blake3": file_hash,
                    "context_lines": 0,
                    "output_json": true,
                },
            },
        }));
    }
}

fn structured_matrix_axis<T: serde::Serialize>(
    axis: &BTreeMap<String, Vec<T>>,
    labels: Option<&BTreeMap<String, String>>,
    read: impl Fn(&str) -> Option<serde_json::Value>,
) -> serde_json::Value {
    let items: Vec<_> = axis
        .iter()
        .take(MATRIX_AXIS_LIMIT)
        .map(|(id, sites)| serde_json::json!({
            "id": id,
            "label": labels.and_then(|labels| labels.get(id)).unwrap_or(id),
            "sites": sites.iter().take(MATRIX_SITE_LIMIT).collect::<Vec<_>>(),
            "site_count": sites.len(),
            "omitted_sites": sites.len().saturating_sub(MATRIX_SITE_LIMIT),
            "read_action": read(id),
        }))
        .collect();
    serde_json::json!({
        "total": axis.len(),
        "omitted": axis.len().saturating_sub(items.len()),
        "items": items,
    })
}

fn exact_setting_read(project: &str, node_id: &str) -> serde_json::Value {
    serde_json::json!({"tool":"get_setting","arguments":{"project_id":project,"node_id":node_id}})
}

fn constructor_source_read(
    project: &str, file: &str, hash: Option<&str>, line: u32,
) -> serde_json::Value {
    serde_json::json!({"tool":"get_full_method_body","arguments":{
        "project_id":project,"file_path":file,"expected_file_blake3":hash,
        "line_start":line,"line_end":line,"context_lines":0,"output_json":true,
    }})
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
        if let Some(context) = matching.first().and_then(|case| case.supporting_context.as_deref()) {
            out.push_str(context);
        }
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
            supporting_context: None,
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
