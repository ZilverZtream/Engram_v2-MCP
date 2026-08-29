//! UI Family Catalog — external audit 2026-08-29 row 5, M1 slice 1 (owner
//! decision 2026-08-29 09:32: build `get_ui_conformance` M1 + M2 and re-A/B).
//!
//! Layer 1 of the design spec (docs/superpowers/specs/2026-08-17): families
//! are clusters of sibling UI instances that are "the same kind of thing
//! here". This slice derives them from what the index ALREADY stores — the
//! `control_layout` / `ui_container` nodes with `container_type`,
//! `layout_style` and `css_class` metadata — and types every axis by the
//! family's actual consistency with evidence counts. Nothing is a default:
//! a value is canonical because most instances carry it, and the deviations
//! are listed. Later slices add markup/CSS mining, persistence and the
//! `get_ui_conformance(region)` pull/check.

use std::collections::{BTreeMap, BTreeSet};

use engram_graph::GraphStore;

/// How uniform an axis is across a family's instances.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Consistency {
    /// Every instance carries the canonical value → conform (hard rule).
    Consistent,
    /// The modal value is canonical, deviations are listed → prescribe.
    Chaotic,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct UiAxis {
    /// `structure` (container type + layout) or `style.classes` (class SET).
    pub axis: &'static str,
    pub consistency: Consistency,
    /// The value most instances carry (class sets rendered space-joined, sorted).
    pub canonical: String,
    /// Every other value observed, most frequent first.
    pub alternatives: Vec<String>,
    /// Instances carrying the canonical value.
    pub evidence_count: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct UiExemplar {
    pub path: String,
    pub node_id: String,
    pub line: u32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct UiFamily {
    pub family_id: String,
    pub family_name: String,
    pub instances: usize,
    pub derived_at_generation: u64,
    pub exemplar: UiExemplar,
    pub axes: Vec<UiAxis>,
    /// Every instance's file (relative path) — for region matching.
    pub instance_paths: Vec<String>,
}

/// Node types the extractor emits for UI containers.
const CONTAINER_TYPES: [&str; 2] = ["control_layout", "ui_container"];

fn meta_str(n: &engram_graph::Node, key: &str) -> String {
    n.metadata
        .as_ref()
        .and_then(|m| m.get(key))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// A class list compared as a SET (`btn btn-primary` == `btn-primary btn`).
fn class_set(css: &str) -> BTreeSet<String> {
    css.split_whitespace()
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn set_key(set: &BTreeSet<String>) -> String {
    set.iter().cloned().collect::<Vec<_>>().join(" ")
}

/// Cluster the project's UI container nodes into families with at least
/// `min_instances` members and derive their contracts.
pub fn build_families(
    graph: &GraphStore,
    project_id: &str,
    min_instances: usize,
) -> anyhow::Result<Vec<UiFamily>> {
    let mut nodes: Vec<engram_graph::Node> = Vec::new();
    for t in CONTAINER_TYPES {
        nodes.extend(graph.query_nodes(project_id, Some(t), None, None, 50_000)?);
    }
    // Cluster key: what kind of container it is and how it lays out.
    let mut clusters: BTreeMap<(String, String), Vec<&engram_graph::Node>> = BTreeMap::new();
    for n in &nodes {
        let ct = meta_str(n, "container_type");
        if ct.is_empty() {
            continue;
        }
        let ls = meta_str(n, "layout_style");
        clusters.entry((ct, ls)).or_default().push(n);
    }

    let mut out = Vec::new();
    for ((ct, ls), members) in clusters {
        if members.len() < min_instances.max(2) {
            continue;
        }
        let instances = members.len();
        let generation = members.iter().map(|n| n.generation).max().unwrap_or(0);

        // structure axis: the cluster key itself — consistent by construction,
        // reported with its evidence so the reader sees the count, not a claim.
        let structure = UiAxis {
            axis: "structure",
            consistency: Consistency::Consistent,
            canonical: if ls.is_empty() {
                ct.clone()
            } else {
                format!("{ct} / {ls}")
            },
            alternatives: Vec::new(),
            evidence_count: instances,
        };

        // style.classes axis: the class SET, modal value canonical.
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut sets: Vec<(BTreeSet<String>, &engram_graph::Node)> = Vec::new();
        for n in &members {
            let set = class_set(&meta_str(n, "css_class"));
            *counts.entry(set_key(&set)).or_default() += 1;
            sets.push((set, n));
        }
        let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let (canonical_key, canonical_count) = ranked
            .first()
            .cloned()
            .unwrap_or_else(|| (String::new(), 0));
        let alternatives: Vec<String> = ranked
            .iter()
            .skip(1)
            .map(|(k, c)| format!("{k} ({c})"))
            .collect();
        let classes = UiAxis {
            axis: "style.classes",
            consistency: if canonical_count == instances {
                Consistency::Consistent
            } else {
                Consistency::Chaotic
            },
            canonical: canonical_key.clone(),
            alternatives,
            evidence_count: canonical_count,
        };

        // Exemplar: the first instance (by path, line) carrying the canonical set.
        let mut carriers: Vec<&engram_graph::Node> = sets
            .iter()
            .filter(|(s, _)| set_key(s) == canonical_key)
            .map(|(_, n)| *n)
            .collect();
        carriers.sort_by(|a, b| {
            a.file_path
                .as_str()
                .cmp(b.file_path.as_str())
                .then(a.start_line.cmp(&b.start_line))
        });
        let ex = carriers.first().copied().unwrap_or(members[0]);

        let family_name = if canonical_key.is_empty() {
            format!("{ct} ({ls})")
        } else {
            format!("{ct} ({ls}) .{}", canonical_key.replace(' ', "."))
        };
        let family_id = format!(
            "ui:{}:{}:{}",
            ct.to_ascii_lowercase(),
            ls.to_ascii_lowercase(),
            canonical_key.replace(' ', "+")
        );
        out.push(UiFamily {
            family_id,
            family_name,
            instances,
            derived_at_generation: generation,
            exemplar: UiExemplar {
                path: ex.file_path.as_str().to_string(),
                node_id: ex.node_id.clone(),
                line: ex.start_line,
            },
            axes: vec![structure, classes],
            instance_paths: members
                .iter()
                .map(|n| n.file_path.as_str().to_string())
                .collect(),
        });
    }
    out.sort_by(|a, b| {
        b.instances
            .cmp(&a.instances)
            .then_with(|| a.family_id.cmp(&b.family_id))
    });
    Ok(out)
}

/// Markdown rendering of a catalog (used by the M2 tool and by tests).
pub fn render_families(families: &[UiFamily], skipped_singletons: usize) -> String {
    let mut s = format!(
        "# UI family catalog — {} famil{} (instances ≥ 2; {} singleton container(s) skipped)\n\n",
        families.len(),
        if families.len() == 1 { "y" } else { "ies" },
        skipped_singletons
    );
    for f in families {
        s.push_str(&format!(
            "## {} — {} instance(s), derived at generation {}\nexemplar: {}:{} ({})\n",
            f.family_name,
            f.instances,
            f.derived_at_generation,
            f.exemplar.path,
            f.exemplar.line,
            f.exemplar.node_id
        ));
        for a in &f.axes {
            s.push_str(&format!(
                "- {}: {:?} — canonical `{}` ({} of {}){}\n",
                a.axis,
                a.consistency,
                a.canonical,
                a.evidence_count,
                f.instances,
                if a.alternatives.is_empty() {
                    String::new()
                } else {
                    format!("; alternatives: {}", a.alternatives.join(", "))
                }
            ));
        }
        s.push('\n');
    }
    s
}

// ── M2: get_ui_conformance(region) — pull the contract, check a candidate ────

/// Region → match: an exact file, a directory prefix (trailing `/`), or a
/// glob (`*` within a segment, `**` across segments, `?` one character).
fn region_matches(region: &str, path: &str) -> bool {
    let r = region.trim().replace('\\', "/");
    if r.is_empty() {
        return false;
    }
    if r.contains('*') || r.contains('?') {
        let pat = regex::escape(&r)
            .replace("\\*\\*/", "(?:.*/)?")
            .replace("\\*\\*", ".*")
            .replace("\\*", "[^/]*")
            .replace("\\?", "[^/]");
        return regex::Regex::new(&format!("^{pat}$"))
            .map(|re| re.is_match(path))
            .unwrap_or(false);
    }
    if r.ends_with('/') {
        return path.starts_with(&r);
    }
    path == r || path.starts_with(&format!("{r}/"))
}

/// Pull: the families with at least one instance inside `region`, reported
/// whole (the contract is the family's, not the region's).
pub fn families_for_region(
    graph: &GraphStore,
    project_id: &str,
    region: &str,
    min_instances: usize,
) -> anyhow::Result<Vec<UiFamily>> {
    let all = build_families(graph, project_id, min_instances)?;
    Ok(all
        .into_iter()
        .filter(|f| f.instance_paths.iter().any(|p| region_matches(region, p)))
        .collect())
}

/// One axis of a check: ok or a named deviation with the expected value.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AxisVerdict {
    pub axis: &'static str,
    pub ok: bool,
    pub expected: String,
    pub found: String,
    pub detail: String,
}

fn join_set(v: Vec<&String>) -> String {
    v.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" ")
}

/// Check: a candidate's class list against the family's contract — every
/// axis at once. Class lists compare as SETS; the structure axis cannot be
/// judged from a class list and says so instead of pretending.
pub fn check_classes(family: &UiFamily, css_class: &str) -> Vec<AxisVerdict> {
    let found = class_set(css_class);
    let mut out = Vec::new();
    for a in &family.axes {
        if a.axis == "style.classes" {
            let canon = class_set(&a.canonical);
            let extra: Vec<&String> = found.difference(&canon).collect();
            let missing: Vec<&String> = canon.difference(&found).collect();
            let ok = extra.is_empty() && missing.is_empty();
            let detail = if ok {
                format!(
                    "class set matches the canonical set ({} of {} instances carry it)",
                    a.evidence_count, family.instances
                )
            } else {
                format!(
                    "extra: [{}]; missing: [{}]",
                    join_set(extra),
                    join_set(missing)
                )
            };
            out.push(AxisVerdict {
                axis: a.axis,
                ok,
                expected: a.canonical.clone(),
                found: set_key(&found),
                detail,
            });
        } else {
            out.push(AxisVerdict {
                axis: a.axis,
                ok: true,
                expected: a.canonical.clone(),
                found: String::new(),
                detail: "not checkable from a class list — compare against the exemplar's markup"
                    .into(),
            });
        }
    }
    out
}

/// The pull/check report: each family's contract (exemplar + typed axes with
/// evidence), then — when a candidate was given — ✓/✗ per axis.
pub fn render_conformance(families: &[UiFamily], verdicts: Option<&[AxisVerdict]>) -> String {
    let mut s = format!(
        "# UI conformance — {} famil{} in the region\n\n",
        families.len(),
        if families.len() == 1 { "y" } else { "ies" }
    );
    for f in families {
        s.push_str(&format!(
            "## {} — {} instance(s), derived at generation {}\nexemplar: {}:{} ({})\n",
            f.family_name,
            f.instances,
            f.derived_at_generation,
            f.exemplar.path,
            f.exemplar.line,
            f.exemplar.node_id
        ));
        for a in &f.axes {
            s.push_str(&format!(
                "- {}: {:?} — canonical `{}` ({} of {}){}\n",
                a.axis,
                a.consistency,
                a.canonical,
                a.evidence_count,
                f.instances,
                if a.alternatives.is_empty() {
                    String::new()
                } else {
                    format!("; alternatives: {}", a.alternatives.join(", "))
                }
            ));
        }
        s.push('\n');
    }
    if let Some(vs) = verdicts {
        let fam = families
            .first()
            .map(|f| f.family_name.as_str())
            .unwrap_or("");
        let bad = vs.iter().filter(|v| !v.ok).count();
        s.push_str(&format!(
            "## Check against `{fam}` — {}\n",
            if bad == 0 {
                "conforms".to_string()
            } else {
                format!("{bad} deviation(s)")
            }
        ));
        for v in vs {
            s.push_str(&format!(
                "- {} {}: expected `{}`{} — {}\n",
                if v.ok { "✓" } else { "✗" },
                v.axis,
                v.expected,
                if v.found.is_empty() {
                    String::new()
                } else {
                    format!(", found `{}`", v.found)
                },
                v.detail
            ));
        }
    }
    s
}
