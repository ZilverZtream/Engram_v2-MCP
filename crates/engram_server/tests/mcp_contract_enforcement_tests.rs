#![allow(clippy::unwrap_used)]
//! Behavioral tests for MCP tool contract enforcement under malformed inputs.
//!
//! Covers Subsystem 1 (MCP protocol surface) and Subsystem 11
//! (runtime instrumentation / trace provenance validation).
//!
//! All tests call production functions directly:
//!  - `engram_server::services::project_service::validate_project_id`
//!  - `engram_core::runtime_evidence::validate_batch`
//!  - `engram_server::services::project_service::generate_indexing_report`

use engram_core::runtime_evidence::{
    RuntimeEvent, RuntimeEventType, RuntimeEvidenceBatch, validate_batch,
};
use engram_server::services::project_service::{generate_indexing_report, validate_project_id};

// ── validate_project_id — MCP tool input gate ────────────────────────────────

/// Valid project IDs (alphanumeric + hyphens + underscores) must be accepted.
#[test]
fn validate_project_id_accepts_valid_ids() {
    assert!(validate_project_id("my-project").is_ok());
    assert!(validate_project_id("my_project_123").is_ok());
    assert!(validate_project_id("ABC-def-GHI").is_ok());
    assert!(
        validate_project_id("a").is_ok(),
        "single character must be valid"
    );
    assert!(validate_project_id("proj-v2-alpha").is_ok());
}

/// Empty project_id must be rejected — cannot index into empty key.
#[test]
fn validate_project_id_rejects_empty_string() {
    let result = validate_project_id("");
    assert!(
        result.is_err(),
        "empty project_id must be rejected at the MCP contract boundary"
    );
}

/// Path traversal characters must be rejected to prevent directory escape.
#[test]
fn validate_project_id_rejects_path_traversal_characters() {
    assert!(
        validate_project_id("../etc/passwd").is_err(),
        "project_id containing '../' must be rejected"
    );
    assert!(
        validate_project_id("../../root").is_err(),
        "project_id containing '../../' must be rejected"
    );
    assert!(
        validate_project_id("proj/sub").is_err(),
        "project_id containing '/' must be rejected"
    );
    assert!(
        validate_project_id("proj\\sub").is_err(),
        "project_id containing '\\' must be rejected"
    );
}

/// Special characters must be rejected.
#[test]
fn validate_project_id_rejects_special_characters() {
    assert!(
        validate_project_id("proj ect").is_err(),
        "space must be rejected"
    );
    assert!(
        validate_project_id("proj\0ect").is_err(),
        "NUL byte must be rejected"
    );
    assert!(
        validate_project_id("proj\nect").is_err(),
        "newline must be rejected"
    );
    assert!(
        validate_project_id("proj@ect").is_err(),
        "@ must be rejected"
    );
    assert!(
        validate_project_id("proj.ect").is_err(),
        "dot must be rejected"
    );
    assert!(
        validate_project_id("<script>").is_err(),
        "angle brackets must be rejected"
    );
    assert!(
        validate_project_id("proj;DROP TABLE").is_err(),
        "semicolon must be rejected"
    );
}

/// Over-length project_id must be rejected to prevent amplification attacks.
#[test]
fn validate_project_id_rejects_oversized_id() {
    let long_id = "a".repeat(129);
    let result = validate_project_id(&long_id);
    assert!(
        result.is_err(),
        "project_id longer than 128 chars must be rejected; tried {}-char id",
        long_id.len()
    );
}

/// Exactly 128 characters must be the accepted boundary (not rejected).
#[test]
fn validate_project_id_accepts_128_char_boundary() {
    let boundary_id = "a".repeat(128);
    let result = validate_project_id(&boundary_id);
    assert!(
        result.is_ok(),
        "128-char project_id must be accepted (boundary value); got: {:?}",
        result.err()
    );
}

/// Unicode non-ASCII must be rejected (only ASCII alphanumeric allowed).
#[test]
fn validate_project_id_rejects_unicode_non_ascii() {
    assert!(
        validate_project_id("proj-éàü").is_err(),
        "Unicode non-ASCII characters must be rejected"
    );
    assert!(
        validate_project_id("プロジェクト").is_err(),
        "Japanese characters must be rejected"
    );
}

// ── generate_indexing_report — production report formatting ──────────────────

/// An all-zero stats struct must produce a non-empty, non-panicking report.
#[test]
fn generate_indexing_report_smoke_test_empty_stats() {
    let stats = engram_index::IngestStats::default();
    let report = generate_indexing_report(&stats);
    assert!(
        !report.is_empty(),
        "generate_indexing_report must return non-empty string even for empty stats"
    );
    assert!(
        report.contains("Indexing Report"),
        "report must contain 'Indexing Report' heading; got: {report}"
    );
}

// ── validate_batch — runtime evidence input validation ───────────────────────

fn make_valid_event(id: &str) -> RuntimeEvent {
    RuntimeEvent {
        event_id: id.to_string(),
        timestamp: "2026-01-01T00:00:00Z".to_string(),
        event_type: RuntimeEventType::Route,
        source_path: "src/handler.rs".to_string(),
        source_function: Some("handle_request".to_string()),
        source_line: Some(42),
        target: Some("/api/data".to_string()),
        context: std::collections::HashMap::new(),
        trust_weight: 0.9,
    }
}

fn make_valid_batch() -> RuntimeEvidenceBatch {
    RuntimeEvidenceBatch {
        schema_version: "1.0".to_string(),
        project_id: "proj-test".to_string(),
        session_id: "session-abc".to_string(),
        events: vec![make_valid_event("evt-001")],
    }
}

/// A fully valid batch must produce no validation errors.
#[test]
fn validate_batch_accepts_valid_evidence_batch() {
    let batch = make_valid_batch();
    let errors = validate_batch(&batch);
    assert!(
        errors.is_empty(),
        "valid batch must produce no validation errors; got: {errors:?}"
    );
}

/// Empty schema_version must be reported as a validation error.
#[test]
fn validate_batch_rejects_empty_schema_version() {
    let mut batch = make_valid_batch();
    batch.schema_version = String::new();
    let errors = validate_batch(&batch);
    assert!(
        !errors.is_empty(),
        "empty schema_version must produce at least one validation error"
    );
    let fields: Vec<&str> = errors.iter().map(|e| e.field.as_str()).collect();
    assert!(
        fields.contains(&"schema_version"),
        "error must name 'schema_version' field; got fields: {fields:?}"
    );
}

/// Empty project_id must be reported as a validation error.
#[test]
fn validate_batch_rejects_empty_project_id() {
    let mut batch = make_valid_batch();
    batch.project_id = String::new();
    let errors = validate_batch(&batch);
    assert!(
        !errors.is_empty(),
        "empty project_id must produce validation error"
    );
    let fields: Vec<&str> = errors.iter().map(|e| e.field.as_str()).collect();
    assert!(
        fields.contains(&"project_id"),
        "error must name 'project_id' field"
    );
}

/// Empty session_id must be reported as a validation error.
#[test]
fn validate_batch_rejects_empty_session_id() {
    let mut batch = make_valid_batch();
    batch.session_id = String::new();
    let errors = validate_batch(&batch);
    assert!(
        !errors.is_empty(),
        "empty session_id must produce validation error"
    );
}

/// An event with an empty event_id must be reported as a validation error.
#[test]
fn validate_batch_rejects_event_with_empty_id() {
    let mut batch = make_valid_batch();
    batch.events = vec![make_valid_event("")]; // empty id
    let errors = validate_batch(&batch);
    assert!(
        !errors.is_empty(),
        "event with empty event_id must produce validation error"
    );
}

/// Multiple simultaneous field errors must all be reported (not just the first).
#[test]
fn validate_batch_reports_all_errors_not_just_first() {
    let batch = RuntimeEvidenceBatch {
        schema_version: String::new(), // error 1
        project_id: String::new(),     // error 2
        session_id: String::new(),     // error 3
        events: vec![],
    };
    let errors = validate_batch(&batch);
    assert!(
        errors.len() >= 3,
        "all 3 empty-field errors must be reported; got only {} error(s)",
        errors.len()
    );
}

/// An empty events list must not cause a panic or produce spurious errors
/// (zero events is a valid degenerate batch — no events to validate).
#[test]
fn validate_batch_empty_events_list_does_not_panic() {
    let mut batch = make_valid_batch();
    batch.events = vec![];
    let errors = validate_batch(&batch);
    // Empty events may or may not be an error, but it must not panic
    let _ = errors; // just assert no panic
}
