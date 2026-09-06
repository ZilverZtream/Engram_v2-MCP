#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 row 6 — the last golden miss on release 30:
//! with every `GetByID` in the candidate list (cycle 11), the qualifier
//! `projekt` of "GetByID in the projekt DAL" still left TWO candidates,
//! because it substring-matches `io-installationsobjektprojekt.vb` as well
//! as `projekt.vb`. A qualifier that names a candidate's class or file stem
//! EXACTLY outranks one that merely occurs inside a longer name.

use engram_core::RelPath;
use engram_core::config::Config;
use engram_graph::Node;
use engram_server::services::ask_engine::planner::plan_query;
use engram_server::services::ask_engine::resolver::resolve_entities_in_context;
use engram_server::state::AppState;

const PID: &str = "resolver-strength-test";
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
fn an_exact_class_or_file_stem_match_outranks_a_substring_match() {
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
                func(
                    "Site/App_Code/grunddata/code/projekt.vb",
                    "_gd.projekt.GetByID",
                ),
                func(
                    "Site/App_Code/installationsobjekt/code/io-installationsobjektprojekt.vb",
                    "_io.installationsobjektprojekt.GetByID",
                ),
                func("Site/App_Code/markers/marker.vb", "_markers.marker.GetByID"),
            ],
        )
        .unwrap();
    let mut plan = plan_query(Q);
    resolve_entities_in_context(&state.graph, PID, &mut plan, Q);
    let m = plan.entities.iter().find(|e| e.text == "GetByID").unwrap();
    assert_eq!(
        m.resolved.len(),
        1,
        "`projekt` names the class/file exactly — the substring cousin does not survive: {:?}",
        m.resolved
    );
    assert!(
        m.resolved[0]
            .canonical
            .to_lowercase()
            .contains("_gd.projekt.getbyid")
            || m.resolved[0]
                .canonical
                .to_lowercase()
                .ends_with("projekt.getbyid"),
        "{:?}",
        m.resolved[0]
    );
    assert!(m.resolved[0].confidence >= 0.8);
}

#[test]
fn substring_only_matches_still_narrow_when_nothing_matches_exactly() {
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
                func(
                    "Site/App_Code/installationsobjekt/code/io-installationsobjektprojekt.vb",
                    "_io.installationsobjektprojekt.GetByID",
                ),
                func("Site/App_Code/markers/marker.vb", "_markers.marker.GetByID"),
            ],
        )
        .unwrap();
    let mut plan = plan_query(Q);
    resolve_entities_in_context(&state.graph, PID, &mut plan, Q);
    let m = plan.entities.iter().find(|e| e.text == "GetByID").unwrap();
    assert_eq!(
        m.resolved.len(),
        1,
        "the only candidate mentioning projekt: {:?}",
        m.resolved
    );
    assert!(
        m.resolved[0]
            .canonical
            .to_lowercase()
            .contains("installationsobjektprojekt")
    );
}

// ── Round-8 P0-2: narrow_by_qualifiers preserves ambiguity ────────────────

#[test]
fn no_qualifier_preserves_ambiguous_backends_not_first() {
    use engram_server::services::ask_engine::resolver::narrow_by_qualifiers;
    // Two backend implementations share the terminal name; the question names
    // NO class/file qualifier — BOTH must survive. The server-cue path used to
    // `.find()` the first, a silent wrong answer.
    let cands = vec![
        func("Site/App_Code/api-json/api-images.vb", "api.DeleteImage"),
        func(
            "Site/App_Code/handlers/imgHandler.vb",
            "imgHandler.DeleteImage",
        ),
    ];
    let out = narrow_by_qualifiers(cands, "who calls DeleteImage on the server", "DeleteImage");
    assert_eq!(
        out.len(),
        2,
        "no qualifier ⇒ ambiguity preserved: {:?}",
        out.iter().map(|n| n.name.clone()).collect::<Vec<_>>()
    );
}

#[test]
fn a_class_qualifier_selects_one_backend() {
    use engram_server::services::ask_engine::resolver::narrow_by_qualifiers;
    let cands = vec![
        func("Site/App_Code/images/images.vb", "images.DeleteImage"),
        func("Site/App_Code/handlers/handler.vb", "handler.DeleteImage"),
    ];
    // "images" (>=4 alphabetic) names the class segment of images.DeleteImage.
    let out = narrow_by_qualifiers(
        cands,
        "the DeleteImage web method in the images class",
        "DeleteImage",
    );
    assert_eq!(
        out.len(),
        1,
        "{:?}",
        out.iter().map(|n| n.name.clone()).collect::<Vec<_>>()
    );
    assert!(out[0].name.to_lowercase().starts_with("images."));
}
