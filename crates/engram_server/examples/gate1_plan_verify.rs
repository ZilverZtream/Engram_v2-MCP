//! Gate 1 for the doc-17 verify slice (pre-registered, falsifiable).
//!
//! Runs `verify_plan` against each Phase-G LOSING arm's ACTUAL proposed file
//! list on the real OciusX graph, and reports whether the verifier's findings
//! intersect the judge-named defect for that story. The pass bar was fixed
//! before this ran: the primary defect flagged in >= 8/15 stories with <= 5
//! findings per plan.
//!
//! Usage: engram_server --example gate1_plan_verify <graph.redb> <pid> <cases.json>

use std::collections::BTreeSet;

use engram_graph::GraphStore;
use engram_server::services::plan_verify::{ImplementationPlan, PlanFile, verify_plan};
use serde::Deserialize;

#[derive(Deserialize)]
struct Case {
    pr: String,
    losing_arm: String,
    losing_impl: i64,
    judge_notes: String,
    plan_files: Vec<Pf>,
}

#[derive(Deserialize)]
struct Pf {
    path: String,
    #[serde(default)]
    change: String,
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let graph_path = &args[1];
    let pid = &args[2];
    let cases: Vec<Case> = serde_json::from_str(&std::fs::read_to_string(&args[3])?)?;

    let graph = GraphStore::open(std::path::Path::new(graph_path))?;
    let mut hits = 0usize;
    let mut over_budget = 0usize;

    for c in &cases {
        let plan = ImplementationPlan {
            files: c
                .plan_files
                .iter()
                .map(|p| PlanFile {
                    path: p.path.clone(),
                    action: "modify".into(),
                    change: p.change.clone(),
                })
                .collect(),
        };
        let (findings, proof) = verify_plan(&graph, pid, &plan);
        if findings.len() > 5 {
            over_budget += 1;
        }

        // Did any finding name a file the judge flagged as missing/wrong?
        // Ground truth = filenames mentioned in the judge note.
        let note = c.judge_notes.to_lowercase();
        let flagged: BTreeSet<String> = findings
            .iter()
            .flat_map(|f| f.expected.iter().cloned())
            .map(|e| e.rsplit('/').next().unwrap_or(&e).to_lowercase())
            .collect();
        let convention_flagged = findings
            .iter()
            .any(|f| matches!(f.kind, engram_server::services::plan_verify::FindingKind::ConventionViolation));

        // A hit: the judge note mentions a file the verifier flagged, OR the
        // judge cited a permission/auth defect and the verifier raised a
        // convention violation.
        let name_hit = flagged.iter().any(|f| f.len() > 4 && note.contains(f.as_str()));
        let conv_hit = convention_flagged
            && (note.contains("permission")
                || note.contains("authoriz")
                || note.contains("access check")
                || note.contains("gate"));
        let hit = name_hit || conv_hit;
        if hit {
            hits += 1;
        }
        println!(
            "PR {} [{} imp{}] findings={} proof_ok={} hit={} ({}{})  {}",
            c.pr,
            c.losing_arm,
            c.losing_impl,
            findings.len(),
            proof.complete(),
            hit,
            if name_hit { "name " } else { "" },
            if conv_hit { "conv" } else { "" },
            findings
                .iter()
                .map(|f| format!("{:?}:{}", f.kind, f.expected.first().cloned().unwrap_or_default()))
                .collect::<Vec<_>>()
                .join(" | ")
        );
    }

    println!(
        "\nGATE 1: hits={}/{} (bar >=8)  over_budget={}/{} (bar 0)  => {}",
        hits,
        cases.len(),
        over_budget,
        cases.len(),
        if hits >= 8 && over_budget == 0 { "PASS" } else { "FAIL" }
    );
    Ok(())
}
