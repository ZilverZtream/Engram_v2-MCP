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

/// Validate managed event extraction ownership; strings cannot mint this proof.
pub fn event_producer(edge: &Edge) -> anyhow::Result<Option<(&str,u64,&str)>> {
    let Some(value)=edge.metadata.as_ref().and_then(|m|m.get("_engram_event_producer")) else { return Ok(None) };
    let object=value.as_object().ok_or_else(||anyhow::anyhow!("Invalid event producer metadata"))?;
    let path=object.get("path").and_then(|v|v.as_str()).unwrap_or("");
    let generation=object.get("generation").and_then(|v|v.as_u64()).unwrap_or(0);
    let owner=object.get("owner_class").and_then(|v|v.as_str()).unwrap_or("");
    anyhow::ensure!(object.len()==4 && object.get("version").and_then(|v|v.as_u64())==Some(1)
        && generation>0 && generation<=edge.generation && !path.is_empty() && path.len()<=2048
        && owner.len()<=2048 && !owner.chars().any(char::is_control)
        && !path.contains('\\') && !path.contains(':') && !path.starts_with('/')
        && path.split('/').all(|p| !matches!(p,""|"."|"..")),"Invalid event producer metadata");
    Ok(Some((path,generation,owner)))
}

/// Whether a current markup declaration still selects the exact producer class.
pub fn event_owner_matches(source: &Node, path: &str, class: &str) -> bool {
    let Some(owner)=source.metadata.as_ref().and_then(|m|m.get("_engram_webforms_owner")).and_then(JsonValue::as_object) else { return false };
    owner.len()==2 && !class.is_empty() && owner.get("path").and_then(JsonValue::as_str)==Some(path)
        && owner.get("class").and_then(JsonValue::as_str).is_some_and(|actual|actual.eq_ignore_ascii_case(class))
}

pub(super) fn rebind_event_control(edge: &mut Edge, source: &Node) -> anyhow::Result<()> {
    if let Some(origin)=edge.metadata.as_ref().and_then(|m|m.get(SOURCE_ORIGIN)) {
        anyhow::ensure!(origin.as_object().is_some_and(|o|o.len()==3)
            && origin.get("version").and_then(JsonValue::as_u64)==Some(1)
            && origin.get("source_id").and_then(JsonValue::as_str)==Some(source.node_id.as_str())
            && origin.get("source_generation").and_then(JsonValue::as_u64).is_some_and(|g|g>0 && g<source.generation),
            "Unverified previous event source origin");
    }
    edge.generation=source.generation;
    edge.metadata.as_mut().and_then(JsonValue::as_object_mut).expect("typed event metadata")
        .insert(SOURCE_ORIGIN.into(),serde_json::json!({"version":1,"source_id":source.node_id,"source_generation":source.generation}));
    Ok(())
}

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
    /// Preferred recorded position; its strength is explicit, never inferred
    /// from the owning declaration's bounds.
    pub source_line: Option<u32>,
    pub source_position_kind: &'static str,
    pub extractor_anchor_line: Option<u32>,
    /// Bounded, sorted call/reference syntax samples, not every call in the source.
    pub reference_lines: Vec<u32>,
    pub reference_lines_truncated: bool,
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
    /// Separate potential dispatch routes, never added to causal BFS counts.
    pub interface_dispatch: InterfaceDispatch,
    /// Same-terminal unresolved syntax for inspection only. Never a causal
    /// caller tier, path, receiver binding, or addition to primary counts.
    pub unbound_member_inspection: Option<UnboundMemberInspection>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnboundMemberWitness {
    pub caller: Node,
    pub unresolved_target_id: String,
    pub receiver: String,
    pub edge_generation: u64,
    pub source_position_kind: &'static str,
    pub reference_lines: Vec<u32>,
    pub reference_lines_truncated: bool,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct UnboundMemberInspection {
    pub directory: String,
    pub member_name: String,
    pub witnesses: Vec<UnboundMemberWitness>,
    pub coverage: ImpactWalkCoverage,
    pub decoded_bytes: usize,
    pub raw_row_limit: usize,
    pub raw_keys_observed: usize,
    pub witness_limit: usize,
    /// One undecoded matching-prefix key can establish that the row cap hid
    /// more work. It does not establish how many eligible witnesses remain.
    pub lookahead_observed: bool,
    pub scoped_prefix_exhausted: bool,
    pub witnesses_omitted_lower_bound: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct PotentialInterfaceCaller {
    pub implementation: Node,
    pub contract: Node,
    pub caller: Node,
    pub implementation_depth: usize,
    pub mapping_generation: u64,
    pub call_generation: u64,
    pub clause_line: Option<u32>,
    pub reference_lines: Vec<u32>,
    pub reference_lines_truncated: bool,
    pub contract_type_key: String,
    pub contract_type: String,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct InterfaceDispatch {
    pub routes: Vec<PotentialInterfaceCaller>,
    pub frontier: Vec<ImpactFrontier>,
    pub coverage: ImpactWalkCoverage,
    pub implementations_checked: usize,
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

fn canonical_script_callable(node: &Node) -> bool {
    let path = node.file_path.as_str();
    node.namespace == engram_core::namespaces::NAMESPACE_MEMORY
        && node.node_type == "function" && node.start_line > 0 && node.end_line >= node.start_line
        && node.language.len() <= 32 && node.name.len() <= 2048
        && matches!(node.language.to_ascii_lowercase().as_str(), "ts" | "typescript" | "js" | "javascript")
        && path.rsplit('.').next().is_some_and(|extension| extension.len() <= 3
            && matches!(extension.to_ascii_lowercase().as_str(), "ts" | "tsx" | "js" | "jsx"))
        && !path.is_empty() && path.len() <= 2048 && !path.contains(['\\', ':'])
        && !path.chars().any(char::is_control)
        && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
        && node.node_id.len() <= 2048 && !node.node_id.chars().any(char::is_control)
        && node.node_id == format!("sym:function:{path}:{}:{}", node.name, node.start_line)
}

/// An optional final observation under the SAME transaction, source capability
/// and remaining work budget. Only canonical directory-scoped JS/TS Calls are
/// examined; type/receiver binding and all other scopes remain unassessed.
fn inspect_unbound_members(
    tx: &redb::ReadTransaction,
    project: &str,
    root: &Node,
    remaining_rows: usize,
    remaining_witnesses: usize,
    budget: &mut Budget,
    admit: &dyn Fn(&Node, Option<&Node>) -> anyhow::Result<ImpactAdmission>,
) -> Option<UnboundMemberInspection> {
    if !canonical_script_callable(root) { return None; }
    let (directory, _) = root.file_path.as_str().rsplit_once('/')?;
    let member = root.name.rsplit('.').next()?;
    if directory.is_empty() || member.is_empty() || member.len() > 128
        || !member.chars().enumerate().all(|(i, c)| c == '_' || c == '$'
            || if i == 0 { c.is_alphabetic() } else { c.is_alphanumeric() }) { return None; }
    let mut result = UnboundMemberInspection {
        // Reserve one shared raw-work slot for an undecoded boundary/lookahead
        // key. Every obtained key remains within the original shared limit.
        raw_row_limit: remaining_rows.saturating_sub(1).min(512),
        witness_limit: remaining_witnesses.min(8), ..Default::default()
    };
    if !budget.live(&mut result.coverage) {
        gap(&mut result.coverage, "unbound_shared_work_budget_unavailable");
        return Some(result);
    }
    if !budget.retain(directory.len() + member.len(), &mut result.coverage) { return Some(result); }
    result.directory = directory.to_owned();
    result.member_name = member.to_owned();
    if result.raw_row_limit == 0 || result.witness_limit == 0 {
        gap(&mut result.coverage, "unbound_shared_row_or_witness_budget_unavailable");
        return Some(result);
    }
    let mut optional_halted = false;
    let attempted = (|| -> anyhow::Result<()> {
        let nodes = tx.open_table(NODES)?;
        let edges = tx.open_table(EDGES)?;
        let charge = |bytes: usize, budget: &mut Budget, coverage: &mut ImpactWalkCoverage,
                      decoded: &mut usize, halted: &mut bool| -> bool {
            if bytes > 64 * 1024 {
                coverage.unknown_nodes += 1;
                gap(coverage, "unbound_record_byte_limit");
                return false;
            }
            if decoded.checked_add(bytes).is_none_or(|total| total > 2 * 1024 * 1024) {
                gap(coverage, "unbound_decode_byte_limit"); *halted = true; return false;
            }
            if !budget.record(bytes, coverage) { *halted = budget.halted; return false; }
            *decoded += bytes;
            true
        };
        let read = |id: &str, budget: &mut Budget, coverage: &mut ImpactWalkCoverage,
                    decoded: &mut usize, halted: &mut bool| -> anyhow::Result<Option<Node>> {
            if !budget.live(coverage) { *halted = true; return Ok(None); }
            let key = format!("{project}\0{id}");
            let Some(raw) = nodes.get(key.as_str())? else {
                coverage.missing_nodes += 1; gap(coverage, "unbound_node_or_file_missing"); return Ok(None);
            };
            if !charge(raw.value().len(), budget, coverage, decoded, halted) { return Ok(None); }
            let node: Node = bincode::DefaultOptions::new().with_fixint_encoding()
                .with_limit(64 * 1024).reject_trailing_bytes().deserialize(raw.value())?;
            anyhow::ensure!(node.node_id == id, "Unbound inspection node/key mismatch");
            Ok(Some(node))
        };
        let prefix = format!("{project}\0calls\0sym:function:{directory}/");
        let mut range = edges.range(prefix.as_str()..)?;
        loop {
            if !budget.live(&mut result.coverage) || optional_halted { return Ok(()); }
            let Some(entry) = range.next() else { break; };
            result.raw_keys_observed += 1;
            let (key, raw) = entry?;
            let key = key.value();
            if !key.starts_with(&prefix) { result.scoped_prefix_exhausted = true; return Ok(()); }
            if result.coverage.raw_rows == result.raw_row_limit {
                result.lookahead_observed = true;
                gap(&mut result.coverage, "unbound_raw_row_limit"); return Ok(());
            }
            result.coverage.raw_rows += 1;
            anyhow::ensure!(key.len() <= project.len() + 3 + "calls".len() + 4096,
                "Unbound inspection edge key exceeds limits");
            let parts = key.split('\0').collect::<Vec<_>>();
            anyhow::ensure!(parts.len() == 4 && parts[0] == project && parts[1] == "calls"
                && parts[2..].iter().all(|id| !id.is_empty() && id.len() <= 2048 && !id.chars().any(char::is_control)),
                "Unbound inspection edge key unavailable");
            let (source, target) = (parts[2], parts[3]);
            if !charge(raw.value().len(), budget, &mut result.coverage, &mut result.decoded_bytes, &mut optional_halted) {
                if optional_halted { return Ok(()); } else { continue; }
            }
            let edge: Edge = bincode::DefaultOptions::new().with_fixint_encoding()
                .with_limit(64 * 1024).reject_trailing_bytes().deserialize(raw.value())?;
            anyhow::ensure!(edge.source_id == source && edge.target_id == target && edge.edge_kind == EdgeKind::Calls,
                "Unbound inspection edge/key mismatch");
            let Some(spelling) = target.strip_prefix(super::UNBOUND_RECEIVER_PREFIX) else { continue; };
            let Some((receiver, terminal)) = spelling.rsplit_once('.') else { continue; };
            if terminal != member { continue; }
            let metadata = edge.metadata.as_ref();
            let marked = metadata.and_then(|m| m.get("receiver_unbound"))
                .is_some_and(|v| v == true || v.as_str() == Some("true"));
            let hint = metadata.and_then(|m| m.get("receiver")).and_then(JsonValue::as_str);
            let matches_hint = hint.is_some_and(|hint| hint.len() <= 512
                && hint.chars().map(|c| if c.is_control() { ' ' } else { c }).eq(receiver.chars()));
            if !marked || !matches_hint || receiver.is_empty() || edge.namespace != engram_core::namespaces::NAMESPACE_MEMORY {
                result.coverage.unknown_nodes += 1; gap(&mut result.coverage, "unbound_marker_or_namespace_unverified"); continue;
            }
            let Some(caller) = read(source, budget, &mut result.coverage, &mut result.decoded_bytes, &mut optional_halted)? else { continue; };
            if !canonical_script_callable(&caller) || !caller.file_path.as_str().starts_with(&format!("{directory}/")) {
                result.coverage.unknown_nodes += 1; gap(&mut result.coverage, "unbound_caller_identity_unverified"); continue;
            }
            let Some(file) = read(&format!("file:{}", caller.file_path.as_str()), budget,
                &mut result.coverage, &mut result.decoded_bytes, &mut optional_halted)? else { continue; };
            if file.node_type != "file" || file.file_path != caller.file_path
                || file.namespace != engram_core::namespaces::NAMESPACE_MEMORY {
                result.coverage.unknown_nodes += 1; gap(&mut result.coverage, "unbound_file_identity_unverified"); continue;
            }
            match admit(&caller, Some(&file))? {
                ImpactAdmission::Selected if edge_source_revision_matches(&edge, &caller) => {}
                ImpactAdmission::NonSelectedRevision => {
                    result.coverage.non_selected_nodes += 1; gap(&mut result.coverage, "unbound_non_selected_caller"); continue;
                }
                _ => {
                    result.coverage.unknown_nodes += 1; gap(&mut result.coverage, "unbound_source_revision_unverified"); continue;
                }
            }
            if !budget.live(&mut result.coverage) { return Ok(()); }
            if result.witnesses.iter().any(|row| row.caller.node_id == source && row.unresolved_target_id == target) { continue; }
            let (_, position_kind, lines, lines_truncated) = recorded_positions(&edge);
            if lines.iter().any(|line| *line < caller.start_line || *line > caller.end_line) {
                result.coverage.unknown_nodes += 1; gap(&mut result.coverage, "unbound_reference_outside_declared_span"); continue;
            }
            if result.witnesses.len() == result.witness_limit {
                result.witnesses_omitted_lower_bound += 1;
                gap(&mut result.coverage, "unbound_witness_limit"); return Ok(());
            }
            if !budget.retain(identity_bytes(&caller) + target.len() + receiver.len()
                + lines.len() * std::mem::size_of::<u32>(), &mut result.coverage) { return Ok(()); }
            result.witnesses.push(UnboundMemberWitness { caller: identity_clone(&caller),
                unresolved_target_id: target.to_owned(), receiver: receiver.to_owned(), edge_generation: edge.generation,
                source_position_kind: position_kind, reference_lines: lines, reference_lines_truncated: lines_truncated });
        }
        result.scoped_prefix_exhausted = !optional_halted && !budget.halted;
        Ok(())
    })();
    if attempted.is_err() {
        result.coverage.unknown_nodes += 1;
        gap(&mut result.coverage, "unbound_graph_or_admission_unavailable");
    }
    result.coverage.nodes_observed = result.witnesses.iter().map(|row| row.caller.node_id.as_str())
        .collect::<std::collections::HashSet<_>>().len();
    result.coverage.reasons.sort();
    Some(result)
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

fn positive_line(value: &JsonValue) -> Option<u32> {
    let line = match value {
        JsonValue::String(text)
            if !text.is_empty() && text.len() <= 10 && text.bytes().all(|b| b.is_ascii_digit()) =>
        {
            text.parse::<u32>().ok()
        }
        JsonValue::Number(number) => number.as_u64().and_then(|line| u32::try_from(line).ok()),
        _ => None,
    }?;
    (line > 0).then_some(line)
}

fn recorded_positions(edge: &Edge) -> (Option<u32>, &'static str, Vec<u32>, bool) {
    let Some(metadata) = edge.metadata.as_ref().and_then(JsonValue::as_object) else {
        return (None, "not_recorded", Vec::new(), false);
    };
    // Ingestion retains at most 128 positions. Bound inspection of older or
    // hand-authored records too; never clone arbitrary extractor metadata.
    let mut lines = std::collections::BTreeSet::new();
    let mut truncated = false;
    if edge.edge_kind == EdgeKind::Calls {
        if let Some(line) = metadata.get("call_site_line").and_then(positive_line) {
            lines.insert(line);
        }
        if let Some(values) = metadata
            .get("call_site_lines")
            .and_then(JsonValue::as_array)
        {
            truncated = values.len() > 128;
            lines.extend(values.iter().take(128).filter_map(positive_line));
        }
        truncated |= metadata
            .get("call_site_lines_truncated")
            .is_some_and(|v| v == true || v.as_str() == Some("true"));
    }
    if !lines.is_empty() {
        truncated |= lines.len() > 8;
        let lines: Vec<_> = lines.into_iter().take(8).collect();
        return (
            lines.first().copied(),
            "call_reference_syntax_metadata",
            lines,
            truncated,
        );
    }
    let anchor = metadata.get("src_line").and_then(positive_line);
    (
        anchor,
        if anchor.is_some() {
            "extractor_anchor_metadata"
        } else {
            "not_recorded"
        },
        Vec::new(),
        truncated,
    )
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

    /// Navigation includes native markup controls and never falls back to a
    /// project-wide legacy node scan.
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
        // Markup controls have native IDs rather than location-based symbols
        // and do not have file Contains edges. Seek only the exact file's
        // control prefix, after all callables have had the shared budget.
        // Impact seed selection keeps its existing callable-first policy.
        if !allow_legacy {
            'controls: for &file_index in &admitted_files {
                if !budget.live(&mut coverage) {
                    break;
                }
                let path = &file_paths[file_index];
                let Some(file) = read(&format!("file:{path}"), &mut budget, &mut coverage)? else {
                    continue;
                };
                let prefix = format!("{project_id}\0control:{path}:");
                let mut entries = nodes.range(prefix.as_str()..)?;
                loop {
                    if !budget.live(&mut coverage) {
                        break 'controls;
                    }
                    if coverage.raw_rows == MAX_RAW_ROWS {
                        gap(&mut coverage, "callable_raw_row_limit");
                        budget.halted = true;
                        break 'controls;
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
                            "Control node identity disagrees with its key"
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
        let outgoing = tx.open_table(ADJ_OUT)?;
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
                    let producer_selected=match event_producer(&edge) {
                        Ok(None)=>true,
                        Err(_)=>false,
                        Ok(Some((path,revision,class)))=>{
                        let producer=read_node(&format!("file:{path}"),&mut budget,&mut output.coverage)?;
                        match producer.as_ref() {
                            Some(file) if file.node_type=="file" && file.file_path.as_str()==path && file.generation==revision
                                && (node.file_path.as_str()==path || event_owner_matches(node,path,class))
                                && file.namespace==engram_core::namespaces::NAMESPACE_MEMORY => admit(file,Some(file))?==ImpactAdmission::Selected,
                            _ => false,
                        }
                        }
                    };
                    if !producer_selected {
                            output.coverage.unknown_nodes+=1;
                            gap(&mut output.coverage,"unverified_event_producer_revision");
                            record_frontier(&mut output.frontier,&mut output.coverage,&mut budget,output.witnesses.len(),
                                source,Some(&target),Some(kind),Some(node),depth+1,"unverified_event_producer_revision");
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
                    let (
                        source_line,
                        source_position_kind,
                        reference_lines,
                        reference_lines_truncated,
                    ) = recorded_positions(&edge);
                    output.witnesses.push(ImpactWitness {
                        source_id: source.to_owned(),
                        target_id: target.clone(),
                        kind: kind.clone(),
                        edge_generation: edge.generation,
                        weight: edge.weight,
                        depth: depth + 1,
                        source_line,
                        source_position_kind,
                        extractor_anchor_line: edge
                            .metadata
                            .as_ref()
                            .and_then(|m| m.get("src_line"))
                            .and_then(positive_line),
                        reference_lines,
                        reference_lines_truncated,
                    });
                }
            }
        }
        // An optional second phase shares this exact read transaction and the
        // remaining decode/string/time/raw-work budgets. Never enqueue these
        // callers in the primary BFS or change its counts, caches or routes.
        let mut dispatch = InterfaceDispatch::default();
        let remaining_rows = MAX_RAW_ROWS.saturating_sub(output.coverage.raw_rows);
        let read_dispatch_edge = |source: &str, target: &str, kind: &EdgeKind,
                                  budget: &mut Budget, coverage: &mut ImpactWalkCoverage| -> anyhow::Result<Option<Edge>> {
            let Some(raw) = edges.get(edge_key(project_id,kind,source,target).as_str())? else {
                gap(coverage,"missing_interface_graph_edge"); return Ok(None);
            };
            if !budget.record(raw.value().len(),coverage) { return Ok(None); }
            let edge: Edge = bincode::DefaultOptions::new().with_fixint_encoding()
                .with_limit(MAX_RECORD_BYTES as u64).reject_trailing_bytes().deserialize(raw.value())?;
            anyhow::ensure!(edge.source_id==source && edge.target_id==target && edge.edge_kind==*kind,
                "Interface graph edge disagrees with adjacency");
            Ok(Some(edge))
        };
        'dispatch: for reached in output.nodes.iter().filter(|row|row.admission==ImpactAdmission::Selected && row.node.node_type=="function"
            && (row.node.language.eq_ignore_ascii_case("vbnet") || row.node.language.eq_ignore_ascii_case("vb"))) {
            if !budget.live(&mut dispatch.coverage) { gap(&mut dispatch.coverage,"primary_work_budget_exhausted"); break; }
            let Some(implementation)=read_node(&reached.node.node_id,&mut budget,&mut dispatch.coverage)? else { continue; };
            dispatch.implementations_checked+=1;
            let status=implementation.metadata.as_ref().and_then(|m|m.get("interface_dispatch_status")).and_then(JsonValue::as_str);
            if status!=Some("source_mapped") && status!=Some("no_explicit_contract") {
                gap(&mut dispatch.coverage,"interface_mapping_unassessed");
                record_frontier(&mut dispatch.frontier,&mut dispatch.coverage,&mut budget,output.witnesses.len()+output.frontier.len()+dispatch.routes.len(),
                    &implementation.node_id,None,Some(&EdgeKind::Implements),Some(&implementation),reached.depth+1,"interface_mapping_unassessed");
            }
            if status==Some("no_explicit_contract") { continue; }
            let Some(implementation_file)=read_node(&format!("file:{}",implementation.file_path.as_str()),&mut budget,&mut dispatch.coverage)? else { continue; };
            let prefix=adj_key(project_id,&EdgeKind::Implements,&implementation.node_id);
            let mut mappings=0;
            let mut verified_mapping=false;
            for entry in outgoing.range((prefix.as_str(), "")..)? {
                if !budget.live(&mut dispatch.coverage) { break 'dispatch; }
                if dispatch.coverage.raw_rows>=remaining_rows { gap(&mut dispatch.coverage,"raw_adjacency_row_limit"); break 'dispatch; }
                dispatch.coverage.raw_rows+=1;
                let (key,value)=entry?; let (observed,contract_id)=key.value();
                if observed!=prefix { break; }
                anyhow::ensure!(contract_id.len()<=2048 && !contract_id.contains(['\0','\n','\r']),"Interface adjacency target exceeds key limits");
                let _=decode_adj_value(value.value())?;
                mappings+=1;
                if mappings>16 { gap(&mut dispatch.coverage,"interface_mapping_fanout_limit"); break; }
                let Some(mapping)=read_dispatch_edge(&implementation.node_id,contract_id,&EdgeKind::Implements,&mut budget,&mut dispatch.coverage)? else {
                    record_frontier(&mut dispatch.frontier,&mut dispatch.coverage,&mut budget,output.witnesses.len()+output.frontier.len()+dispatch.routes.len(),
                        &implementation.node_id,Some(contract_id),Some(&EdgeKind::Implements),Some(&implementation),reached.depth+1,"interface_mapping_edge_unavailable");
                    continue;
                };
                let supports=interface_bindings(&mapping).ok().flatten();
                let Some(supports)=supports else {
                    gap(&mut dispatch.coverage,"unverified_interface_member_mapping");
                    record_frontier(&mut dispatch.frontier,&mut dispatch.coverage,&mut budget,output.witnesses.len()+output.frontier.len()+dispatch.routes.len(),
                        &implementation.node_id,Some(contract_id),Some(&EdgeKind::Implements),Some(&implementation),reached.depth+1,"unverified_interface_member_mapping");
                    continue;
                };
                if !supports.complete { gap(&mut dispatch.coverage,"interface_binding_supports_omitted"); }
                let Some(contract)=read_node(contract_id,&mut budget,&mut dispatch.coverage)? else {
                    record_frontier(&mut dispatch.frontier,&mut dispatch.coverage,&mut budget,output.witnesses.len()+output.frontier.len()+dispatch.routes.len(),
                        contract_id,Some(&implementation.node_id),Some(&EdgeKind::Implements),None,reached.depth+1,"interface_contract_unavailable");
                    continue;
                };
                let Some(contract_file)=read_node(&format!("file:{}",contract.file_path.as_str()),&mut budget,&mut dispatch.coverage)? else {
                    record_frontier(&mut dispatch.frontier,&mut dispatch.coverage,&mut budget,output.witnesses.len()+output.frontier.len()+dispatch.routes.len(),
                        contract_id,Some(&implementation.node_id),Some(&EdgeKind::Implements),Some(&contract),reached.depth+1,"interface_contract_file_unavailable");
                    continue;
                };
                let mapping_admitted=mapping.namespace==engram_core::namespaces::NAMESPACE_MEMORY && edge_source_revision_matches(&mapping,&implementation)
                    && admit(&implementation,Some(&implementation_file))?==ImpactAdmission::Selected
                    && admit(&contract,Some(&contract_file))?==ImpactAdmission::Selected;
                for support in supports.bindings {
                    if !mapping_admitted || !support.matches(&implementation,&implementation_file,&contract,&contract_file) {
                        gap(&mut dispatch.coverage,"unverified_interface_mapping_revision");
                        record_frontier(&mut dispatch.frontier,&mut dispatch.coverage,&mut budget,output.witnesses.len()+output.frontier.len()+dispatch.routes.len(),
                            &implementation.node_id,Some(contract_id),Some(&EdgeKind::Implements),Some(&implementation),reached.depth+1,"unverified_interface_mapping_revision");
                        continue;
                    }
                    verified_mapping=true;
                    if reached.depth+2>max_depth {
                        gap(&mut dispatch.coverage,"interface_dispatch_depth_limit");
                        record_frontier(&mut dispatch.frontier,&mut dispatch.coverage,&mut budget,output.witnesses.len()+output.frontier.len()+dispatch.routes.len(),
                            contract_id,Some(&implementation.node_id),Some(&EdgeKind::Calls),Some(&contract),reached.depth+2,"interface_dispatch_depth_limit");
                        continue;
                    }
                    let call_prefix=adj_key(project_id,&EdgeKind::Calls,contract_id);
                    let mut callers=0;
                    for entry in incoming.range((call_prefix.as_str(), "")..)? {
                        if !budget.live(&mut dispatch.coverage) { break 'dispatch; }
                        if dispatch.coverage.raw_rows>=remaining_rows { gap(&mut dispatch.coverage,"raw_adjacency_row_limit"); break 'dispatch; }
                        dispatch.coverage.raw_rows+=1;
                        let (key,value)=entry?; let (observed,caller_id)=key.value();
                        if observed!=call_prefix { break; }
                        anyhow::ensure!(caller_id.len()<=2048 && !caller_id.contains(['\0','\n','\r']),"Interface adjacency source exceeds key limits");
                        let _=decode_adj_value(value.value())?;
                        callers+=1;
                        if callers>64 { gap(&mut dispatch.coverage,"interface_caller_fanout_limit"); break; }
                        let Some(call)=read_dispatch_edge(caller_id,contract_id,&EdgeKind::Calls,&mut budget,&mut dispatch.coverage)? else {
                            record_frontier(&mut dispatch.frontier,&mut dispatch.coverage,&mut budget,output.witnesses.len()+output.frontier.len()+dispatch.routes.len(),
                                caller_id,Some(contract_id),Some(&EdgeKind::Calls),None,reached.depth+2,"interface_call_edge_unavailable");
                            continue;
                        };
                        let Some(caller)=read_node(caller_id,&mut budget,&mut dispatch.coverage)? else {
                            record_frontier(&mut dispatch.frontier,&mut dispatch.coverage,&mut budget,output.witnesses.len()+output.frontier.len()+dispatch.routes.len(),
                                caller_id,Some(contract_id),Some(&EdgeKind::Calls),None,reached.depth+2,"interface_caller_unavailable");
                            continue;
                        };
                        if excluded_source_file.is_some_and(|path|caller.file_path.as_str()==path) { dispatch.coverage.internal_rows_skipped+=1; continue; }
                        let Some(caller_file)=read_node(&format!("file:{}",caller.file_path.as_str()),&mut budget,&mut dispatch.coverage)? else {
                            record_frontier(&mut dispatch.frontier,&mut dispatch.coverage,&mut budget,output.witnesses.len()+output.frontier.len()+dispatch.routes.len(),
                                caller_id,Some(contract_id),Some(&EdgeKind::Calls),Some(&caller),reached.depth+2,"interface_caller_file_unavailable");
                            continue;
                        };
                        let call_supports=interface_bindings(&call).ok().flatten();
                        let call_admitted=call.namespace==engram_core::namespaces::NAMESPACE_MEMORY && edge_source_revision_matches(&call,&caller)
                            && admit(&caller,Some(&caller_file))?==ImpactAdmission::Selected;
                        let matching=call_supports.as_ref().and_then(|bindings|bindings.bindings.iter().find(|binding|
                            binding.target==support.target && binding.matches(&caller,&caller_file,&contract,&contract_file)));
                        if !call_admitted || call_supports.is_none() || matching.is_none() && call_supports.as_ref().is_some_and(|b|b.bindings.iter().any(|s|s.target.contract_type_key==support.target.contract_type_key)) {
                            gap(&mut dispatch.coverage,"unverified_interface_call_binding");
                            record_frontier(&mut dispatch.frontier,&mut dispatch.coverage,&mut budget,output.witnesses.len()+output.frontier.len()+dispatch.routes.len(),
                                caller_id,Some(contract_id),Some(&EdgeKind::Calls),Some(&caller),reached.depth+2,"unverified_interface_call_binding");
                            continue;
                        }
                        if call_supports.as_ref().is_some_and(|s|!s.complete) { gap(&mut dispatch.coverage,"interface_binding_supports_omitted"); }
                        let Some(call_support)=matching else { continue; }; // Another closed generic instance.
                        if !budget.live(&mut dispatch.coverage) { break 'dispatch; }
                        if dispatch.routes.iter().any(|route|route.implementation.node_id==implementation.node_id && route.contract.node_id==contract_id
                            && route.contract_type_key==support.target.contract_type_key && route.caller.node_id==caller_id) { continue; }
                        if dispatch.routes.len()==32 { gap(&mut dispatch.coverage,"potential_interface_route_limit"); break 'dispatch; }
                        if output.witnesses.len()+output.frontier.len()+dispatch.frontier.len()+dispatch.routes.len()>=MAX_WITNESSES {
                            gap(&mut dispatch.coverage,"shared_witness_edge_limit"); break 'dispatch;
                        }
                        if !budget.retain(identity_bytes(&implementation)+identity_bytes(&contract)+identity_bytes(&caller)
                            +support.target.contract_type_key.len()+support.target.contract_type.len(),&mut dispatch.coverage) { break 'dispatch; }
                        dispatch.routes.push(PotentialInterfaceCaller { implementation:identity_clone(&implementation),contract:identity_clone(&contract),caller:identity_clone(&caller),
                            implementation_depth:reached.depth,mapping_generation:mapping.generation,call_generation:call.generation,
                            clause_line:support.reference_lines.first().copied(),reference_lines:call_support.reference_lines.clone(),
                            reference_lines_truncated:call_support.reference_lines_truncated,contract_type_key:support.target.contract_type_key.clone(),contract_type:support.target.contract_type.clone() });
                    }
                }
            }
            if status==Some("source_mapped") && !verified_mapping {
                gap(&mut dispatch.coverage,"interface_member_mapping_unavailable");
                record_frontier(&mut dispatch.frontier,&mut dispatch.coverage,&mut budget,output.witnesses.len()+output.frontier.len()+dispatch.routes.len(),
                    &implementation.node_id,None,Some(&EdgeKind::Implements),Some(&implementation),reached.depth+1,"interface_member_mapping_unavailable");
            }
        }
        dispatch.coverage.nodes_observed=dispatch.routes.iter().map(|row|row.caller.node_id.as_str()).collect::<std::collections::HashSet<_>>().len();
        dispatch.coverage.reasons.sort();
        output.interface_dispatch=dispatch;
        if excluded_source_file.is_none() && seeds.len() == 1 && causal(&EdgeKind::Calls) {
            if let Some(root) = output.nodes.iter().find(|row| row.depth == 0 && row.admission == ImpactAdmission::Selected) {
                let remaining = MAX_RAW_ROWS.saturating_sub(output.coverage.raw_rows
                    .saturating_add(output.interface_dispatch.coverage.raw_rows));
                let remaining_witnesses = MAX_WITNESSES.saturating_sub(output.witnesses.len()
                    + output.frontier.len() + output.interface_dispatch.routes.len() + output.interface_dispatch.frontier.len());
                output.unbound_member_inspection = inspect_unbound_members(&tx, project_id, &root.node,
                    remaining, remaining_witnesses, &mut budget, admit);
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

    fn script_node(path: &str, name: &str, start: u32) -> Node {
        let mut value = node(&format!("sym:function:{path}:{name}:{start}"), path, 1);
        value.name = name.into(); value.language = "typescript".into();
        value.start_line = start; value.end_line = start + 10;
        value
    }

    fn unbound_edge(caller: &Node, receiver: &str) -> Edge {
        let mut value = edge(&caller.node_id, &format!("{UNBOUND_RECEIVER_PREFIX}{receiver}.resetState"), EdgeKind::Calls, 1);
        value.language = "typescript".into();
        value.metadata = Some(serde_json::json!({"receiver_unbound":true,"receiver":receiver,
            "call_site_lines":[caller.start_line+2,caller.start_line+3]}));
        value
    }

    fn unbound_fixture(store: &GraphStore) -> anyhow::Result<(Node, Node, Edge)> {
        let root = script_node("src/images/image.ts", "resetState", 1);
        let caller = script_node("src/images/group.ts", "clear", 10);
        let mut nodes = vec![root.clone(), caller.clone(), file(root.file_path.as_str(), 1), file(caller.file_path.as_str(), 1)];
        let mut edges = Vec::new();
        for (index, name) in ["deleteOne", "reload", "openImage"].iter().enumerate() {
            let direct = script_node(root.file_path.as_str(), name, 30 + index as u32 * 20);
            edges.push(edge(&direct.node_id, &root.node_id, EdgeKind::Calls, 1)); nodes.push(direct);
        }
        store.upsert_nodes_and_edges(PROJECT, &nodes, &edges)?;
        let unresolved = unbound_edge(&caller, "h");
        Ok((root, caller, unresolved))
    }

    fn primary_facts(walk: &IncomingImpactWalk) -> anyhow::Result<JsonValue> {
        Ok(serde_json::json!({"nodes":serde_json::to_value(&walk.nodes)?,"witnesses":walk.witnesses,
            "frontier":walk.frontier,"coverage":walk.coverage,"interface":walk.interface_dispatch}))
    }

    #[test]
    fn unbound_inspection_admits_source_syntax_without_rewriting_causal_facts() -> anyhow::Result<()> {
        owned_case(|store| {
            let (root, caller, original) = unbound_fixture(store)?;
            let expected = [("src/images/image.ts", 1), ("src/images/group.ts", 1)];
            let observe = || store.bounded_incoming_walk(PROJECT, &[root.node_id.clone()], 6, &causal,
                &|node, file| selected(&expected, node, file));
            let baseline = primary_facts(&observe()?)?;
            store.upsert_edges(PROJECT, &[original.clone()])?;
            let admitted = observe()?;
            assert_eq!(primary_facts(&admitted)?, baseline);
            let inspection = admitted.unbound_member_inspection.unwrap();
            assert_eq!(inspection.witnesses.len(), 1);
            assert_eq!(inspection.witnesses[0].caller.node_id, caller.node_id);
            assert_eq!(inspection.witnesses[0].reference_lines, vec![12,13]);
            assert_eq!(inspection.coverage.nodes_observed, 1);
            assert!(matches!(store.resolve_symbol(PROJECT, &original.target_id, None, None)?, ResolveResult::NotFound));
            let file_id = format!("file:{}", caller.file_path.as_str());
            let original_file = store.get_node(PROJECT, &file_id)?.unwrap();
            for scenario in ["stale_caller", "future_caller", "foreign_caller", "wrong_caller_path",
                "stale_file", "future_file", "foreign_file", "wrong_file_kind", "foreign_edge",
                "stale_edge", "bad_marker", "bad_receiver", "bad_origin", "outside_span"] {
                let mut bad_caller = caller.clone(); let mut bad_file = original_file.clone(); let mut bad_edge = original.clone();
                match scenario {
                    "stale_caller" => bad_caller.generation = 0,
                    "future_caller" => bad_caller.generation = 2,
                    "foreign_caller" => bad_caller.namespace = "history".into(),
                    "wrong_caller_path" => bad_caller.file_path = RelPath::new("src/images2/group.ts"),
                    "stale_file" => bad_file.generation = 0,
                    "future_file" => bad_file.generation = 2,
                    "foreign_file" => bad_file.namespace = "history".into(),
                    "wrong_file_kind" => bad_file.node_type = "function".into(),
                    "foreign_edge" => bad_edge.namespace = "history".into(),
                    "stale_edge" => bad_edge.generation = 0,
                    "bad_marker" => bad_edge.metadata.as_mut().unwrap()["receiver_unbound"] = serde_json::json!(false),
                    "bad_receiver" => bad_edge.metadata.as_mut().unwrap()["receiver"] = serde_json::json!("different"),
                    "bad_origin" => bad_edge.metadata.as_mut().unwrap()[SOURCE_ORIGIN] = JsonValue::Null,
                    _ => bad_edge.metadata.as_mut().unwrap()["call_site_lines"] = serde_json::json!([999]),
                }
                store.upsert_nodes(PROJECT, &[bad_caller, bad_file])?;
                store.upsert_edges(PROJECT, &[bad_edge])?;
                let rejected = observe()?;
                assert_eq!(primary_facts(&rejected)?, baseline, "{scenario}");
                let inspection = rejected.unbound_member_inspection.unwrap();
                assert!(inspection.witnesses.is_empty() && inspection.coverage.truncated, "{scenario}: {inspection:?}");
                assert!(!inspection.coverage.reasons.is_empty());
                store.upsert_nodes(PROJECT, &[caller.clone(), original_file.clone()])?;
                store.upsert_edges(PROJECT, &[original.clone()])?;
            }
            let mut inherited = original.clone(); inherited.generation = 2;
            inherited.metadata.as_mut().unwrap()[SOURCE_ORIGIN] = serde_json::json!({
                "version":1,"source_id":caller.node_id,"source_generation":1});
            store.upsert_edges(PROJECT, &[inherited])?;
            let inherited = observe()?;
            assert_eq!(primary_facts(&inherited)?, baseline);
            let witness = &inherited.unbound_member_inspection.as_ref().unwrap().witnesses[0];
            assert_eq!(witness.caller.generation, 1);
            assert_eq!(witness.edge_generation, 2);
            Ok(())
        })
    }

    #[test]
    fn unbound_inspection_bounds_skipped_rows_and_discloses_unknown_remainder() -> anyhow::Result<()> {
        owned_case(|store| {
            let (root, _, unresolved) = unbound_fixture(store)?;
            store.upsert_edges(PROJECT, &[unresolved])?;
            let skipped = (0..513).map(|number| edge("sym:function:src/images/aaa.ts:Probe:1",
                &format!("::unrelated_{number:04}"), EdgeKind::Calls, 1)).collect::<Vec<_>>();
            store.upsert_edges(PROJECT, &skipped)?;
            let walk = store.bounded_incoming_walk(PROJECT, &[root.node_id], 6, &causal,
                &|node, file| selected(&[("src/images/image.ts",1),("src/images/group.ts",1)], node, file))?;
            assert_eq!(walk.nodes.iter().filter(|node| node.depth>0 && node.admission==ImpactAdmission::Selected).count(), 3);
            let inspection = walk.unbound_member_inspection.unwrap();
            assert!(inspection.witnesses.is_empty() && inspection.coverage.truncated);
            assert_eq!(inspection.coverage.raw_rows, 512);
            assert_eq!(inspection.raw_keys_observed, 513);
            assert!(inspection.lookahead_observed && !inspection.scoped_prefix_exhausted);
            assert_eq!(inspection.witnesses_omitted_lower_bound, 0, "Unexamined rows are not known eligible witnesses");
            Ok(())
        })?;
        owned_case(|store| {
            let (root, _, _) = unbound_fixture(store)?;
            let mut paths = vec![root.file_path.as_str().to_owned()];
            let mut nodes = Vec::new(); let mut edges = Vec::new();
            for number in 0..9 {
                let path = format!("src/images/candidate{number}.ts");
                let caller = script_node(&path, "Probe", 10);
                edges.push(unbound_edge(&caller, &format!("h{number}")));
                nodes.extend([file(&path, 1), caller]); paths.push(path);
            }
            store.upsert_nodes_and_edges(PROJECT, &nodes, &edges)?;
            let expected = paths.iter().map(|path| (path.as_str(), 1)).collect::<Vec<_>>();
            let walk = store.bounded_incoming_walk(PROJECT, &[root.node_id], 6, &causal,
                &|node, file| selected(&expected, node, file))?;
            assert_eq!(walk.nodes.iter().filter(|node| node.depth>0 && node.admission==ImpactAdmission::Selected).count(), 3);
            let inspection = walk.unbound_member_inspection.unwrap();
            assert_eq!(inspection.witnesses.len(), 8);
            assert_eq!(inspection.coverage.nodes_observed, 8);
            assert_eq!(inspection.witnesses_omitted_lower_bound, 1);
            assert!(inspection.coverage.truncated && !inspection.scoped_prefix_exhausted);
            Ok(())
        })
    }

    #[test]
    fn unbound_bad_records_and_spent_shared_budget_preserve_primary_evidence() -> anyhow::Result<()> {
        owned_case(|store| {
            let (root, caller, original) = unbound_fixture(store)?;
            store.upsert_edges(PROJECT, &[original.clone()])?;
            let expected = [("src/images/image.ts",1),("src/images/group.ts",1)];
            let observe = || store.bounded_incoming_walk(PROJECT, &[root.node_id.clone()], 6, &causal,
                &|node, file| selected(&expected, node, file));
            let baseline = primary_facts(&observe()?)?;
            let file_id = format!("file:{}", caller.file_path.as_str());
            let original_file = store.get_node(PROJECT, &file_id)?.unwrap();
            for scenario in ["large_edge", "large_node", "large_file", "corrupt_edge", "corrupt_node", "mismatched_node", "missing_file"] {
                let mut bad_caller = caller.clone(); let mut bad_file = original_file.clone(); let mut bad_edge = original.clone();
                if scenario == "large_edge" { bad_edge.metadata.as_mut().unwrap()["payload"] = serde_json::json!("FOREIGN_INSPECTION_SENTINEL".repeat(4000)); }
                if scenario == "large_node" { bad_caller.metadata.as_mut().unwrap()["payload"] = serde_json::json!("FOREIGN_INSPECTION_SENTINEL".repeat(4000)); }
                if scenario == "large_file" { bad_file.metadata.as_mut().unwrap()["payload"] = serde_json::json!("FOREIGN_INSPECTION_SENTINEL".repeat(4000)); }
                store.upsert_nodes(PROJECT, &[bad_caller, bad_file])?; store.upsert_edges(PROJECT, &[bad_edge])?;
                // Mutate this fixture's existing database handle between completed
                // read transactions; no second owner or production data is opened.
                if matches!(scenario, "corrupt_edge" | "corrupt_node" | "mismatched_node" | "missing_file") {
                    let write = store.db.begin_write()?;
                    if scenario == "corrupt_edge" {
                        write.open_table(EDGES)?.insert(edge_key(PROJECT,&EdgeKind::Calls,&caller.node_id,&original.target_id).as_str(), &[255u8][..])?;
                    } else {
                        let mut table = write.open_table(NODES)?;
                        let id = if scenario == "missing_file" { &file_id } else { &caller.node_id };
                        let key = format!("{PROJECT}\0{id}");
                        if scenario == "missing_file" { table.remove(key.as_str())?; }
                        else if scenario == "corrupt_node" { table.insert(key.as_str(), &[255u8][..])?; }
                        else {
                            let mut mismatched = caller.clone(); mismatched.node_id = "FOREIGN_INSPECTION_SENTINEL".into();
                            table.insert(key.as_str(), bincode::serialize(&mismatched)?.as_slice())?;
                        }
                    }
                    write.commit()?;
                }
                let rejected = observe()?;
                assert_eq!(primary_facts(&rejected)?, baseline, "{scenario}");
                let inspection = rejected.unbound_member_inspection.unwrap();
                assert!(inspection.witnesses.is_empty() && inspection.coverage.truncated, "{scenario}");
                assert!(!serde_json::to_string(&inspection)?.contains("FOREIGN_INSPECTION_SENTINEL"));
                store.upsert_nodes(PROJECT, &[caller.clone(), original_file.clone()])?;
                store.upsert_edges(PROJECT, &[original.clone()])?;
            }
            let tx = store.db.begin_read()?;
            let mut spent = Budget { started: Instant::now()-WALK_TIME-Duration::from_secs(1), string_bytes:0, decode_bytes:0, halted:false };
            let inspected = inspect_unbound_members(&tx, PROJECT, &root, MAX_RAW_ROWS, MAX_WITNESSES, &mut spent,
                &|node, file| selected(&expected, node, file)).unwrap();
            assert!(inspected.coverage.truncated && inspected.witnesses.is_empty());
            assert_eq!(inspected.raw_keys_observed, 0);
            assert_eq!(inspected.decoded_bytes, 0);
            assert!(spent.halted);
            Ok(())
        })
    }

    #[test]
    fn persisted_reference_samples_do_not_reassign_owners_or_upgrade_legacy_anchors()
    -> anyhow::Result<()> {
        owned_case(|store| {
            let invalid = serde_json::json!([
                0,
                -1,
                1.5,
                4294967296u64,
                "4294967296",
                "+96",
                " 96",
                true,
                {},
                null
            ]);
            let mut oversized = vec![serde_json::json!(0); 128];
            oversized.push(serde_json::json!(96));
            let cases = vec![
                (
                    serde_json::json!({"src_line":"92","call_site_line":"96"}),
                    Some(96),
                    "call_reference_syntax_metadata",
                    vec![96],
                    false,
                ),
                (
                    serde_json::json!({"src_line":92,"call_site_line":101,"call_site_lines":[101,"96",101,0]}),
                    Some(96),
                    "call_reference_syntax_metadata",
                    vec![96, 101],
                    false,
                ),
                (
                    serde_json::json!({"src_line":92,"call_site_line":"96","via":"addressof"}),
                    Some(96),
                    "call_reference_syntax_metadata",
                    vec![96],
                    false,
                ),
                (
                    serde_json::json!({"src_line":92,"call_site_lines":(96..=110).rev().collect::<Vec<_>>()}),
                    Some(96),
                    "call_reference_syntax_metadata",
                    (96..=103).collect(),
                    true,
                ),
                (
                    serde_json::json!({"src_line":92,"call_site_line":96,"call_site_lines_truncated":true}),
                    Some(96),
                    "call_reference_syntax_metadata",
                    vec![96],
                    true,
                ),
                (
                    serde_json::json!({"src_line":92,"call_site_lines":oversized}),
                    Some(92),
                    "extractor_anchor_metadata",
                    vec![],
                    true,
                ),
                (
                    serde_json::json!({"src_line":"92","call_site_lines":invalid}),
                    Some(92),
                    "extractor_anchor_metadata",
                    vec![],
                    false,
                ),
                (
                    serde_json::json!({"src_line":"92"}),
                    Some(92),
                    "extractor_anchor_metadata",
                    vec![],
                    false,
                ),
                (
                    serde_json::json!({"src_line":0,"call_site_line":false}),
                    None,
                    "not_recorded",
                    vec![],
                    false,
                ),
                (
                    serde_json::json!({"src_line":4294967296u64,"call_site_lines":"[96]"}),
                    None,
                    "not_recorded",
                    vec![],
                    false,
                ),
                (
                    serde_json::json!({"src_line":92,"call_site_line":"000000000096"}),
                    Some(92),
                    "extractor_anchor_metadata",
                    vec![],
                    false,
                ),
                (
                    serde_json::json!({"call_site_line":4294967295u64}),
                    Some(u32::MAX),
                    "call_reference_syntax_metadata",
                    vec![u32::MAX],
                    false,
                ),
                (serde_json::Value::Null, None, "not_recorded", vec![], false),
            ];
            let mut nodes = vec![file("policy.vb", 1), node("target", "policy.vb", 1)];
            let mut edges = Vec::new();
            for (index, (metadata, ..)) in cases.iter().enumerate() {
                let mut caller = node(&format!("same-name-overload:{index}:92"), "policy.vb", 1);
                caller.start_line = 92;
                caller.end_line = 120;
                let mut call = edge(&caller.node_id, "target", EdgeKind::Calls, 1);
                call.metadata = (!metadata.is_null()).then(|| metadata.clone());
                nodes.push(caller);
                edges.push(call);
            }
            // Attractive position metadata must not authorize a stale edge.
            let mut stale = edge("stale", "target", EdgeKind::Calls, 2);
            stale.metadata = Some(serde_json::json!({"call_site_line":96}));
            nodes.push(node("stale", "policy.vb", 1));
            edges.push(stale);
            store.upsert_nodes(PROJECT, &nodes)?;
            store.upsert_edges(PROJECT, &edges)?;
            let walk =
                store.bounded_incoming_walk(PROJECT, &["target".into()], 1, &causal, &|n, f| {
                    selected(&[("policy.vb", 1)], n, f)
                })?;
            assert_eq!(walk.witnesses.len(), cases.len());
            for (index, (_, line, kind, lines, truncated)) in cases.iter().enumerate() {
                let id = format!("same-name-overload:{index}:92");
                let witness = walk.witnesses.iter().find(|w| w.source_id == id).unwrap();
                assert_eq!(
                    (witness.source_line, witness.source_position_kind),
                    (*line, *kind)
                );
                assert_eq!(
                    (&witness.reference_lines, witness.reference_lines_truncated),
                    (lines, *truncated)
                );
                let owner = walk.nodes.iter().find(|n| n.node.node_id == id).unwrap();
                assert_eq!((owner.node.start_line, owner.node.end_line), (92, 120));
            }
            Ok(())
        })
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
