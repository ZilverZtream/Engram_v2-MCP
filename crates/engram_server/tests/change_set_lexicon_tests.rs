#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 row 1 (owner decision 10:58). Live finding: an
//! English-only story ("…the reporting of quantities…") produced concepts
//! `project, manager, reporting` and no Swedish term, while the code (and
//! the project's own .resx) say `Mängdredovisning`. With the resx lexicon
//! the Swedish implementation file renders BY DEFAULT with the `lexicon`
//! signal, and coverage names the translation.

use engram_core::config::Config;
use engram_server::models::GetChangeSetRequest;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;
use serde_json::{Value, json};

const STORY: &str = "As a project manager I want the reporting of quantities to show the change requests per fiber installation plan so that invoicing matches the field work";

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
    for d in [
        "Site/App_GlobalResources",
        "Site/App_Code/redovisning/code",
        "Site/App_Code/noise",
    ] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    std::fs::write(
        root.join("Site/App_GlobalResources/text.resx"),
        resx(&[
            ("Registration_of_quantities", "Mängdredovisning"),
            ("Registration_of_CAW", "ÄTA-registrering"),
            ("Fiber_installation_plan", "Fiberinstallationsplan"),
            ("Save", "Spara"),
        ]),
    )
    .unwrap();
    std::fs::write(
        root.join("Site/App_GlobalResources/text.en.resx"),
        resx(&[
            ("Registration_of_quantities", "Reporting of Quantities"),
            ("Registration_of_CAW", "Change Requests"),
            ("Fiber_installation_plan", "Fiber installation plan"),
            ("Save", "Save"),
        ]),
    )
    .unwrap();
    std::fs::write(
        root.join("Site/App_Code/redovisning/code/mangdredovisning.vb"),
        "Public Class mangdredovisning\n    ' Mängdredovisning per fiberinstallationsplan\n    Public Function GetByPlan(plan_id As Integer) As Object\n        Return (From m In db.rk_mangdredovisning Where m.plan_id = plan_id).ToList()\n    End Function\nEnd Class\n",
    )
    .unwrap();
    std::fs::write(
        root.join("Site/App_Code/redovisning/code/ata_registrering.vb"),
        "Public Class ata_registrering\n    ' ÄTA-registrering (change requests) per plan\n    Public Function ListForPlan(plan_id As Integer) As Object\n        Return db.rk_ata.Where(Function(a) a.plan_id = plan_id).ToList()\n    End Function\nEnd Class\n",
    )
    .unwrap();
    for i in 0..30 {
        std::fs::write(
            root.join(format!("Site/App_Code/noise/report_helper{i:02}.vb")),
            format!("Public Class report_helper{i:02}\n    Public Function ProjectManagerReport{i}() As String\n        Return \"reporting\"\n    End Function\nEnd Class\n"),
        )
        .unwrap();
    }
    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: tmp.path().join("data"),
        max_project_files: Some(200),
        max_project_bytes: Some(4 * 1024 * 1024),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "LexiconFixture".into(),
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

fn paths(v: &Value, key: &str) -> Vec<String> {
    v[key]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|f| f["path"].as_str().map(|s| s.to_lowercase()))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_english_story_reaches_the_swedish_implementation_through_the_resx_lexicon() {
    let (_tmp, engram, pid) = build().await;
    let req: GetChangeSetRequest =
        serde_json::from_value(json!({"project_id": pid, "story": STORY, "output_json": true}))
            .unwrap();
    let res = engram.handle_get_change_set(req).await.unwrap();
    let t = res.content[0].as_text().unwrap().text.clone();
    let v: Value = serde_json::from_str(&t).unwrap_or_else(|e| panic!("not JSON ({e}):\n{t}"));

    let concepts: Vec<String> = v["concepts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_lowercase())
        .collect();
    assert!(
        concepts
            .iter()
            .any(|c| c.starts_with("mangdredovisning") || c.starts_with("mängdredovisning")),
        "the lexicon term is a DEFAULT concept, got {concepts:?}"
    );

    let files = paths(&v, "files");
    let omissions = paths(&v, "omissions");
    for must in [
        "redovisning/code/mangdredovisning.vb",
        "redovisning/code/ata_registrering.vb",
    ] {
        assert!(
            files.iter().any(|p| p.contains(must)),
            "{must} must render (files: {files:?}; omitted: {omissions:?})"
        );
    }
    let f = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| {
            f["path"]
                .as_str()
                .unwrap_or("")
                .to_lowercase()
                .contains("mangdredovisning.vb")
        })
        .unwrap();
    let sig = f.to_string();
    assert!(
        sig.contains("lexicon"),
        "the file carries the lexicon signal: {sig}"
    );
    assert!(
        t.contains("lexicon"),
        "coverage names the translation:\n{t}"
    );
}
