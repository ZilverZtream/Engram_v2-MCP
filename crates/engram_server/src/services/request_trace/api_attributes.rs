//! Literal VB attribute routes over the caller's admitted source inventory.
use super::{
    api::{self, AttributeRegistration, DeclaredAction, Route},
    execution,
    routing::{self, Routing, Source},
};
use engram_graph::{GraphStore, Node};
use std::{collections::BTreeMap, sync::Arc};

fn word(token: &routing::Token, name: &str) -> bool {
    !token.string && token.text.eq_ignore_ascii_case(name)
}

fn attributes(text: &str, name: &str) -> Vec<String> {
    routing::tokens(text, true)
        .windows(4)
        .filter_map(|tokens| {
            (word(&tokens[0], name)
                && word(&tokens[1], "(")
                && tokens[2].string
                && tokens[2].text.len() <= 4096
                && word(&tokens[3], ")"))
            .then(|| tokens[2].text.clone())
        })
        .collect()
}

fn preceding(lines: &[&str], declaration: usize) -> String {
    let mut lines = lines[..declaration]
        .iter()
        .rev()
        .take(16)
        .take_while(|line| {
            let line = line.trim();
            line.is_empty() || line.starts_with('<') || line.starts_with('\'')
        })
        .copied()
        .collect::<Vec<_>>();
    lines.reverse();
    lines.join("\n")
}

fn declaration(lines: &[&str], node: &Node, kind: &str) -> Option<usize> {
    (node.start_line.saturating_sub(1) as usize..(node.end_line as usize).min(lines.len()))
        .take(16)
        .find(|&line| {
            let tokens = routing::tokens(lines[line], true);
            tokens.iter().any(|token| word(token, "Public"))
                && tokens
                    .iter()
                    .any(|token| word(token, kind) || (kind == "Function" && word(token, "Sub")))
        })
}

// Keep the original template as evidence. Only whole named segments and the
// common integer constraint are interpreted; this is not a route runtime.
fn parameters(pattern: &str, request: &str) -> Option<BTreeMap<String, String>> {
    let parts = request.trim_matches('/').split('/').collect::<Vec<_>>();
    let mut plain = Vec::new();
    for (index, segment) in pattern.trim_matches('/').split('/').enumerate() {
        if let Some(body) = segment
            .strip_prefix('{')
            .and_then(|part| part.strip_suffix('}'))
        {
            if let Some((name, constraint)) = body.split_once(':') {
                if constraint != "int" || parts.get(index)?.parse::<i32>().is_err() {
                    return None;
                }
                plain.push(format!("{{{name}}}"));
                continue;
            }
        }
        plain.push(segment.to_owned());
    }
    routing::parameters(&plain.join("/"), request)
}

fn capped(result: &mut Routing) -> bool {
    if result.api_candidates.len() < routing::MAX_CANDIDATES
        && result.gaps.len() < routing::MAX_CANDIDATES
    {
        return false;
    }
    result.candidates_truncated = true;
    result.gaps.push("Attribute route scan stopped at its candidate/diagnostic limit; remaining coverage is unknown.".into());
    true
}

/// Add bounded, exact-action candidates; conventional candidates stay intact.
pub fn extend(
    graph: &Arc<GraphStore>,
    project: &str,
    result: &mut Routing,
    sources: &[Source<'_>],
    selected: &BTreeMap<String, u64>,
) -> anyhow::Result<()> {
    let mut registrations = Vec::new();
    'registration_scan: for source in sources
        .iter()
        .filter(|source| source.path.to_ascii_lowercase().ends_with(".vb"))
    {
        let tokens = routing::tokens(source.content, true);
        for call in tokens.windows(4).filter(|call| {
            word(&call[0], ".")
                && word(&call[1], "MapHttpAttributeRoutes")
                && word(&call[2], "(")
                && word(&call[3], ")")
        }) {
            if registrations.len() == routing::MAX_CANDIDATES {
                result.candidates_truncated = true;
                result.gaps.push(
                    "Attribute registration inventory stopped at its candidate limit.".into(),
                );
                break 'registration_scan;
            }
            registrations.push((
                routing::app_root(source.path, sources),
                AttributeRegistration {
                    source_path: source.path.into(),
                    source_line: call[1].line,
                    source_blake3: blake3::hash(source.content.as_bytes()).to_hex().to_string(),
                    evidence: source
                        .content
                        .lines()
                        .nth(call[1].line - 1)
                        .unwrap_or_default()
                        .into(),
                },
            ));
        }
    }
    if registrations.is_empty() {
        return Ok(());
    }
    let initial = result.api_candidates.len();
    for source in sources
        .iter()
        .filter(|source| source.path.to_ascii_lowercase().ends_with("controller.vb"))
    {
        if capped(result) {
            return Ok(());
        }
        let Some(root) = routing::app_root(source.path, sources) else {
            continue;
        };
        let enabled = registrations
            .iter()
            .filter(|(app, _)| *app == Some(root))
            .collect::<Vec<_>>();
        if enabled.is_empty() {
            continue;
        }
        let tokens = routing::tokens(source.content, true);
        if !tokens.iter().any(|token| word(token, "Route")) {
            continue;
        }
        let admit = |node: &Node, _file: Option<&Node>| {
            Ok(
                if selected.get(node.file_path.as_str()) == Some(&node.generation) {
                    engram_graph::store::ImpactAdmission::Selected
                } else {
                    engram_graph::store::ImpactAdmission::NonSelectedRevision
                },
            )
        };
        let (ids, coverage) =
            graph.bounded_file_navigation_seeds(project, &[source.path.to_owned()], &admit)?;
        if coverage.truncated {
            result.gaps.push(format!(
                "{}: attribute action inventory truncated",
                source.path
            ));
        }
        let nodes = ids
            .iter()
            .map(|id| graph.get_node(project, id))
            .collect::<anyhow::Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        let lines = source.content.lines().collect::<Vec<_>>();
        for class in nodes.iter().filter(|node| node.node_type == "class") {
            if capped(result) {
                return Ok(());
            }
            let class_name = class.name.rsplit('.').next().unwrap_or(&class.name);
            if !class_name.to_ascii_lowercase().ends_with("controller")
                || !execution::verified_node(graph, project, class, sources, selected)?
            {
                continue;
            }
            let Some(class_line) = declaration(&lines, class, "Class") else {
                continue;
            };
            let header = lines[class_line..lines.len().min(class_line + 6)].join("\n");
            if !routing::tokens(&header, true)
                .windows(2)
                .any(|tokens| word(&tokens[0], "Inherits") && word(&tokens[1], "ApiController"))
            {
                continue;
            }
            let prefixes = attributes(&preceding(&lines, class_line), "RoutePrefix");
            if prefixes.len() > 1 {
                result.gaps.push(format!(
                    "{}: multiple RoutePrefix declarations remain unresolved",
                    source.path
                ));
                continue;
            }
            let prefix = prefixes.first().map(String::as_str).unwrap_or("");
            for method in nodes.iter().filter(|node| {
                node.node_type == "function"
                    && node.start_line > class.start_line
                    && node.end_line <= class.end_line
            }) {
                if capped(result) {
                    return Ok(());
                }
                if !execution::verified_node(graph, project, method, sources, selected)? {
                    continue;
                }
                let Some(line) = declaration(&lines, method, "Function") else {
                    continue;
                };
                let authored = preceding(&lines, line);
                let name = execution::member_name(method);
                if !api::method_selected(
                    lines[line],
                    &authored,
                    result.method.as_deref(),
                    Some(name),
                    name,
                ) {
                    continue;
                }
                for literal in attributes(&authored, "Route") {
                    let pattern = if let Some(absolute) = literal.strip_prefix("~/") {
                        absolute.to_owned()
                    } else {
                        [prefix.trim_matches('/'), literal.trim_matches('/')]
                            .into_iter()
                            .filter(|part| !part.is_empty())
                            .collect::<Vec<_>>()
                            .join("/")
                    };
                    if pattern.len() > 4096 {
                        continue;
                    }
                    let Some(parameters) = parameters(&pattern, &result.request_path) else {
                        continue;
                    };
                    for (_, registration) in &enabled {
                        if capped(result) {
                            return Ok(());
                        }
                        result.api_candidates.push(Route {
                            controller: class_name[..class_name.len() - "Controller".len()].into(),
                            route_name: format!("attribute:{class_name}.{name}"),
                            pattern: pattern.clone(),
                            parameters: parameters.clone(),
                            source_path: source.path.into(),
                            source_line: line + 1,
                            source_blake3: blake3::hash(source.content.as_bytes())
                                .to_hex()
                                .to_string(),
                            evidence: authored.clone(),
                            declared_action: Some(DeclaredAction {
                                class_node_id: class.node_id.clone(),
                                method_node_id: method.node_id.clone(),
                                name: name.into(),
                                declaration_line: line + 1,
                                route_prefix: prefix.into(),
                                registration: AttributeRegistration::clone(registration),
                            }),
                        });
                    }
                }
            }
        }
    }
    if result.api_candidates.len() > initial {
        result
            .gaps
            .retain(|gap| !gap.starts_with("No route/page candidate in selected sources."));
        result.gaps.push("Attribute candidates interpret literal Route/RoutePrefix, whole parameters and int constraints only. Registration control flow, other constraints, optional/composite segments and runtime precedence remain unproven.".into());
    }
    Ok(())
}
