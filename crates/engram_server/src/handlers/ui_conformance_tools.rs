//! Indexed UI patterns and current authored requirements have separate authority.
use crate::{
    models::GetUiConformanceRequest,
    services::{project_policy, ui_catalog},
    tools::Engram,
};
use rmcp::{
    ErrorData as McpError,
    model::{CallToolResult, Content},
};
use std::{collections::BTreeSet, path::Path};
use std::fmt::Write;

const INDEX_FILE_LIMIT: usize = 50_000;
const POLICY_TARGET_LIMIT: usize = 128;

fn page_class_evidence(path: &str, source: &str, candidate: &str) -> String {
    let sites = crate::services::request_trace::control_events::class_sites(source);
    let wanted = candidate.split_ascii_whitespace().collect::<BTreeSet<_>>();
    let mut report = format!("Literal class usage in {path} ({} elements with classes):\n", sites.len());
    for class in wanted {
        let mut lines = sites.iter().filter(|(_, value)| value.split_ascii_whitespace().any(|v| v == class));
        let Some((first, _)) = lines.next() else {
            let _ = writeln!(report, "- {class}: 0 observed elements");
            continue;
        };
        let _ = writeln!(report, "- {class}: {} elements, first at {path}:{first}", 1 + lines.count());
    }
    report
}

fn class_precedents(
    graph: &engram_graph::GraphStore, project: &str, root: &Path,
    page: &str, source: &str, candidate: &str,
) -> String {
    use std::collections::BTreeMap;
    let wanted: BTreeSet<_> = candidate.split_ascii_whitespace().collect();
    if wanted.is_empty() { return String::new(); }
    let mut patterns = BTreeMap::new();
    let mut observe = |file: &str, text: &str| {
        for (line, classes) in crate::services::request_trace::control_events::class_sites(text) {
            let class_set: BTreeSet<_> = classes.split_ascii_whitespace().collect();
            let shared = class_set.intersection(&wanted).count();
            if shared == 0 || (file == page && class_set == wanted) { continue; }
            let pattern = class_set.iter().copied().collect::<Vec<_>>().join(" ");
            let location = (file.to_owned(), line);
            let entry = patterns.entry((std::cmp::Reverse(shared), class_set.symmetric_difference(&wanted).count(), pattern)).or_insert((0,location.clone()));
            entry.0 += 1;
            if location < entry.1 { entry.1 = location; }
        }
    };
    observe(page, source);
    let parent = Path::new(page).parent().unwrap_or(Path::new(""));
    let mut peers = Vec::new();
    if let Ok(directory) = engram_core::safe_join(root, page)
        && let Some(dir) = directory.parent()
        && let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let path = parent.join(name).to_string_lossy().replace('\\', "/");
            if path != page && matches!(Path::new(&path).extension().and_then(|s| s.to_str()), Some("aspx"|"ascx"|"master"|"html"|"htm")) {
                peers.push(path);
            }
        }
    }
    peers.sort();
    let mut sampled = 0;
    for peer in peers.iter().take(12) {
        if let Ok(text) = crate::planner_source::read(graph, project, root, peer) {
            observe(peer, &text);
            sampled += 1;
        }
    }
    let mut report = format!("Comparable class combinations on this page and {sampled} of {} neighboring markup files (up to three examples):\n",peers.len());
    if patterns.is_empty() { report.push_str("- No different combination shared a candidate class in the sampled markup.\n"); }
    for ((std::cmp::Reverse(shared), _, classes), (count, (file, line))) in patterns.into_iter().take(3) {
        let _ = writeln!(report, "- {file}:{line}: `{classes}` ({shared} shared classes, {count} observed elements)");
    }
    report
}

impl Engram {
    pub async fn handle_get_ui_conformance(
        &self,
        req: GetUiConformanceRequest,
    ) -> Result<CallToolResult, McpError> {
        super::validate_project_id(&req.project_id)?;
        let record = self.ensure_project_record(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let text = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
            let region = req.region.trim().replace('\\', "/");
            let min = req.min_instances.unwrap_or(2).max(2);
            let mut families = ui_catalog::families_for_region(&graph, &req.project_id, &region, min)?;
            let exact = !region.contains(['*', '?']) && !region.ends_with('/') && Path::new(&region).extension().is_some();
            let mut matches = BTreeSet::new();
            let mut inventory_capped = false;
            if exact {
                // Requirements can apply before a new file is created/indexed.
                matches.insert(region.clone());
            } else {
                let mut files = graph.query_nodes(&req.project_id, Some("file"), None, None, INDEX_FILE_LIMIT + 1)?;
                inventory_capped = files.len() > INDEX_FILE_LIMIT;
                files.truncate(INDEX_FILE_LIMIT);
                for path in files.iter().map(|node| node.file_path.as_str())
                    .chain(families.iter().flat_map(|family| family.instance_paths.iter().map(String::as_str))) {
                    if ui_catalog::region_matches(&region, path) { matches.insert(path.to_owned()); }
                }
            }
            let matched = matches.len();
            let targets: Vec<_> = matches.into_iter().take(POLICY_TARGET_LIMIT).collect();
            let omitted = matched.saturating_sub(targets.len());
            let mut policy = project_policy::resolve(Path::new(&record.directory), &targets, &[])
                .with_source_recovery(&req.project_id);
            if targets.is_empty() || omitted > 0 || inventory_capped { policy.coverage = "partial".into(); }
            policy.notes.push(format!("Conformance guidance scope: requested region {region:?}; {} target paths supplied; {omitted} observed matching paths omitted by the limit of {POLICY_TARGET_LIMIT} target paths. Target basis: {}. No repository source tree was walked to discover targets.", targets.len(), if exact {"explicit requested file; existence/index membership not established"} else {"bounded indexed file observations matching the region"}));
            if inventory_capped { policy.notes.push(format!("Indexed file inventory capped at{INDEX_FILE_LIMIT}; additional region matches and their guidance may be missing. Total remaining matches are unknown.")); }
            if targets.is_empty() { policy.notes.push("No concrete indexed target matched. Request an exact file path to assess scoped guidance, including a planned new file. This does not establish that no authored requirements apply.".into()); }
            policy.notes.push("Family counts may include instances outside the requested region. Those observations do not extend policy authority to the edit target. Recheck guidance for the final edit scope; current instructions do not verify indexed pattern freshness, UI compliance or runtime behavior.".into());
            if let Some(candidate) = req.candidate_classes.as_deref() {
                let classes = candidate.split_ascii_whitespace().collect::<BTreeSet<_>>();
                families.sort_by_key(|family| std::cmp::Reverse(family.axes.iter()
                    .filter(|axis| axis.axis == "style.classes")
                    .flat_map(|axis| axis.canonical.split_ascii_whitespace())
                    .filter(|name| classes.contains(name)).count()));
            }
            let mut report = if families.is_empty() {
                format!("result: no_families\nregion: {region}\nNo repeated indexed UI pattern with >= {min} instances matches this region. Implementation observations are not authored policy; class/structure comparisons are unassessed for this empty observation. Authored requirements can still apply and are reported below. Try a sibling region for additional implementation examples.\n")
            } else {
                let verdicts = req.candidate_classes.as_deref().map(|classes| ui_catalog::check_classes(&families[0], classes));
                ui_catalog::render_conformance(&families, verdicts.as_deref())
            };
            if exact && let Some(candidate) = req.candidate_classes.as_deref()
                && matches!(Path::new(&region).extension().and_then(|e| e.to_str())
                    .map(str::to_ascii_lowercase).as_deref(), Some("aspx" | "ascx" | "master" | "html" | "htm"))
                && let Ok(source) = crate::planner_source::read(&graph, &req.project_id, Path::new(&record.directory), &region)
            {
                let mut evidence = page_class_evidence(&region, &source, candidate);
                evidence.push_str(&class_precedents(&graph, &req.project_id, Path::new(&record.directory), &region, &source, candidate));
                report.insert_str(0, &evidence);
            }
            report.push_str("\nGuidance target paths (not a complete edit scope):\n");
            for path in &targets { report.push_str(&format!("- `{path}`\n")); }
            report.push_str(&policy.render_markdown());
            Ok(report)
        }).await.map_err(|error| McpError::internal_error(error.to_string(), None))?
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }
}
