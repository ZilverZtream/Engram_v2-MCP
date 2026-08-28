#![allow(clippy::unwrap_used)]
//! Row-4 audit (docs/audits/04-concept-and-consumer-discovery.md) A11:
//! `get_concept_footprint` clamps `max_per_group` to ≤ 100, so a
//! 137-file "Mentioned only in text" section (live, `installationsobjekt`)
//! can never be listed in full by any caller — the 14 residual G1 misses.
//! The cap stays reported ("… and N more") but the ceiling must let a
//! caller ask for the whole list.

use engram_core::config::Config;
use engram_server::models::GetConceptFootprintRequest;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;
use serde_json::json;

const N: usize = 120;

async fn build() -> (tempfile::TempDir, Engram, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(root.join("Site/pages")).unwrap();
    for i in 0..N {
        std::fs::write(
            root.join(format!("Site/pages/page{i:03}.aspx.vb")),
            format!(
                "Partial Class page{i:03}\n    Private Sub Page_Load(sender As Object, e As EventArgs)\n        ' personalliggare listing {i}\n        Dim x = personalliggare_{i}\n    End Sub\nEnd Class\n"
            ),
        )
        .unwrap();
    }
    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: tmp.path().join("data"),
        max_project_files: Some(500),
        max_project_bytes: Some(8 * 1024 * 1024),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "CeilingFixture".into(),
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

async fn footprint(engram: &Engram, pid: &str, max_per_group: usize) -> String {
    let req: GetConceptFootprintRequest = serde_json::from_value(
        json!({"project_id": pid, "concept": "personalliggare", "max_per_group": max_per_group}),
    )
    .unwrap();
    let res = engram.handle_get_concept_footprint(req).await.unwrap();
    res.content[0].as_text().unwrap().text.clone()
}

fn files_named(out: &str) -> usize {
    (0..N)
        .filter(|i| out.contains(&format!("page{i:03}.aspx.vb")))
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_caller_can_ask_for_the_whole_text_section() {
    let (_tmp, engram, pid) = build().await;
    let out = footprint(&engram, &pid, 500).await;
    assert_eq!(
        files_named(&out),
        N,
        "max_per_group=500 must list all {N} files (ceiling), got:\n{}",
        out.lines()
            .filter(|l| l.contains("more") || l.starts_with("## "))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        !out.contains("… and") && !out.contains("... and"),
        "nothing may be cut at 500:\n{out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_default_cap_is_still_reported_not_silent() {
    let (_tmp, engram, pid) = build().await;
    let out = footprint(&engram, &pid, 100).await;
    let named = files_named(&out);
    assert!(
        named <= 100 && named >= 90,
        "cap 100 ⇒ about 100 named, got {named}"
    );
    assert!(
        (out.contains("… and") || out.contains("... and")) && out.contains("more"),
        "the cut must be stated:\n{out}"
    );
}
