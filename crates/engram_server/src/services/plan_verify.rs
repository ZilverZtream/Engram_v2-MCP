//! `verify_implementation_plan` v0 (doc 17, owner-approved slice).
//!
//! Phase G measured a Q&A engine's marginal value to an implementing agent at
//! exactly 0.00 over 15 stories: 32 of 42 asks were rated useful, yet the
//! LOSSES came from questions nobody asked — the arm that lost PR 1890 wrote a
//! service method with no permission gate while every sibling handler enforces
//! one, and no ask touched authorization. Answers wait to be requested; a
//! verifier interrogates the PLAN unconditionally.
//!
//! v0 emits two verdict kinds and nothing else — the enriched-dossier history
//! (structural map -0.4, change-pattern -0.27 at n=15) says a chatty advisor
//! anchors agents and costs more than it saves, so the finding budget is hard.
//!
//! Every enumeration carries a [`CoverageProof`]: a companion set that could
//! not be fully enumerated is reported as unproven rather than silently
//! trimmed.

use std::collections::{BTreeSet, HashSet};

use engram_graph::{EdgeKind, GraphStore};

use super::ask_engine::providers::CoverageProof;

/// One file the plan intends to touch. `change` is the agent's own text — the
/// verifier reads it as the plan's claim about that file, never as truth.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PlanFile {
    pub path: String,
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub change: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ImplementationPlan {
    pub files: Vec<PlanFile>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum FindingKind {
    /// The plan touches one member of a set that ships together.
    MissingCompanion,
    /// The plan touches a surface whose siblings all honour a contract the
    /// plan's own text does not satisfy.
    ConventionViolation,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PlanFinding {
    pub kind: FindingKind,
    pub subject: String,
    pub expected: Vec<String>,
    pub rationale: String,
}

/// Findings emitted per plan. The bound is a product decision, not a
/// resource one: a verifier that lists everything is a verifier agents learn
/// to skim.
const MAX_FINDINGS: usize = 5;
/// Companions named per finding.
const MAX_EXPECTED: usize = 6;
/// A co-change edge weaker than this is coincidence, not a companion.
const COUPLING_FLOOR: u32 = 10;
/// Fraction of sibling handlers that must honour a contract for it to bind.
const CONVENTION_RATIO: f32 = 0.6;
/// Index scan bound; cap+1 makes a hit observable.
const FILE_INDEX_CAP: usize = 20_000;

const PERMISSION_TOKENS: [&str; 8] = [
    "checkread",
    "checkwrite",
    "checkifadmin",
    "checkuseraccess",
    "checkaccess",
    "hasprojectaccess",
    "check_pr_id",
    "authorize",
];

fn norm(p: &str) -> String {
    p.replace('\\', "/").to_lowercase()
}

fn dir_of(p: &str) -> &str {
    match p.rfind('/') {
        Some(i) => &p[..i],
        None => "",
    }
}

fn file_of(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// Page-family siblings of a WebForms unit: markup, code-behind, designer.
/// A page is one shippable unit; touching the code-behind alone is the
/// classic half-change.
fn page_family(path: &str) -> Vec<String> {
    let p = norm(path);
    let mut out = Vec::new();
    for markup in [".aspx", ".ascx", ".master"] {
        if p.ends_with(markup) {
            for ext in [".vb", ".cs", ".designer.vb", ".designer.cs"] {
                out.push(format!("{p}{ext}"));
            }
            return out;
        }
        for code in [".vb", ".cs", ".designer.vb", ".designer.cs"] {
            let suffix = format!("{markup}{code}");
            if p.ends_with(&suffix) {
                let base = &p[..p.len() - code.len()];
                out.push(base.to_string());
                for other in [".vb", ".cs", ".designer.vb", ".designer.cs"] {
                    if other != code {
                        out.push(format!("{base}{other}"));
                    }
                }
                return out;
            }
        }
    }
    out
}

/// Localization family: `text.resx`, `text.sv.resx`, `text.de.resx` … one key
/// added in one language is a shipped-broken translation everywhere else.
fn resx_family(path: &str, index: &[String]) -> Vec<String> {
    let p = norm(path);
    if !p.ends_with(".resx") {
        return Vec::new();
    }
    let dir = dir_of(&p);
    let stem = file_of(&p).trim_end_matches(".resx");
    // `text.sv` -> `text`; `text` -> `text`.
    let base = match stem.rfind('.') {
        Some(i) if stem.len() - i <= 6 => &stem[..i],
        _ => stem,
    };
    let prefix = format!("{dir}/{base}");
    index
        .iter()
        .filter(|f| {
            f.ends_with(".resx")
                && **f != p
                && (f.starts_with(&format!("{prefix}.")) || **f == format!("{prefix}.resx"))
        })
        .cloned()
        .collect()
}

/// Files this one has historically shipped with. Weight is the evidence:
/// a single coincidental commit is not a contract.
fn coupled_companions(
    graph: &GraphStore,
    project_id: &str,
    path: &str,
    proof: &mut CoverageProof,
) -> Vec<String> {
    let id = format!("file:{}", path.replace('\\', "/"));
    match graph.neighbors(project_id, EdgeKind::TemporalCoupling, &id, 64) {
        Ok(n) => n
            .into_iter()
            .filter(|(_, w)| *w >= COUPLING_FLOOR)
            .filter_map(|(t, _)| t.strip_prefix("file:").map(norm))
            .collect(),
        Err(_) => {
            proof.graph_errors += 1;
            Vec::new()
        }
    }
}

/// Does this file's own text honour the permission contract?
fn mentions_permission(text: &str) -> bool {
    let t = text.to_lowercase();
    PERMISSION_TOKENS.iter().any(|tok| t.contains(tok))
}

/// Mine the contract the file's OWN siblings keep: of the functions defined
/// here, how many gate on a permission check? Returns (guarded, total).
fn auth_contract(
    graph: &GraphStore,
    project_id: &str,
    path: &str,
    proof: &mut CoverageProof,
) -> (usize, usize) {
    let fns = match graph.query_nodes(project_id, Some("function"), None, Some(&norm(path)), 200) {
        Ok(f) => f,
        Err(_) => {
            proof.graph_errors += 1;
            return (0, 0);
        }
    };
    let total = fns.len();
    let mut guarded = 0usize;
    for f in &fns {
        let Ok(callees) = graph.neighbors(project_id, EdgeKind::Calls, &f.node_id, 200) else {
            proof.graph_errors += 1;
            continue;
        };
        let hit = callees.iter().any(|(t, _)| {
            let name = graph
                .get_node(project_id, t)
                .ok()
                .flatten()
                .map(|n| n.name)
                .unwrap_or_else(|| t.clone());
            mentions_permission(&name)
        });
        if hit {
            guarded += 1;
        }
    }
    (guarded, total)
}

/// Verify a plan against the codebase it intends to change.
///
/// Returns the findings and the proof of what could be enumerated. An
/// incomplete proof means the verification itself is partial — callers must
/// not read "no findings" as "verified".
pub fn verify_plan(
    graph: &GraphStore,
    project_id: &str,
    plan: &ImplementationPlan,
) -> (Vec<PlanFinding>, CoverageProof) {
    let mut proof = CoverageProof {
        policy: format!(
            "coupling_floor={COUPLING_FLOOR} convention_ratio={CONVENTION_RATIO} max_findings={MAX_FINDINGS}"
        ),
        ..Default::default()
    };
    proof.sources_discovered = plan.files.len();

    // The file index: companions can only be named if they EXIST.
    let index: Vec<String> = match graph.query_nodes(project_id, Some("file"), None, None, FILE_INDEX_CAP + 1) {
        Ok(v) => {
            if v.len() > FILE_INDEX_CAP {
                proof.source_cap_hit = true;
            }
            v.into_iter()
                .take(FILE_INDEX_CAP)
                .map(|n| norm(n.file_path.as_str()))
                .collect()
        }
        Err(_) => {
            proof.graph_errors += 1;
            Vec::new()
        }
    };
    let index_set: HashSet<&String> = index.iter().collect();
    let planned: HashSet<String> = plan.files.iter().map(|f| norm(&f.path)).collect();

    let mut companions: Vec<PlanFinding> = Vec::new();
    let mut violations: Vec<PlanFinding> = Vec::new();

    for f in &plan.files {
        let p = norm(&f.path);
        proof.sources_processed += 1;

        // ---- MissingCompanion -------------------------------------------
        let mut expected: BTreeSet<String> = BTreeSet::new();
        for cand in page_family(&p) {
            if index_set.contains(&cand) && !planned.contains(&cand) {
                expected.insert(cand);
            }
        }
        for cand in resx_family(&p, &index) {
            if !planned.contains(&cand) {
                expected.insert(cand);
            }
        }
        for cand in coupled_companions(graph, project_id, &f.path, &mut proof) {
            if !planned.contains(&cand) {
                expected.insert(cand);
            }
        }
        if !expected.is_empty() {
            let all: Vec<String> = expected.into_iter().collect();
            let shown: Vec<String> = all.iter().take(MAX_EXPECTED).cloned().collect();
            companions.push(PlanFinding {
                kind: FindingKind::MissingCompanion,
                subject: f.path.clone(),
                rationale: format!(
                    "{} file(s) ship with {} but are absent from the plan",
                    all.len(),
                    file_of(&p)
                ),
                expected: shown,
            });
        }

        // ---- ConventionViolation ----------------------------------------
        let (guarded, total) = auth_contract(graph, project_id, &f.path, &mut proof);
        if total >= 2 && (guarded as f32) / (total as f32) >= CONVENTION_RATIO && !mentions_permission(&f.change)
        {
            violations.push(PlanFinding {
                kind: FindingKind::ConventionViolation,
                subject: f.path.clone(),
                expected: vec!["a permission/access check before the work".to_string()],
                rationale: format!(
                    "{guarded}/{total} handlers defined in {} gate on a permission check; this change does not",
                    file_of(&p)
                ),
            });
        }
    }

    // Violations first: a missing guard is a defect, a missing companion is
    // an omission.
    let mut findings = violations;
    findings.extend(companions);
    findings.truncate(MAX_FINDINGS);
    (findings, proof)
}
