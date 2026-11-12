//! Assemble source-bound static request paths using the existing graph and flow reader.
use super::routing::{RouteCandidate, Source};
use engram_graph::{EdgeKind, GraphStore, Node};
use regex::Regex;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

const MAX_METHODS: usize = 256;
const MAX_NODES: usize = 64;
const MAX_DEPTH: usize = 6;
const MAX_EDGES: usize = 128;

#[derive(Serialize)]
pub struct Handler {
    pub node_id: String,
    pub name: String,
    pub file: String,
    pub line: u32,
    pub binding: String,
}

#[derive(Serialize)]
pub struct Hop {
    pub from: String,
    pub to: String,
    pub kind: String,
    pub target_name: String,
    pub target_file: Option<String>,
    pub target_line: Option<u32>,
    pub target_source_verified: bool,
    pub evidence: Option<serde_json::Value>,
}

#[derive(Serialize)]
pub struct PageTrace {
    pub page: String,
    pub codebehind: Option<String>,
    pub handlers: Vec<Handler>,
    pub paths: Vec<Hop>,
    pub source_members: Vec<serde_json::Value>,
    pub gaps: Vec<String>,
    pub trigger_coverage: &'static str,
    pub interpretation: &'static str,
}

fn attribute(source: &str, key: &str) -> Option<String> {
    let re = Regex::new(&format!(
        r#"(?is)\b{}\s*=\s*(?:"([^"]*)"|'([^']*)')"#,
        regex::escape(key)
    ))
    .ok()?;
    let captures = re.captures(source)?;
    Some(
        captures
            .get(1)
            .or_else(|| captures.get(2))?
            .as_str()
            .to_owned(),
    )
}

fn without_markup_comments(source: &str) -> String {
    Regex::new(r"(?s)<%--.*?--%>|<!--.*?-->")
        .expect("comment syntax")
        .replace_all(source, |c: &regex::Captures<'_>| {
            c[0].chars()
                .map(|v| if v == '\n' { '\n' } else { ' ' })
                .collect::<String>()
        })
        .into_owned()
}

fn relative_target(page: &str, raw: &str, app_root: Option<&str>) -> Option<String> {
    let raw = raw.replace('\\', "/");
    if raw.contains([':', '\0', '%']) || raw.starts_with('/') {
        return None;
    }
    let joined = if let Some(target) = raw.strip_prefix("~/") {
        format!("{}/{target}", app_root?)
    } else {
        format!(
            "{}/{}",
            page.rsplit_once('/').map_or("", |(parent, _)| parent),
            raw
        )
    };
    let mut parts = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => (),
            ".." => {
                parts.pop()?;
            }
            value => parts.push(value),
        }
    }
    Some(parts.join("/"))
}

fn member_name(node: &Node) -> &str {
    node.name
        .rsplit(['.', ':'])
        .next()
        .unwrap_or(&node.name)
        .split('(')
        .next()
        .unwrap_or(&node.name)
}

/// Every expansion requires a graph file fingerprint and the selected physical
/// revision. Matching current disk text alone cannot authorize a newer graph view.
fn verified_node(
    graph: &GraphStore,
    project: &str,
    node: &Node,
    sources: &[Source<'_>],
    selected: &BTreeMap<String, u64>,
) -> anyhow::Result<bool> {
    let path = node.file_path.as_str();
    let Some(revision) = selected.get(path) else {
        return Ok(false);
    };
    if node.generation != *revision {
        return Ok(false);
    }
    let Some(source) = sources.iter().find(|source| source.path == path) else {
        return Ok(false);
    };
    if node.start_line == 0
        || node.end_line < node.start_line
        || node.end_line as usize > source.content.lines().count()
    {
        return Ok(false);
    }
    let Some(file) = graph.get_node(project, &format!("file:{path}"))? else {
        return Ok(false);
    };
    Ok(file.generation == *revision
        && file
            .metadata
            .as_ref()
            .and_then(|v| v.get("file_hash"))
            .and_then(|v| v.as_str())
            .is_some_and(|hash| hash == blake3::hash(source.content.as_bytes()).to_hex().as_str()))
}

/// A source-authored binding, not a suffix-based guess that every *_Click runs.
fn handler_bindings(
    markup: &str,
    source: &str,
    methods: &[Node],
    action: Option<&str>,
) -> Vec<(Node, String)> {
    let requested = action.map(str::trim).filter(|a| !a.is_empty());
    let click = requested.and_then(|a| a.strip_prefix("click "));
    let form = requested.is_some_and(|a| a.eq_ignore_ascii_case("submit form"));
    let load =
        requested.is_none_or(|a| a.eq_ignore_ascii_case("load page")) || click.is_some() || form;
    let tags = Regex::new(r"(?s)<[A-Za-z][^>]*>").expect("markup tag syntax");
    let mut selected = Vec::new();
    for node in methods {
        let name = member_name(node);
        let mut reasons = Vec::new();
        // Header and Handles are confined to the indexed member's first line.
        // Multiline/dynamic bindings remain explicit gaps at the page level.
        let header = source
            .lines()
            .nth(node.start_line.saturating_sub(1) as usize)
            .unwrap_or("");
        let tokens =
            super::routing::tokens(header, node.language == "vb" || node.language == "vbnet");
        if let Some(position) = tokens
            .iter()
            .position(|token| !token.string && token.text.eq_ignore_ascii_case("Handles"))
        {
            for binding in tokens[position + 1..].split(|token| !token.string && token.text == ",")
            {
                if binding.len() != 3
                    || binding.iter().any(|token| token.string)
                    || binding[1].text != "."
                {
                    continue;
                }
                let control = binding[0].text.as_str();
                let event = binding[2].text.as_str();
                if (load
                    && matches!(control.to_ascii_lowercase().as_str(), "me" | "mybase")
                    && event.eq_ignore_ascii_case("load"))
                    || click.is_some_and(|target| {
                        control.eq_ignore_ascii_case(target) && event.eq_ignore_ascii_case("click")
                    })
                {
                    reasons.push(format!("Handles {control}.{event}"));
                }
            }
        }
        for tag in tags.find_iter(markup) {
            let id = attribute(tag.as_str(), "ID");
            if click.is_some_and(|target| {
                id.as_deref()
                    .is_some_and(|id| id.eq_ignore_ascii_case(target))
            }) || form
            {
                if attribute(tag.as_str(), "OnClick")
                    .is_some_and(|handler| handler.eq_ignore_ascii_case(name))
                {
                    reasons.push(format!(
                        "{} OnClick={name}",
                        id.as_deref().unwrap_or("unnamed control")
                    ));
                }
            }
        }
        if load
            && name.eq_ignore_ascii_case("Page_Load")
            && attribute(markup, "AutoEventWireup")
                .is_some_and(|value| value.eq_ignore_ascii_case("true"))
        {
            reasons.push("Page_Load with authored AutoEventWireup=true".into());
        }
        // An explicit method asks for that source entry, but does not prove HTTP dispatch.
        if requested.is_some_and(|value| value.eq_ignore_ascii_case(name)) {
            reasons.push("explicit method selection; HTTP event binding not established".into());
        }
        if !reasons.is_empty() {
            selected.push((node.clone(), reasons.join("; ")));
        }
    }
    selected
}

pub fn trace_page(
    graph: &Arc<GraphStore>,
    project: &str,
    route: &RouteCandidate,
    action: Option<&str>,
    include_db: bool,
    include_triggers: bool,
    sources: &[Source<'_>],
    selected: &BTreeMap<String, u64>,
) -> anyhow::Result<PageTrace> {
    let mut result = PageTrace {
        page: route.page.clone(),
        codebehind: None,
        handlers: Vec::new(),
        paths: Vec::new(),
        source_members: Vec::new(),
        gaps: Vec::new(),
        trigger_coverage: if !include_db {
            "not_requested_include_db_false"
        } else if !include_triggers {
            "not_requested"
        } else {
            "unknown_no_trigger_binding_in_existing_graph"
        },
        interpretation: "Static possible paths. Source order is not execution, branch reachability, successful completion, HTTP status or proof that a trigger fires. Calls and database targets retain their indexed evidence.",
    };
    let Some(page) = sources.iter().find(|source| source.path == route.page) else {
        result.gaps.push("Page source unavailable".into());
        return Ok(result);
    };
    let markup = without_markup_comments(page.content);
    let directive =
        Regex::new(r"(?is)<%@\s*(?:Page|WebHandler|WebService)\b.*?%>").expect("directive syntax");
    let Some(directive) = directive.find(&markup) else {
        result
            .gaps
            .push("Page/handler directive unavailable".into());
        return Ok(result);
    };
    let Some(raw) = attribute(directive.as_str(), "CodeFile")
        .or_else(|| attribute(directive.as_str(), "CodeBehind"))
    else {
        result.gaps.push("No authored code-behind directive; inline/inherited handler resolution is not established".into());
        return Ok(result);
    };
    let root = super::routing::app_root(page.path, sources);
    let Some(path) = relative_target(page.path, &raw, root) else {
        result
            .gaps
            .push("Code-behind target escapes the project or lacks hosting evidence".into());
        return Ok(result);
    };
    let matches: Vec<_> = sources
        .iter()
        .filter(|source| source.path.eq_ignore_ascii_case(&path))
        .collect();
    if matches.len() != 1 {
        result.gaps.push(format!(
            "Code-behind {path} has {} selected source matches",
            matches.len()
        ));
        return Ok(result);
    }
    let source = matches[0];
    result.codebehind = Some(source.path.into());
    let mut methods =
        graph.query_nodes_in_file(project, Some("function"), source.path, MAX_METHODS + 1)?;
    if methods.len() > MAX_METHODS {
        methods.truncate(MAX_METHODS);
        result
            .gaps
            .push(format!("Method inventory truncated at {MAX_METHODS}"));
    }
    let chosen = handler_bindings(&markup, source.content, &methods, action);
    if chosen.is_empty() {
        result.gaps.push("No requested authored handler binding found; dynamic, multiline and inherited bindings are not inferred".into());
    }
    if action.is_some_and(|a| a.eq_ignore_ascii_case("submit form")) {
        result.gaps.push("Submit control unspecified; returned click handlers are alternatives, not an execution sequence".into());
    }
    let mut queue = VecDeque::new();
    for (node, binding) in chosen {
        if !verified_node(graph, project, &node, sources, selected)? {
            result.gaps.push(format!(
                "{}: graph/source revision or fingerprint unavailable or changed; handler withheld",
                node.node_id
            ));
            continue;
        }
        result.handlers.push(Handler {
            node_id: node.node_id.clone(),
            name: member_name(&node).into(),
            file: source.path.into(),
            line: node.start_line,
            binding,
        });
        queue.push_back((node, 0));
    }
    let database = include_db.then(|| super::database::Catalog::new(sources));
    if let Some(database) = &database {
        result.gaps.extend(database.gaps.iter().cloned());
        result.gaps.push("Database coverage is partial: source fallback covers local literal SqlCommand stored-procedure candidates. Dynamic commands, inline SQL/ORM translation, aliases, nested procedure execution and deployed database identity are not resolved.".into());
    }
    let mut visited = BTreeSet::new();
    while let Some((node, depth)) = queue.pop_front() {
        if !visited.insert(node.node_id.clone()) {
            continue;
        }
        if visited.len() > MAX_NODES {
            result.gaps.push(format!(
                "Call/data expansion truncated at {MAX_NODES} nodes"
            ));
            break;
        }
        if depth >= MAX_DEPTH {
            result.gaps.push(format!(
                "{}: expansion stopped at depth {MAX_DEPTH}",
                node.node_id
            ));
            continue;
        }
        if !verified_node(graph, project, &node, sources, selected)? {
            result.gaps.push(format!("{}: target source unavailable or differs from selected graph revision; expansion stopped", node.node_id));
            continue;
        }
        if let Some(source) = sources
            .iter()
            .find(|source| source.path == node.file_path.as_str())
        {
            let mut excerpt = String::new();
            let mut shown = 0;
            let lines = node
                .end_line
                .saturating_sub(node.start_line)
                .saturating_add(1) as usize;
            for line in source
                .content
                .lines()
                .skip(node.start_line.saturating_sub(1) as usize)
                .take(lines)
            {
                if excerpt.len() + line.len() + 1 > 2048 {
                    break;
                }
                excerpt.push_str(line);
                excerpt.push('\n');
                shown += 1;
            }
            result.source_members.push(serde_json::json!({"node_id":node.node_id,"file":source.path,"line_start":node.start_line,
                "line_end":node.end_line,"source_blake3":blake3::hash(source.content.as_bytes()).to_hex().to_string(),
                "excerpt":excerpt,"shown_lines":shown,"excerpt_truncated":shown<lines,
                "interpretation":"Authored code, including guards and response expressions. Source order does not prove an operation executes or completes.",
                "recovery":{"tool":"get_full_method_body","arguments":{"project_id":project,"file_path":source.path,"line_start":node.start_line,"context_lines":0}}}));
            if node.node_type == "function"
                && let Some(database) = &database
            {
                let body = source
                    .content
                    .lines()
                    .skip(node.start_line as usize - 1)
                    .take(lines)
                    .collect::<Vec<_>>()
                    .join("\n");
                database.append(
                    project,
                    &node,
                    &body,
                    &mut result.paths,
                    &mut result.source_members,
                    &mut result.gaps,
                    MAX_EDGES,
                );
            }
        }
        let mut kinds = vec![
            EdgeKind::Calls,
            EdgeKind::ReadsState,
            EdgeKind::WritesState,
            EdgeKind::DataBinding,
            EdgeKind::ApiCall,
        ];
        if include_db {
            kinds.extend([
                EdgeKind::SqlCalls,
                EdgeKind::QueriesTable,
                EdgeKind::CallsStoredProcedure,
                EdgeKind::StoredProcReadsTable,
                EdgeKind::StoredProcWritesTable,
            ]);
        }
        for kind in kinds {
            let (edges, truncated) =
                graph.outgoing_edges_of_kind(project, kind.clone(), &node.node_id, MAX_EDGES)?;
            if truncated {
                result.gaps.push(format!(
                    "{}: {} edges truncated at {MAX_EDGES}",
                    node.node_id,
                    kind.as_str()
                ));
            }
            for edge in edges {
                if edge.generation != node.generation {
                    result.gaps.push(format!(
                        "{} -> {}: edge revision differs from selected source; withheld",
                        node.node_id, edge.target_id
                    ));
                    continue;
                }
                if result.paths.len() >= MAX_EDGES {
                    result.gaps.push(format!(
                        "Complete path response truncated at {MAX_EDGES} edges"
                    ));
                    queue.clear();
                    break;
                }
                let target = graph.get_node(project, &edge.target_id)?;
                let target_verified = match &target {
                    Some(target) => verified_node(graph, project, target, sources, selected)?,
                    None => false,
                };
                result.paths.push(Hop {
                    from: node.node_id.clone(),
                    to: edge.target_id.clone(),
                    kind: kind.as_str().into(),
                    target_name: target
                        .as_ref()
                        .map_or_else(|| edge.target_id.clone(), |n| n.name.clone()),
                    target_file: target
                        .as_ref()
                        .filter(|_| target_verified)
                        .map(|n| n.file_path.to_string()),
                    target_line: target
                        .as_ref()
                        .filter(|_| target_verified)
                        .map(|n| n.start_line),
                    target_source_verified: target_verified,
                    evidence: edge.metadata,
                });
                if matches!(
                    kind,
                    EdgeKind::Calls | EdgeKind::CallsStoredProcedure | EdgeKind::SqlCalls
                ) {
                    if let Some(target) = target {
                        queue.push_back((target, depth + 1));
                    } else {
                        result
                            .gaps
                            .push(format!("{}: unresolved target", edge.target_id));
                    }
                }
            }
        }
    }
    result.gaps.sort();
    result.gaps.dedup();
    Ok(result)
}
