//! Rebuild Handles wiring from admitted declarations when only markup changes.
//! Endpoint location and the file which produced a relationship are distinct.
use anyhow::{Context, Result, ensure};
use engram_core::{RelPath, ids::NodeId};
use engram_graph::{Edge, EdgeKind, Node};
use engram_index::IngestStats;
use serde_json::json;
use std::collections::{HashMap, HashSet};

pub(crate) const PRODUCER: &str = "_engram_event_producer";

#[derive(Default)]
pub(crate) struct PreparedBindings {
    pub edges: Vec<Edge>,
    _reservation: Option<engram_core::memory::AllocationGuard>,
    _source: Option<std::sync::Arc<engram_index::SourceSnapshotGuard>>,
}

fn handles(signature: &str) -> Vec<&str> {
    let Some(clause) = engram_index::vb_extractor::declared_handles_clause(signature) else {
        return Vec::new();
    };
    clause
        .split(',')
        .filter_map(|part| {
            let (control, event) = part.trim().split_once('.')?;
            let valid =
                |s: &str| !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '_');
            (valid(control)
                && valid(event)
                && !matches!(
                    control.to_ascii_lowercase().as_str(),
                    "me" | "mybase" | "myclass"
                ))
            .then_some(control)
        })
        .collect()
}

/// Managed-writer provenance; extractor metadata is strings, never this object.
pub(crate) fn stamp(edge: &mut Edge, producer: &str, revision: u64, owner: &str, source: &Node) {
    let metadata = edge
        .metadata
        .get_or_insert_with(|| json!({}))
        .as_object_mut()
        .expect("managed edge metadata");
    metadata.insert(
        PRODUCER.into(),
        json!({"version":1,"path":producer,"generation":revision,"owner_class":owner}),
    );
    if source.generation != edge.generation {
        metadata.insert(
            "_engram_source_origin".into(),
            json!({"version":1,"source_id":source.node_id,"source_generation":source.generation}),
        );
    }
}

/// Called before graph mutation. Full/mixed extraction already has fresh VB
/// wiring; only unchanged companions require a stored-declaration projection.
pub(crate) async fn inherited_handles(
    state: &crate::state::AppState,
    project: &str,
    generation: u64,
    stats: &IngestStats,
) -> Result<PreparedBindings> {
    let requires_projection = |p: &RelPath, e: &engram_index::ExtractedEdge| {
        e.kind == "codebehind_file"
            && engram_index::webforms::is_webforms_markup(std::path::Path::new(p.as_str()))
            && e.target_name
                .rsplit_once('.')
                .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("vb"))
            && stats
                .symbols
                .iter()
                .any(|(path, s)| path.as_str() == p.as_str() && s.kind == "control")
            && !stats.all_files.iter().any(|p| p.as_str() == e.target_name)
    };
    if !stats.edges.iter().any(|(p, e)| requires_projection(p, e)) {
        return Ok(PreparedBindings::default());
    }
    let reservation = engram_core::memory::AllocationGuard::try_new(
        state.memory_budget.as_ref(),
        32 * 1024 * 1024,
        engram_core::memory::Subsystem::Graph,
        "WebForms admitted companion metadata",
    )?;
    ensure!(
        stats.all_files.len() <= 250000,
        "WebForms source inventory exceeds limit"
    );
    let mut pages = Vec::new();
    let mut inventory_bytes = 0usize;
    let mut control_count = 0usize;
    for (markup, edge) in &stats.edges {
        if !requires_projection(markup, edge) {
            continue;
        }
        ensure!(
            pages.len() < 4096,
            "WebForms companion inventory exceeds limit"
        );
        let owners: HashSet<_> = stats
            .edges
            .iter()
            .filter(|(p, e)| p == markup && e.kind == "codebehind_class")
            .filter_map(|(_, e)| e.metadata.as_ref()?.get("fqn"))
            .collect();
        ensure!(
            owners.len() == 1,
            "WebForms companion class ownership is ambiguous: {}",
            markup.as_str()
        );
        let owner = owners.into_iter().next().unwrap();
        inventory_bytes = inventory_bytes
            .saturating_add(markup.as_str().len() + edge.target_name.len() + owner.len());
        ensure!(
            inventory_bytes <= 2 * 1024 * 1024,
            "WebForms companion text exceeds limit"
        );
        let mut controls = HashMap::new();
        for (_, symbol) in stats
            .symbols
            .iter()
            .filter(|(p, s)| p == markup && s.kind == "control")
        {
            control_count += 1;
            inventory_bytes = inventory_bytes.saturating_add(symbol.name.len());
            ensure!(
                control_count <= 16384 && inventory_bytes <= 2 * 1024 * 1024,
                "WebForms control inventory exceeds limit"
            );
            controls.insert(symbol.name.clone(), symbol.start_line);
        }
        pages.push((
            markup.as_str().to_owned(),
            edge.target_name.clone(),
            owner.as_str().to_owned(),
            controls,
        ));
    }
    if pages.is_empty() {
        return Ok(PreparedBindings::default());
    }
    ensure!(
        pages.len() <= 4096,
        "WebForms companion inventory exceeds limit"
    );
    let runtime = state
        .get_project_cached(project)
        .context("WebForms companion selection is unavailable")?;
    let old = super::project_service::get_active_generation(state, project).await?;
    let view = std::sync::Arc::new(super::project_service::pin_source_generation(
        state,
        &runtime.search,
        project,
        old,
    )?);
    let membership = runtime
        .search
        .selected_source_revision_lookup(project, old, view.clone())?;
    let graph = state.graph.clone();
    let pid = project.to_owned();
    tokio::task::spawn_blocking(move || -> Result<PreparedBindings> {
        let mut result = Vec::new();
        let mut retained_bytes=0usize;
        for (markup, companion, owner, controls) in pages {
            let file = graph.get_node(&pid, &NodeId::file(&companion).0)?.context("WebForms companion metadata is absent")?;
            ensure!(membership.matches(&companion,file.generation)? == Some(true), "WebForms companion revision is not admitted: {companion}");
            let methods = graph.file_members_bounded(&pid, &companion, 4096)?;
            let mut signature_bytes = 0usize;
            for method in methods {
                if method.node_type != "function" || method.file_path.as_str() != companion { continue; }
                ensure!(super::impact_walk_service::admit_selected_source(&membership,&method,Some(&file))?
                    == engram_graph::store::ImpactAdmission::Selected, "WebForms handler revision is not admitted: {}", method.node_id);
                let Some((class,_)) = method.name.rsplit_once('.') else { continue; };
                if !class.eq_ignore_ascii_case(&owner) { continue; }
                let signature = method.metadata.as_ref().and_then(|m|m.get("signature")).and_then(|v|v.as_str())
                    .context("WebForms companion lacks complete stored signatures; refresh the codebehind before markup")?;
                signature_bytes = signature_bytes.saturating_add(signature.len());
                ensure!(signature.len() <= 64 * 1024 && signature_bytes <= 1024 * 1024, "WebForms signature metadata exceeds limit");
                for control in handles(signature) {
                    let mut matching=controls.iter().filter(|(id,_)|id.eq_ignore_ascii_case(control));
                    let Some((control,line))=matching.next() else { continue; };
                    ensure!(matching.next().is_none(),"Ambiguous VB control identity");
                    let control=control.as_str();
                    retained_bytes=retained_bytes.saturating_add(2048+2*(markup.len()+control.len()+companion.len()+method.node_id.len()+method.name.len()));
                    ensure!(result.len()<8192 && retained_bytes<=16*1024*1024,"WebForms Handles inventory exceeds byte/count limit");
                    let source = Node { node_id:NodeId::control(&markup,control).0,node_type:"control".into(),name:control.into(),
                        namespace:"memory".into(),language:"aspx".into(),file_path:RelPath::new(&markup),start_line:*line,end_line:*line,generation,metadata:None };
                    let mut edge = Edge { source_id:source.node_id.clone(),target_id:method.node_id.clone(),edge_kind:EdgeKind::Dependency,
                        namespace:"memory".into(),language:"vbnet".into(),weight:1,generation,updated_at_ms:crate::utils::now_ms(),
                        metadata:Some(json!({"fqn":method.name,"src_line":method.start_line.to_string(),"binding_evidence":"admitted_vb_handles_signature"})) };
                    stamp(&mut edge,&companion,file.generation,&owner,&source);
                    result.push(edge);
                }
            }
        }
        Ok(PreparedBindings { edges:result,_reservation:Some(reservation),_source:Some(view) })
    }).await?
}

/// Stamp freshly extracted control wiring without advancing a foreign control
/// node to the producer's generation. Markup-owned edges retain their owner.
pub(crate) async fn stamp_extracted(
    state: &crate::state::AppState,
    project: &str,
    nodes: &[Node],
    edges: &mut [Edge],
    producers: &HashMap<(String, String), (String, String)>,
    stats: &IngestStats,
) -> Result<()> {
    let graph = &state.graph;
    let mut lookup = None;
    for edge in edges {
        if edge.edge_kind != EdgeKind::Dependency {
            continue;
        }
        let Some((path, owner)) = producers.get(&(edge.source_id.clone(), edge.target_id.clone()))
        else {
            continue;
        };
        let source = match nodes.iter().find(|n| n.node_id == edge.source_id) {
            Some(source) => Some(source.clone()),
            None => graph.get_node(project, &edge.source_id)?,
        };
        let Some(source) = source else {
            continue;
        }; // An unresolved endpoint acquires no proof.
        if !stats.all_files.iter().any(|p| p == &source.file_path) {
            if lookup.is_none() {
                let runtime = state
                    .get_project_cached(project)
                    .context("Event source selection is unavailable")?;
                let old = super::project_service::get_active_generation(state, project).await?;
                let view = std::sync::Arc::new(super::project_service::pin_source_generation(
                    state,
                    &runtime.search,
                    project,
                    old,
                )?);
                lookup = Some(
                    runtime
                        .search
                        .selected_source_revision_lookup(project, old, view)?,
                );
            }
            let file = graph.get_node(project, &NodeId::file(source.file_path.as_str()).0)?;
            ensure!(
                super::impact_walk_service::admit_selected_source(
                    lookup.as_ref().unwrap(),
                    &source,
                    file.as_ref()
                )? == engram_graph::store::ImpactAdmission::Selected,
                "Event source control revision is not admitted"
            );
        }
        ensure!(
            source.file_path.as_str() == path
                || engram_graph::store::event_owner_matches(&source, path, owner),
            "Event producer does not match the declared markup backing class"
        );
        let previous = graph.get_edges_by_endpoints(
            project,
            &[(
                edge.source_id.clone(),
                edge.edge_kind.clone(),
                edge.target_id.clone(),
            )],
        )?;
        for old in previous {
            match engram_graph::store::event_producer(&old)? {
                Some((producer, _, class)) => ensure!(
                    (producer == path && class.eq_ignore_ascii_case(owner))
                        || stats.all_files.iter().any(|p| p.as_str() == producer),
                    "Event binding is still supported by another unchanged producer; refresh both producers together"
                ),
                None if old.generation < edge.generation => ensure!(
                    stats.all_files.iter().any(|p| p == &source.file_path),
                    "Legacy event producer is unverified; refresh markup and codebehind together"
                ),
                None => {}
            }
        }
        stamp(edge, path, edge.generation, owner, &source);
    }
    Ok(())
}
