#![allow(clippy::unwrap_used)]
//! Doc-17 slice: `verify_implementation_plan` v0 — two verdict kinds.
//!
//! The Phase-G result (impl delta 0.00 across 15 stories) said a Q&A engine
//! does not change implementations: 32/42 asks were "useful" yet the LOSSES
//! came from questions nobody asked (PR 1890 dropped a permission gate the
//! merged service enforces; no ask touched authorization). A verifier asks
//! those questions unconditionally, of the PLAN.
//!
//! v0 scope: MissingCompanion (page family, resx language family, co-change
//! companions) and ConventionViolation (a mined auth contract over sibling
//! handlers). Every finding carries a CoverageProof; nothing is claimed
//! complete that cannot be proven.

use engram_core::RelPath;
use engram_graph::{Edge, EdgeKind, GraphStore, Node};
use engram_server::services::plan_verify::{
    FindingKind, ImplementationPlan, PlanFile, verify_plan,
};

fn store() -> (tempfile::TempDir, GraphStore) {
    let dir = tempfile::tempdir().unwrap();
    let s = GraphStore::open(&dir.path().join("g.redb")).unwrap();
    (dir, s)
}

fn file_node(path: &str) -> Node {
    Node {
        node_id: format!("file:{path}"),
        node_type: "file".to_string(),
        name: path.rsplit('/').next().unwrap_or(path).to_string(),
        namespace: String::new(),
        language: String::new(),
        file_path: RelPath::new(path),
        start_line: 0,
        end_line: 0,
        generation: 1,
        metadata: None,
    }
}

fn fn_node(name: &str, path: &str, line: u32) -> Node {
    Node {
        node_id: format!("sym:function:{path}:{name}:{line}"),
        node_type: "function".to_string(),
        name: name.to_string(),
        namespace: String::new(),
        language: "vb".to_string(),
        file_path: RelPath::new(path),
        start_line: line,
        end_line: line + 20,
        generation: 1,
        metadata: None,
    }
}

fn edge(kind: EdgeKind, src: &str, dst: &str, weight: u32) -> Edge {
    Edge {
        source_id: src.to_string(),
        target_id: dst.to_string(),
        namespace: String::new(),
        language: String::new(),
        edge_kind: kind,
        weight,
        generation: 1,
        metadata: None,
        updated_at_ms: 0,
    }
}

fn plan(files: &[(&str, &str)]) -> ImplementationPlan {
    ImplementationPlan {
        files: files
            .iter()
            .map(|(p, c)| PlanFile {
                path: p.to_string(),
                action: "modify".to_string(),
                change: c.to_string(),
            })
            .collect(),
    }
}

#[test]
fn a_plan_touching_only_the_code_behind_is_missing_its_page_family() {
    let (_d, s) = store();
    let pid = "p1";
    let nodes = vec![
        file_node("Site/pages/marker_edit.aspx"),
        file_node("Site/pages/marker_edit.aspx.vb"),
        file_node("Site/pages/marker_edit.aspx.designer.vb"),
    ];
    s.upsert_nodes_and_edges(pid, &nodes, &[]).unwrap();

    let (findings, proof) = verify_plan(
        &s,
        pid,
        &plan(&[("Site/pages/marker_edit.aspx.vb", "add a button handler")]),
    );
    let missing: Vec<&str> = findings
        .iter()
        .filter(|f| f.kind == FindingKind::MissingCompanion)
        .flat_map(|f| f.expected.iter().map(|e| e.as_str()))
        .collect();
    assert!(
        missing.iter().any(|m| m.ends_with("marker_edit.aspx")),
        "the page markup is a companion of its code-behind: {missing:?}"
    );
    assert!(
        proof.complete(),
        "the family enumeration is provable: {proof:?}"
    );
}

#[test]
fn a_plan_touching_one_resx_language_is_missing_the_family() {
    let (_d, s) = store();
    let pid = "p2";
    let nodes = vec![
        file_node("Site/App_GlobalResources/text.resx"),
        file_node("Site/App_GlobalResources/text.sv.resx"),
        file_node("Site/App_GlobalResources/text.de.resx"),
        file_node("Site/App_GlobalResources/text.nb.resx"),
    ];
    s.upsert_nodes_and_edges(pid, &nodes, &[]).unwrap();

    let (findings, _p) = verify_plan(
        &s,
        pid,
        &plan(&[("Site/App_GlobalResources/text.sv.resx", "add key")]),
    );
    let expected: Vec<&str> = findings
        .iter()
        .filter(|f| f.kind == FindingKind::MissingCompanion)
        .flat_map(|f| f.expected.iter().map(|e| e.as_str()))
        .collect();
    for want in ["text.de.resx", "text.nb.resx", "text.resx"] {
        assert!(
            expected.iter().any(|e| e.ends_with(want)),
            "the whole language family ships together — missing {want}: {expected:?}"
        );
    }
}

#[test]
fn a_strongly_coupled_companion_is_flagged_but_a_weak_one_is_not() {
    let (_d, s) = store();
    let pid = "p3";
    let nodes = vec![
        file_node("Site/ts/qty/qtyManager.ts"),
        file_node("Site/~.js/roqQtyManager.js"),
        file_node("Site/unrelated/other.vb"),
    ];
    let edges = vec![
        edge(
            EdgeKind::TemporalCoupling,
            "file:Site/ts/qty/qtyManager.ts",
            "file:Site/~.js/roqQtyManager.js",
            40,
        ),
        edge(
            EdgeKind::TemporalCoupling,
            "file:Site/ts/qty/qtyManager.ts",
            "file:Site/unrelated/other.vb",
            1,
        ),
    ];
    s.upsert_nodes_and_edges(pid, &nodes, &edges).unwrap();

    let (findings, _p) = verify_plan(
        &s,
        pid,
        &plan(&[("Site/ts/qty/qtyManager.ts", "edit the manager")]),
    );
    let expected: Vec<&str> = findings
        .iter()
        .filter(|f| f.kind == FindingKind::MissingCompanion)
        .flat_map(|f| f.expected.iter().map(|e| e.as_str()))
        .collect();
    assert!(
        expected.iter().any(|e| e.ends_with("roqqtymanager.js")),
        "the compiled bundle co-changes at weight 40: {expected:?}"
    );
    assert!(
        !expected.iter().any(|e| e.ends_with("other.vb")),
        "a weight-1 coincidence is noise, not a companion: {expected:?}"
    );
}

#[test]
fn a_handler_that_skips_the_files_own_permission_convention_is_flagged() {
    // PR 1890's actual loss: the proposal's new service method carried no
    // permission/project-access gate although every sibling enforces one.
    let (_d, s) = store();
    let pid = "p4";
    let path = "Site/App_Code/io/api-json/api-io.vb";
    let mut nodes = vec![
        file_node(path),
        fn_node("CheckRead", "Site/App_Code/sec/useraccess.vb", 10),
    ];
    let mut edges = Vec::new();
    for (i, name) in ["ioGetA", "ioGetB", "ioGetC", "ioGetD"].iter().enumerate() {
        let n = fn_node(name, path, (i as u32 + 1) * 100);
        edges.push(edge(
            EdgeKind::Calls,
            &n.node_id,
            "sym:function:Site/App_Code/sec/useraccess.vb:CheckRead:10",
            1,
        ));
        nodes.push(n);
    }
    s.upsert_nodes_and_edges(pid, &nodes, &edges).unwrap();

    let (findings, _p) = verify_plan(
        &s,
        pid,
        &plan(&[(
            path,
            "Public Function ioGetE(qry) As JSONreturn\n  Return db.Query(...)\nEnd Function",
        )]),
    );
    let conv: Vec<&_> = findings
        .iter()
        .filter(|f| f.kind == FindingKind::ConventionViolation)
        .collect();
    assert_eq!(
        conv.len(),
        1,
        "one violation for the unguarded handler: {findings:?}"
    );
    assert!(
        conv[0].rationale.contains("4/4") || conv[0].rationale.contains("4 of 4"),
        "the contract states its evidence counts: {}",
        conv[0].rationale
    );
}

#[test]
fn a_plan_that_honours_the_convention_and_ships_its_family_is_clean() {
    // The noise bound: a correct plan produces ZERO findings. Phase-G history
    // says a flooding verifier anchors agents and costs more than it saves.
    let (_d, s) = store();
    let pid = "p5";
    let path = "Site/App_Code/io/api-json/api-io.vb";
    let mut nodes = vec![
        file_node(path),
        fn_node("CheckRead", "Site/App_Code/sec/useraccess.vb", 10),
    ];
    let mut edges = Vec::new();
    for (i, name) in ["ioGetA", "ioGetB"].iter().enumerate() {
        let n = fn_node(name, path, (i as u32 + 1) * 100);
        edges.push(edge(
            EdgeKind::Calls,
            &n.node_id,
            "sym:function:Site/App_Code/sec/useraccess.vb:CheckRead:10",
            1,
        ));
        nodes.push(n);
    }
    s.upsert_nodes_and_edges(pid, &nodes, &edges).unwrap();

    let (findings, proof) = verify_plan(
        &s,
        pid,
        &plan(&[(
            path,
            "Public Function ioGetC(qry) As JSONreturn\n  If Not _us.UserAccess.CheckRead(...) Then Return s\n  Return db.Query(...)\nEnd Function",
        )]),
    );
    assert!(findings.is_empty(), "a correct plan is quiet: {findings:?}");
    assert!(proof.complete(), "{proof:?}");
}
