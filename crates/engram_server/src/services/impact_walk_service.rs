//! Source-revision admission and bounded witnesses for incoming change exposure.
//! These are indexed relationships, never proof of execution or safe changes.
use crate::state::AppState;
use anyhow::{Context, Result, ensure};
use engram_graph::store::{ImpactAdmission, ImpactWalkCoverage, IncomingImpactWalk, ResolveResult};
use engram_index::SourceSnapshotGuard;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

const RESPONSE_BYTES: usize = 64 * 1024;

/// Apply one already-held source selection to both graph endpoints' physical
/// node/file revisions. This helper never pins or observes a newer HEAD.
pub(crate) fn admit_selected_source(
    membership: &engram_index::hybrid::SelectedSourceRevisionLookup,
    node: &engram_graph::Node,
    file: Option<&engram_graph::Node>,
) -> Result<ImpactAdmission> {
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
}

/// Indexed downstream observations for one exact target, never an edit-safety
/// guarantee. Counts describe the same bounded walk before display clipping.
/// None means unavailable, whereas Some(0) requires an admitted selected root.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ImpactAssessment {
    pub source_generation: Option<u64>,
    pub source_layout: String,
    pub requested_max_depth: usize,
    pub target_revision_admitted: bool,
    pub status: String,
    pub coverage: ImpactWalkCoverage,
    pub selected_dependents_observed: Option<usize>,
    pub direct_dependents_observed: Option<usize>,
    pub transitive_dependents_observed: Option<usize>,
    /// The existing bounded JSON witness/path/node payload, without Markdown.
    pub evidence: Value,
}

enum TargetRequest {
    Resolve {
        file_path: Option<String>,
        symbol_fqn: Option<String>,
    },
    Exact(String),
}

struct CollectedImpact {
    value: Value,
    source: Arc<SourceSnapshotGuard>,
    generation: u64,
    target_revision_admitted: bool,
    coverage: ImpactWalkCoverage,
    direct_dependents: usize,
    transitive_dependents: usize,
}

fn section(value: &Value) -> String {
    format!(
        "\n## Revision-admitted change impact\n\nIndexed causal paths under the named source selection; not a proposed-change simulation, runtime proof or safe-change verdict.\n\n```json\n{}\n```\n",
        serde_json::to_string(value).expect("JSON value serialization")
    )
}

struct Endpoint<'a> {
    id: &'a str,
    name: Option<&'a str>,
    kind: Option<&'a str>,
    file: Option<&'a str>,
    start: Option<u32>,
    end: Option<u32>,
    generation: Option<u64>,
    depth: Option<usize>,
    admission: &'static str,
    is_seed: bool,
}

fn node_endpoint(node: &engram_graph::store::ImpactWalkNode) -> Endpoint<'_> {
    Endpoint {
        id: &node.node.node_id,
        name: Some(&node.node.name),
        kind: Some(&node.node.node_type),
        file: Some(node.node.file_path.as_str()),
        start: Some(node.node.start_line),
        end: Some(node.node.end_line),
        generation: Some(node.node.generation),
        depth: Some(node.depth),
        admission: if node.admission == ImpactAdmission::Selected {
            "selected"
        } else {
            "unknown"
        },
        is_seed: node.depth == 0,
    }
}

fn recovery_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 2048
        && !path.contains(['\\', ':'])
        && !path.chars().any(char::is_control)
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

struct RecoveryDraft {
    reference: String,
    fresh: Option<(String, Value)>,
}

struct CompactEvidence<'a> {
    project: &'a str,
    remaining: usize,
    files: Vec<Value>,
    file_refs: BTreeMap<String, String>,
    nodes: Vec<Value>,
    node_indices: BTreeMap<String, usize>,
    edges: Vec<Value>,
    edge_refs: BTreeMap<(String, String, String), String>,
    recoveries: Vec<Value>,
    recovery_refs: BTreeMap<String, String>,
}

fn canonical_hint(id: &str) -> (Option<&str>, &str) {
    if let Some(rest) = id.strip_prefix("sym:") {
        let mut parts = rest.splitn(3, ':');
        let _kind = parts.next();
        if let (Some(file), Some(symbol)) = (parts.next(), parts.next()) {
            if let Some((name, line)) = symbol.rsplit_once(':') {
                if line.parse::<u32>().is_ok() && !name.is_empty() {
                    return (Some(file), name);
                }
            }
        }
    }
    (id.strip_prefix("file:"), id)
}

impl CompactEvidence<'_> {
    fn reference(&self, id: &str) -> Option<String> {
        self.node_indices.get(id).map(|index| format!("n{index}"))
    }

    fn recovery(&self, tool: &str, arguments: Value, scope_ref: &str) -> Result<RecoveryDraft> {
        let key = serde_json::to_string(&json!({"tool":tool,"arguments":arguments}))?;
        if let Some(reference) = self.recovery_refs.get(&key) {
            return Ok(RecoveryDraft {
                reference: reference.clone(),
                fresh: None,
            });
        }
        let reference = format!("r{}", self.recoveries.len());
        Ok(RecoveryDraft {
            reference: reference.clone(),
            fresh: Some((
                key,
                json!({"ref":reference,"tool":tool,"arguments":arguments,"scope_ref":scope_ref}),
            )),
        })
    }

    fn range_recovery(&self, file: &str, start: u32, end: u32) -> Result<RecoveryDraft> {
        self.recovery("get_full_method_body", json!({"project_id":self.project,
            "file_path":file,"line_start":start,"line_end":end,"context_lines":0,"output_json":true}),
            "range")
    }

    fn lookup_recovery(&self, endpoint: &Endpoint<'_>) -> Result<RecoveryDraft> {
        let (hint_file, _) = canonical_hint(endpoint.id);
        let file = endpoint
            .file
            .or(hint_file)
            .filter(|path| recovery_path(path));
        let mut arguments = json!({"project_id":self.project,"limit":1,"offset":0,
            "include_retained_context":false});
        if let Some(file) = file {
            arguments["scope"] = json!(file);
        }
        self.recovery("get_codebase_overview", arguments, "navigation")
    }

    fn commit_recovery(&mut self, draft: RecoveryDraft) {
        if let Some((key, value)) = draft.fresh {
            self.recovery_refs.insert(key, draft.reference);
            self.recoveries.push(value);
        }
    }

    fn charge(&mut self, values: &[&Value]) -> Result<bool> {
        // Conservative per-array comma charge; a fixed envelope reserve covers
        // final scalar counters. Each staged row is bounded by the graph's
        // retained-string budget; no unbounded body is materialized here.
        let bytes = values
            .iter()
            .try_fold(0usize, |total, value| -> Result<usize> {
                Ok(total
                    .checked_add(serde_json::to_vec(value)?.len() + 1)
                    .context("Impact response size overflow")?)
            })?;
        if bytes > self.remaining {
            return Ok(false);
        }
        self.remaining -= bytes;
        Ok(true)
    }

    fn endpoint(&mut self, endpoint: &Endpoint<'_>) -> Result<Option<String>> {
        if let Some(reference) = self.reference(endpoint.id) {
            return Ok(Some(reference));
        }
        let file = endpoint
            .file
            .or(canonical_hint(endpoint.id).0)
            .filter(|path| path.len() <= 2048);
        let file_ref = file.map(|path| {
            self.file_refs
                .get(path)
                .cloned()
                .unwrap_or_else(|| format!("f{}", self.files.len()))
        });
        let file_row = file
            .filter(|path| !self.file_refs.contains_key(*path))
            .map(|path| json!({"ref":file_ref,"path":path}));
        let recovery = match (endpoint.file, endpoint.start, endpoint.end) {
            (Some(file), Some(start), Some(end))
                if recovery_path(file) && start > 0 && end >= start =>
            {
                self.range_recovery(file, start, end)?
            }
            _ => self.lookup_recovery(endpoint)?,
        };
        let reference = format!("n{}", self.nodes.len());
        let name = endpoint
            .name
            .map(|name| &name[..name.floor_char_boundary(256)]);
        let row = json!({"ref":reference,"node_id":endpoint.id,"name":name,"kind":endpoint.kind,
            "name_truncated":endpoint.name.is_some_and(|name|name.len()>256),
            "admission":endpoint.admission,"is_seed":endpoint.is_seed,"depth":endpoint.depth,
            "file_ref":file_ref,"start_line":endpoint.start,"end_line":endpoint.end,
            "generation":endpoint.generation,"recovery_ref":recovery.reference});
        let mut staged = vec![&row];
        if let Some(file) = &file_row {
            staged.push(file);
        }
        if let Some((_, value)) = &recovery.fresh {
            staged.push(value);
        }
        if !self.charge(&staged)? {
            return Ok(None);
        }
        if let (Some(path), Some(file_row), Some(file_ref)) = (file, file_row, file_ref) {
            self.file_refs.insert(path.into(), file_ref);
            self.files.push(file_row);
        }
        self.commit_recovery(recovery);
        self.node_indices
            .insert(endpoint.id.into(), self.nodes.len());
        self.nodes.push(row);
        Ok(Some(reference))
    }

    fn lookup_boundary(&mut self, endpoint: &Endpoint<'_>) -> Result<Option<String>> {
        let recovery = if endpoint.admission == "selected" {
            self.recovery(
                "impact_analysis",
                json!({"project_id":self.project,
                "symbol_fqn":endpoint.id,"max_depth":1,"limit":8}),
                "direct",
            )?
        } else {
            self.lookup_recovery(endpoint)?
        };
        if let Some((_, value)) = &recovery.fresh {
            if !self.charge(&[value])? {
                return Ok(None);
            }
        }
        let reference = recovery.reference.clone();
        self.commit_recovery(recovery);
        Ok(Some(reference))
    }
}

fn render(
    walk: &IncomingImpactWalk,
    target: &str,
    project: &str,
    generation: u64,
    max_depth: usize,
    display_limit: usize,
    managed: bool,
) -> Result<Value> {
    let selected: Vec<_> = walk
        .nodes
        .iter()
        .filter(|node| node.admission == ImpactAdmission::Selected)
        .collect();
    let total = selected.iter().filter(|node| node.depth > 0).count();
    let lookup: HashMap<_, _> = walk
        .nodes
        .iter()
        .map(|node| (node.node.node_id.as_str(), node))
        .collect();
    let mut value = json!({"schema_version":2,"source_generation":generation,"target_ref":null,
        "requested_max_depth":max_depth,"source_layout":if managed{"managed"}else{"attested_legacy"},
        "status":"bounded_indexed_exposure","coverage":walk.coverage,"selected_dependents_observed":total,
        "selected_identities_observed":selected.len(),"selected_identities_returned":0,
        "selected_dependent_identities_returned":0,"node_identities_omitted":0,"edges_omitted":0,
        "frontier_rows_omitted":walk.coverage.frontier_omitted,"shortest_paths_omitted":total,
        "omitted_from_display":total,"display_truncated":false,"files":[],"nodes":[],"edges":[],
        "shortest_paths":[],"frontier":[],"recoveries":[],"continuation":null,
        "recovery_scopes":{
            "range":"Current-source range at indexed positions, not complete-file, method-boundary, callsite or value-flow proof. Verify returned file hash/freshness and project exclusions.",
            "navigation":"Bounded selected-source CodeMap for the known file, or a project representative when the file is unknown. Best-effort exploration only: no recovered binding, absence or runtime proof.",
            "direct":"Narrow direct impact for the admitted exact identity. A follow-up uses its own source selection; no complete pagination or atomic replay is promised.",
            "caller":"Caller declaration-range recovery; no callsite line was recorded.",
            "within":"Recorded source_line lies inside the caller declaration range; reuse its recovery. Indexed reference position only, not execution or value-flow proof.",
            "line":"Recorded source_line outside the caller range; recover that current source line. Indexed reference position only, not execution or value-flow proof."
        },
        "working_tree_checked":false,"exhaustive_codebase":false,"change_breakage_proven":false,
        "display_scope":"limit applies to shortest_paths, not canonical identities or additional verified edges",
        "edge_direction":"source_ref is caller/dependent; target_ref is the reached dependency",
        "limitations":["Selected means indexed node/file revisions and edge extraction/origin admission, not compiler, runtime or proposed-change validation.",
        "Additional verified connections include cross/revisit edges; they do not enumerate every simple path or imply field/value flow.",
        "Source-line evidence is recorded extractor metadata only. Declaration-range recovery is not a callsite. Current source may have changed; verify recovery hashes/freshness and project exclusions.",
        "Missing, unverified, dynamic and uninspected bindings remain unknown. Work/depth/byte caps may omit evidence; empty output proves no absence."]});
    let mut out = CompactEvidence {
        project,
        remaining: RESPONSE_BYTES.saturating_sub(section(&value).len() + 1024),
        files: Vec::new(),
        file_refs: BTreeMap::new(),
        nodes: Vec::new(),
        node_indices: BTreeMap::new(),
        edges: Vec::new(),
        edge_refs: BTreeMap::new(),
        recoveries: Vec::new(),
        recovery_refs: BTreeMap::new(),
    };
    let connected: std::collections::HashSet<_> = walk
        .witnesses
        .iter()
        .flat_map(|edge| [edge.source_id.as_str(), edge.target_id.as_str()])
        .collect();
    let mut ordered = selected.clone();
    ordered.sort_by(|a, b| {
        (a.node.node_id != target)
            .cmp(&(b.node.node_id != target))
            .then_with(|| {
                (!connected.contains(a.node.node_id.as_str()))
                    .cmp(&(!connected.contains(b.node.node_id.as_str())))
            })
            .then_with(|| a.depth.cmp(&b.depth))
            .then_with(|| a.node.node_id.cmp(&b.node.node_id))
    });
    let mut edge_reserve = None;
    for node in ordered {
        if node.node.node_id != target
            && !connected.contains(node.node.node_id.as_str())
            && edge_reserve.is_none()
        {
            // Sampled file-component roots without a verified external edge
            // must not consume the space needed to address actual consumers.
            let reserve = out.remaining.min(8 * 1024);
            out.remaining -= reserve;
            edge_reserve = Some(reserve);
        }
        let current = node_endpoint(node);
        if let Some(reference) = out.endpoint(&current)? {
            if node.node.node_id == target {
                value["target_ref"] = json!(reference);
                if let Some(recovery) = out.lookup_boundary(&current)? {
                    value["continuation"] = json!({"at_ref":reference,"recovery_ref":recovery,
                        "scope_ref":if current.admission=="selected"{"direct"}else{"navigation"}});
                }
            }
        }
    }
    out.remaining += edge_reserve.unwrap_or(0);
    let selected_returned = out.nodes.len();
    let dependent_returned = out
        .nodes
        .iter()
        .filter(|node| node["is_seed"] == false)
        .count();
    let tree_edge = |edge: &engram_graph::store::ImpactWitness| {
        lookup.get(edge.source_id.as_str()).is_some_and(|node| {
            node.predecessor.as_deref() == Some(edge.target_id.as_str())
                && node.via_kind.as_ref() == Some(&edge.kind)
        })
    };
    let mut edges: Vec<_> = walk.witnesses.iter().collect();
    edges.sort_by_key(|edge| !tree_edge(edge));
    let mut edge_omissions = 0;
    for edge in edges {
        let (Some(source_ref), Some(target_ref)) = (
            out.reference(&edge.source_id),
            out.reference(&edge.target_id),
        ) else {
            edge_omissions += 1;
            continue;
        };
        let Some(source) = lookup
            .get(edge.source_id.as_str())
            .filter(|node| node.admission == ImpactAdmission::Selected)
        else {
            edge_omissions += 1;
            continue;
        };
        let source_index = out.node_indices[&edge.source_id];
        let source_recovery = out.nodes[source_index]["recovery_ref"]
            .as_str()
            .context("Node recovery missing")?
            .to_owned();
        let caller_range_known = recovery_path(source.node.file_path.as_str())
            && source.node.start_line > 0
            && source.node.end_line >= source.node.start_line;
        let line_within_caller = caller_range_known
            && edge
                .source_line
                .is_some_and(|line| line >= source.node.start_line && line <= source.node.end_line);
        let recovery = match edge.source_line {
            Some(line) if !line_within_caller && recovery_path(source.node.file_path.as_str()) => {
                Some(out.range_recovery(source.node.file_path.as_str(), line, line)?)
            }
            _ => None,
        };
        let recovery_ref = recovery
            .as_ref()
            .map_or(source_recovery, |recovery| recovery.reference.clone());
        let reference = format!("e{}", out.edges.len());
        let row = json!({"ref":reference,"source_ref":source_ref,"target_ref":target_ref,"kind":edge.kind.as_str(),
            "edge_generation":edge.edge_generation,"source_generation":source.node.generation,
            "edge_source_binding":if edge.edge_generation==source.node.generation{"extraction_generation"}else{"verified_unchanged_caller_origin"},
            "connection_role":if tree_edge(edge){"shortest_predecessor"}else{"additional_verified_connection"},
            "source_line":edge.source_line,"recovery_ref":recovery_ref,
            "scope_ref":if line_within_caller{"within"}else if recovery.is_some(){"line"}else if edge.source_line.is_some() || !caller_range_known{"navigation"}else{"caller"}});
        let mut staged = vec![&row];
        if let Some((_, value)) = recovery
            .as_ref()
            .and_then(|recovery| recovery.fresh.as_ref())
        {
            staged.push(value);
        }
        if !out.charge(&staged)? {
            edge_omissions += 1;
            continue;
        }
        if let Some(recovery) = recovery {
            out.commit_recovery(recovery);
        }
        out.edge_refs.insert(
            (source_ref, target_ref, edge.kind.as_str().into()),
            reference,
        );
        out.edges.push(row);
    }
    let mut frontier = Vec::new();
    let mut frontier_omissions = walk.coverage.frontier_omitted;
    for lead in &walk.frontier {
        let known = lookup.get(lead.source_id.as_str());
        let current = if let Some(known) = known {
            node_endpoint(known)
        } else {
            Endpoint {
                id: &lead.source_id,
                name: None,
                kind: None,
                file: lead.file_path.as_ref().map(|path| path.as_str()),
                start: lead.start_line,
                end: lead.end_line,
                generation: lead.node_generation,
                depth: Some(lead.depth),
                admission: if lead.file_path.is_some() {
                    "unknown"
                } else {
                    "unresolved"
                },
                is_seed: false,
            }
        };
        let Some(source_ref) = out.endpoint(&current)? else {
            frontier_omissions += 1;
            continue;
        };
        let target_ref = match lead.target_id.as_deref() {
            None => None,
            Some(target) => {
                if let Some(reference) = out.reference(target) {
                    Some(reference)
                } else if let Some(known) = lookup.get(target) {
                    out.endpoint(&node_endpoint(known))?
                } else {
                    None
                }
            }
        };
        if lead.target_id.is_some() && target_ref.is_none() {
            frontier_omissions += 1;
            continue;
        }
        let source_index = out.node_indices[&lead.source_id];
        let row = json!({"source_ref":source_ref,"target_ref":target_ref,"kind":lead.kind.as_ref().map(|kind|kind.as_str()),
            "reason":lead.reason,"depth":lead.depth,"recovery_ref":out.nodes[source_index]["recovery_ref"]});
        if out.charge(&[&row])? {
            frontier.push(row);
        } else {
            frontier_omissions += 1;
        }
    }
    let mut paths = Vec::new();
    let mut candidates: Vec<_> = selected
        .iter()
        .filter(|node| node.depth > 0)
        .copied()
        .collect();
    candidates.sort_by(|a, b| {
        b.depth
            .cmp(&a.depth)
            .then_with(|| a.node.node_id.cmp(&b.node.node_id))
    });
    for dependent in candidates.into_iter().take(display_limit.clamp(1, 25)) {
        let mut node_refs = Vec::new();
        let mut edge_refs = Vec::new();
        let mut cursor = dependent;
        let mut complete = true;
        for _ in 0..=8 {
            let Some(reference) = out.reference(&cursor.node.node_id) else {
                complete = false;
                break;
            };
            node_refs.push(reference);
            let Some(parent) = cursor.predecessor.as_deref() else {
                break;
            };
            let (Some(parent_node), Some(parent_ref), Some(kind)) = (
                lookup.get(parent),
                out.reference(parent),
                cursor.via_kind.as_ref(),
            ) else {
                complete = false;
                break;
            };
            let key = (
                out.reference(&cursor.node.node_id).unwrap(),
                parent_ref,
                kind.as_str().to_owned(),
            );
            let Some(edge) = out.edge_refs.get(&key) else {
                complete = false;
                break;
            };
            edge_refs.push(edge.clone());
            cursor = parent_node;
        }
        if !complete || cursor.predecessor.is_some() {
            continue;
        }
        node_refs.reverse();
        edge_refs.reverse();
        let row = json!({"dependent_ref":out.reference(&dependent.node.node_id),"node_refs":node_refs,"edge_refs":edge_refs,
            "seed_ref":out.reference(&cursor.node.node_id),"seed_relationship":if cursor.node.node_id==target{"requested_target"}else{"contained_declaration_in_requested_file; structural_origin_not_a_causal_edge"}});
        if out.charge(&[&row])? {
            paths.push(row);
        }
    }
    let omitted_paths = total - paths.len();
    value["selected_identities_returned"] = json!(selected_returned);
    value["selected_dependent_identities_returned"] = json!(dependent_returned);
    value["node_identities_omitted"] = json!(selected.len() - selected_returned);
    value["edges_omitted"] = json!(edge_omissions);
    value["frontier_rows_omitted"] = json!(frontier_omissions);
    value["shortest_paths_omitted"] = json!(omitted_paths);
    value["omitted_from_display"] = json!(omitted_paths);
    value["display_truncated"] = json!(
        omitted_paths > 0
            || selected_returned < selected.len()
            || edge_omissions > 0
            || frontier_omissions > 0
    );
    value["files"] = json!(out.files);
    value["nodes"] = json!(out.nodes);
    value["edges"] = json!(out.edges);
    value["shortest_paths"] = json!(paths);
    value["frontier"] = json!(frontier);
    value["recoveries"] = json!(out.recoveries);
    ensure!(
        section(&value).len() <= RESPONSE_BYTES,
        "Impact evidence exceeds response budget"
    );
    Ok(value)
}

async fn collect_evidence(
    state: &AppState,
    project: &str,
    target_request: TargetRequest,
    max_depth: usize,
    display_limit: usize,
) -> Result<CollectedImpact> {
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
        let exact = matches!(&target_request, TargetRequest::Exact(_));
        let target = match target_request {
            TargetRequest::Exact(id) => id,
            TargetRequest::Resolve {
                file_path: Some(path),
                ..
            } => format!("file:{path}"),
            TargetRequest::Resolve { symbol_fqn, .. } => {
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
            }
        };
        let mut seeds = vec![target.clone()];
        let mut seed_cap = false;
        let mut internal_file = None;
        if !exact {
            let root = graph
                .get_node(&project, &target)?
                .context("Impact target unavailable")?;
            if root.node_type == "file" {
                internal_file = Some(root.file_path.as_str().to_owned());
                let (members, capped) = graph.file_component_members(
                    &project,
                    root.file_path.as_str(),
                    &target,
                    127,
                )?;
                seeds.extend(members);
                seed_cap = capped;
            }
        }
        let admit = |node: &engram_graph::Node,
                     file: Option<&engram_graph::Node>|
         -> Result<ImpactAdmission> {
            admit_selected_source(&membership, node, file)
        };
        let mut walk = graph.bounded_incoming_walk_excluding_file(
            &project,
            &seeds,
            max_depth,
            &super::blast_radius_service::is_causal_dependency,
            &admit,
            internal_file.as_deref(),
        )?;
        if seed_cap {
            walk.coverage.truncated = true;
            walk.coverage
                .reasons
                .push("file_seed_cap; uninspected declarations may have other callers".into());
        }
        let target_revision_admitted = walk.nodes.iter().any(|node| {
            node.depth == 0
                && node.node.node_id == target
                && node.admission == ImpactAdmission::Selected
        });
        let direct_dependents = walk
            .nodes
            .iter()
            .filter(|node| node.admission == ImpactAdmission::Selected && node.depth == 1)
            .count();
        let transitive_dependents = walk
            .nodes
            .iter()
            .filter(|node| node.admission == ImpactAdmission::Selected && node.depth > 1)
            .count();
        let mut value = render(
            &walk,
            &target,
            &project,
            generation,
            max_depth,
            display_limit,
            source.is_managed(),
        )?;
        let mut coverage = walk.coverage;
        value["dependency_scope"] = json!(if internal_file.is_some() {
            "external_file_dependents; same-file source rows excluded before accepted-result budgets"
        } else {
            "symbol_dependents; same-file callers included"
        });
        value["target_revision_admitted"] = json!(target_revision_admitted);
        if !target_revision_admitted {
            coverage.truncated = true;
            coverage.reasons.push("target_revision_not_admitted".into());
            value["status"] = json!("unavailable");
            value["coverage"] = json!(coverage);
            value["selected_dependents_observed"] = Value::Null;
            value["omitted_from_display"] = Value::Null;
            value["error"] = json!("The requested root was not admitted by the held source selection; unavailable evidence is not zero impact.");
        }
        ensure!(section(&value).len() <= RESPONSE_BYTES, "Impact evidence exceeds response budget");
        Ok(CollectedImpact {
            value,
            source,
            generation,
            target_revision_admitted,
            coverage,
            direct_dependents,
            transitive_dependents,
        })
    })
    .await
    .context("Impact evidence worker unavailable")?
}

fn unavailable_value(max_depth: usize, error: &anyhow::Error) -> Value {
    json!({"schema_version":2,"source_generation":null,"requested_max_depth":max_depth,
        "status":"unavailable","coverage":{"truncated":true,"reasons":["source_evidence_unavailable"]},
        "files":[],"nodes":[],"edges":[],"shortest_paths":[],"frontier":[],"recoveries":[],"omitted_from_display":null,
        "error":format!("{error:#}").chars().take(2048).collect::<String>(),
        "exhaustive_codebase":false,"change_breakage_proven":false,
        "limitations":["Unavailable source evidence is not zero impact. Resolve the target and source snapshot before relying on current-source paths."]})
}

/// Retain the same source capability through graph collection, rendering and
/// caller response assembly. Errors disclose unavailable evidence separately
/// from the older, unverified live-graph inventory. The existing tool's rendered
/// contract is unchanged; consumers of structured facts use collect_assessment.
pub async fn collect(
    state: &AppState,
    project: &str,
    file_path: Option<String>,
    symbol_fqn: Option<String>,
    max_depth: usize,
    display_limit: usize,
) -> (String, Option<Arc<SourceSnapshotGuard>>, Option<u64>) {
    let result = collect_evidence(
        state,
        project,
        TargetRequest::Resolve {
            file_path,
            symbol_fqn,
        },
        max_depth,
        display_limit,
    )
    .await;
    match result {
        Ok(collected) => (
            section(&collected.value),
            Some(collected.source),
            Some(collected.generation),
        ),
        Err(error) => (section(&unavailable_value(max_depth, &error)), None, None),
    }
}

/// Collect once for an exact declaration identity, without name resolution or
/// rendered-Markdown parsing. The returned guard must remain held through the
/// consumer's actual blocking work and response assembly; use source_generation
/// from this assessment rather than recapturing a later HEAD.
pub async fn collect_assessment(
    state: &AppState,
    project: &str,
    exact_node_id: &str,
    max_depth: usize,
    display_limit: usize,
) -> (ImpactAssessment, Option<Arc<SourceSnapshotGuard>>) {
    let result = if exact_node_id.is_empty()
        || exact_node_id.len() > 2048
        || exact_node_id.contains(['\0', '\n', '\r'])
    {
        Err(anyhow::anyhow!(
            "Impact assessment requires an exact bounded node_id"
        ))
    } else {
        collect_evidence(
            state,
            project,
            TargetRequest::Exact(exact_node_id.to_owned()),
            max_depth,
            display_limit,
        )
        .await
    };
    match result {
        Ok(mut collected) => {
            let admitted = collected.target_revision_admitted;
            if !admitted {
                collected.coverage.truncated = true;
                if !collected
                    .coverage
                    .reasons
                    .iter()
                    .any(|reason| reason == "target_revision_not_admitted")
                {
                    collected
                        .coverage
                        .reasons
                        .push("target_revision_not_admitted".into());
                }
                collected.value["status"] = json!("unavailable");
                collected.value["coverage"] = json!(collected.coverage);
                collected.value["selected_dependents_observed"] = Value::Null;
                collected.value["omitted_from_display"] = Value::Null;
                collected.value["error"] = json!(
                    "The exact target revision was not admitted by the held source selection; unavailable evidence is not zero impact."
                );
            }
            let source_layout = if collected.source.is_managed() {
                "managed"
            } else {
                "attested_legacy"
            };
            let assessment = ImpactAssessment {
                source_generation: Some(collected.generation),
                source_layout: source_layout.into(),
                requested_max_depth: max_depth,
                target_revision_admitted: admitted,
                status: if admitted {
                    "bounded_indexed_exposure"
                } else {
                    "unavailable"
                }
                .into(),
                coverage: collected.coverage,
                selected_dependents_observed: admitted
                    .then_some(collected.direct_dependents + collected.transitive_dependents),
                direct_dependents_observed: admitted.then_some(collected.direct_dependents),
                transitive_dependents_observed: admitted.then_some(collected.transitive_dependents),
                evidence: collected.value,
            };
            (assessment, Some(collected.source))
        }
        Err(error) => {
            let coverage = ImpactWalkCoverage {
                truncated: true,
                reasons: vec!["source_evidence_unavailable".into()],
                ..Default::default()
            };
            let mut evidence = unavailable_value(max_depth, &error);
            evidence["coverage"] = json!(coverage);
            (
                ImpactAssessment {
                    source_generation: None,
                    source_layout: "unavailable".into(),
                    requested_max_depth: max_depth,
                    target_revision_admitted: false,
                    status: "unavailable".into(),
                    coverage,
                    selected_dependents_observed: None,
                    direct_dependents_observed: None,
                    transitive_dependents_observed: None,
                    evidence,
                },
                None,
            )
        }
    }
}
