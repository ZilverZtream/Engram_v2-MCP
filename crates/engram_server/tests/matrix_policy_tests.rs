#![allow(clippy::unwrap_used)]
//! QA plans must carry authored requirements even without inferred rule cases.
use engram_core::{Config, ProjectRecord};
use engram_server::{AppState, Engram};
use serde_json::json;
use std::path::Path;

const PID: &str = "matrix-authored-policy";
const TARGET: &str = "Site/App_Code/layers/Save.vb";
const TOPIC: &str = ".github/instructions/layers.instructions.md";
const EXCEPTION: &str = "The original-project restriction controls UI visibility only; saves require creator-company ownership.";

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn topic(body: &str) -> String {
    format!("---\napplyTo: \"Site/App_Code/layers/**\"\n---\n{body}\n")
}

fn fixture() -> (tempfile::TempDir, Engram) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    write(&root, TARGET, "Public Class Save\nEnd Class\n");
    write(
        &root,
        "AGENTS.md",
        "Preserve accepted authorization boundaries.\n",
    );
    write(&root, TOPIC, &topic(EXCEPTION));
    write(
        &root,
        ".github/instructions/reports.instructions.md",
        "---\napplyTo: \"Site/Reports/**\"\n---\nUNRELATED_REPORT_REQUIREMENT\n",
    );
    let (state, _) = AppState::new(Config {
        data_dir: temp.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        ..Default::default()
    })
    .unwrap();
    state
        .registry
        .put_project(&ProjectRecord {
            project_id: PID.into(),
            project_name: PID.into(),
            directory: root.to_string_lossy().into_owned(),
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
    (temp, Engram::new(state))
}

async fn matrix(engram: &Engram, historical: bool) -> String {
    let mut request = json!({"project_id":PID,"files":[TARGET]});
    if historical {
        request["knowledge_before"] = json!("2026-01-01");
    }
    let result = engram
        .handle_derive_test_matrix(serde_json::from_value(request).unwrap())
        .await
        .unwrap();
    result.content[0].as_text().unwrap().text.clone()
}

#[tokio::test]
async fn matrix_delivers_scoped_complete_guidance_and_live_changes_without_inferred_cases() {
    let (temp, engram) = fixture();
    let root = temp.path().join("project");
    let first = matrix(&engram, false).await;
    assert!(first.contains("Preserve accepted authorization boundaries."));
    assert!(first.contains(&topic(EXCEPTION)));
    assert!(
        first.contains(
            &blake3::hash(topic(EXCEPTION).as_bytes())
                .to_hex()
                .to_string()
        )
    );
    assert!(!first.contains("UNRELATED_REPORT_REQUIREMENT"));
    assert!(first.contains("No source-verified rule cases available"));
    assert!(first.contains("Test execution: not_run"));
    assert!(first.contains("not proof of implementation compliance"));

    let updated = topic(&format!(
        "{EXCEPTION}\nVerify visible controls separately from save authorization."
    ));
    write(&root, TOPIC, &updated);
    let second = matrix(&engram, false).await;
    assert!(second.contains(&updated));
    assert!(second.contains(&blake3::hash(updated.as_bytes()).to_hex().to_string()));
    assert!(
        !second.contains(
            &blake3::hash(topic(EXCEPTION).as_bytes())
                .to_hex()
                .to_string()
        )
    );
    drop(engram);
    temp.close().unwrap();
}

#[tokio::test]
async fn matrix_discloses_oversized_guidance_and_withholds_live_policy_for_history() {
    let (temp, engram) = fixture();
    let oversized = topic(&format!(
        "OVERSIZED_POLICY_START\n{}\n{EXCEPTION}",
        "Whole authored qualification. ".repeat(800)
    ));
    write(&temp.path().join("project"), TOPIC, &oversized);
    let current = matrix(&engram, false).await;
    assert!(current.contains(&format!("Policy source omitted/unresolved: `{TOPIC}`")));
    assert!(current.contains(&blake3::hash(oversized.as_bytes()).to_hex().to_string()));
    assert!(current.contains("Read the complete project-relative file"));
    assert!(!current.contains("OVERSIZED_POLICY_START"));
    assert!(
        !current.contains(EXCEPTION),
        "An excerpt must not replace an omitted complete policy"
    );

    let historical = matrix(&engram, true).await;
    assert!(historical.contains("historical_unavailable"));
    assert!(historical.contains("Live repository policy was withheld"));
    assert!(!historical.contains("Preserve accepted authorization boundaries."));
    assert!(!historical.contains(TOPIC));
    drop(engram);
    temp.close().unwrap();
}
