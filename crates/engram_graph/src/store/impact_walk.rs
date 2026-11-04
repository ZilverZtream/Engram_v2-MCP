//! Bounded reverse dependency observations from one graph read transaction.
//! Source admission is supplied by the caller's held source capability. This
//! module never opens source files, a lexical store, or native vector tables.
use super::*;
use bincode::Options;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

const MAX_SEEDS: usize = 128;
const MAX_NODES: usize = 512;
const MAX_WITNESSES: usize = 2048;
const MAX_FRONTIER: usize = 128;
const MAX_RETAINED_STRING_BYTES: usize = 256 * 1024;
const MAX_RAW_ROWS: usize = 16_384;
const MAX_PER_ENDPOINT_KIND: usize = 64;
const MAX_DEPTH: usize = 8;
const MAX_RECORD_BYTES: usize = 256 * 1024;
const MAX_DECODE_BYTES: usize = 8 * 1024 * 1024;
const WALK_TIME: Duration = Duration::from_secs(2);

// Reserved producer evidence. Extractor metadata is projected to JSON strings
// by ingestion, so it cannot supply this typed nested object. Raw GraphStore
// writers retain their existing trusted managed-writer boundary.
const SOURCE_ORIGIN: &str = "_engram_source_origin";

pub(super) fn edge_source_revision_matches(edge: &Edge, source: &Node) -> bool {
    if edge.source_id != source.node_id {
        return false;
    }
    // A remap's logical generation must never override its original caller
    // revision. In particular, a later source refresh can share the same
    // numeric generation as an already remapped edge.
    let origin = match edge.metadata.as_ref() {
        None => None,
        Some(JsonValue::Object(metadata)) => metadata.get(SOURCE_ORIGIN),
        // This is not ordinary extractor metadata. Preserve it, but do not
        // infer that an internal origin key is absent inside an opaque value.
        Some(_) => return false,
    };
    let Some(origin) = origin else {
        return edge.generation == source.generation;
    };
    let Some(origin) = origin.as_object() else {
        return false;
    };
    origin.len() == 3
        && origin.get("version").and_then(JsonValue::as_u64) == Some(1)
        && origin.get("source_id").and_then(JsonValue::as_str) == Some(source.node_id.as_str())
        && origin.get("source_generation").and_then(JsonValue::as_u64) == Some(source.generation)
}

/// Called by the scoped cleanup/remapper with the unchanged caller observed in
/// its own read transaction. Preserve logical edge generation for existing GC
/// rules, while recording only a caller revision that the old edge proves.
pub(super) fn bind_remapped_source_origin(old: &Edge, remapped: &mut Edge, source: Option<&Node>) {
    if old.namespace != engram_core::namespaces::NAMESPACE_MEMORY {
        return;
    }
    let verified = source.is_some_and(|source| {
        old.source_id == remapped.source_id && edge_source_revision_matches(old, source)
    });
    let metadata = remapped
        .metadata
        .get_or_insert_with(|| serde_json::json!({}));
    if let JsonValue::Object(metadata) = metadata {
        if verified {
            let source = source.expect("verified source");
            metadata.insert(
                SOURCE_ORIGIN.into(),
                serde_json::json!({
                    "version": 1,
                    "source_id": source.node_id,
                    "source_generation": source.generation,
                }),
            );
        } else {
            // Preserve mismatched/invalid evidence, or mark an unattested
            // remap explicitly. Stripping it to absence could later authorize
            // this old edge when a caller catches up to its logical generation.
            metadata.entry(SOURCE_ORIGIN).or_insert(JsonValue::Null);
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum ImpactAdmission {
    Selected,
    NonSelectedRevision,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImpactWalkNode {
    /// Graph identity and source position. Metadata is removed after admission.
    pub node: Node,
    pub admission: ImpactAdmission,
    /// Seeds have depth zero; every following step is an incoming dependency.
    pub depth: usize,
    /// The target this caller reaches on its first shortest observed path.
    pub predecessor: Option<String>,
    pub via_kind: Option<EdgeKind>,
    pub edge_generation: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImpactWitness {
    pub source_id: String,
    pub target_id: String,
    pub kind: EdgeKind,
    pub edge_generation: u64,
    pub weight: u32,
    pub depth: usize,
    /// Recorded extractor reference line, never inferred from a declaration.
    pub source_line: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImpactFrontier {
    pub source_id: String,
    pub target_id: Option<String>,
    pub kind: Option<EdgeKind>,
    pub reason: &'static str,
    pub depth: usize,
    pub file_path: Option<RelPath>,
    pub start_line: Option<u32>,
    pub end_line: Option<u32>,
    pub node_generation: Option<u64>,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct ImpactWalkCoverage {
    /// Raw adjacency entries inspected, including entries rejected afterwards.
    pub raw_rows: usize,
    /// File-impact rows whose source belongs to the requested file. Raw/decode
    /// work is charged, but these are not external dependents or frontier.
    pub internal_rows_skipped: usize,
    /// Distinct source identities admitted/excluded after edge validation.
    pub nodes_observed: usize,
    /// Missing graph-node read observations, not proof of absent source code.
    pub missing_nodes: usize,
    /// Unknown admission/edge-source observations; an ID may occur repeatedly.
    pub unknown_nodes: usize,
    pub non_selected_nodes: usize,
    pub cycles_or_revisits: usize,
    pub max_depth_reached: usize,
    /// Work, display-independent traversal, or unknown-source coverage was lost.
    pub truncated: bool,
    pub reasons: Vec<String>,
    pub frontier_omitted: usize,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct IncomingImpactWalk {
    /// Selected and unknown identities only; excluded revisions are counted.
    pub nodes: Vec<ImpactWalkNode>,
    pub witnesses: Vec<ImpactWitness>,
    /// Unverified or missing endpoints, separate from admitted causal edges.
    pub frontier: Vec<ImpactFrontier>,
    pub coverage: ImpactWalkCoverage,
}

fn gap(coverage: &mut ImpactWalkCoverage, reason: &'static str) {
    coverage.truncated = true;
    if !coverage.reasons.iter().any(|old| old == reason) {
        coverage.reasons.push(reason.into());
    }
}

struct Budget {
    started: Instant,
    string_bytes: usize,
    decode_bytes: usize,
    halted: bool,
}

impl Budget {
    fn live(&mut self, coverage: &mut ImpactWalkCoverage) -> bool {
        if self.started.elapsed() >= WALK_TIME {
            gap(coverage, "cooperative_deadline");
            self.halted = true;
        }
        !self.halted
    }

    fn retain(&mut self, bytes: usize, coverage: &mut ImpactWalkCoverage) -> bool {
        match self.string_bytes.checked_add(bytes) {
            Some(total) if total <= MAX_RETAINED_STRING_BYTES => {
                self.string_bytes = total;
                true
            }
            _ => {
                gap(coverage, "retained_string_byte_limit");
                self.halted = true;
                false
            }
        }
    }

    fn record(&mut self, bytes: usize, coverage: &mut ImpactWalkCoverage) -> bool {
        if bytes > MAX_RECORD_BYTES {
            gap(coverage, "graph_record_byte_limit");
            coverage.unknown_nodes += 1;
            return false;
        }
        match self.decode_bytes.checked_add(bytes) {
            Some(total) if total <= MAX_DECODE_BYTES => {
                self.decode_bytes = total;
                true
            }
            _ => {
                gap(coverage, "graph_decode_byte_limit");
                self.halted = true;
                false
            }
        }
    }
}

fn identity_bytes(node: &Node) -> usize {
    node.node_id.len()
        + node.node_type.len()
        + node.name.len()
        + node.namespace.len()
        + node.language.len()
        + node.file_path.as_str().len()
}

fn record_frontier(
    frontier: &mut Vec<ImpactFrontier>,
    coverage: &mut ImpactWalkCoverage,
    budget: &mut Budget,
    witness_count: usize,
    source: &str,
    target: Option<&str>,
    kind: Option<&EdgeKind>,
    node: Option<&Node>,
    depth: usize,
    reason: &'static str,
) {
    if frontier.len() == MAX_FRONTIER || frontier.len() + witness_count >= MAX_WITNESSES {
        coverage.frontier_omitted += 1;
        gap(coverage, "frontier_lead_limit");
        return;
    }
    let bytes = source.len()
        + target.map_or(0, str::len)
        + node.map_or(0, |node| node.file_path.as_str().len());
    if !budget.retain(bytes, coverage) {
        coverage.frontier_omitted += 1;
        return;
    }
    frontier.push(ImpactFrontier {
        source_id: source.to_owned(),
        target_id: target.map(str::to_owned),
        kind: kind.cloned(),
        reason,
        depth,
        file_path: node.map(|node| node.file_path.clone()),
        start_line: node.map(|node| node.start_line),
        end_line: node.map(|node| node.end_line),
        node_generation: node.map(|node| node.generation),
    });
}

fn recorded_source_line(edge: &Edge) -> Option<u32> {
    let value = edge.metadata.as_ref()?.get("src_line")?;
    let line = match value {
        JsonValue::String(text) if text.len() <= 10 => text.parse::<u32>().ok(),
        JsonValue::Number(number) => number.as_u64().and_then(|line| u32::try_from(line).ok()),
        _ => None,
    }?;
    (line > 0).then_some(line)
}

// Called only after the retained-byte gate. Never clone arbitrary metadata
// into the traversal cache or response, even transiently.
fn identity_clone(node: &Node) -> Node {
    Node {
        node_id: node.node_id.clone(),
        node_type: node.node_type.clone(),
        name: node.name.clone(),
        namespace: node.namespace.clone(),
        language: node.language.clone(),
        file_path: node.file_path.clone(),
        start_line: node.start_line,
        end_line: node.end_line,
        generation: node.generation,
        metadata: None,
    }
}

struct Cached {
    node: Node,
    admission: ImpactAdmission,
}

/// Remember a source only after its edge's generation was validated. The
/// callback already observed full bounded node/file metadata; none is retained.
fn remember(
    node: &Node,
    admission: ImpactAdmission,
    depth: usize,
    via: Option<(&str, &EdgeKind, u64)>,
    cache: &mut HashMap<String, Cached>,
    output: &mut IncomingImpactWalk,
    budget: &mut Budget,
) -> Option<usize> {
    if cache.len() >= MAX_NODES {
        gap(&mut output.coverage, "visited_node_limit");
        budget.halted = true;
        return None;
    }
    let retained_output = admission != ImpactAdmission::NonSelectedRevision;
    // Cache key, cached Node, optional report Node and predecessor are distinct
    // allocations. Charge every retained identity/name/path string before clone.
    let bytes = node.node_id.len()
        + identity_bytes(&node)
        + if retained_output {
            identity_bytes(&node) + via.map_or(0, |v| v.0.len())
        } else {
            0
        };
    if !budget.retain(bytes, &mut output.coverage) {
        return None;
    }
    let node = identity_clone(node);
    let index = if retained_output {
        let index = output.nodes.len();
        output.nodes.push(ImpactWalkNode {
            node: node.clone(),
            admission,
            depth,
            predecessor: via.map(|v| v.0.to_owned()),
            via_kind: via.map(|v| v.1.clone()),
            edge_generation: via.map(|v| v.2),
        });
        Some(index)
    } else {
        output.coverage.non_selected_nodes += 1;
        None
    };
    if admission == ImpactAdmission::Unknown {
        output.coverage.unknown_nodes += 1;
        gap(&mut output.coverage, "unknown_source_admission");
    }
    output.coverage.nodes_observed += 1;
    cache.insert(node.node_id.clone(), Cached { node, admission });
    index
}

impl GraphStore {
    /// Impact seeds for exact selected files, with native callable-prefix
    /// seeks before structural Contains discovery. A bounded legacy scan runs
    /// only when both produce no seeds. Unlinked noncanonical supplemental
    /// nodes in a mixed native graph are not exhaustively inventoried.
    pub fn bounded_file_impact_seeds(
        &self,
        project_id: &str,
        file_paths: &[String],
        admit: &dyn Fn(&Node, Option<&Node>) -> anyhow::Result<ImpactAdmission>,
    ) -> anyhow::Result<(Vec<String>, ImpactWalkCoverage)> {
        self.bounded_file_impact_seeds_inner(project_id, file_paths, admit, true)
    }

    /// Navigation never falls back to a project-wide legacy node scan.
    pub fn bounded_file_navigation_seeds(
        &self,
        project_id: &str,
        file_paths: &[String],
        admit: &dyn Fn(&Node, Option<&Node>) -> anyhow::Result<ImpactAdmission>,
    ) -> anyhow::Result<(Vec<String>, ImpactWalkCoverage)> {
        self.bounded_file_impact_seeds_inner(project_id, file_paths, admit, false)
    }

    fn bounded_file_impact_seeds_inner(
        &self,
        project_id: &str,
        file_paths: &[String],
        admit: &dyn Fn(&Node, Option<&Node>) -> anyhow::Result<ImpactAdmission>,
        allow_legacy: bool,
    ) -> anyhow::Result<(Vec<String>, ImpactWalkCoverage)> {
        anyhow::ensure!(
            !project_id.is_empty()
                && project_id.len() <= 1024
                && !project_id.chars().any(char::is_control),
            "Invalid callable project identity"
        );
        anyhow::ensure!(
            !file_paths.is_empty() && file_paths.len() <= 8,
            "Callable discovery requires 1..8 files"
        );
        for path in file_paths {
            anyhow::ensure!(
                !path.is_empty() && path.len() <= 2048 && !path.chars().any(char::is_control),
                "Invalid callable file identity"
            );
        }
        let tx = self.db.begin_read()?;
        let nodes = tx.open_table(NODES)?;
        let outgoing = tx.open_table(ADJ_OUT)?;
        let mut coverage = ImpactWalkCoverage::default();
        let mut budget = Budget {
            started: Instant::now(),
            string_bytes: 0,
            decode_bytes: 0,
            halted: false,
        };
        let mut seeds = std::collections::BTreeSet::new();
        let mut other_declarations = std::collections::BTreeSet::new();
        let mut admitted_files = Vec::new();
        let mut file_origins = Vec::new();
        let decode = |bytes: &[u8],
                      budget: &mut Budget,
                      coverage: &mut ImpactWalkCoverage|
         -> anyhow::Result<Option<Node>> {
            if !budget.live(coverage) || !budget.record(bytes.len(), coverage) {
                return Ok(None);
            }
            let node: Node = bincode::DefaultOptions::new()
                .with_fixint_encoding()
                .with_limit(MAX_RECORD_BYTES as u64)
                .reject_trailing_bytes()
                .deserialize(bytes)?;
            anyhow::ensure!(
                node.node_id.len() <= 2048 && !node.node_id.chars().any(char::is_control),
                "Invalid callable node identity"
            );
            Ok(Some(node))
        };
        let read = |id: &str,
                    budget: &mut Budget,
                    coverage: &mut ImpactWalkCoverage|
         -> anyhow::Result<Option<Node>> {
            let key = format!("{project_id}\0{id}");
            let Some(raw) = nodes.get(key.as_str())? else {
                coverage.missing_nodes += 1;
                gap(coverage, "missing_callable_component_node");
                return Ok(None);
            };
            let node = decode(raw.value(), budget, coverage)?;
            anyhow::ensure!(
                node.as_ref().is_none_or(|node| node.node_id == id),
                "Callable node identity disagrees with its key"
            );
            Ok(node)
        };
        let callable =
            |kind: &str| matches!(kind, "function" | "method" | "constructor" | "property");
        let observe = |node: &Node,
                       file: &Node,
                       seeds: &mut std::collections::BTreeSet<String>,
                       other_declarations: &mut std::collections::BTreeSet<String>,
                       budget: &mut Budget,
                       coverage: &mut ImpactWalkCoverage|
         -> anyhow::Result<()> {
            if node.file_path != file.file_path
                || seeds.contains(&node.node_id)
                || other_declarations.contains(&node.node_id)
            {
                return Ok(());
            }
            coverage.nodes_observed += 1;
            match admit(node, Some(file))? {
                ImpactAdmission::NonSelectedRevision => {
                    coverage.non_selected_nodes += 1;
                    return Ok(());
                }
                ImpactAdmission::Unknown => {
                    coverage.unknown_nodes += 1;
                    gap(coverage, "unknown_callable_source_admission");
                    return Ok(());
                }
                ImpactAdmission::Selected => {}
            }
            if !budget.live(coverage) {
                return Ok(());
            }
            if !callable(&node.node_type) {
                if other_declarations.len() == MAX_SEEDS {
                    gap(coverage, "noncallable_declaration_candidate_limit");
                } else if budget.retain(node.node_id.len(), coverage) {
                    other_declarations.insert(node.node_id.clone());
                }
                return Ok(());
            }
            if seeds.len() == MAX_SEEDS {
                gap(coverage, "callable_seed_limit");
                budget.halted = true;
                return Ok(());
            }
            if budget.retain(node.node_id.len(), coverage) {
                seeds.insert(node.node_id.clone());
            }
            Ok(())
        };
        'callables: for (file_index, path) in file_paths.iter().enumerate() {
            if !budget.live(&mut coverage) {
                break;
            }
            let file_id = format!("file:{path}");
            let Some(file) = read(&file_id, &mut budget, &mut coverage)? else {
                continue;
            };
            if file.node_type != "file"
                || file.file_path.as_str() != path
                || admit(&file, Some(&file))? != ImpactAdmission::Selected
            {
                coverage.unknown_nodes += 1;
                gap(&mut coverage, "unadmitted_callable_file");
                continue;
            }
            if !budget.retain(file_id.len(), &mut coverage) {
                break;
            }
            file_origins.push(file_id);
            admitted_files.push(file_index);
            // NodeId::symbol and ingest_service's native producers use this
            // exact kind/file prefix. Fields/classes never consume seed slots.
            for kind in ["function", "method", "constructor", "property"] {
                let prefix = format!("{project_id}\0sym:{kind}:{path}:");
                let mut entries = nodes.range(prefix.as_str()..)?;
                loop {
                    if !budget.live(&mut coverage) {
                        break 'callables;
                    }
                    if coverage.raw_rows == MAX_RAW_ROWS {
                        gap(&mut coverage, "callable_raw_row_limit");
                        budget.halted = true;
                        break 'callables;
                    }
                    let Some(entry) = entries.next() else {
                        break;
                    };
                    coverage.raw_rows += 1;
                    let (key, raw) = entry?;
                    if !key.value().starts_with(&prefix) {
                        break;
                    }
                    if let Some(node) = decode(raw.value(), &mut budget, &mut coverage)? {
                        anyhow::ensure!(
                            key.value() == format!("{project_id}\0{}", node.node_id),
                            "Callable node identity disagrees with its key"
                        );
                        observe(
                            &node,
                            &file,
                            &mut seeds,
                            &mut other_declarations,
                            &mut budget,
                            &mut coverage,
                        )?;
                    }
                }
            }
        }
        // No structural class/field work starts until callable seeks for ALL
        // selected files have had access to the shared work budget.
        'components: for file_index in admitted_files {
            if !budget.live(&mut coverage) {
                break;
            }
            let path = &file_paths[file_index];
            let file_id = format!("file:{path}");
            let Some(file) = read(&file_id, &mut budget, &mut coverage)? else {
                continue;
            };
            let mut queue = VecDeque::new();
            let mut expanded = std::collections::BTreeSet::new();
            if !budget.retain(file_id.len() * 2, &mut coverage) {
                break;
            }
            expanded.insert(file_id.clone());
            queue.push_back((file_id, 0usize));
            while let Some((container, depth)) = queue.pop_front() {
                let prefix = adj_key(project_id, &EdgeKind::Contains, &container);
                let mut entries = outgoing.range((prefix.as_str(), "")..)?;
                loop {
                    if !budget.live(&mut coverage) {
                        break 'components;
                    }
                    if coverage.raw_rows == MAX_RAW_ROWS {
                        gap(&mut coverage, "callable_raw_row_limit");
                        budget.halted = true;
                        break 'components;
                    }
                    let Some(entry) = entries.next() else {
                        break;
                    };
                    coverage.raw_rows += 1;
                    let (key, raw) = entry?;
                    let (observed, id) = key.value();
                    if observed != prefix {
                        break;
                    }
                    let _ = decode_adj_value(raw.value())?;
                    anyhow::ensure!(
                        id.len() <= 2048 && !id.chars().any(char::is_control),
                        "Invalid callable component identity"
                    );
                    if seeds.contains(id) || expanded.contains(id) {
                        continue;
                    }
                    let Some(node) = read(id, &mut budget, &mut coverage)? else {
                        continue;
                    };
                    if node.file_path.as_str() != path {
                        continue;
                    }
                    observe(
                        &node,
                        &file,
                        &mut seeds,
                        &mut other_declarations,
                        &mut budget,
                        &mut coverage,
                    )?;
                    if !matches!(
                        node.node_type.as_str(),
                        "class" | "module" | "interface" | "struct" | "namespace"
                    ) {
                        continue;
                    }
                    if admit(&node, Some(&file))? != ImpactAdmission::Selected {
                        coverage.unknown_nodes += 1;
                        gap(&mut coverage, "unadmitted_callable_container");
                        continue;
                    }
                    if depth == MAX_DEPTH {
                        gap(&mut coverage, "callable_component_depth_limit");
                        continue;
                    }
                    if expanded.len() == MAX_NODES {
                        gap(&mut coverage, "callable_component_node_limit");
                        budget.halted = true;
                        break 'components;
                    }
                    if !budget.retain(id.len() * 2, &mut coverage) {
                        break 'components;
                    }
                    expanded.insert(id.to_owned());
                    queue.push_back((id.to_owned(), depth + 1));
                }
            }
        }
        if !allow_legacy && seeds.is_empty() && other_declarations.is_empty() {
            gap(
                &mut coverage,
                "native_declaration_membership_unavailable; legacy scan disabled",
            );
        }
        if allow_legacy && seeds.is_empty() && other_declarations.is_empty() && !budget.halted {
            gap(
                &mut coverage,
                "bounded_legacy_callable_fallback; native membership unavailable",
            );
            let prefix = format!("{project_id}\0");
            let mut entries = nodes.range(prefix.as_str()..)?;
            loop {
                if !budget.live(&mut coverage) {
                    break;
                }
                if coverage.raw_rows == MAX_RAW_ROWS {
                    gap(&mut coverage, "callable_raw_row_limit");
                    break;
                }
                let Some(entry) = entries.next() else {
                    break;
                };
                coverage.raw_rows += 1;
                let (key, raw) = entry?;
                if !key.value().starts_with(&prefix) {
                    break;
                }
                let Some(node) = decode(raw.value(), &mut budget, &mut coverage)? else {
                    continue;
                };
                anyhow::ensure!(
                    key.value() == format!("{project_id}\0{}", node.node_id),
                    "Legacy callable node identity disagrees with its key"
                );
                if node.node_type == "file"
                    || !file_paths
                        .iter()
                        .any(|path| path == node.file_path.as_str())
                {
                    continue;
                }
                let Some(file) = read(
                    &format!("file:{}", node.file_path.as_str()),
                    &mut budget,
                    &mut coverage,
                )?
                else {
                    continue;
                };
                if file.node_type != "file" || file.file_path != node.file_path {
                    continue;
                }
                observe(
                    &node,
                    &file,
                    &mut seeds,
                    &mut other_declarations,
                    &mut budget,
                    &mut coverage,
                )?;
            }
        }
        // Type/data-only areas still have causal consumers (for example
        // InheritsFrom and ReadsSetting). They use only slots remaining AFTER
        // every discovered callable, never an alphabetic class/field prefix.
        for id in file_origins.into_iter().chain(other_declarations) {
            if seeds.len() == MAX_SEEDS {
                gap(&mut coverage, "declaration_seed_limit");
                break;
            }
            seeds.insert(id);
        }
        coverage.reasons.sort();
        Ok((seeds.into_iter().collect(), coverage))
    }

    /// Reverse BFS over the supplied causal-kind policy. Only Selected nodes
    /// expand. Unknown sources are inspection leads; non-selected revisions do
    /// not become current impact. Direct and transitive consumers must use the
    /// same caller admission rule. A two-second deadline is cooperative between
    /// reads/callbacks, never a preemptive deadline for an external callback.
    pub fn bounded_incoming_walk(
        &self,
        project_id: &str,
        seeds: &[String],
        max_depth: usize,
        causal: &dyn Fn(&EdgeKind) -> bool,
        admit: &dyn Fn(&Node, Option<&Node>) -> anyhow::Result<ImpactAdmission>,
    ) -> anyhow::Result<IncomingImpactWalk> {
        self.bounded_incoming_walk_excluding_file(project_id, seeds, max_depth, causal, admit, None)
    }

    /// File-level impact treats the requested file's own declarations as the
    /// component, including declarations omitted from a bounded seed sample.
    /// Decode their rows under the shared work budget, then skip them before
    /// accepted-node, witness, frontier and per-endpoint result budgets.
    pub fn bounded_incoming_walk_excluding_file(
        &self,
        project_id: &str,
        seeds: &[String],
        max_depth: usize,
        causal: &dyn Fn(&EdgeKind) -> bool,
        admit: &dyn Fn(&Node, Option<&Node>) -> anyhow::Result<ImpactAdmission>,
        excluded_source_file: Option<&str>,
    ) -> anyhow::Result<IncomingImpactWalk> {
        anyhow::ensure!(
            excluded_source_file.is_none_or(|path| !path.is_empty() && path.len() <= 2048),
            "Invalid internal impact file boundary"
        );
        anyhow::ensure!(
            !project_id.is_empty() && project_id.len() <= 1024,
            "Impact project identity must be 1..1024 bytes"
        );
        anyhow::ensure!(
            !seeds.is_empty() && seeds.len() <= MAX_SEEDS,
            "Impact walk requires 1..128 seed identities; disclose any caller-side seed omission"
        );
        anyhow::ensure!(
            (1..=MAX_DEPTH).contains(&max_depth),
            "Impact depth must be 1..8"
        );
        for identity in std::iter::once(project_id).chain(seeds.iter().map(String::as_str)) {
            anyhow::ensure!(
                !identity.is_empty()
                    && identity.len() <= 2048
                    && !identity.contains(['\0', '\n', '\r']),
                "Impact identity exceeds key limits"
            );
        }
        let mut output = IncomingImpactWalk::default();
        let mut budget = Budget {
            started: Instant::now(),
            string_bytes: 0,
            decode_bytes: 0,
            halted: false,
        };
        // All node, file-node, edge and adjacency observations share this tx.
        let tx = self.db.begin_read()?;
        let nodes = tx.open_table(NODES)?;
        let edges = tx.open_table(EDGES)?;
        let incoming = tx.open_table(ADJ_IN)?;
        let read_node = |id: &str,
                         budget: &mut Budget,
                         coverage: &mut ImpactWalkCoverage|
         -> anyhow::Result<Option<Node>> {
            if !budget.live(coverage) {
                return Ok(None);
            }
            let key = format!("{project_id}\0{id}");
            let Some(value) = nodes.get(key.as_str())? else {
                coverage.missing_nodes += 1;
                gap(coverage, "missing_graph_node");
                return Ok(None);
            };
            if !budget.record(value.value().len(), coverage) {
                return Ok(None);
            }
            let node: Node = bincode::DefaultOptions::new()
                .with_fixint_encoding()
                .with_limit(MAX_RECORD_BYTES as u64)
                .reject_trailing_bytes()
                .deserialize(value.value())?;
            anyhow::ensure!(
                node.node_id == id,
                "Graph node identity disagrees with its key"
            );
            Ok(Some(node))
        };
        let classify = |node: &Node,
                        budget: &mut Budget,
                        coverage: &mut ImpactWalkCoverage,
                        frontier: &mut Vec<ImpactFrontier>,
                        witness_count: usize,
                        depth: usize|
         -> anyhow::Result<ImpactAdmission> {
            let missing_before = coverage.missing_nodes;
            let file = if node.file_path.as_str().is_empty() {
                None
            } else {
                read_node(
                    &format!("file:{}", node.file_path.as_str()),
                    budget,
                    coverage,
                )?
            };
            if file.is_none() && !node.file_path.as_str().is_empty() {
                let reason = if coverage.missing_nodes > missing_before {
                    "missing_file_node"
                } else {
                    "file_record_unavailable_within_budget"
                };
                record_frontier(
                    frontier,
                    coverage,
                    budget,
                    witness_count,
                    &format!("file:{}", node.file_path.as_str()),
                    Some(&node.node_id),
                    None,
                    None,
                    depth,
                    reason,
                );
            }
            let admission = admit(node, file.as_ref())?;
            budget.live(coverage);
            Ok(admission)
        };
        let mut cache = HashMap::new();
        let mut queue = VecDeque::new();
        let mut ordered_seeds: Vec<_> = seeds.iter().collect();
        ordered_seeds.sort();
        ordered_seeds.dedup();
        for seed in ordered_seeds {
            if !budget.live(&mut output.coverage) {
                break;
            }
            let missing_before = output.coverage.missing_nodes;
            let Some(node) = read_node(seed, &mut budget, &mut output.coverage)? else {
                let reason = if output.coverage.missing_nodes > missing_before {
                    "missing_graph_node"
                } else {
                    "seed_record_unavailable_within_budget"
                };
                record_frontier(
                    &mut output.frontier,
                    &mut output.coverage,
                    &mut budget,
                    output.witnesses.len(),
                    seed,
                    None,
                    None,
                    None,
                    0,
                    reason,
                );
                continue;
            };
            let admission = classify(
                &node,
                &mut budget,
                &mut output.coverage,
                &mut output.frontier,
                output.witnesses.len(),
                0,
            )?;
            if admission != ImpactAdmission::Selected {
                record_frontier(
                    &mut output.frontier,
                    &mut output.coverage,
                    &mut budget,
                    output.witnesses.len(),
                    seed,
                    None,
                    None,
                    Some(&node),
                    0,
                    if admission == ImpactAdmission::NonSelectedRevision {
                        "non_selected_seed_revision"
                    } else {
                        "unknown_seed_admission"
                    },
                );
            }
            if !budget.live(&mut output.coverage) {
                break;
            }
            if let Some(index) = remember(
                &node,
                admission,
                0,
                None,
                &mut cache,
                &mut output,
                &mut budget,
            ) {
                if admission == ImpactAdmission::Selected {
                    queue.push_back(index);
                }
            }
        }
        'walk: while let Some(index) = queue.pop_front() {
            if !budget.live(&mut output.coverage) {
                break;
            }
            let depth = output.nodes[index].depth;
            // A transient endpoint copy is bounded by its admitted identity.
            let target = output.nodes[index].node.node_id.clone();
            for kind in EdgeKind::ALL {
                if !causal(kind) {
                    continue;
                }
                let prefix = adj_key(project_id, kind, &target);
                let mut accepted = 0usize;
                let mut entries = incoming.range((prefix.as_str(), "")..)?;
                loop {
                    if !budget.live(&mut output.coverage) {
                        break 'walk;
                    }
                    if output.coverage.raw_rows == MAX_RAW_ROWS {
                        gap(&mut output.coverage, "raw_adjacency_row_limit");
                        break 'walk;
                    }
                    let Some(entry) = entries.next() else {
                        break;
                    };
                    // Charge raw work BEFORE prefix, cycle, revision or policy skips.
                    output.coverage.raw_rows += 1;
                    let (key, value) = entry?;
                    let (observed_prefix, source) = key.value();
                    if observed_prefix != prefix {
                        break;
                    }
                    anyhow::ensure!(
                        source.len() <= 2048 && !source.contains(['\0', '\n', '\r']),
                        "Graph adjacency source exceeds impact key limits"
                    );
                    let _ = decode_adj_value(value.value())?;
                    let edge_id = edge_key(project_id, kind, source, &target);
                    let Some(raw) = edges.get(edge_id.as_str())? else {
                        output.coverage.unknown_nodes += 1;
                        gap(&mut output.coverage, "missing_graph_edge");
                        record_frontier(
                            &mut output.frontier,
                            &mut output.coverage,
                            &mut budget,
                            output.witnesses.len(),
                            source,
                            Some(&target),
                            Some(kind),
                            None,
                            depth + 1,
                            "missing_graph_edge",
                        );
                        continue;
                    };
                    if !budget.record(raw.value().len(), &mut output.coverage) {
                        record_frontier(
                            &mut output.frontier,
                            &mut output.coverage,
                            &mut budget,
                            output.witnesses.len(),
                            source,
                            Some(&target),
                            Some(kind),
                            None,
                            depth + 1,
                            "edge_record_unavailable_within_budget",
                        );
                        if budget.halted {
                            break 'walk;
                        }
                        continue;
                    }
                    let edge: Edge = bincode::DefaultOptions::new()
                        .with_fixint_encoding()
                        .with_limit(MAX_RECORD_BYTES as u64)
                        .reject_trailing_bytes()
                        .deserialize(raw.value())?;
                    anyhow::ensure!(
                        edge.source_id == source
                            && edge.target_id == target
                            && edge.edge_kind == *kind,
                        "Graph edge identity disagrees with its adjacency key"
                    );
                    let loaded = if cache.contains_key(source) {
                        None
                    } else {
                        let missing_before = output.coverage.missing_nodes;
                        let loaded = read_node(source, &mut budget, &mut output.coverage)?;
                        if loaded.is_none() {
                            let reason = if output.coverage.missing_nodes > missing_before {
                                "missing_graph_node"
                            } else {
                                "source_record_unavailable_within_budget"
                            };
                            record_frontier(
                                &mut output.frontier,
                                &mut output.coverage,
                                &mut budget,
                                output.witnesses.len(),
                                source,
                                Some(&target),
                                Some(kind),
                                None,
                                depth + 1,
                                reason,
                            );
                            if budget.halted {
                                break 'walk;
                            }
                            continue;
                        }
                        loaded
                    };
                    let node = cache
                        .get(source)
                        .map(|cached| &cached.node)
                        .or(loaded.as_ref())
                        .expect("cached or freshly read source");
                    if excluded_source_file.is_some_and(|path| node.file_path.as_str() == path) {
                        output.coverage.internal_rows_skipped += 1;
                        continue;
                    }
                    if edge.namespace != engram_core::namespaces::NAMESPACE_MEMORY {
                        output.coverage.unknown_nodes += 1;
                        gap(&mut output.coverage, "unverified_edge_source_namespace");
                        record_frontier(
                            &mut output.frontier,
                            &mut output.coverage,
                            &mut budget,
                            output.witnesses.len(),
                            source,
                            Some(&target),
                            Some(kind),
                            Some(node),
                            depth + 1,
                            "unverified_edge_source_namespace",
                        );
                        continue;
                    }
                    if loaded.is_some() && cache.len() >= MAX_NODES {
                        gap(&mut output.coverage, "visited_node_limit");
                        break 'walk;
                    }
                    // Do not mark visited or invoke source admission on a stale
                    // edge. A later correctly bound edge may still admit source.
                    if !edge_source_revision_matches(&edge, node) {
                        output.coverage.unknown_nodes += 1;
                        gap(&mut output.coverage, "unverified_edge_source_revision");
                        record_frontier(
                            &mut output.frontier,
                            &mut output.coverage,
                            &mut budget,
                            output.witnesses.len(),
                            source,
                            Some(&target),
                            Some(kind),
                            Some(node),
                            depth + 1,
                            "unverified_edge_source_revision",
                        );
                        continue;
                    }
                    let cached = cache.get(source);
                    if let Some(cached) = cached {
                        output.coverage.cycles_or_revisits += 1;
                        if cached.admission != ImpactAdmission::Selected {
                            record_frontier(
                                &mut output.frontier,
                                &mut output.coverage,
                                &mut budget,
                                output.witnesses.len(),
                                source,
                                Some(&target),
                                Some(kind),
                                Some(node),
                                depth + 1,
                                if cached.admission == ImpactAdmission::NonSelectedRevision {
                                    "non_selected_source_revision"
                                } else {
                                    "unknown_source_admission"
                                },
                            );
                            continue;
                        }
                    }
                    let admission = match cached {
                        Some(cached) => cached.admission,
                        None => classify(
                            node,
                            &mut budget,
                            &mut output.coverage,
                            &mut output.frontier,
                            output.witnesses.len(),
                            depth + 1,
                        )?,
                    };
                    if !budget.live(&mut output.coverage) {
                        break 'walk;
                    }
                    if admission == ImpactAdmission::NonSelectedRevision {
                        record_frontier(
                            &mut output.frontier,
                            &mut output.coverage,
                            &mut budget,
                            output.witnesses.len(),
                            source,
                            Some(&target),
                            Some(kind),
                            Some(node),
                            depth + 1,
                            "non_selected_source_revision",
                        );
                        if cached.is_none() {
                            remember(
                                loaded.as_ref().expect("fresh source"),
                                admission,
                                depth + 1,
                                None,
                                &mut cache,
                                &mut output,
                                &mut budget,
                            );
                        }
                        continue;
                    }
                    if admission == ImpactAdmission::Unknown {
                        record_frontier(
                            &mut output.frontier,
                            &mut output.coverage,
                            &mut budget,
                            output.witnesses.len(),
                            source,
                            Some(&target),
                            Some(kind),
                            Some(node),
                            depth + 1,
                            "unknown_source_admission",
                        );
                        if cached.is_none() {
                            remember(
                                loaded.as_ref().expect("fresh source"),
                                admission,
                                depth + 1,
                                Some((&target, kind, edge.generation)),
                                &mut cache,
                                &mut output,
                                &mut budget,
                            );
                        }
                        continue;
                    }
                    if depth == max_depth && cached.is_none() {
                        gap(&mut output.coverage, "depth_limit");
                        record_frontier(
                            &mut output.frontier,
                            &mut output.coverage,
                            &mut budget,
                            output.witnesses.len(),
                            source,
                            Some(&target),
                            Some(kind),
                            Some(node),
                            depth + 1,
                            "depth_limit",
                        );
                        continue;
                    }
                    if accepted == MAX_PER_ENDPOINT_KIND {
                        gap(&mut output.coverage, "per_endpoint_kind_limit");
                        break;
                    }
                    accepted += 1;
                    if output.witnesses.len() + output.frontier.len() == MAX_WITNESSES {
                        gap(&mut output.coverage, "witness_edge_limit");
                        break 'walk;
                    }
                    if !budget.retain(source.len() + target.len(), &mut output.coverage) {
                        break 'walk;
                    }
                    if cached.is_none() {
                        let Some(index) = remember(
                            loaded.as_ref().expect("fresh source"),
                            admission,
                            depth + 1,
                            Some((&target, kind, edge.generation)),
                            &mut cache,
                            &mut output,
                            &mut budget,
                        ) else {
                            if budget.halted {
                                break 'walk;
                            }
                            continue;
                        };
                        if admission == ImpactAdmission::Selected {
                            queue.push_back(index);
                        }
                    }
                    output.coverage.max_depth_reached =
                        output.coverage.max_depth_reached.max(depth + 1);
                    output.witnesses.push(ImpactWitness {
                        source_id: source.to_owned(),
                        target_id: target.clone(),
                        kind: kind.clone(),
                        edge_generation: edge.generation,
                        weight: edge.weight,
                        depth: depth + 1,
                        source_line: recorded_source_line(&edge),
                    });
                }
            }
        }
        output.coverage.reasons.sort();
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROJECT: &str = "incoming-impact";

    fn owned_case(case: impl FnOnce(&GraphStore) -> anyhow::Result<()>) -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().to_path_buf();
        let result = {
            let store = GraphStore::open(&directory.path().join("graph.redb"))?;
            case(&store)
        }; // The database and all read transactions close before owned teardown.
        let cleanup = directory.close();
        match (result, cleanup) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) => Err(error),
            (result, Err(cleanup)) => Err(anyhow::anyhow!(
                "Owned graph fixture retained at {}: {cleanup}; behavior result: {result:?}",
                path.display()
            )),
        }
    }

    fn node(id: &str, path: &str, generation: u64) -> Node {
        Node {
            node_id: id.into(),
            node_type: "function".into(),
            name: id.into(),
            namespace: engram_core::namespaces::NAMESPACE_MEMORY.into(),
            language: "vb".into(),
            file_path: RelPath::new(path),
            start_line: 10,
            end_line: 20,
            generation,
            metadata: Some(serde_json::json!({"source_identity": "fixture"})),
        }
    }

    fn file(path: &str, generation: u64) -> Node {
        let mut node = node(&format!("file:{path}"), path, generation);
        node.node_type = "file".into();
        node
    }

    fn edge(source: &str, target: &str, kind: EdgeKind, generation: u64) -> Edge {
        Edge {
            source_id: source.into(),
            target_id: target.into(),
            namespace: engram_core::namespaces::NAMESPACE_MEMORY.into(),
            language: "vb".into(),
            edge_kind: kind,
            weight: 1,
            generation,
            metadata: None,
            updated_at_ms: 1,
        }
    }

    fn causal(kind: &EdgeKind) -> bool {
        matches!(kind, EdgeKind::Calls | EdgeKind::Dependency)
    }

    // An independent source-selection oracle: the caller owns the selected
    // per-file physical revision. The graph walk must keep these revisions.
    fn selected(
        expected: &[(&str, u64)],
        node: &Node,
        file: Option<&Node>,
    ) -> anyhow::Result<ImpactAdmission> {
        // Admission needs the original metadata, not the response projection.
        assert_eq!(
            node.metadata.as_ref().unwrap()["source_identity"],
            "fixture"
        );
        let Some(file) = file else {
            return Ok(ImpactAdmission::Unknown);
        };
        assert_eq!(
            file.metadata.as_ref().unwrap()["source_identity"],
            "fixture"
        );
        let Some((_, generation)) = expected
            .iter()
            .find(|(path, _)| *path == node.file_path.as_str())
        else {
            return Ok(ImpactAdmission::Unknown);
        };
        Ok(
            if node.generation == *generation
                && file.generation == *generation
                && file.node_id == format!("file:{}", node.file_path.as_str())
            {
                ImpactAdmission::Selected
            } else {
                ImpactAdmission::NonSelectedRevision
            },
        )
    }

    #[test]
    fn persisted_incoming_chain_retains_mixed_source_revisions_and_exact_witnesses()
    -> anyhow::Result<()> {
        owned_case(|store| {
            // The route is present as actual graph edges, including the cache
            // intermediary. No inferred HTTP/UI link is invented by traversal.
            let identities = [
                ("ClassifyAutomatic", "policy.vb", 2),
                ("Resolve", "policy.vb", 2),
                ("ResolvePolicy", "spatial.vb", 1),
                ("QueryCacheCore", "spatial.vb", 1),
                ("QueryCache", "spatial.vb", 1),
                ("QueryViewport", "spatial.vb", 1),
                ("GetFileLayerViewport", "viewport.aspx.vb", 3),
            ];
            let mut nodes: Vec<_> = identities
                .iter()
                .map(|(id, path, generation)| node(id, path, *generation))
                .collect();
            nodes.extend([
                file("policy.vb", 2),
                file("spatial.vb", 1),
                file("viewport.aspx.vb", 3),
            ]);
            store.upsert_nodes(PROJECT, &nodes)?;
            let edges: Vec<_> = identities
                .windows(2)
                .map(|pair| edge(pair[1].0, pair[0].0, EdgeKind::Calls, pair[1].2))
                .collect();
            store.upsert_edges(PROJECT, &edges)?;
            let expected = [("policy.vb", 2), ("spatial.vb", 1), ("viewport.aspx.vb", 3)];
            let walk = store.bounded_incoming_walk(
                PROJECT,
                &["ClassifyAutomatic".into()],
                6,
                &causal,
                &|node, file| selected(&expected, node, file),
            )?;
            assert!(!walk.coverage.truncated, "{:?}", walk.coverage);
            assert_eq!(walk.nodes.len(), 7);
            assert_eq!(walk.witnesses.len(), 6);
            let mut current = "GetFileLayerViewport";
            for depth in (0..=6).rev() {
                let observed = walk
                    .nodes
                    .iter()
                    .find(|entry| entry.node.node_id == current)
                    .unwrap();
                assert_eq!(observed.depth, depth);
                assert_eq!(observed.admission, ImpactAdmission::Selected);
                assert!(observed.node.metadata.is_none());
                assert_eq!(observed.node.generation, identities[depth].2);
                if depth == 0 {
                    assert!(observed.predecessor.is_none());
                    assert_eq!(current, "ClassifyAutomatic");
                } else {
                    assert_eq!(observed.edge_generation, Some(identities[depth].2));
                    assert_eq!(observed.via_kind, Some(EdgeKind::Calls));
                    assert_eq!(
                        observed.predecessor.as_deref(),
                        Some(identities[depth - 1].0)
                    );
                    assert!(
                        walk.witnesses
                            .iter()
                            .any(|witness| witness.source_id == current
                                && witness.target_id == identities[depth - 1].0
                                && witness.kind == EdgeKind::Calls
                                && witness.edge_generation == identities[depth].2
                                && witness.depth == depth)
                    );
                    current = identities[depth - 1].0;
                }
            }
            Ok(())
        })
    }

    #[test]
    fn historical_neighbors_and_cycles_do_not_become_transitive_callers() -> anyhow::Result<()> {
        owned_case(|store| {
            store.upsert_nodes(
                PROJECT,
                &[
                    node("target", "live.vb", 1),
                    node("caller", "live.vb", 1),
                    file("live.vb", 1),
                ],
            )?;
            let mut edges = vec![
                edge("caller", "target", EdgeKind::Calls, 1),
                edge("target", "caller", EdgeKind::Calls, 1),
            ];
            for kind in [
                EdgeKind::CoOccurrence,
                EdgeKind::TemporalCoupling,
                EdgeKind::Contains,
            ] {
                let mut historical = edge("historical-only", "target", kind, 99);
                historical.weight = u32::MAX;
                edges.push(historical);
            }
            store.upsert_edges(PROJECT, &edges)?;
            let walk = store.bounded_incoming_walk(
                PROJECT,
                &["target".into()],
                1,
                &causal,
                &|node, file| selected(&[("live.vb", 1)], node, file),
            )?;
            assert_eq!(
                walk.nodes
                    .iter()
                    .map(|entry| entry.node.node_id.as_str())
                    .collect::<Vec<_>>(),
                ["target", "caller"]
            );
            assert_eq!(
                walk.witnesses.len(),
                2,
                "Both verified connections survive; the cycle is never expanded twice"
            );
            assert!(
                walk.witnesses
                    .iter()
                    .all(|edge| edge.kind == EdgeKind::Calls)
            );
            assert!(walk.coverage.cycles_or_revisits > 0);
            assert!(
                !walk.coverage.truncated,
                "A back edge alone is not a depth frontier: {:?}",
                walk.coverage
            );
            Ok(())
        })
    }

    #[test]
    fn actual_callee_remap_preserves_inherited_caller_but_source_refresh_removes_old_call()
    -> anyhow::Result<()> {
        owned_case(|store| {
            let mut original = node("callee-line-10", "callee.vb", 1);
            original.name = "Resolve".into();
            let mut unproven = edge("unproven-caller", "callee-line-10", EdgeKind::Calls, 0);
            // A provider-like JSON string is not typed producer evidence.
            unproven.metadata = Some(JsonValue::Object(serde_json::Map::from_iter([(
                SOURCE_ORIGIN.to_owned(),
                JsonValue::String(
                    "{\"version\":1,\"source_id\":\"unproven-caller\",\"source_generation\":1}"
                        .into(),
                ),
            )])));
            let mut external = edge("external-caller", "callee-line-10", EdgeKind::Dependency, 1);
            external.namespace = "knowledge".into();
            store.upsert_nodes_and_edges(
                PROJECT,
                &[
                    original,
                    file("callee.vb", 1),
                    node("caller", "caller.vb", 1),
                    node("unproven-caller", "caller.vb", 1),
                    node("external-caller", "caller.vb", 1),
                    file("caller.vb", 1),
                ],
                &[
                    edge("caller", "callee-line-10", EdgeKind::Calls, 1),
                    unproven,
                    external,
                ],
            )?;

            let changed_callee_paths = std::collections::HashSet::from(["callee.vb".to_owned()]);
            let mut target = "callee-line-10".to_owned();
            // A second actual move exercises carrying the first valid origin,
            // rather than re-attesting a logical generation as a source one.
            for generation in [2, 3] {
                let next = format!("callee-line-{}", generation * 10);
                let mut moved = node(&next, "callee.vb", generation);
                moved.name = "Resolve".into();
                moved.start_line = generation as u32 * 10;
                moved.end_line = moved.start_line + 10;
                store.upsert_nodes(PROJECT, &[moved, file("callee.vb", generation)])?;
                store.purge_stale_nodes_for_paths(PROJECT, &changed_callee_paths, generation)?;
                assert!(store.get_node(PROJECT, &target)?.is_none());
                assert!(
                    store
                        .get_edges_by_endpoints(
                            PROJECT,
                            &[("caller".into(), EdgeKind::Calls, target.clone())]
                        )?
                        .is_empty()
                );
                let remapped = store.get_edges_by_endpoints(
                    PROJECT,
                    &[("caller".into(), EdgeKind::Calls, next.clone())],
                )?;
                assert_eq!(remapped.len(), 1);
                assert_eq!(
                    remapped[0].generation, generation,
                    "Logical cleanup generation stays unchanged"
                );
                assert_eq!(
                    remapped[0].metadata.as_ref().unwrap()[SOURCE_ORIGIN],
                    serde_json::json!({
                        "version": 1, "source_id": "caller", "source_generation": 1,
                    })
                );
                let walk = store.bounded_incoming_walk(
                    PROJECT,
                    &[next.clone()],
                    3,
                    &causal,
                    &|node, file| {
                        selected(&[("callee.vb", generation), ("caller.vb", 1)], node, file)
                    },
                )?;
                let caller = walk
                    .nodes
                    .iter()
                    .find(|entry| entry.node.node_id == "caller")
                    .unwrap();
                assert_eq!(caller.node.generation, 1);
                assert_eq!(caller.edge_generation, Some(generation));
                assert_eq!(caller.depth, 1);
                assert_eq!(caller.predecessor.as_deref(), Some(next.as_str()));
                assert!(
                    !walk
                        .nodes
                        .iter()
                        .any(|entry| entry.node.node_id == "unproven-caller"
                            || entry.node.node_id == "external-caller")
                );
                assert!(
                    walk.coverage
                        .reasons
                        .iter()
                        .any(|reason| reason == "unverified_edge_source_revision")
                );
                assert!(
                    walk.coverage
                        .reasons
                        .iter()
                        .any(|reason| reason == "unverified_edge_source_namespace")
                );
                target = next;
            }

            // Re-extract the caller at a stable node ID, now without any call.
            // Its old remapped edge must not be resurrected from origin data.
            store.upsert_nodes(
                PROJECT,
                &[node("caller", "caller.vb", 4), file("caller.vb", 4)],
            )?;
            store.purge_stale_nodes_for_paths(
                PROJECT,
                &std::collections::HashSet::from(["caller.vb".to_owned()]),
                4,
            )?;
            assert!(
                store
                    .get_edges_by_endpoints(
                        PROJECT,
                        &[("caller".into(), EdgeKind::Calls, target.clone())]
                    )?
                    .is_empty()
            );
            let walk =
                store.bounded_incoming_walk(PROJECT, &[target], 3, &causal, &|node, file| {
                    selected(&[("callee.vb", 3), ("caller.vb", 4)], node, file)
                })?;
            assert!(
                !walk
                    .nodes
                    .iter()
                    .any(|entry| entry.node.node_id == "caller")
            );
            assert!(walk.witnesses.is_empty());
            Ok(())
        })
    }

    #[test]
    fn remapped_origin_cannot_be_reauthorized_when_caller_catches_up_to_logical_generation()
    -> anyhow::Result<()> {
        owned_case(|store| {
            let mut original = node("callee-10", "callee.vb", 1);
            original.name = "Resolve".into();
            store.upsert_nodes_and_edges(
                PROJECT,
                &[
                    original,
                    file("callee.vb", 1),
                    file("caller.vb", 1),
                    node("caller", "caller.vb", 1),
                    node("unattested", "caller.vb", 1),
                ],
                &[
                    edge("caller", "callee-10", EdgeKind::Calls, 1),
                    edge("unattested", "callee-10", EdgeKind::Calls, 0),
                ],
            )?;
            let callee_paths = std::collections::HashSet::from(["callee.vb".to_owned()]);
            let caller_paths = std::collections::HashSet::from(["caller.vb".to_owned()]);
            let mut previous_target = "callee-10".to_owned();

            for generation in [2, 3] {
                let target = format!("callee-{}", generation * 10);
                let mut moved = node(&target, "callee.vb", generation);
                moved.name = "Resolve".into();
                store.upsert_nodes(PROJECT, &[moved, file("callee.vb", generation)])?;
                store.purge_stale_nodes_for_paths(PROJECT, &callee_paths, generation)?;
                assert!(store.get_node(PROJECT, &previous_target)?.is_none());

                // The caller is then refreshed without a call, at the SAME
                // logical generation as the existing target remap. The old
                // cleanup guard skips e.generation >= active_generation.
                store.upsert_nodes(
                    PROJECT,
                    &[
                        node("caller", "caller.vb", generation),
                        node("unattested", "caller.vb", generation),
                        file("caller.vb", generation),
                    ],
                )?;
                store.purge_stale_nodes_for_paths(PROJECT, &caller_paths, generation)?;
                let edges = store.get_edges_by_endpoints(
                    PROJECT,
                    &[
                        ("caller".into(), EdgeKind::Calls, target.clone()),
                        ("unattested".into(), EdgeKind::Calls, target.clone()),
                    ],
                )?;
                assert_eq!(
                    edges.len(),
                    2,
                    "Exercise existing persisted rows, not absent-edge success"
                );
                assert!(edges.iter().all(|edge| edge.generation == generation));
                let bound = edges
                    .iter()
                    .find(|edge| edge.source_id == "caller")
                    .unwrap();
                assert_eq!(
                    bound.metadata.as_ref().unwrap()[SOURCE_ORIGIN],
                    serde_json::json!({
                        "version": 1, "source_id": "caller", "source_generation": 1,
                    }),
                    "A mismatched origin must never be re-attested from generation equality"
                );
                let unbound = edges
                    .iter()
                    .find(|edge| edge.source_id == "unattested")
                    .unwrap();
                let metadata = unbound.metadata.as_ref().unwrap().as_object().unwrap();
                assert!(metadata.contains_key(SOURCE_ORIGIN));
                assert!(
                    metadata[SOURCE_ORIGIN].is_null(),
                    "An unproven remap needs a durable invalid marker"
                );
                let walk = store.bounded_incoming_walk(
                    PROJECT,
                    &[target.clone()],
                    2,
                    &causal,
                    &|node, file| {
                        selected(
                            &[("callee.vb", generation), ("caller.vb", generation)],
                            node,
                            file,
                        )
                    },
                )?;
                assert_eq!(
                    walk.nodes.len(),
                    1,
                    "Neither stale caller is a current dependency"
                );
                assert_eq!(walk.nodes[0].node.node_id, target);
                assert!(walk.witnesses.is_empty());
                assert!(walk.coverage.unknown_nodes >= 2);
                assert!(
                    walk.coverage
                        .reasons
                        .iter()
                        .any(|reason| reason == "unverified_edge_source_revision")
                );
                previous_target = target;
            }
            Ok(())
        })
    }

    #[test]
    fn stale_edges_do_not_poison_later_valid_paths_and_bounded_unknowns_are_explicit()
    -> anyhow::Result<()> {
        owned_case(|store| {
            let mut nodes = vec![
                node("target", "live.vb", 1),
                file("live.vb", 1),
                node("a-unknown", "missing-file.vb", 1),
                node("b-future", "live.vb", 2),
                node("d-valid-later", "live.vb", 1),
                node("beyond-selected-depth", "live.vb", 1),
                node("behind-unknown", "live.vb", 1),
            ];
            let mut edges = vec![
                edge("a-unknown", "target", EdgeKind::Calls, 1),
                edge("b-future", "target", EdgeKind::Calls, 2),
                edge("c-missing-node", "target", EdgeKind::Calls, 1),
                edge("d-valid-later", "target", EdgeKind::Calls, 9),
                edge("d-valid-later", "target", EdgeKind::Dependency, 1),
                edge("beyond-selected-depth", "d-valid-later", EdgeKind::Calls, 1),
                edge("behind-unknown", "a-unknown", EdgeKind::Calls, 1),
            ];
            for number in 0..75 {
                let id = format!("z-wide-{number:03}");
                nodes.push(node(&id, "live.vb", 1));
                edges.push(edge(&id, "target", EdgeKind::Calls, 1));
            }
            store.upsert_nodes(PROJECT, &nodes)?;
            store.upsert_edges(PROJECT, &edges)?;
            let walk = store.bounded_incoming_walk(
                PROJECT,
                &["target".into()],
                1,
                &causal,
                &|node, file| selected(&[("live.vb", 1)], node, file),
            )?;
            let later = walk
                .nodes
                .iter()
                .find(|entry| entry.node.node_id == "d-valid-later")
                .unwrap();
            assert_eq!(later.admission, ImpactAdmission::Selected);
            assert_eq!(later.via_kind, Some(EdgeKind::Dependency));
            assert_eq!(later.edge_generation, Some(1));
            assert_eq!(later.predecessor.as_deref(), Some("target"));
            assert!(walk.witnesses.iter().all(|edge| edge.edge_generation != 9));
            assert_eq!(
                walk.nodes
                    .iter()
                    .find(|entry| entry.node.node_id == "a-unknown")
                    .unwrap()
                    .admission,
                ImpactAdmission::Unknown
            );
            for excluded in [
                "b-future",
                "c-missing-node",
                "behind-unknown",
                "beyond-selected-depth",
            ] {
                assert!(
                    !walk
                        .nodes
                        .iter()
                        .any(|entry| entry.node.node_id == excluded),
                    "Unexpected caller: {excluded}"
                );
            }
            assert_eq!(walk.coverage.non_selected_nodes, 1);
            assert!(walk.coverage.missing_nodes >= 2);
            assert!(walk.coverage.unknown_nodes >= 2);
            assert!(walk.coverage.truncated);
            for reason in [
                "unverified_edge_source_revision",
                "unknown_source_admission",
                "per_endpoint_kind_limit",
                "depth_limit",
            ] {
                assert!(
                    walk.coverage.reasons.iter().any(|actual| actual == reason),
                    "Missing {reason}: {:?}",
                    walk.coverage
                );
            }
            assert!(walk.coverage.raw_rows <= MAX_RAW_ROWS);
            assert!(walk.nodes.len() <= MAX_NODES);
            assert!(walk.witnesses.len() <= MAX_WITNESSES);

            // A short identity with a large persisted display name must not
            // bypass the retained-string cap or masquerade as absent callers.
            let mut large = node("large-description", "live.vb", 1);
            large.name = "n".repeat(140 * 1024);
            store.upsert_nodes(PROJECT, &[large])?;
            let bounded = store.bounded_incoming_walk(
                PROJECT,
                &["large-description".into()],
                1,
                &causal,
                &|node, file| selected(&[("live.vb", 1)], node, file),
            )?;
            assert!(bounded.nodes.is_empty() && bounded.witnesses.is_empty());
            assert!(bounded.coverage.truncated);
            assert!(
                bounded
                    .coverage
                    .reasons
                    .iter()
                    .any(|reason| reason == "retained_string_byte_limit")
            );
            assert_eq!(
                store
                    .get_node(PROJECT, "large-description")?
                    .unwrap()
                    .name
                    .len(),
                140 * 1024
            );
            Ok(())
        })
    }
}
