//! Explicit VB IHttpHandler entries; hosting and successful execution remain unknown.
use super::{
    execution,
    routing::{self, RouteCandidate, Source},
};
use engram_graph::{GraphStore, Node};
use regex::Regex;
use std::{collections::BTreeMap, sync::Arc};

fn word(token: &routing::Token, expected: &str) -> bool {
    !token.string && token.text.eq_ignore_ascii_case(expected)
}

fn interface_entry(node: &Node, source: &Source<'_>) -> bool {
    if !matches!(node.language.as_str(), "vb" | "vbnet") {
        return false;
    }
    let lines: Vec<_> = source.content.lines().collect();
    let Some(header) = lines
        .get(node.start_line.saturating_sub(1) as usize..node.end_line as usize)
        .and_then(|lines| {
            lines
                .iter()
                .take(16)
                .map(|line| routing::tokens(line, true))
                .find(|tokens| tokens.iter().any(|token| word(token, "Sub")))
        })
    else {
        return false;
    };
    if header.iter().any(|token| word(token, "Shared")) {
        return false;
    }
    let Some(at) = header.iter().position(|token| word(token, "Implements")) else {
        return false;
    };
    header[at + 1..]
        .split(|token| word(token, ","))
        .any(|binding| {
            if binding.iter().any(|token| token.string) {
                return false;
            }
            matches!(
                binding
                    .iter()
                    .map(|token| token.text.as_str())
                    .collect::<String>()
                    .to_ascii_lowercase()
                    .as_str(),
                "ihttphandler.processrequest"
                    | "system.web.ihttphandler.processrequest"
                    | "global.system.web.ihttphandler.processrequest"
            )
        })
}

fn owned_by(class: &Node, method: &Node, classes: &[Node], path: &str) -> bool {
    let ranged = classes
        .iter()
        .filter(|candidate| {
            candidate.start_line < method.start_line && candidate.end_line >= method.end_line
        })
        .min_by_key(|candidate| candidate.end_line.saturating_sub(candidate.start_line));
    if ranged.is_some_and(|owner| owner.node_id == class.node_id) {
        return true;
    }
    // The VB fallback has declaration-only class ranges and qualified member IDs.
    if class.start_line != class.end_line {
        return false;
    }
    let qualified = format!("{}.{}", class.name, execution::member_name(method));
    method.name.eq_ignore_ascii_case(&qualified)
        || method
            .metadata
            .as_ref()
            .and_then(|meta| meta.get("fqn"))
            .and_then(|value| value.as_str())
            .is_some_and(|name| name.eq_ignore_ascii_case(&qualified))
        || method.node_id.eq_ignore_ascii_case(
            &engram_core::NodeId::symbol("function", None, path, &qualified, method.start_line).0,
        )
}

pub(super) fn trace(
    graph: &Arc<GraphStore>,
    project: &str,
    route: &RouteCandidate,
    action: Option<&str>,
    include_db: bool,
    include_triggers: bool,
    sources: &[Source<'_>],
    selected: &BTreeMap<String, u64>,
) -> anyhow::Result<execution::PageTrace> {
    let mut result = execution::empty_trace(&route.page, include_db, include_triggers);
    result.gaps.push("IHttpHandler bindings are static source candidates. Hosting, handler registration, request verb, authorization and successful execution are not established. Page lifecycle events are not HTTP-handler entries.".into());
    let Some(page) = sources.iter().find(|source| source.path == route.page) else {
        result.gaps.push("HTTP-handler source unavailable".into());
        return Ok(result);
    };
    let markup = execution::without_markup_comments(page.content);
    let directive =
        Regex::new(r"(?is)<%@\s*WebHandler\b.*?%>").expect("WebHandler directive syntax");
    let Some(directive) = directive.find(&markup) else {
        result
            .gaps
            .push("Authored WebHandler directive unavailable".into());
        return Ok(result);
    };
    let Some(expected) = execution::attribute(directive.as_str(), "Class") else {
        result
            .gaps
            .push("WebHandler Class identity unavailable".into());
        return Ok(result);
    };
    let root = routing::app_root(page.path, sources);
    let path = execution::attribute(directive.as_str(), "CodeFile")
        .or_else(|| execution::attribute(directive.as_str(), "CodeBehind"))
        .and_then(|raw| execution::relative_target(page.path, &raw, root));
    let Some(path) = path else {
        result.gaps.push("No supported explicit code-behind target; inline, inherited, App_Code-only and dynamic handler resolution remain unexamined".into());
        return Ok(result);
    };
    let matches: Vec<_> = sources
        .iter()
        .filter(|source| source.path.eq_ignore_ascii_case(&path))
        .collect();
    if matches.len() != 1 {
        result.gaps.push(format!(
            "HTTP-handler code-behind {path} has {}selected source matches",
            matches.len()
        ));
        return Ok(result);
    }
    let source = matches[0];
    result.codebehind = Some(source.path.into());
    let classes = graph.query_nodes_in_file(project, Some("class"), source.path, 65)?;
    if classes.len() > 64 {
        result.gaps.push(
            "HTTP-handler class inventory exceeds64; ownership incomplete and entries withheld"
                .into(),
        );
        return Ok(result);
    }
    let candidates: Vec<_> = classes
        .iter()
        .filter(|node| {
            node.name.eq_ignore_ascii_case(&expected)
                || node
                    .metadata
                    .as_ref()
                    .and_then(|meta| meta.get("fqn"))
                    .and_then(|value| value.as_str())
                    .is_some_and(|name| name.eq_ignore_ascii_case(&expected))
        })
        .collect();
    if candidates.len() != 1 {
        result.gaps.push(format!(
            "WebHandler Class={expected} has {}exact indexed identities; entries withheld",
            candidates.len()
        ));
        return Ok(result);
    }
    let class = candidates[0];
    if !execution::verified_node(graph, project, class, sources, selected)? {
        result.gaps.push(
            "WebHandler class source or selected revision changed/unavailable; entries withheld"
                .into(),
        );
        return Ok(result);
    }
    let mut methods = graph.query_nodes_in_file(project, Some("function"), source.path, 257)?;
    if methods.len() > 256 {
        methods.truncate(256);
        result
            .gaps
            .push("HTTP-handler method inventory truncated at256; coverage incomplete".into());
    }
    let requested = action.map(str::trim).filter(|value| !value.is_empty());
    let mut chosen = Vec::new();
    for method in methods {
        if !owned_by(class, &method, &classes, source.path) || !interface_entry(&method, source) {
            continue;
        }
        if requested.is_some_and(|value| {
            !value.eq_ignore_ascii_case("process request")
                && !value.eq_ignore_ascii_case(execution::member_name(&method))
        }) {
            continue;
        }
        if chosen.len() == 16 {
            result
                .gaps
                .push("HTTP-handler entry inventory truncated at16; coverage incomplete".into());
            break;
        }
        chosen.push((method, format!("Authored WebHandler Class={expected} and instance Implements IHttpHandler.ProcessRequest; static source binding, runtime dispatch unproven")));
    }
    if chosen.is_empty() {
        result.gaps.push("No admitted VB instance Implements IHttpHandler.ProcessRequest matched the directive class/action. Names alone, inherited implementations and multiline headers are not resolved.".into());
    }
    execution::trace_selected(
        graph, project, result, chosen, include_db, sources, selected,
    )
}
