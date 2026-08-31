//! Typed evidence providers. Each returns `Vec<EvidenceItem>` produced directly
//! from the substrate (search engine, registry, graph) plus a `ProviderOutcome`
//! that distinguishes Hit / Empty / Failed — never `unwrap_or_default`, so a
//! retrieval error can never masquerade as "nothing found".
//!
//! Search-backed providers here (Task 4); graph-backed providers appended in
//! Task 5.

use engram_core::namespaces;
use engram_core::registry::Registry;
use engram_graph::{EdgeKind, GraphStore, Node};
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
        include_path_suffixes: None,
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
    search_with(search, q, kind, authority, provider, generation, cancel, id).await
}

/// Round-2 audit P0-4: source evidence restricted to the paths of a requested
/// modality — the suffix filter is applied INSIDE the index on both legs, so
/// a report/schema/resource file surfaces even when code chunks dominate.
#[allow(clippy::too_many_arguments)]
pub async fn modality_evidence(
    search: &HybridSearchEngine,
    project_id: &str,
    generation: u64,
    suffixes: &[&str],
    provider: &str,
    query: &str,
    top_k: usize,
    cancel: &CancellationToken,
    id: &mut usize,
) -> (Vec<EvidenceItem>, ProviderOutcome) {
    let mut q = base_query(
        project_id,
        namespaces::NAMESPACE_MEMORY,
        generation,
        query,
        top_k,
    );
    q.include_path_suffixes = Some(suffixes.iter().map(|s| s.to_string()).collect());
    search_with(
        search,
        q,
        EvidenceKind::SourceCode,
        Authority::CurrentCode,
        provider,
        generation,
        cancel,
        id,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn search_with(
    search: &HybridSearchEngine,
    q: HybridQuery,
    kind: EvidenceKind,
    authority: Authority,
    provider: &str,
    generation: u64,
    cancel: &CancellationToken,
    id: &mut usize,
) -> (Vec<EvidenceItem>, ProviderOutcome) {
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
        search, project_id, namespace, generation, kind, authority, provider, query, top_k, cancel,
        id,
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
    let incoming = match graph.find_incoming_edges_with_kind(
        project_id,
        None,
        target_node_id,
        limit.clamp(1, 1000),
    ) {
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
        // Propagate a graph read error as Failed — never let it look like "no
        // usages found" (the invariant this module's header states).
        let incoming =
            match graph.find_incoming_edges_with_kind(project_id, None, &node.node_id, max) {
                Ok(v) => v,
                Err(e) => return (vec![], ProviderOutcome::failed(e.to_string())),
            };
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
        // Describe the node itself; do NOT echo the concept/question (that would
        // fool a downstream term-coverage adequacy check).
        let content = format!("{} ({})", name, n.node_type);
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
    let couplings = match engram_graph::algorithms::coupling::file_temporal_couplings(
        graph,
        project_id,
        file_node_id,
        2,
        20,
    ) {
        Ok(c) => c,
        Err(e) => return (vec![], ProviderOutcome::failed(e.to_string())),
    };
    if couplings.is_empty() {
        return (vec![], ProviderOutcome::empty());
    }
    let mut items = Vec::new();
    for c in couplings {
        let node = graph
            .get_node(project_id, &c.neighbor_node_id)
            .ok()
            .flatten();
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

/// The resolved symbol's OWN location (golden `ox_exact_6`: "Which API file
/// exposes ioUpdateBaseTypeInBulk?" resolved the symbol yet no arm returned
/// where it lives). A definition is evidence for any question that names the
/// symbol, independent of the usage intent.
/// Source lines of a definition, bounded (`DEFINITION_BODY_LINES`, 4 KB).
pub const DEFINITION_BODY_LINES: usize = 60;

fn definition_body(dir: &std::path::Path, rel: &str, a: u32, b: u32) -> Option<String> {
    let full = engram_core::safe_join(dir, rel).ok()?;
    if std::fs::metadata(&full).ok()?.len() > 4 * 1024 * 1024 {
        return None;
    }
    let text = std::fs::read_to_string(&full).ok()?;
    let start = (a.max(1) as usize) - 1;
    let end = (b as usize)
        .max(start + 1)
        .min(start + DEFINITION_BODY_LINES);
    let body: Vec<&str> = text
        .lines()
        .skip(start)
        .take(end - start)
        .map(|l| l.trim_end())
        .collect();
    if body.is_empty() {
        return None;
    }
    let mut s = body.join("\n");
    if s.len() > 4000 {
        let mut cut = 4000;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
    Some(s)
}

/// Round-2 audit P0-4c (owner 2026-08-30): ONE bounded call-graph hop from the
/// files the first pass cited. For every function in a seed file, follow
/// `Calls` / `ApiCall` / `SqlCalls` edges and keep the callees whose name or
/// file matches the question's cues (its own words, plus authorization cues
/// when the question asks how something is authorized). "How does a bulk
/// update get authorized" is answered by `CanUserBulkUpdate`, one call away
/// from the API entry point the search arms cite.
#[allow(clippy::too_many_arguments)]
pub fn callee_evidence(
    graph: &GraphStore,
    project_dir: Option<&std::path::Path>,
    project_id: &str,
    seed_paths: &[String],
    named_files: &[String],
    question: &str,
    max_items: usize,
    id: &mut usize,
) -> Vec<EvidenceItem> {
    const STOP: &[&str] = &[
        "does", "from", "with", "that", "this", "what", "which", "where", "when", "then", "than",
        "into", "onto", "about", "would", "could", "should", "there", "their", "they", "have",
        "been", "being", "were", "will", "your", "through", "point", "entry", "gets", "get",
    ];
    let lower = question.to_lowercase();
    let mut cues: Vec<String> = lower
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .filter(|t| t.len() >= 4 && !STOP.contains(t))
        .map(|t| t.chars().take(6).collect::<String>())
        .collect();
    if [
        "authori",
        "permission",
        "permit",
        "allowed",
        "secure",
        "protect",
        "role",
    ]
    .iter()
    .any(|c| lower.contains(c))
    {
        cues.extend(
            [
                "canuser", "check", "permis", "auth", "allow", "role", "access", "guard",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
    }
    cues.sort();
    cues.dedup();
    let mut out = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    // P0-4d precision guard: a callee that lives in a file the first pass
    // already cited adds nothing but crowding (live r39: seven same-file
    // callees of one .vb pushed the history and schema evidence out of the
    // cap); one callee per defining file; the cue must match the NAME.
    let seed_set: std::collections::HashSet<String> = seed_paths
        .iter()
        .map(|s| s.replace('\\', "/").to_lowercase())
        .collect();
    let mut files_cited: std::collections::HashSet<String> = std::collections::HashSet::new();
    // A hop from a file the question NAMES is the answer itself: three direct
    // route targets must not starve the wrapper continuation (live r46).
    let cap = if named_files.is_empty() {
        max_items
    } else {
        max_items.max(6)
    };
    // Live r50: the family hop's six slots filled with direct route targets
    // before the wrapper continuation could emit api-images.vb — the question
    // said "images" and nothing listened. The cap's last two slots belong to
    // CUE-HIT items (callee name or file BASENAME carrying a question word).
    let mut non_cue = 0usize;
    for seed in seed_paths.iter().take(4) {
        let seed_norm = seed.replace('\\', "/");
        // Item 8: a hop from the file the question NAMES ("which server API
        // functions does orderPanel.ts call?") is the answer itself — no cue
        // gate, direct evidence, and the file node's own edges count too.
        let seed_l = seed_norm.to_lowercase();
        let is_named = named_files.iter().any(|n| {
            let n = n.replace('\\', "/").to_lowercase();
            seed_l.ends_with(&n)
                || seed_l.rsplit('/').next().is_some_and(|f| {
                    f == n || f.rsplit_once('.').is_some_and(|(stem, _)| stem == n)
                })
        });
        let fns = match graph.query_nodes(project_id, Some("function"), None, Some(&seed_norm), 200)
        {
            Ok(v) => v,
            Err(_) => continue,
        };
        let mut sources: Vec<(String, String)> = fns
            .iter()
            .take(if is_named { 200 } else { 60 })
            .map(|f| (f.node_id.clone(), f.name.clone()))
            .collect();
        if is_named {
            let stem = seed_norm
                .rsplit('/')
                .next()
                .unwrap_or(&seed_norm)
                .to_string();
            sources.push((format!("file:{seed_norm}"), stem));
        }
        let kinds: [EdgeKind; 3] = if is_named {
            [EdgeKind::ApiCall, EdgeKind::SqlCalls, EdgeKind::Calls]
        } else {
            [EdgeKind::Calls, EdgeKind::ApiCall, EdgeKind::SqlCalls]
        };
        for (src_id, src_name) in &sources {
            for kind in kinds.iter().cloned() {
                let Ok(nbrs) = graph.neighbors(project_id, kind.clone(), src_id, 40) else {
                    continue;
                };
                for (target, weight) in nbrs {
                    if !seen.insert(target.clone()) {
                        continue;
                    }
                    let Ok(Some(n)) = graph.get_node(project_id, &target) else {
                        continue;
                    };
                    let name_l = n.name.to_lowercase();
                    let path_l = n.file_path.as_str().replace('\\', "/").to_lowercase();
                    let basename = path_l.rsplit('/').next().unwrap_or("");
                    let cue_hit = cues
                        .iter()
                        .any(|c| name_l.contains(c.as_str()) || basename.contains(c.as_str()));
                    let hit = is_named || cue_hit;
                    if !hit || seed_set.contains(&path_l) {
                        continue;
                    }
                    // A non-cue direct callee may not take one of the reserved
                    // slots — but its WRAPPER continuation below still runs
                    // (that is where the served implementation lives).
                    let direct_suppressed = !cue_hit && non_cue >= cap.saturating_sub(2);
                    if !direct_suppressed && !files_cited.insert(path_l.clone()) {
                        continue;
                    }
                    let some = Some(n.clone());
                    let (path, lines, name, gen_) = node_fields(&some, &target);
                    let mut content = format!(
                        "{} calls {name} ({}) — defined in {}{}",
                        src_name,
                        n.node_type,
                        path.clone().unwrap_or_default(),
                        lines
                            .map(|(a, b)| format!(" lines {a}-{b}"))
                            .unwrap_or_default()
                    );
                    if let (Some(dir), Some(p), Some((a, b))) =
                        (project_dir, path.as_deref(), lines)
                        && let Some(body) = definition_body(dir, p, a, b)
                    {
                        content.push('\n');
                        content.push_str(&body);
                    }
                    let wrapper_name = name.clone();
                    if !direct_suppressed {
                        if !cue_hit {
                            non_cue += 1;
                        }
                        out.push(graph_relation_item(
                            "callee",
                            target.clone(),
                            path,
                            lines,
                            name,
                            content,
                            gen_,
                            weight.max(8),
                            if is_named { 0.85 } else { 0.6 },
                            Authority::CurrentCode,
                            id,
                        ));
                        if out.len() >= cap {
                            return out;
                        }
                    }
                    // Item 8 (golden ox_multi_4): a script callee that is
                    // itself an API WRAPPER — `api.ajax().getImage` holds the
                    // route edge to `/api.asmx/getimg` — carries the call one
                    // hop further to the served implementation.
                    if matches!(kind, EdgeKind::Calls)
                        && (path_l.ends_with(".ts") || path_l.ends_with(".js"))
                        && let Ok(wnbrs) =
                            graph.neighbors(project_id, EdgeKind::ApiCall, &target, 10)
                    {
                        for (impl_id, w2) in wnbrs {
                            if !seen.insert(impl_id.clone()) {
                                continue;
                            }
                            let Ok(Some(w)) = graph.get_node(project_id, &impl_id) else {
                                continue;
                            };
                            if !matches!(w.node_type.as_str(), "function" | "method" | "sub") {
                                continue;
                            }
                            let wpath = w.file_path.as_str().replace('\\', "/").to_lowercase();
                            if seed_set.contains(&wpath) || !files_cited.insert(wpath.clone()) {
                                continue;
                            }
                            let wsome = Some(w.clone());
                            let (wp, wl, wname, wgen) = node_fields(&wsome, &impl_id);
                            let mut wcontent = format!(
                                "{} calls {} through the {} wrapper — served by {}{}",
                                src_name,
                                wname,
                                wrapper_name,
                                wp.clone().unwrap_or_default(),
                                wl.map(|(a, b)| format!(" lines {a}-{b}"))
                                    .unwrap_or_default()
                            );
                            if let (Some(dir), Some(p2), Some((a, b))) =
                                (project_dir, wp.as_deref(), wl)
                                && let Some(body) = definition_body(dir, p2, a, b)
                            {
                                wcontent.push('\n');
                                wcontent.push_str(&body);
                            }
                            let wbase = wpath.rsplit('/').next().unwrap_or("");
                            let wcue = cues.iter().any(|c| {
                                wname.to_lowercase().contains(c.as_str())
                                    || wbase.contains(c.as_str())
                            });
                            if !wcue {
                                if non_cue >= cap.saturating_sub(2) {
                                    continue;
                                }
                                non_cue += 1;
                            }
                            out.push(graph_relation_item(
                                "callee",
                                impl_id.clone(),
                                wp,
                                wl,
                                wname,
                                wcontent,
                                wgen,
                                w2.max(8),
                                if is_named { 0.85 } else { 0.6 },
                                Authority::CurrentCode,
                                id,
                            ));
                            if out.len() >= cap {
                                return out;
                            }
                        }
                    }
                }
            }
        }
    }
    out
}

pub fn definition_evidence(
    graph: &GraphStore,
    project_dir: Option<&std::path::Path>,
    project_id: &str,
    node_id: &str,
    id: &mut usize,
) -> (Vec<EvidenceItem>, ProviderOutcome) {
    let node = match graph.get_node(project_id, node_id) {
        Ok(n) => n,
        Err(e) => return (vec![], ProviderOutcome::failed(e.to_string())),
    };
    let Some(n) = node else {
        return (vec![], ProviderOutcome::empty());
    };
    let some = Some(n.clone());
    let (path, lines, name, gen_) = node_fields(&some, node_id);
    // Round-2 audit P0-4: a FILE entity (a mention that is a file stem) has no
    // symbol span — cite its head so the file itself is evidence.
    let lines = if n.node_type == "file" && lines.is_none() {
        Some((1, 30))
    } else {
        lines
    };
    let content = format!(
        "{name} ({}) is defined in {}{}",
        n.node_type,
        path.clone().unwrap_or_default(),
        lines
            .map(|(a, b)| format!(" lines {a}-{b}"))
            .unwrap_or_default()
    );
    // Row 6 (release 32 live, golden `ox_impact_4`): the definition's own
    // source lines ARE the evidence a question about its body needs (the
    // callee it names, the guard it performs); "is defined in … lines a-b"
    // alone left every named term uncovered and the answer Unsupported.
    let content = match (project_dir, path.as_deref(), lines) {
        (Some(dir), Some(p), Some((a, b))) => match definition_body(dir, p, a, b) {
            Some(body) => format!("{content}\n{body}"),
            None => content,
        },
        _ => content,
    };
    let item = graph_relation_item(
        "definition",
        node_id.to_string(),
        path,
        lines,
        name,
        content,
        gen_,
        1,
        1.0,
        Authority::CurrentCode,
        id,
    );
    (vec![item], ProviderOutcome::hit())
}
