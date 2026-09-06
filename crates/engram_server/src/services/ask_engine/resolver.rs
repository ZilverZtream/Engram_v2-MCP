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
/// Batch 6 (doc 11, live r62 exact_5): a bare identifier that matches BOTH a
/// source symbol (`sym:` node) and a derived artifact of it (`state:` — a
/// session-cached value, a route alias) is not ambiguous — the source
/// symbols alone survive. Only fires when it actually narrows.
pub fn collapse_derived_resolutions(resolved: &mut Vec<super::plan::ResolvedEntity>) {
    let syms = resolved
        .iter()
        .filter(|r| {
            r.node_id
                .as_deref()
                .is_some_and(|id| id.starts_with("sym:"))
        })
        .count();
    if syms > 0 && syms < resolved.len() {
        resolved.retain(|r| {
            r.node_id
                .as_deref()
                .is_some_and(|id| id.starts_with("sym:"))
        });
    }
}

/// Batch 8 (doc 11, live r64 usage_5): the full ORIGINAL-case directory
/// prefix of `path` up to and including the infix `scope` ("Site/…/ts/map"
/// for scope "ts/map") — the engine's include_path_prefixes is ANCHORED, so
/// an infix scope must expand to real prefixes before it can steer.
pub fn scope_full_prefix(path: &str, scope: &str) -> Option<String> {
    let norm = path.replace('\\', "/");
    let low = norm.to_lowercase();
    let s = scope.to_lowercase();
    if low == s || low.starts_with(&format!("{s}/")) {
        return norm.get(..s.len()).map(|p| p.to_string());
    }
    let needle = format!("/{s}/");
    if let Some(i) = low.find(&needle) {
        return norm.get(..i + 1 + s.len()).map(|p| p.to_string());
    }
    if low.ends_with(&format!("/{s}")) {
        return Some(norm);
    }
    None
}

/// Batch 8: expand each infix scope to the distinct real prefixes present in
/// the project's file nodes (cap 8) — APPENDED, so the raw scope still
/// serves the post-retrieval pool filter while the engine gets anchored
/// prefixes it can actually match.
pub fn expand_path_scopes(graph: &GraphStore, project_id: &str, ql: &mut super::plan::Qualifiers) {
    if ql.path_prefixes.is_empty() {
        return;
    }
    let Ok(files) = graph.query_nodes(project_id, Some("file"), None, None, usize::MAX) else {
        return;
    };
    let mut expanded: Vec<String> = Vec::new();
    for scope in ql.path_prefixes.clone() {
        for n in &files {
            if expanded.len() >= 8 {
                break;
            }
            if let Some(p) = scope_full_prefix(n.file_path.as_str(), &scope) {
                if p.to_lowercase() != scope.to_lowercase() && !expanded.contains(&p) {
                    expanded.push(p);
                }
            }
        }
    }
    for p in expanded {
        if !ql.path_prefixes.contains(&p) {
            ql.path_prefixes.push(p);
        }
    }
}

/// "GetByID in the projekt DAL" → the `projekt` candidate only).
/// `NotFound`/`Err` leaves it empty — a text-search-only entity, never a hard
/// failure (the provider layer still searches by the raw text).
pub fn resolve_entities_in_context(
    graph: &GraphStore,
    project_id: &str,
    plan: &mut QueryPlan,
    question: &str,
) {
    let ql = question.to_lowercase();
    let server_cue = ql.contains("server")
        || ql.contains("web method")
        || ql.contains("webmethod")
        || ql.contains("backend")
        || ql.contains("implements")
        || ql.contains(" vb ");
    let is_server_path = |n: &Node| {
        let p = n.file_path.as_str().to_lowercase();
        p.ends_with(".vb") || p.ends_with(".cs") || p.ends_with(".asmx")
    };
    for m in plan.entities.iter_mut() {
        match graph.resolve_symbol(project_id, &m.text, None, None) {
            Ok(ResolveResult::Unique(n)) => {
                // Round-7: a server cue on a CLIENT-only unique resolution —
                // "who calls DeleteImage on the server" resolving to a TS
                // deleteImage — should reach the server definition. Look up a
                // same-terminal server (.vb/.cs/.asmx) function and prefer it.
                if server_cue && !is_server_path(&n) {
                    let srv: Option<Node> = graph
                        .query_nodes_by_symbol_name(project_id, &m.text, None, 50)
                        .ok()
                        .and_then(|nodes| {
                            nodes.into_iter().find(|c| {
                                is_server_path(c)
                                    && matches!(
                                        c.node_type.as_str(),
                                        "function" | "method" | "sub" | "procedure"
                                    )
                            })
                        });
                    match srv {
                        Some(s) => m.resolved = vec![node_to_resolved(&s, 0.8)],
                        None => m.resolved = vec![node_to_resolved(&n, 0.9)],
                    }
                } else {
                    m.resolved = vec![node_to_resolved(&n, 0.9)];
                }
            }
            Ok(ResolveResult::Ambiguous(v)) => {
                // Round-7: a SERVER cue disambiguates a client/server name clash
                // toward the server definition (DeleteImage → the VB
                // api.DeleteImage). Only narrows when it strictly reduces the set.
                let v: Vec<Node> = if server_cue {
                    let srv: Vec<Node> = v.iter().filter(|n| is_server_path(n)).cloned().collect();
                    if !srv.is_empty() && srv.len() < v.len() {
                        srv
                    } else {
                        v
                    }
                } else {
                    v
                };
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
    // Batch 6 (doc 11, live r62 exact_5): source symbols outrank the derived
    // artifacts they spawn — a bare identifier matching both is not ambiguous.
    for m in plan.entities.iter_mut() {
        collapse_derived_resolutions(&mut m.resolved);
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
            // A dispatch-derived resolution is the ROUTE the name is served
            // by, not a competing symbol — kind Route keeps the status
            // calibration from calling the pair ambiguous (live r46).
            let mut r = node_to_resolved(n, conf);
            r.kind = EntityKind::Route;
            m.resolved.push(r);
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
    let stem_names: Vec<String> = stems.iter().map(|(s, _)| s.clone()).collect();
    let picked = compound_join_pick(&words, &stem_names)?;
    let winning_stem = stem_names[picked].as_str();
    // Among the winning stem's files the SOURCE extension wins (a compiled
    // .js twin shares the .ts stem).
    let rank = |n: &Node| -> u8 {
        let p = n.file_path.as_str().to_lowercase();
        if p.ends_with(".designer.vb") {
            4
        } else if p.ends_with(".ts") || p.ends_with(".tsx") || p.ends_with(".vb") {
            0
        } else if p.ends_with(".js") || p.ends_with(".jsx") {
            3
        } else {
            1
        }
    };
    let n = stems
        .iter()
        .filter(|(s, _)| s == winning_stem)
        .map(|(_, n)| *n)
        .min_by_key(|n| rank(n))?;
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

/// Item 8, cycle 31 (live r47): which file stem does a question's compound
/// name pick? Three rules, each earned by a live miss:
/// * THREE words or more — "icon picker" (two incidental words) seized
///   bootstrap-iconpicker.css and displaced ox_multi_2's real evidence;
/// * PREFIX/SUFFIX containment — "mapmarker" sat mid-string in
///   gispdfelementfactoryformapmarkers;
/// * ONE distinct stem — "markerinfowindow" is honestly ambiguous across the
///   five *MarkerInfowindow families (a ts/js twin still counts once).
/// Returns the index of a file with the winning stem.
pub fn compound_join_pick(words: &[&str], stems: &[String]) -> Option<usize> {
    let mut best: Option<(usize, String)> = None; // (join length, stem)
    for chunk in words.windows(3) {
        let join = chunk.concat();
        if join.len() < 8 {
            continue;
        }
        let mut matching: Vec<&str> = stems
            .iter()
            .filter(|s| s.starts_with(&join) || s.ends_with(&join))
            .map(|s| s.as_str())
            .collect();
        matching.sort_unstable();
        matching.dedup();
        if matching.len() == 1 && best.as_ref().is_none_or(|(l, _)| join.len() > *l) {
            best = Some((join.len(), matching[0].to_string()));
        }
    }
    let (_, stem) = best?;
    stems.iter().position(|s| *s == stem)
}

/// Item 8, cycle 32 (owner-approved): "marker info window" is honestly
/// ambiguous across the five *MarkerInfowindow files — one FAMILY. No entity
/// is minted (that would be a wrong guess or an ambiguity status), but a hop
/// from each family member is direct evidence, so the family seeds the
/// callee hop. Returns project-relative paths, source extensions preferred,
/// one per stem; empty when a unique compound exists (the entity path) or
/// when nothing family-shaped matches.
pub fn compound_family_seeds(graph: &GraphStore, project_id: &str, question: &str) -> Vec<String> {
    let words: Vec<&str> = question
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| w.len() >= 3 && w.chars().all(|c| c.is_ascii_lowercase()))
        .collect();
    if words.len() < 3 {
        return Vec::new();
    }
    let Ok(files) = graph.query_nodes(project_id, Some("file"), None, None, usize::MAX) else {
        return Vec::new();
    };
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
    let stem_names: Vec<String> = stems.iter().map(|(s, _)| s.clone()).collect();
    if compound_join_pick(&words, &stem_names).is_some() {
        return Vec::new();
    }
    let mut best: Option<(usize, Vec<String>)> = None;
    for chunk in words.windows(3) {
        let join = chunk.concat();
        if join.len() < 8 {
            continue;
        }
        let mut matching: Vec<String> = stem_names
            .iter()
            .filter(|s| s.starts_with(&join) || s.ends_with(&join))
            .cloned()
            .collect();
        matching.sort_unstable();
        matching.dedup();
        // Live r49: the real family (ata/io/permit/pl/vehicle MarkerInfowindow)
        // has FIVE members — a family is a family whatever its size; only the
        // seed list is capped below.
        if matching.len() >= 2 && best.as_ref().is_none_or(|(l, _)| join.len() > *l) {
            best = Some((join.len(), matching));
        }
    }
    let Some((_, family)) = best else {
        return Vec::new();
    };
    let rank = |n: &Node| -> u8 {
        let p = n.file_path.as_str().to_lowercase();
        if p.ends_with(".designer.vb") {
            4
        } else if p.ends_with(".ts") || p.ends_with(".tsx") || p.ends_with(".vb") {
            0
        } else if p.ends_with(".js") || p.ends_with(".jsx") {
            3
        } else {
            1
        }
    };
    let mut out = Vec::new();
    for stem in &family {
        if let Some(n) = stems
            .iter()
            .filter(|(s, _)| s == stem)
            .map(|(_, n)| *n)
            .min_by_key(|n| rank(n))
        {
            out.push(n.file_path.as_str().replace('\\', "/"));
        }
    }
    out.truncate(4);
    out
}
