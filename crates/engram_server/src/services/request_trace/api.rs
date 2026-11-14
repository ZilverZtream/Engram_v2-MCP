//! Conventional VB Web API entry candidates. Dispatch and DI remain runtime questions.
use super::{
    execution,
    routing::{self, Source, Token},
};
use engram_graph::GraphStore;
use serde::Serialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Debug, Serialize)]
pub struct Route {
    pub controller: String,
    pub route_name: String,
    pub pattern: String,
    pub parameters: BTreeMap<String, String>,
    pub source_path: String,
    pub source_line: usize,
    pub source_blake3: String,
    pub evidence: String,
}

fn word(token: &Token, value: &str) -> bool {
    !token.string && token.text.eq_ignore_ascii_case(value)
}

// Split only the outer invocation; a defaults object's commas are not arguments.
fn arguments(tokens: &[Token]) -> Option<Vec<&[Token]>> {
    let mut depth = 0usize;
    let mut start = 0;
    let mut parts = Vec::new();
    for (i, token) in tokens.iter().enumerate().take(512) {
        if token.string {
            continue;
        }
        match token.text.as_str() {
            "(" | "{" => depth += 1,
            ")" if depth == 0 => {
                parts.push(&tokens[start..i]);
                return Some(parts);
            }
            ")" | "}" => depth = depth.checked_sub(1)?,
            "," if depth == 0 => {
                parts.push(&tokens[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    None
}

pub(super) fn routes(source: &Source<'_>, path: &str) -> Vec<Route> {
    if !source.path.to_ascii_lowercase().ends_with(".vb") {
        return Vec::new();
    }
    let tokens = routing::tokens(source.content, true);
    let mut result = Vec::new();
    for i in 1..tokens.len().saturating_sub(1) {
        if !word(&tokens[i - 1], ".")
            || !word(&tokens[i], "MapHttpRoute")
            || !word(&tokens[i + 1], "(")
        {
            continue;
        }
        let Some(parts) = arguments(&tokens[i + 2..]) else {
            continue;
        };
        let mut name = None;
        let mut template = None;
        for (position, part) in parts.iter().enumerate() {
            let (key, value) = match *part {
                [value] if value.string => (
                    if position == 0 {
                        "name"
                    } else if position == 1 {
                        "routeTemplate"
                    } else {
                        ""
                    },
                    value,
                ),
                [key, colon, equal, value]
                    if !key.string && word(colon, ":") && word(equal, "=") && value.string =>
                {
                    (key.text.as_str(), value)
                }
                _ => continue,
            };
            if value.text.len() > 4096 {
                continue;
            }
            if key.eq_ignore_ascii_case("name") {
                name = Some(value.text.clone());
            }
            if key.eq_ignore_ascii_case("routeTemplate") {
                template = Some(value.text.clone());
            }
        }
        let (Some(name), Some(template)) = (name, template) else {
            continue;
        };
        let Some(parameters) = routing::parameters(&template, path) else {
            continue;
        };
        let Some(controller) = parameters
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("controller"))
            .map(|(_, value)| value.clone())
        else {
            continue;
        };
        if controller.is_empty()
            || !controller
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            continue;
        }
        result.push(Route {
            controller,
            route_name: name,
            pattern: template,
            parameters,
            source_path: source.path.into(),
            source_line: tokens[i].line,
            source_blake3: blake3::hash(source.content.as_bytes()).to_hex().to_string(),
            evidence: source
                .content
                .lines()
                .skip(tokens[i].line - 1)
                .take(6)
                .collect::<Vec<_>>()
                .join("\n"),
        });
        if result.len() == routing::MAX_CANDIDATES {
            break;
        }
    }
    result
}

fn method_selected(
    body: &str,
    preceding: &str,
    verb: Option<&str>,
    action: Option<&str>,
    name: &str,
) -> bool {
    let header = routing::tokens(body.lines().next().unwrap_or(""), true);
    if !header.iter().any(|t| word(t, "Public")) || name.eq_ignore_ascii_case("New") {
        return false;
    }
    if action.is_some_and(|value| !value.eq_ignore_ascii_case(name)) {
        return false;
    }
    let attributes = routing::tokens(preceding, true);
    if attributes.iter().any(|t| word(t, "NonAction")) {
        return false;
    }
    let explicit: Vec<_> = ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"]
        .into_iter()
        .filter(|v| attributes.iter().any(|t| word(t, &format!("Http{v}"))))
        .collect();
    if let Some(verb) = verb {
        if !explicit.is_empty() {
            return explicit.iter().any(|v| v.eq_ignore_ascii_case(verb));
        }
        // Only the documented name convention; no inferred default POST selection.
        return name
            .to_ascii_lowercase()
            .starts_with(&verb.to_ascii_lowercase());
    }
    !explicit.is_empty() || action.is_some()
}

pub fn trace(
    graph: &Arc<GraphStore>,
    project: &str,
    route: &Route,
    verb: Option<&str>,
    action: Option<&str>,
    include_db: bool,
    include_triggers: bool,
    sources: &[Source<'_>],
    selected: &BTreeMap<String, u64>,
) -> anyhow::Result<Value> {
    let expected = format!("{}Controller", route.controller);
    let declared_dependency_registrations = super::container_registrations::in_route_source(&route.source_path, sources);
    let mut outputs = Vec::new();
    let mut gaps = vec!["Conventional source candidates only: route registration, precedence, constraints, omitted optional segments, action overload selection and dependency injection are not established.".to_string()];
    let root = routing::app_root(&route.source_path, sources);
    for source in sources.iter().filter(|s| {
        s.path.to_ascii_lowercase().ends_with(".vb") && routing::app_root(s.path, sources) == root
    }) {
        let tokens = routing::tokens(source.content, true);
        if !tokens
            .windows(2)
            .any(|w| word(&w[0], "Class") && word(&w[1], &expected))
        {
            continue;
        }
        let admit = |node: &engram_graph::Node, _file: Option<&engram_graph::Node>| {
            Ok(
                if selected.get(node.file_path.as_str()) == Some(&node.generation) {
                    engram_graph::store::ImpactAdmission::Selected
                } else {
                    engram_graph::store::ImpactAdmission::NonSelectedRevision
                },
            )
        };
        let (ids, coverage) =
            graph.bounded_file_navigation_seeds(project, &[source.path.to_string()], &admit)?;
        if coverage.truncated {
            gaps.push(format!(
                "{}: controller member inventory truncated",
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
        for class in nodes.iter().filter(|n| {
            n.node_type == "class"
                && n.name
                    .rsplit('.')
                    .next()
                    .is_some_and(|name| name.eq_ignore_ascii_case(&expected))
        }) {
            if !execution::verified_node(graph, project, class, sources, selected)? {
                gaps.push(format!(
                    "{}: controller source differs from selected revision",
                    source.path
                ));
                continue;
            }
            let lines = source.content.lines().collect::<Vec<_>>();
            let header = lines
                .iter()
                .skip(class.start_line as usize - 1)
                .take(6)
                .copied()
                .collect::<Vec<_>>()
                .join("\n");
            let class_tokens = routing::tokens(&header, true);
            if !class_tokens
                .windows(2)
                .any(|w| word(&w[0], "Inherits") && word(&w[1], "ApiController"))
            {
                gaps.push(format!(
                    "{}: direct ApiController inheritance not established",
                    class.name
                ));
                continue;
            }
            let mut chosen = Vec::new();
            for node in nodes.iter().filter(|n| {
                n.node_type == "function"
                    && n.start_line > class.start_line
                    && n.end_line <= class.end_line
            }) {
                let name = execution::member_name(node);
                let start = node.start_line.saturating_sub(1) as usize;
                // Roslyn includes attributes in a MethodBlock span; fallback
                // indexes can start at the declaration itself. Keep both citations.
                let declaration = (start..(node.end_line as usize).min(lines.len()))
                    .take(16)
                    .find(|&i| {
                        let t = routing::tokens(lines[i], true);
                        t.iter().any(|v| word(v, "Public"))
                            && t.iter().any(|v| word(v, "Sub") || word(v, "Function"))
                    });
                let Some(declaration) = declaration else {
                    continue;
                };
                let body = lines[declaration];
                let preceding = lines[..declaration]
                    .iter()
                    .rev()
                    .take(16)
                    .take_while(|line| {
                        let l = line.trim();
                        l.is_empty() || l.starts_with('<') || l.starts_with('\'')
                    })
                    .copied()
                    .collect::<Vec<_>>()
                    .join("\n");
                if method_selected(
                    body,
                    &preceding,
                    verb,
                    action.or_else(|| route.parameters.get("action").map(String::as_str)),
                    name,
                ) {
                    chosen.push((node.clone(), format!("Conventional controller {} and authored HTTP verb/action candidate; runtime dispatch not proven", class.name)));
                }
            }
            let mut initial = execution::empty_trace(source.path, include_db, include_triggers);
            initial.codebehind = Some(source.path.into());
            if chosen.is_empty() {
                initial.gaps.push(
                    "No public action matching the requested HTTP verb/action was established."
                        .into(),
                );
            }
            let trace = execution::trace_selected(
                graph, project, initial, chosen, include_db, sources, selected,
            )?;
            outputs.push(json!({"controller":class.name,"controller_file":source.path,"controller_line":class.start_line,
                "handlers":trace.handlers,"paths":trace.paths,"source_members":trace.source_members,"gaps":trace.gaps,
                "trigger_coverage":trace.trigger_coverage,"interpretation":trace.interpretation}));
            if outputs.len() >= 8 {
                gaps.push("Controller expansion stopped at eight candidates.".into());
                return Ok(json!({"route":route,"controllers":outputs,"gaps":gaps,"declared_dependency_registrations":declared_dependency_registrations}));
            }
        }
    }
    if outputs.is_empty() {
        gaps.push("No source-verified VB ApiController matched the route controller value.".into());
    }
    Ok(json!({"route":route,"controllers":outputs,"gaps":gaps,"declared_dependency_registrations":declared_dependency_registrations}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn conventional_vb_route_and_post_action_are_candidates_with_parameters() {
        let source = Source {
            path: "Site/App_Code/Config.vb",
            content: "config.Routes.MapHttpRoute(\nname:=\"hooks\",\nrouteTemplate:=\"webhook/{controller}/{projectId}\",\ndefaults:=New With {.projectId = RouteParameter.Optional})",
        };
        let routes = routes(&source, "webhook/vetro/42");
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].controller, "vetro");
        assert_eq!(routes[0].parameters["projectId"], "42");
        assert_eq!(routes[0].source_line, 1);
        assert!(method_selected(
            "Public Sub Receive(projectId As Integer)",
            "<HttpPost>",
            Some("POST"),
            None,
            "Receive"
        ));
        assert!(!method_selected(
            "Public Sub Receive(projectId As Integer)",
            "<HttpPost>",
            Some("GET"),
            None,
            "Receive"
        ));
        assert!(!method_selected(
            "Public Sub Receive(projectId As Integer)",
            "<HttpPost, NonAction>",
            Some("POST"),
            None,
            "Receive"
        ));
        let positional = Source {
            path: source.path,
            content: "routes.MapHttpRoute(\"hooks\", \"webhook/{controller}/{projectId}\")",
        };
        assert_eq!(super::routes(&positional, "webhook/vetro/42").len(), 1);
    }

    #[test]
    fn api_trace_returns_authored_handler_source_and_keeps_http_verbs_separate() {
        let temp = tempfile::tempdir().unwrap();
        let graph = Arc::new(GraphStore::open(&temp.path().join("graph.redb")).unwrap());
        let path = "Site/App_Code/InventoryController.vb";
        let vb = "Public Class InventoryController\nInherits ApiController\n<HttpPost>\nPublic Sub Save(id As Integer)\nResponse.Write(\"saved\")\nEnd Sub\n<HttpGet>\nPublic Function Read(id As Integer) As String\nReturn \"read\"\nEnd Function\nEnd Class\n";
        let sources = [
            Source {
                path: "Site/Global.asax",
                content: "<%@ Application %>",
            },
            Source {
                path: "Site/App_Code/Routes.vb",
                content: "config.Routes.MapHttpRoute(\"inventory\", \"api/{controller}/{id}\")",
            },
            Source { path, content: vb },
        ];
        let mut nodes = Vec::new();
        for (id, kind, name, start, end) in [
            ("class", "class", "InventoryController", 1, 11),
            ("post", "function", "InventoryController.Save", 3, 6),
            ("get", "function", "InventoryController.Read", 7, 10),
            ("file", "file", path, 1, 11),
        ] {
            nodes.push(engram_graph::Node {
                node_id: if id == "file" {
                    format!("file:{path}")
                } else {
                    id.into()
                },
                node_type: kind.into(),
                name: name.into(),
                namespace: "memory".into(),
                language: "vbnet".into(),
                file_path: engram_core::RelPath::new(path),
                start_line: start,
                end_line: end,
                generation: 1,
                metadata: if id == "file" {
                    Some(json!({"file_hash":blake3::hash(vb.as_bytes()).to_hex().to_string()}))
                } else {
                    None
                },
            });
        }
        let edges = [
            (format!("file:{path}"), "class"),
            ("class".into(), "post"),
            ("class".into(), "get"),
        ]
        .into_iter()
        .map(|(source, target)| engram_graph::Edge {
            source_id: source,
            target_id: target.into(),
            namespace: "memory".into(),
            language: "vbnet".into(),
            edge_kind: engram_graph::EdgeKind::Contains,
            weight: 1,
            generation: 1,
            metadata: None,
            updated_at_ms: 1,
        })
        .collect::<Vec<_>>();
        graph
            .upsert_nodes_and_edges("fixture", &nodes, &edges)
            .unwrap();
        let selected = BTreeMap::from([(path.to_string(), 1)]);
        let resolved = routing::resolve("POST /api/inventory/9", &sources).unwrap();
        assert_eq!(resolved.api_candidates.len(), 1);
        assert!(resolved.candidates.is_empty());
        let post = trace(
            &graph,
            "fixture",
            &resolved.api_candidates[0],
            Some("POST"),
            None,
            false,
            false,
            &sources,
            &selected,
        )
        .unwrap();
        let controller = &post["controllers"][0];
        assert_eq!(controller["handlers"][0]["name"], "Save");
        assert_eq!(controller["handlers"].as_array().unwrap().len(), 1);
        assert!(
            controller["source_members"][0]["excerpt"]
                .as_str()
                .unwrap()
                .contains("Response.Write(\"saved\")")
        );
        assert_eq!(controller["source_members"][0]["line_start"], 3);
        let get = trace(
            &graph,
            "fixture",
            &resolved.api_candidates[0],
            Some("GET"),
            None,
            false,
            false,
            &sources,
            &selected,
        )
        .unwrap();
        assert_eq!(get["controllers"][0]["handlers"][0]["name"], "Read");
        assert_eq!(
            get["controllers"][0]["handlers"].as_array().unwrap().len(),
            1
        );
        drop(graph);
        temp.close().unwrap();
    }
}
