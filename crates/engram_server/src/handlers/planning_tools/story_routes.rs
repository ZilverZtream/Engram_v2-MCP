//! Resolve HTTP paths from story text against registered source routes.
use super::story_sources::Witness;
use crate::services::request_trace::{api, routing::Source};
use engram_graph::{EdgeKind, GraphStore, Node};
use engram_index::{
    HybridSearchEngine,
    grep::{self, FreshnessMode, GrepQuery},
};
use regex::Regex;
use std::{
    collections::{BTreeSet, HashMap},
    path::Path,
    sync::LazyLock,
};

pub(super) fn mentions(text: &str) -> Vec<(String, String)> {
    static URL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\b(GET|POST|PUT|PATCH|DELETE|HEAD)\s+(/[\w/{\}.:-]+)").unwrap()
    });
    URL.captures_iter(text)
        .take(8)
        .map(|m| {
            (
                m[1].to_ascii_uppercase(),
                m[2].trim_end_matches(['.', '/']).to_owned(),
            )
        })
        .collect()
}

struct Target {
    node: Node,
    line: usize,
    text: String,
    reason: String,
}

fn returned_call(body: &[&str], start: usize) -> Option<(usize, String, String, String)> {
    static CALL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^\s*return\s+([A-Za-z_]\w*(?:\.\w+)*)\.(\w+)\s*\(").unwrap()
    });
    body.iter().enumerate().find_map(|(i, line)| {
        let m = CALL.captures(line)?;
        Some((start + i, line.trim().into(), m[1].into(), m[2].into()))
    })
}

fn next_owner(graph: &GraphStore, project: &str, root: &Path, previous: &Target) -> Option<Target> {
    let n = &previous.node;
    let content = crate::planner_source::read(graph, project, root, n.file_path.as_str()).ok()?;
    let lines: Vec<_> = content.lines().collect();
    let body = lines.get(n.start_line.checked_sub(1)? as usize..n.end_line as usize)?;
    let (call_line, _, receiver, name) = returned_call(body, n.start_line as usize)?;
    let calls = graph
        .neighbors(project, EdgeKind::Calls, &n.node_id, 128)
        .ok()?;
    let linked: BTreeSet<_> = calls.into_iter().map(|(id, _)| id).collect();
    let receiver_type = receiver
        .rsplit('.')
        .next()?
        .trim_start_matches('_')
        .to_lowercase();
    let mut candidates = std::collections::BTreeMap::new();
    for id in &linked {
        if let Some(node) = graph.get_node(project, id).ok()? {
            if node.name.rsplit('.').next()?.eq_ignore_ascii_case(&name) {
                candidates.insert(node.node_id.clone(), node);
            }
        }
    }
    for class in graph
        .query_nodes_by_symbol_name(project, &receiver_type, None, 32)
        .ok()?
    {
        if class.node_type != "class" {
            continue;
        }
        for node in graph
            .functions_for_source_lines(project, class.file_path.as_str())
            .ok()?
        {
            if node.name.rsplit('.').next()?.eq_ignore_ascii_case(&name)
                && node.start_line >= class.start_line
                && node.end_line <= class.end_line
            {
                candidates.insert(node.node_id.clone(), node);
            }
        }
    }
    let mut matches = Vec::new();
    for candidate in candidates.into_values() {
        if candidate.node_type != "function" || candidate.node_id == n.node_id {
            continue;
        }
        let owner = candidate
            .name
            .rsplit_once('.')
            .map(|(o, _)| o.rsplit('.').next().unwrap_or(o))
            .unwrap_or("");
        let file = candidate.file_path.as_str();
        let stem = Path::new(file).file_stem()?.to_str()?.to_lowercase();
        // Field names alone cannot establish binding. For interface dispatch require the authored
        // receiver type plus an explicit implementation on the matching concrete declaration.
        let text = if linked.contains(&candidate.node_id)
            || owner.eq_ignore_ascii_case(&receiver_type)
            || stem == receiver_type
        {
            crate::planner_source::read(graph, project, root, file).ok()
        } else {
            None
        };
        let Some(text) = text else { continue };
        let Some(line) = text
            .lines()
            .nth(candidate.start_line.saturating_sub(1) as usize)
        else {
            continue;
        };
        let lower = line.to_lowercase();
        let interface = format!("i{receiver_type}");
        let declared = content.lines().any(|line| {
            let parts = line
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>();
            parts.windows(3).any(|w| {
                w[0].eq_ignore_ascii_case(&receiver)
                    && w[1].eq_ignore_ascii_case("as")
                    && parts.iter().any(|s| s.eq_ignore_ascii_case(&interface))
            })
        });
        let qualified_owner = candidate.name.to_lowercase();
        let static_receiver = receiver.contains('.')
            && qualified_owner.trim_start_matches('_').ends_with(&format!(
                "{}.{}",
                receiver.trim_start_matches('_').to_lowercase(),
                name.to_lowercase()
            ));
        if !static_receiver
            && !linked.contains(&candidate.node_id)
            && !(declared
                && lower.contains(&format!("implements {interface}.{}", name.to_lowercase())))
        {
            continue;
        }
        if lower.trim_start().starts_with("function ")
            && text.lines().any(|l| {
                l.trim_start()
                    .to_lowercase()
                    .starts_with("public interface ")
            })
            && !lower.contains("implements ")
        {
            continue;
        }
        matches.push(Target {
            line: candidate.start_line as usize,
            text: line.trim().into(),
            node: candidate,
            reason: format!(
                "{}; {}:{} returns {receiver}.{name}",
                previous.reason,
                n.file_path.as_str(),
                call_line
            ),
        });
    }
    (matches.len() == 1).then(|| matches.pop().unwrap())
}

pub(super) fn extend(
    out: &mut HashMap<String, Witness>,
    graph: &GraphStore,
    engine: &HybridSearchEngine,
    project: &str,
    root: &Path,
    generation: u64,
    story: &str,
) {
    let mentions = mentions(story);
    if mentions.is_empty() {
        return;
    }
    let query = GrepQuery {
        project_id: project.into(),
        namespace: "memory".into(),
        generation,
        pattern: "MapHttpRoute".into(),
        regex: false,
        case_sensitive: Some(false),
        multiline: false,
        path_prefix: None,
        exclude_path_prefixes: vec![],
        language: None,
        context_before: 0,
        context_after: 0,
        max_results: 128,
        freshness: FreshnessMode::Off,
    };
    let Ok(hits) = grep::grep(engine, root, &query, || Ok(Vec::new())) else {
        return;
    };
    let paths: BTreeSet<_> = hits.matches.into_iter().map(|m| m.file_path).collect();
    for (verb, path) in mentions
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(3)
    {
        let mut actions = Vec::new();
        for file in &paths {
            let Ok(content) = crate::planner_source::read(graph, project, root, file) else {
                continue;
            };
            for route in api::routes(
                &Source {
                    path: file,
                    content: &content,
                },
                &path,
            ) {
                let class_name = format!("{}Controller", route.controller);
                let Ok(classes) = graph.query_nodes_by_symbol_name(project, &class_name, None, 16)
                else {
                    continue;
                };
                for class in classes.into_iter().filter(|n| n.node_type == "class") {
                    let source = class.file_path.as_str();
                    let Ok(text) = crate::planner_source::read(graph, project, root, source) else {
                        continue;
                    };
                    let lines: Vec<_> = text.lines().collect();
                    if !lines
                        .iter()
                        .skip(class.start_line.saturating_sub(1) as usize)
                        .take(8)
                        .any(|l| l.to_ascii_lowercase().contains("inherits apicontroller"))
                    {
                        continue;
                    }
                    for node in graph
                        .functions_for_source_lines(project, source)
                        .unwrap_or_default()
                    {
                        let start = node.start_line.saturating_sub(1) as usize;
                        let Some(header) = lines.get(start) else {
                            continue;
                        };
                        if node.start_line < class.start_line || node.end_line > class.end_line {
                            continue;
                        }
                        let name = node.name.rsplit('.').next().unwrap_or(&node.name);
                        let attrs = lines[..start]
                            .iter()
                            .rev()
                            .take_while(|l| l.trim().starts_with('<'))
                            .copied()
                            .collect::<Vec<_>>()
                            .join(" ");
                        if !api::method_selected(header, &attrs, Some(&verb), None, name) {
                            continue;
                        }
                        let omitted = route
                            .pattern
                            .split('/')
                            .filter_map(|s| s.strip_prefix('{')?.strip_suffix('}'))
                            .filter(|key| {
                                !route.parameters.keys().any(|k| k.eq_ignore_ascii_case(key))
                            });
                        let identifiers: Vec<_> = header
                            .split(|c: char| !c.is_alphanumeric() && c != '_')
                            .collect();
                        if omitted
                            .into_iter()
                            .any(|key| identifiers.iter().any(|t| t.eq_ignore_ascii_case(key)))
                        {
                            continue;
                        }
                        actions.push(Target {
                            line: node.start_line as usize,
                            text: header.trim().into(),
                            node,
                            reason: format!(
                                "{verb} {path} matches {}:{} ({})",
                                route.source_path, route.source_line, route.pattern
                            ),
                        });
                    }
                }
            }
        }
        actions.sort_by(|a, b| a.node.node_id.cmp(&b.node.node_id));
        actions.dedup_by(|a, b| a.node.node_id == b.node.node_id);
        if actions.len() != 1 {
            continue;
        }
        let action = actions.pop().unwrap();
        let mut chain = vec![action];
        for _ in 0..2 {
            let Some(next) = next_owner(graph, project, root, chain.last().unwrap()) else {
                break;
            };
            if chain.iter().any(|a| a.node.node_id == next.node.node_id) {
                break;
            }
            chain.push(next);
        }
        // Prefer the owners reached through the controller. Keep its registration in their evidence.
        for (index, target) in chain.into_iter().rev().take(2).enumerate() {
            out.insert(
                target.node.node_id,
                Witness {
                    line: target.line,
                    text: target.text,
                    reason: target.reason,
                    route: Some(path.clone()),
                    score: 520 - index * 12,
                },
            );
        }
    }
}
