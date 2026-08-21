//! Typed evidence providers. Each returns `Vec<EvidenceItem>` produced directly
//! from the substrate (search engine, registry, graph) plus a `ProviderOutcome`
//! that distinguishes Hit / Empty / Failed — never `unwrap_or_default`, so a
//! retrieval error can never masquerade as "nothing found".
//!
//! Search-backed providers here (Task 4); graph-backed providers appended in
//! Task 5.

use engram_core::namespaces;
use engram_core::registry::Registry;
use engram_graph::{GraphStore, Node};
use engram_index::{HybridHit, HybridQuery, HybridSearchEngine};
use tokio_util::sync::CancellationToken;

use super::evidence::{Authority, EvidenceItem, EvidenceKind};
use super::status::ProviderStatus;

/// Lightweight per-arm result the retrieval layer folds into a `ProviderReport`.
pub struct ProviderOutcome {
    pub status: ProviderStatus,
    pub note: Option<String>,
}
impl ProviderOutcome {
    pub fn hit() -> Self {
        Self {
            status: ProviderStatus::Hit,
            note: None,
        }
    }
    pub fn empty() -> Self {
        Self {
            status: ProviderStatus::Empty,
            note: None,
        }
    }
    pub fn failed(msg: impl Into<String>) -> Self {
        Self {
            status: ProviderStatus::Failed,
            note: Some(msg.into()),
        }
    }
}

const SNIPPET_CHARS: usize = 1200;

fn base_query(
    project_id: &str,
    namespace: &str,
    generation: u64,
    text: &str,
    top_k: usize,
) -> HybridQuery {
    HybridQuery {
        project_id: project_id.to_string(),
        namespace: namespace.to_string(),
        generation,
        text: text.to_string(),
        top_k,
        fts_mode: "loose".into(),
        include_path_prefixes: None,
        exclude_path_prefixes: None,
        language_filters: None,
        author_filter: None,
        date_after: None,
        date_before: None,
        use_mmr: true,
    }
}

#[allow(clippy::too_many_arguments)]
fn hit_to_evidence(
    h: &HybridHit,
    kind: EvidenceKind,
    authority: Authority,
    provider: &str,
    generation: u64,
    content: String,
    id: &mut usize,
) -> EvidenceItem {
    *id += 1;
    EvidenceItem {
        evidence_id: format!("ev_{id}"),
        kind,
        authority,
        path: Some(h.path.as_str().replace('\\', "/")),
        lines: (h.start_line > 0).then_some((h.start_line, h.end_line)),
        symbol_id: None, // search hits carry no node id
        title: None,
        content: content.chars().take(SNIPPET_CHARS).collect(),
        generation: Some(generation),
        commit: None,
        timestamp: h.timestamp,
        confidence: 0.85,
        relevance: h.score.clamp(0.0, 1.0),
        extraction_method: "fts".into(),
        warnings: vec![],
        provider: provider.into(),
        score: None,
        directness: None,
    }
}

#[allow(clippy::too_many_arguments)]
async fn search_arm(
    search: &HybridSearchEngine,
    project_id: &str,
    namespace: &str,
    generation: u64,
    kind: EvidenceKind,
    authority: Authority,
    provider: &str,
    query: &str,
    top_k: usize,
    cancel: &CancellationToken,
    id: &mut usize,
) -> (Vec<EvidenceItem>, ProviderOutcome) {
    let q = base_query(project_id, namespace, generation, query, top_k);
    match search.search(&q, None, cancel).await {
        Err(e) => (vec![], ProviderOutcome::failed(e.to_string())),
        Ok(hits) if hits.is_empty() => (vec![], ProviderOutcome::empty()),
        Ok(hits) => {
            let mut items = Vec::with_capacity(hits.len());
            for h in &hits {
                let content = search
                    .get_doc_by_pk(&h.pk)
                    .ok()
                    .flatten()
                    .map(|t| t.2)
                    .or_else(|| h.snippet.clone())
                    .unwrap_or_default();
                items.push(hit_to_evidence(
                    h, kind, authority, provider, generation, content, id,
                ));
            }
            (items, ProviderOutcome::hit())
        }
    }
}

/// Source-code evidence from the code (memory) namespace.
pub async fn code_evidence(
    search: &HybridSearchEngine,
    project_id: &str,
    generation: u64,
    query: &str,
    top_k: usize,
    cancel: &CancellationToken,
    id: &mut usize,
) -> (Vec<EvidenceItem>, ProviderOutcome) {
    search_arm(
        search,
        project_id,
        namespaces::NAMESPACE_MEMORY,
        generation,
        EvidenceKind::SourceCode,
        Authority::CurrentCode,
        "code",
        query,
        top_k,
        cancel,
        id,
    )
    .await
}

/// Knowledge-namespace evidence (doc / insight / business_logic / history) with
/// a caller-chosen kind + authority + provider label.
#[allow(clippy::too_many_arguments)]
pub async fn knowledge_evidence(
    search: &HybridSearchEngine,
    project_id: &str,
    generation: u64,
    namespace: &str,
    kind: EvidenceKind,
    authority: Authority,
    provider: &str,
    query: &str,
    top_k: usize,
    cancel: &CancellationToken,
    id: &mut usize,
) -> (Vec<EvidenceItem>, ProviderOutcome) {
    search_arm(
        search, project_id, namespace, generation, kind, authority, provider, query, top_k,
        cancel, id,
    )
    .await
}

/// Memory-bank notes (kind-aware authority) via a registry scan + lexical filter.
pub fn memory_evidence(
    registry: &Registry,
    project_id: &str,
    query: &str,
    top_k: usize,
    id: &mut usize,
) -> (Vec<EvidenceItem>, ProviderOutcome) {
    let sections = match registry.list_memory_sections(project_id) {
        Ok(s) => s,
        Err(e) => return (vec![], ProviderOutcome::failed(e.to_string())),
    };
    let terms: Vec<String> = query
        .to_lowercase()
        .split_whitespace()
        .filter(|s| s.len() >= 3)
        .map(|s| s.to_string())
        .collect();
    let mut scored: Vec<(usize, &engram_core::registry::MemorySection)> = Vec::new();
    for s in &sections {
        let hay = format!("{} {}", s.title, s.content).to_lowercase();
        let hits = terms.iter().filter(|t| hay.contains(t.as_str())).count();
        if hits > 0 {
            scored.push((hits, s));
        }
    }
    if scored.is_empty() {
        return (vec![], ProviderOutcome::empty());
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    let mut items = Vec::new();
    for (_, s) in scored.into_iter().take(top_k) {
        *id += 1;
        let authority = match s.kind.as_deref() {
            Some("decision") | Some("reference") => Authority::ApprovedRequirement,
            _ => Authority::AgentMemory,
        };
        items.push(EvidenceItem {
            evidence_id: format!("ev_{id}"),
            kind: EvidenceKind::MemoryNote,
            authority,
            path: None,
            lines: None,
            symbol_id: None,
            title: Some(s.title.clone()),
            content: s.content.chars().take(SNIPPET_CHARS).collect(),
            generation: None,
            commit: None,
            timestamp: (s.updated_at_ms > 0).then_some(s.updated_at_ms),
            confidence: 0.8,
            relevance: 0.5,
            extraction_method: "memory".into(),
            warnings: vec![],
            provider: "memory".into(),
            score: None,
            directness: None,
        });
    }
    (items, ProviderOutcome::hit())
}

// ── graph-backed providers (Task 5) ──────────────────────────────────────────

fn node_fields(
    node: &Option<Node>,
    fallback_id: &str,
) -> (Option<String>, Option<(u32, u32)>, String, Option<u64>) {
    match node {
        Some(n) => (
            Some(n.file_path.as_str().replace('\\', "/")),
            (n.start_line > 0).then_some((n.start_line, n.end_line)),
            if n.name.is_empty() {
                n.node_id.clone()
            } else {
                n.name.clone()
            },
            Some(n.generation),
        ),
        None => (None, None, fallback_id.to_string(), None),
    }
}

#[allow(clippy::too_many_arguments)]
fn graph_relation_item(
    provider: &str,
    symbol_id: String,
    path: Option<String>,
    lines: Option<(u32, u32)>,
    name: String,
    content: String,
    generation: Option<u64>,
    weight: u32,
    directness: f32,
    authority: Authority,
    id: &mut usize,
) -> EvidenceItem {
    *id += 1;
    EvidenceItem {
        evidence_id: format!("ev_{id}"),
        kind: EvidenceKind::GraphRelation,
        authority,
        path,
        lines,
        symbol_id: Some(symbol_id),
        title: Some(name),
        content,
        generation,
        commit: None,
        timestamp: None,
        confidence: 0.9,
        relevance: (weight as f32 / 10.0).min(1.0),
        extraction_method: "graph".into(),
        warnings: vec![],
        provider: provider.into(),
        score: None,
        directness: Some(directness),
    }
}

/// Reverse-dependency: who points at `target_node_id` (all edge kinds). The
/// impact arm's core — direct, high-authority graph relations.
pub fn impact_evidence(
    graph: &GraphStore,
    project_id: &str,
    target_node_id: &str,
    limit: usize,
    id: &mut usize,
) -> (Vec<EvidenceItem>, ProviderOutcome) {
    let incoming =
        match graph.find_incoming_edges_with_kind(project_id, None, target_node_id, limit.clamp(1, 1000)) {
            Ok(v) => v,
            Err(e) => return (vec![], ProviderOutcome::failed(e.to_string())),
        };
    if incoming.is_empty() {
        return (vec![], ProviderOutcome::empty());
    }
    let mut items = Vec::new();
    for (src_id, kind, weight) in incoming {
        let node = graph.get_node(project_id, &src_id).ok().flatten();
        let (path, lines, name, gen_) = node_fields(&node, &src_id);
        let content = format!("{name} {} the target (weight {weight})", kind.as_str());
        items.push(graph_relation_item(
            "impact",
            src_id,
            path,
            lines,
            name,
            content,
            gen_,
            weight,
            0.9,
            Authority::CurrentCode,
            id,
        ));
    }
    (items, ProviderOutcome::hit())
}

/// Symbol usage: resolve candidate nodes by name, then their incoming references.
pub fn symbol_ref_evidence(
    graph: &GraphStore,
    project_id: &str,
    symbol_name: &str,
    file_scope: Option<&str>,
    max: usize,
    id: &mut usize,
) -> (Vec<EvidenceItem>, ProviderOutcome) {
    let nodes = match graph.query_nodes_by_symbol_name(project_id, symbol_name, file_scope, 50) {
        Ok(n) => n,
        Err(e) => return (vec![], ProviderOutcome::failed(e.to_string())),
    };
    if nodes.is_empty() {
        return (vec![], ProviderOutcome::empty());
    }
    let mut items = Vec::new();
    for node in &nodes {
        let incoming = graph
            .find_incoming_edges_with_kind(project_id, None, &node.node_id, max)
            .unwrap_or_default();
        for (src, kind, weight) in incoming.into_iter().take(max) {
            let src_node = graph.get_node(project_id, &src).ok().flatten();
            let (path, lines, name, gen_) = node_fields(&src_node, &src);
            let content = format!("{name} {} {}", kind.as_str(), node.name);
            items.push(graph_relation_item(
                "usage",
                src,
                path,
                lines,
                name,
                content,
                gen_,
                weight,
                0.85,
                Authority::CurrentCode,
                id,
            ));
        }
    }
    if items.is_empty() {
        (vec![], ProviderOutcome::empty())
    } else {
        (items, ProviderOutcome::hit())
    }
}

/// Concept footprint: nodes whose name matches the concept, grouped by node_type
/// (bounded per group). A lighter, typed take on get_concept_footprint's scan.
pub fn concept_evidence(
    graph: &GraphStore,
    project_id: &str,
    concept: &str,
    cap: usize,
    id: &mut usize,
) -> (Vec<EvidenceItem>, ProviderOutcome) {
    const SCAN: usize = 200_000;
    let nodes = match graph.query_nodes(project_id, None, None, None, SCAN) {
        Ok(n) => n,
        Err(e) => return (vec![], ProviderOutcome::failed(e.to_string())),
    };
    let stems: Vec<String> = concept
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| s.len() >= 3)
        .map(|s| s.to_string())
        .collect();
    if stems.is_empty() {
        return (vec![], ProviderOutcome::empty());
    }
    let mut per_kind: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut items = Vec::new();
    for n in &nodes {
        if n.node_type == "file" {
            continue;
        }
        let name_l = n.name.to_lowercase();
        if !stems.iter().any(|s| name_l.contains(s.as_str())) {
            continue;
        }
        let seen = per_kind.entry(n.node_type.clone()).or_insert(0);
        if *seen >= cap {
            continue;
        }
        *seen += 1;
        let (path, lines, name, gen_) = node_fields(&Some(n.clone()), &n.node_id);
        let content = format!("{} ({}) matches concept '{concept}'", name, n.node_type);
        let mut it = graph_relation_item(
            "concept",
            n.node_id.clone(),
            path,
            lines,
            name,
            content,
            gen_,
            1,
            0.6,
            Authority::CurrentCode,
            id,
        );
        it.kind = EvidenceKind::ConceptGroup;
        items.push(it);
    }
    if items.is_empty() {
        (vec![], ProviderOutcome::empty())
    } else {
        (items, ProviderOutcome::hit())
    }
}

/// Change companions: files that historically co-change with `file_node_id`.
pub fn companion_evidence(
    graph: &GraphStore,
    project_id: &str,
    file_node_id: &str,
    id: &mut usize,
) -> (Vec<EvidenceItem>, ProviderOutcome) {
    let couplings =
        match engram_graph::algorithms::coupling::file_temporal_couplings(graph, project_id, file_node_id, 2, 20) {
            Ok(c) => c,
            Err(e) => return (vec![], ProviderOutcome::failed(e.to_string())),
        };
    if couplings.is_empty() {
        return (vec![], ProviderOutcome::empty());
    }
    let mut items = Vec::new();
    for c in couplings {
        let node = graph.get_node(project_id, &c.neighbor_node_id).ok().flatten();
        let (path, lines, name, gen_) = node_fields(&node, &c.neighbor_node_id);
        let content = format!("co-changes with {file_node_id} ({} times)", c.weight);
        let mut it = graph_relation_item(
            "companion",
            c.neighbor_node_id.clone(),
            path,
            lines,
            name,
            content,
            gen_,
            c.weight,
            0.5,
            Authority::MergedHistory,
            id,
        );
        it.extraction_method = "git".into();
        items.push(it);
    }
    (items, ProviderOutcome::hit())
}
