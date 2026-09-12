//! Inspection leads from unresolved indexed names, never bound caller edges.
use engram_graph::{EdgeKind, GraphStore, Node};
use serde::Serialize;
use std::collections::HashSet;

use super::access_layer_tools::{CallerLocation, fqn_from_node};
use super::caller_excerpts::{CallerExcerpt, collect};

#[derive(Debug, Clone, Serialize)]
pub struct UnresolvedCallerLeads {
    pub status: String,
    pub detail: String,
    pub truncated: bool,
    pub excerpts: Vec<CallerExcerpt>,
}

fn names(fqn: &str) -> Vec<String> {
    let mut name = fqn;
    let mut result = Vec::new();
    loop {
        result.push(format!("::{name}"));
        if result.len() == 8 {
            break;
        }
        let Some((_, suffix)) = name.split_once('.') else {
            break;
        };
        name = suffix;
    }
    result
}

pub fn lookup(
    graph: &GraphStore,
    project: &str,
    root: &str,
    target: &Node,
    confirmed: &[CallerLocation],
    limit: usize,
    budget: &mut usize,
) -> UnresolvedCallerLeads {
    let mut result = UnresolvedCallerLeads {
        status: "inspection_leads".into(),
        detail: "Unresolved indexed name references only: overloads, receivers, comments or strings may match. These are not confirmed callers and do not affect caller counts or blast scores. Lookup covers at most 8 exact-spelling name suffixes and the first 100 references per suffix in source-id order, with one lookahead for truncation; other spellings, semantic placeholders and unindexed calls may be absent. Use source search and type-aware inspection to establish consumers.".into(),
        truncated: false, excerpts: Vec::new(),
    };
    let limit = limit.min(20);
    if limit == 0 {
        result.status = "omitted".into();
        return result;
    }
    let scan = || -> anyhow::Result<(Vec<CallerLocation>, bool)> {
        let mut found = Vec::new();
        let mut seen = HashSet::new();
        let mut truncated = false;
        for placeholder in names(&fqn_from_node(target)) {
            // Existing nodes and resolved endpoints are not unresolved leads.
            if graph.get_node(project, &placeholder)?.is_some() {
                continue;
            }
            let (incoming, capped) =
                graph.incoming_edge_prefix(project, EdgeKind::Calls, &placeholder, 100)?;
            truncated |= capped;
            let endpoints: Vec<_> = incoming
                .into_iter()
                .map(|(source, kind, _)| (source, kind, placeholder.clone()))
                .collect();
            for edge in graph.get_edges_by_endpoints(project, &endpoints)? {
                let Some(source) = graph.get_node(project, &edge.source_id)? else {
                    continue;
                };
                if source.node_type != "function" || source.generation > edge.generation {
                    continue;
                }
                let fqn = fqn_from_node(&source);
                if confirmed.iter().any(|c| {
                    c.fqn == fqn
                        && c.file_path == source.file_path.as_str()
                        && c.line == source.start_line
                }) || !seen.insert(source.node_id.clone())
                {
                    continue;
                }
                found.push(CallerLocation {
                    fqn,
                    file_path: source.file_path.to_string(),
                    line_kind: "declaration",
                    line: source.start_line,
                    line_end: source.end_line,
                    edge_kind: "unresolved_name_reference".into(),
                });
            }
        }
        found.sort_by(|a, b| (&a.file_path, a.line).cmp(&(&b.file_path, b.line)));
        truncated |= found.len() > limit;
        found.truncate(limit);
        Ok((found, truncated))
    };
    match scan() {
        Err(error) => {
            result.status = "failed".into();
            result.detail = format!(
                "Unresolved caller lookup failed: {error}. Inspect source references directly."
            );
        }
        Ok((leads, truncated)) => {
            result.truncated = truncated;
            for lead in leads {
                let mut excerpt = collect(graph, project, root, &lead, &target.name, budget);
                excerpt.detail = format!(
                    "Unresolved name lead; target overload and receiver are not established. {}",
                    excerpt.detail
                );
                result.excerpts.push(excerpt);
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn suffix_lookup_is_bounded_and_keeps_qualification() {
        assert_eq!(
            names("Reports.Value.Format"),
            ["::Reports.Value.Format", "::Value.Format", "::Format"]
        );
        assert_eq!(names("a.b.c.d.e.f.g.h.i.j").len(), 8);
    }

    fn fixture() -> (tempfile::TempDir, GraphStore, Node) {
        let temp = tempfile::tempdir().unwrap();
        let graph = GraphStore::open(&temp.path().join("graph")).unwrap();
        let source = "Public Sub Export()\n    Reports.Value.Format(1)\nEnd Sub\n";
        std::fs::write(temp.path().join("Report.vb"), source).unwrap();
        let target = Node {
            node_id: "target".into(),
            node_type: "function".into(),
            name: "Reports.Value.Format".into(),
            namespace: "memory".into(),
            language: "vb".into(),
            file_path: engram_core::RelPath::new("Value.vb"),
            start_line: 1,
            end_line: 3,
            generation: 1,
            metadata: None,
        };
        let caller = Node {
            node_id: "caller".into(),
            name: "Reports.Export".into(),
            file_path: engram_core::RelPath::new("Report.vb"),
            ..target.clone()
        };
        let file = Node {
            node_id: "file:Report.vb".into(),
            node_type: "file".into(),
            metadata: Some(
                serde_json::json!({"file_hash":blake3::hash(source.as_bytes()).to_hex().to_string()}),
            ),
            ..caller.clone()
        };
        graph
            .upsert_nodes("p", &[target.clone(), caller, file])
            .unwrap();
        graph
            .upsert_edges(
                "p",
                &[engram_graph::Edge {
                    source_id: "caller".into(),
                    target_id: "::Reports.Value.Format".into(),
                    namespace: "memory".into(),
                    language: "vb".into(),
                    edge_kind: EdgeKind::Calls,
                    weight: 1,
                    generation: 1,
                    metadata: None,
                    updated_at_ms: 0,
                }],
            )
            .unwrap();
        (temp, graph, target)
    }

    #[test]
    fn unresolved_reference_is_an_inspection_lead_with_verified_source() {
        let (temp, graph, target) = fixture();
        let result = lookup(
            &graph,
            "p",
            temp.path().to_str().unwrap(),
            &target,
            &[],
            5,
            &mut 65536,
        );
        assert_eq!(result.excerpts.len(), 1);
        assert_eq!(result.excerpts[0].status, "verified_source");
        assert!(
            result.excerpts[0]
                .numbered_source
                .contains("2:     Reports.Value.Format(1)")
        );
        assert!(result.detail.contains("not confirmed callers"));
        assert!(!result.truncated);
        // Graph binding remains absent; the helper only reads evidence.
        assert!(
            graph
                .find_incoming_edges_with_kind("p", Some(EdgeKind::Calls), "target", 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn old_edges_and_already_confirmed_callers_are_excluded() {
        let (temp, graph, target) = fixture();
        let confirmed = CallerLocation {
            fqn: "Reports.Export".into(),
            file_path: "Report.vb".into(),
            line_kind: "declaration",
            line: 1,
            line_end: 3,
            edge_kind: "calls".into(),
        };
        assert!(
            lookup(
                &graph,
                "p",
                temp.path().to_str().unwrap(),
                &target,
                &[confirmed],
                5,
                &mut 65536
            )
            .excerpts
            .is_empty()
        );
        let mut caller = graph.get_node("p", "caller").unwrap().unwrap();
        caller.generation = 2;
        graph.upsert_nodes("p", &[caller]).unwrap();
        assert!(
            lookup(
                &graph,
                "p",
                temp.path().to_str().unwrap(),
                &target,
                &[],
                5,
                &mut 65536
            )
            .excerpts
            .is_empty()
        );
    }

    #[test]
    fn stale_source_is_withheld_and_zero_limit_omits_lookup() {
        let (temp, graph, target) = fixture();
        std::fs::write(temp.path().join("Report.vb"), "modified\n").unwrap();
        let result = lookup(
            &graph,
            "p",
            temp.path().to_str().unwrap(),
            &target,
            &[],
            5,
            &mut 65536,
        );
        assert_eq!(result.excerpts[0].status, "withheld");
        assert!(result.excerpts[0].numbered_source.is_empty());
        let omitted = lookup(
            &graph,
            "p",
            temp.path().to_str().unwrap(),
            &target,
            &[],
            0,
            &mut 65536,
        );
        assert_eq!(omitted.status, "omitted");
        assert!(omitted.excerpts.is_empty());
    }

    #[test]
    fn output_limit_is_reported_and_repeated_names_deduplicate() {
        let (temp, graph, target) = fixture();
        let caller = graph.get_node("p", "caller").unwrap().unwrap();
        let second = Node {
            node_id: "second".into(),
            name: "Reports.OtherExport".into(),
            ..caller
        };
        graph.upsert_nodes("p", &[second]).unwrap();
        let edge = engram_graph::Edge {
            source_id: "caller".into(),
            target_id: "::Value.Format".into(),
            namespace: "memory".into(),
            language: "vb".into(),
            edge_kind: EdgeKind::Calls,
            weight: 1,
            generation: 1,
            metadata: None,
            updated_at_ms: 0,
        };
        graph
            .upsert_edges(
                "p",
                &[
                    edge.clone(),
                    engram_graph::Edge {
                        source_id: "second".into(),
                        ..edge
                    },
                ],
            )
            .unwrap();
        let all = lookup(
            &graph,
            "p",
            temp.path().to_str().unwrap(),
            &target,
            &[],
            5,
            &mut 65536,
        );
        assert_eq!(all.excerpts.len(), 2);
        let capped = lookup(
            &graph,
            "p",
            temp.path().to_str().unwrap(),
            &target,
            &[],
            1,
            &mut 65536,
        );
        assert_eq!(capped.excerpts.len(), 1);
        assert!(capped.truncated);
    }
}
