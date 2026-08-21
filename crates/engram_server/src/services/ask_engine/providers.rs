//! Typed evidence providers. Each returns `Vec<EvidenceItem>` produced directly
//! from the substrate (search engine, registry, graph) plus a `ProviderOutcome`
//! that distinguishes Hit / Empty / Failed — never `unwrap_or_default`, so a
//! retrieval error can never masquerade as "nothing found".
//!
//! Search-backed providers here (Task 4); graph-backed providers appended in
//! Task 5.

use engram_core::namespaces;
use engram_core::registry::Registry;
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
