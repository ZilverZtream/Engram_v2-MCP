//! Literal VB ASPX/ASMX WebMethod entries, using the existing admitted trace walker.
use super::{
    execution,
    routing::{self, RouteCandidate, Source},
};
use engram_graph::{GraphStore, Node};
use regex::Regex;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

fn word(token: &routing::Token, value: &str) -> bool {
    !token.string && token.text.eq_ignore_ascii_case(value)
}

fn entry(node: &Node, source: &Source<'_>, method: &str, page: bool) -> bool {
    if !execution::member_name(node).eq_ignore_ascii_case(method) {
        return false;
    }
    let lines: Vec<_> = source.content.lines().collect();
    let start = node.start_line.saturating_sub(1) as usize;
    // Roslyn spans can include attributes; the fallback starts at the declaration.
    let declaration = (start..(node.end_line as usize).min(lines.len()))
        .take(16)
        .find(|&i| {
            let tokens = routing::tokens(lines[i], true);
            tokens.iter().any(|t| word(t, "Function") || word(t, "Sub"))
        });
    let Some(declaration) = declaration else {
        return false;
    };
    let header = routing::tokens(lines[declaration], true);
    if !header.iter().any(|t| word(t, "Public"))
        || (page != header.iter().any(|t| word(t, "Shared")))
    {
        return false;
    }
    let preceding = lines[..declaration]
        .iter()
        .rev()
        .take(16)
        .take_while(|line| {
            let line = line.trim();
            line.is_empty() || line.starts_with('<') || line.starts_with('\'')
        })
        .copied()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    let attributes = routing::tokens(&preceding, true);
    if attributes.iter().any(|t| word(t, "MessageName")) {
        return false;
    }
    // Only an attribute name, never a quoted example/comment or a method-body call.
    attributes.windows(2).any(|pair| {
        pair[0].text == "<"
            && !pair[0].string
            && (word(&pair[1], "WebMethod") || word(&pair[1], "WebMethodAttribute"))
    }) || attributes.windows(8).any(|part| {
        part[0].text == "<"
            && word(&part[1], "System")
            && part[2].text == "."
            && word(&part[3], "Web")
            && part[4].text == "."
            && word(&part[5], "Services")
            && part[6].text == "."
            && (word(&part[7], "WebMethod") || word(&part[7], "WebMethodAttribute"))
    })
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
    let method = route.web_method.as_deref().expect("WebMethod route entry");
    if action.is_some_and(|value| !value.eq_ignore_ascii_case(method)) {
        result.gaps.push(
            "action conflicts with the literal WebMethod URL suffix; no entry selected".into(),
        );
        return Ok(result);
    }
    let Some(page_source) = sources.iter().find(|s| s.path == route.page) else {
        result
            .gaps
            .push("WebMethod page/service source unavailable".into());
        return Ok(result);
    };
    let markup = execution::without_markup_comments(page_source.content);
    let service = route.page.to_ascii_lowercase().ends_with(".asmx");
    let directive = Regex::new(if service {
        r"(?is)<%@\s*WebService\b.*?%>"
    } else {
        r"(?is)<%@\s*Page\b.*?%>"
    })
    .expect("directive syntax");
    let Some(directive) = directive.find(&markup) else {
        result
            .gaps
            .push("Authored Page/WebService directive unavailable".into());
        return Ok(result);
    };
    let directive = directive.as_str();
    let Some(expected) =
        execution::attribute(directive, if service { "Class" } else { "Inherits" })
    else {
        result
            .gaps
            .push("WebMethod type identity missing from the authored directive".into());
        return Ok(result);
    };
    let root = routing::app_root(page_source.path, sources);
    let explicit = execution::attribute(directive, "CodeFile")
        .or_else(|| execution::attribute(directive, "CodeBehind"))
        .and_then(|path| execution::relative_target(page_source.path, &path, root));
    let app_code = root.map(|root| {
        if root.is_empty() {
            "App_Code/".to_owned()
        } else {
            format!("{root}/App_Code/")
        }
    });
    let mut classes = graph.query_nodes(project, Some("class"), Some(&expected), None, 65)?;
    if classes.len() > 64 {
        classes.truncate(64);
        result
            .gaps
            .push("WebMethod type candidate inventory truncated at64".into());
    }
    let mut chosen = Vec::new();
    let mut files = BTreeSet::new();
    let mut seen = BTreeSet::new();
    'types: for class in classes
        .iter()
        .filter(|class| class.name.eq_ignore_ascii_case(&expected))
    {
        let Some(source) = sources.iter().find(|s| s.path == class.file_path.as_str()) else {
            continue;
        };
        if !source.path.to_ascii_lowercase().ends_with(".vb") {
            continue;
        }
        let explicit_file = explicit
            .as_deref()
            .is_some_and(|p| p.eq_ignore_ascii_case(source.path));
        let app_code_file = service
            && app_code
                .as_deref()
                .is_some_and(|prefix| source.path.starts_with(prefix))
            && routing::app_root(source.path, sources) == root;
        if !explicit_file && !app_code_file {
            continue;
        }
        if !execution::verified_node(graph, project, class, sources, selected)? {
            result.gaps.push(format!(
                "{}: WebMethod type source/revision not admitted",
                source.path
            ));
            continue;
        }
        let short_type = expected.rsplit('.').next().unwrap_or(&expected);
        let declaration = source
            .content
            .lines()
            .skip(class.start_line.saturating_sub(1) as usize)
            .take((class.end_line - class.start_line + 1).min(16) as usize)
            .any(|line| {
                routing::tokens(line, true)
                    .windows(2)
                    .any(|pair| word(&pair[0], "Class") && word(&pair[1], short_type))
            });
        if !declaration {
            continue;
        }
        let mut members = graph.query_nodes_in_file(project, Some("function"), source.path, 257)?;
        if members.len() > 256 {
            members.truncate(256);
            result.gaps.push(format!(
                "{}: WebMethod member inventory truncated at256",
                source.path
            ));
        }
        let file_classes = graph.query_nodes_in_file(project, Some("class"), source.path, 65)?;
        if file_classes.len() > 64 {
            result.gaps.push(format!(
                "{}: class ownership inventory incomplete",
                source.path
            ));
            continue;
        }
        for node in members.iter().filter(|n| n.start_line > class.start_line) {
            let owner = file_classes
                .iter()
                .filter(|c| n_contains(c, node))
                .min_by_key(|c| c.end_line.saturating_sub(c.start_line));
            let ranged_owner = owner.is_some_and(|owner| owner.node_id == class.node_id);
            // The existing VB fallback records a class declaration line, while
            // its functions retain qualified names and complete method ranges.
            // A one-line class record cannot establish enclosing body bounds.
            let qualified = format!("{}.{method}", class.name);
            let qualified_id = engram_core::NodeId::symbol(
                "function",
                None,
                source.path,
                &qualified,
                node.start_line,
            );
            let named_owner = class.start_line == class.end_line
                && ([
                    Some(node.name.as_str()),
                    node.metadata
                        .as_ref()
                        .and_then(|m| m.get("fqn"))
                        .and_then(|v| v.as_str()),
                ]
                .into_iter()
                .flatten()
                .any(|name| name.eq_ignore_ascii_case(&qualified))
                    || node.node_id.eq_ignore_ascii_case(&qualified_id.0));
            if (!ranged_owner && !named_owner) || !entry(node, source, method, !service) {
                continue;
            }
            if seen.insert(node.node_id.clone()) {
                if chosen.len() == 16 {
                    result
                        .gaps
                        .push("WebMethod entry expansion truncated at16 candidates".into());
                    break 'types;
                }
                files.insert(source.path.to_owned());
                let owner_basis = if ranged_owner {
                    "indexed class/member ranges"
                } else {
                    "indexed qualified name with declaration-only class range; compiler ownership unverified"
                };
                chosen.push((node.clone(), format!("Authored {} Class/Inherits={expected} and public {}WebMethod {method}; {owner_basis}; source candidate, runtime dispatch not established", if service { "WebService" } else { "Page" }, if service { "" } else { "Shared " })));
            }
        }
    }
    if chosen.is_empty() {
        result.gaps.push("No admitted public VB WebMethod matched the directive type and URL suffix; page methods require Shared. Inherited methods, inline server code, aliases and unavailable class identities are not resolved.".into());
    }
    if chosen.len() > 1 {
        result.gaps.push("Multiple attributed WebMethod implementations remain candidates; overload/runtime selection is not established".into());
    }
    if files.len() == 1 {
        result.codebehind = files.into_iter().next();
    }
    result.gaps.push("WebMethod attributes and source types are static candidates. Hosting/configuration, HTTP protocol/verb, serialization, authentication, session availability and successful execution are not established. Page lifecycle handlers are not included in this method entry.".into());
    execution::trace_selected(
        graph, project, result, chosen, include_db, sources, selected,
    )
}

fn n_contains(class: &Node, member: &Node) -> bool {
    class.start_line < member.start_line && class.end_line >= member.end_line
}
