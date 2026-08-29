#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 row 6 (owner: keep looping) — golden miss
//! `ox_impact_4`: "What would break if GetByID in the projekt DAL stopped
//! calling check_pr_id?" resolved `GetByID` to four `_ata.*` symbols and came
//! back AMBIGUOUS although the question says which one: the qualifier words
//! of the question ("projekt") narrow ambiguous candidates by their path or
//! qualified name.

use engram_core::RelPath;
use engram_core::config::Config;
use engram_graph::Node;
use engram_server::services::ask_engine::planner::plan_query;
use engram_server::services::ask_engine::resolver::{
    resolve_entities, resolve_entities_in_context,
};
use engram_server::state::AppState;

const PID: &str = "resolver-qualifier-test";
const Q: &str = "What would break if GetByID in the projekt DAL stopped calling check_pr_id?";

fn state() -> (tempfile::TempDir, AppState) {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();
    let cfg = Config {
        data_dir,
        allowed_roots: vec![project_dir.clone()],
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    let (state, _rx) = AppState::new(cfg).unwrap();
    state
        .registry
        .put_project(&engram_core::ProjectRecord {
            project_id: PID.into(),
            project_name: PID.into(),
            directory: project_dir.to_string_lossy().into_owned(),
            project_type: "dotnet_webforms_vb".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            reindex_required_since_ms: None,
        })
        .unwrap();
    state
        .registry
        .set_meta(PID, "active_generation", "1")
        .unwrap();
    (tmp, state)
}

fn func(path: &str, fqn: &str) -> Node {
    Node {
        node_id: format!("sym:function:{path}:{fqn}:1"),
        node_type: "function".into(),
        name: fqn.into(),
        namespace: "memory".into(),
        language: "vbnet".into(),
        file_path: RelPath::new(path),
        start_line: 1,
        end_line: 20,
        generation: 1,
        metadata: None,
    }
}

#[test]
fn the_questions_qualifier_narrows_an_ambiguous_symbol_to_the_named_module() {
    let (_tmp, state) = state();
    state
        .graph
        .upsert_nodes(
            PID,
            &[
                func(
                    "Site/App_Code/ata/code/atalista.vb",
                    "_ata.atalista.GetByID",
                ),
                func("Site/App_Code/ata/code/huvud.vb", "_ata.huvud.GetByID"),
                func(
                    "Site/App_Code/grunddata/code/projekt.vb",
                    "_grunddata.projekt.GetByID",
                ),
                func("Site/App_Code/markers/marker.vb", "_markers.marker.GetByID"),
            ],
        )
        .unwrap();
    let mut plain = plan_query(Q);
    resolve_entities(&state.graph, PID, &mut plain);
    let m = plain
        .entities
        .iter()
        .find(|e| e.text == "GetByID")
        .unwrap_or_else(|| panic!("GetByID is an entity: {plain:?}"));
    assert!(
        m.resolved.len() >= 2,
        "precondition: without context the name is ambiguous ({} candidates): {:?}",
        m.resolved.len(),
        m.resolved
    );

    let mut plan = plan_query(Q);
    resolve_entities_in_context(&state.graph, PID, &mut plan, Q);
    let m = plan.entities.iter().find(|e| e.text == "GetByID").unwrap();
    assert_eq!(
        m.resolved.len(),
        1,
        "the qualifier 'projekt' selects one candidate: {:?}",
        m.resolved
    );
    assert!(
        m.resolved[0].canonical.to_lowercase().contains("projekt"),
        "{:?}",
        m.resolved[0]
    );
    assert!(m.resolved[0].confidence >= 0.8);
}

#[test]
fn without_a_matching_qualifier_the_candidates_are_kept() {
    let (_tmp, state) = state();
    state
        .graph
        .upsert_nodes(
            PID,
            &[
                func(
                    "Site/App_Code/ata/code/atalista.vb",
                    "_ata.atalista.GetByID",
                ),
                func("Site/App_Code/ata/code/huvud.vb", "_ata.huvud.GetByID"),
            ],
        )
        .unwrap();
    let q = "Where is GetByID defined?";
    let mut plan = plan_query(q);
    resolve_entities_in_context(&state.graph, PID, &mut plan, q);
    let m = plan.entities.iter().find(|e| e.text == "GetByID").unwrap();
    assert_eq!(
        m.resolved.len(),
        2,
        "no qualifier → both branches stay: {:?}",
        m.resolved
    );
}
