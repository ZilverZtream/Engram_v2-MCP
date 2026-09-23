#![allow(clippy::unwrap_used)]
//! `get_concept_footprint` groups were ordered by a plain alphabetical sort and
//! then cut at `max_per_group`, so on a real concept (hundreds of matches) the
//! identifiers that ARE the concept lost their slot to whatever sorted first.
//! The cut is honest ("… and N more") but the agent sees 8 of 425 and concludes
//! the concept does not reach the code that owns it.
//!
//! Same bug shape as the co-change `file_pairs` truncation: sort, truncate,
//! lose the signal. A group must be ranked by how well each name matches the
//! concept BEFORE the cap applies.

use engram_core::config::Config;
use engram_server::models::GetConceptFootprintRequest;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;
use serde_json::json;

/// Alphabetically-early names that match the concept only as a token PREFIX.
const DECOYS: usize = 40;

async fn build() -> (tempfile::TempDir, Engram, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(root.join("src")).unwrap();
    // "widgetry" starts with the stem "widget", so these all match the concept
    // and sort ahead of any name starting later in the alphabet.
    for i in 0..DECOYS {
        std::fs::write(
            root.join(format!("src/Aa{i:03}Widgetry.vb")),
            format!(
                "Public Class Aa{i:03}WidgetryHandler\n    \
                 Public Function Aa{i:03}WidgetryLookup(id As Integer) As Integer\n        \
                 Return id\n    End Function\nEnd Class\n"
            ),
        )
        .unwrap();
    }
    // The identifiers that ARE the concept: an exact token match, sorting last.
    std::fs::write(
        root.join("src/ZzWidgetStore.vb"),
        "Public Class ZzWidgetStore\n    \
         Public Function UpdateWidget(id As Integer) As Integer\n        \
         Return id\n    End Function\n    \
         Public Function WidgetWritePermission(id As Integer) As Boolean\n        \
         Return True\n    End Function\nEnd Class\n",
    )
    .unwrap();
    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: tmp.path().join("data"),
        max_project_files: Some(200),
        max_project_bytes: Some(8 * 1024 * 1024),
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "FootprintRanking".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsVb,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    (tmp, engram, pid)
}

async fn footprint(engram: &Engram, pid: &str) -> String {
    // Default cap: exactly what an agent following the mandated workflow gets.
    let req: GetConceptFootprintRequest =
        serde_json::from_value(json!({"project_id": pid, "concept": "widget"})).unwrap();
    let res = engram.handle_get_concept_footprint(req).await.unwrap();
    res.content[0].as_text().unwrap().text.clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_exact_concept_match_outranks_alphabetically_earlier_prefix_matches() {
    let (_tmp, engram, pid) = build().await;
    let out = footprint(&engram, &pid).await;
    assert!(
        out.contains("UpdateWidget"),
        "a name whose token IS the concept must survive the default cap, but the \
         group was filled alphabetically:\n{out}"
    );
    assert!(
        out.contains("WidgetWritePermission"),
        "second exact-token match must also outrank prefix-only decoys:\n{out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_cut_is_still_reported_when_a_group_is_capped() {
    let (_tmp, engram, pid) = build().await;
    let out = footprint(&engram, &pid).await;
    assert!(
        out.contains("more"),
        "ranking must not silence the truncation notice:\n{out}"
    );
}
