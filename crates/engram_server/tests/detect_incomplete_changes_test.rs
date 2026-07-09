//! TODO-29: edit-completeness engine — untouched co-change partners and
//! shared state keys must surface; complete edits must pass clean.

#![allow(clippy::unwrap_used)]
use engram_core::Config;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;

async fn setup() -> (tempfile::TempDir, AppState, engram_server::Engram, String) {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(root.join("a.vb"), "' a").unwrap();
    let cfg = Config {
        allowed_roots: vec![root.to_path_buf()],
        data_dir: root.join("engram_data"),
        max_project_files: Some(100),
        max_project_bytes: Some(1024 * 1024),
        embedding_backend: "fts_only".into(),
        max_concurrent_jobs: 2,
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "EditTest".into(),
            project_type: engram_server::models::ProjectType::General,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    (tmp, state, engram, pid)
}

fn file_node(id: &str, path: &str) -> engram_graph::Node {
    engram_graph::Node {
        node_id: id.to_string(),
        node_type: "file".to_string(),
        name: path.rsplit('/').next().unwrap_or(path).to_string(),
        namespace: "memory".to_string(),
        language: "vb".to_string(),
        file_path: engram_core::RelPath::new(path),
        start_line: 0,
        end_line: 0,
        generation: 1,
        metadata: None,
    }
}

fn edge(s: &str, t: &str, kind: engram_graph::EdgeKind, w: u32) -> engram_graph::Edge {
    engram_graph::Edge {
        source_id: s.to_string(),
        target_id: t.to_string(),
        namespace: "memory".to_string(),
        language: "vb".to_string(),
        edge_kind: kind,
        weight: w,
        generation: 1,
        metadata: None,
        updated_at_ms: 0,
    }
}

fn text_of(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn untouched_partner_and_state_key_surface() {
    let (_tmp, state, engram, pid) = setup().await;
    use engram_graph::EdgeKind;

    let g = &state.graph;
    g.upsert_nodes(
        &pid,
        &[
            file_node("file:pages/edit.aspx", "pages/edit.aspx"),
            file_node("file:pages/edit.aspx.vb", "pages/edit.aspx.vb"),
            engram_graph::Node {
                node_id: "sym:function:pages/edit.aspx.vb:Save:5".into(),
                node_type: "function".into(),
                name: "Save".into(),
                namespace: "memory".into(),
                language: "vb".into(),
                file_path: engram_core::RelPath::new("pages/edit.aspx.vb"),
                start_line: 5,
                end_line: 20,
                generation: 1,
                metadata: None,
            },
            engram_graph::Node {
                node_id: "sym:function:pages/list.aspx.vb:Load:5".into(),
                node_type: "function".into(),
                name: "Load".into(),
                namespace: "memory".into(),
                language: "vb".into(),
                file_path: engram_core::RelPath::new("pages/list.aspx.vb"),
                start_line: 5,
                end_line: 20,
                generation: 1,
                metadata: None,
            },
            engram_graph::Node {
                node_id: "state:Session:CartID".into(),
                node_type: "global_state".into(),
                name: "CartID".into(),
                namespace: "memory".into(),
                language: "vb".into(),
                file_path: engram_core::RelPath::new("state"),
                start_line: 0,
                end_line: 0,
                generation: 1,
                metadata: None,
            },
        ],
    )
    .unwrap();
    g.upsert_edges(
        &pid,
        &[
            // Strong co-change: edited page <-> its codebehind (untouched).
            edge(
                "file:pages/edit.aspx",
                "file:pages/edit.aspx.vb",
                EdgeKind::TemporalCoupling,
                42,
            ),
            // Edited symbol writes a key another file reads.
            edge(
                "sym:function:pages/edit.aspx.vb:Save:5",
                "state:Session:CartID",
                EdgeKind::WritesState,
                1,
            ),
            edge(
                "sym:function:pages/list.aspx.vb:Load:5",
                "state:Session:CartID",
                EdgeKind::ReadsState,
                1,
            ),
        ],
    )
    .unwrap();

    // Case 1: only the .aspx edited — partner must surface.
    let r = engram
        .detect_incomplete_changes(Parameters(
            engram_server::models::DetectIncompleteChangesRequest {
                project_id: pid.clone(),
                edited_files: vec!["pages/edit.aspx".into()],
                max_partners: 5,
            },
        ))
        .await
        .unwrap();
    let text = text_of(&r);
    assert!(
        text.contains("edit.aspx.vb") && text.contains("42 co-changes"),
        "untouched codebehind must surface with evidence:\n{text}"
    );

    // Case 2: codebehind edited — shared state key must surface list.aspx.vb.
    let r2 = engram
        .detect_incomplete_changes(Parameters(
            engram_server::models::DetectIncompleteChangesRequest {
                project_id: pid.clone(),
                edited_files: vec!["pages/edit.aspx.vb".into(), "pages/edit.aspx".into()],
                max_partners: 5,
            },
        ))
        .await
        .unwrap();
    let text2 = text_of(&r2);
    assert!(
        text2.contains("CartID") && text2.contains("list.aspx.vb"),
        "shared state key with untouched reader must surface:\n{text2}"
    );

    // Case 3: everything edited — clean bill.
    let r3 = engram
        .detect_incomplete_changes(Parameters(
            engram_server::models::DetectIncompleteChangesRequest {
                project_id: pid.clone(),
                edited_files: vec![
                    "pages/edit.aspx".into(),
                    "pages/edit.aspx.vb".into(),
                    "pages/list.aspx.vb".into(),
                ],
                max_partners: 5,
            },
        ))
        .await
        .unwrap();
    let text3 = text_of(&r3);
    assert!(
        text3.contains("consistent with a complete change"),
        "complete edit set must pass clean:\n{text3}"
    );
}

#[tokio::test]
async fn session_bookends_track_scope_drift() {
    let (_tmp, state, engram, pid) = setup().await;
    use engram_graph::EdgeKind;

    let g = &state.graph;
    g.upsert_nodes(
        &pid,
        &[
            file_node("file:a.aspx", "a.aspx"),
            file_node("file:a.aspx.vb", "a.aspx.vb"),
        ],
    )
    .unwrap();
    g.upsert_edges(
        &pid,
        &[edge(
            "file:a.aspx",
            "file:a.aspx.vb",
            EdgeKind::TemporalCoupling,
            30,
        )],
    )
    .unwrap();

    // Open: plan both files — the brief should be CLEAN (plan covers the pair).
    let open = engram
        .begin_edit_session(Parameters(engram_server::models::BeginEditSessionRequest {
            project_id: pid.clone(),
            planned_files: vec!["a.aspx".into(), "a.aspx.vb".into()],
            story: Some("test change".into()),
        }))
        .await
        .unwrap();
    let open_text = text_of(&open);
    assert!(open_text.contains("Edit session OPEN"), "{open_text}");
    assert!(
        open_text.contains("consistent with a complete change"),
        "planned pair covers the coupling:
{open_text}"
    );

    // Close having edited only ONE file: drift + partner finding must show.
    let close = engram
        .complete_edit_session(Parameters(
            engram_server::models::CompleteEditSessionRequest {
                project_id: pid.clone(),
                edited_files: vec!["a.aspx".into()],
                dossier: None,
            },
        ))
        .await
        .unwrap();
    let close_text = text_of(&close);
    assert!(
        close_text.contains("Planned but NOT edited") && close_text.contains("a.aspx.vb"),
        "scope drift must name the dropped file:
{close_text}"
    );
    assert!(
        close_text.contains("30 co-changes"),
        "completeness check runs on the actual set:
{close_text}"
    );

    // Session consumed: completing again errors.
    let again = engram
        .complete_edit_session(Parameters(
            engram_server::models::CompleteEditSessionRequest {
                project_id: pid.clone(),
                edited_files: vec!["a.aspx".into()],
                dossier: None,
            },
        ))
        .await;
    assert!(
        again.is_err(),
        "second complete must report no open session"
    );
}
