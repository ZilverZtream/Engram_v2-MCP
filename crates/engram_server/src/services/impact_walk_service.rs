//! Source-revision admission and bounded witnesses for incoming change exposure.
//! These are indexed relationships, never proof of execution or safe changes.
use crate::state::AppState;
use anyhow::{Context, Result, ensure};
use engram_graph::store::{ImpactAdmission, ImpactWalkCoverage, IncomingImpactWalk, ResolveResult};
use engram_index::SourceSnapshotGuard;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
};

const RESPONSE_BYTES: usize = 64 * 1024;
const IMMEDIATE_BYTES: usize = 2 * 1024;
pub const SUMMARY_BYTES: usize = 12 * 1024;
const SOURCE_ROUTE_BYTES: usize = 4 * 1024;

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
    /// Bounded JSON evidence, without Markdown: the retained graph or an
    /// explicitly identified summary display projected after safety evaluation.
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
    // All admitted external files in this bounded walk, before graph display
    // clipping. None is unavailable/not a file query, never an empty inventory.
    dependent_files: Option<BTreeMap<String, usize>>,
    advisory_scope: AdvisoryScope,
}

/// Internal, unprojected membership; rendered rows may omit seed declarations.
pub(crate) enum AdvisoryScope {
    Symbol,
    CompleteFile(Vec<String>),
    Unavailable(&'static str),
}

fn immediate_section(value: &Value, include_references: bool) -> Option<String> {
    if value["status"] != "bounded_indexed_exposure" || value["target_revision_admitted"] == false {
        return None;
    }
    let node = value["nodes"]
        .as_array()?
        .iter()
        .find(|node| node["admission"] == "selected" && node["depth"] == 1)?;
    let reference = node["ref"].as_str()?;
    let edge = value["edges"].as_array()?.iter().find(|edge| {
        edge["source_ref"] == reference
            && edge["connection_role"] != "additional_verified_connection"
    })?;
    let recovery = value["recoveries"]
        .as_array()?
        .iter()
        .find(|recovery| recovery["ref"] == node["recovery_ref"])?;
    let file = value["files"]
        .as_array()?
        .iter()
        .find(|file| file["ref"] == node["file_ref"])?["path"]
        .as_str()?;
    let display = |text: &str, limit: usize| {
        let mut clipped: String = text
            .chars()
            .take(limit)
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        if text.chars().count() > limit {
            clipped.push('…');
        }
        clipped
    };
    let call = json!({"tool":recovery["tool"],"arguments":recovery["arguments"]});
    let excerpt = recovery["scope_ref"] == "file_excerpt";
    let references = if include_references {
        format!(
            " (node {}; edge {}; recovery {}).",
            reference,
            edge["ref"].as_str()?,
            recovery["ref"].as_str()?
        )
    } else if excerpt {
        " (declaration range unavailable).".into()
    } else {
        format!("–{}.", node["end_line"])
    };
    let scope = if recovery["scope_ref"] == "range" {
        "Read the indexed declaration range from the current file. The action has not run; inspect its returned hash and verification. Complete-method boundaries and invocation location are unverified."
    } else if excerpt {
        "Read a bounded current-file excerpt. Compare its returned hash and pagination metadata; method boundaries, indexed freshness and invocation location are unverified."
    } else {
        "Navigate declarations under a fresh source selection. This action does not read or verify the consumer body."
    };
    let kind=edge["kind"].as_str()?;
    let relationship=if kind=="implements_interface" {
        "declaration/implementation dependency"
    }else{"recorded dependency"};
    let output = format!(
        "\n## First immediate indexed consumer\n\n{} — {}:{}{}\nRelationship: {} (dependent → dependency; {}).\n{}\nOne next evidence read; other consumers and unknown branches follow. No runtime or safe-change proof.\n\n```json\n{}\n```\n",
        display(node["name"].as_str()?, 96),
        display(file, 160),
        if excerpt {
            "unavailable".to_owned()
        } else {
            node["start_line"].to_string()
        },
        references,
        display(kind,64),
        relationship,
        scope,
        serde_json::to_string(&call).expect("JSON value serialization")
    );
    // Long metadata remains in the canonical tables; never truncate executable
    // arguments or let this convenience prefix consume the graph budget.
    (output.len() <= IMMEDIATE_BYTES).then_some(output)
}

fn section(value: &Value) -> String {
    let count=|number:Option<u64>|number.map(|n|n.to_string()).unwrap_or_else(||"unknown".into());
    let admitted=if value["target_revision_admitted"]==true {
        value["selected_dependents_observed"].as_u64()
    }else{None};
    let dispatch=&value["potential_interface_callers"];
    format!(
        "{}\nAdmitted indexed dependents: {}. Separate potential interface callers: {} ({} recorded routes).\nBounded static observations; no absence, runtime binding or overall risk verdict.\n\n## Revision-admitted change impact\n\nIndexed causal paths under the named source selection; not a proposed-change simulation, runtime proof or safe-change verdict.\n\n```json\n{}\n```\n",
        immediate_section(value, true).unwrap_or_default(),
        count(admitted),
        count(dispatch["distinct_callers_observed"].as_u64()),
        count(dispatch["routes_observed"].as_u64()),
        serde_json::to_string(value).expect("JSON value serialization")
    )
}

fn describe_summary_node(
    full: &Value,
    node: &Value,
    nodes: &HashMap<&str, &Value>,
    files: &HashMap<&str, &Value>,
    recoveries: &HashMap<&str, &Value>,
    target: Option<&Value>,
    file_component: bool,
) -> Value {
    let action = |reference: &Value| {
        reference.as_str().and_then(|reference| recoveries.get(reference))
            .map(|row| json!({"tool":row["tool"],"arguments":row["arguments"]}))
    };
    let recovery = node["recovery_ref"]
        .as_str()
        .and_then(|reference| recoveries.get(reference));
    let connection = full["edges"].as_array().into_iter().flatten().filter(|edge| {
        edge["source_ref"] == node["ref"] && (target.is_some_and(|target| edge["target_ref"] == target["ref"])
            || (file_component && edge["target_ref"].as_str()
                .and_then(|reference| nodes.get(reference)).is_some_and(|member|
                    member["admission"] == "selected" && member["is_seed"] == true
                        && target.is_some_and(|target| member["file_ref"] == target["file_ref"]))))
    }).min_by_key(|edge| edge["connection_role"] == "additional_verified_connection");
    json!({"node_id":node["node_id"],"name":node["name"],
    "file_path":node["file_ref"].as_str().and_then(|reference| files.get(reference)),
    "start_line":node["start_line"],"end_line":node["end_line"],"generation":node["generation"],
    "admission":node["admission"],"source_read":action(&node["recovery_ref"]),
    "source_read_scope":recovery.map(|row|match row["scope_ref"].as_str() {
        Some("range")=>"explicit_source_range",Some("file_excerpt")=>"current_file_excerpt",
        _=>"selected_source_navigation"}),
    "sample_connection_to_target":connection.map(|edge| {
        let reached = edge["target_ref"].as_str().and_then(|reference|nodes.get(reference));
        let mut witness = json!({
        "source_node_id":node["node_id"],"target_node_id":reached.map(|node|&node["node_id"]),
        "kind":edge["kind"],"edge_generation":edge["edge_generation"],"source_generation":node["generation"],
        "edge_source_binding":edge.get("edge_source_binding").cloned().unwrap_or(json!("extraction_generation")),
        "source_line":edge["source_line"],
        "recorded_position_read":if edge["source_position_scope"] == "unattributed_extractor_anchor" {
            None
        } else { action(edge.get("recovery_ref").unwrap_or(&node["recovery_ref"])) },
        "source_position_kind":edge.get("source_position_kind").cloned().unwrap_or(json!("not_recorded")),
        "extractor_anchor_line":edge["extractor_anchor_line"],
        "reference_lines":edge.get("reference_lines").cloned().unwrap_or(json!([])),
        "reference_lines_truncated":edge.get("reference_lines_truncated").cloned().unwrap_or(json!(false)),
        "source_position_scope":edge.get("source_position_scope").cloned().unwrap_or(json!("not_recorded"))
        });
        if file_component {
            witness["target_scope"] = json!(if edge["target_ref"] == full["target_ref"] { "requested_file" } else { "file_member" });
            witness["target_name"] = reached.map(|node|node["name"].clone()).unwrap_or(Value::Null);
            witness["target_generation"] = reached.map(|node|node["generation"].clone()).unwrap_or(Value::Null);
        }
        // The same executable read can cover a symbol or file-member
        // witness. Preserve its position/scope; share only exact actions.
        if let Some(source_read) = action(&node["recovery_ref"]) {
            if witness.get("recorded_position_read") == Some(&source_read) {
                witness.as_object_mut().expect("witness object").remove("recorded_position_read");
                witness["recorded_position_uses_source_read"] = json!(true);
            }
        }
        witness
    })})
}

/// Project the already-collected facts. No graph/source/model query is made.
fn summary_value(
    full: &Value,
    project: &str,
    file_path: Option<&str>,
    symbol_fqn: Option<&str>,
    display_limit: usize,
    direct: Option<usize>,
    transitive: Option<usize>,
    dependent_files: Option<&BTreeMap<String, usize>>,
) -> Value {
    let nodes: HashMap<_, _> = full["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|node| Some((node["ref"].as_str()?, node)))
        .collect();
    let files: HashMap<_, _> = full["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|file| Some((file["ref"].as_str()?, &file["path"])))
        .collect();
    let recoveries: HashMap<_, _> = full["recoveries"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|action| Some((action["ref"].as_str()?, action)))
        .collect();
    let admitted = full["target_revision_admitted"] == true;
    let target = full["target_ref"]
        .as_str()
        .and_then(|reference| nodes.get(reference))
        .copied();
    let file_component = target.is_some_and(|node| node["kind"] == "file");
    let action = |reference: &Value| {
        reference
            .as_str()
            .and_then(|reference| recoveries.get(reference))
            .map(|row| json!({"tool":row["tool"],"arguments":row["arguments"]}))
    };
    let describe = |node: &Value| {
        describe_summary_node(full,node,&nodes,&files,&recoveries,target,file_component)
    };
    let mut arguments = json!({"project_id":project,"max_depth":full["requested_max_depth"],
        "limit":display_limit.clamp(1,25),"detail":"graph"});
    let exact = target.and_then(|node| node["node_id"].as_str());
    let bounded = |value: &str| {
        !value.is_empty() && value.len() <= 2048 && !value.chars().any(char::is_control)
    };
    let continuation = if full["target_resolution"]["status"] == "ambiguous" {
        None
    } else if let Some(symbol) = exact.or(symbol_fqn).filter(|symbol| bounded(symbol)) {
        arguments["symbol_fqn"] = json!(symbol);
        Some(json!({"tool":"impact_analysis","arguments":arguments}))
    } else if let Some(file) = file_path.filter(|file| bounded(file)) {
        arguments["file_path"] = json!(file);
        Some(json!({"tool":"impact_analysis","arguments":arguments}))
    } else {
        None
    };
    let frontier = full["frontier"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut value = json!({"schema_version":2,"detail":"summary",
        "source_generation":full["source_generation"],"source_layout":full["source_layout"],
        "requested_max_depth":full["requested_max_depth"],"status":full["status"],
        "target_revision_admitted":admitted,"selected_dependents_observed":full["selected_dependents_observed"],
        "direct_dependents_observed":direct,"transitive_dependents_observed":transitive,
        "verified_connections_returned_in_graph":if admitted {full["edges"].as_array().map(Vec::len)}else{None},
        "coverage":full["coverage"],
        "coverage_counter_semantics":{
            "missing_nodes":"Missing graph-node read observations; the same identity may occur more than once. Not absent source declarations.",
            "unknown_nodes":"Unknown admission or edge-source observations; identities may repeat. Zero does not resolve missing-node or depth frontiers.",
            "frontier_depth":"An entry beyond requested_max_depth names an untraversed boundary, not an admitted dependent at that depth."
        },"working_tree_checked":false,"exhaustive_codebase":false,
        "dependency_scope":full["dependency_scope"],
        "change_breakage_proven":false,"target":null,"target_omitted_from_summary":target.is_some(),
        "immediate_consumers":[],"immediate_consumers_omitted_from_summary":direct,
        "immediate_consumer_detail_limit_bytes":2048,
        "immediate_selection_scope":"At most four observed direct consumers, one per source file before repeats; canonical order within each round. Per-row and total byte limits can omit rows. This is a sample, not a complete inventory. File-member witnesses name actual edges. recorded_position_uses_source_read=true reuses the owning consumer's source_read action, not a separate callsite proof.",
        "frontier":[],"frontier_rows_omitted_from_summary":frontier.len(),
        "graph_omissions":{"node_identities_omitted":full["node_identities_omitted"],
            "unconnected_file_seeds":full["unconnected_file_seeds_omitted"],
            "edges_omitted":full["edges_omitted"],"shortest_paths_omitted":full["shortest_paths_omitted"],
            "frontier_rows_omitted":full["frontier_rows_omitted"]},
        "graph_continuation":continuation,
        "continuation_scope":"Fresh bounded observation, not saved pagination or atomic replay; compare source_generation and coverage before combining results. No valid bounded target means continuation unavailable.",
        "separate_context":"Graph links omitted declaration inventory; historical/advisory context is separate.",
        "limitations":["Counts are bounded indexed exposure, not numbers of consumers that will break. Omitted rows and uninspected/dynamic bindings remain unknown.",
            "Source reads bind current file bytes/ranges, not callsites, complete method boundaries, value flow, compiler results or runtime execution."]});
    if let Some(error) = full.get("error") {
        value["error"] = error.clone();
    }
    if let Some(dispatch) = full.get("potential_interface_callers") {
        let mut projection=dispatch.clone();
        projection["routes"]=json!([]);
        projection["frontier"]=json!([]);
        projection["routes_omitted"]=dispatch["routes_observed"].clone();
        projection["frontier_omitted"]=json!(dispatch["frontier"].as_array().map_or(0,Vec::len)
            +dispatch["frontier_omitted"].as_u64().unwrap_or(0) as usize);
        projection.as_object_mut().unwrap().remove("configuration_inspection");
        projection.as_object_mut().unwrap().remove("configuration_inspection_actions_omitted");
        value["potential_interface_callers"]=projection;
    }
    if file_component {
        value["requested_file_node_id"] = target.map(|node|node["node_id"].clone()).unwrap_or(Value::Null);
    }
    // Reserve shared candidate semantics before any optional rows consume the
    // summary budget, including when every candidate row is later omitted.
    if recoveries.values().any(|row| row["scope_ref"] == "inspection_candidate") {
        value["inspection_candidate_scope"] = full["recovery_scopes"]["inspection_candidate"].clone();
    }
    if recoveries.values().any(|row| row["scope_ref"] == "enclosing_declaration") {
        value["enclosing_declaration_scope"] = json!("Enclosing context; receiver/edge unresolved. Fresh bounded read; verify hash/freshness.");
    }
    // Charge the actual bounded prefix, plus framing/freshness reserve. Its
    // 2KiB ceiling is not its usual cost; over-reserving it hides useful rows.
    let immediate_prefix=immediate_section(full,false).unwrap_or_default();
    let fits = |value: &Value| {
        serde_json::to_vec(value)
            .expect("JSON value serialization")
            .len()
            <= SUMMARY_BYTES.saturating_sub(immediate_prefix.len()+1024)
    };
    if let Some(resolution) = full.get("target_resolution") {
        value["target_resolution"] = resolution.clone();
        while !fits(&value) {
            let removed = value["target_resolution"]["candidates"]
                .as_array_mut().and_then(Vec::pop);
            if removed.is_none() { break; }
            let omitted = value["target_resolution"]["coverage"]["selected_candidates_omitted"]
                .as_u64().unwrap_or(0);
            value["target_resolution"]["coverage"]["selected_candidates_omitted"] = json!(omitted + 1);
        }
    }
    if let Some(target) = target {
        value["target"] = describe(target);
        if fits(&value) {
            value["target_omitted_from_summary"] = json!(false);
        } else {
            value["target"] = Value::Null;
        }
    }
    // Reserve actual potential callers before optional ordinary consumer rows.
    // These remain separate, non-additive observations of one dispatch hop.
    if let Some(dispatch)=full.get("potential_interface_callers") {
        let mut routes=Vec::new();
        for route in dispatch["routes"].as_array().into_iter().flatten().take(1) {
            routes.push(route.clone());
            value["potential_interface_callers"]["routes"]=json!(&routes);
            if !fits(&value) { routes.pop(); value["potential_interface_callers"]["routes"]=json!(&routes); break; }
        }
        value["potential_interface_callers"]["routes_omitted"]=json!(dispatch["routes_observed"].as_u64().unwrap_or(0).saturating_sub(routes.len() as u64));
    }
    let mut immediate = Vec::new();
    if admitted {
        let mut selected: Vec<_> = nodes
            .values()
            .copied()
            .filter(|node| node["admission"] == "selected" && node["depth"] == 1)
            .collect();
        selected.sort_by(|a, b| a["node_id"].as_str().cmp(&b["node_id"].as_str()));
        let mut file_counts = HashMap::new();
        let mut ranked: Vec<_> = selected.into_iter().map(|node| {
            let file = node["file_ref"].as_str().and_then(|reference|files.get(reference)).and_then(|path|path.as_str());
            let count = file_counts.entry(file).or_insert(0usize);
            let rank = *count;
            *count += 1;
            (rank, node)
        }).collect();
        ranked.sort_by(|(a_rank,a),(b_rank,b)|a_rank.cmp(b_rank)
            .then_with(||a["node_id"].as_str().cmp(&b["node_id"].as_str())));
        for (_, node) in ranked {
            if immediate.len() == 4 { break; }
            let described = describe(node);
            if described["sample_connection_to_target"].is_null()
                || serde_json::to_vec(&described).expect("JSON value serialization").len() > 2048 { continue; }
            immediate.push(described);
            value["immediate_consumers"] = json!(&immediate);
            if !fits(&value) {
                immediate.pop();
                value["immediate_consumers"] = json!(&immediate);
                continue;
            }
        }
    }
    value["immediate_consumers_omitted_from_summary"] =
        json!(direct.map(|count| count.saturating_sub(immediate.len())));
    // File inventory is established independently of declaration display
    // clipping. Keep its counts/paths before optional route/inspection detail.
    if let Some(files) = dependent_files {
        append_dependent_files(&mut value, files, &fits);
    }
    // Admitted source routes outrank optional inspection-detail rows. Required
    // counts, gaps, target/direct witnesses and graph recovery are already fixed.
    append_source_routes(&mut value, full, project, &fits);
    // A second potential route is optional after the ordinary diverse sample.
    if let Some(dispatch)=full.get("potential_interface_callers") {
        let mut routes=value["potential_interface_callers"]["routes"].as_array().cloned().unwrap_or_default();
        for route in dispatch["routes"].as_array().into_iter().flatten().skip(1).take(1) {
            routes.push(route.clone()); value["potential_interface_callers"]["routes"]=json!(&routes);
            if !fits(&value) { routes.pop();value["potential_interface_callers"]["routes"]=json!(&routes); }
        }
        value["potential_interface_callers"]["routes_omitted"]=json!(dispatch["routes_observed"].as_u64().unwrap_or(0).saturating_sub(routes.len() as u64));
    }
    let mut leads = Vec::new();
    for lead in frontier.iter().take(8) {
        let node = lead["source_ref"]
            .as_str()
            .and_then(|reference| nodes.get(reference))
            .copied();
        let parent = lead["target_ref"]
            .as_str()
            .and_then(|reference| nodes.get(reference))
            .copied();
        let hint = node.filter(|node|node["name"].is_null()).and_then(|node|node["node_id"].as_str()).and_then(|id| {
            let (file, symbol) = canonical_hint(id);
            file.filter(|_|symbol != id).map(|file|json!({"file_path":file,"symbol_name":symbol,
                "authority":"unverified_identity_hint; navigation may not contain this declaration"}))
        });
        leads.push(json!({"node_id":node.map(|node|&node["node_id"]),
            "parent_node_id":parent.map(|node|&node["node_id"]),"reason":lead["reason"],
            "kind":lead["kind"],"depth":lead["depth"],"admission":node.map(|node|&node["admission"]),
            "recovery":action(&lead["recovery_ref"]),
            "recovery_scope":lead["recovery_ref"].as_str().and_then(|reference|recoveries.get(reference)).map(|row|&row["scope_ref"]),
            "lookup_hint":hint}));
        value["frontier"] = json!(&leads);
        if !fits(&value) {
            // A long optional inspection action must not by itself erase a
            // named unknown. Keep its identity/reason and disclose that the
            // full graph holds the omitted inspection details.
            let row = leads.last_mut().unwrap().as_object_mut().unwrap();
            let details = ["recovery", "recovery_scope", "lookup_hint"].iter()
                .any(|key| row.get(*key).is_some_and(|value| !value.is_null()));
            row.remove("recovery"); row.remove("recovery_scope"); row.remove("lookup_hint");
            row.insert("recovery_details_omitted".into(), json!(details));
            value["frontier"] = json!(&leads);
            if !fits(&value) { leads.pop(); value["frontier"] = json!(&leads); break; }
        }
    }
    value["frontier_rows_omitted_from_summary"] = json!(frontier.len() - leads.len());
    // Optional interface gap rows must not displace the ordinary file-diverse
    // consumer sample. Counts/scope/coverage were reserved before every row.
    if let Some(dispatch)=full.get("potential_interface_callers") {
        let mut leads=Vec::new();
        for lead in dispatch["frontier"].as_array().into_iter().flatten().take(2) {
            leads.push(lead.clone());
            value["potential_interface_callers"]["frontier"]=json!(&leads);
            if !fits(&value) { leads.pop(); value["potential_interface_callers"]["frontier"]=json!(&leads); break; }
        }
        value["potential_interface_callers"]["frontier_omitted"]=json!(dispatch["frontier"].as_array().map_or(0,Vec::len)-leads.len()
            +dispatch["frontier_omitted"].as_u64().unwrap_or(0) as usize);
        // Text-search suggestions come last: never trade a caller, witness or
        // unknown boundary for an optional configuration discovery action.
        if let Some(inspection)=dispatch.get("configuration_inspection") {
            let mut base=inspection.clone();
            let discovery=base.as_object_mut().unwrap().remove("path_discovery");
            let omitted=base.as_object_mut().unwrap().remove("path_discovery_actions_omitted");
            let narrowing_omitted=base.as_object_mut().unwrap().remove("narrowed_actions_omitted");
            let generic=base.get("unrestricted_fallback").cloned().unwrap_or_else(||base.clone());
            value["potential_interface_callers"]["configuration_inspection"]=generic.clone();
            if !fits(&value) {
                value["potential_interface_callers"].as_object_mut().unwrap().remove("configuration_inspection");
                value["potential_interface_callers"]["configuration_inspection_actions_omitted"]=json!(
                    generic["actions"].as_array().map_or(0,Vec::len)+generic["actions_omitted"].as_u64().unwrap_or(0) as usize);
                if !fits(&value) { value["potential_interface_callers"].as_object_mut().unwrap().remove("configuration_inspection_actions_omitted"); }
            } else {
                if base.get("unrestricted_fallback").is_some() {
                    value["potential_interface_callers"]["configuration_inspection"]=base;
                    if !fits(&value) {
                        let total=inspection["actions"].as_array().map_or(0,Vec::len)+inspection["actions_omitted"].as_u64().unwrap_or(0) as usize;
                        value["potential_interface_callers"]["configuration_inspection"]=generic;
                        value["potential_interface_callers"]["configuration_inspection"]["narrowed_actions_omitted"]=json!(total);
                        if !fits(&value) {value["potential_interface_callers"]["configuration_inspection"].as_object_mut().unwrap().remove("narrowed_actions_omitted");}
                    }
                } else if let Some(omitted)=narrowing_omitted {
                    value["potential_interface_callers"]["configuration_inspection"]["narrowed_actions_omitted"]=omitted;
                    if !fits(&value) {value["potential_interface_callers"]["configuration_inspection"].as_object_mut().unwrap().remove("narrowed_actions_omitted");}
                }
                if let Some(discovery)=discovery {
                    append_configuration_path_discovery(&mut value,&discovery,&fits);
                } else if let Some(omitted)=omitted {
                    value["potential_interface_callers"]["configuration_inspection"]["path_discovery_actions_omitted"]=omitted;
                    if !fits(&value) { value["potential_interface_callers"]["configuration_inspection"].as_object_mut().unwrap().remove("path_discovery_actions_omitted"); }
                }
            }
        } else if let Some(omitted)=dispatch.get("configuration_inspection_actions_omitted") {
            value["potential_interface_callers"]["configuration_inspection_actions_omitted"]=omitted.clone();
            if !fits(&value) { value["potential_interface_callers"].as_object_mut().unwrap().remove("configuration_inspection_actions_omitted"); }
        }
    }
    if let Some(inspection) = full.get("unbound_member_call_inspection") {
        append_unbound_inspection(&mut value, inspection, &fits);
    }
    if let Some(omitted) = full.get("unbound_member_call_inspection_omitted") {
        value["unbound_member_call_inspection_omitted"] = omitted.clone();
        if !fits(&value) { value.as_object_mut().unwrap().remove("unbound_member_call_inspection_omitted"); }
    }
    if let Some(inspection) = full.get("terminal_member_inspection") {
        append_terminal_inspection(&mut value, inspection, &fits);
    }
    if let Some(inspection) = full.get("type_name_inspection") {
        append_type_name_inspection(&mut value, inspection, &fits);
    } else if full.get("type_name_inspection_omitted") == Some(&json!(true)) {
        value["type_name_inspection_omitted"] = json!(true);
        if !fits(&value) { value.as_object_mut().unwrap().remove("type_name_inspection_omitted"); }
    }
    append_caller_file_starts(&mut value, project, &fits);
    value
}

// Type mentions can reveal constructors and return types that the incoming
// callable graph does not model. Keep this lazy search separate from that graph.
fn type_name_inspection(node: &engram_graph::Node, project: &str) -> Option<Value> {
    let path = node.file_path.as_str();
    if node.node_type != "class" || !recovery_path(path)
        || !node.node_id.starts_with(&format!("sym:class:{path}:")) {
        return None;
    }
    let name = node.name.rsplit('.').next()?;
    if name.is_empty() || name.len() > 128 || !name.chars().enumerate().all(|(i, c)|
        c == '_' || c == '$' || if i == 0 { c.is_alphabetic() } else { c.is_alphanumeric() }) {
        return None;
    }
    let (directory, _) = path.rsplit_once('/')?;
    Some(json!({
        "scope":"Inspect indexed literal type-name mentions in the target directory, including constructors, return types and fields. This action has not run. Matches can include declarations, comments, prefix names and unrelated types; they do not add graph dependents or prove binding, serialization flow, breakage or absence. Other directories and aliases remain unassessed. Follow returned current-source actions/hashes before inferring a route.",
        "action":{"tool":"grep_project","arguments":{"project_id":project,"pattern":name,
            "regex":false,"case_sensitive":false,"path_prefix":format!("{directory}/"),
            "max_results":8,"context_before":2,"context_after":2,"freshness":"off","output_json":true}}
    }))
}

fn append_type_name_inspection(value: &mut Value, inspection: &Value, fits: &dyn Fn(&Value) -> bool) {
    value["type_name_inspection"] = inspection.clone();
    if !fits(value) {
        value.as_object_mut().unwrap().remove("type_name_inspection");
        value["type_name_inspection_omitted"] = json!(true);
        if !fits(value) { value.as_object_mut().unwrap().remove("type_name_inspection_omitted"); }
    }
}

// Keep the focused member search if its broader fallback alone would overflow.
// Neither action executes a search or contributes graph evidence.
fn append_terminal_inspection(value: &mut Value, inspection: &Value, fits: &dyn Fn(&Value) -> bool) {
    value["terminal_member_inspection"] = inspection.clone();
    if !fits(value) && value["terminal_member_inspection"].get("unrestricted_fallback").is_some() {
        value["terminal_member_inspection"].as_object_mut().unwrap().remove("unrestricted_fallback");
        value["terminal_member_inspection"]["unrestricted_fallback_omitted"] = json!(true);
    }
    if !fits(value) {
        value.as_object_mut().unwrap().remove("terminal_member_inspection");
        value["terminal_member_inspection_omitted"] = json!(true);
        if !fits(value) { value.as_object_mut().unwrap().remove("terminal_member_inspection_omitted"); }
    }
}

/// Recover useful current-file windows from this frame only. A window can
/// combine connected indexed declaration spans, never infer value flow or pull
/// an input producer into reverse impact. No database/source/HEAD query occurs.
fn append_source_routes(value: &mut Value, full: &Value, project: &str, fits: &dyn Fn(&Value) -> bool) {
    if value["target_revision_admitted"] != true { return; }
    let nodes: HashMap<_, _> = full["nodes"].as_array().into_iter().flatten()
        .filter_map(|node| Some((node["ref"].as_str()?, node))).collect();
    let files: HashMap<_, _> = full["files"].as_array().into_iter().flatten()
        .filter_map(|file| Some((file["ref"].as_str()?, file))).collect();
    let recoveries: HashMap<_, _> = full["recoveries"].as_array().into_iter().flatten()
        .filter_map(|row| Some((row["ref"].as_str()?, row))).collect();
    let Some(target) = full["target_ref"].as_str().and_then(|reference| nodes.get(reference)).copied() else { return };
    let callable = |node: &Value| node["admission"] == "selected"
        && matches!(node["kind"].as_str(), Some("function" | "method" | "constructor"));
    let edges: Vec<_> = full["edges"].as_array().into_iter().flatten().filter(|edge| {
        !matches!(edge["kind"].as_str(), Some("contains" | "includes_file" | "implements_interface" | "implements_interface_member"))
            && edge["source_ref"].as_str().and_then(|r| nodes.get(r)).is_some_and(|n| callable(n))
            && edge["target_ref"].as_str().and_then(|r| nodes.get(r)).is_some_and(|n| callable(n))
    }).collect();
    let mut parents = BTreeMap::new();
    for edge in &edges {
        let source = edge["source_ref"].as_str().unwrap();
        let parent = edge["target_ref"].as_str().unwrap();
        if edge["connection_role"] != "additional_verified_connection"
            && nodes[source]["depth"].as_u64().zip(nodes[parent]["depth"].as_u64())
                .is_some_and(|(source, parent)| source == parent + 1) {
            parents.entry(source).or_insert(parent);
        }
    }
    let trace = |reference: &str| -> Option<Vec<String>> {
        let mut path = vec![reference.to_owned()];
        while let Some(parent) = parents.get(path.last()?.as_str()) {
            if path.len() >= nodes.len() || path.iter().any(|r| r == parent) { return None; }
            path.push((*parent).to_owned());
        }
        let seed = nodes.get(path.last()?.as_str())?;
        if seed["is_seed"] != true || !(seed["ref"] == target["ref"]
            || (target["kind"] == "file" && seed["file_ref"] == target["file_ref"])) { return None; }
        path.reverse(); Some(path)
    };
    let mut eligible = Vec::new();
    let mut traces = HashMap::new();
    let mut ineligible = 0usize;
    for node in nodes.values().copied().filter(|node| callable(node) && node["depth"].as_u64().is_some_and(|d| d >= 2)) {
        let reference = node["ref"].as_str().unwrap();
        let path = node["file_ref"].as_str().and_then(|r| files.get(r)).and_then(|f| f["path"].as_str());
        let bounds = node["start_line"].as_u64().zip(node["end_line"].as_u64());
        let recovery = node["recovery_ref"].as_str().and_then(|r| recoveries.get(r));
        let usable = path.zip(bounds).zip(recovery).filter(|((path, (start, end)), recovery)|
            *start > 0 && *end >= *start && end - start < 120 && node["generation"].as_u64().is_some()
                && recovery["tool"] == "get_full_method_body" && recovery["scope_ref"] == "range"
                && recovery["arguments"]["file_path"] == *path
                && recovery["arguments"]["line_start"].as_u64() == Some(*start)
                && recovery["arguments"]["line_end"].as_u64().is_some_and(|limit| limit >= *end));
        if usable.is_none() { ineligible += 1; continue; }
        let Some(path) = trace(reference) else { ineligible += 1; continue };
        traces.insert(reference, path); eligible.push(node);
    }
    eligible.sort_by(|a, b| a["node_id"].as_str().cmp(&b["node_id"].as_str()));
    if eligible.is_empty() { return; }
    let indices: HashMap<_, _> = eligible.iter().enumerate().map(|(i, node)| (node["ref"].as_str().unwrap(), i)).collect();
    let mut owners: Vec<_> = (0..eligible.len()).collect();
    let mut lows: Vec<_> = eligible.iter().map(|node| node["start_line"].as_u64().unwrap()).collect();
    let mut highs: Vec<_> = eligible.iter().map(|node| node["end_line"].as_u64().unwrap()).collect();
    fn owner(owners: &mut [usize], mut i: usize) -> usize {
        while owners[i] != i { owners[i] = owners[owners[i]]; i = owners[i]; } i
    }
    // Merge nearer connections first: an outer wrapper must not use the whole
    // range budget before two nearer declarations can share one source read.
    let mut ordered_edges = edges.clone();
    ordered_edges.sort_by_key(|edge| {
        let a = nodes[edge["source_ref"].as_str().unwrap()];
        let b = nodes[edge["target_ref"].as_str().unwrap()];
        (a["depth"].as_u64().unwrap_or(0).max(b["depth"].as_u64().unwrap_or(0)),
            a["node_id"].as_str(), b["node_id"].as_str())
    });
    for edge in ordered_edges {
        let Some((&a, &b)) = edge["source_ref"].as_str().and_then(|r| indices.get(r))
            .zip(edge["target_ref"].as_str().and_then(|r| indices.get(r))) else { continue };
        let a = owner(&mut owners, a); let b = owner(&mut owners, b);
        if a == b || eligible[a]["file_ref"] != eligible[b]["file_ref"]
            || eligible[a]["generation"] != eligible[b]["generation"] { continue; }
        let lo = lows[a].min(lows[b]); let hi = highs[a].max(highs[b]);
        if hi - lo >= 120 { continue; }
        let (keep, drop) = (a.min(b), a.max(b)); owners[drop] = keep; lows[keep] = lo; highs[keep] = hi;
    }
    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..eligible.len() { groups.entry(owner(&mut owners, i)).or_default().push(i); }
    let groups: Vec<_> = groups.into_values().collect();
    let score = |group: &Vec<usize>| {
        let refs: BTreeSet<_> = group.iter().map(|i| eligible[*i]["ref"].as_str().unwrap()).collect();
        let incoming: BTreeSet<_> = edges.iter().filter_map(|edge| {
            let a = edge["source_ref"].as_str()?; let b = edge["target_ref"].as_str()?;
            (refs.contains(b) && !refs.contains(a)).then_some(a)
        }).collect();
        let mut spans: Vec<_> = group.iter().map(|i| (eligible[*i]["start_line"].as_u64().unwrap(), eligible[*i]["end_line"].as_u64().unwrap())).collect();
        spans.sort_unstable(); let mut covered = 0; let mut previous = 0;
        for (lo, hi) in spans { covered += hi.saturating_sub(previous.max(lo - 1)); previous = previous.max(hi); }
        let read_lines = group.iter().map(|i| eligible[*i]["end_line"].as_u64().unwrap()).max().unwrap()
            - group.iter().map(|i| eligible[*i]["start_line"].as_u64().unwrap()).min().unwrap() + 1;
        (std::cmp::Reverse(!incoming.is_empty()),
            group.iter().map(|i| eligible[*i]["depth"].as_u64().unwrap()).min().unwrap(),
            std::cmp::Reverse(group.len()), incoming.len(), std::cmp::Reverse(covered),
            read_lines, eligible[group[0]]["node_id"].as_str().unwrap())
    };
    let mut ranked: Vec<_> = (0..groups.len()).collect(); ranked.sort_by_key(|i| score(&groups[*i]));
    let mut file_counts = HashMap::new();
    let mut rounds: Vec<_> = ranked.into_iter().map(|i| {
        let file = eligible[groups[i][0]]["file_ref"].as_str().unwrap();
        let rank = file_counts.entry(file).or_insert(0usize); let round = *rank; *rank += 1; (round, i)
    }).collect();
    rounds.sort_by_key(|(round, i)| (*round, score(&groups[*i])));
    // Keep the useful selected-file discovery from the previous projection
    // even when route allocation clips the later optional navigation object.
    let mut hint_depths = BTreeMap::new();
    for node in nodes.values().filter(|node| node["admission"] == "selected"
        && node["kind"] == "file" && node["is_seed"] == false) {
        let Some(depth) = node["depth"].as_u64().filter(|depth| *depth > 0) else { continue };
        if node["file_ref"] == target["file_ref"] { continue; }
        let Some(path) = node["file_ref"].as_str().and_then(|r| files.get(r)).and_then(|file| file["path"].as_str()) else { continue };
        hint_depths.entry(path).and_modify(|old: &mut u64| *old = (*old).min(depth)).or_insert(depth);
    }
    let mut hints: Vec<_> = hint_depths.into_iter().collect();
    hints.sort_by(|(a, ad), (b, bd)| ad.cmp(bd).then_with(|| a.cmp(b)));
    let build = |chosen: &[usize]| {
        let mut references = BTreeSet::new(); let mut seeds = BTreeSet::new(); let mut reads = Vec::new();
        references.insert(target["ref"].as_str().unwrap().to_owned());
        for index in chosen {
            let group = &groups[*index]; let first = eligible[group[0]];
            let covered: Vec<_> = group.iter().map(|i| eligible[*i]["ref"].as_str().unwrap()).collect();
            for reference in &covered {
                let path = &traces[reference]; seeds.insert(path[0].clone()); references.extend(path.iter().cloned());
            }
            let start = group.iter().map(|i| eligible[*i]["start_line"].as_u64().unwrap()).min().unwrap();
            let end = group.iter().map(|i| eligible[*i]["end_line"].as_u64().unwrap()).max().unwrap();
            reads.push(json!({"covered_refs":covered,"tool":"get_full_method_body","arguments":{
                "project_id":project,"file_path":files[first["file_ref"].as_str().unwrap()]["path"],
                "line_start":start,"line_end":end,"context_lines":0,"include_caller_bodies":false,"output_json":true}}));
        }
        let selected_nodes: Vec<_> = references.iter().map(|r| nodes[r.as_str()]).collect();
        let file_refs: BTreeSet<_> = selected_nodes.iter().filter_map(|node| node["file_ref"].as_str()).collect();
        let selected_files: Vec<_> = file_refs.iter().filter_map(|r| files.get(r)).copied().collect();
        let selected_edges: Vec<_> = edges.iter().filter(|edge|
            edge["source_ref"].as_str().is_some_and(|r| references.contains(r))
                && edge["target_ref"].as_str().is_some_and(|r| references.contains(r))).map(|edge| {
            let mut edge = (*edge).clone(); edge.as_object_mut().unwrap().remove("recovery_ref"); edge
        }).collect();
        let selected_nodes: Vec<_> = selected_nodes.iter().map(|node| json!({"ref":node["ref"],"node_id":node["node_id"],
            "generation":node["generation"],"file_ref":node["file_ref"],
            "indexed_lines":[node["start_line"],node["end_line"]]})).collect();
        let mut projection = json!({"scope":"Selected links, not value flow/runtime. Reads inspect current ranges, not method/call bounds. Graph retains other branches/position reads. Input producers may lie outside reverse impact.",
            "requested_target_ref":target["ref"],"seed_refs":seeds,"admission":"selected","nodes":selected_nodes,
            "files":selected_files,"connections":selected_edges,"connection_defaults":full["omitted_defaults"]["edges"],
            "reads":reads,"eligible_windows":groups.len(),"windows_omitted":groups.len()-chosen.len(),
            "file_hints":[],"file_hints_omitted":hints.len(),
            "ineligible_retained_callables":ineligible});
        if target["kind"] == "file" {
            projection["seed_scope"] = json!("Callable file members are structural seeds, not causal file-to-member edges.");
        }
        projection
    };
    value["source_routes"] = build(&[]);
    if serde_json::to_vec(&value["source_routes"]).unwrap().len() > SOURCE_ROUTE_BYTES || !fits(value) {
        value.as_object_mut().unwrap().remove("source_routes"); value["source_routes_omitted"] = json!(true);
        if !fits(value) { value.as_object_mut().unwrap().remove("source_routes_omitted"); } return;
    }
    let mut chosen = Vec::new();
    for (_, index) in rounds {
        if chosen.len() == 3 { break; }
        chosen.push(index); let routes = build(&chosen);
        if serde_json::to_vec(&routes).unwrap().len() <= SOURCE_ROUTE_BYTES {
            value["source_routes"] = routes;
            if fits(value) { continue; }
        }
        chosen.pop(); value["source_routes"] = build(&chosen);
    }
    let mut shown = Vec::new();
    for (path, _) in &hints {
        if shown.len() == 2 { break; }
        shown.push(*path);
        value["source_routes"]["file_hints"] = json!(&shown);
        value["source_routes"]["file_hints_omitted"] = json!(hints.len() - shown.len());
        if serde_json::to_vec(&value["source_routes"]).unwrap().len() > SOURCE_ROUTE_BYTES || !fits(value) {
            shown.pop(); value["source_routes"]["file_hints"] = json!(&shown);
            value["source_routes"]["file_hints_omitted"] = json!(hints.len() - shown.len());
        }
    }
}

/// Optional navigation consumes only spare display space after all existing
/// witnesses and frontiers have been selected. It never queries a new HEAD.
fn append_consumer_navigation(
    value: &mut Value,
    full: &Value,
    project: &str,
    fits: &dyn Fn(&Value) -> bool,
) {
    if value["target_revision_admitted"] != true { return; }
    let immediate = value["immediate_consumers"].as_array().cloned().unwrap_or_default();
    if immediate.is_empty() { return; }
    let nodes: HashMap<_, _> = full["nodes"].as_array().into_iter().flatten()
        .filter_map(|node| Some((node["ref"].as_str()?, node))).collect();
    let files: HashMap<_, _> = full["files"].as_array().into_iter().flatten()
        .filter_map(|file| Some((file["ref"].as_str()?, &file["path"]))).collect();
    let recoveries: HashMap<_, _> = full["recoveries"].as_array().into_iter().flatten()
        .filter_map(|row| Some((row["ref"].as_str()?, row))).collect();
    let parents: Vec<_> = immediate.iter().filter_map(|row| {
        nodes.values().copied().find(|node| node["node_id"] == row["node_id"]
            && node["admission"] == "selected" && node["depth"] == 1)
    }).collect();
    let mut next = Vec::new();
    for parent in &parents {
        for edge in full["edges"].as_array().into_iter().flatten()
            .filter(|edge| edge["target_ref"] == parent["ref"]) {
            let Some(node) = edge["source_ref"].as_str().and_then(|r| nodes.get(r)).copied() else { continue };
            if node["admission"] != "selected" || node["depth"] != 2
                || node["node_id"] == parent["node_id"] { continue; }
            next.push((*parent, node));
        }
    }
    next.sort_by(|(ap, a), (bp, b)| ap["node_id"].as_str().cmp(&bp["node_id"].as_str())
        .then_with(||a["node_id"].as_str().cmp(&b["node_id"].as_str())));
    next.dedup_by(|(ap, a), (bp, b)| ap["node_id"] == bp["node_id"] && a["node_id"] == b["node_id"]);
    value["consumer_navigation"] = json!({
        "scope":"Fresh consumer-rooted direct queries; non-additive counts. Inline depth-2 edges and file hints sample retained selected data, not all callers/value flow. Navigate outward; edges point to dependencies.",
        "expansions":[],"expansions_omitted":immediate.len(),
        "next_level":[],"next_level_omitted":next.len()
    });
    if !fits(value) {
        value.as_object_mut().unwrap().remove("consumer_navigation");
        value["consumer_navigation_omitted"] = json!(true);
        if !fits(value) { value.as_object_mut().unwrap().remove("consumer_navigation_omitted"); }
        return;
    }
    let mut expansions = Vec::new();
    for parent in &parents {
        let Some(id) = parent["node_id"].as_str().filter(|id|
            !id.is_empty() && id.len() <= 2048 && !id.chars().any(char::is_control)) else { continue };
        expansions.push(json!({"tool":"impact_analysis","arguments":{
            "project_id":project,"symbol_fqn":id,"max_depth":1,"limit":4,"detail":"summary"}}));
        value["consumer_navigation"]["expansions"] = json!(&expansions);
        value["consumer_navigation"]["expansions_omitted"] = json!(immediate.len() - expansions.len());
        if !fits(value) {
            expansions.pop();
            value["consumer_navigation"]["expansions"] = json!(&expansions);
            value["consumer_navigation"]["expansions_omitted"] = json!(immediate.len() - expansions.len());
        }
    }
    // Round-robin by parent/file keeps a busy same-file branch from taking
    // every optional slot. Canonical ordering makes the sample deterministic.
    let mut file_counts = HashMap::new();
    let mut ranked: Vec<_> = next.into_iter().map(|(parent, node)| {
        let key = (parent["node_id"].as_str(), node["file_ref"].as_str());
        let count = file_counts.entry(key).or_insert(0usize);
        let rank = *count; *count += 1; (rank, parent, node)
    }).collect();
    ranked.sort_by(|(ar, ap, a), (br, bp, b)| ar.cmp(br)
        .then_with(||ap["node_id"].as_str().cmp(&bp["node_id"].as_str()))
        .then_with(||a["node_id"].as_str().cmp(&b["node_id"].as_str())));
    let total = ranked.len();
    let mut leads = Vec::new();
    for (_, parent, node) in ranked {
        if leads.len() == 4 { break; }
        let mut lead = describe_summary_node(full,node,&nodes,&files,&recoveries,Some(parent),false);
        let Some(mut connection) = lead.as_object_mut().unwrap().remove("sample_connection_to_target")
            .filter(|connection|!connection.is_null()) else { continue };
        // The two identities are already explicit in the unit. Position and
        // recovery provenance remain complete; no callsite is inferred.
        connection.as_object_mut().unwrap().remove("source_node_id");
        connection.as_object_mut().unwrap().remove("target_node_id");
        lead["parent_node_id"] = parent["node_id"].clone();
        lead["connection_to_parent"] = connection;
        leads.push(lead);
        value["consumer_navigation"]["next_level"] = json!(&leads);
        value["consumer_navigation"]["next_level_omitted"] = json!(total - leads.len());
        if !fits(value) {
            leads.pop();
            value["consumer_navigation"]["next_level"] = json!(&leads);
            value["consumer_navigation"]["next_level_omitted"] = json!(total - leads.len());
        }
    }
    // File identities farther down a retained branch may be the only bridge
    // from backend declarations to a consumer's source tree. These are paths
    // to inspect, not inferred value flow or additional admitted callers.
    // Allocate them last so existing witnesses and complete actions survive.
    let target_file = full["target_ref"].as_str().and_then(|reference| nodes.get(reference))
        .and_then(|node| node["file_ref"].as_str()).and_then(|reference| files.get(reference))
        .and_then(|path| path.as_str());
    let mut depths = BTreeMap::new();
    for node in nodes.values() {
        if node["admission"] != "selected" || node["kind"] != "file"
            || node["is_seed"] != false { continue; }
        let Some(depth) = node["depth"].as_u64().filter(|depth| *depth > 0) else { continue };
        let Some(path) = node["file_ref"].as_str().and_then(|reference| files.get(reference))
            .and_then(|path| path.as_str()).filter(|path| Some(*path) != target_file) else { continue };
        depths.entry(path).and_modify(|old: &mut u64| *old = (*old).min(depth)).or_insert(depth);
    }
    if depths.is_empty() { return; }
    let mut paths: Vec<_> = depths.into_iter().collect();
    paths.sort_by(|(a, ad), (b, bd)| ad.cmp(bd).then_with(|| a.cmp(b)));
    let total = paths.len();
    value["consumer_navigation"]["selected_file_hints"] = json!([]);
    value["consumer_navigation"]["selected_file_hints_omitted"] = json!(total);
    if !fits(value) {
        value["consumer_navigation"].as_object_mut().unwrap().remove("selected_file_hints");
        if !fits(value) {
            value["consumer_navigation"].as_object_mut().unwrap().remove("selected_file_hints_omitted");
        }
        return;
    }
    let mut hints = Vec::new();
    for (path, _) in paths {
        if hints.len() == 2 { break; }
        hints.push(path);
        value["consumer_navigation"]["selected_file_hints"] = json!(&hints);
        value["consumer_navigation"]["selected_file_hints_omitted"] = json!(total - hints.len());
        if !fits(value) {
            hints.pop();
            value["consumer_navigation"]["selected_file_hints"] = json!(&hints);
            value["consumer_navigation"]["selected_file_hints_omitted"] = json!(total - hints.len());
        }
    }
}

fn render_summary(value: &Value, immediate_prefix: &str) -> String {
    format!(
        "{}\n## Revision-admitted change impact\n\nSummary of the same bounded source selection. Expand the graph for all retained identities, connections and paths.\n\n```json\n{}\n```\n",
        immediate_prefix,
        serde_json::to_string(value).expect("JSON value serialization")
    )
}

// The first projection reserves conservative framing space. Once the actual
// envelope is known, use spare bytes to recover a search for an empty capped
// scan before adding optional consumer-expansion shortcuts. Keep every fact,
// witness and source route already selected by the first projection.
fn recover_capped_member_search(value: &mut Value, full: &Value, fits: &dyn Fn(&Value) -> bool) {
    let unresolved = &full["unbound_member_call_inspection"];
    if value.get("terminal_member_inspection").is_some()
        || unresolved["coverage"]["truncated"] != true
        || unresolved["distinct_callers_observed"] != 0
    {
        return;
    }
    let Some(inspection) = full.get("terminal_member_inspection") else { return; };
    let mut candidate = value.clone();
    candidate.as_object_mut().unwrap().remove("terminal_member_inspection_omitted");
    append_terminal_inspection(&mut candidate, inspection, fits);
    if candidate.get("terminal_member_inspection").is_some() && fits(&candidate) {
        *value = candidate;
    }
}

// A hinted file can have a selected callable whose route crosses an API or
// another non-callable node. Keep that declaration useful for source inspection
// without presenting it as one of the callable-only source routes. Reuse only
// this frame's explicit range action; do not resolve names or query source.
fn append_hinted_file_declarations(value: &mut Value, full: &Value, fits: &dyn Fn(&Value) -> bool) {
    let hints: Vec<_> = value["source_routes"]["file_hints"].as_array().into_iter().flatten()
        .filter_map(Value::as_str).map(str::to_owned).collect();
    if hints.is_empty() { return; }
    let files: HashMap<_, _> = full["files"].as_array().into_iter().flatten()
        .filter_map(|file| Some((file["ref"].as_str()?, file["path"].as_str()?))).collect();
    let recoveries: HashMap<_, _> = full["recoveries"].as_array().into_iter().flatten()
        .filter_map(|row| Some((row["ref"].as_str()?, row))).collect();
    let shown: BTreeSet<_> = value["source_routes"]["nodes"].as_array().into_iter().flatten()
        .chain(value["immediate_consumers"].as_array().into_iter().flatten())
        .filter_map(|node| node["node_id"].as_str()).map(str::to_owned).collect();
    let mut candidates = Vec::new();
    for node in full["nodes"].as_array().into_iter().flatten() {
        if node["admission"] != "selected" || node["is_seed"] != false
            || !matches!(node["kind"].as_str(), Some("function" | "method" | "constructor"))
        { continue; }
        let Some(id) = node["node_id"].as_str().filter(|id| !shown.contains(*id)) else { continue; };
        let Some(path) = node["file_ref"].as_str().and_then(|r| files.get(r)).copied() else { continue; };
        let Some(rank) = hints.iter().position(|hint| hint == path) else { continue; };
        let Some((start, end)) = node["start_line"].as_u64().zip(node["end_line"].as_u64()) else { continue; };
        if start == 0 || end < start || end - start >= 120 || node["generation"].as_u64().is_none() { continue; }
        let Some(recovery) = node["recovery_ref"].as_str().and_then(|r| recoveries.get(r)) else { continue; };
        if recovery["tool"] != "get_full_method_body" || recovery["scope_ref"] != "range"
            || recovery["arguments"]["file_path"] != path
            || recovery["arguments"]["line_start"].as_u64() != Some(start)
            || recovery["arguments"]["line_end"].as_u64() != Some(end)
        { continue; }
        candidates.push((rank, node["depth"].as_u64().unwrap_or(u64::MAX), start, id, node, *recovery));
    }
    candidates.sort_by(|a, b| (a.0, a.1, a.2, a.3).cmp(&(b.0, b.1, b.2, b.3)));
    if candidates.is_empty() { return; }
    let mut rows = Vec::new();
    let mut used_files = BTreeSet::new();
    for (rank, _, _, id, node, recovery) in &candidates {
        if rows.len() == 2 { break; }
        if used_files.contains(rank) { continue; }
        rows.push(json!({"node_id":id,"generation":node["generation"],
            "source_read":{"tool":recovery["tool"],"arguments":recovery["arguments"]}}));
        value["hinted_file_declarations"] = json!({
            "scope":"Selected declarations in hinted files, not value-flow routes or runtime binding. Reads inspect current indexed ranges; method bounds remain unverified.",
            "eligible_declarations_observed":candidates.len(),"declarations_omitted":candidates.len()-rows.len(),"declarations":rows});
        if fits(value) { used_files.insert(*rank); } else { rows.pop(); }
    }
    if rows.is_empty() {
        value.as_object_mut().unwrap().remove("hinted_file_declarations");
    } else {
        value["hinted_file_declarations"]["declarations"] = json!(&rows);
        value["hinted_file_declarations"]["declarations_omitted"] = json!(candidates.len()-rows.len());
    }
}

fn summary_section(
    full: &Value,
    project: &str,
    file_path: Option<&str>,
    symbol_fqn: Option<&str>,
    display_limit: usize,
    direct: Option<usize>,
    transitive: Option<usize>,
    dependent_files: Option<&BTreeMap<String, usize>>,
) -> String {
    let mut value=summary_value(full,project,file_path,symbol_fqn,display_limit,direct,transitive,dependent_files);
    let immediate_prefix=immediate_section(full,false).unwrap_or_default();
    // The only caller suffix is envelope::footer: fixed ASCII plus u64
    // generation/age (or unavailable timestamp), always below 256 bytes.
    // Measure the exact wrapper; retain the older, conservative row selection.
    let framing = render_summary(&Value::Null,&immediate_prefix).len() - 4;
    let fits = |candidate: &Value|
        serde_json::to_vec(candidate).expect("JSON value serialization").len()
            <= SUMMARY_BYTES.saturating_sub(framing + 256);
    recover_capped_member_search(&mut value, full, &fits);
    append_hinted_file_declarations(&mut value,full,&fits);
    append_consumer_navigation(&mut value,full,project,&fits);
    render_summary(&value,&immediate_prefix)
}

/// Display projection of an existing assessment, after its safety decision.
/// No new graph/source query or source-head capture occurs here.
pub(crate) fn assessment_summary(
    assessment: &ImpactAssessment,
    project: &str,
    exact_node_id: &str,
    display_limit: usize,
) -> anyhow::Result<Value> {
    let mut value=summary_value(&assessment.evidence,project,None,Some(exact_node_id),
        display_limit,assessment.direct_dependents_observed,assessment.transitive_dependents_observed,None);
    // Unavailable frames may lack graph fields. Preserve the typed capability
    // and nullable observations rather than inventing a layout or zero impact.
    value["source_generation"]=json!(assessment.source_generation);
    value["source_layout"]=json!(assessment.source_layout);
    value["requested_max_depth"]=json!(assessment.requested_max_depth);
    value["status"]=json!(assessment.status);
    value["target_revision_admitted"]=json!(assessment.target_revision_admitted);
    value["selected_dependents_observed"]=json!(assessment.selected_dependents_observed);
    value["coverage"]=json!(assessment.coverage);
    let fits = |candidate: &Value|
        serde_json::to_vec(candidate).expect("JSON value serialization").len()<=SUMMARY_BYTES;
    recover_capped_member_search(&mut value, &assessment.evidence, &fits);
    append_hinted_file_declarations(&mut value,&assessment.evidence,&fits);
    append_consumer_navigation(&mut value,&assessment.evidence,project,&fits);
    anyhow::ensure!(serde_json::to_vec(&value)?.len()<=SUMMARY_BYTES,
        "Required downstream summary facts exceed the 12KiB display bound");
    Ok(value)
}

/// Reserve file navigation after direct witnesses, before optional route and
/// inspection detail. Counts cover the held walk, not just rendered graph rows.
fn append_dependent_files(
    value: &mut Value,
    files: &BTreeMap<String, usize>,
    fits: &dyn Fn(&Value) -> bool,
) {
    value["observed_dependent_files"] = json!({
        "files_observed":files.len(),"files_omitted":files.len(),"files":[],
        "scope":"Selected external paths before display clipping; walk limits apply, not runtime proof."
    });
    if !fits(value) {
        value.as_object_mut().unwrap().remove("observed_dependent_files");
        value["observed_dependent_files_omitted"] = json!(true);
        if !fits(value) {
            value.as_object_mut().unwrap().remove("observed_dependent_files_omitted");
        }
        return;
    }
    let mut ranked: Vec<_> = files.iter().collect();
    ranked.sort_by(|(a_path,a_depth),(b_path,b_depth)|
        a_depth.cmp(b_depth).then_with(||a_path.cmp(b_path)));
    let mut rows = Vec::new();
    for (path, depth) in ranked {
        if rows.len() == 32 { break; }
        rows.push(json!({"file_path":path,"min_depth":depth}));
        value["observed_dependent_files"]["files"] = json!(&rows);
        value["observed_dependent_files"]["files_omitted"] = json!(files.len()-rows.len());
        if !fits(value) {
            rows.pop();
            value["observed_dependent_files"]["files"] = json!(&rows);
            value["observed_dependent_files"]["files_omitted"] = json!(files.len()-rows.len());
        }
    }
}

fn dependent_file_inventory(
    walk: &IncomingImpactWalk,
    requested_file: Option<&str>,
    admitted: bool,
) -> Option<BTreeMap<String, usize>> {
    let requested_file = requested_file.filter(|_|admitted)?;
    let mut files: BTreeMap<String, usize> = BTreeMap::new();
    for row in &walk.nodes {
        let path = row.node.file_path.as_str();
        if row.admission == ImpactAdmission::Selected && row.depth > 0
            && !path.is_empty() && path != requested_file {
            files.entry(path.to_owned()).and_modify(|depth|*depth=(*depth).min(row.depth))
                .or_insert(row.depth);
        }
    }
    Some(files)
}

#[cfg(test)]
mod dependent_file_navigation_behavior {
    use super::*;

    #[test]
    fn optional_file_navigation_preserves_evidence_when_only_some_paths_fit() {
        let original = json!({"coverage":{"truncated":true,"reasons":["file_seed_cap"]},
            "immediate_consumers":[{"source_read":{"tool":"get_full_method_body"}}],
            "frontier":[{"reason":"missing_graph_node"}],
            "graph_continuation":{"tool":"impact_analysis"}});
        let files = BTreeMap::from([
            (format!("src/{}.rs","a".repeat(2041)),1),
            ("src/brief.rs".into(),1),
            ("src/downstream.rs".into(),3),
        ]);
        for extra_bytes in [0,50,350,500,4096] {
            let mut report = original.clone();
            let limit = serde_json::to_vec(&report).unwrap().len()+extra_bytes;
            append_dependent_files(&mut report,&files,&|v|serde_json::to_vec(v).unwrap().len()<=limit);
            assert!(serde_json::to_vec(&report).unwrap().len()<=limit);
            if let Some(inventory) = report.get("observed_dependent_files") {
                let rows = inventory["files"].as_array().unwrap();
                assert_eq!(inventory["files_observed"],3);
                assert_eq!(rows.len() as u64+inventory["files_omitted"].as_u64().unwrap(),3);
                if extra_bytes==500 {
                    assert!(rows.iter().any(|row|row["file_path"]=="src/brief.rs"),
                        "One oversized path must not conceal later short paths that fit");
                    assert!(inventory["files_omitted"].as_u64().unwrap()>0);
                }
            }
            report.as_object_mut().unwrap().remove("observed_dependent_files");
            report.as_object_mut().unwrap().remove("observed_dependent_files_omitted");
            assert_eq!(report,original,"Navigation cannot evict or alter existing evidence");
        }
    }
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

#[derive(Clone)]
struct CompactEvidence<'a> {
    project: &'a str,
    observed_nodes: &'a [engram_graph::store::ImpactWalkNode],
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
        let key = serde_json::to_string(&json!({"tool":tool,"arguments":arguments,"scope_ref":scope_ref}))?;
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

    fn file_excerpt_recovery(&self, file: &str) -> Result<RecoveryDraft> {
        self.recovery(
            "get_full_method_body",
            json!({"project_id":self.project,
            "file_path":file,"context_lines":0,"output_json":true}),
            "file_excerpt",
        )
    }

    fn lookup_recovery(&self, endpoint: &Endpoint<'_>) -> Result<RecoveryDraft> {
        let (hint_file, hint_name) = canonical_hint(endpoint.id);
        let file = endpoint
            .file
            .or(hint_file)
            .filter(|path| recovery_path(path));
        let mut arguments = json!({"project_id":self.project,"limit":1});
        if let Some(file) = file {
            arguments["scope"] = json!(file);
        }
        // Missing graph identities are never repaired by name similarity.
        // A unique declaration already admitted in this held walk can still
        // provide an inspection lead, without adding reads or resolving edges.
        let hint_kind = endpoint.id.strip_prefix("sym:").and_then(|s| s.split(':').next());
        let hint_line = endpoint.id.rsplit_once(':').and_then(|(_, line)| line.parse::<u32>().ok());
        fn terminal(name: &str) -> &str { name.rsplit(['.', ':']).next().unwrap_or("") }
        if endpoint.admission == "unresolved" && file.is_some() && file == hint_file
            && hint_line.is_some_and(|line| line > 0)
            && !terminal(hint_name).is_empty()
        {
            let mut candidates = self.observed_nodes.iter().filter(|item| {
                item.admission == ImpactAdmission::Selected
                    && Some(item.node.file_path.as_str()) == file
                    && Some(item.node.node_type.as_str()) == hint_kind
                    && Some(item.node.start_line) == hint_line
                    && terminal(&item.node.name) == terminal(hint_name)
                    && item.node.node_id != endpoint.id
                    && item.node.node_id == engram_core::ids::NodeId::symbol(
                        &item.node.node_type, None, item.node.file_path.as_str(),
                        &item.node.name, item.node.start_line).0
            });
            if let Some(candidate) = candidates.next().filter(|_| candidates.next().is_none()) {
                arguments["symbol_node_id"] = json!(candidate.node.node_id);
                let mut recovery = self.recovery("get_codebase_overview", arguments, "inspection_candidate")?;
                if let Some((_, row)) = recovery.fresh.as_mut() {
                    row["inspection_candidate"] = json!({"node_id":candidate.node.node_id,
                        "generation":candidate.node.generation,
                        "basis":"Unique admitted declaration in this held walk with the same hinted file, kind, start line and terminal name. Inspection lead only; original identity and edge remain unresolved."});
                }
                return Ok(recovery);
            }
        }
        // An unresolved receiver is not an alternate identity for its method.
        // Inspect a unique admitted containing declaration without repairing
        // the receiver, its edge, or any uncertainty in the held walk.
        if endpoint.admission == "unresolved" && hint_kind == Some("self")
            && file.is_some() && file == hint_file && hint_line.is_some_and(|line| line > 0)
        {
            let line = hint_line.expect("checked positive hint");
            let mut candidates = self.observed_nodes.iter().filter(|item| {
                item.admission == ImpactAdmission::Selected
                    && Some(item.node.file_path.as_str()) == file
                    && matches!(item.node.node_type.as_str(), "function" | "method")
                    && item.node.start_line > 0 && item.node.start_line <= line && item.node.end_line >= line
                    && item.node.node_id == engram_core::ids::NodeId::symbol(
                        &item.node.node_type, None, item.node.file_path.as_str(),
                        &item.node.name, item.node.start_line).0
            });
            if let Some(candidate) = candidates.next().filter(|_| candidates.next().is_none()) {
                arguments["symbol_node_id"] = json!(candidate.node.node_id);
                let mut recovery = self.recovery("get_codebase_overview", arguments, "enclosing_declaration")?;
                if let Some((_, row)) = recovery.fresh.as_mut() {
                    row["enclosing_declaration"] = json!({"node_id":candidate.node.node_id,
                        "generation":candidate.node.generation,
                        "basis":"Unique admitted canonical callable in this held walk containing the unverified receiver's positive file/line hint. Enclosing context only; the receiver identity and edge remain unresolved."});
                }
                return Ok(recovery);
            }
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
            _ if endpoint.admission == "selected"
                && endpoint.kind == Some("file")
                && endpoint.file.is_some_and(recovery_path) =>
            {
                self.file_excerpt_recovery(endpoint.file.expect("checked file path"))?
            }
            _ => self.lookup_recovery(endpoint)?,
        };
        let reference = format!("n{}", self.nodes.len());
        let name = endpoint
            .name
            .map(|name| &name[..name.floor_char_boundary(256)]);
        let mut row = json!({"ref":reference,"node_id":endpoint.id,"name":name,"kind":endpoint.kind,
            "admission":endpoint.admission,"is_seed":endpoint.is_seed,"depth":endpoint.depth,
            "file_ref":file_ref,"start_line":endpoint.start,"end_line":endpoint.end,
            "generation":endpoint.generation,"recovery_ref":recovery.reference});
        row.as_object_mut()
            .expect("Node object")
            .retain(|_, value| !value.is_null());
        if endpoint.name.is_some_and(|name| name.len() > 256) {
            row["name_truncated"] = json!(true);
        }
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

fn append_configuration_path_discovery(value: &mut Value, discovery: &Value, fits: &impl Fn(&Value)->bool) {
    if !value["potential_interface_callers"]["configuration_inspection"].is_object() { return; }
    let mut actions=discovery["actions"].as_array().cloned().unwrap_or_default();
    let total=actions.len()+discovery["actions_omitted"].as_u64().unwrap_or(0) as usize;
    loop {
        let mut bounded=discovery.clone();
        bounded["actions"]=json!(&actions);bounded["actions_omitted"]=json!(total-actions.len());
        value["potential_interface_callers"]["configuration_inspection"]["path_discovery"]=bounded;
        if fits(value) { return; }
        if actions.pop().is_none() { break; }
    }
    let inspection=value["potential_interface_callers"]["configuration_inspection"].as_object_mut().unwrap();
    inspection.remove("path_discovery");
    inspection.insert("path_discovery_actions_omitted".into(),json!(total));
    if !fits(value) {
        value["potential_interface_callers"]["configuration_inspection"].as_object_mut().unwrap().remove("path_discovery_actions_omitted");
    }
}

fn promote_configuration_search(value: &mut Value, actions: Vec<Value>, total: usize, fits: &impl Fn(&Value)->bool) {
    if actions.is_empty() || !value["potential_interface_callers"]["configuration_inspection"].is_object() {return;}
    let generic=value["potential_interface_callers"]["configuration_inspection"].clone();
    let promoted=json!({"scope":"First try literal contract-name searches outside each action's listed known route-file prefixes. Prefix exclusions also cover similarly named files; preserve them when adding task exclusions. These actions have not run. Use unrestricted_fallback to include those files and colocated setup. Indexed matches do not prove registration, concrete dispatch, execution or absence.",
        "actions_omitted":total-actions.len(),"actions":actions,"unrestricted_fallback":generic.clone()});
    value["potential_interface_callers"]["configuration_inspection"]=promoted;
    if !fits(value) {
        value["potential_interface_callers"]["configuration_inspection"]=generic;
        value["potential_interface_callers"]["configuration_inspection"]["narrowed_actions_omitted"]=json!(total);
        if !fits(value) {value["potential_interface_callers"]["configuration_inspection"].as_object_mut().unwrap().remove("narrowed_actions_omitted");}
    }
}

fn configuration_inspection(project: &str, contract: &str, files: [&str; 3]) -> Option<Value> {
    if !files.iter().all(|file| recovery_path(file)) { return None; }
    // VB constructed type arguments can contain dots. Take the declaring
    // type before its argument list, then its literal simple identifier.
    let mut depth=0usize;
    let mut start=0;
    for (offset,ch) in contract.char_indices() {
        match ch {
            '(' => depth+=1,
            ')' => depth=depth.checked_sub(1)?,
            '.' if depth==0 => start=offset+1,
            _ => {}
        }
    }
    if depth!=0 { return None; }
    let name=contract[start..].split('(').next()?.trim();
    if name.len()>128 || !name.chars().next().is_some_and(|c|c=='_' || c.is_alphabetic())
        || !name.chars().all(|c|c=='_' || c.is_alphanumeric()) { return None; }
    let mut common=files[0].rsplit_once('/')?.0.split('/').collect::<Vec<_>>();
    for file in &files[1..] {
        let directory=file.rsplit_once('/')?.0.split('/').collect::<Vec<_>>();
        common.truncate(common.iter().zip(&directory).take_while(|(a,b)|a==b).count());
    }
    if common.is_empty() { return None; }
    Some(json!({"tool":"grep_project","arguments":{"project_id":project,"pattern":name,
        "regex":false,"case_sensitive":false,"path_prefix":format!("{}/",common.join("/")),
        "max_results":8,"context_before":2,"context_after":2,"freshness":"off","output_json":true}}))
}

fn render_interface_dispatch(walk: &IncomingImpactWalk, project: &str) -> Value {
    let dispatch=&walk.interface_dispatch;
    let mut value=json!({"status":"partial_static_dispatch_observation",
        "scope":"One explicit VB source-interface method boundary from selected reached methods, searching fully admitted recorded Calls. Rejected locators and unresolved aliases are not inventoried. Open/variant generic, partial-interface, implicit, inherited, property and other-language dispatch are unassessed. Empty routes do not prove absence of interface callers.",
        "non_additive_to_primary_counts":true,"runtime_dispatch_proven":false,
        "implementations_checked":dispatch.implementations_checked,"coverage":dispatch.coverage,
        "routes_observed":dispatch.routes.len(),"distinct_callers_observed":dispatch.coverage.nodes_observed,
        "routes":[],"routes_omitted":dispatch.routes.len(),"frontier":[],
        "frontier_omitted":dispatch.frontier.len()+dispatch.coverage.frontier_omitted});
    let endpoint=|node: &engram_graph::Node|json!({"node_id":node.node_id,"name":node.name,
        "file_path":node.file_path,"generation":node.generation,"start_line":node.start_line,"end_line":node.end_line});
    let mut rows=Vec::new();
    for route in &dispatch.routes {
        let caller=&route.caller;
        if caller.start_line==0 || caller.end_line<caller.start_line { continue; }
        let start=caller.start_line.saturating_sub(20).max(1);
        let end=caller.end_line.min(start.saturating_add(79));
        let outside: Vec<_>=route.reference_lines.iter().copied().filter(|line|*line<start || *line>end).collect();
        let row=json!({"implementation":endpoint(&route.implementation),"implementation_primary_depth":route.implementation_depth,
            "contract":endpoint(&route.contract),"caller":endpoint(caller),"potential_depth":route.implementation_depth+2,
            "contract_type_key":route.contract_type_key,"contract_type":route.contract_type,
            "caller_to_contract_admission":"source_revision_admitted_indexed_call",
            "contract_call_scope":"Caller-to-contract is a recorded static Calls relation under the selected revisions. Current bytes, concrete dispatch, execution and complete extraction remain unverified.",
            "connections":[{"source":"implementation","target":"contract","kind":"implements_interface_member","traversed":"outgoing",
                "generation":route.mapping_generation,"clause_line":route.clause_line},
                {"source":"caller","target":"contract","kind":"calls","traversed":"incoming","generation":route.call_generation,
                    "reference_lines":route.reference_lines,"reference_lines_truncated":route.reference_lines_truncated}],
            "caller_source_read":{"tool":"get_full_method_body","arguments":{"project_id":project,"file_path":caller.file_path,
                "line_start":start,"line_end":end,"context_lines":0,"output_json":true}},
            "reference_samples_outside_context":outside.len(),
            "recorded_position_read":outside.first().map(|line|json!({"tool":"get_full_method_body","arguments":{
                "project_id":project,"file_path":caller.file_path,"line_start":line,
                "line_end":caller.end_line.min(line.saturating_add(30)),"context_lines":0,"output_json":true}})),
            "source_read_scope":"Current caller context: up to20 lines before its declaration, at most80 lines through its indexed end. Callsite samples remain separate. Verify freshness; potential dispatch is not runtime binding proof."});
        if serde_json::to_vec(&row).expect("JSON serialization").len()>4096 { continue; }
        rows.push(row); value["routes"]=json!(&rows);
        if serde_json::to_vec(&value).expect("JSON serialization").len()>8*1024 { rows.pop();value["routes"]=json!(&rows);break; }
    }
    value["routes_omitted"]=json!(dispatch.routes.len()-rows.len());
    let mut frontier=Vec::new();
    for lead in &dispatch.frontier {
        let mut row=json!(lead);
        let hint=lead.file_path.as_ref().map(|path|path.as_str()).or_else(||canonical_hint(&lead.source_id).0);
        if let Some(file)=hint.filter(|file|recovery_path(file)) {
            row["inspection"] = if let Some(start)=lead.start_line.filter(|start|*start>0) {
                json!({"tool":"get_full_method_body","arguments":{"project_id":project,"file_path":file,
                    "line_start":start,"line_end":lead.end_line.unwrap_or(start).min(start.saturating_add(30)).max(start),"context_lines":0,"output_json":true},
                    "scope":"Current source at indexed position; an inspection lead, not an admitted dispatch edge."})
            } else { json!({"tool":"get_codebase_overview","arguments":{"project_id":project,"scope":file,"limit":1},
                "scope":"Navigation from an unverified identity's path hint; the declaration may be absent."}) };
        }
        frontier.push(row); value["frontier"]=json!(&frontier);
        if serde_json::to_vec(&value).expect("JSON serialization").len()>10*1024 { frontier.pop();value["frontier"]=json!(&frontier);break; }
    }
    value["frontier_omitted"]=json!(dispatch.frontier.len()-frontier.len()+dispatch.coverage.frontier_omitted);

    value
}

fn render_unbound_inspection(walk: &IncomingImpactWalk, project: &str) -> Option<Value> {
    let inspection = walk.unbound_member_inspection.as_ref()?;
    let mut value = json!({"status":"partial_same_terminal_syntax_inspection",
        "scope":"Recorded unresolved member syntax in the target directory's canonical JS/TS function IDs. This does not bind a receiver/callee or add primary callers. Other directories, older/noncanonical producers, type/alias/dispatch coverage and runtime behavior remain unassessed. Empty or capped results prove no absence.",
        "directory":inspection.directory,"member_name":inspection.member_name,
        "coverage":inspection.coverage,"decoded_bytes":inspection.decoded_bytes,
        "raw_row_limit":inspection.raw_row_limit,"undecoded_lookahead_observed":inspection.lookahead_observed,
        "raw_keys_observed":inspection.raw_keys_observed,
        "scoped_prefix_exhausted":inspection.scoped_prefix_exhausted,"witness_limit":inspection.witness_limit,
        "distinct_callers_observed":inspection.coverage.nodes_observed,
        "witnesses_retained":inspection.witnesses.len(),
        "additional_witnesses_omitted_lower_bound":inspection.witnesses_omitted_lower_bound,
        "candidates":[],"candidates_omitted_from_display":inspection.witnesses.len()});
    let mut rows = Vec::new();
    for witness in &inspection.witnesses {
        let caller = &witness.caller;
        if !recovery_path(caller.file_path.as_str()) || caller.start_line == 0 || caller.end_line < caller.start_line { continue; }
        let start = witness.reference_lines.first().copied().unwrap_or(caller.start_line)
            .saturating_sub(2).max(caller.start_line);
        let end = caller.end_line.min(start.saturating_add(79));
        let mut context = json!({"project_id":project,"file_path":caller.file_path,"method_name":caller.name.rsplit('.').next().unwrap_or(&caller.name),
            "line":caller.start_line,"include_history":false,"include_business_logic":false,"include_full_body":false,
            "include_caller_bodies":false,"max_callers":2,"output_json":true});
        if let Some((prefix, _)) = caller.name.rsplit_once('.') {
            context["class_name"] = json!(prefix.rsplit('.').next().unwrap_or(prefix));
        }
        let row = json!({"caller":{"node_id":caller.node_id,"name":caller.name,"file_path":caller.file_path,
                "generation":caller.generation,"start_line":caller.start_line,"end_line":caller.end_line},
            "unresolved_target_id":witness.unresolved_target_id,"receiver":witness.receiver,
            "receiver_scope":"Producer's bounded receiver hint; inspect source for the full expression",
            "edge_generation":witness.edge_generation,"source_position_kind":witness.source_position_kind,
            "reference_lines":witness.reference_lines,"reference_lines_truncated":witness.reference_lines_truncated,
            "reference_samples_outside_context":witness.reference_lines.iter().filter(|line| **line < start || **line > end).count(),
            "actions":[
                {"tool":"get_full_method_body","arguments":{"project_id":project,"file_path":caller.file_path,
                    "line_start":start,"line_end":end,"context_lines":0,"output_json":true}},
                {"tool":"get_method_edit_context","arguments":context},
                {"tool":"impact_analysis","arguments":{"project_id":project,"symbol_fqn":caller.node_id,
                    "max_depth":6,"limit":12,"detail":"summary"}}]});
        if serde_json::to_vec(&row).expect("JSON serialization").len() > 4096 { continue; }
        rows.push(row); value["candidates"] = json!(&rows);
        if serde_json::to_vec(&value).expect("JSON serialization").len() > 8 * 1024 {
            rows.pop(); value["candidates"] = json!(&rows); break;
        }
    }
    value["candidates_omitted_from_display"] = json!(inspection.witnesses.len() - rows.len());
    Some(value)
}

/// Optional inspection never displaces canonical consumers, interface routes
/// or unknown frontiers. Keep exact omission counts when the descriptor fits;
/// the ordinary graph continuation remains available when it does not.
fn append_unbound_inspection(value: &mut Value, inspection: &Value, fits: &dyn Fn(&Value) -> bool) {
    let mut projected = inspection.clone();
    projected["candidates"] = json!([]);
    projected.as_object_mut().unwrap().remove("caller_file_start_inspections_omitted");
    let omitted = inspection["candidates_omitted_from_display"].as_u64().unwrap_or(0);
    let mut candidates = inspection["candidates"].as_array().cloned().unwrap_or_default();
    // A graph may already carry these late optional actions. Refit the old
    // summary evidence first, then independently allocate context actions.
    for candidate in &mut candidates {
        candidate.as_object_mut().unwrap().remove("caller_file_start_inspection");
    }
    projected["candidates_omitted_from_display"] = json!(omitted + candidates.len() as u64);
    value["unbound_member_call_inspection"] = projected;
    if !fits(value) {
        value.as_object_mut().unwrap().remove("unbound_member_call_inspection");
        value["unbound_member_call_inspection_omitted"] = json!(true);
        if !fits(value) { value.as_object_mut().unwrap().remove("unbound_member_call_inspection_omitted"); }
        return;
    }
    let mut retained = Vec::new();
    for candidate in candidates.iter() {
        retained.push(candidate.clone());
        value["unbound_member_call_inspection"]["candidates"] = json!(&retained);
        value["unbound_member_call_inspection"]["candidates_omitted_from_display"] = json!(omitted + (candidates.len() - retained.len()) as u64);
        if !fits(value) {
            retained.pop();
            value["unbound_member_call_inspection"]["candidates"] = json!(&retained);
            value["unbound_member_call_inspection"]["candidates_omitted_from_display"] = json!(omitted + (candidates.len() - retained.len()) as u64);
            break;
        }
    }
}

/// A separate current-file inspection lead may expose fields/construction
/// outside the invocation clip. Allocate only after all existing evidence;
/// never remove a witness, older action or terminal search to make it fit.
fn append_caller_file_starts(value: &mut Value, project: &str, fits: &dyn Fn(&Value) -> bool) {
    let count=value["unbound_member_call_inspection"]["candidates"].as_array().map_or(0,Vec::len);
    let mut omitted=count.saturating_sub(8);
    for index in 0..count.min(8) {
        let Some(file)=value["unbound_member_call_inspection"]["candidates"][index]["caller"]["file_path"]
            .as_str().filter(|path|recovery_path(path)).map(str::to_owned) else { continue; };
        value["unbound_member_call_inspection"]["candidates"][index]["caller_file_start_inspection"]=json!({
            "scope":"Optional current caller-file beginning for possible receiver type/construction context. This action has not run, may omit the origin, and does not bind a receiver/callee. Follow returned paging/hash checks; inspect the invocation separately.",
            "action":{"tool":"get_full_method_body","arguments":{"project_id":project,"file_path":file,
                "line_start":1,"context_lines":0,"output_json":true}}});
        let candidate=&value["unbound_member_call_inspection"]["candidates"][index];
        if serde_json::to_vec(candidate).expect("JSON serialization").len()>4096
            || serde_json::to_vec(&value["unbound_member_call_inspection"]).expect("JSON serialization").len()>8*1024
            || !fits(value) {
            value["unbound_member_call_inspection"]["candidates"][index].as_object_mut().unwrap()
                .remove("caller_file_start_inspection");
            omitted+=1;
        }
    }
    if omitted>0 {
        value["unbound_member_call_inspection"]["caller_file_start_inspections_omitted"]=json!(omitted);
        if serde_json::to_vec(&value["unbound_member_call_inspection"]).expect("JSON serialization").len()>8*1024 || !fits(value) {
            value["unbound_member_call_inspection"].as_object_mut().unwrap().remove("caller_file_start_inspections_omitted");
        }
    }
}

#[cfg(test)]
mod caller_file_start_behavior {
    use super::*;

    #[test]
    fn class_name_recovery_preserves_vb_case_inspection_and_mandatory_evidence() {
        let mut node = engram_graph::Node {
            node_id:"sym:class:src/contracts.vb:Demo.ViewportDescriptor:3".into(),
            node_type:"class".into(),name:"Demo.ViewportDescriptor".into(),
            namespace:"memory".into(),language:"vb".into(),
            file_path:engram_core::RelPath::new("src/contracts.vb"),
            start_line:3,end_line:8,generation:7,metadata:None,
        };
        let inspection = type_name_inspection(&node, "project").unwrap();
        assert_eq!(inspection["action"]["arguments"]["pattern"], "ViewportDescriptor");
        assert_eq!(inspection["action"]["arguments"]["case_sensitive"], false);
        let original = json!({"target":{"name":"Demo.ViewportDescriptor"},
            "coverage":{"truncated":true},"immediate_consumers":[{"source_read":{"tool":"get_full_method_body"}}]});
        let mut full = original.clone();
        append_type_name_inspection(&mut full, &inspection, &|_|true);
        let size = serde_json::to_vec(&full).unwrap().len();
        for limit in [serde_json::to_vec(&original).unwrap().len(), size - 1, size] {
            let mut actual = original.clone();
            append_type_name_inspection(&mut actual, &inspection, &|value|serde_json::to_vec(value).unwrap().len() <= limit);
            assert!(serde_json::to_vec(&actual).unwrap().len() <= limit);
            let action = actual.as_object_mut().unwrap().remove("type_name_inspection");
            actual.as_object_mut().unwrap().remove("type_name_inspection_omitted");
            assert_eq!(action.is_some(), limit == size);
            assert_eq!(actual, original, "Recovery cannot evict source witnesses or coverage");
        }
        for name in ["Demo.Invalid(Type)", "Demo.", "Demo.a:b", "Demo.9bad"] {
            node.name = name.into();
            assert!(type_name_inspection(&node, "project").is_none());
        }
        node.name = "Demo.ViewportDescriptor".into();
        node.node_id = "legacy-class".into();
        assert!(type_name_inspection(&node, "project").is_none());
        node.node_id = "sym:class:../outside.vb:Demo.ViewportDescriptor:3".into();
        node.file_path = engram_core::RelPath::new("../outside.vb");
        assert!(type_name_inspection(&node, "project").is_none());
    }

    #[test]
    fn hinted_file_declarations_cannot_displace_evidence_or_guess_a_source_range() {
        let original=json!({"source_routes":{"file_hints":["src/client.ts"],"nodes":[]},
            "coverage":{"truncated":true},"frontier":[{"reason":"missing_graph_node"}],
            "terminal_member_inspection":{"action":{"tool":"grep_project"}}});
        let frame=json!({"files":[{"ref":"f1","path":"src/client.ts"}],
            "nodes":[{"ref":"n1","node_id":"sym:function:src/client.ts:save:10","kind":"function",
                "admission":"selected","is_seed":false,"file_ref":"f1","generation":7,
                "depth":4,"start_line":10,"end_line":20,"recovery_ref":"r1"}],
            "recoveries":[{"ref":"r1","tool":"get_full_method_body","scope_ref":"range",
                "arguments":{"project_id":"project","file_path":"src/client.ts","line_start":10,
                    "line_end":20,"context_lines":0,"output_json":true}}]});
        let mut roomy=original.clone();
        append_hinted_file_declarations(&mut roomy,&frame,&|_|true);
        assert_eq!(roomy["hinted_file_declarations"]["declarations"].as_array().unwrap().len(),1);
        let required_bytes=serde_json::to_vec(&roomy).unwrap().len();
        for limit in [serde_json::to_vec(&original).unwrap().len(),required_bytes-1,required_bytes] {
            let mut actual=original.clone();
            append_hinted_file_declarations(&mut actual,&frame,&|value|serde_json::to_vec(value).unwrap().len()<=limit);
            assert!(serde_json::to_vec(&actual).unwrap().len()<=limit);
            let lead=actual.as_object_mut().unwrap().remove("hinted_file_declarations");
            assert_eq!(lead.is_some(),limit==required_bytes);
            assert_eq!(actual,original,"Navigation must preserve every preselected fact and recovery");
        }
        for (field,bad_value) in [("admission",json!("unresolved")),("generation",Value::Null),
            ("end_line",json!(130)),("is_seed",json!(true))] {
            let mut bad=frame.clone();bad["nodes"][0][field]=bad_value;
            let mut actual=original.clone();append_hinted_file_declarations(&mut actual,&bad,&|_|true);
            assert_eq!(actual,original,"Unusable declaration must not become a convenient source lead");
        }
        let mut wrong_range=frame.clone();wrong_range["recoveries"][0]["arguments"]["file_path"]=json!("src/other.ts");
        let mut actual=original.clone();append_hinted_file_declarations(&mut actual,&wrong_range,&|_|true);
        assert_eq!(actual,original);
    }

    #[test]
    fn member_recovery_sacrifices_broad_fallback_before_focused_action_or_existing_evidence() {
        let original = json!({"selected_dependents_observed":3,"coverage":{"truncated":true}});
        let inspection = json!({"action":{"tool":"grep_project","arguments":{"pattern":".policy"}},
            "unrestricted_fallback":{"tool":"grep_project","arguments":{"pattern":"policy","path_prefix":"src/map/"}}});
        let mut focused = inspection.clone();
        focused.as_object_mut().unwrap().remove("unrestricted_fallback");
        focused["unrestricted_fallback_omitted"] = json!(true);
        let mut expected = original.clone();
        expected["terminal_member_inspection"] = focused;
        let size = serde_json::to_vec(&expected).unwrap().len();
        let mut report = original.clone();
        append_terminal_inspection(&mut report, &inspection, &|value|serde_json::to_vec(value).unwrap().len() <= size);
        assert_eq!(report, expected);
        let mut report = original.clone();
        let size = serde_json::to_vec(&original).unwrap().len();
        append_terminal_inspection(&mut report, &inspection, &|value|serde_json::to_vec(value).unwrap().len() <= size);
        assert_eq!(report, original);
    }

    fn baseline(padding: usize, candidates: usize) -> Value {
        json!({"selected_dependents_observed":3,"coverage":{"truncated":true},
            "terminal_member_inspection":{"action":{"tool":"grep_project"}},
            "unbound_member_call_inspection":{"candidates_omitted_from_display":0,
                "candidates":(0..candidates).map(|index|json!({
                    "caller":{"file_path":format!("src/images/group{index}.ts")},
                    "reference_lines":[131,132],"actions":[{"tool":"get_full_method_body"}],
                    "retained_witness": "x".repeat(padding)})).collect::<Vec<_>>()}})
    }

    fn without_new_actions(mut value: Value) -> Value {
        let inspection=&mut value["unbound_member_call_inspection"];
        inspection.as_object_mut().unwrap().remove("caller_file_start_inspections_omitted");
        for row in inspection["candidates"].as_array_mut().unwrap() {
            row.as_object_mut().unwrap().remove("caller_file_start_inspection");
        }
        value
    }

    #[test]
    fn caller_file_start_actions_never_displace_witnesses_or_exceed_response_budgets() {
        // Crowding at the row, inspection section and total response limits
        // must sacrifice only optional context, preserving the original facts.
        for (padding, count, whole_limit) in [(3750,1,RESPONSE_BYTES),(3500,2,RESPONSE_BYTES),
            (0,2,SUMMARY_BYTES-1024),(0,2,RESPONSE_BYTES)] {
            let original=baseline(padding,count);
            let mut report=original.clone();
            let limit=if padding==0 {serde_json::to_vec(&report).unwrap().len()+150} else {whole_limit};
            append_caller_file_starts(&mut report,"project",&|v|serde_json::to_vec(v).unwrap().len()<=limit);
            assert_eq!(without_new_actions(report.clone()),original);
            assert!(serde_json::to_vec(&report).unwrap().len()<=limit);
            let inspection=&report["unbound_member_call_inspection"];
            assert!(serde_json::to_vec(inspection).unwrap().len()<=8192);
            for row in inspection["candidates"].as_array().unwrap() {
                assert!(serde_json::to_vec(row).unwrap().len()<=4096);
            }
            assert!(inspection["caller_file_start_inspections_omitted"].as_u64().unwrap_or(0)>0);
        }
    }

    #[test]
    fn caller_file_start_graph_actions_cannot_evict_summary_witnesses() {
        let original=baseline(0,2);
        let mut graph=original.clone();
        append_caller_file_starts(&mut graph,"project",&|_|true);
        assert!(graph["unbound_member_call_inspection"]["candidates"][0]
            .get("caller_file_start_inspection").is_some());
        let mut summary=original.clone();
        summary.as_object_mut().unwrap().remove("unbound_member_call_inspection");
        let limit=serde_json::to_vec(&original).unwrap().len();
        append_unbound_inspection(&mut summary,&graph["unbound_member_call_inspection"],
            &|v|serde_json::to_vec(v).unwrap().len()<=limit);
        assert_eq!(summary,original);
        append_caller_file_starts(&mut summary,"project",&|v|serde_json::to_vec(v).unwrap().len()<=limit);
        assert_eq!(summary,original);
    }
}

fn append_rendered_edge(
    out: &mut CompactEvidence<'_>,
    edge: &engram_graph::store::ImpactWitness,
    source: &engram_graph::store::ImpactWalkNode,
    source_ref: String,
    target_ref: String,
    is_tree: bool,
) -> Result<bool> {
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
    // A page directive or file marker does not delimit file-wide ownership.
    let ownership_range_known =
        caller_range_known && !matches!(source.node.node_type.as_str(), "file" | "page");
    // Some stitched UI edges retain the handler's declaration anchor on
    // the markup control edge. It is evidence, but not a position in the
    // control's file. Never turn that contradiction into a fabricated read.
    let weak_anchor = edge.source_position_kind == "extractor_anchor_metadata";
    let position_scope = if edge.source_line.is_none() {
        "not_recorded"
    } else if weak_anchor && ownership_range_known && !line_within_caller {
        "unattributed_extractor_anchor"
    } else if weak_anchor && !ownership_range_known {
        "unverified_file_anchor"
    } else {
        "indexed_metadata_only"
    };
    let recovery = match edge.source_line {
        Some(line)
            if !line_within_caller
                && position_scope != "unattributed_extractor_anchor"
                && recovery_path(source.node.file_path.as_str()) =>
        {
            Some(out.range_recovery(source.node.file_path.as_str(), line, line)?)
        }
        _ => None,
    };
    let recovery_ref = recovery.as_ref().map_or_else(
        || source_recovery.clone(),
        |recovery| recovery.reference.clone(),
    );
    let reference = format!("e{}", out.edges.len());
    let mut row = json!({"ref":reference,"source_ref":source_ref,"target_ref":target_ref,"kind":edge.kind.as_str(),
        "edge_generation":edge.edge_generation});
    if edge.edge_generation != source.node.generation {
        row["edge_source_binding"] = json!("verified_unchanged_caller_origin");
    }
    if !is_tree {
        row["connection_role"] = json!("additional_verified_connection");
    }
    if let Some(line) = edge.source_line {
        row["source_line"] = json!(line);
        row["source_position_kind"] = json!(edge.source_position_kind);
        row["source_position_scope"] = json!(position_scope);
    }
    if !edge.reference_lines.is_empty() {
        row["reference_lines"] = json!(edge.reference_lines);
    }
    if let Some(line) = edge.extractor_anchor_line {
        row["extractor_anchor_line"] = json!(line);
    }
    if edge.reference_lines_truncated {
        row["reference_lines_truncated"] = json!(true);
    }
    if recovery_ref != source_recovery {
        row["recovery_ref"] = json!(recovery_ref);
    }
    let mut staged = vec![&row];
    if let Some((_, value)) = recovery
        .as_ref()
        .and_then(|recovery| recovery.fresh.as_ref())
    {
        staged.push(value);
    }
    if !out.charge(&staged)? { return Ok(false); }
    if let Some(recovery) = recovery {
        out.commit_recovery(recovery);
    }
    out.edge_refs.insert(
        (source_ref, target_ref, edge.kind.as_str().into()),
        reference,
    );
    out.edges.push(row);
    Ok(true)
}

/// Stage the whole connection: a failed endpoint/position read must not leave
/// an orphan consuming the budget intended for a later usable witness.
fn append_witness_unit(
    out: &mut CompactEvidence<'_>,
    edge: &engram_graph::store::ImpactWitness,
    lookup: &HashMap<&str, &engram_graph::store::ImpactWalkNode>,
) -> Result<bool> {
    let (Some(source),Some(target))=(lookup.get(edge.source_id.as_str()),lookup.get(edge.target_id.as_str())) else {
        return Ok(false);
    };
    if source.admission!=ImpactAdmission::Selected || target.admission!=ImpactAdmission::Selected {
        return Ok(false);
    }
    let is_tree=source.predecessor.as_deref()==Some(edge.target_id.as_str())
        && source.via_kind.as_ref()==Some(&edge.kind);
    if let (Some(a),Some(b))=(out.reference(&edge.source_id),out.reference(&edge.target_id)) {
        if out.edge_refs.contains_key(&(a.clone(),b.clone(),edge.kind.as_str().to_owned())) { return Ok(true); }
        // Existing endpoints need only the already-atomic edge/recovery charge;
        // avoid copying the projection for every additional connection.
        return append_rendered_edge(out,edge,source,a,b,is_tree);
    }
    let mut staged=out.clone();
    let Some(source_ref)=staged.endpoint(&node_endpoint(source))? else { return Ok(false); };
    let Some(target_ref)=staged.endpoint(&node_endpoint(target))? else { return Ok(false); };
    if !append_rendered_edge(&mut staged,edge,source,source_ref,target_ref,is_tree)? { return Ok(false); }
    *out=staged;
    Ok(true)
}

fn compact_description(out: &CompactEvidence<'_>, target_ref: &str, id: &str) -> Option<Value> {
    let frame=json!({"target_ref":target_ref,"nodes":out.nodes,"files":out.files,
        "recoveries":out.recoveries,"edges":out.edges});
    projected_description(&frame,id)
}

fn projected_description(frame: &Value, id: &str) -> Option<Value> {
    let nodes:HashMap<_,_>=frame["nodes"].as_array()?.iter()
        .filter_map(|node|Some((node["ref"].as_str()?,node))).collect();
    let files:HashMap<_,_>=frame["files"].as_array()?.iter()
        .filter_map(|file|Some((file["ref"].as_str()?,&file["path"]))).collect();
    let recoveries:HashMap<_,_>=frame["recoveries"].as_array()?.iter()
        .filter_map(|row|Some((row["ref"].as_str()?,row))).collect();
    let target=frame["target_ref"].as_str().and_then(|reference|nodes.get(reference)).copied();
    let node=nodes.values().copied().find(|node|node["node_id"]==id)?;
    Some(describe_summary_node(&frame,node,&nodes,&files,&recoveries,target,
        target.is_some_and(|node|node["kind"]=="file")))
}

/// Choose the same canonical, file-diverse direct rows that can actually fit
/// a summary with their connection. This uses the held walk only, not another
/// graph transaction, source read, name lookup or admission decision.
fn preferred_direct_witnesses<'a>(
    walk: &'a IncomingImpactWalk,
    target: &str,
    project: &str,
) -> Result<Vec<&'a engram_graph::store::ImpactWitness>> {
    let lookup:HashMap<_,_>=walk.nodes.iter().map(|node|(node.node.node_id.as_str(),node)).collect();
    let Some(root)=lookup.get(target).filter(|node|node.admission==ImpactAdmission::Selected) else {
        return Ok(Vec::new());
    };
    let file_component=root.node.node_type=="file";
    let mut direct:Vec<_>=walk.nodes.iter().filter(|node|
        node.admission==ImpactAdmission::Selected && node.depth==1).collect();
    direct.sort_by(|a,b|a.node.node_id.cmp(&b.node.node_id));
    let mut counts=HashMap::new();
    let mut ranked:Vec<_>=direct.into_iter().map(|node| {
        let count=counts.entry(node.node.file_path.as_str()).or_insert(0usize);
        let rank=*count;*count+=1;(rank,node)
    }).collect();
    ranked.sort_by(|(a_rank,a),(b_rank,b)|a_rank.cmp(b_rank)
        .then_with(||a.node.node_id.cmp(&b.node.node_id)));
    let mut probe=CompactEvidence {project,observed_nodes:&walk.nodes,remaining:RESPONSE_BYTES,
        files:Vec::new(),file_refs:BTreeMap::new(),nodes:Vec::new(),node_indices:BTreeMap::new(),
        edges:Vec::new(),edge_refs:BTreeMap::new(),recoveries:Vec::new(),recovery_refs:BTreeMap::new()};
    let Some(root_ref)=probe.endpoint(&node_endpoint(root))? else { return Ok(Vec::new()); };
    let mut preferred=Vec::new();
    for (_,source) in ranked {
        let edge=walk.witnesses.iter().filter(|edge| {
            edge.source_id==source.node.node_id && lookup.get(edge.target_id.as_str()).is_some_and(|reached|
                reached.admission==ImpactAdmission::Selected && reached.depth==0
                    && (reached.node.node_id==target || (file_component && reached.node.file_path==root.node.file_path)))
        }).min_by_key(|edge|!(source.predecessor.as_deref()==Some(edge.target_id.as_str())
            && source.via_kind.as_ref()==Some(&edge.kind)));
        let Some(edge)=edge else { continue; };
        let mut staged=probe.clone();
        if !append_witness_unit(&mut staged,edge,&lookup)? { continue; }
        let Some(row)=compact_description(&staged,&root_ref,&source.node.node_id) else { continue; };
        if row["sample_connection_to_target"].is_null()
            || serde_json::to_vec(&row)?.len()>2048 { continue; }
        probe=staged;preferred.push(edge);
        if preferred.len()==4 { break; }
    }
    Ok(preferred)
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
    let ordinary=render_projection(walk,target,project,generation,max_depth,display_limit,managed,&[])?;
    if ordinary["edges_omitted"].as_u64().unwrap_or(0)==0 { return Ok(ordinary); }
    let preferred=preferred_direct_witnesses(walk,target,project)?;
    if preferred.iter().all(|edge|projected_description(&ordinary,&edge.source_id).is_some_and(|row|
        !row["sample_connection_to_target"].is_null()
            && serde_json::to_vec(&row).expect("JSON value serialization").len()<=2048)) { return Ok(ordinary); }
    render_projection(walk,target,project,generation,max_depth,display_limit,managed,&preferred)
}

fn render_projection(
    walk: &IncomingImpactWalk,
    target: &str,
    project: &str,
    generation: u64,
    max_depth: usize,
    display_limit: usize,
    managed: bool,
    preferred: &[&engram_graph::store::ImpactWitness],
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
        "selected_dependent_identities_returned":0,"node_identities_omitted":0,"unconnected_file_seeds_omitted":0,"edges_omitted":0,
        "frontier_rows_omitted":walk.coverage.frontier_omitted,"shortest_paths_omitted":total,
        "omitted_from_display":total,"display_truncated":false,"files":[],"nodes":[],"edges":[],
        "shortest_paths":[],"frontier":[],"recoveries":[],"continuation":null,
        "omitted_defaults":{
            "nodes":{"name_truncated":false,"optional_metadata":null},
            "edges":{"edge_source_binding":"extraction_generation","connection_role":"shortest_predecessor","source_line":null,"source_position_kind":"not_recorded","source_position_scope":"not_recorded","extractor_anchor_line":null,"reference_lines":[],"reference_lines_truncated":false},
            "edge_source_generation":"Read nodes[source_ref].generation; edge_generation remains explicit. A different edge_generation requires the explicit verified_unchanged_caller_origin binding.",
            "edge_recovery":"Absent recovery_ref means nodes[source_ref].recovery_ref. source_line prefers a recorded call/reference sample, otherwise a weaker extractor anchor. An unattributed_extractor_anchor contradicts the known source span: retain its number but do not assign it to that file; node recovery inspects the source node only. An unverified_file_anchor has no known span and offers a candidate file read, not ownership proof. Other recorded positions use the caller range when covered, otherwise an explicit line recovery. Samples are not exhaustive; truncation means omitted or uninspected positions. Indexed metadata does not prove current binding or execution."
        },
        "recovery_scopes":{
            "range":"Current-source range at indexed positions, not complete-file, method-boundary, callsite or value-flow proof. Verify returned file hash/freshness and project exclusions.",
            "file_excerpt":"Bounded current-file lines with a complete-file hash. Follow next_page and compare hashes across fresh reads; no indexed freshness, method boundaries or invocation location is verified.",
            "navigation":"Bounded selected-source CodeMap for the known file, or a project representative when the file is unknown. Best-effort exploration only: no recovered binding, absence or runtime proof.",
            "inspection_candidate":"Exact CodeMap navigation to a unique separately admitted declaration matching the missing identity's hinted file, kind, start line and terminal name. Candidate only, not identity equivalence; the original graph identity and edge remain unresolved. Follow-up is a fresh observation and may not rediscover the declaration within its own bounds.",
            "enclosing_declaration":"Exact CodeMap navigation to a unique admitted canonical callable containing an unresolved receiver's unverified file/line hint. Enclosing context only, not receiver binding or identity equivalence; the original graph identity and edge remain unresolved. Follow-up is a fresh bounded observation; verify returned source hash and freshness.",
            "direct":"Narrow direct impact for the admitted exact identity. A follow-up uses its own source selection; no complete pagination or atomic replay is promised.",
        },
        "working_tree_checked":false,"exhaustive_codebase":false,"change_breakage_proven":false,
        "display_scope":"limit applies to shortest_paths. Unconnected file seed declarations may be omitted with a separate CodeMap inventory; retained identities and connections remain subject to the response budget.",
        "edge_direction":"source_ref is caller/dependent; target_ref is the reached dependency",
        "limitations":["Selected means indexed node/file revisions and edge extraction/origin admission, not compiler, runtime or proposed-change validation.",
        "Additional verified connections include cross/revisit edges; they do not enumerate every simple path or imply field/value flow.",
        "Source positions distinguish call_reference_syntax_metadata from weaker extractor_anchor_metadata. reference_lines contains at most 8 sorted recorded samples, never an exhaustive call list; a sample may be a delegate/property reference rather than a direct invocation. Declaration-range recovery is not a callsite. Current source may have changed; verify recovery hashes/freshness and project exclusions.",
        "Missing, unverified, dynamic and uninspected bindings remain unknown. Work/depth/byte caps may omit evidence; empty output proves no absence."]});
    value["potential_interface_callers"]=render_interface_dispatch(walk,project);
    let mut required: std::collections::HashSet<&str> = walk.witnesses.iter()
        .flat_map(|edge|[edge.source_id.as_str(),edge.target_id.as_str()]).collect();
    required.insert(target);
    for node in &walk.nodes {
        if node.depth>0 { required.insert(&node.node.node_id); }
        if let Some(parent)=node.predecessor.as_deref() { required.insert(parent); }
    }
    for lead in walk.frontier.iter().chain(&walk.interface_dispatch.frontier) {
        required.insert(&lead.source_id);
        if let Some(parent)=lead.target_id.as_deref() { required.insert(parent); }
    }
    for route in &walk.interface_dispatch.routes {
        required.extend([route.implementation.node_id.as_str(),route.contract.node_id.as_str(),route.caller.node_id.as_str()]);
    }
    let requested_file=lookup.get(target).filter(|node|
        node.admission==ImpactAdmission::Selected && node.node.node_type=="file")
        .map(|node|node.node.file_path.as_str());
    let unconnected_seed=|node: &engram_graph::store::ImpactWalkNode|
        node.depth==0 && requested_file==Some(node.node.file_path.as_str())
            && !required.contains(node.node.node_id.as_str());
    let unconnected_seeds=selected.iter().filter(|node|unconnected_seed(node)).count();
    value["unconnected_file_seeds_omitted"]=json!(unconnected_seeds);
    if unconnected_seeds>0 {
        value["declaration_inventory"]=json!({"tool":"get_codebase_overview",
            "arguments":{"project_id":project,"scope":requested_file,"limit":1},
            "scope":"Fresh bounded declaration inventory for the requested file; follow its symbol pages. Unconnected file seeds were omitted only from this display, not traversal or observed counts; they are not extra dependents. Verify source_generation before combining fresh results."});
    }
    let mut out = CompactEvidence {
        project,
        observed_nodes: &walk.nodes,
        remaining: RESPONSE_BYTES.saturating_sub(section(&value).len() + 1024 + IMMEDIATE_BYTES),
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
    let mut ordered: Vec<_> = selected.iter().copied().filter(|node|!unconnected_seed(node)).collect();
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
    let tree_edge = |edge: &engram_graph::store::ImpactWitness| {
        lookup.get(edge.source_id.as_str()).is_some_and(|node| {
            node.predecessor.as_deref() == Some(edge.target_id.as_str())
                && node.via_kind.as_ref() == Some(&edge.kind)
        })
    };
    let mut edges: Vec<_> = walk.witnesses.iter().collect();
    edges.sort_by_key(|edge| !tree_edge(edge));
    let mut edge_omissions = 0;
    if preferred.is_empty() {
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
            if !append_rendered_edge(&mut out,edge,source,source_ref,target_ref,tree_edge(edge))? {
                edge_omissions += 1;
            }
        }
    } else {
        // A bounded second projection of this SAME admitted walk. Preserve
        // complete useful connections before optional standalone identities.
        if let Some(root)=lookup.get(target) {
            let current=node_endpoint(root);
            if let Some(reference)=out.endpoint(&current)? {
                value["target_ref"]=json!(&reference);
                if let Some(recovery)=out.lookup_boundary(&current)? {
                    value["continuation"]=json!({"at_ref":reference,"recovery_ref":recovery,
                        "scope_ref":if current.admission=="selected"{"direct"}else{"navigation"}});
                }
            }
        }
        for edge in preferred.iter().copied().chain(edges.iter().copied()) {
            append_witness_unit(&mut out,edge,&lookup)?;
        }
        for node in ordered {
            out.endpoint(&node_endpoint(node))?;
        }
        edge_omissions=walk.witnesses.len().saturating_sub(out.edges.len());
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
    let selected_returned=out.nodes.iter().filter(|node|node["admission"]=="selected").count();
    let dependent_returned=out.nodes.iter().filter(|node|node["admission"]=="selected" && node["is_seed"]==false).count();
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
    // Configuration discovery is optional. Allocate it only after every
    // canonical graph row, so its availability cannot displace evidence.
    let mut suggestions=BTreeMap::new();
    let mut narrowed=BTreeMap::new();
    for route in &walk.interface_dispatch.routes {
        let files=[route.implementation.file_path.as_str(),route.contract.file_path.as_str(),route.caller.file_path.as_str()];
        if let Some(action)=configuration_inspection(project,&route.contract_type,files) {
            let key=(action["arguments"]["pattern"].as_str().unwrap().to_owned(),
                action["arguments"]["path_prefix"].as_str().unwrap().to_owned());
            let excluded=files.into_iter().map(str::to_owned).collect::<std::collections::BTreeSet<_>>().into_iter().collect::<Vec<_>>();
            let mut preferred=action.clone();preferred["arguments"]["exclude_path_prefixes"]=json!(&excluded);
            narrowed.entry((key.clone(),excluded)).or_insert(preferred);
            suggestions.entry(key).or_insert(action);
        }
    }
    if !suggestions.is_empty() {
        let total=suggestions.len();
        let mut actions=suggestions.into_values().take(2).collect::<Vec<_>>();
        loop {
            value["potential_interface_callers"]["configuration_inspection"]=json!({"scope":"Optional indexed literal text searches within the shared directory of admitted implementation, contract and caller files. Matches may include configuration, declarations, comments or unrelated names; neither registration, routing, runtime dispatch nor absence is proven. Setup elsewhere or beyond the match cap can be missed. Follow discovered files with current source/hash inspection.","actions":actions,"actions_omitted":total-actions.len()});
            let fits=section(&value).len()<=RESPONSE_BYTES && serde_json::to_vec(&value["potential_interface_callers"]).expect("JSON serialization").len()<=10*1024;
            if fits { break; }
            if actions.pop().is_none() {
                value["potential_interface_callers"].as_object_mut().unwrap().remove("configuration_inspection");
                value["potential_interface_callers"]["configuration_inspection_actions_omitted"]=json!(total);
                if section(&value).len()>RESPONSE_BYTES || serde_json::to_vec(&value["potential_interface_callers"]).expect("JSON serialization").len()>10*1024 {
                    value["potential_interface_callers"].as_object_mut().unwrap().remove("configuration_inspection_actions_omitted");
                }
                break;
            }
        }
    }
    let total=narrowed.len();
    promote_configuration_search(&mut value,narrowed.into_values().take(2).collect(),total,&|value|
        section(value).len()<=RESPONSE_BYTES && serde_json::to_vec(&value["potential_interface_callers"]).expect("JSON serialization").len()<=10*1024);
    // Alternative path discovery is allocated after the literal search and
    // its unrestricted fallback, never ahead of canonical evidence.
    // No extra source-map, filesystem, graph or source-body scan occurs here.
    let scopes=value["potential_interface_callers"]["configuration_inspection"]["actions"]
        .as_array().into_iter().flatten()
        .filter_map(|action|action["arguments"]["path_prefix"].as_str())
        .map(|scope|scope.trim_end_matches('/').to_owned()).collect::<std::collections::BTreeSet<_>>();
    if !scopes.is_empty() {
        let actions=scopes.into_iter().take(2).map(|scope|json!({"tool":"get_codebase_overview","arguments":{
            "project_id":project,"scope":scope,"path_contains":"Config","limit":4}})).collect::<Vec<_>>();
        let discovery=json!({"scope":"Alternative lookup: selected paths containing literal Config within the admitted route's shared directory. This action has not run. File names are advisory, not registration, binding or runtime proof. Follow returned source/hash actions; use the text-search actions if names/scopes are insufficient. Other configuration names can be missed.","actions":actions,"actions_omitted":0});
        append_configuration_path_discovery(&mut value,&discovery,&|value|
            section(value).len()<=RESPONSE_BYTES && serde_json::to_vec(&value["potential_interface_callers"]).expect("JSON serialization").len()<=10*1024);
    }
    if let Some(inspection) = render_unbound_inspection(walk, project) {
        append_unbound_inspection(&mut value, &inspection, &|value| section(value).len() <= RESPONSE_BYTES);
    }
    // Optional source discovery comes after canonical evidence. A terminal
    // mention can recover an unassessed receiver, never establish its callee.
    if let Some(root) = lookup.get(target).filter(|n|
        n.admission == ImpactAdmission::Selected && n.node.node_type == "function") {
        let path = root.node.file_path.as_str();
        let name = root.node.name.rsplit('.').next().unwrap_or("");
        if recovery_path(path) && (path.ends_with(".ts") || path.ends_with(".tsx"))
            && name.len() <= 128 && !name.is_empty()
            && name.chars().enumerate().all(|(i,c)| c == '_' || c == '$'
                || if i == 0 { c.is_alphabetic() } else { c.is_alphanumeric() }) {
            if let Some((directory, _)) = path.rsplit_once('/') {
                let fallback = json!({"tool":"grep_project","arguments":{"project_id":project,
                        "pattern":name,"regex":false,"case_sensitive":true,"path_prefix":format!("{directory}/"),
                        "max_results":8,"context_before":2,"context_after":2,"freshness":"off","output_json":true}});
                let mut action = fallback.clone();
                action["arguments"]["pattern"] = json!(format!(".{name}"));
                let inspection = json!({
                    "scope":"First inspect indexed literal .member mentions in the target directory. Spaced/bracket access and bare names need the unrestricted_fallback (same directory, bare name); it may be omitted by the display budget. Comments, prefix names and other receivers can match; this does not bind a receiver/callee, prove execution or absence, or add callers to impact counts. Follow returned current-source actions/hashes; inspect the caller and its separate impact before inferring a route.",
                    "action":action,"unrestricted_fallback":fallback});
                append_terminal_inspection(&mut value, &inspection, &|value| section(value).len() <= RESPONSE_BYTES);
            }
        }
    }
    if let Some(root) = lookup.get(target).filter(|n| n.admission == ImpactAdmission::Selected) {
        if let Some(inspection) = type_name_inspection(&root.node, project) {
            append_type_name_inspection(&mut value, &inspection, &|value| section(value).len() <= RESPONSE_BYTES);
        }
    }
    append_caller_file_starts(&mut value, project, &|value|section(value).len()<=RESPONSE_BYTES);
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
                        ResolveResult::Ambiguous(candidates) => {
                            let (value, coverage) = ambiguous_target_value(
                                &graph, &membership, &project, &symbol, candidates,
                                generation, max_depth, display_limit, source.is_managed(),
                            )?;
                            return Ok(CollectedImpact {
                                value, source, generation, target_revision_admitted: false,
                                coverage, direct_dependents: 0, transitive_dependents: 0,
                                dependent_files: None,
                                advisory_scope: AdvisoryScope::Unavailable("target_name_ambiguous"),
                            });
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
        let advisory_scope=if !target_revision_admitted {
            AdvisoryScope::Unavailable("target_revision_not_admitted")
        } else if internal_file.is_some() {
            if seed_cap {
                AdvisoryScope::Unavailable("file_member_inventory_capped")
            } else if seeds.iter().any(|id|!walk.nodes.iter().any(|node|
                node.depth==0 && node.node.node_id==*id && node.admission==ImpactAdmission::Selected)) {
                AdvisoryScope::Unavailable("file_members_not_fully_admitted")
            } else {
                AdvisoryScope::CompleteFile(seeds.iter().filter(|id|*id!=&target).cloned().collect())
            }
        } else { AdvisoryScope::Symbol };
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
        let dependent_files = dependent_file_inventory(&walk, internal_file.as_deref(), target_revision_admitted);
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
            value["potential_interface_callers"]=json!({"status":"unavailable","routes_observed":null,"routes_omitted":null,
                "routes":[],"frontier":[],"frontier_omitted":null,"runtime_dispatch_proven":false,
                "reason":"The requested target revision was not admitted; unavailable evidence is not zero potential callers."});
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
            dependent_files,
            advisory_scope,
        })
    })
    .await
    .context("Impact evidence worker unavailable")?
}

/// Disambiguation is a separate bounded observation, never an admitted root or
/// an impact walk. Even one surviving selected candidate still needs selection.
fn ambiguous_target_value(
    graph: &engram_graph::GraphStore,
    membership: &engram_index::hybrid::SelectedSourceRevisionLookup,
    project: &str,
    symbol: &str,
    mut candidates: Vec<engram_graph::Node>,
    generation: u64,
    max_depth: usize,
    display_limit: usize,
    managed: bool,
) -> Result<(Value, ImpactWalkCoverage)> {
    const INSPECT: usize = 64;
    const RECORD_BYTES: usize = 64 * 1024;
    const RESOLUTION_BYTES: usize = 6 * 1024;
    let observed = candidates.len();
    let qualifier = symbol.rsplit_once('.').map(|(prefix, _)|
        prefix.rsplit('.').next().unwrap_or(prefix));
    // A file-stem hint only orders inspection leads; it does not bind a class
    // or receiver and must never turn this ambiguity into automatic selection.
    let hinted = |node: &engram_graph::Node| {
        let stem = node.file_path.as_str().rsplit('/').next()
            .and_then(|file| file.rsplit_once('.').map(|(stem, _)| stem));
        node.name == symbol || qualifier.is_some() && stem == qualifier
    };
    candidates.sort_by(|a, b| hinted(b).cmp(&hinted(a))
        .then_with(|| a.file_path.as_str().cmp(b.file_path.as_str()))
        .then_with(|| a.start_line.cmp(&b.start_line))
        .then_with(|| a.node_id.cmp(&b.node_id)));
    let mut seen = std::collections::HashSet::new();
    let (mut inspected, mut selected, mut rejected, mut unknown, mut duplicates) = (0, 0, 0, 0, 0);
    let mut rows = Vec::new();
    for candidate in candidates.iter().take(INSPECT) {
        inspected += 1;
        if !seen.insert(candidate.node_id.as_str()) { duplicates += 1; continue; }
        if candidate.node_id.is_empty() || candidate.node_id.len() > 2048
            || candidate.node_id.chars().any(char::is_control)
            || !recovery_path(candidate.file_path.as_str()) {
            unknown += 1; continue;
        }
        // At most two bounded record reads per inspected candidate (8MiB in
        // total), including records already returned by the legacy resolver.
        // A missing/oversized/corrupt record is unavailable, not a stale match.
        let node = match graph.get_node_bounded(project, &candidate.node_id, RECORD_BYTES) {
            Ok(Some((node, _))) if node.node_id == candidate.node_id
                && node.file_path == candidate.file_path && node.generation == candidate.generation => node,
            _ => { unknown += 1; continue; }
        };
        let file = match graph.get_node_bounded(project,
            &format!("file:{}", node.file_path.as_str()), RECORD_BYTES) {
            Ok(Some((file, _))) => file,
            _ => { unknown += 1; continue; }
        };
        match admit_selected_source(membership, &node, Some(&file))? {
            ImpactAdmission::Selected => selected += 1,
            ImpactAdmission::NonSelectedRevision => { rejected += 1; continue; }
            _ => { unknown += 1; continue; }
        }
        let read = (node.start_line > 0 && node.end_line >= node.start_line).then(||
            json!({"tool":"get_full_method_body","arguments":{"project_id":project,
                "file_path":node.file_path.as_str(),"line_start":node.start_line,
                "line_end":node.end_line.min(node.start_line.saturating_add(119)),
                "context_lines":0,"output_json":true}}));
        let row = json!({"node_id":node.node_id,"name":node.name.chars().take(240).collect::<String>(),
            "kind":node.node_type,"file_path":node.file_path.as_str(),"generation":node.generation,
            "start_line":node.start_line,"end_line":node.end_line,
            "admission":"selected_candidate_not_selected_target","source_read":read,
            "impact":{"tool":"impact_analysis","arguments":{"project_id":project,
                "symbol_fqn":node.node_id,"max_depth":max_depth,
                "limit":display_limit.clamp(1,25),"detail":"graph"}}});
        if rows.len() < 8 {
            rows.push(row);
            // Reserve the fixed scope and counters before optional rows.
            if serde_json::to_vec(&rows)?.len() > RESOLUTION_BYTES - 2048 { rows.pop(); }
        }
    }
    let displayed = rows.len();
    let resolution = json!({"status":"ambiguous","selection_required":true,"candidates":rows,
        "scope":"Resolver suggestions admitted by the held source selection, not proven matches for the requested qualifier, class or receiver. Select an exact identity explicitly. Resolver completeness is unknown; a singleton displayed list does not resolve ambiguity. Source reads are capped current-file ranges, not method-boundary or binding proof. Actions observe fresh state; compare source generation and hashes.",
        "ordering":"Exact stored name or qualifier/file-stem hint first, then path/line/identity; inspection order only, no automatic binding.",
        "coverage":{"resolver_completeness":"unknown","resolver_candidates_observed":observed,
            "inspection_limit":INSPECT,"record_limit_bytes":RECORD_BYTES,
            "candidates_inspected":inspected,"candidates_not_inspected":observed-inspected,
            "duplicate_candidate_ids":duplicates,"non_selected_candidates":rejected,
            "unavailable_candidates":unknown,"selected_candidates_observed":selected,
            "selected_candidates_omitted":selected-displayed,"display_limit":8,
            "section_limit_bytes":RESOLUTION_BYTES}});
    ensure!(serde_json::to_vec(&resolution)?.len() <= RESOLUTION_BYTES, "Target recovery exceeds budget");
    let coverage = ImpactWalkCoverage {
        truncated: true, reasons: vec!["target_name_ambiguous".into(), "target_candidate_inventory_not_exhaustive".into()],
        ..Default::default()
    };
    let mut value = unavailable_value(max_depth,
        &anyhow::anyhow!("Impact target is ambiguous; select an exact candidate identity"));
    value["source_generation"] = json!(generation);
    value["source_layout"] = json!(if managed { "managed" } else { "attested_legacy" });
    value["coverage"] = json!(&coverage);
    value["target_resolution"] = resolution;
    ensure!(section(&value).len() <= RESPONSE_BYTES, "Target recovery exceeds response budget");
    Ok((value, coverage))
}

fn unavailable_value(max_depth: usize, error: &anyhow::Error) -> Value {
    json!({"schema_version":2,"source_generation":null,"requested_max_depth":max_depth,
        "target_revision_admitted":false,"selected_dependents_observed":null,"working_tree_checked":false,
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
    collect_with_detail(
        state,
        project,
        file_path,
        symbol_fqn,
        max_depth,
        display_limit,
        crate::models::ImpactDetail::Graph,
    )
    .await
}

pub async fn collect_with_detail(
    state: &AppState,
    project: &str,
    file_path: Option<String>,
    symbol_fqn: Option<String>,
    max_depth: usize,
    display_limit: usize,
    detail: crate::models::ImpactDetail,
) -> (String, Option<Arc<SourceSnapshotGuard>>, Option<u64>) {
    let (text,source,generation,_)=collect_with_advisory_detail(state,project,file_path,symbol_fqn,max_depth,display_limit,detail).await;
    (text,source,generation)
}

pub(crate) async fn collect_for_advisory(
    state: &AppState, project: &str, file_path: Option<String>, symbol_fqn: Option<String>,
    max_depth: usize, display_limit: usize,
) -> (String, Option<Arc<SourceSnapshotGuard>>, Option<u64>, AdvisoryScope) {
    collect_with_advisory_detail(state,project,file_path,symbol_fqn,max_depth,display_limit,crate::models::ImpactDetail::Graph).await
}

async fn collect_with_advisory_detail(
    state: &AppState, project: &str, file_path: Option<String>, symbol_fqn: Option<String>,
    max_depth: usize, display_limit: usize, detail: crate::models::ImpactDetail,
) -> (String, Option<Arc<SourceSnapshotGuard>>, Option<u64>, AdvisoryScope) {
    let result = collect_evidence(
        state,
        project,
        TargetRequest::Resolve {
            file_path: file_path.clone(),
            symbol_fqn: symbol_fqn.clone(),
        },
        max_depth,
        display_limit,
    )
    .await;
    match result {
        Ok(collected) => (
            if detail == crate::models::ImpactDetail::Summary {
                summary_section(
                    &collected.value,
                    project,
                    file_path.as_deref(),
                    symbol_fqn.as_deref(),
                    display_limit,
                    collected
                        .target_revision_admitted
                        .then_some(collected.direct_dependents),
                    collected
                        .target_revision_admitted
                        .then_some(collected.transitive_dependents),
                    collected.dependent_files.as_ref(),
                )
            } else {
                section(&collected.value)
            },
            Some(collected.source),
            Some(collected.generation),
            collected.advisory_scope,
        ),
        Err(error) => {
            let value = unavailable_value(max_depth, &error);
            (
                if detail == crate::models::ImpactDetail::Summary {
                    summary_section(
                        &value,
                        project,
                        file_path.as_deref(),
                        symbol_fqn.as_deref(),
                        display_limit,
                        None,
                        None,
                        None,
                    )
                } else {
                    section(&value)
                },
                None,
                None,
                AdvisoryScope::Unavailable("source_evidence_unavailable"),
            )
        }
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
