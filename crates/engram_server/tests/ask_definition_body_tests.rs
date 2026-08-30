#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 row 6 — the last golden miss on release 32:
//! `ox_impact_4` ("What would break if GetByID in the projekt DAL stopped
//! calling check_pr_id?") now resolves to ONE candidate, yet the status is
//! Unsupported: the definition arm's evidence said only "… is defined in
//! projekt.vb lines 815-829", so the named term `check_pr_id` — which the
//! body calls on its first line — was nowhere in the evidence. A definition's
//! own source lines ARE the evidence a question about its body needs.

use engram_core::RelPath;
use engram_core::config::Config;
use engram_graph::Node;
use engram_server::services::ask_engine::plan::{EntityKind, EntityMention, ResolvedEntity};
use engram_server::services::ask_engine::planner::plan_query;
use engram_server::services::ask_engine::retrieval::{Depth, RetrievalCtx, gather_evidence};
use engram_server::services::ask_engine::status::uncovered_named_terms_with;
use engram_server::services::project_service::{ensure_project_runtime, get_active_generation};
use engram_server::state::AppState;
use rmcp::handler::server::tool::Parameters;
use std::time::Duration;

const NID: &str = "sym:function:Site/App_Code/grunddata/code/projekt.vb:_gd.projekt.GetByID:5";
const Q: &str = "What would break if GetByID in the projekt DAL stopped calling check_pr_id?";

async fn fixture() -> (tempfile::TempDir, AppState, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let proj = tmp.path().join("project");
    std::fs::create_dir_all(proj.join("Site/App_Code/grunddata/code")).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::write(
        proj.join("Site/App_Code/grunddata/code/projekt.vb"),
        "Namespace _gd\nPublic Class projekt\n    ''' <summary>Get a project by id</summary>\n    ''' <param name=\"pr_id\">Project Id#</param>\n    Public Shared Function GetByID(pr_id As Integer) As pr_projekt\n        ' DATA ACCESS CHECKED (PR)\n        If Not _us.accessctrl.Check_pr_id(pr_id) Then Return Nothing\n        Dim db = New iFaltDataContext()\n        Return db.pr_projekts.FirstOrDefault(Function(p) p.pr_id = pr_id)\n    End Function\nEnd Class\nEnd Namespace\n",
    )
    .unwrap();
    let cfg = Config {
        data_dir,
        allowed_roots: vec![proj.clone()],
        max_project_files: Some(50),
        max_project_bytes: Some(1 << 20),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: proj.to_string_lossy().to_string(),
            project_name: "defbody".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsVb,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    state
        .graph
        .upsert_nodes(
            &pid,
            &[Node {
                node_id: NID.into(),
                node_type: "function".into(),
                name: "_gd.projekt.GetByID".into(),
                namespace: "memory".into(),
                language: "vbnet".into(),
                file_path: RelPath::new("Site/App_Code/grunddata/code/projekt.vb"),
                start_line: 5,
                end_line: 10,
                generation: 1,
                metadata: None,
            }],
        )
        .unwrap();
    (tmp, state, pid)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_definition_evidence_carries_the_body_so_a_named_callee_is_covered() {
    let (_tmp, state, pid) = fixture().await;
    let ps = ensure_project_runtime(&state, &pid).await.unwrap();
    let gen_ = get_active_generation(&state, &pid).await.unwrap();
    let mut plan = plan_query(Q);
    // The arm keys on the RESOLVED node id: pin the GetByID mention to the fixture node.
    let mut pinned = false;
    for e in plan.entities.iter_mut() {
        if e.text.eq_ignore_ascii_case("GetByID") {
            e.resolved = vec![ResolvedEntity {
                kind: EntityKind::Symbol,
                canonical: "_gd.projekt.GetByID".into(),
                node_id: Some(NID.into()),
                confidence: 0.9,
            }];
            pinned = true;
        }
    }
    if !pinned {
        plan.entities.push(EntityMention {
            text: "GetByID".into(),
            guessed_kind: EntityKind::Symbol,
            resolved: vec![ResolvedEntity {
                kind: EntityKind::Symbol,
                canonical: "_gd.projekt.GetByID".into(),
                node_id: Some(NID.into()),
                confidence: 0.9,
            }],
        });
    }
    let ctx = RetrievalCtx {
        insights_enabled: true,
        search: ps.search.clone(),
        graph: state.graph.clone(),
        registry: state.registry.clone(),
        project_id: pid.clone(),
        generation: gen_,
    };
    let (evidence, _reports) = gather_evidence(
        &ctx,
        &plan,
        Q,
        Depth::Standard,
        Duration::from_secs(10),
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    // The definition arm's item (other arms also carry the symbol id with a bare
    // "name (type)" content); its content starts with "… is defined in …".
    let def = evidence
        .iter()
        .find(|e| e.symbol_id.as_deref() == Some(NID) && e.content.contains("is defined in"))
        .or_else(|| {
            evidence
                .iter()
                .find(|e| e.content.contains("is defined in"))
        })
        .unwrap_or_else(|| panic!("the definition arm produced evidence: {evidence:?}"));
    assert!(
        def.content.to_lowercase().contains("check_pr_id"),
        "the definition evidence carries the body (the callee the question names): {}",
        def.content
    );
    assert!(
        def.content.contains("Public Shared Function GetByID"),
        "the signature line is part of the body: {}",
        def.content
    );
    let uncovered = format!("{:?}", uncovered_named_terms_with(Q, &evidence, &[])).to_lowercase();
    assert!(
        !uncovered.contains("check_pr_id"),
        "with the body in evidence the named callee is covered: uncovered = {uncovered}"
    );
}
