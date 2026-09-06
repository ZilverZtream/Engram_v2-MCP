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
    CoverageClass, TargetStatus, ValidationCheck, ValidationCoverage, compute_validation_verdict,
    language_target_mismatch, strip_code_comments,
};
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;
use serde_json::json;

// ── helper-level (the verdict logic) ──────────────────────────────────────

fn chk(cat: &str, status: &str, class: CoverageClass) -> ValidationCheck {
    ValidationCheck::new(cat, status, class, vec![])
}

/// Coverage with `verified` project-derived checks; not a modify.
fn cov(verified: usize, target: TargetStatus) -> ValidationCoverage {
    ValidationCoverage {
        verified_checks: verified,
        assertion_checks: 0,
        generic_lint_checks: 0,
        target,
        change_kind_modify: false,
    }
}

#[test]
fn generic_lint_alone_is_insufficient_not_pass() {
    // A passing sync-hazard scan with NO contract verified is the exact bug:
    // it must be INSUFFICIENT, never PASS.
    let checks = vec![chk("sync_hazards", "pass", CoverageClass::GenericLint)];
    assert_eq!(
        compute_validation_verdict(&checks, &cov(0, TargetStatus::Unspecified)),
        "INSUFFICIENT"
    );
}

#[test]
fn caller_assertion_alone_is_insufficient_not_pass() {
    // Round-8 P0-1: a green check of the CALLER'S OWN asserted strings (session
    // keys / expected tables) is not project coverage — INSUFFICIENT, not PASS.
    let checks = vec![
        chk("session_keys", "pass", CoverageClass::AssertionOnly),
        chk("vb_traps", "pass", CoverageClass::GenericLint),
    ];
    assert_eq!(
        compute_validation_verdict(&checks, &cov(0, TargetStatus::Exists)),
        "INSUFFICIENT"
    );
}

#[test]
fn target_file_existence_alone_is_not_contract_coverage() {
    // Confirming the file exists is not verifying the code — still INSUFFICIENT.
    let checks = vec![
        chk("sync_hazards", "pass", CoverageClass::GenericLint),
        chk("target_file", "pass", CoverageClass::Meta),
    ];
    assert_eq!(
        compute_validation_verdict(&checks, &cov(0, TargetStatus::Exists)),
        "INSUFFICIENT"
    );
}

#[test]
fn a_verified_contract_can_pass() {
    let checks = vec![
        chk("sql_tables", "pass", CoverageClass::Verified),
        chk("sync_hazards", "pass", CoverageClass::GenericLint),
    ];
    assert_eq!(
        compute_validation_verdict(&checks, &cov(1, TargetStatus::Exists)),
        "PASS"
    );
}

#[test]
fn modify_without_exact_target_is_insufficient() {
    // Round-8 P0-1: a modification can only be verified against the existing
    // file; without an EXACT target in the index there is nothing to check.
    let checks = vec![chk("sql_tables", "pass", CoverageClass::Verified)];
    let mut c = cov(1, TargetStatus::Unspecified);
    c.change_kind_modify = true;
    assert_eq!(compute_validation_verdict(&checks, &c), "INSUFFICIENT");
}

#[test]
fn any_fail_fails_and_provider_failure_is_insufficient() {
    assert_eq!(
        compute_validation_verdict(
            &[chk("sql_tables", "fail", CoverageClass::Verified)],
            &cov(1, TargetStatus::Exists)
        ),
        "FAIL"
    );
    assert_eq!(
        compute_validation_verdict(
            &[chk("sql_tables", "pass", CoverageClass::Verified)],
            &cov(1, TargetStatus::ProviderFailed)
        ),
        "INSUFFICIENT"
    );
}

#[test]
fn language_target_mismatch_is_detected() {
    // C# code aimed at a .vb file (and vice-versa) is a mismatch; matching or
    // shared extensions are not.
    assert!(language_target_mismatch("csharp", Some("Site/orders.vb")).is_some());
    assert!(language_target_mismatch("vb", Some("Site/orders.cs")).is_some());
    assert!(language_target_mismatch("csharp", Some("Site/orders.cs")).is_none());
    assert!(language_target_mismatch("vb", Some("Site/orders.vb")).is_none());
    assert!(language_target_mismatch("vb", Some("Site/page.aspx")).is_none()); // shared
    assert!(language_target_mismatch("csharp", None).is_none());
}

#[test]
fn comment_stripping_removes_comment_only_tokens() {
    // A token present ONLY in a comment must not survive the strip.
    assert!(!strip_code_comments("// audit_probe_key", false).contains("audit_probe_key"));
    assert!(!strip_code_comments("' audit_probe_key", true).contains("audit_probe_key"));
    // A token in real code (or a string literal) survives.
    assert!(strip_code_comments("var x = audit_probe_key;", false).contains("audit_probe_key"));
    assert!(
        strip_code_comments("Session(\"audit_probe_key\") = 1", true).contains("audit_probe_key")
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handler_bare_name_containment_is_not_caller_compatibility_pass() {
    // P0-2 (round-7): supplying original_method_name must NOT earn contract
    // coverage from a mere substring presence. With no target and no resolved
    // method, `class X {}` + original_method_name "X" previously returned PASS
    // ("Method name 'X' preserved"). That is unearned success — it must be
    // INSUFFICIENT, never PASS.
    let (_t, _s, engram, pid) = fixture().await;
    let out = validate(
        &engram,
        json!({"project_id": pid, "code": "class X {}", "language": "csharp",
               "original_method_name": "X", "output_json": true}),
    )
    .await;
    assert!(
        out.contains("INSUFFICIENT"),
        "substring name-presence with no resolved method must be INSUFFICIENT:\n{out}"
    );
    assert!(
        !out.contains("\"overall_verdict\": \"PASS\""),
        "must not PASS on lexical name containment:\n{out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handler_exact_existing_modify_target_is_not_rejected() {
    // P0-1 (round-7): a REAL indexed file, given by its exact relative path,
    // must resolve as existing — never be rejected as "not in the indexed
    // project". File nodes key on `file:{path}` with Node.name = basename, so a
    // substring query on the full path can never match; identity must resolve
    // through the file node id (with a basename+exact-path fallback).
    // Round-8 P0-1: VB code for the .vb target — the previous version used C#
    // code for a .vb file, which now (correctly) fails on language mismatch and
    // masked whether target resolution itself worked.
    let (_t, _s, engram, pid) = fixture().await;
    let out = validate(
        &engram,
        json!({"project_id": pid, "code": "Public Class orders\n  Public Function GetAll() As String\n    Return \"x\"\n  End Function\nEnd Class\n",
               "language": "vb",
               "target_file": "Site/orders.vb", "change_kind": "modify", "output_json": true}),
    )
    .await;
    assert!(
        !out.contains("not in the indexed project"),
        "an exact existing file must NOT be rejected as nonexistent:\n{out}"
    );
    assert!(
        !out.contains("\"status\": \"fail\""),
        "no FAIL for a real existing modify target with matching-language code:\n{out}"
    );
}

// ── Round-8 P0-1: the three live false-PASS reproductions ─────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handler_arbitrary_vb_with_only_generic_lint_is_insufficient() {
    // Live repro 1: arbitrary VB text at an existing .vb target. Only the
    // generic VB-trap lint runs — no PROJECT contract is verified. Must be
    // INSUFFICIENT, never PASS.
    let (_t, _s, engram, pid) = fixture().await;
    let out = validate(
        &engram,
        json!({"project_id": pid, "code": "arbitrary generated text\n", "language": "vb",
               "target_file": "Site/orders.vb", "change_kind": "modify", "output_json": true}),
    )
    .await;
    assert!(
        out.contains("INSUFFICIENT"),
        "generic VB lint alone must not PASS:\n{out}"
    );
    assert!(!out.contains("\"overall_verdict\": \"PASS\""), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handler_comment_only_session_key_is_insufficient() {
    // Live repro 2: a session key that appears ONLY in a comment must not count
    // as handled, so the only check is a caller assertion → INSUFFICIENT.
    let (_t, _s, engram, pid) = fixture().await;
    let out = validate(
        &engram,
        json!({"project_id": pid, "code": "// audit_probe_key\n", "language": "csharp",
               "expected_session_keys": ["audit_probe_key"], "output_json": true}),
    )
    .await;
    assert!(
        out.contains("INSUFFICIENT"),
        "a session key present only in a comment must not earn PASS:\n{out}"
    );
    assert!(!out.contains("\"overall_verdict\": \"PASS\""), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handler_language_target_extension_mismatch_fails() {
    // Live repro 3: C# code aimed at a .vb file. The content cannot be valid for
    // that target — FAIL, never PASS.
    let (_t, _s, engram, pid) = fixture().await;
    let out = validate(
        &engram,
        json!({"project_id": pid, "code": "// audit_probe_key\n", "language": "csharp",
               "target_file": "Site/orders.vb", "change_kind": "modify",
               "expected_session_keys": ["audit_probe_key"], "output_json": true}),
    )
    .await;
    assert!(
        out.contains("FAIL"),
        "language/target mismatch must FAIL:\n{out}"
    );
    assert!(!out.contains("\"overall_verdict\": \"PASS\""), "{out}");
}
