//! Bounded area discovery for the existing instruction-drafting workflow.
//! The index routes discovery; complete captured files remain the evidence.
use super::*;
use crate::{services::project_policy, state::AppState};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

fn default_files() -> usize {
    4
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AreaSelection {
    /// Exact indexed file or directory boundary; not a filesystem crawl/glob.
    pub scope: String,
    #[serde(default = "default_files")]
    pub max_files: usize,
    /// Exact file/directory boundaries excluded from automatic discovery.
    #[serde(default)]
    pub exclude_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SelectedFile {
    pub path: String,
    pub roles: BTreeSet<String>,
    pub indexed_blake3: Option<String>,
    pub captured_blake3: Option<String>,
    pub source_status: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AreaEvidence {
    pub scope: String,
    pub excluded_paths: Vec<String>,
    pub files: Vec<SelectedFile>,
    pub symbols_and_relations: Vec<Value>,
    pub review_evidence: Value,
    pub gaps: Vec<String>,
    pub coverage: &'static str,
}

pub struct Assembly {
    pub request: MaintenanceRequest,
    pub evidence: AreaEvidence,
    // The collector and full-file observations retain the same source lease.
    pub(crate) _source: Arc<engram_index::SourceSnapshotGuard>,
}

pub(super) fn within(path: &str, scope: &str) -> bool {
    #[cfg(windows)]
    let (folded_path, folded_scope) = (path.to_lowercase(), scope.to_lowercase());
    #[cfg(windows)]
    let (path, scope) = (folded_path.as_str(), folded_scope.as_str());
    path == scope
        || path
            .strip_prefix(scope)
            .is_some_and(|tail| tail.starts_with('/'))
}

pub fn validate(request: &MaintenanceRequest) -> Result<()> {
    let selected = request
        .area_context
        .as_ref()
        .context("Area selection missing")?;
    paths(std::slice::from_ref(&selected.scope), 1)?;
    ensure!(
        selected.max_files > 0 && selected.max_files <= 8,
        "Area max_files must be 1..8"
    );
    ensure!(
        selected.exclude_paths.len() <= 16,
        "At most sixteen excluded area boundaries"
    );
    if !selected.exclude_paths.is_empty() {
        paths(&selected.exclude_paths, 16)?;
    }
    ensure!(
        !request.new_instruction_targets.is_empty(),
        "Area context requires a declared new instruction/provenance target"
    );
    ensure!(
        request.evidence_paths.len() <= 16,
        "At most sixteen explicit evidence paths"
    );
    if !request.evidence_paths.is_empty() {
        paths(&request.evidence_paths, 16)?;
    }
    for path in request
        .evidence_paths
        .iter()
        .chain(&request.instruction_paths)
    {
        ensure!(
            !selected
                .exclude_paths
                .iter()
                .any(|scope| within(path, scope)),
            "Explicit evidence is inside an excluded area: {path}"
        );
    }
    area_draft::validate_targets(
        &request.new_instruction_targets,
        &request.instruction_paths,
        &request.evidence_paths,
    )?;
    ensure!(
        request.base_commit.len() == 40
            && request.base_commit.bytes().all(|c| c.is_ascii_hexdigit()),
        "base_commit must be an exact full commit ID"
    );
    Ok(())
}

fn add(
    files: &mut BTreeMap<String, SelectedFile>,
    path: &str,
    role: &str,
    hash: Option<String>,
    gaps: &mut Vec<String>,
) {
    if !files.contains_key(path) && files.len() == 16 {
        gaps.push(format!(
            "Evidence path cap: {path} ({role}); source not captured"
        ));
        return;
    }
    let file = files.entry(path.into()).or_insert_with(|| SelectedFile {
        path: path.into(),
        roles: BTreeSet::new(),
        indexed_blake3: hash.clone(),
        captured_blake3: None,
        source_status: "pending_complete_capture".into(),
    });
    if hash.is_some() {
        file.indexed_blake3 = hash;
    }
    file.roles.insert(role.into());
}

fn indexed_hash(
    state: &AppState,
    pid: &str,
    path: &str,
    membership: &engram_index::hybrid::SelectedSourceRevisionLookup,
    gaps: &mut Vec<String>,
) -> Result<Option<String>> {
    if let Some(node) = state.graph.get_node(pid, &format!("file:{path}"))? {
        if membership.matches(path, node.generation)? == Some(true) {
            if let Some(hash) = node.metadata.and_then(|meta| {
                meta.get("file_hash")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            }) {
                return Ok(Some(hash));
            }
        }
    }
    gaps.push(format!(
        "{path}: selected-revision file fingerprint unavailable; graph facts withheld"
    ));
    Ok(None)
}

fn complete_candidate(root: &Path, path: &str, gaps: &mut Vec<String>) -> bool {
    match engram_core::safe_open_read(root, path).and_then(|file| Ok(file.metadata()?)) {
        Ok(metadata) if metadata.is_file() && metadata.len() <= FILE_LIMIT as u64 => true,
        Ok(_) => {
            gaps.push(format!(
                "Automatic source exceeds 128 KiB: {path}; no complete body captured"
            ));
            false
        }
        Err(error) => {
            gaps.push(format!(
                "Automatic source unavailable: {path} ({error}); no source claim established"
            ));
            false
        }
    }
}

pub async fn assemble(
    state: &AppState,
    pid: &str,
    root: &Path,
    request: MaintenanceRequest,
) -> Result<Assembly> {
    validate(&request)?;
    let runtime = crate::services::project_service::ensure_project_runtime(state, pid).await?;
    let generation = crate::services::project_service::get_active_generation(state, pid).await?;
    let source = Arc::new(crate::services::project_service::pin_source_generation(
        state,
        &runtime.search,
        pid,
        generation,
    )?);
    ensure!(
        source.is_managed(),
        "Automatic area context requires a managed source snapshot; explicitly repair source or use manual evidence_paths"
    );
    let membership =
        runtime
            .search
            .selected_source_revision_lookup(pid, generation, source.clone())?;
    let state = state.clone();
    let pid = pid.to_owned();
    let root = root.to_owned();
    tokio::task::spawn_blocking(move || -> Result<Assembly> {
        let selected = request.area_context.as_ref().context("Area selection missing")?;
        let mut files = BTreeMap::new();
        let mut gaps = Vec::new();
        let mut symbols = Vec::new();
        let excluded = |path: &str| selected.exclude_paths.iter().any(|scope| within(path, scope));
        for path in &request.evidence_paths { add(&mut files, path, "explicit_evidence", None, &mut gaps); }
        let patterns = request.new_instruction_targets.iter().flat_map(|target| &target.area_scope)
            .map(|pattern| project_policy::compile_scope(pattern).map_err(anyhow::Error::msg)).collect::<Result<Vec<_>>>()?;
        let (candidates, capped) = membership.paths_in_scope(&selected.scope, 128)?;
        if capped { gaps.push("Area path discovery reached 128 identities; remaining area coverage is unknown".into()); }
        let mut area_files = Vec::new();
        for (path, _) in candidates {
            if excluded(&path) || !patterns.iter().any(|pattern| pattern.is_match(&path)) { continue; }
            if area_files.len() == selected.max_files {
                gaps.push(format!("Area file cap: {path}; additional selected-area files were not captured"));
                break;
            }
            if !complete_candidate(&root, &path, &mut gaps) { continue; }
            let hash = indexed_hash(&state, &pid, &path, &membership, &mut gaps)?;
            add(&mut files, &path, "area_implementation", hash, &mut gaps);
            area_files.push(path);
        }
        if area_files.is_empty() { gaps.push("No selected indexed area implementation was captured; this is not evidence that the area is empty".into()); }
        // Complete applicable authored guidance takes priority over optional callers.
        let policy = project_policy::resolve_with_budget(&root, &area_files, &area_files,
            project_policy::PolicyBudget { max_sources: 8, max_characters_per_source: FILE_LIMIT, max_characters_total: FILE_LIMIT });
        for note in policy.notes { gaps.push(format!("Shared guidance coverage: {note}")); }
        for omitted in policy.omissions { gaps.push(format!("Shared guidance omitted: {} ({})", omitted.path, omitted.reason)); }
        for policy in policy.sources {
            if policy.truncated { gaps.push(format!("Shared guidance incomplete: {}; not captured as policy", policy.path)); continue; }
            if excluded(&policy.path) { continue; }
            add(&mut files, &policy.path, "shared_guidance", None, &mut gaps);
        }
        let mut anchors = 0usize;
        let mut consumer_paths = BTreeSet::new();
        for path in &area_files {
            let mut nodes = state.graph.query_nodes_in_file(&pid, None, path, 17)?;
            if nodes.len() > 16 { gaps.push(format!("Declaration cap: {path}; more indexed declarations exist")); nodes.truncate(16); }
            for node in nodes {
                if node.node_type == "file" || membership.matches(path, node.generation)? != Some(true) { continue; }
                symbols.push(json!({"path":path,"node_id":node.node_id,"name":node.name,"kind":node.node_type,
                    "start_line":node.start_line,"end_line":node.end_line,"role":"entry_point_candidate",
                    "relation":"indexed_declaration; runtime entry-point behavior is not established"}));
                if anchors == 8 { continue; }
                anchors += 1;
                let (callers, capped) = crate::handlers::incoming_caller_edges_checked(&state.graph, &pid, &node.node_id, 8)?;
                if capped { gaps.push(format!("Consumer cap for {}; not proof of all callers", node.node_id)); }
                for (caller, kind, _) in callers {
                    let Some(caller) = state.graph.get_node(&pid, &caller)? else { gaps.push("A caller graph record is unavailable".into()); continue; };
                    let caller_path = caller.file_path.as_str();
                    if excluded(caller_path) || membership.matches(caller_path, caller.generation)? != Some(true) { continue; }
                    if !files.contains_key(caller_path) && consumer_paths.len() == 3 {
                        gaps.push(format!("Consumer file cap: {caller_path}; not captured")); continue;
                    }
                    if !complete_candidate(&root, caller_path, &mut gaps) { continue; }
                    let hash = indexed_hash(&state, &pid, caller_path, &membership, &mut gaps)?;
                    add(&mut files, caller_path, "consumer", hash, &mut gaps);
                    add(&mut files, caller_path, "companion_candidate", None, &mut gaps);
                    consumer_paths.insert(caller_path.to_owned());
                    if symbols.len() < 64 {
                        symbols.push(json!({"path":caller_path,"node_id":caller.node_id,"name":caller.name,"kind":caller.node_type,
                            "start_line":caller.start_line,"end_line":caller.end_line,"role":"consumer",
                            "related_to":node.node_id,"relation":format!("indexed_{kind:?}; not execution or accepted policy")}));
                    }
                }
            }
        }
        for path in area_files.iter().skip(1) { add(&mut files, path, "house_example_candidate", None, &mut gaps); }
        let relevant: Vec<_> = files.values().filter(|f| f.roles.contains("area_implementation") || f.roles.contains("consumer"))
            .map(|f| f.path.clone()).collect();
        let review = match crate::handlers::review_decisions::discover(&state, &pid, 8, |event|
            relevant.iter().any(|path| event.rationale.contains(path) || event.finding_id.contains(path))) {
            Ok(discovery) => json!({"coverage":discovery.coverage,"items":discovery.candidates.into_iter().map(|candidate| {
                let effective_status = crate::handlers::review_decisions::effective_status(&candidate.event, "externally_attested_fix_not_reverified");
                let workflow_only = crate::handlers::review_decisions::workflow_disposition_only(&candidate.event);
                json!({"review_id":candidate.review_id,"event":candidate.event,"record_blake3":candidate.record_blake3,
                    "effective_status":effective_status,"workflow_disposition_only":workflow_only,
                    "authority":"imported declaration, not authenticated policy or proof of accepted scope",
                    "recovery":{"tool":"get_review_decisions","arguments":{"project_id":pid,"review_id":candidate.review_id}}})
            }).collect::<Vec<_>>() }),
            Err(error) => { gaps.push(format!("Review evidence unavailable: {error}")); json!({"status":"unavailable"}) }
        };
        if consumer_paths.is_empty() {
            gaps.push("No direct consumer source was discovered; this does not prove the area has no consumers or dynamic callers".into());
        }
        let mut enriched = request.clone();
        enriched.area_context = None;
        enriched.evidence_paths = files.keys().filter(|path| !request.instruction_paths.contains(path)).cloned().collect();
        Ok(Assembly { request: enriched, evidence: AreaEvidence {
            scope: selected.scope.clone(), excluded_paths: selected.exclude_paths.clone(), files: files.into_values().collect(),
            symbols_and_relations: symbols, review_evidence: review, gaps,
            coverage: "Bounded indexed area discovery and direct caller candidates. Complete current/Git-before files are captured separately. Graph structure and imported review dispositions do not establish purpose, invariants or accepted exceptions; draft/review those against complete evidence. No validation was executed or exhaustive filesystem/consumer/test/policy discovery performed; routing never widens the proposed area scope.",
        }, _source: source })
    }).await.context("Area evidence worker unavailable")?
}

/// Graph positions/relations are useful only when their complete current body
/// still matches its indexed fingerprint. Changed source remains visible as
/// evidence, but stale graph claims are withheld rather than presented as fact.
pub(super) fn bind(area: &mut AreaEvidence, evidence: &[Evidence], instructions: &[Instruction]) {
    let mut verified = BTreeSet::new();
    for file in &mut area.files {
        let snapshot = evidence
            .iter()
            .find(|e| e.path == file.path)
            .and_then(|e| e.after.as_ref())
            .or_else(|| {
                instructions
                    .iter()
                    .find(|i| i.path == file.path)
                    .map(|i| &i.current)
            });
        file.captured_blake3 = snapshot.map(|s| s.blake3.clone());
        file.source_status = match (file.indexed_blake3.as_deref(), snapshot) {
            (Some(indexed), Some(current)) if indexed == current.blake3 => {
                verified.insert(file.path.clone());
                "indexed_fingerprint_matches_complete_current_source"
            }
            (Some(_), Some(_)) => {
                area.gaps.push(format!(
                    "{} changed since indexing; graph declarations/relations withheld",
                    file.path
                ));
                "current_source_changed_since_indexing"
            }
            (None, Some(_)) => "complete_source; indexed_graph_fingerprint_unavailable",
            (_, None) => "current_source_unavailable",
        }
        .into();
    }
    let verified_symbols: BTreeSet<_> = area
        .symbols_and_relations
        .iter()
        .filter(|symbol| {
            symbol["path"]
                .as_str()
                .is_some_and(|p| verified.contains(p))
        })
        .filter_map(|symbol| symbol["node_id"].as_str().map(str::to_owned))
        .collect();
    area.symbols_and_relations.retain(|symbol| {
        symbol["path"]
            .as_str()
            .is_some_and(|p| verified.contains(p))
            && symbol
                .get("related_to")
                .and_then(Value::as_str)
                .is_none_or(|id| verified_symbols.contains(id))
    });
}
