//! Graph-backed entity resolver. Turns each surface-form `EntityMention` into
//! zero (search-only), one (unique), or several (ambiguous branch) concrete
//! `ResolvedEntity`s via the graph's `resolve_symbol`. Sync — redb reads are
//! sync; the orchestrator wraps this in `spawn_blocking`.

use engram_graph::{GraphStore, Node, ResolveResult};

use super::plan::{EntityKind, EntityMention, QueryPlan, ResolvedEntity};

/// Cap on candidate branches kept for an ambiguous mention.
const MAX_BRANCHES: usize = 4;

/// Map a graph `node_type` onto our coarse `EntityKind`.
pub fn node_to_entity_kind(node_type: &str) -> EntityKind {
    match node_type {
        "file" => EntityKind::File,
        "function" | "class" | "interface" | "stored_proc" | "inline_sql" => EntityKind::Symbol,
        "db_table" => EntityKind::Table,
        "db_column" => EntityKind::Column,
        "global_state" => EntityKind::Setting,
        "page" | "control" | "ui_container" | "control_layout" => EntityKind::UiControl,
        "route_handler" | "http_handler" | "web_service" | "wcf_service" => EntityKind::Route,
        "insight" => EntityKind::Concept,
        _ => EntityKind::Unknown,
    }
}

fn node_to_resolved(n: &Node, confidence: f32) -> ResolvedEntity {
    ResolvedEntity {
        kind: node_to_entity_kind(&n.node_type),
        canonical: if n.name.is_empty() {
            n.node_id.clone()
        } else {
            n.name.clone()
        },
        node_id: Some(n.node_id.clone()),
        confidence,
    }
}

/// Resolve every entity mention against the graph, filling `resolved`.
/// `NotFound`/`Err` leaves it empty — a text-search-only entity, never a hard
/// failure (the provider layer still searches by the raw text).
pub fn resolve_entities(graph: &GraphStore, project_id: &str, plan: &mut QueryPlan) {
    resolve_entities_in_context(graph, project_id, plan, "");
}

const QUALIFIER_STOPWORDS: [&str; 24] = [
    "what", "would", "break", "where", "which", "when", "does", "stopped", "calling", "defined",
    "used", "from", "with", "that", "this", "into", "file", "files", "code", "still", "change",
    "changed", "happens", "should",
];

/// The question's qualifier words: alphabetic tokens (≥ 4 letters) that are
/// neither stopwords nor the entity mention itself.
fn qualifier_tokens(question: &str, mention: &str) -> Vec<String> {
    let m = mention.to_lowercase();
    question
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| t.chars().filter(|c| c.is_alphabetic()).count() >= 4)
        .filter(|t| *t != m && !QUALIFIER_STOPWORDS.contains(t))
        .map(|t| t.to_string())
        .collect()
}

/// Resolve every entity mention against the graph, filling `resolved`; an
/// AMBIGUOUS name is narrowed by the question's qualifier words when they
/// occur in a candidate's path or qualified name (golden `ox_impact_4`:
/// "GetByID in the projekt DAL" → the `projekt` candidate only).
/// `NotFound`/`Err` leaves it empty — a text-search-only entity, never a hard
/// failure (the provider layer still searches by the raw text).
pub fn resolve_entities_in_context(
    graph: &GraphStore,
    project_id: &str,
    plan: &mut QueryPlan,
    question: &str,
) {
    for m in plan.entities.iter_mut() {
        match graph.resolve_symbol(project_id, &m.text, None, None) {
            Ok(ResolveResult::Unique(n)) => {
                m.resolved = vec![node_to_resolved(&n, 0.9)];
            }
            Ok(ResolveResult::Ambiguous(v)) => {
                let toks = qualifier_tokens(question, &m.text);
                // Match STRENGTH (release 30 live, golden `ox_impact_4`): a qualifier
                // that names a candidate's class or file stem EXACTLY outranks one
                // that merely occurs inside a longer name — `projekt` is the class
                // of `_gd.projekt.GetByID` / `projekt.vb` and only a substring of
                // `installationsobjektprojekt`. The strongest tier alone survives.
                let scored: Vec<(u8, &Node)> = if toks.is_empty() {
                    Vec::new()
                } else {
                    v.iter()
                        .filter_map(|n| {
                            let path = n.file_path.as_str().replace('\\', "/").to_lowercase();
                            let stem = path
                                .rsplit('/')
                                .next()
                                .unwrap_or("")
                                .split('.')
                                .next()
                                .unwrap_or("")
                                .to_string();
                            let segments: Vec<String> = n
                                .name
                                .to_lowercase()
                                .split(|c: char| c == '.' || c == ':')
                                .map(|s| s.to_string())
                                .collect();
                            let hay = format!("{path} {} {}", n.node_id, n.name).to_lowercase();
                            let strength = toks
                                .iter()
                                .map(|t| {
                                    if stem == *t || segments.iter().any(|s| s == t) {
                                        2
                                    } else if hay.contains(t.as_str()) {
                                        1
                                    } else {
                                        0
                                    }
                                })
                                .max()
                                .unwrap_or(0);
                            (strength > 0).then_some((strength, n))
                        })
                        .collect()
                };
                let best = scored.iter().map(|(s, _)| *s).max().unwrap_or(0);
                let narrowed: Vec<&Node> = scored
                    .iter()
                    .filter(|(s, _)| *s == best)
                    .map(|(_, n)| *n)
                    .collect();
                m.resolved = if !narrowed.is_empty() && narrowed.len() < v.len() {
                    let conf = if narrowed.len() == 1 { 0.8 } else { 0.5 };
                    narrowed
                        .iter()
                        .take(MAX_BRANCHES)
                        .map(|n| node_to_resolved(n, conf))
                        .collect()
                } else {
                    v.iter()
                        .take(MAX_BRANCHES)
                        .map(|n| node_to_resolved(n, 0.5))
                        .collect()
                };
            }
            Ok(ResolveResult::NotFound) | Err(_) => {
                // Round-2 audit P0-4: a mention that IS a file stem
                // ("api-installationsobjektprojekt") resolves to that file so
                // the definition arm cites it.
                let stem = m.text.trim().to_lowercase();
                if stem.len() >= 6 && !stem.contains(' ') {
                    let want = stem
                        .rsplit('/')
                        .next()
                        .unwrap_or(&stem)
                        .split('.')
                        .next()
                        .unwrap_or("")
                        .to_string();
                    if let Ok(files) =
                        graph.query_nodes(project_id, Some("file"), None, None, usize::MAX)
                    {
                        let hits: Vec<&Node> = files
                            .iter()
                            .filter(|n| {
                                let p = n.file_path.as_str().replace('\\', "/").to_lowercase();
                                p.rsplit('/')
                                    .next()
                                    .unwrap_or("")
                                    .split('.')
                                    .next()
                                    .unwrap_or("")
                                    == want
                            })
                            .collect();
                        if !hits.is_empty() && hits.len() <= MAX_BRANCHES {
                            m.guessed_kind = EntityKind::File;
                            let conf = if hits.len() == 1 { 0.9 } else { 0.5 };
                            m.resolved = hits.iter().map(|n| node_to_resolved(n, conf)).collect();
                        }
                    }
                }
            }
        }
    }
    // Item 8 (live r44/r45, ox_causal_1): an API NAME literal
    // (`athDeleteByID`) may name a LEGACY client function AND the broker's
    // arm ("which VB function handles it?"). The dispatched implementation is
    // a resolution BRANCH — added whether or not the name bound to a symbol.
    for m in plan.entities.iter_mut() {
        if m.guessed_kind != EntityKind::Symbol || m.text.len() < 4 || m.text.contains(' ') {
            continue;
        }
        let Ok(targets) = graph.find_dispatch_targets(project_id, &m.text) else {
            continue;
        };
        let fresh: Vec<Node> = targets
            .iter()
            .filter(|id| {
                !m.resolved
                    .iter()
                    .any(|r| r.node_id.as_deref() == Some(id.as_str()))
            })
            .filter_map(|id| graph.get_node(project_id, id).ok().flatten())
            .collect();
        if fresh.is_empty() {
            continue;
        }
        let conf = if m.resolved.is_empty() && fresh.len() == 1 {
            0.85
        } else {
            0.8
        };
        for n in &fresh {
            if m.resolved.len() >= MAX_BRANCHES {
                break;
            }
            m.resolved.push(node_to_resolved(n, conf));
        }
    }
    if let Some(m) = compound_file_mention(graph, project_id, question, &plan.entities) {
        plan.entities.push(m);
    }
}

/// Item 8 (golden ox_multi_4): a UI name spoken as WORDS — "marker info
/// window" — names no token the entity scan sees, yet JOINED it is a file
/// stem (`ioMarkerInfowindow.ts`). Take 2–3 word windows of plain lowercase
/// words and keep the longest join that is a substring of exactly ONE file
/// stem; that file becomes a resolved File entity (and thereby a named seed
/// for the callee hop).
fn compound_file_mention(
    graph: &GraphStore,
    project_id: &str,
    question: &str,
    entities: &[EntityMention],
) -> Option<EntityMention> {
    if entities
        .iter()
        .any(|m| m.guessed_kind == EntityKind::File && !m.resolved.is_empty())
    {
        return None;
    }
    let words: Vec<&str> = question
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| w.len() >= 3 && w.chars().all(|c| c.is_ascii_lowercase()))
        .collect();
    if words.len() < 2 {
        return None;
    }
    let files = graph
        .query_nodes(project_id, Some("file"), None, None, usize::MAX)
        .ok()?;
    let stems: Vec<(String, &Node)> = files
        .iter()
        .map(|n| {
            let p = n.file_path.as_str().replace('\\', "/").to_lowercase();
            (
                p.rsplit('/')
                    .next()
                    .unwrap_or("")
                    .split('.')
                    .next()
                    .unwrap_or("")
                    .to_string(),
                n,
            )
        })
        .collect();
    let mut best: Option<(usize, &Node)> = None;
    for win in [3usize, 2] {
        if best.is_some() {
            break;
        }
        for chunk in words.windows(win) {
            let join = chunk.concat();
            if join.len() < 8 {
                continue;
            }
            let hits: Vec<&Node> = stems
                .iter()
                .filter(|(s, _)| s.contains(&join))
                .map(|(_, n)| *n)
                .collect();
            if hits.len() == 1 && best.is_none_or(|(l, _)| join.len() > l) {
                best = Some((join.len(), hits[0]));
            }
        }
    }
    let (_, n) = best?;
    let file_name = n
        .file_path
        .as_str()
        .replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string();
    Some(EntityMention {
        text: file_name,
        guessed_kind: EntityKind::File,
        resolved: vec![node_to_resolved(n, 0.85)],
    })
}
