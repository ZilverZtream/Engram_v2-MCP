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
    pub capture_limit_bytes: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery: Option<Value>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CaptureLimits {
    pub authored_guidance_file_bytes: usize,
    pub automatic_code_file_bytes: usize,
    pub manual_legacy_evidence_file_bytes: usize,
    pub aggregate_packet_source_bytes: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AreaEvidence {
    pub scope: String,
    pub excluded_paths: Vec<String>,
    pub files: Vec<SelectedFile>,
    pub symbols_and_relations: Vec<Value>,
    pub review_evidence: Value,
    pub gaps: Vec<String>,
    pub discovery_coverage: Value,
    pub capture_limits: CaptureLimits,
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
        capture_limit_bytes: FILE_LIMIT,
        recovery: None,
    });
    if hash.is_some() {
        file.indexed_blake3 = hash;
    }
    file.roles.insert(role.into());
    file.capture_limit_bytes = if !file.roles.contains("shared_guidance")
        && !file.roles.contains("authored_instruction")
        && (file.roles.contains("area_implementation")
            || file.roles.contains("consumer")
            || file.roles.contains("consumer_recovery_candidate"))
        && automatic_code_path(path)
    {
        AUTOMATIC_CODE_LIMIT
    } else {
        FILE_LIMIT
    };
}

pub(super) fn automatic_code_path(path: &str) -> bool {
    let folded = path.to_ascii_lowercase();
    if folded.split('/').any(|part| {
        matches!(
            part,
            ".claude" | ".github" | ".agents" | ".cursor" | ".windsurf" | "instructions"
        )
    }) {
        return false;
    }
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "rs" | "vb"
                    | "cs"
                    | "js"
                    | "ts"
                    | "tsx"
                    | "jsx"
                    | "aspx"
                    | "ascx"
                    | "ashx"
                    | "asmx"
                    | "asp"
                    | "html"
                    | "htm"
                    | "sql"
                    | "css"
                    | "py"
                    | "java"
                    | "go"
                    | "cpp"
                    | "c"
                    | "h"
            )
        })
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

fn complete_candidate(root: &Path, path: &str, limit: usize, gaps: &mut Vec<String>) -> bool {
    match engram_core::safe_open_read(root, path).and_then(|file| Ok(file.metadata()?)) {
        Ok(metadata) if metadata.is_file() && metadata.len() <= limit as u64 => true,
        Ok(_) => {
            gaps.push(format!(
                "Automatic source exceeds its {limit}-byte capture limit: {path}; no complete body captured"
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
            let limit = if automatic_code_path(&path) { AUTOMATIC_CODE_LIMIT } else { FILE_LIMIT };
            if !complete_candidate(&root, &path, limit, &mut gaps) { continue; }
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
        let admit = |node: &engram_graph::Node, file: Option<&engram_graph::Node>| {
            if excluded(node.file_path.as_str()) { return Ok(engram_graph::store::ImpactAdmission::Unknown); }
            crate::services::impact_walk_service::admit_selected_source(&membership, node, file)
        };
        let (seeds, seed_coverage) = if area_files.is_empty() { (Vec::new(), Default::default()) }
            else { state.graph.bounded_file_impact_seeds(&pid, &area_files, &admit)? };
        let walk = if seeds.is_empty() { engram_graph::store::IncomingImpactWalk::default() }
            else { state.graph.bounded_incoming_walk(&pid, &seeds, 1,
                &crate::services::blast_radius_service::is_causal_dependency, &admit)? };
        if seed_coverage.truncated || seed_coverage.unknown_nodes > 0 || seed_coverage.non_selected_nodes > 0 {
            gaps.push(format!("Callable discovery is partial: {}", seed_coverage.reasons.join(", ")));
        }
        if walk.coverage.truncated || walk.coverage.unknown_nodes > 0 || walk.coverage.non_selected_nodes > 0 {
            gaps.push(format!("Consumer traversal is partial: {}", walk.coverage.reasons.join(", ")));
        }
        let lookup: BTreeMap<_, _> = walk.nodes.iter().map(|node| (node.node.node_id.as_str(), node)).collect();
        // Group all observed external files before the independent source-body
        // budget. Internal callable relationships never consume external slots.
        let mut consumers: BTreeMap<String, Vec<_>> = BTreeMap::new();
        for caller in walk.nodes.iter().filter(|node| node.depth == 1
            && node.admission == engram_graph::store::ImpactAdmission::Selected) {
            let path = caller.node.file_path.as_str();
            if excluded(path) || within(path, &selected.scope) { continue; }
            if path.len() > 512 { gaps.push("A consumer path exceeds maintenance identity limits; source not captured".into()); continue; }
            consumers.entry(path.to_owned()).or_default().push(caller);
        }
        let observed_consumers = consumers.len();
        let mut consumer_paths = BTreeSet::new();
        for (path, callers) in &consumers {
            let caller = &callers[0].node;
            let hash = indexed_hash(&state, &pid, path, &membership, &mut gaps)?;
            let recovery = json!({"tool":"get_full_method_body","arguments":{"project_id":pid,
                "file_path":path,"line_start":caller.start_line,"line_end":caller.end_line,"context_lines":0,"output_json":true},
                "indexed_physical_generation":caller.generation,"expected_indexed_file_blake3":hash,
                "scope":"Indexed line-range recovery lead, not a complete consumer file. Revalidate the whole-file fingerprint before using current positions; this does not widen the proposed instruction scope."});
            let limit = if automatic_code_path(path) { AUTOMATIC_CODE_LIMIT } else { FILE_LIMIT };
            let preselected = files.contains_key(path);
            let captured = (consumer_paths.len() < 3 || preselected)
                && (files.len() < 16 || preselected)
                && complete_candidate(&root, path, limit, &mut gaps);
            if !captured { gaps.push(format!("Known consumer {path} requires separate complete-source recovery (consumer/body budget); not silent absence")); }
            add(&mut files, path, if captured { "consumer" } else { "consumer_recovery_candidate" }, hash, &mut gaps);
            if let Some(file) = files.get_mut(path) {
                file.recovery = Some(recovery.clone());
                if !captured { file.source_status = "known_indexed_consumer; complete_source_not_captured".into(); }
            }
            if captured {
                add(&mut files, path, "companion_candidate", None, &mut gaps);
                if !preselected { consumer_paths.insert(path.to_owned()); }
            }
            for caller in callers {
                if symbols.len() == 32 { gaps.push("Consumer relation display cap; additional observed relations are not shown".into()); break; }
                let Some(target) = caller.predecessor.as_deref().and_then(|id| lookup.get(id)) else { continue; };
                symbols.push(json!({"path":path,"node_id":caller.node.node_id,"name":caller.node.name,"kind":caller.node.node_type,
                    "start_line":caller.node.start_line,"end_line":caller.node.end_line,
                    "role":if captured {"consumer"} else {"consumer_recovery_candidate"},
                    "related_to":target.node.node_id,"related_to_path":target.node.file_path,
                    "edge_kind":caller.via_kind.as_ref().map(|kind| kind.as_str()),"edge_generation":caller.edge_generation,
                    "source_generation":caller.node.generation,"recovery":recovery,
                    "relation":"indexed_causal_direct_consumer; not execution, runtime entry-point proof or accepted policy"}));
            }
        }
        // Display and traversal budgets are distinct: all selected callable
        // seeds were queried even when their declaration is not displayed.
        let mut anchors: Vec<_> = walk.nodes.iter().filter(|node| node.depth == 0 && node.admission == engram_graph::store::ImpactAdmission::Selected).collect();
        let callable = |kind: &str| matches!(kind, "function" | "method" | "constructor" | "property");
        anchors.sort_by(|left, right| (!callable(&left.node.node_type)).cmp(&(!callable(&right.node.node_type)))
            .then_with(|| left.node.file_path.as_str().cmp(right.node.file_path.as_str()))
            .then_with(|| left.node.node_id.cmp(&right.node.node_id)));
        for anchor in anchors {
            if symbols.len() == 64 { gaps.push("Callable declaration display cap; traversal used all retained callable seeds".into()); break; }
            symbols.push(json!({"path":anchor.node.file_path,"node_id":anchor.node.node_id,"name":anchor.node.name,
                "kind":anchor.node.node_type,"start_line":anchor.node.start_line,"end_line":anchor.node.end_line,
                "role":if matches!(anchor.node.node_type.as_str(), "function" | "method" | "constructor" | "property") {
                    "callable_declaration_candidate" } else { "indexed_declaration" },
                "relation":"indexed_declaration; runtime entry-point behavior is not established"}));
        }
        let discovery_coverage = json!({"source_generation":generation,"seed_discovery":seed_coverage,
            "consumer_walk":walk.coverage,"requested_declaration_seeds":seeds.len(),
            "selected_declaration_seeds":walk.nodes.iter().filter(|node|node.depth==0 && node.admission==engram_graph::store::ImpactAdmission::Selected).count(),
            "selected_callable_seeds":walk.nodes.iter().filter(|node| node.depth==0 && node.admission==engram_graph::store::ImpactAdmission::Selected
                && callable(&node.node.node_type)).count(),
            "observed_external_consumer_files":observed_consumers,
            "automatic_consumer_file_cap":3,"relation_display_cap":32,"total_symbol_display_cap":64,
            "membership_scope":"Native canonical callable identities and file/class Contains membership; bounded legacy scan only when native membership yields no selected callables. Mixed unlinked noncanonical supplemental declarations are not exhaustively inventoried."});
        for path in area_files.iter().skip(1) { add(&mut files, path, "house_example_candidate", None, &mut gaps); }
        let relevant: Vec<_> = files.values().filter(|f| f.roles.contains("area_implementation") || f.roles.contains("consumer") || f.roles.contains("consumer_recovery_candidate"))
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
        if observed_consumers == 0 {
            gaps.push("No admitted direct consumer was discovered within the callable/traversal bounds; this does not prove the area has no consumers or dynamic callers".into());
        }
        for path in &request.instruction_paths {
            if let Some(file) = files.get_mut(path) { file.roles.insert("authored_instruction".into()); file.capture_limit_bytes = FILE_LIMIT; }
        }
        let mut enriched = request.clone();
        enriched.area_context = None;
        enriched.evidence_paths = files.values().filter(|file| !request.instruction_paths.contains(&file.path)
            && (!file.roles.contains("consumer_recovery_candidate") || file.roles.contains("explicit_evidence")
                || file.roles.contains("area_implementation") || file.roles.contains("shared_guidance")))
            .map(|file| file.path.clone()).collect();
        Ok(Assembly { request: enriched, evidence: AreaEvidence {
            scope: selected.scope.clone(), excluded_paths: selected.exclude_paths.clone(), files: files.into_values().collect(),
            symbols_and_relations: symbols, review_evidence: review, gaps, discovery_coverage,
            capture_limits: CaptureLimits { authored_guidance_file_bytes:FILE_LIMIT,automatic_code_file_bytes:AUTOMATIC_CODE_LIMIT,
                manual_legacy_evidence_file_bytes:FILE_LIMIT,aggregate_packet_source_bytes:PACKET_LIMIT },
            coverage: "Bounded callable-first indexed area discovery and revision-admitted causal direct consumers under one held source selection. Classes/fields are not runtime entry points. Complete current/Git-before files are captured separately; named recovery leads are not complete file evidence. Graph structure and imported review dispositions do not establish purpose, invariants or accepted exceptions; draft/review those against complete evidence. No validation was executed or exhaustive filesystem/consumer/test/policy discovery performed; routing never widens the proposed area scope.",
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
            (_, None) if file.roles.contains("consumer_recovery_candidate") => {
                "known_indexed_consumer; complete_source_not_captured"
            }
            (_, None) => "current_source_unavailable",
        }
        .into();
    }
    let recovery_paths: BTreeSet<_> = area
        .files
        .iter()
        .filter(|file| file.roles.contains("consumer_recovery_candidate"))
        .map(|file| file.path.clone())
        .collect();
    area.symbols_and_relations.retain_mut(|symbol| {
        let Some(path) = symbol["path"].as_str() else { return false; };
        // A target declaration need not be in the bounded symbol display. Its
        // independently captured complete file is the fingerprint authority.
        if symbol.get("related_to_path").and_then(Value::as_str)
            .is_some_and(|target| !verified.contains(target)) { return false; }
        if recovery_paths.contains(path) || symbol["role"] == "consumer_recovery_candidate" {
            symbol["role"] = json!("consumer_recovery_candidate");
            symbol["relation_status"] = json!("indexed_recovery_lead_only; complete_current_source_not_captured");
            symbol["relation"] = json!("Indexed consumer recovery lead, not a fingerprint-verified current-source relation or complete file evidence");
            return true;
        }
        verified.contains(path)
    });
}
