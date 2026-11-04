//! Source-revision admission and bounded witnesses for incoming change exposure.
//! These are indexed relationships, never proof of execution or safe changes.
use crate::state::AppState;
use anyhow::{Context, Result, ensure};
use engram_graph::store::{ImpactAdmission, IncomingImpactWalk, ResolveResult};
use engram_index::SourceSnapshotGuard;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

const RESPONSE_BYTES: usize = 64 * 1024;

fn section(value: &Value) -> String {
    format!(
        "\n## Revision-admitted change impact\n\nIndexed causal paths under the named source selection; not a proposed-change simulation, runtime proof or safe-change verdict.\n\n```json\n{}\n```\n",
        serde_json::to_string_pretty(value).expect("JSON value serialization")
    )
}

fn render(
    walk: &IncomingImpactWalk,
    target: &str,
    generation: u64,
    max_depth: usize,
    display_limit: usize,
    managed: bool,
) -> Result<Value> {
    let lookup: HashMap<_, _> = walk
        .nodes
        .iter()
        .map(|node| (node.node.node_id.as_str(), node))
        .collect();
    let mut candidates: Vec<_> = walk
        .nodes
        .iter()
        .filter(|node| node.admission == ImpactAdmission::Selected && node.depth > 0)
        .collect();
    candidates.sort_by(|a, b| {
        b.depth
            .cmp(&a.depth)
            .then_with(|| a.node.node_id.cmp(&b.node.node_id))
    });
    let total = candidates.len();
    let mut dependents = Vec::new();
    let mut nodes: BTreeMap<String, Value> = BTreeMap::new();
    let mut witnesses: BTreeMap<String, Value> = BTreeMap::new();
    let mut value = json!({"source_generation":generation,"target":target,"requested_max_depth":max_depth,
        "source_layout":if managed {"managed"} else {"attested_legacy"},
        "coverage":walk.coverage,"dependents":[],"nodes":[],"witnesses":[],"omitted_from_display":total,
        "status":"bounded_indexed_exposure","display_truncated":false,"selected_dependents_observed":total,
        "working_tree_checked":false,"exhaustive_codebase":false,"change_breakage_proven":false,
        "display_order":"deepest_paths_first_then_node_id; every displayed path includes its direct intermediates",
        "limitations":["Paths are admitted by indexed node/file revisions and extraction generation or verified unchanged-caller origin, not by compiler or runtime validation.",
            "The current working tree was not fingerprinted. Check index freshness and method edit context before editing.",
            "Dynamic, missing, unsupported and uninspected bindings remain unknown; an empty list is not proof of no impact.",
            "Historical companions and structural containment never create causal paths. Depth/work/display limits may omit further exposure.",
            "For an omitted branch, rerun this tool on its exact node_id with a smaller max_depth."]});
    for dependent in candidates.into_iter().take(display_limit.clamp(1, 25)) {
        let mut path = Vec::new();
        let mut cursor = Some(dependent);
        for _ in 0..=8 {
            let Some(node) = cursor else {
                break;
            };
            path.push(node.node.node_id.clone());
            cursor = node
                .predecessor
                .as_deref()
                .map(|id| {
                    lookup
                        .get(id)
                        .copied()
                        .context("Walk witness predecessor unavailable")
                })
                .transpose()?;
        }
        ensure!(
            cursor.is_none(),
            "Walk witness contains a cycle or exceeds the depth bound"
        );
        path.reverse();
        let mut next_nodes = nodes.clone();
        let mut next_witnesses = witnesses.clone();
        for id in &path {
            let node = lookup
                .get(id.as_str())
                .context("Walk witness source unavailable")?;
            next_nodes.insert(id.clone(), json!({"node_id":id,"name":node.node.name,
                "file_path":node.node.file_path,"start_line":node.node.start_line,"end_line":node.node.end_line,
                "generation":node.node.generation,"kind":node.node.node_type}));
            if let Some(predecessor) = &node.predecessor {
                next_witnesses.insert(id.clone(), json!({"source_id":id,"target_id":predecessor,
                    "kind":node.via_kind.as_ref().map(|kind| kind.as_str()),"edge_generation":node.edge_generation,
                    "source_generation":node.node.generation,
                    "edge_source_binding":if node.edge_generation == Some(node.node.generation) {
                        "extraction_generation"
                    } else { "verified_unchanged_caller_origin" }}));
            }
        }
        let mut next_dependents = dependents.clone();
        let seed = path.first().context("Walk witness has no source origin")?;
        let seed_relationship = if seed == target {
            "requested_target"
        } else {
            "contained_declaration_in_requested_file; structural_origin_not_a_causal_edge"
        };
        next_dependents.push(
            json!({"node_id":dependent.node.node_id,"file_path":dependent.node.file_path,
            "start_line":dependent.node.start_line,"generation":dependent.node.generation,
            "depth":dependent.depth,"seed_node_id":seed,"seed_relationship":seed_relationship,"path":path}),
        );
        value["dependents"] = json!(next_dependents);
        value["nodes"] = json!(next_nodes.values().collect::<Vec<_>>());
        value["witnesses"] = json!(next_witnesses.values().collect::<Vec<_>>());
        if section(&value).len() > RESPONSE_BYTES {
            continue;
        }
        dependents = next_dependents;
        nodes = next_nodes;
        witnesses = next_witnesses;
    }
    value["dependents"] = json!(dependents);
    value["nodes"] = json!(nodes.into_values().collect::<Vec<_>>());
    value["witnesses"] = json!(witnesses.into_values().collect::<Vec<_>>());
    value["omitted_from_display"] = json!(total - dependents.len());
    value["display_truncated"] = json!(total != dependents.len());
    value["selected_dependents_observed"] = json!(total);
    ensure!(
        section(&value).len() <= RESPONSE_BYTES,
        "Impact evidence exceeds response budget"
    );
    Ok(value)
}

/// Retain the same source capability through graph collection, rendering and
/// caller response assembly. Errors disclose unavailable evidence separately
/// from the older, unverified live-graph inventory.
pub async fn collect(
    state: &AppState,
    project: &str,
    file_path: Option<String>,
    symbol_fqn: Option<String>,
    max_depth: usize,
    display_limit: usize,
) -> (String, Option<Arc<SourceSnapshotGuard>>, Option<u64>) {
    let result: Result<(Value, Arc<SourceSnapshotGuard>)> = async {
        ensure!((1..=8).contains(&max_depth), "max_depth must be 1..8");
        let runtime = super::project_service::ensure_project_runtime(state, project).await?;
        let generation = super::project_service::get_active_generation(state, project).await?;
        let source = Arc::new(super::project_service::pin_source_generation(
            state,
            &runtime.search,
            project,
            generation,
        )?);
        let membership =
            runtime
                .search
                .selected_source_revision_lookup(project, generation, source.clone())?;
        let graph = state.graph.clone();
        let project = project.to_owned();
        tokio::task::spawn_blocking(move || -> Result<_> {
            let target = if let Some(path) = file_path {
                format!("file:{path}")
            } else {
                let symbol = symbol_fqn.context("Impact target missing")?;
                if graph.get_node(&project, &symbol)?.is_some() {
                    symbol
                } else {
                    match graph.resolve_symbol(&project, &symbol, None, None)? {
                        ResolveResult::Unique(node) => node.node_id,
                        ResolveResult::Ambiguous(_) => {
                            anyhow::bail!("Impact target is ambiguous; select an exact node_id")
                        }
                        ResolveResult::NotFound => anyhow::bail!(
                            "Impact target is absent; this does not establish no impact"
                        ),
                    }
                }
            };
            let root = graph
                .get_node(&project, &target)?
                .context("Impact target unavailable")?;
            let mut seeds = vec![target.clone()];
            let mut seed_cap = false;
            if root.node_type == "file" {
                let (members, capped) = graph.file_component_members(
                    &project,
                    root.file_path.as_str(),
                    &target,
                    127,
                )?;
                seeds.extend(members);
                seed_cap = capped;
            }
            let admit = |node: &engram_graph::Node,
                         file: Option<&engram_graph::Node>|
             -> Result<ImpactAdmission> {
                if node.namespace != engram_core::namespaces::NAMESPACE_MEMORY
                    || node.file_path.as_str().is_empty()
                {
                    return Ok(ImpactAdmission::Unknown);
                }
                match membership.matches(node.file_path.as_str(), node.generation)? {
                    Some(false) => return Ok(ImpactAdmission::NonSelectedRevision),
                    None => return Ok(ImpactAdmission::Unknown),
                    Some(true) => {}
                }
                let Some(file) = file else {
                    return Ok(ImpactAdmission::Unknown);
                };
                if file.node_type != "file"
                    || file.file_path != node.file_path
                    || file.namespace != engram_core::namespaces::NAMESPACE_MEMORY
                {
                    return Ok(ImpactAdmission::Unknown);
                }
                Ok(
                    match membership.matches(node.file_path.as_str(), file.generation)? {
                        Some(true) => ImpactAdmission::Selected,
                        Some(false) | None => ImpactAdmission::Unknown,
                    },
                )
            };
            let mut walk = graph.bounded_incoming_walk(
                &project,
                &seeds,
                max_depth,
                &super::blast_radius_service::is_causal_dependency,
                &admit,
            )?;
            if seed_cap {
                walk.coverage.truncated = true;
                walk.coverage
                    .reasons
                    .push("file_seed_cap; uninspected declarations may have other callers".into());
            }
            let value = render(
                &walk,
                &target,
                generation,
                max_depth,
                display_limit,
                source.is_managed(),
            )?;
            Ok((value, source))
        })
        .await
        .context("Impact evidence worker unavailable")?
    }
    .await;
    match result {
        Ok((value, source)) => {
            let generation = value["source_generation"].as_u64();
            (section(&value), Some(source), generation)
        }
        Err(error) => (
            section(
                &json!({"source_generation":null,"requested_max_depth":max_depth,
            "status":"unavailable","coverage":{"truncated":true,"reasons":["source_evidence_unavailable"]},
            "dependents":[],"nodes":[],"witnesses":[],"omitted_from_display":null,
            "error":format!("{error:#}").chars().take(2048).collect::<String>(),
            "exhaustive_codebase":false,"change_breakage_proven":false,
            "limitations":["Unavailable source evidence is not zero impact. Resolve the target and source snapshot before relying on current-source paths."]}),
            ),
            None,
            None,
        ),
    }
}
