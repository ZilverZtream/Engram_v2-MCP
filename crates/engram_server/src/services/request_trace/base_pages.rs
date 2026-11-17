//! Resolved project-base source context does not establish event execution.
use super::{execution, routing::Source};
use engram_graph::{EdgeKind, GraphStore, Node};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
};

#[derive(Serialize)]
pub struct Link {
    pub derived_type: String,
    pub file: String,
    pub line: u32,
    pub source_blake3: String,
    pub base_type: String,
    pub target_file: String,
    pub target_line: u32,
    pub target_source_blake3: String,
}
#[derive(Serialize)]
pub struct Resolution {
    pub links: Vec<Link>,
    pub gaps: Vec<String>,
    pub truncated: bool,
    pub interpretation: &'static str,
}

fn current_type<'a>(
    graph: &GraphStore,
    project: &str,
    node: &Node,
    sources: &[Source<'a>],
    selected: &BTreeMap<String, u64>,
) -> anyhow::Result<Option<(Source<'a>, String)>> {
    let Some(source) = sources
        .iter()
        .find(|source| source.path == node.file_path.as_str())
        .copied()
    else {
        return Ok(None);
    };
    if node.node_type != "class"
        || node.namespace != "memory"
        || !execution::verified_node(graph, project, node, sources, selected)?
    {
        return Ok(None);
    }
    let owner = crate::services::business_logic_service::declaring_owner(
        source.content,
        "vb",
        node.start_line,
    );
    if !owner.eq_ignore_ascii_case(&node.name) {
        return Ok(None);
    }
    Ok(Some((source, owner)))
}

pub(super) fn collect<'a>(
    graph: &Arc<GraphStore>,
    project: &str,
    page: Source<'a>,
    code: Source<'a>,
    sources: &[Source<'a>],
    selected: &BTreeMap<String, u64>,
) -> anyhow::Result<(Option<Resolution>, Vec<(Node, String)>)> {
    if !code.path.to_ascii_lowercase().ends_with(".vb") {
        return Ok((None, vec![]));
    }
    let mut result = Resolution {
        links: vec![],
        gaps: vec![],
        truncated: false,
        interpretation: "Root page project-base context from existing resolved inheritance edges and current selected source declarations. At most4 base types and256 methods per base file. Authored PreInit/Init/Load subscriptions are possible source context, not compiler binding, guaranteed base-handler execution, lifecycle order, authorization or the effective master. Conditional assignments remain unexecuted observations. Overrides, master/control inheritance, external frameworks, root namespace/import binding and unresolved targets remain unexamined.",
    };
    let markup = execution::without_markup_comments(page.content);
    let declared = execution::attribute(&markup, "Inherits");
    let mut roots = graph.query_nodes_in_file(project, Some("class"), code.path, 17)?;
    if roots.len() > 16 {
        roots.truncate(16);
        result.truncated = true;
        result
            .gaps
            .push("Page class inventory capped at16; additional classes unexamined".into());
    }
    let mut matching = Vec::new();
    if super::control_routes::admitted(graph, project, &page, selected)? {
        for node in roots {
            if let Some((_, owner)) = current_type(graph, project, &node, sources, selected)? {
                if declared.as_deref().is_some_and(|name| {
                    owner.eq_ignore_ascii_case(name)
                        || (!name.contains('.')
                            && owner
                                .rsplit('.')
                                .next()
                                .is_some_and(|part| part.eq_ignore_ascii_case(name)))
                }) {
                    matching.push(node);
                }
            }
        }
    }
    if matching.len() != 1 {
        // No inferred class binding from a first/basename match or stale markup.
        result.gaps.push(format!("Root Page.Inherits class has {} admitted declaration matches; project-base context withheld",matching.len()));
        return Ok((Some(result), vec![]));
    }
    let mut queue = VecDeque::from([(matching.remove(0), 0usize)]);
    let mut visited = BTreeSet::new();
    let mut chosen = Vec::new();
    while let Some((node, depth)) = queue.pop_front() {
        if !visited.insert(node.node_id.clone()) {
            result
                .gaps
                .push("Repeated project-base identity; further inheritance unexamined".into());
            continue;
        }
        let Some((source, owner)) = current_type(graph, project, &node, sources, selected)? else {
            result.gaps.push(format!(
                "{}: selected class/source declaration unavailable",
                node.node_id
            ));
            continue;
        };
        let (edges, capped) =
            graph.outgoing_edges_of_kind(project, EdgeKind::InheritsFrom, &node.node_id, 4)?;
        if capped {
            result.truncated = true;
            result.gaps.push(format!(
                "{}: inheritance edge inventory capped",
                node.node_id
            ));
        }
        if edges.len() > 1 {
            result.gaps.push(format!(
                "{}: multiple base edges are ambiguous; further context withheld",
                node.node_id
            ));
            continue;
        }
        for edge in edges {
            if result.links.len() == 4 || depth == 4 {
                result.truncated = true;
                result.gaps.push(
                    "Project-base context capped at4 types; remaining inheritance unexamined"
                        .into(),
                );
                break;
            }
            if edge.namespace != node.namespace || edge.generation != node.generation {
                result.gaps.push(format!(
                    "{}: inheritance edge revision differs from selected class",
                    node.node_id
                ));
                continue;
            }
            let Some(base) = graph.get_node(project, &edge.target_id)? else {
                result.gaps.push(format!(
                    "{}: base target unresolved; no name-based binding inferred",
                    edge.target_id
                ));
                continue;
            };
            let Some((base_source, base_owner)) =
                current_type(graph, project, &base, sources, selected)?
            else {
                result.gaps.push(format!(
                    "{}: base class source/revision/declaration not admitted",
                    base.node_id
                ));
                continue;
            };
            result.links.push(Link {
                derived_type: owner.clone(),
                file: source.path.into(),
                line: node.start_line,
                source_blake3: blake3::hash(source.content.as_bytes()).to_hex().to_string(),
                base_type: base_owner.clone(),
                target_file: base_source.path.into(),
                target_line: base.start_line,
                target_source_blake3: blake3::hash(base_source.content.as_bytes())
                    .to_hex()
                    .to_string(),
            });
            let mut methods =
                graph.query_nodes_in_file(project, Some("function"), base_source.path, 257)?;
            if methods.len() > 256 {
                methods.truncate(256);
                result.truncated = true;
                result.gaps.push(format!(
                    "{}: base method inventory capped at256",
                    base_source.path
                ));
            }
            methods.retain(|method| {
                crate::services::business_logic_service::declaring_owner(
                    base_source.content,
                    "vb",
                    method.start_line,
                )
                .eq_ignore_ascii_case(&base_owner)
            });
            for (method, binding) in execution::handler_bindings(
                "",
                base_source.content,
                &methods,
                Some("load page"),
                None,
            ) {
                chosen.push((method,format!("project base source context {base_owner}: {binding}; execution and effective master unproven")));
            }
            queue.push_back((base, depth + 1));
        }
    }
    Ok((Some(result), chosen))
}
