//! Admit compiler-selected ordinary source locators before any graph mutation. No name/arity
//! resolver may turn an unverified locator into an asserted member binding.
use anyhow::{Result, ensure};
use engram_core::ids::NodeId;
use engram_graph::{
    Node,
    store::{SourceCallBinding, SourceCallBindings, SourceMemberLocator},
};
use engram_index::IngestStats;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

use super::interface_bindings::PreparedBinding;

struct Request {
    index: usize,
    source_path: String,
    source_name: String,
    source_line: u32,
    locator: Option<SourceMemberLocator>,
    reference_line: Option<u32>,
}

pub(crate) async fn prepare(
    state: &crate::state::AppState,
    project: &str,
    stats: &IngestStats,
    nodes: Vec<Node>,
) -> Result<(
    Vec<Node>,
    HashMap<usize, PreparedBinding>,
    Option<engram_core::memory::AllocationGuard>,
)> {
    // Ordinary source calls are much denser than interface dispatch. Allow a
    // 1K-file cohort at the native 256-locator/file cap, charging actual bytes
    // before deserializing or constructing admission indexes. Limits fail the
    // publication rather than silently reverting to name/arity resolution.
    let mut count = 0usize;
    let mut estimated = 0usize;
    for (path, edge) in &stats.edges {
        let Some(metadata) = &edge.metadata else {
            continue;
        };
        let payload = metadata.get("source_call_target_v1");
        if payload.is_none() && !metadata.contains_key("source_call_status") {
            continue;
        }
        count += 1;
        ensure!(
            path.as_str().len() <= 2048
                && edge.source_name.len() <= 2048
                && payload.is_none_or(|payload| payload.len() <= 24 * 1024),
            "Source call source identity byte limit"
        );
        estimated = estimated.saturating_add(
            payload.map_or(0, String::len) + path.as_str().len() + edge.source_name.len() + 512,
        );
        ensure!(
            count <= 262144 && estimated <= 128 * 1024 * 1024,
            "Source call binding admission work limit"
        );
    }
    if count == 0 {
        return Ok((nodes, HashMap::new(), None));
    }
    let reservation = engram_core::memory::AllocationGuard::try_new(
        state.memory_budget.as_ref(),
        (estimated as u64)
            .saturating_mul(4)
            .saturating_add((nodes.len() as u64).saturating_mul(128))
            .saturating_add(16 * 1024 * 1024),
        engram_core::memory::Subsystem::Graph,
        "exact source call bindings",
    )?;
    let mut requests = Vec::new();
    let mut bytes = 0usize;
    for (index, (path, edge)) in stats.edges.iter().enumerate() {
        let Some(metadata) = &edge.metadata else {
            continue;
        };
        let payload = metadata.get("source_call_target_v1");
        if payload.is_none() && !metadata.contains_key("source_call_status") {
            continue;
        }
        ensure!(
            edge.source_kind == "function"
                && edge.target_kind.as_deref() == Some("function")
                && edge.source_language.eq_ignore_ascii_case("vb")
                && edge.kind == "calls"
                && !metadata.contains_key("interface_call_status")
                && !metadata.contains_key("interface_call_target_v1")
                && !metadata.contains_key("interface_member_mapping_v1"),
            "Invalid source_call edge role"
        );
        ensure!(
            metadata
                .get("source_call_status")
                .is_none_or(|status| matches!(status.as_str(), "source_located" | "unknown")),
            "Invalid source call status"
        );
        let locator = payload
            .map(|value| -> Result<_> {
                ensure!(
                    value.len() <= 24 * 1024,
                    "Source call locator exceeds byte limit"
                );
                let locator: SourceMemberLocator = serde_json::from_str(value)?;
                ensure!(
                    locator.valid()
                        && locator.name == edge.target_name
                        && edge
                            .target_start_line
                            .is_none_or(|line| line == locator.start_line),
                    "Invalid source_call declaration locator"
                );
                Ok(locator)
            })
            .transpose()?
            .filter(|_| {
                metadata
                    .get("source_call_status")
                    .is_none_or(|status| status == "source_located")
            });
        bytes = bytes.saturating_add(
            payload.map_or(0, String::len) + path.as_str().len() + edge.source_name.len(),
        );
        ensure!(
            requests.len() < 262144 && bytes <= 128 * 1024 * 1024,
            "Source call binding admission work limit"
        );
        requests.push(Request {
            index,
            source_path: path.as_str().into(),
            source_name: edge.source_name.clone(),
            source_line: edge.source_start_line,
            locator,
            reference_line: metadata
                .get("call_site_line")
                .and_then(|line| line.parse().ok()),
        });
    }
    let cohort: HashSet<_> = stats
        .all_files
        .iter()
        .map(|p| p.as_str().to_owned())
        .collect();
    let needs_inherited = requests.iter().any(|request| {
        request
            .locator
            .as_ref()
            .is_some_and(|l| !cohort.contains(&l.file_path))
    });
    let mut source = None;
    let membership = if needs_inherited {
        let runtime = state
            .get_project_cached(project)
            .ok_or_else(|| anyhow::anyhow!("Source call source selection unavailable"))?;
        let old = super::project_service::get_active_generation(state, project).await?;
        let view = std::sync::Arc::new(super::project_service::pin_source_generation(
            state,
            &runtime.search,
            project,
            old,
        )?);
        let lookup = runtime
            .search
            .selected_source_revision_lookup(project, old, view.clone())?;
        source = Some(view);
        Some(lookup)
    } else {
        None
    };
    let graph = state.graph.clone();
    let project = project.to_owned();
    tokio::task::spawn_blocking(move || -> Result<_> {
        let _held_source = source;
        let mut nodes = nodes;
        let by_id: HashMap<_, _> = nodes
            .iter()
            .enumerate()
            .map(|(index, node)| (node.node_id.as_str(), index))
            .collect();
        let mut by_source: HashMap<_, Vec<usize>> = HashMap::new();
        for (index, node) in nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| node.node_type == "function")
        {
            by_source
                .entry((node.file_path.as_str(), node.name.as_str(), node.start_line))
                .or_default()
                .push(index);
        }
        let mut inherited = HashMap::new();
        let mut inherited_bytes = 0usize;
        let mut result = HashMap::new();
        let mut unknown = HashSet::new();
        for request in requests {
            let candidates = by_source.get(&(
                request.source_path.as_str(),
                request.source_name.as_str(),
                request.source_line,
            ));
            let source_index = candidates
                .filter(|rows| rows.len() == 1)
                .and_then(|rows| rows.first())
                .copied()
                .ok_or_else(|| {
                    anyhow::anyhow!("Source call source declaration is missing or ambiguous")
                })?;
            let producer = &nodes[source_index];
            let source_file = &nodes[*by_id
                .get(NodeId::file(&request.source_path).0.as_str())
                .ok_or_else(|| anyhow::anyhow!("Source call producer file unavailable"))?];
            let mut prepared = PreparedBinding {
                source_id: producer.node_id.clone(),
                target_id: format!(
                    "unverified-source-call:{}:{}:{}",
                    request.source_path, request.source_line, request.index
                ),
                evidence: None,
            };
            if let Some(locator) = request.locator {
                let target_id = NodeId::symbol(
                    "function",
                    None,
                    &locator.file_path,
                    &locator.name,
                    locator.start_line,
                )
                .0;
                let target_file_id = NodeId::file(&locator.file_path).0;
                let inherited_target = !cohort.contains(&locator.file_path);
                if inherited_target {
                    for id in [&target_id, &target_file_id] {
                        if !inherited.contains_key(id) {
                            ensure!(
                                inherited.len() < 8192,
                                "Inherited source_call endpoint limit"
                            );
                            ensure!(
                                inherited_bytes < 8 * 1024 * 1024,
                                "Inherited source_call record limit"
                            );
                            let charged = graph.get_node_bounded(
                                &project,
                                id,
                                (8 * 1024 * 1024 - inherited_bytes).min(256 * 1024),
                            )?;
                            let node = charged.map(|(node, bytes)| {
                                inherited_bytes += bytes;
                                node
                            });
                            inherited.insert(id.clone(), node);
                        }
                    }
                }
                let node = |id: &String| {
                    by_id
                        .get(id.as_str())
                        .map(|index| &nodes[*index])
                        .or_else(|| inherited.get(id).and_then(Option::as_ref))
                };
                if let (Some(target), Some(target_file)) = (node(&target_id), node(&target_file_id))
                {
                    let field = |node: &Node, key: &str| {
                        node.metadata.as_ref().and_then(|m| m.get(key)).cloned()
                    };
                    let parser = field(source_file, "vb_parser_provenance").unwrap_or(Value::Null);
                    let parser_verified = serde_json::from_value::<
                        engram_index::vb_extractor::VbParserProvenance,
                    >(parser.clone())
                    .is_ok_and(|p| {
                        matches!(
                            p,
                            engram_index::vb_extractor::VbParserProvenance::Roslyn { .. }
                        ) && p.is_verified()
                    });
                    let source_id = field(producer, "semantic_member_id")
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .unwrap_or_default();
                    let binding = SourceCallBinding {
                        target: locator,
                        source_semantic_id: source_id.clone(),
                        source_generation: producer.generation,
                        source_file_hash: field(source_file, "file_hash")
                            .and_then(|v| v.as_str().map(str::to_owned))
                            .unwrap_or_default(),
                        target_generation: target.generation,
                        target_file_hash: field(target_file, "file_hash")
                            .and_then(|v| v.as_str().map(str::to_owned))
                            .unwrap_or_default(),
                        parser,
                        reference_lines: request
                            .reference_line
                            .filter(|line| {
                                *line >= producer.start_line && *line <= producer.end_line
                            })
                            .into_iter()
                            .collect(),
                        reference_lines_truncated: false,
                    };
                    let selected = !inherited_target
                        || membership.as_ref().is_some_and(|lookup| {
                            super::impact_walk_service::admit_selected_source(
                                lookup,
                                target,
                                Some(target_file),
                            )
                            .is_ok_and(|a| a == engram_graph::store::ImpactAdmission::Selected)
                        });
                    if parser_verified
                        && selected
                        && binding.matches(producer, source_file, target, target_file)
                    {
                        prepared.target_id = target.node_id.clone();
                        prepared.evidence = Some(serde_json::to_value(SourceCallBindings {
                            version: 1,
                            complete: true,
                            bindings: vec![binding],
                        })?);
                    }
                }
            }
            if prepared.evidence.is_none() {
                unknown.insert(source_index);
            }
            result.insert(request.index, prepared);
        }
        for index in unknown {
            let meta = nodes[index]
                .metadata
                .get_or_insert_with(|| json!({}))
                .as_object_mut()
                .expect("source metadata object");
            meta.insert("source_call_admission_status".into(), json!("unknown"));
        }
        Ok((nodes, result, Some(reservation)))
    })
    .await?
}
