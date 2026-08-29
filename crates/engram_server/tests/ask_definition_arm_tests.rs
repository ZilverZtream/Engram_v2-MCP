#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 row 6 (owner: keep looping) — golden miss
//! `ox_exact_6`: "Which API file exposes ioUpdateBaseTypeInBulk?" resolved the
//! symbol, yet no retrieval arm returned where it lives (the symbol arms run
//! only for usage/test intents), so the answer was "unsupported". A resolved
//! symbol's own definition is evidence for any question that names it.

use engram_core::RelPath;
use engram_core::config::Config;
use engram_graph::Node;
use engram_server::services::ask_engine::plan::{EntityKind, EntityMention, ResolvedEntity};
use engram_server::services::ask_engine::planner::plan_query;
use engram_server::services::ask_engine::retrieval::{Depth, RetrievalCtx, gather_evidence};
use engram_server::services::ask_engine::status::ProviderStatus;
use engram_server::services::project_service::{ensure_project_runtime, get_active_generation};
use engram_server::state::AppState;
use rmcp::handler::server::tool::Parameters;
use std::time::Duration;

const NID: &str = "sym:function:Site/App_Code/api-json/api.vb:api.ioUpdateBaseTypeInBulk:10";

async fn fixture() -> (tempfile::TempDir, AppState, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let proj = tmp.path().join("project");
    std::fs::create_dir_all(proj.join("Site/App_Code/api-json")).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::write(
        proj.join("Site/App_Code/api-json/api.vb"),
        "Public Class api\n    <WebMethod()>\n    Public Function ioUpdateBaseTypeInBulk(ids As String) As String\n        Return \"ok\"\n    End Function\nEnd Class\n",
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
            project_name: "deffix".into(),
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
                name: "ioUpdateBaseTypeInBulk".into(),
                namespace: "memory".into(),
                language: "vbnet".into(),
                file_path: RelPath::new("Site/App_Code/api-json/api.vb"),
                start_line: 10,
                end_line: 30,
                generation: 1,
                metadata: None,
            }],
        )
        .unwrap();
    (tmp, state, pid)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resolved_symbols_definition_is_evidence_without_a_usage_intent() {
    let (_tmp, state, pid) = fixture().await;
    let ps = ensure_project_runtime(&state, &pid).await.unwrap();
    let gen_ = get_active_generation(&state, &pid).await.unwrap();
    let q = "Which API file exposes ioUpdateBaseTypeInBulk?";
    let mut plan = plan_query(q);
    // The planner may or may not resolve on this tiny fixture; the arm keys on the RESOLVED node id.
    if !plan.entities.iter().any(|e| !e.resolved.is_empty()) {
        plan.entities.push(EntityMention {
            text: "ioUpdateBaseTypeInBulk".into(),
            guessed_kind: EntityKind::Symbol,
            resolved: vec![ResolvedEntity {
                kind: EntityKind::Symbol,
                canonical: "api.ioUpdateBaseTypeInBulk".into(),
                node_id: Some(NID.into()),
                confidence: 0.9,
            }],
        });
    } else {
        for e in plan.entities.iter_mut() {
            for r in e.resolved.iter_mut() {
                r.node_id = Some(NID.into());
            }
        }
    }
    let ctx = RetrievalCtx {
        insights_enabled: true,
        search: ps.search.clone(),
        graph: state.graph.clone(),
        registry: state.registry.clone(),
        project_id: pid.clone(),
        generation: gen_,
    };
    let (evidence, reports) = gather_evidence(
        &ctx,
        &plan,
        q,
        Depth::Standard,
        Duration::from_secs(10),
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    let def = reports
        .iter()
        .find(|r| r.provider == "definition")
        .unwrap_or_else(|| panic!("a definition arm ran: {reports:?}"));
    assert_eq!(def.status, ProviderStatus::Hit, "{reports:?}");
    let item = evidence
        .iter()
        .find(|e| e.provider == "definition")
        .unwrap_or_else(|| {
            panic!(
                "definition evidence present: {:?}",
                evidence
                    .iter()
                    .map(|e| (&e.provider, &e.path))
                    .collect::<Vec<_>>()
            )
        });
    assert!(
        item.path
            .as_deref()
            .unwrap_or("")
            .ends_with("api-json/api.vb"),
        "{item:?}"
    );
    assert_eq!(item.lines, Some((10, 30)));
    assert!(item.content.contains("ioUpdateBaseTypeInBulk"));
}
