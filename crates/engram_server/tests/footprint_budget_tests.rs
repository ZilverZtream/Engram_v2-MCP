#![allow(clippy::unwrap_used)]
//! `get_concept_footprint` spends its per-group budget on rows that cannot be
//! the answer.
//!
//! Measured live (concept "task", `max_per_group: 100`): 100 rows shown but only
//! 91 distinct names — `ShowingTaskCount` appeared four times (twice in the
//! TypeScript source, twice again in the compiled bundle) — and 10 rows sat on
//! declaration/compiled-bundle paths, five of them `typings/google.maps/
//! index.d.ts` classes. The function an edit would actually touch was not in the
//! first hundred.
//!
//! Ranking by concept match (already shipped) was necessary and not sufficient:
//! the budget must also not be spent on declarations, and one name repeated many
//! times must not crowd out distinct evidence. Anything collapsed is REPORTED,
//! never silently dropped.

use engram_core::config::Config;
use engram_server::models::GetConceptFootprintRequest;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;
use serde_json::json;

async fn build() -> (tempfile::TempDir, Engram, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(root.join("typings/vendorlib")).unwrap();
    std::fs::create_dir_all(root.join("ts/widgets")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();

    // Declarations that match the concept but implement nothing.
    std::fs::write(
        root.join("typings/vendorlib/index.d.ts"),
        "declare class WidgetTracker { }\ndeclare class WidgetInfo { }\n\
         declare class WidgetOptions { }\ndeclare class WidgetStatus { }\n",
    )
    .unwrap();
    // One name repeated across several source locations.
    let repeated = (0..6)
        .map(|i| format!("export function ShowingWidgetCount() {{ return {i}; }}\n"))
        .collect::<String>();
    std::fs::write(root.join("ts/widgets/counter.ts"), repeated).unwrap();
    // The implementation an edit would touch — alphabetically last on purpose.
    std::fs::write(
        root.join("src/ZzWidgetStore.vb"),
        "Public Class ZzWidgetStore\n    \
         Public Function UpdateWidget(id As Integer) As Integer\n        \
         Return id\n    End Function\nEnd Class\n",
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
            project_name: "FootprintBudget".into(),
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

async fn footprint(engram: &Engram, pid: &str, cap: usize) -> String {
    let req: GetConceptFootprintRequest = serde_json::from_value(
        json!({"project_id": pid, "concept": "widget", "max_per_group": cap}),
    )
    .unwrap();
    let res = engram.handle_get_concept_footprint(req).await.unwrap();
    res.content[0].as_text().unwrap().text.clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_declaration_file_never_occupies_footprint_budget() {
    let (_tmp, engram, pid) = build().await;
    let out = footprint(&engram, &pid, 8).await;
    // Scoped to GRAPH rows on purpose: a declaration may still be named in the
    // lexical "mentioned only in text" section, which does not spend a group's
    // capped budget. What must not happen is a declaration holding a symbol row.
    let graph_rows: Vec<&str> = out
        .lines()
        .filter(|l| l.starts_with("- ") && l.contains("node_id=sym:"))
        .collect();
    assert!(
        !graph_rows.iter().any(|l| l.contains(".d.ts")),
        "declarations implement nothing and must not hold a symbol row:\n{}",
        graph_rows.join("\n")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_repeated_name_does_not_crowd_out_distinct_evidence() {
    let (_tmp, engram, pid) = build().await;
    let out = footprint(&engram, &pid, 8).await;
    // Count ROWS, not substrings: every row prints the name twice — once as the
    // display name and once inside `node_id=sym:function:…:ShowingWidgetCount:N`.
    let repeats = out
        .lines()
        .filter(|l| l.starts_with("- ShowingWidgetCount "))
        .count();
    assert!(
        repeats <= 2,
        "a single name repeated {repeats} times consumed the budget:\n{out}"
    );
    assert!(
        out.contains("UpdateWidget"),
        "the implementation must survive the cap:\n{out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_capped_group_samples_distinct_files_not_one_file_alphabet() {
    // Live, both failures had this shape: the "permit area" group showed 8 rows
    // of 424 and every one was an `_api2.dto.*` member from a single directory
    // (`_` sorts first); the "task" group spent five of eight slots on one
    // interface file, so `aktivitet.vb` — where the logic actually lives — got
    // no slot at all. A capped sample must show the BREADTH a change has to
    // reach, not one file's alphabet.
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(root.join("src")).unwrap();
    let crowded = (1..=12)
        .map(|i| {
            format!(
                "    Public Function AaaWidget{i:02}(id As Integer) As Integer\n        \
                 Return id\n    End Function\n"
            )
        })
        .collect::<String>();
    std::fs::write(
        root.join("src/AaaWidgetBox.vb"),
        format!("Public Class AaaWidgetBox\n{crowded}End Class\n"),
    )
    .unwrap();
    std::fs::write(
        root.join("src/ZzOther.vb"),
        "Public Class ZzOther\n    \
         Public Function ZzWidgetHandler(id As Integer) As Integer\n        \
         Return id\n    End Function\nEnd Class\n",
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
            project_name: "FootprintBreadth".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsVb,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();

    let out = footprint(&engram, &pid, 4).await;
    assert!(
        out.contains("ZzOther.vb"),
        "a capped group must reach a second file before repeating the first:\n{out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn collapsed_repeats_are_reported_not_silently_dropped() {
    let (_tmp, engram, pid) = build().await;
    let out = footprint(&engram, &pid, 8).await;
    assert!(
        out.contains("occurrence"),
        "what was collapsed must be stated in the output:\n{out}"
    );
}
