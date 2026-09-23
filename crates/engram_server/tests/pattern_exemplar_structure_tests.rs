#![allow(clippy::unwrap_used)]
//! `find_implementation_pattern` offered files that cannot be imitated.
//!
//! Live, a behaviour-phrased query ("notify another map component without
//! calling it directly, using a custom event") inferred kind `Any`, so ranking
//! fell through to raw full-text score and exemplars #1-#2 were a vendored
//! library's XML documentation under `Bin/` and a `.d.ts` declaration file —
//! each annotated "no event handlers found in the graph for this file", under a
//! footer reading "no house pattern can be claimed from these files". The tool
//! knew they demonstrated nothing and presented them anyway.
//!
//! The fix excludes what cannot implement (declaration files, build output);
//! it does NOT reorder by structure. `lexical_match_beats_unrelated_density_
//! for_classes_and_pages` (planning_tools unit tests) deliberately requires a
//! lexically better match to beat unrelated structural density, and that
//! invariant stands — among real code, lexical fit still decides.

use engram_core::config::Config;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;
use serde_json::json;

const QUERY: &str = "notify another component when the selection changes";

async fn build() -> (tempfile::TempDir, Engram, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(root.join("Bin")).unwrap();
    std::fs::create_dir_all(root.join("typings")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();

    // Prose repeating the query's vocabulary far more often than real code
    // does, in build output. Not under any vendor directory name, so the
    // existing vendor filter does not catch it.
    std::fs::write(
        root.join("Bin/thirdparty-docs.xml"),
        "<doc><summary>notify another component when the selection changes; \
         the selection changes notify listeners of the component</summary></doc>\n"
            .repeat(120),
    )
    .unwrap();
    std::fs::write(
        root.join("typings/component-events.d.ts"),
        "// notify another component when the selection changes\n\
         declare function notifySelectionChanged(component: string): void;\n".repeat(40),
    )
    .unwrap();

    // The only file here that anyone could imitate.
    std::fs::write(
        root.join("src/SelectionBroker.vb"),
        "Public Class SelectionBroker\n    \
         Public Sub Page_Load(sender As Object, e As EventArgs)\n        \
         NotifySelectionChanged()\n    End Sub\n    \
         Public Sub NotifySelectionChanged()\n        \
         ' notify another component when the selection changes\n        \
         RaiseEvent SelectionChanged()\n    End Sub\n    \
         Public Event SelectionChanged()\nEnd Class\n",
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
            project_name: "ExemplarCandidacy".into(),
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

async fn exemplar_paths(engram: &Engram, pid: &str) -> Vec<String> {
    let res = engram
        .handle_find_implementation_pattern(
            serde_json::from_value(json!({
                "project_id": pid, "pattern_query": QUERY,
                "max_examples": 3, "output_json": true
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let v: serde_json::Value =
        serde_json::from_str(&res.content[0].as_text().unwrap().text).unwrap();
    v["exemplars"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_declaration_file_is_never_offered_as_an_implementation_exemplar() {
    let (_tmp, engram, pid) = build().await;
    let paths = exemplar_paths(&engram, &pid).await;
    assert!(
        !paths.iter().any(|p| p.ends_with(".d.ts")),
        "a .d.ts declares, it never implements: {paths:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn build_output_is_never_offered_as_an_implementation_exemplar() {
    let (_tmp, engram, pid) = build().await;
    let paths = exemplar_paths(&engram, &pid).await;
    assert!(
        !paths.iter().any(|p| p.to_ascii_lowercase().starts_with("bin/")),
        "generated build output is not a house pattern: {paths:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_implementing_file_is_what_remains() {
    let (_tmp, engram, pid) = build().await;
    let paths = exemplar_paths(&engram, &pid).await;
    assert_eq!(
        paths.first().map(String::as_str),
        Some("src/SelectionBroker.vb"),
        "the code that implements the behaviour must lead: {paths:?}"
    );
}
