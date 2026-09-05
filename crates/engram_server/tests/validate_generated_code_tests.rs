#![allow(clippy::unwrap_used)]
//! Round-6: validate_generated_code must not PASS without verifying the
//! project's contract.
//!
//! Round-5's fix was unreachable: the always-on sync-hazard check meant the
//! `checks` list was never empty during normal execution, so hazard-free
//! `class X {}` still returned PASS, and the regression test only exercised
//! the extracted helper with a state the handler could not produce. These
//! tests drive the REAL handler with real request objects.

use engram_core::config::Config;
use engram_server::handlers::access_layer_tools::{
    TargetStatus, ValidationCheck, ValidationCoverage, compute_validation_verdict,
};
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;
use serde_json::json;

// ── helper-level (the verdict logic) ──────────────────────────────────────

fn chk(cat: &str, status: &str) -> ValidationCheck {
    ValidationCheck {
        category: cat.to_string(),
        status: status.to_string(),
        details: vec![],
    }
}

fn cov(contract: usize, target: TargetStatus) -> ValidationCoverage {
    ValidationCoverage {
        contract_checks_ran: contract,
        target,
    }
}

#[test]
fn generic_lint_alone_is_insufficient_not_pass() {
    // A passing sync-hazard scan with NO contract verified is the exact bug:
    // it must be INSUFFICIENT, never PASS.
    let checks = vec![chk("sync_hazards", "pass")];
    assert_eq!(
        compute_validation_verdict(&checks, &cov(0, TargetStatus::Unspecified)),
        "INSUFFICIENT"
    );
}

#[test]
fn target_file_existence_alone_is_not_contract_coverage() {
    // Confirming the file exists is not verifying the code — still INSUFFICIENT.
    let checks = vec![chk("sync_hazards", "pass"), chk("target_file", "pass")];
    assert_eq!(
        compute_validation_verdict(&checks, &cov(0, TargetStatus::Exists)),
        "INSUFFICIENT"
    );
}

#[test]
fn a_verified_contract_can_pass() {
    let checks = vec![chk("sql_tables", "pass"), chk("sync_hazards", "pass")];
    assert_eq!(
        compute_validation_verdict(&checks, &cov(1, TargetStatus::Exists)),
        "PASS"
    );
}

#[test]
fn any_fail_fails_and_provider_failure_is_insufficient() {
    assert_eq!(
        compute_validation_verdict(&[chk("sql_tables", "fail")], &cov(1, TargetStatus::Exists)),
        "FAIL"
    );
    assert_eq!(
        compute_validation_verdict(
            &[chk("sql_tables", "pass")],
            &cov(1, TargetStatus::ProviderFailed)
        ),
        "INSUFFICIENT"
    );
}

// ── handler-level (the real path the auditor exposed) ─────────────────────

async fn fixture() -> (tempfile::TempDir, AppState, Engram, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(root.join("Site")).unwrap();
    std::fs::write(
        root.join("Site/orders.vb"),
        "Public Class orders\n    Public Function GetAll() As String\n        Dim q = From r In db.orders Select r\n        Return \"x\"\n    End Function\nEnd Class\n",
    )
    .unwrap();
    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: tmp.path().join("data"),
        max_project_files: Some(20),
        max_project_bytes: Some(512 * 1024),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "ValidateFixture".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsVb,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    (tmp, state, engram, pid)
}

async fn validate(engram: &Engram, v: serde_json::Value) -> String {
    let req = serde_json::from_value(v).unwrap();
    let res = engram.handle_validate_generated_code(req).await.unwrap();
    res.content[0].as_text().unwrap().text.clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handler_bare_code_with_no_contract_is_insufficient_not_pass() {
    // The exact auditor scenario: class X {} with nothing else.
    let (_t, _s, engram, pid) = fixture().await;
    let out = validate(
        &engram,
        json!({"project_id": pid, "code": "class X {}", "language": "csharp", "output_json": true}),
    )
    .await;
    assert!(
        out.contains("INSUFFICIENT"),
        "bare code with no contract must be INSUFFICIENT, not PASS:\n{out}"
    );
    assert!(!out.contains("\"overall_verdict\": \"PASS\""), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handler_nonexistent_modify_target_fails() {
    let (_t, _s, engram, pid) = fixture().await;
    let out = validate(
        &engram,
        json!({"project_id": pid, "code": "class X {}", "language": "csharp",
               "target_file": "Site/DOES-NOT-EXIST.cs", "change_kind": "modify", "output_json": true}),
    )
    .await;
    assert!(
        out.contains("FAIL"),
        "a modify against a nonexistent target must FAIL:\n{out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handler_new_file_create_is_not_failed_for_absence() {
    // Legitimate new-file generation: absence from the index is expected.
    let (_t, _s, engram, pid) = fixture().await;
    let out = validate(
        &engram,
        json!({"project_id": pid, "code": "class X {}", "language": "csharp",
               "target_file": "Site/BrandNew.cs", "change_kind": "create", "output_json": true}),
    )
    .await;
    // Not a FAIL for absence; still INSUFFICIENT because no contract was given.
    assert!(
        !out.contains("is not in the indexed project"),
        "a create target must not be failed for absence:\n{out}"
    );
    assert!(out.contains("INSUFFICIENT"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handler_substring_target_does_not_satisfy_existence() {
    // "orders" is a substring of the real "Site/orders.vb"; an exact-match
    // resolver must NOT treat a fragment as the existing file.
    let (_t, _s, engram, pid) = fixture().await;
    let out = validate(
        &engram,
        json!({"project_id": pid, "code": "class X {}", "language": "csharp",
               "target_file": "orders", "change_kind": "modify", "output_json": true}),
    )
    .await;
    assert!(
        out.contains("FAIL") && out.contains("not in the indexed project"),
        "a path fragment must not satisfy exact existence:\n{out}"
    );
}
