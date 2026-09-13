#![allow(clippy::unwrap_used)]

use engram_core::config::Config;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;
use serde_json::json;
use sha2::{Digest, Sha256};

fn sha256(bytes: &[u8]) -> String {
    format!("{:X}", Sha256::digest(bytes))
}

async fn fixture() -> (tempfile::TempDir, Engram, String, std::path::PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("Model.schema"), "entity Order\n").unwrap();
    std::fs::write(root.join("Generated.vb"), "Public Class Order\nEnd Class\n").unwrap();
    let config = Config {
        allowed_roots: vec![root.clone()],
        data_dir: temp.path().join("data"),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let (state, _) = AppState::new(config).unwrap();
    let engram = Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().into_owned(),
            project_name: "GeneratorReceiptFixture".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsVb,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let project_id = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    (temp, engram, project_id, root)
}

fn write_receipt(root: &std::path::Path, invoked: bool) -> String {
    let source = root.join("Model.schema");
    let target = root.join("Generated.vb");
    let source_bytes = std::fs::read(&source).unwrap();
    let target_bytes = std::fs::read(&target).unwrap();
    let receipt = json!({
        "SourceFile": source,
        "CustomTool": "FixtureGenerator",
        "ProjectName": "Fixture",
        "SolutionFile": root.join("Fixture.sln"),
        "VisualStudioVersion": "test-host-1",
        "Invoked": invoked,
        "Files": [
            {
                "Path": source,
                "ExistedBefore": true,
                "ExistsAfter": true,
                "LengthBefore": source_bytes.len(),
                "LengthAfter": source_bytes.len(),
                "Sha256Before": sha256(&source_bytes),
                "Sha256After": sha256(&source_bytes),
                "Changed": false
            },
            {
                "Path": target,
                "ExistedBefore": false,
                "ExistsAfter": true,
                "LengthBefore": 0,
                "LengthAfter": target_bytes.len(),
                "Sha256Before": null,
                "Sha256After": sha256(&target_bytes),
                "Changed": true
            }
        ]
    });
    // Exercise the Windows-hosted receipt shape used by IDE command buses.
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend(serde_json::to_vec_pretty(&receipt).unwrap());
    std::fs::write(root.join("generator-receipt.json"), &bytes).unwrap();
    sha256(&bytes)
}

async fn validate(engram: &Engram, value: serde_json::Value) -> Result<String, String> {
    let request = serde_json::from_value(value).map_err(|error| error.to_string())?;
    engram
        .handle_validate_generated_code(request)
        .await
        .map(|result| result.content[0].as_text().unwrap().text.clone())
        .map_err(|error| error.message.to_string())
}

fn request(project_id: &str, root: &std::path::Path, receipt_sha256: &str) -> serde_json::Value {
    let code = std::fs::read(root.join("Generated.vb")).unwrap();
    json!({
        "project_id": project_id,
        "code_file": "Generated.vb",
        "code_file_blake3": blake3::hash(&code).to_hex().to_string(),
        "target_file": "Generated.vb",
        "language": "vb",
        "generator_receipt_file": "generator-receipt.json",
        "generator_receipt_sha256": receipt_sha256,
        "output_json": true
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_pascal_case_receipt_earns_hash_bound_project_coverage() {
    let (_temp, engram, project_id, root) = fixture().await;
    let receipt_sha256 = write_receipt(&root, true);
    let output = validate(&engram, request(&project_id, &root, &receipt_sha256))
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(value["overall_verdict"], "PASS", "{value}");
    assert_eq!(value["coverage"]["verified_checks"], 1, "{value}");
    assert!(output.contains("generator_provenance"), "{output}");
    assert!(output.contains("FixtureGenerator"), "{output}");
    assert!(output.contains("test-host-1"), "{output}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn current_output_mismatch_fails_instead_of_replaying_stale_success() {
    let (_temp, engram, project_id, root) = fixture().await;
    let receipt_sha256 = write_receipt(&root, true);
    std::fs::write(
        root.join("Generated.vb"),
        "Public Class Tampered\nEnd Class\n",
    )
    .unwrap();
    let output = validate(&engram, request(&project_id, &root, &receipt_sha256))
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(value["overall_verdict"], "FAIL", "{value}");
    assert!(output.contains("does not match the receipt"), "{output}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn not_invoked_receipt_fails_and_inline_code_cannot_claim_provenance() {
    let (_temp, engram, project_id, root) = fixture().await;
    let receipt_sha256 = write_receipt(&root, false);
    let output = validate(&engram, request(&project_id, &root, &receipt_sha256))
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(value["overall_verdict"], "FAIL", "{value}");
    assert!(output.contains("generator was not invoked"), "{output}");

    let error = validate(
        &engram,
        json!({
            "project_id": project_id,
            "code": "Public Class Order\nEnd Class",
            "target_file": "Generated.vb",
            "language": "vb",
            "generator_receipt_file": "generator-receipt.json",
            "generator_receipt_sha256": receipt_sha256,
            "output_json": true
        }),
    )
    .await
    .unwrap_err();
    assert!(error.contains("requires hash-bound code_file"), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receipt_hash_and_project_boundary_are_enforced() {
    let (_temp, engram, project_id, root) = fixture().await;
    let receipt_sha256 = write_receipt(&root, true);
    let mut wrong_hash = request(&project_id, &root, &"0".repeat(64));
    let error = validate(&engram, wrong_hash.take()).await.unwrap_err();
    assert!(error.contains("receipt SHA-256 mismatch"), "{error}");

    let outside = tempfile::NamedTempFile::new().unwrap();
    let mut absolute = request(&project_id, &root, &receipt_sha256);
    absolute["generator_receipt_file"] = json!(outside.path());
    let error = validate(&engram, absolute).await.unwrap_err();
    assert!(error.contains("must be project-relative"), "{error}");

    let mut traversal = request(&project_id, &root, &receipt_sha256);
    traversal["generator_receipt_file"] = json!("../generator-receipt.json");
    let error = validate(&engram, traversal).await.unwrap_err();
    assert!(
        error.contains("escapes") || error.contains("resolve") || error.contains("traversal"),
        "{error}"
    );
}
