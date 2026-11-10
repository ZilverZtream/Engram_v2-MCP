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

/// Selected property identity reached by an indexed Calls edge. This does not
/// classify a property access as a read/write or establish runtime value flow.
#[derive(Debug, Clone, Serialize)]
pub struct PropertyReference {
    /// Admitted identity only; arbitrary graph metadata is not retained.
    pub target: Node,
    pub edge_generation: u64,
    pub source_line: Option<u32>,
    pub source_position_kind: &'static str,
    pub extractor_anchor_line: Option<u32>,
    pub reference_lines: Vec<u32>,
    pub reference_lines_truncated: bool,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct PropertyReferences {
    /// Present only after source and canonical file admission. Metadata removed.
    pub source: Option<Node>,
    pub references: Vec<PropertyReference>,
    /// raw_rows includes at most 64 examined rows plus one undecoded lookahead
    /// key. nodes_observed counts source/target declarations, not file lookups.
    pub coverage: ImpactWalkCoverage,
    pub excluded_nodes: usize,
    pub non_property_nodes: usize,
}

/// A revision-checked indexed Calls target with the same identity/position
/// representation as a property reference; this does not establish dispatch,
/// value flow, or runtime execution.
pub type CallableReference = PropertyReference;

#[derive(Debug, Default, Clone, Serialize)]
pub struct CallableReferences {
    pub references: Vec<CallableReference>,
    /// Additional callable admission work only. The shared scan's omissions
    /// and unknown observations are retained separately, without double counts.
    pub coverage: ImpactWalkCoverage,
    pub scan_coverage: ImpactWalkCoverage,
    /// Callable identities encountered after edge/source verification, before
    /// target/file admission. This is not an exhaustive callable inventory.
    pub candidates_observed: usize,
    /// Observed candidates not admitted/retained, including pending capacity.
    pub candidates_omitted: usize,
    pub excluded_nodes: usize,
    /// Peak separately bounded metadata-free pending identity/position bytes.
    pub pending_bytes: usize,
}

/// A compiler-located explicit implementation of a called interface member.
/// This is a potential dispatch route, not a direct Calls edge or runtime proof.
#[derive(Debug, Clone, Serialize)]
pub struct InterfaceImplementationRoute {
    pub caller: Node,
    pub contract: Node,
    pub implementation: Node,
    pub call_generation: u64,
    pub mapping_generation: u64,
    pub reference_lines: Vec<u32>,
    pub reference_lines_truncated: bool,
    pub clause_line: Option<u32>,
    pub contract_type_key: String,
    pub contract_type: String,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct InterfaceImplementations {
    pub routes: Vec<InterfaceImplementationRoute>,
    /// Separate optional work; never added to direct callable counts.
    pub coverage: ImpactWalkCoverage,
    pub excluded_nodes: usize,
}

const CALLABLE_PENDING_LEADS: usize = 16;
const CALLABLE_PENDING_BYTES: usize = 64 * 1024;

const PROPERTY_RAW_ROWS: usize = 64;
const PROPERTY_LEADS: usize = 8;
const PROPERTY_RECORD_BYTES: usize = 64 * 1024;
const PROPERTY_DECODE_BYTES: usize = 2 * 1024 * 1024;
const PROPERTY_STRING_BYTES: usize = 64 * 1024;
const PROPERTY_TIME: Duration = Duration::from_millis(500);

struct PropertyBudget {
    started: Instant,
    decode_bytes: usize,
    retained_bytes: usize,
    halted: bool,
}

impl PropertyBudget {
    fn live(&mut self, coverage: &mut ImpactWalkCoverage) -> bool {
        if self.started.elapsed() >= PROPERTY_TIME {
            gap(coverage, "property_reference_cooperative_deadline");
            self.halted = true;
        }
        !self.halted
    }

    fn record(&mut self, bytes: usize, coverage: &mut ImpactWalkCoverage) -> bool {
        if !self.live(coverage) { return false; }
        if bytes > PROPERTY_RECORD_BYTES {
            coverage.unknown_nodes += 1;
            gap(coverage, "property_reference_record_byte_limit");
            return false;
        }
        if self.decode_bytes.checked_add(bytes).is_none_or(|total| total > PROPERTY_DECODE_BYTES) {
            gap(coverage, "property_reference_decode_byte_limit");
            self.halted = true;
            return false;
        }
        self.decode_bytes += bytes;
        true
    }

    fn retain(&mut self, node: &Node, coverage: &mut ImpactWalkCoverage) -> bool {
        if !self.live(coverage) { return false; }
        if self.retained_bytes.checked_add(identity_bytes(node)).is_none_or(|total| total > PROPERTY_STRING_BYTES) {
            gap(coverage, "property_reference_retained_string_limit");
            self.halted = true;
            return false;
        }
        self.retained_bytes += identity_bytes(node);
        true
    }
}

fn property_identity_bounded(node: &Node) -> bool {
    let text = |value: &str, max: usize| value.len() <= max && !value.chars().any(char::is_control);
    let path = node.file_path.as_str();
    !node.node_id.is_empty() && text(&node.node_id, 2048) && text(&node.name, 2048)
        && text(&node.node_type, 64) && text(&node.language, 64) && text(&node.namespace, 64)
        && !path.is_empty() && text(path, 2048) && !path.contains(['\\', ':'])
        && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
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
    if requires_source_call_binding(edge) {
        // Called after endpoint admission. Never mix arbitrary edge-wide
        // positions into the compiler-selected declaration's witness.
        let Ok(Some(proofs)) = source_call_bindings(edge) else {
            return (None, "not_recorded", Vec::new(), false);
        };
        let mut lines = std::collections::BTreeSet::new();
        let mut truncated = !proofs.complete;
        for proof in proofs.bindings {
            lines.extend(proof.reference_lines);
            truncated |= proof.reference_lines_truncated;
        }
        truncated |= lines.len() > 8;
        let lines: Vec<_> = lines.into_iter().take(8).collect();
        return (lines.first().copied(), if lines.is_empty() { "not_recorded" }
            else { "call_reference_syntax_metadata" }, lines, truncated);
    }
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
    /// Follow at most sixteen already observed callee identities from an exact
    /// caller to explicit VB interface implementations. Every endpoint, its
    /// canonical file, Calls binding and Implements binding is rechecked in one
    /// read transaction. The caller retains its source-selection capability.
    ///
    /// This optional observation has its own shared budget: 128 adjacency rows
    /// plus one undecoded lookahead, sixteen routes, 64KiB per record, 2MiB total
    /// decoded records, 64KiB retained identity strings and a cooperative 500ms
    /// deadline. Rejections consume work; supplied IDs never grant admission.
    pub fn bounded_interface_implementations(
        &self,
        project_id: &str,
        caller_id: &str,
        contract_ids: &[String],
        admit: &dyn Fn(&Node, Option<&Node>) -> anyhow::Result<ImpactAdmission>,
        excluded: &dyn Fn(&Node) -> bool,
    ) -> anyhow::Result<InterfaceImplementations> {
        let valid_id = |id: &str, limit| !id.is_empty() && id.len() <= limit
            && !id.chars().any(char::is_control);
        anyhow::ensure!(valid_id(project_id, 1024) && valid_id(caller_id, 2048)
            && contract_ids.len() <= 16 && contract_ids.iter().all(|id|valid_id(id, 2048)),
            "Invalid interface implementation observation identities");
        let mut output = (|| -> anyhow::Result<InterfaceImplementations> {
            let mut output = InterfaceImplementations::default();
            if contract_ids.is_empty() { return Ok(output); }
            let mut budget = PropertyBudget { started: Instant::now(), decode_bytes: 0, retained_bytes: 0, halted: false };
            let tx = self.db.begin_read()?;
            let nodes = tx.open_table(NODES)?;
            let edges = tx.open_table(EDGES)?;
            let incoming = tx.open_table(ADJ_IN)?;
            let read = |id: &str, budget: &mut PropertyBudget, coverage: &mut ImpactWalkCoverage| -> anyhow::Result<Option<Node>> {
                if !budget.live(coverage) { return Ok(None); }
                let Some(raw) = nodes.get(format!("{project_id}\0{id}").as_str())? else {
                    coverage.missing_nodes += 1;
                    gap(coverage, "interface_implementation_node_or_file_missing");
                    return Ok(None);
                };
                if !budget.record(raw.value().len(), coverage) { return Ok(None); }
                let node: Node = bincode::DefaultOptions::new().with_fixint_encoding()
                    .with_limit(PROPERTY_RECORD_BYTES as u64).reject_trailing_bytes().deserialize(raw.value())?;
                anyhow::ensure!(node.node_id == id, "Interface implementation node disagrees with its key");
                if !property_identity_bounded(&node) || node.namespace != engram_core::namespaces::NAMESPACE_MEMORY {
                    coverage.unknown_nodes += 1;
                    gap(coverage, "interface_implementation_identity_or_namespace_unavailable");
                    return Ok(None);
                }
                Ok(Some(node))
            };
            let selected_file = |node: &Node, budget: &mut PropertyBudget, output: &mut InterfaceImplementations|
                -> anyhow::Result<Option<Node>> {
                if excluded(node) {
                    output.excluded_nodes += 1;
                    gap(&mut output.coverage, "interface_implementation_endpoint_excluded");
                    return Ok(None);
                }
                let Some(file) = read(&format!("file:{}", node.file_path.as_str()), budget, &mut output.coverage)?
                    else { return Ok(None) };
                if file.node_type != "file" || file.file_path != node.file_path || file.generation != node.generation {
                    output.coverage.unknown_nodes += 1;
                    gap(&mut output.coverage, "interface_implementation_canonical_file_unavailable");
                    return Ok(None);
                }
                if excluded(&file) {
                    output.excluded_nodes += 1;
                    gap(&mut output.coverage, "interface_implementation_file_excluded");
                    return Ok(None);
                }
                if !budget.live(&mut output.coverage) { return Ok(None); }
                let node_admission = admit(node, Some(&file))?;
                if !budget.live(&mut output.coverage) { return Ok(None); }
                let file_admission = admit(&file, Some(&file))?;
                if !budget.live(&mut output.coverage) { return Ok(None); }
                match (node_admission, file_admission) {
                    (ImpactAdmission::Selected, ImpactAdmission::Selected) => Ok(Some(file)),
                    (ImpactAdmission::NonSelectedRevision, _) | (_, ImpactAdmission::NonSelectedRevision) => {
                        output.coverage.non_selected_nodes += 1;
                        gap(&mut output.coverage, "interface_implementation_non_selected_revision"); Ok(None)
                    }
                    _ => {
                        output.coverage.unknown_nodes += 1;
                        gap(&mut output.coverage, "interface_implementation_admission_unknown"); Ok(None)
                    }
                }
            };
            let read_edge = |source: &str, target: &str, kind: &EdgeKind,
                             budget: &mut PropertyBudget, coverage: &mut ImpactWalkCoverage| -> anyhow::Result<Option<Edge>> {
                if !budget.live(coverage) { return Ok(None); }
                let Some(raw) = edges.get(edge_key(project_id, kind, source, target).as_str())? else {
                    gap(coverage, "interface_implementation_edge_missing"); return Ok(None);
                };
                if !budget.record(raw.value().len(), coverage) { return Ok(None); }
                let edge: Edge = bincode::DefaultOptions::new().with_fixint_encoding()
                    .with_limit(PROPERTY_RECORD_BYTES as u64).reject_trailing_bytes().deserialize(raw.value())?;
                anyhow::ensure!(edge.source_id == source && edge.target_id == target && edge.edge_kind == *kind,
                    "Interface implementation edge disagrees with its key");
                Ok(Some(edge))
            };
            let Some(caller) = read(caller_id, &mut budget, &mut output.coverage)? else { return Ok(output) };
            if caller.node_type != "function" || !(caller.language.eq_ignore_ascii_case("vb")
                || caller.language.eq_ignore_ascii_case("vbnet")) { return Ok(output); }
            let Some(caller_file) = selected_file(&caller, &mut budget, &mut output)? else { return Ok(output) };
            let mut seen = std::collections::HashSet::new();
            'contracts: for contract_id in contract_ids {
                if !seen.insert(contract_id) { continue; }
                if !budget.live(&mut output.coverage) { break; }
                let Some(contract) = read(contract_id, &mut budget, &mut output.coverage)? else { continue };
                if contract.metadata.as_ref().and_then(|m|m.get("interface_member_declaration"))
                    .and_then(JsonValue::as_str) != Some("true") { continue; }
                let Some(contract_file) = selected_file(&contract, &mut budget, &mut output)? else { continue };
                let Some(call) = read_edge(caller_id, contract_id, &EdgeKind::Calls, &mut budget, &mut output.coverage)? else { continue };
                let Some(calls) = interface_bindings(&call).ok().flatten() else {
                    gap(&mut output.coverage, "interface_implementation_call_binding_unavailable"); continue;
                };
                if call.namespace != engram_core::namespaces::NAMESPACE_MEMORY || !edge_source_revision_matches(&call, &caller) {
                    gap(&mut output.coverage, "interface_implementation_call_source_unverified"); continue;
                }
                if !calls.complete { gap(&mut output.coverage, "interface_implementation_call_bindings_omitted"); }
                let verified: Vec<_> = calls.bindings.iter().filter(|binding| {
                    if binding.matches(&caller, &caller_file, &contract, &contract_file) { true }
                    else { gap(&mut output.coverage, "interface_implementation_call_binding_unverified"); false }
                }).collect();
                if verified.is_empty() { continue; }
                let prefix = adj_key(project_id, &EdgeKind::Implements, contract_id);
                for entry in incoming.range((prefix.as_str(), "")..)? {
                    if !budget.live(&mut output.coverage) { break 'contracts; }
                    let (key, adjacency) = entry?;
                    let (observed, implementation_id) = key.value();
                    if observed != prefix { break; }
                    output.coverage.raw_rows += 1;
                    if output.coverage.raw_rows > 128 {
                        gap(&mut output.coverage, "interface_implementation_raw_row_limit"); break 'contracts;
                    }
                    anyhow::ensure!(valid_id(implementation_id, 2048), "Invalid interface implementation adjacency identity");
                    let _ = decode_adj_value(adjacency.value())?;
                    let Some(implementation) = read(implementation_id, &mut budget, &mut output.coverage)? else { continue };
                    output.coverage.nodes_observed += 1;
                    if implementation.node_type != "function" || !(implementation.language.eq_ignore_ascii_case("vb")
                        || implementation.language.eq_ignore_ascii_case("vbnet")) {
                        gap(&mut output.coverage, "interface_implementation_declaration_unavailable"); continue;
                    }
                    let Some(implementation_file) = selected_file(&implementation, &mut budget, &mut output)? else { continue };
                    let Some(mapping) = read_edge(implementation_id, contract_id, &EdgeKind::Implements,
                        &mut budget, &mut output.coverage)? else { continue };
                    let Some(mappings) = interface_bindings(&mapping).ok().flatten() else {
                        gap(&mut output.coverage, "interface_implementation_mapping_unavailable"); continue;
                    };
                    if mapping.namespace != engram_core::namespaces::NAMESPACE_MEMORY || !edge_source_revision_matches(&mapping, &implementation) {
                        gap(&mut output.coverage, "interface_implementation_mapping_source_unverified"); continue;
                    }
                    if !mappings.complete { gap(&mut output.coverage, "interface_implementation_mapping_bindings_omitted"); }
                    for support in &mappings.bindings {
                        let Some(call_support) = verified.iter().find(|binding| binding.target == support.target) else { continue };
                        if !support.matches(&implementation, &implementation_file, &contract, &contract_file) {
                            gap(&mut output.coverage, "interface_implementation_mapping_binding_unverified"); continue;
                        }
                        if output.routes.iter().any(|row| row.contract.node_id == *contract_id
                            && row.implementation.node_id == implementation.node_id && row.contract_type_key == support.target.contract_type_key) { continue; }
                        if output.routes.len() == 16 {
                            gap(&mut output.coverage, "interface_implementation_route_limit"); break 'contracts;
                        }
                        let bytes = identity_bytes(&caller) + identity_bytes(&contract) + identity_bytes(&implementation)
                            + support.target.contract_type_key.len() + support.target.contract_type.len()
                            + call_support.reference_lines.len() * std::mem::size_of::<u32>();
                        if budget.retained_bytes.checked_add(bytes).is_none_or(|n| n > PROPERTY_STRING_BYTES) {
                            gap(&mut output.coverage, "interface_implementation_retained_byte_limit"); break 'contracts;
                        }
                        if !budget.live(&mut output.coverage) { break 'contracts; }
                        budget.retained_bytes += bytes;
                        output.routes.push(InterfaceImplementationRoute {
                            caller: identity_clone(&caller), contract: identity_clone(&contract), implementation: identity_clone(&implementation),
                            call_generation: call.generation, mapping_generation: mapping.generation,
                            reference_lines: call_support.reference_lines.clone(), reference_lines_truncated: call_support.reference_lines_truncated,
                            clause_line: support.reference_lines.first().copied(), contract_type_key: support.target.contract_type_key.clone(),
                            contract_type: support.target.contract_type.clone(),
                        });
                    }
                }
            }
            Ok(output)
        })()?;
        for reason in &mut output.coverage.reasons {
            *reason = reason.replace("property_reference_", "interface_implementation_");
        }
        output.coverage.reasons.sort();
        Ok(output)
    }

    /// Inspect one exact source's outgoing Calls adjacency under one graph
    /// transaction. At most 64 rows are decoded and one further key establishes
    /// a boundary; non-property/rejected rows consume the same allowance.
    /// Selection is supplied by the caller's held source capability. Exclusions
    /// are explicit policy filters, never navigation-scope or same-file filters.
    pub fn bounded_property_references(
        &self,
        project_id: &str,
        source_id: &str,
        admit: &dyn Fn(&Node, Option<&Node>) -> anyhow::Result<ImpactAdmission>,
        excluded: &dyn Fn(&Node) -> bool,
    ) -> anyhow::Result<PropertyReferences> {
        self.bounded_declaration_references_inner(project_id, source_id, admit, excluded, None)
    }

    /// Opt-in callable leads from the same bounded outgoing Calls scan.
    /// Properties retain their original work/capacity priority. At most sixteen
    /// metadata-free callable candidates wait for admission after that pass;
    /// their target/file reads spend only its remaining budget and transaction.
    pub fn bounded_declaration_references(
        &self,
        project_id: &str,
        source_id: &str,
        admit: &dyn Fn(&Node, Option<&Node>) -> anyhow::Result<ImpactAdmission>,
        excluded: &dyn Fn(&Node) -> bool,
    ) -> anyhow::Result<(PropertyReferences, CallableReferences)> {
        let mut callables = CallableReferences::default();
        let properties = self.bounded_declaration_references_inner(
            project_id, source_id, admit, excluded, Some(&mut callables))?;
        callables.scan_coverage = properties.coverage.clone();
        if properties.coverage.truncated {
            gap(&mut callables.coverage, "callable_reference_shared_scan_incomplete");
        }
        callables.candidates_omitted = callables.candidates_observed - callables.references.len();
        for reason in &mut callables.coverage.reasons {
            *reason = reason.replace("property_reference_", "callable_reference_");
        }
        callables.coverage.reasons.sort();
        Ok((properties, callables))
    }

    fn bounded_declaration_references_inner(
        &self,
        project_id: &str,
        source_id: &str,
        admit: &dyn Fn(&Node, Option<&Node>) -> anyhow::Result<ImpactAdmission>,
        excluded: &dyn Fn(&Node) -> bool,
        mut callables: Option<&mut CallableReferences>,
    ) -> anyhow::Result<PropertyReferences> {
        let mut pending: Vec<CallableReference> = Vec::new();
        anyhow::ensure!(!project_id.is_empty() && project_id.len() <= 1024
            && !project_id.chars().any(char::is_control), "Invalid property-reference project identity");
        anyhow::ensure!(!source_id.is_empty() && source_id.len() <= 2048
            && !source_id.chars().any(char::is_control), "Invalid property-reference source identity");
        let mut output = PropertyReferences::default();
        let mut budget = PropertyBudget { started: Instant::now(), decode_bytes: 0, retained_bytes: 0, halted: false };
        let tx = self.db.begin_read()?;
        let nodes = tx.open_table(NODES)?;
        let edges = tx.open_table(EDGES)?;
        let outgoing = tx.open_table(ADJ_OUT)?;
        let read = |id: &str, budget: &mut PropertyBudget, coverage: &mut ImpactWalkCoverage| -> anyhow::Result<Option<Node>> {
            if !budget.live(coverage) { return Ok(None); }
            let key = format!("{project_id}\0{id}");
            let Some(raw) = nodes.get(key.as_str())? else {
                coverage.missing_nodes += 1;
                gap(coverage, "property_reference_node_or_file_missing");
                return Ok(None);
            };
            if !budget.record(raw.value().len(), coverage) { return Ok(None); }
            let node: Node = bincode::DefaultOptions::new().with_fixint_encoding()
                .with_limit(PROPERTY_RECORD_BYTES as u64).reject_trailing_bytes().deserialize(raw.value())?;
            anyhow::ensure!(node.node_id == id, "Property-reference node identity disagrees with its key");
            if !property_identity_bounded(&node) || node.namespace != engram_core::namespaces::NAMESPACE_MEMORY {
                coverage.unknown_nodes += 1;
                gap(coverage, "property_reference_identity_or_namespace_unavailable");
                return Ok(None);
            }
            Ok(Some(node))
        };
        let selected = |node: &Node, budget: &mut PropertyBudget, coverage: &mut ImpactWalkCoverage,
                        excluded_files: Option<&mut usize>| -> anyhow::Result<bool> {
            let file_id = format!("file:{}", node.file_path.as_str());
            let Some(file) = read(&file_id, budget, coverage)? else { return Ok(false) };
            if file.node_type != "file" || file.file_path != node.file_path {
                coverage.unknown_nodes += 1;
                gap(coverage, "property_reference_canonical_file_unavailable");
                return Ok(false);
            }
            if let Some(count) = excluded_files {
                if excluded(&file) {
                    *count += 1;
                    gap(coverage, "property_reference_canonical_file_excluded");
                    return Ok(false);
                }
            }
            if !budget.live(coverage) { return Ok(false); }
            let node_admission = admit(node, Some(&file))?;
            if !budget.live(coverage) { return Ok(false); }
            let file_admission = admit(&file, Some(&file))?;
            if !budget.live(coverage) { return Ok(false); }
            match (node_admission, file_admission) {
                (ImpactAdmission::Selected, ImpactAdmission::Selected) => Ok(true),
                (ImpactAdmission::NonSelectedRevision, _) | (_, ImpactAdmission::NonSelectedRevision) => {
                    coverage.non_selected_nodes += 1;
                    gap(coverage, "property_reference_non_selected_revision");
                    Ok(false)
                }
                _ => {
                    coverage.unknown_nodes += 1;
                    gap(coverage, "property_reference_admission_unknown");
                    Ok(false)
                }
            }
        };
        let Some(source) = read(source_id, &mut budget, &mut output.coverage)? else { return Ok(output) };
        output.coverage.nodes_observed += 1;
        if excluded(&source) {
            output.excluded_nodes += 1;
            gap(&mut output.coverage, "property_reference_source_excluded");
            return Ok(output);
        }
        if !selected(&source, &mut budget, &mut output.coverage, None)? || !budget.retain(&source, &mut output.coverage) {
            return Ok(output);
        }
        output.source = Some(identity_clone(&source));
        let prefix = adj_key(project_id, &EdgeKind::Calls, source_id);
        let mut rows = outgoing.range((prefix.as_str(), "")..)?;
        loop {
            if !budget.live(&mut output.coverage) { break; }
            let Some(entry) = rows.next() else { break };
            let (key, adjacency) = entry?;
            let (observed_prefix, target_id) = key.value();
            if observed_prefix != prefix { break; }
            // Charge before property/type/exclusion/admission checks. The extra
            // key is never decoded or used to discover another target.
            output.coverage.raw_rows += 1;
            if output.coverage.raw_rows > PROPERTY_RAW_ROWS {
                gap(&mut output.coverage, "property_reference_raw_row_limit");
                break;
            }
            if output.references.len() == PROPERTY_LEADS {
                gap(&mut output.coverage, "property_reference_lead_limit");
                break;
            }
            anyhow::ensure!(!target_id.is_empty() && target_id.len() <= 2048
                && !target_id.chars().any(char::is_control), "Invalid property-reference adjacency target");
            let _ = decode_adj_value(adjacency.value())?;
            if !budget.live(&mut output.coverage) { break; }
            let edge_id = edge_key(project_id, &EdgeKind::Calls, source_id, target_id);
            let Some(raw) = edges.get(edge_id.as_str())? else {
                output.coverage.unknown_nodes += 1;
                gap(&mut output.coverage, "property_reference_edge_missing");
                continue;
            };
            if !budget.record(raw.value().len(), &mut output.coverage) { continue; }
            let edge: Edge = bincode::DefaultOptions::new().with_fixint_encoding()
                .with_limit(PROPERTY_RECORD_BYTES as u64).reject_trailing_bytes().deserialize(raw.value())?;
            anyhow::ensure!(edge.source_id == source_id && edge.target_id == target_id && edge.edge_kind == EdgeKind::Calls,
                "Property-reference edge identity disagrees with its adjacency key");
            if edge.namespace != engram_core::namespaces::NAMESPACE_MEMORY || !edge_source_revision_matches(&edge, &source) {
                output.coverage.unknown_nodes += 1;
                gap(&mut output.coverage, "property_reference_edge_source_unverified");
                continue;
            }
            if requires_source_call_binding(&edge) {
                // Exact endpoint metadata is read in this transaction under the
                // same shared scan/record budget, before retaining any lead.
                let valid = match read(target_id, &mut budget, &mut output.coverage)? {
                    Some(target) => {
                        let source_file = read(&format!("file:{}", source.file_path.as_str()), &mut budget, &mut output.coverage)?;
                        let target_file = read(&format!("file:{}", target.file_path.as_str()), &mut budget, &mut output.coverage)?;
                        match (source_file.as_ref(), target_file.as_ref()) {
                            (Some(sf), Some(tf)) => source_call_binding_matches(&edge, &source, sf, &target, tf),
                            _ => false,
                        }
                    }
                    None => false,
                };
                if !valid {
                    output.coverage.unknown_nodes += 1;
                    gap(&mut output.coverage, "source_call_binding_unverified");
                    if let Some(observed) = callables.as_deref_mut() {
                        gap(&mut observed.coverage, "source_call_binding_unverified");
                    }
                    continue;
                }
            }
            let Some(target) = read(target_id, &mut budget, &mut output.coverage)? else { continue };
            if target.node_id != source.node_id { output.coverage.nodes_observed += 1; }
            if target.node_type != "property" {
                output.non_property_nodes += 1;
                if let Some(observed) = callables.as_deref_mut() {
                    if matches!(target.node_type.as_str(), "function" | "method" | "constructor" | "sub" | "procedure") {
                        observed.candidates_observed += 1;
                        if pending.len() == CALLABLE_PENDING_LEADS {
                            gap(&mut observed.coverage, "callable_reference_pending_lead_limit");
                        } else {
                            let (source_line, source_position_kind, reference_lines, reference_lines_truncated) = recorded_positions(&edge);
                            let bytes = identity_bytes(&target) + reference_lines.len() * std::mem::size_of::<u32>();
                            if observed.pending_bytes.checked_add(bytes).is_none_or(|n| n > CALLABLE_PENDING_BYTES) {
                                gap(&mut observed.coverage, "callable_reference_pending_byte_limit");
                            } else {
                                observed.pending_bytes += bytes;
                                let extractor_anchor_line = edge.metadata.as_ref().and_then(|m| m.get("src_line")).and_then(positive_line);
                                pending.push(CallableReference { target: identity_clone(&target), edge_generation: edge.generation,
                                    source_line, source_position_kind, extractor_anchor_line, reference_lines, reference_lines_truncated });
                            }
                        }
                    }
                }
                continue;
            }
            if excluded(&target) {
                output.excluded_nodes += 1;
                gap(&mut output.coverage, "property_reference_target_excluded");
                continue;
            }
            if !selected(&target, &mut budget, &mut output.coverage, None)? || !budget.retain(&target, &mut output.coverage) {
                continue;
            }
            let (source_line, source_position_kind, reference_lines, reference_lines_truncated) = recorded_positions(&edge);
            let extractor_anchor_line = edge.metadata.as_ref().and_then(|m| m.get("src_line")).and_then(positive_line);
            output.references.push(PropertyReference { target: identity_clone(&target), edge_generation: edge.generation,
                source_line, source_position_kind, extractor_anchor_line, reference_lines, reference_lines_truncated });
        }
        output.coverage.reasons.sort();
        if let Some(observed) = callables {
            for mut candidate in pending {
                if !budget.live(&mut observed.coverage) { break; }
                // Re-read only a known identity in this same transaction. Its
                // arbitrary metadata was never retained alongside properties.
                let target = match read(&candidate.target.node_id, &mut budget, &mut observed.coverage) {
                    Ok(Some(target)) => target,
                    Ok(None) => continue,
                    Err(_) => {
                        observed.coverage.unknown_nodes += 1;
                        gap(&mut observed.coverage, "callable_reference_target_read_failed");
                        continue;
                    }
                };
                observed.coverage.nodes_observed += 1;
                if excluded(&target) {
                    observed.excluded_nodes += 1;
                    gap(&mut observed.coverage, "callable_reference_target_excluded");
                    continue;
                }
                let admitted = match selected(&target, &mut budget, &mut observed.coverage, Some(&mut observed.excluded_nodes)) {
                    Ok(admitted) => admitted,
                    Err(_) => {
                        observed.coverage.unknown_nodes += 1;
                        gap(&mut observed.coverage, "callable_reference_file_read_or_admission_failed");
                        false
                    }
                };
                if !admitted || !budget.retain(&target, &mut observed.coverage) { continue; }
                candidate.target = identity_clone(&target);
                observed.references.push(candidate);
            }
        }
        Ok(output)
    }

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
                    if requires_source_call_binding(&edge) {
                        // The traversal cache retains identities only. Re-read
                        // full endpoint records rather than admitting stripped
                        // or old cached metadata after a target republication.
                        let full_source = read_node(source, &mut budget, &mut output.coverage)?;
                        let full_target = read_node(&target, &mut budget, &mut output.coverage)?;
                        let valid = if let (Some(full_source), Some(full_target)) = (full_source.as_ref(), full_target.as_ref()) {
                            let source_file = read_node(&format!("file:{}", full_source.file_path.as_str()), &mut budget, &mut output.coverage)?;
                            let target_file = read_node(&format!("file:{}", full_target.file_path.as_str()), &mut budget, &mut output.coverage)?;
                            match (source_file.as_ref(), target_file.as_ref()) {
                                (Some(sf), Some(tf)) => source_call_binding_matches(&edge, full_source, sf, full_target, tf),
                                _ => false,
                            }
                        } else { false };
                        if !valid {
                            output.coverage.unknown_nodes += 1;
                            gap(&mut output.coverage, "source_call_binding_unverified");
                            record_frontier(&mut output.frontier, &mut output.coverage, &mut budget, output.witnesses.len(),
                                source, Some(&target), Some(kind), Some(node), depth + 1, "source_call_binding_unverified");
                            continue;
                        }
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

    fn interface_navigation_node(path: &str, name: &str, line: u32, contract: bool) -> Node {
        let id = engram_core::ids::NodeId::symbol("function", None, path, name, line).0;
        let mut value = node(&id, path, 7);
        value.name = name.into(); value.start_line = line; value.end_line = line + 5;
        value.metadata = Some(serde_json::json!({"semantic_member_id":format!("M:{name}:{line}"),
            "semantic_span_start":line.to_string(),"semantic_span_length":"5",
            "interface_member_declaration":if contract {"true"} else {"false"}}));
        value
    }

    fn interface_navigation_file(path: &str) -> Node {
        let mut value = file(path, 7);
        value.metadata = Some(serde_json::json!({"file_hash":"b".repeat(64),
            "vb_syntax_source_sha256":"c".repeat(64),"vb_parser_provenance":{"backend":"roslyn","parser":"fixture"}}));
        value
    }

    fn interface_navigation_edge(source: &Node, contract: &Node, kind: EdgeKind, closed: char) -> Edge {
        let binding = InterfaceBinding {
            target: InterfaceMemberLocator { version:1, name:contract.name.clone(),
                semantic_id:contract.metadata.as_ref().unwrap()["semantic_member_id"].as_str().unwrap().into(),
                file_path:contract.file_path.as_str().into(),start_line:contract.start_line,
                span_start:contract.start_line,span_length:5,syntax_source_sha256:"c".repeat(64),
                contract_type_key:closed.to_string().repeat(64),contract_type:format!("IBox(Of {closed})") },
            source_semantic_id:source.metadata.as_ref().unwrap()["semantic_member_id"].as_str().unwrap().into(),
            source_generation:7,source_file_hash:"b".repeat(64),target_generation:7,target_file_hash:"b".repeat(64),
            parser:serde_json::json!({"backend":"roslyn","parser":"fixture"}),
            reference_lines:vec![source.start_line+1],reference_lines_truncated:false,
        };
        let mut value = edge(&source.node_id, &contract.node_id, kind, 7);
        value.metadata = Some(serde_json::json!({INTERFACE_BINDINGS:InterfaceBindings {
            version:1,complete:true,bindings:vec![binding]}}));
        value
    }

    fn interface_navigation_fixture(store: &GraphStore) -> anyhow::Result<(Node, Node, Node, Node)> {
        let caller = interface_navigation_node("Caller.vb", "Controller.Read", 10, false);
        let contract = interface_navigation_node("Contract.vb", "IBox.Read", 6, true);
        let alpha = interface_navigation_node("Service.vb", "Service.FetchAlpha", 20, false);
        let beta = interface_navigation_node("Service.vb", "Service.FetchBeta", 30, false);
        store.upsert_nodes_and_edges(PROJECT, &[caller.clone(),contract.clone(),alpha.clone(),beta.clone(),
            interface_navigation_file("Caller.vb"),interface_navigation_file("Contract.vb"),interface_navigation_file("Service.vb")],
            &[interface_navigation_edge(&caller,&contract,EdgeKind::Calls,'a'),
                interface_navigation_edge(&alpha,&contract,EdgeKind::Implements,'a'),
                interface_navigation_edge(&beta,&contract,EdgeKind::Implements,'b')])?;
        Ok((caller,contract,alpha,beta))
    }

    #[test]
    fn interface_implementations_follow_exact_closed_contract_without_inventing_direct_calls() -> anyhow::Result<()> {
        owned_case(|store| {
            let (caller,contract,alpha,beta) = interface_navigation_fixture(store)?;
            let admit = |_: &Node, _: Option<&Node>| Ok(ImpactAdmission::Selected);
            let before = store.bounded_declaration_references(PROJECT,&caller.node_id,&admit,&|_|false)?;
            let output = store.bounded_interface_implementations(PROJECT,&caller.node_id,&[contract.node_id.clone()],&admit,&|_|false)?;
            assert_eq!(output.routes.len(),1,"Different constructed contract must not become a possible target");
            let route = &output.routes[0];
            assert_eq!(route.implementation.node_id,alpha.node_id);
            assert_ne!(route.implementation.node_id,beta.node_id);
            assert_eq!(route.contract.node_id,contract.node_id);
            assert_eq!(route.caller.node_id,caller.node_id);
            assert_eq!(route.reference_lines,[11]);
            assert_eq!(route.clause_line,Some(21));
            assert!(route.caller.metadata.is_none() && route.contract.metadata.is_none() && route.implementation.metadata.is_none());
            assert!(!output.coverage.truncated);
            let after = store.bounded_declaration_references(PROJECT,&caller.node_id,&admit,&|_|false)?;
            assert_eq!(serde_json::to_value(before)?,serde_json::to_value(after)?,"Optional routes cannot alter direct call evidence");

            let overload = interface_navigation_node("Contract.vb","IBox.Read",12,true);
            let wrong = interface_navigation_node("Other.vb","Other.Fetch",20,false);
            store.upsert_nodes_and_edges(PROJECT,&[overload.clone(),wrong.clone(),interface_navigation_file("Other.vb")],
                &[interface_navigation_edge(&wrong,&overload,EdgeKind::Implements,'a')])?;
            let output = store.bounded_interface_implementations(PROJECT,&caller.node_id,&[overload.node_id],&admit,&|_|false)?;
            assert!(output.routes.is_empty(),"A supplied same-name overload without its exact call edge is not a route");
            assert!(output.coverage.truncated);
            Ok(())
        })
    }

    #[test]
    fn interface_implementations_reject_stale_excluded_and_unverified_endpoint_files() -> anyhow::Result<()> {
        owned_case(|store| {
            let (caller,contract,alpha,_) = interface_navigation_fixture(store)?;
            let ids = vec![contract.node_id.clone()];
            let admit = |_: &Node, _: Option<&Node>| Ok(ImpactAdmission::Selected);
            let excluded = store.bounded_interface_implementations(PROJECT,&caller.node_id,&ids,&admit,
                &|n|n.node_type=="file" && n.file_path.as_str()=="Service.vb")?;
            assert!(excluded.routes.is_empty() && excluded.excluded_nodes==2);
            let nonselected = store.bounded_interface_implementations(PROJECT,&caller.node_id,&ids,
                &|n,_|Ok(if n.node_type=="file" && n.file_path.as_str()=="Service.vb" {ImpactAdmission::NonSelectedRevision} else {ImpactAdmission::Selected}),&|_|false)?;
            assert!(nonselected.routes.is_empty() && nonselected.coverage.non_selected_nodes==2);
            let mut old = interface_navigation_edge(&alpha,&contract,EdgeKind::Implements,'a');
            old.metadata.as_mut().unwrap()[SOURCE_ORIGIN] = serde_json::json!({"version":1,"source_id":alpha.node_id,"source_generation":6});
            store.upsert_nodes_and_edges(PROJECT,&[],&[old])?;
            let stale = store.bounded_interface_implementations(PROJECT,&caller.node_id,&ids,&admit,&|_|false)?;
            assert!(stale.routes.is_empty());
            assert!(stale.coverage.reasons.iter().any(|s|s=="interface_implementation_mapping_source_unverified"));
            // Restore the mapping, but invalidate the selected source file hash.
            let mut source_file = interface_navigation_file("Caller.vb");
            source_file.metadata.as_mut().unwrap()["file_hash"] = serde_json::json!("d".repeat(64));
            store.upsert_nodes_and_edges(PROJECT,&[source_file],&[interface_navigation_edge(&alpha,&contract,EdgeKind::Implements,'a')])?;
            let unverified = store.bounded_interface_implementations(PROJECT,&caller.node_id,&ids,&admit,&|_|false)?;
            assert!(unverified.routes.is_empty() && unverified.coverage.raw_rows==0);
            assert!(unverified.coverage.reasons.iter().any(|s|s=="interface_implementation_call_binding_unverified"));
            Ok(())
        })
    }

    #[test]
    fn interface_implementations_bound_rejected_work_and_disclose_omitted_routes() -> anyhow::Result<()> {
        owned_case(|store| {
            let (caller,contract,_,_) = interface_navigation_fixture(store)?;
            let admit = |_: &Node, _: Option<&Node>| Ok(ImpactAdmission::Selected);
            let mut values = Vec::new(); let mut edges = Vec::new();
            for index in 0..135 {
                let implementation = interface_navigation_node("Service.vb", &format!("Service.A{index:03}"), 40+index, false);
                edges.push(interface_navigation_edge(&implementation,&contract,EdgeKind::Implements,'a'));
                values.push(implementation);
            }
            store.upsert_nodes_and_edges(PROJECT,&values,&edges)?;
            let excluded = store.bounded_interface_implementations(PROJECT,&caller.node_id,&[contract.node_id.clone()],&admit,
                &|n|n.file_path.as_str()=="Service.vb")?;
            assert!(excluded.routes.is_empty());
            assert_eq!(excluded.coverage.raw_rows,129,"128 rejected rows and one undecoded boundary");
            assert_eq!(excluded.excluded_nodes,128);
            assert!(excluded.coverage.reasons.iter().any(|s|s=="interface_implementation_raw_row_limit"));
            let limited = store.bounded_interface_implementations(PROJECT,&caller.node_id,&[contract.node_id],&admit,&|_|false)?;
            assert_eq!(limited.routes.len(),16);
            assert!(limited.coverage.reasons.iter().any(|s|s=="interface_implementation_route_limit"));
            Ok(())
        })
    }

    fn property(id: &str, path: &str, generation: u64) -> Node {
        let mut value = node(id, path, generation);
        value.node_type = "property".into();
        value
    }

    #[test]
    fn property_references_admit_selected_cross_file_and_inherited_source_edges() -> anyhow::Result<()> {
        owned_case(|store| {
            let source = node("source", "src/producer.vb", 1);
            let first = property("property:first", "src/model.vb", 2);
            let second = property("property:second", "src/other.vb", 4);
            let mut direct = edge(&source.node_id, &first.node_id, EdgeKind::Calls, 1);
            direct.metadata = Some(serde_json::json!({"src_line":10,"call_site_lines":[15,12,15],"via":"getter"}));
            let mut inherited = edge(&source.node_id, &second.node_id, EdgeKind::Calls, 4);
            inherited.metadata = Some(serde_json::json!({"src_line":10,"call_site_line":17,
                "_engram_source_origin":{"version":1,"source_id":"source","source_generation":1}}));
            store.upsert_nodes_and_edges(PROJECT, &[source,first,second,file("src/producer.vb",1),file("src/model.vb",2),file("src/other.vb",4)], &[direct,inherited])?;
            let output = store.bounded_property_references(PROJECT,"source",&|n,f| selected(&[("src/producer.vb",1),("src/model.vb",2),("src/other.vb",4)],n,f),&|_| false)?;
            assert_eq!(output.references.len(),2);
            assert_eq!(output.coverage.raw_rows,2);
            assert!(!output.coverage.truncated);
            assert!(output.source.as_ref().unwrap().metadata.is_none());
            assert!(output.references.iter().all(|r| r.target.metadata.is_none()));
            let first = output.references.iter().find(|r| r.target.node_id=="property:first").unwrap();
            assert_eq!(first.target.generation,2);
            assert_eq!(first.edge_generation,1);
            assert_eq!(first.reference_lines,[12,15]);
            assert_eq!(first.source_line,Some(12));
            assert_eq!(first.extractor_anchor_line,Some(10));
            assert_eq!(first.source_position_kind,"call_reference_syntax_metadata");
            let inherited = output.references.iter().find(|r| r.target.node_id=="property:second").unwrap();
            assert_eq!(inherited.edge_generation,4);
            assert_eq!(inherited.target.generation,4);
            assert_eq!(inherited.source_line,Some(17));
            // The misleading extractor `via` is neither copied nor interpreted.
            assert!(serde_json::to_value(&output)?.to_string().find("getter").is_none());
            Ok(())
        })
    }

    #[test]
    fn property_references_reject_stale_origins_nonselected_missing_and_excluded_targets() -> anyhow::Result<()> {
        owned_case(|store| {
            let mut nodes = vec![node("source","src/source.vb",2),file("src/source.vb",2),file("src/model.vb",2)];
            let mut edges = Vec::new();
            for (id,generation) in [("accepted",2),("stale-origin",2),("old-property",1),("excluded",2)] {
                nodes.push(property(id,"src/model.vb",generation));
                let mut connection=edge("source",id,EdgeKind::Calls,2);
                if id=="stale-origin" { connection.metadata=Some(serde_json::json!({
                    "_engram_source_origin":{"version":1,"source_id":"source","source_generation":1}})); }
                edges.push(connection);
            }
            nodes.push(property("no-file","src/absent.vb",2));
            edges.push(edge("source","no-file",EdgeKind::Calls,2));
            edges.push(edge("source","missing-node",EdgeKind::Calls,2));
            store.upsert_nodes_and_edges(PROJECT,&nodes,&edges)?;
            let output=store.bounded_property_references(PROJECT,"source",&|n,f| selected(&[("src/source.vb",2),("src/model.vb",2),("src/absent.vb",2)],n,f),&|n| n.node_id=="excluded")?;
            assert_eq!(output.references.iter().map(|r|r.target.node_id.as_str()).collect::<Vec<_>>(),["accepted"]);
            assert_eq!(output.coverage.non_selected_nodes,1);
            assert_eq!(output.coverage.missing_nodes,2);
            assert_eq!(output.coverage.unknown_nodes,1);
            assert_eq!(output.excluded_nodes,1);
            assert!(output.coverage.truncated);
            for reason in ["property_reference_edge_source_unverified","property_reference_non_selected_revision",
                "property_reference_node_or_file_missing","property_reference_target_excluded"] {
                assert!(output.coverage.reasons.iter().any(|r|r==reason),"{reason}");
            }
            assert_eq!(output.references[0].source_line,None);
            assert_eq!(output.references[0].source_position_kind,"not_recorded");
            let unavailable=store.bounded_property_references(PROJECT,"source",&|n,f|selected(&[("src/source.vb",3)],n,f),&|_|false)?;
            assert!(unavailable.source.is_none() && unavailable.references.is_empty());
            assert_eq!(unavailable.coverage.raw_rows,0);
            assert_eq!(unavailable.coverage.non_selected_nodes,1);
            Ok(())
        })
    }

    #[test]
    fn property_references_raw_limit_includes_nonproperty_rejections() -> anyhow::Result<()> {
        owned_case(|store| {
            let mut nodes=vec![node("source","src/source.vb",1),file("src/source.vb",1),property("z-property","src/source.vb",1)];
            let mut edges=vec![edge("source","z-property",EdgeKind::Calls,1)];
            for index in 0..70 {
                let id=format!("a-function-{index:03}");
                nodes.push(node(&id,"src/source.vb",1));edges.push(edge("source",&id,EdgeKind::Calls,1));
            }
            store.upsert_nodes_and_edges(PROJECT,&nodes,&edges)?;
            let output=store.bounded_property_references(PROJECT,"source",&|n,f|selected(&[("src/source.vb",1)],n,f),&|_|false)?;
            assert!(output.references.is_empty());
            assert_eq!(output.coverage.raw_rows,65,"64 examined rows plus one undecoded lookahead");
            assert_eq!(output.non_property_nodes,64);
            assert!(output.coverage.reasons.iter().any(|r|r=="property_reference_raw_row_limit"));
            let (properties,callables)=store.bounded_declaration_references(PROJECT,"source",&|n,f|selected(&[("src/source.vb",1)],n,f),&|_|false)?;
            assert_eq!(serde_json::to_value(&properties)?,serde_json::to_value(&output)?);
            assert_eq!(callables.scan_coverage.raw_rows,65);
            assert_eq!(callables.candidates_observed,64);
            assert_eq!(callables.references.len(),16);
            assert_eq!(callables.candidates_omitted,48);
            assert!(callables.coverage.truncated,"Raw lookahead must remain a callable coverage gap");
            Ok(())
        })
    }

    #[test]
    fn property_references_lead_limit_preserves_recorded_position_gaps() -> anyhow::Result<()> {
        owned_case(|store| {
            let mut nodes=vec![node("source","src/source.vb",1),file("src/source.vb",1)];
            let mut edges=Vec::new();
            for index in 0..9 {
                let id=format!("property-{index}");nodes.push(property(&id,"src/source.vb",1));
                let mut connection=edge("source",&id,EdgeKind::Calls,1);
                connection.metadata=Some(serde_json::json!({"call_site_lines":(1..=12).collect::<Vec<u32>>()}));
                edges.push(connection);
            }
            store.upsert_nodes_and_edges(PROJECT,&nodes,&edges)?;
            let output=store.bounded_property_references(PROJECT,"source",&|n,f|selected(&[("src/source.vb",1)],n,f),&|_|false)?;
            assert_eq!(output.references.len(),8);
            assert_eq!(output.coverage.raw_rows,9);
            assert!(output.references.iter().all(|r|r.reference_lines==[1,2,3,4,5,6,7,8] && r.reference_lines_truncated));
            assert!(output.coverage.reasons.iter().any(|r|r=="property_reference_lead_limit"));
            Ok(())
        })
    }

    #[test]
    fn property_references_bound_record_and_shared_decode_work() -> anyhow::Result<()> {
        owned_case(|store| {
            let mut oversized=property("a-oversized","src/source.vb",1);
            oversized.metadata=Some(serde_json::json!({"source_identity":"fixture","payload":"x".repeat(70*1024)}));
            store.upsert_nodes_and_edges(PROJECT,&[node("source","src/source.vb",1),file("src/source.vb",1),
                oversized,property("b-valid","src/source.vb",1)],
                &[edge("source","a-oversized",EdgeKind::Calls,1),edge("source","b-valid",EdgeKind::Calls,1)])?;
            let output=store.bounded_property_references(PROJECT,"source",&|n,f|selected(&[("src/source.vb",1)],n,f),&|_|false)?;
            assert_eq!(output.references.len(),1);
            assert_eq!(output.references[0].target.node_id,"b-valid");
            assert_eq!(output.coverage.unknown_nodes,1);
            assert!(output.coverage.reasons.iter().any(|r|r=="property_reference_record_byte_limit"));
            Ok(())
        })?;
        owned_case(|store| {
            let mut nodes=vec![node("source","src/source.vb",1),file("src/source.vb",1),property("z-property","src/source.vb",1)];
            let mut edges=vec![edge("source","z-property",EdgeKind::Calls,1)];
            for index in 0..60 {
                let id=format!("a-large-function-{index:03}");let mut target=node(&id,"src/source.vb",1);
                target.metadata=Some(serde_json::json!({"source_identity":"fixture","payload":"x".repeat(50*1024)}));
                nodes.push(target);edges.push(edge("source",&id,EdgeKind::Calls,1));
            }
            store.upsert_nodes_and_edges(PROJECT,&nodes,&edges)?;
            let output=store.bounded_property_references(PROJECT,"source",&|n,f|selected(&[("src/source.vb",1)],n,f),&|_|false)?;
            assert!(output.references.is_empty());
            assert!(output.coverage.raw_rows<64,"shared decoding stops before adjacency capacity");
            assert!(output.coverage.reasons.iter().any(|r|r=="property_reference_decode_byte_limit"));
            let (properties,callables)=store.bounded_declaration_references(PROJECT,"source",&|n,f|selected(&[("src/source.vb",1)],n,f),&|_|false)?;
            assert_eq!(serde_json::to_value(&properties)?,serde_json::to_value(&output)?);
            assert!(callables.candidates_observed>0 && callables.references.is_empty(),
                "Callable admission must not reset a decode budget exhausted by the property pass");
            assert_eq!(callables.candidates_omitted,callables.candidates_observed);
            assert!(callables.scan_coverage.reasons.iter().any(|r|r=="property_reference_decode_byte_limit"));
            assert!(callables.coverage.truncated);
            Ok(())
        })
    }

    #[test]
    fn property_references_require_exact_source_and_canonical_memory_file() -> anyhow::Result<()> {
        owned_case(|store| {
            let mut wrong_file=file("src/source.vb",1);wrong_file.node_type="class".into();
            store.upsert_nodes_and_edges(PROJECT,&[node("source","src/source.vb",1),wrong_file,
                property("property","src/source.vb",1)],&[edge("source","property",EdgeKind::Calls,1)])?;
            let no_fallback=store.bounded_property_references(PROJECT,"source-not-present",&|_,_|Ok(ImpactAdmission::Selected),&|_|false)?;
            assert!(no_fallback.source.is_none() && no_fallback.references.is_empty());
            assert_eq!(no_fallback.coverage.missing_nodes,1);
            let wrong_kind=store.bounded_property_references(PROJECT,"source",&|_,_|Ok(ImpactAdmission::Selected),&|_|false)?;
            assert!(wrong_kind.source.is_none() && wrong_kind.references.is_empty());
            assert_eq!(wrong_kind.coverage.raw_rows,0);
            assert!(wrong_kind.coverage.reasons.iter().any(|r|r=="property_reference_canonical_file_unavailable"));
            let mut foreign=file("src/source.vb",1);foreign.namespace="history".into();
            store.upsert_nodes_and_edges(PROJECT,&[foreign],&[])?;
            let nonmemory=store.bounded_property_references(PROJECT,"source",&|_,_|Ok(ImpactAdmission::Selected),&|_|false)?;
            assert!(nonmemory.source.is_none() && nonmemory.references.is_empty());
            assert!(nonmemory.coverage.reasons.iter().any(|r|r=="property_reference_identity_or_namespace_unavailable"));
            Ok(())
        })
    }

    #[test]
    fn declaration_references_reach_fourteenth_callee_with_revision_checked_positions() -> anyhow::Result<()> {
        owned_case(|store| {
            let mut nodes = vec![node("source", "src/controller.vb", 1), file("src/controller.vb", 1), file("src/service.vb", 3)];
            let mut edges = Vec::new();
            for index in 0..14 {
                let id = format!("callee-{index:02}");
                let mut target = node(&id, "src/service.vb", 3);
                target.node_type = ["function", "method", "constructor", "sub", "procedure"][index % 5].into();
                target.metadata.as_mut().unwrap()["unrelated_payload"] = serde_json::json!("x".repeat(1024));
                nodes.push(target);
                let mut call = edge("source", &id, EdgeKind::Calls, 3);
                call.metadata = Some(serde_json::json!({"src_line":10,"call_site_lines":[27,25,27],
                    "_engram_source_origin":{"version":1,"source_id":"source","source_generation":1}}));
                edges.push(call);
            }
            store.upsert_nodes_and_edges(PROJECT, &nodes, &edges)?;
            let admit = |n: &Node, f: Option<&Node>| selected(&[("src/controller.vb",1),("src/service.vb",3)], n, f);
            let baseline = store.bounded_property_references(PROJECT, "source", &admit, &|_| false)?;
            let (properties, callables) = store.bounded_declaration_references(PROJECT, "source", &admit, &|_| false)?;
            assert_eq!(serde_json::to_value(&properties)?, serde_json::to_value(&baseline)?);
            assert_eq!(callables.references.len(), 14);
            assert_eq!(callables.candidates_omitted, 0);
            assert!(!callables.coverage.truncated && !callables.scan_coverage.truncated);
            let last = callables.references.iter().find(|r| r.target.node_id == "callee-13").unwrap();
            assert_eq!(last.target.generation, 3);
            assert_eq!(last.edge_generation, 3);
            assert_eq!(properties.source.unwrap().generation, 1);
            assert_eq!(last.reference_lines, [25,27]);
            assert_eq!(last.source_line, Some(25));
            assert_eq!(last.extractor_anchor_line, Some(10));
            assert_eq!(last.source_position_kind, "call_reference_syntax_metadata");
            assert!(callables.references.iter().all(|r| r.target.metadata.is_none()));
            assert!(!serde_json::to_string(&callables)?.contains("unrelated_payload"));
            Ok(())
        })
    }

    #[test]
    fn declaration_references_reject_unadmitted_callees_and_preserve_shared_scan_gaps() -> anyhow::Result<()> {
        owned_case(|store| {
            let mut nodes = vec![node("source", "src/source.vb", 2), file("src/source.vb", 2), file("src/model.vb", 2),
                file("src/excluded-file.vb",2)];
            let mut edges = Vec::new();
            for (id,path,generation) in [("accepted","src/model.vb",2), ("old-target","src/model.vb",1),
                ("excluded","src/model.vb",2), ("excluded-file","src/excluded-file.vb",2),
                ("no-file","src/absent.vb",2), ("stale-edge","src/model.vb",2), ("foreign","src/model.vb",2)] {
                let mut target = node(id,path,generation);
                if id == "foreign" { target.namespace = "history".into(); }
                nodes.push(target);
                edges.push(edge("source",id,EdgeKind::Calls,if id == "stale-edge" {1} else {2}));
            }
            edges.push(edge("source","missing-target",EdgeKind::Calls,2));
            store.upsert_nodes_and_edges(PROJECT,&nodes,&edges)?;
            let admit = |n: &Node, f: Option<&Node>| selected(&[("src/source.vb",2),("src/model.vb",2),
                ("src/absent.vb",2),("src/excluded-file.vb",2)],n,f);
            let excluded = |n: &Node| n.node_id == "excluded" || n.node_id == "file:src/excluded-file.vb";
            let baseline = store.bounded_property_references(PROJECT,"source",&admit,&excluded)?;
            let (properties, callables) = store.bounded_declaration_references(PROJECT,"source",&admit,&excluded)?;
            assert_eq!(serde_json::to_value(&properties)?,serde_json::to_value(&baseline)?);
            assert_eq!(callables.references.iter().map(|r|r.target.node_id.as_str()).collect::<Vec<_>>(),["accepted"]);
            assert_eq!(callables.candidates_observed,5);
            assert_eq!(callables.candidates_omitted,4);
            assert_eq!(callables.coverage.non_selected_nodes,1);
            assert_eq!(callables.coverage.missing_nodes,1);
            assert_eq!(callables.excluded_nodes,2);
            assert!(callables.coverage.truncated && callables.scan_coverage.truncated);
            assert_eq!(serde_json::to_value(&callables.scan_coverage)?,serde_json::to_value(&properties.coverage)?);
            for reason in ["property_reference_edge_source_unverified", "property_reference_identity_or_namespace_unavailable", "property_reference_node_or_file_missing"] {
                assert!(callables.scan_coverage.reasons.iter().any(|r|r==reason),"{reason}");
            }
            let (_, unavailable) = store.bounded_declaration_references(PROJECT,"source",
                &|n,f| selected(&[("src/source.vb",3)],n,f),&|_|false)?;
            assert!(unavailable.references.is_empty() && unavailable.scan_coverage.truncated);
            assert_eq!(unavailable.scan_coverage.raw_rows,0);
            Ok(())
        })
    }

    #[test]
    fn declaration_references_callable_crowding_never_displaces_later_properties() -> anyhow::Result<()> {
        owned_case(|store| {
            let mut nodes=vec![node("source","src/source.vb",1),file("src/source.vb",1)];
            let mut edges=Vec::new();
            for index in 0..30 {
                let id=format!("a-callee-{index:02}");
                nodes.push(node(&id,"src/source.vb",1));edges.push(edge("source",&id,EdgeKind::Calls,1));
            }
            for index in 0..8 {
                let id=format!("z-property-{index}");
                nodes.push(property(&id,"src/source.vb",1));edges.push(edge("source",&id,EdgeKind::Calls,1));
            }
            store.upsert_nodes_and_edges(PROJECT,&nodes,&edges)?;
            let admit=|n: &Node,f: Option<&Node>|selected(&[("src/source.vb",1)],n,f);
            let baseline=store.bounded_property_references(PROJECT,"source",&admit,&|_|false)?;
            let (properties,callables)=store.bounded_declaration_references(PROJECT,"source",&admit,&|_|false)?;
            assert_eq!(properties.references.len(),8);
            assert_eq!(serde_json::to_value(&properties)?,serde_json::to_value(&baseline)?);
            assert_eq!(callables.references.len(),16);
            assert_eq!(callables.candidates_observed,30);
            assert_eq!(callables.candidates_omitted,14);
            assert!(callables.coverage.reasons.iter().any(|r|r=="callable_reference_pending_lead_limit"));
            assert!(!properties.coverage.truncated,"Pending callable overflow is not a lost property scan");
            Ok(())
        })
    }

    #[test]
    fn declaration_references_optional_admission_errors_preserve_properties_and_other_callees() -> anyhow::Result<()> {
        for corrupt_file in [false,true] {
            owned_case(|store| {
                let nodes=[node("source","src/source.vb",1),file("src/source.vb",1),
                    node("a-broken","src/broken.vb",1),file("src/broken.vb",1),
                    node("b-good","src/source.vb",1),property("z-property","src/source.vb",1)];
                let edges=[edge("source","a-broken",EdgeKind::Calls,1),
                    edge("source","b-good",EdgeKind::Calls,1),edge("source","z-property",EdgeKind::Calls,1)];
                store.upsert_nodes_and_edges(PROJECT,&nodes,&edges)?;
                if corrupt_file {
                    let tx=store.db.begin_write()?;
                    {
                        let mut table=tx.open_table(NODES)?;
                        let key=format!("{PROJECT}\0file:src/broken.vb");
                        table.insert(key.as_str(), b"invalid node record".as_slice())?;
                    }
                    tx.commit()?;
                }
                let admit=|n: &Node,f: Option<&Node>| {
                    if !corrupt_file && n.node_id=="file:src/broken.vb" {
                        anyhow::bail!("fixture callable-file admission failed");
                    }
                    selected(&[("src/source.vb",1),("src/broken.vb",1)],n,f)
                };
                let baseline=store.bounded_property_references(PROJECT,"source",&admit,&|_|false)?;
                let (properties,callables)=store.bounded_declaration_references(PROJECT,"source",&admit,&|_|false)?;
                assert_eq!(properties.references.len(),1);
                assert_eq!(serde_json::to_value(&properties)?,serde_json::to_value(&baseline)?);
                assert_eq!(callables.references.iter().map(|r|r.target.node_id.as_str()).collect::<Vec<_>>(),["b-good"]);
                assert_eq!(callables.candidates_observed,2);
                assert_eq!(callables.candidates_omitted,1);
                assert!(callables.coverage.truncated && !callables.scan_coverage.truncated);
                assert!(callables.coverage.reasons.iter().any(|r|r=="callable_reference_file_read_or_admission_failed"));
                Ok(())
            })?;
        }
        Ok(())
    }

    #[test]
    fn declaration_references_bound_pending_identity_bytes_without_hiding_property_output() -> anyhow::Result<()> {
        owned_case(|store| {
            let path=format!("src/{}.vb","p".repeat(1900));
            let mut nodes=vec![node("source","src/source.vb",1),file("src/source.vb",1),file(&path,1),property("z-property","src/source.vb",1)];
            let mut edges=vec![edge("source","z-property",EdgeKind::Calls,1)];
            for index in 0..14 {
                let id=format!("a-{index:02}-{}","i".repeat(1900));
                let mut target=node(&id,&path,1);target.name="n".repeat(1900);
                nodes.push(target);edges.push(edge("source",&id,EdgeKind::Calls,1));
            }
            store.upsert_nodes_and_edges(PROJECT,&nodes,&edges)?;
            let admit=|n: &Node,f: Option<&Node>|selected(&[("src/source.vb",1),(&path,1)],n,f);
            let baseline=store.bounded_property_references(PROJECT,"source",&admit,&|_|false)?;
            let (properties,callables)=store.bounded_declaration_references(PROJECT,"source",&admit,&|_|false)?;
            assert_eq!(serde_json::to_value(&properties)?,serde_json::to_value(&baseline)?);
            assert_eq!(properties.references.len(),1);
            assert_eq!(callables.candidates_observed,14);
            assert!(callables.pending_bytes<=64*1024 && callables.candidates_omitted>0);
            assert!(!callables.references.is_empty());
            assert!(callables.coverage.reasons.iter().any(|r|r=="callable_reference_pending_byte_limit"));
            assert!(callables.references.iter().all(|r|r.target.metadata.is_none()));
            Ok(())
        })
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

    fn source_call_fixture(boolean_line: u32, guid_line: u32) -> (Vec<Node>, Edge, SourceCallBinding) {
        let mut caller = node("", "src/delete.vb", 7);
        caller.name = "Image.DeleteLayers".into();
        caller.start_line = 226;
        caller.end_line = 265;
        caller.node_id = engram_core::ids::NodeId::symbol("function", None, "src/delete.vb", &caller.name, caller.start_line).0;
        caller.metadata.as_mut().unwrap()["semantic_member_id"] = serde_json::json!("M:Image.DeleteLayers(System.Int32)");
        let mut boolean = node("", "src/log.vb", 9);
        boolean.name = "Logger.Create".into();
        boolean.start_line = boolean_line;
        boolean.end_line = boolean_line + 20;
        boolean.node_id = engram_core::ids::NodeId::symbol("function", None, "src/log.vb", &boolean.name, boolean.start_line).0;
        boolean.metadata = Some(serde_json::json!({"source_identity":"fixture",
            "semantic_member_id":"M:Logger.Create(System.String,System.String,System.String,System.Boolean,System.Boolean,System.String,System.String)",
            "semantic_span_start":"2736", "semantic_span_length":"207", "arity":"7", "arity_min":"5"}));
        let mut guid = boolean.clone();
        guid.start_line = guid_line;
        guid.end_line = guid_line + 20;
        guid.node_id = engram_core::ids::NodeId::symbol("function", None, "src/log.vb", &guid.name, guid.start_line).0;
        guid.metadata.as_mut().unwrap()["semantic_member_id"] = serde_json::json!("M:Logger.Create(System.String,System.String,System.String,System.Boolean,System.Guid,System.String)");
        guid.metadata.as_mut().unwrap()["arity"] = serde_json::json!("6");
        let parser = serde_json::json!({"backend":"roslyn", "contract":"ordinary-source-fixture-v1"});
        let mut caller_file = file("src/delete.vb", 7);
        caller_file.metadata = Some(serde_json::json!({"source_identity":"fixture", "file_hash":"a".repeat(64),
            "vb_syntax_source_sha256":"b".repeat(64), "vb_parser_provenance":parser}));
        let mut target_file = file("src/log.vb", 9);
        target_file.metadata = Some(serde_json::json!({"source_identity":"fixture", "file_hash":"c".repeat(64),
            "vb_syntax_source_sha256":"d".repeat(64), "vb_parser_provenance":parser}));
        let proof = SourceCallBinding {
            target: SourceMemberLocator { version:1, name:boolean.name.clone(),
                semantic_id:boolean.metadata.as_ref().unwrap()["semantic_member_id"].as_str().unwrap().into(),
                file_path:"src/log.vb".into(), start_line:boolean_line, span_start:2736, span_length:207,
                syntax_source_sha256:"d".repeat(64) },
            source_semantic_id:caller.metadata.as_ref().unwrap()["semantic_member_id"].as_str().unwrap().into(),
            source_generation:7, source_file_hash:"a".repeat(64), target_generation:9,
            target_file_hash:"c".repeat(64), parser, reference_lines:vec![260], reference_lines_truncated:false,
        };
        let mut call = edge(&caller.node_id, &boolean.node_id, EdgeKind::Calls, 7);
        call.metadata = Some(serde_json::json!({"source_call_status":"source_located",
            "source_call_binding_admission":"verified_source_declaration", "args":"5",
            // A free-standing extractor line must not replace admitted evidence.
            "call_site_line":261,
            SOURCE_CALL_BINDINGS:SourceCallBindings {version:1, complete:true, bindings:vec![proof.clone()]}}));
        (vec![caller, boolean, guid, caller_file, target_file], call, proof)
    }

    fn current_source_call_node(node: &Node, file: Option<&Node>) -> anyhow::Result<ImpactAdmission> {
        Ok(if file.is_some_and(|file| file.generation == node.generation && file.file_path == node.file_path) {
            ImpactAdmission::Selected
        } else { ImpactAdmission::Unknown })
    }

    #[test]
    fn source_call_selected_overload_and_witness_survive_reversed_declaration_order() -> anyhow::Result<()> {
        for (boolean_line, guid_line) in [(43,77),(77,43)] {
            owned_case(|store| {
                let (nodes, call, _) = source_call_fixture(boolean_line, guid_line);
                store.upsert_nodes_and_edges(PROJECT, &nodes, &[call.clone()])?;
                let (_, outgoing) = store.bounded_declaration_references(PROJECT, &call.source_id, &current_source_call_node, &|_|false)?;
                assert_eq!(outgoing.references.len(),1);
                assert_eq!(outgoing.references[0].target.node_id, nodes[1].node_id);
                assert_eq!(outgoing.references[0].reference_lines, vec![260]);
                let incoming = store.bounded_incoming_walk(PROJECT, &[call.target_id.clone()], 1, &causal, &current_source_call_node)?;
                assert_eq!(incoming.witnesses.len(),1);
                assert_eq!(incoming.witnesses[0].reference_lines, vec![260]);
                let unrelated = store.bounded_incoming_walk(PROJECT, &[nodes[2].node_id.clone()], 1, &causal, &current_source_call_node)?;
                assert!(unrelated.witnesses.is_empty());
                Ok(())
            })?;
        }
        Ok(())
    }

    #[test]
    fn source_call_republication_or_corrupt_proof_cannot_become_current_impact() -> anyhow::Result<()> {
        for change in ["target_signature", "target_span", "target_syntax", "target_hash", "target_generation",
            "source_signature", "source_hash", "source_generation", "parser", "missing_proof", "malformed_proof", "incomplete_proof", "unknown_admission", "interface_target"] {
            owned_case(|store| {
                let (mut nodes, mut call, _) = source_call_fixture(43,77);
                store.upsert_nodes_and_edges(PROJECT, &nodes, &[call.clone()])?;
                match change {
                    "target_signature" => nodes[1].metadata.as_mut().unwrap()["semantic_member_id"] = nodes[2].metadata.as_ref().unwrap()["semantic_member_id"].clone(),
                    "target_span" => nodes[1].metadata.as_mut().unwrap()["semantic_span_length"] = serde_json::json!("208"),
                    "target_syntax" => nodes[4].metadata.as_mut().unwrap()["vb_syntax_source_sha256"] = serde_json::json!("e".repeat(64)),
                    "target_hash" => nodes[4].metadata.as_mut().unwrap()["file_hash"] = serde_json::json!("e".repeat(64)),
                    "target_generation" => { nodes[1].generation += 1; nodes[4].generation += 1; },
                    "source_signature" => nodes[0].metadata.as_mut().unwrap()["semantic_member_id"] = serde_json::json!("M:Image.DeleteLayers(System.String)"),
                    "source_hash" => nodes[3].metadata.as_mut().unwrap()["file_hash"] = serde_json::json!("e".repeat(64)),
                    "source_generation" => { nodes[0].generation += 1; nodes[3].generation += 1;
                        // Even if edge source-revision evidence is refreshed, the old compiler binding is stale.
                        call.generation += 1; },
                    "parser" => nodes[4].metadata.as_mut().unwrap()["vb_parser_provenance"]["contract"] = serde_json::json!("different"),
                    "missing_proof" => { call.metadata.as_mut().unwrap().as_object_mut().unwrap().remove(SOURCE_CALL_BINDINGS); },
                    "malformed_proof" => call.metadata.as_mut().unwrap()[SOURCE_CALL_BINDINGS] = serde_json::json!({"version":1,"bindings":"invalid"}),
                    "incomplete_proof" => call.metadata.as_mut().unwrap()[SOURCE_CALL_BINDINGS]["complete"] = serde_json::json!(false),
                    "unknown_admission" => call.metadata.as_mut().unwrap()["source_call_binding_admission"] = serde_json::json!("unknown"),
                    "interface_target" => nodes[1].metadata.as_mut().unwrap()["interface_member_declaration"] = serde_json::json!("true"),
                    _ => unreachable!(),
                }
                store.upsert_nodes_and_edges(PROJECT, &nodes, &[call.clone()])?;
                let (_, outgoing) = store.bounded_declaration_references(PROJECT, &call.source_id, &current_source_call_node, &|_|false)?;
                assert!(outgoing.references.is_empty(), "{change}");
                assert!(outgoing.coverage.reasons.iter().any(|r| r == "source_call_binding_unverified"), "{change}: {:?}", outgoing.coverage);
                let incoming = store.bounded_incoming_walk(PROJECT, &[call.target_id.clone()], 1, &causal, &current_source_call_node)?;
                assert!(incoming.witnesses.is_empty(), "{change}");
                assert!(incoming.frontier.iter().any(|f| f.reason == "source_call_binding_unverified"), "{change}");
                Ok(())
            })?;
        }
        Ok(())
    }

    #[test]
    fn source_call_unknown_locator_never_falls_through_generic_name_resolution() -> anyhow::Result<()> {
        owned_case(|store| {
            let (mut nodes, mut call, _) = source_call_fixture(43,77);
            // Only one matching declaration exists: generic name matching would
            // otherwise produce a plausible but unsupported current call.
            nodes.remove(2);
            call.target_id = "::Logger.Create".into();
            call.metadata = Some(serde_json::json!({"source_call_status":"unknown", "args":"5"}));
            store.upsert_nodes_and_edges(PROJECT, &nodes, &[call.clone()])?;
            assert_eq!(store.resolve_symbol_edges(PROJECT)?,0);
            assert!(store.try_resolve_unresolved_target(PROJECT, &call.source_id, &call.target_id).is_none());
            let (_, outgoing) = store.bounded_declaration_references(PROJECT, &call.source_id, &current_source_call_node, &|_|false)?;
            assert!(outgoing.references.is_empty());
            assert!(outgoing.coverage.reasons.iter().any(|r| r == "source_call_binding_unverified"));
            Ok(())
        })
    }

    #[test]
    fn source_call_merge_keeps_owned_positions_bounded_and_refuses_partial_proofs() -> anyhow::Result<()> {
        let (_, call, mut proof) = source_call_fixture(43,77);
        let mut base = call.metadata.unwrap().as_object().unwrap().clone();
        proof.reference_lines = (226..258).collect();
        let extra = serde_json::json!({SOURCE_CALL_BINDINGS:SourceCallBindings {version:1, complete:true, bindings:vec![proof.clone()]}});
        merge_source_call_bindings(&mut base, extra.as_object().unwrap())?;
        let merged: SourceCallBindings = serde_json::from_value(base[SOURCE_CALL_BINDINGS].clone())?;
        assert_eq!(merged.bindings.len(),1);
        assert_eq!(merged.bindings[0].reference_lines.len(),32);
        assert!(merged.bindings[0].reference_lines_truncated);
        assert!(merged.complete);
        let mut distinct = Vec::new();
        for generation in 10..26 { let mut changed = proof.clone(); changed.target_generation = generation; distinct.push(changed); }
        let extra = serde_json::json!({SOURCE_CALL_BINDINGS:SourceCallBindings {version:1, complete:true, bindings:distinct}});
        merge_source_call_bindings(&mut base, extra.as_object().unwrap())?;
        let merged: SourceCallBindings = serde_json::from_value(base[SOURCE_CALL_BINDINGS].clone())?;
        assert_eq!(merged.bindings.len(),16);
        assert!(!merged.complete);
        let mut excessive = proof;
        excessive.reference_lines = (226..259).collect();
        let extra = serde_json::json!({SOURCE_CALL_BINDINGS:SourceCallBindings {version:1, complete:true, bindings:vec![excessive]}});
        let mut empty = serde_json::Map::new();
        assert!(merge_source_call_bindings(&mut empty, extra.as_object().unwrap()).is_err());
        assert!(empty.is_empty());
        Ok(())
    }

}
