#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 row 1 / P0-3: on OciusX the English-only story
//! translated into seven Swedish terms and every one of them ran a concept
//! footprint (~1 s each) — 38 s. The lexicon contributes at most
//! `LEXICON_CONCEPT_CAP` (4) concept terms, most specific first; coverage
//! still lists every translation so nothing is hidden.

use engram_core::config::Config;
use engram_server::models::GetChangeSetRequest;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;
use serde_json::{Value, json};

const STORY: &str = "As a project manager I want the reporting of quantities, the change requests, the fiber installation plan, the inspection round and the customer invoice to be visible per work team";
const SV: [&str; 6] = [
    "mängdredovisning",
    "äta-registrering",
    "fiberinstallationsplan",
    "besiktningsrunda",
    "kundfaktura",
    "arbetslag",
];
const EN: [&str; 6] = [
    "Reporting of Quantities",
    "Change Requests",
    "Fiber installation plan",
    "Inspection round",
    "Customer invoice",
    "Work team",
];

fn resx(entries: &[(&str, &str)]) -> String {
    let mut s = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root>\n");
    for (k, v) in entries {
        s.push_str(&format!(
            "  <data name=\"{k}\" xml:space=\"preserve\">\n    <value>{v}</value>\n  </data>\n"
        ));
    }
    s.push_str("</root>\n");
    s
}

async fn build() -> (tempfile::TempDir, Engram, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(root.join("Site/App_GlobalResources")).unwrap();
    std::fs::create_dir_all(root.join("Site/App_Code")).unwrap();
    let keys: Vec<String> = (0..6).map(|i| format!("K{i}")).collect();
    let sv: Vec<(&str, &str)> = keys
        .iter()
        .map(|k| k.as_str())
        .zip(SV.iter().copied())
        .collect();
    let en: Vec<(&str, &str)> = keys
        .iter()
        .map(|k| k.as_str())
        .zip(EN.iter().copied())
        .collect();
    std::fs::write(root.join("Site/App_GlobalResources/text.resx"), resx(&sv)).unwrap();
    std::fs::write(
        root.join("Site/App_GlobalResources/text.en.resx"),
        resx(&en),
    )
    .unwrap();
    for (i, t) in SV.iter().enumerate() {
        let ascii: String = t
            .chars()
            .map(|c| match c {
                'ä' | 'å' => 'a',
                'ö' => 'o',
                '-' => '_',
                x => x,
            })
            .collect();
        std::fs::write(
            root.join(format!("Site/App_Code/{ascii}.vb")),
            format!("Public Class {ascii}\n    ' {t}\n    Public Function List{i}() As Object\n        Return Nothing\n    End Function\nEnd Class\n"),
        )
        .unwrap();
    }
    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: tmp.path().join("data"),
        max_project_files: Some(50),
        max_project_bytes: Some(1024 * 1024),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "LexiconCapFixture".into(),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_lexicon_contributes_at_most_the_cap_while_coverage_lists_every_translation() {
    let (_tmp, engram, pid) = build().await;
    let req: GetChangeSetRequest =
        serde_json::from_value(json!({"project_id": pid, "story": STORY, "output_json": true}))
            .unwrap();
    let res = engram.handle_get_change_set(req).await.unwrap();
    let t = res.content[0].as_text().unwrap().text.clone();
    let v: Value = serde_json::from_str(&t).unwrap_or_else(|e| panic!("not JSON ({e}):\n{t}"));
    let listed = v["coverage"]["lexicon_concepts"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0);
    assert!(
        listed >= 5,
        "coverage lists every translation ({listed}): {}",
        v["coverage"]["lexicon_concepts"]
    );
    let concepts: Vec<String> = v["concepts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_lowercase())
        .collect();
    let swedish = |c: &str| {
        SV.iter().any(|s| {
            let folded: String = s
                .chars()
                .map(|ch| match ch {
                    'ä' | 'å' => 'a',
                    'ö' => 'o',
                    x => x,
                })
                .collect();
            c.contains(&s.replace('-', ""))
                || c.contains(&folded.replace('-', ""))
                || s.split('-').any(|p| p.len() >= 5 && c == p)
        })
    };
    let from_lexicon = concepts.iter().filter(|c| swedish(c)).count();
    assert!(
        from_lexicon >= 1
            && from_lexicon <= engram_server::handlers::planning_tools::LEXICON_CONCEPT_CAP,
        "lexicon-derived concepts capped at {} (got {from_lexicon}): {concepts:?}",
        engram_server::handlers::planning_tools::LEXICON_CONCEPT_CAP
    );
}
