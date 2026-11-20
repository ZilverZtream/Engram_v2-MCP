//! Scoped WebForms evidence outside the handler body.
use engram_graph::{GraphStore, Node};
use regex::Regex;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::LazyLock,
};

static MASTER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<%@\s*Page\b[^%]*?\bMasterPageFile\s*=\s*["']([^"']+)["']"#).unwrap()
});
static CODE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<%@\s*Master\b[^%]*?\b(?:CodeFile|CodeBehind)\s*=\s*["']([^"']+)["']"#)
        .unwrap()
});
static CALL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b([a-z_]\w*(?:redirect|access|authoriz)\w*|redirect\w*)\s*\(").unwrap()
});
static SUB: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:Sub|Function)\s+(\w+)\s*\(").unwrap());
static NON_ENDING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bResponse\s*\.\s*Redirect\s*\(.*,\s*(?:endResponse\s*:=\s*)?False\s*\)")
        .unwrap()
});

fn rel(root: &Path, file: &Path) -> Option<String> {
    Some(file.strip_prefix(root).ok()?.to_str()?.replace('\\', "/"))
}
fn resolve(app: &Path, parent: &Path, path: &str) -> PathBuf {
    if let Some(path) = path.strip_prefix("~/") {
        app.join(path)
    } else {
        parent.join(path)
    }
}

pub(super) fn evidence(graph: &GraphStore, pid: &str, root: &Path, nodes: &[Node]) -> Vec<String> {
    let pages: BTreeSet<_> = nodes
        .iter()
        .filter_map(|node| {
            let (page, ext) = node.file_path.as_str().rsplit_once('.')?;
            (page.to_ascii_lowercase().ends_with(".aspx")
                && matches!(ext.to_ascii_lowercase().as_str(), "cs" | "vb"))
            .then_some(page)
        })
        .collect();
    let mut out = BTreeSet::new();
    let mut masters = BTreeSet::new();
    let mut configs = std::collections::BTreeMap::new();
    if pages.len() > 16 {
        out.insert("Page context limited to the first 16 distinct pages.".into());
    }
    for page in pages.into_iter().take(16) {
        let file = root.join(page);
        let app = super::super::access_layer_tools::discover_web_application_root(root, &file);
        for dir in file
            .parent()
            .into_iter()
            .flat_map(Path::ancestors)
            .take_while(|dir| dir.starts_with(&app))
        {
            let rules = configs.entry(dir.to_owned()).or_insert_with(|| {
                for name in ["Web.config", "web.config"] {
                    let Some(path) = rel(root, &dir.join(name)) else {
                        continue;
                    };
                    if let Ok(source) = crate::planner_source::read(graph, pid, root, &path) {
                        return Some((
                            path,
                            crate::services::auth_config_service::parse_location_rules(&source),
                        ));
                    }
                }
                None
            });
            let Some((config, rules)) = rules else {
                continue;
            };
            let local = rel(dir, &file).unwrap_or_default().to_ascii_lowercase();
            for rule in rules {
                let path = rule.path.trim_end_matches('/').to_ascii_lowercase();
                if path != "(global)"
                    && path != "."
                    && local != path
                    && !local.starts_with(&format!("{path}/"))
                {
                    continue;
                }
                out.insert(format!("{config} location {} applies to {page}: allow roles {:?}, allow users {:?}, deny roles {:?}, deny users {:?}.",rule.path,rule.allow_roles,rule.allow_users,rule.deny_roles,rule.deny_users));
            }
        }
        let Ok(markup) = crate::planner_source::read(graph, pid, root, page) else {
            continue;
        };
        let Some(link) = MASTER.captures(&markup) else {
            continue;
        };
        let master = resolve(&app, file.parent().unwrap_or(root), &link[1]);
        let Some(path) = rel(root, &master) else {
            continue;
        };
        out.insert(format!("{page} declares master {path}."));
        if !masters.insert(path.clone()) {
            continue;
        }
        let Ok(markup) = crate::planner_source::read(graph, pid, root, &path) else {
            continue;
        };
        let Some(code) = CODE.captures(&markup) else {
            continue;
        };
        let Some(behind) = rel(
            root,
            &resolve(&app, master.parent().unwrap_or(root), &code[1]),
        ) else {
            continue;
        };
        let Ok(source) = crate::planner_source::read(graph, pid, root, &behind) else {
            continue;
        };
        master_calls(graph, pid, root, &behind, &source, &mut out);
    }
    out.into_iter().collect()
}

fn master_calls(
    graph: &GraphStore,
    pid: &str,
    root: &Path,
    behind: &str,
    source: &str,
    out: &mut BTreeSet<String>,
) {
    let mut method = String::new();
    let mut calls = Vec::new();
    let mut lifecycle = BTreeSet::new();
    for (i, line) in source.lines().enumerate() {
        let stmt = line.trim();
        if stmt.starts_with(['\'', '/']) {
            continue;
        }
        let lower = stmt.to_ascii_lowercase();
        if let Some(decl) = SUB.captures(stmt) {
            method = decl[1].to_ascii_lowercase();
            if matches!(
                method.as_str(),
                "onload" | "page_load" | "page_init" | "oninit"
            ) || lower.contains("handles me.load")
                || lower.contains("handles mybase.load")
                || lower.contains("handles me.init")
            {
                lifecycle.insert(method.clone());
            }
            continue;
        }
        if matches!(lower.as_str(), "end sub" | "end function") {
            method.clear();
        }
        if method.is_empty() {
            continue;
        }
        for call in CALL.captures_iter(stmt) {
            calls.push((method.clone(), call[1].to_string(), i + 1));
        }
    }
    // Calls inside an access helper can be relevant even when a lifecycle
    // event reaches it through another local method. Report the enclosing
    // method and source line without inferring that a branch always runs.
    let mut redirects = BTreeSet::new();
    for (owner, callee, line) in &calls {
        let target = callee.to_ascii_lowercase();
        let is_lifecycle = lifecycle.contains(owner);
        let access_related = ["access", "authoriz", "permission"]
            .iter()
            .any(|term| owner.contains(term) || target.contains(term));
        if is_lifecycle || access_related {
            let kind = if is_lifecycle {
                "master lifecycle"
            } else {
                "master method"
            };
            out.insert(format!("{behind}:{line} {kind} {owner} calls {callee}."));
            if target.contains("redirect") {
                redirects.insert(callee);
            }
        }
    }
    for callee in redirects.into_iter().take(12) {
        let Ok(targets) = graph.query_nodes_by_symbol_name(pid, callee, None, 2) else {
            continue;
        };
        let [node] = targets.as_slice() else { continue };
        if node.node_type != "function" {
            continue;
        }
        let path = node.file_path.as_str();
        let Ok(text) = crate::planner_source::read(graph, pid, root, path) else {
            continue;
        };
        let start = node.start_line.saturating_sub(1) as usize;
        for (i, line) in text
            .lines()
            .enumerate()
            .take(node.end_line as usize)
            .skip(start)
        {
            if !line.trim_start().starts_with(['\'', '/']) && NON_ENDING.is_match(line) {
                out.insert(format!("{path}:{} candidate helper {callee} calls Response.Redirect with endResponse=false. This call alone does not terminate page execution.",i+1));
            }
        }
    }
}
