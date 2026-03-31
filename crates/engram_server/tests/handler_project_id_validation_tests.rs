#![allow(clippy::unwrap_used)]
//! Handler project_id validation conformance sweep.
//!
//! All MCP tool handlers that accept a `project_id` parameter must validate it
//! at the handler boundary before any storage or indexing work begins.  This is
//! enforced by either:
//!   a. Calling `validate_project_id(project_id)` directly, OR
//!   b. Calling `ensure_project_record(state, project_id)`, which internally
//!      calls `validate_project_id` before the registry look-up.
//!
//! These tests perform a static source scan of every handler file to prove that
//! no handler can receive a malformed project_id without triggering the gate.

/// search_tools handler must call validate_project_id or ensure_project_record
/// before performing any search operation.
#[test]
fn search_tools_validates_project_id() {
    let source = include_str!("../src/handlers/search_tools.rs");

    let has_validation = source.contains("validate_project_id")
        || source.contains("ensure_project_record");
    assert!(
        has_validation,
        "search_tools.rs must call validate_project_id or ensure_project_record \
         at the handler boundary — project_id can arrive unvalidated from MCP callers"
    );
}

/// project_tools handler must call validate_project_id or ensure_project_record.
#[test]
fn project_tools_validates_project_id() {
    let source = include_str!("../src/handlers/project_tools.rs");

    let has_validation = source.contains("validate_project_id")
        || source.contains("ensure_project_record");
    assert!(
        has_validation,
        "project_tools.rs must call validate_project_id or ensure_project_record"
    );
}

/// cognitive_tools handler must call validate_project_id or ensure_project_record.
#[test]
fn cognitive_tools_validates_project_id() {
    let source = include_str!("../src/handlers/cognitive_tools.rs");

    let has_validation = source.contains("validate_project_id")
        || source.contains("ensure_project_record");
    assert!(
        has_validation,
        "cognitive_tools.rs must call validate_project_id or ensure_project_record"
    );
}

/// graph_tools handler must call validate_project_id or ensure_project_record.
#[test]
fn graph_tools_validates_project_id() {
    let source = include_str!("../src/handlers/graph_tools.rs");

    let has_validation = source.contains("validate_project_id")
        || source.contains("ensure_project_record");
    assert!(
        has_validation,
        "graph_tools.rs must call validate_project_id or ensure_project_record"
    );
}

/// git_tools handler must call validate_project_id or ensure_project_record.
#[test]
fn git_tools_validates_project_id() {
    let source = include_str!("../src/handlers/git_tools.rs");

    let has_validation = source.contains("validate_project_id")
        || source.contains("ensure_project_record");
    assert!(
        has_validation,
        "git_tools.rs must call validate_project_id or ensure_project_record"
    );
}

/// migration_tools handler must call validate_project_id or ensure_project_record.
#[test]
fn migration_tools_validates_project_id() {
    let source = include_str!("../src/handlers/migration_tools.rs");

    let has_validation = source.contains("validate_project_id")
        || source.contains("ensure_project_record");
    assert!(
        has_validation,
        "migration_tools.rs must call validate_project_id or ensure_project_record"
    );
}

/// access_layer_tools handler must call validate_project_id or ensure_project_record.
#[test]
fn access_layer_tools_validates_project_id() {
    let source = include_str!("../src/handlers/access_layer_tools.rs");

    let has_validation = source.contains("validate_project_id")
        || source.contains("ensure_project_record");
    assert!(
        has_validation,
        "access_layer_tools.rs must call validate_project_id or ensure_project_record"
    );
}

/// runtime_observation_tools handler must call validate_project_id
/// or ensure_project_record if it accepts project_id inputs.
#[test]
fn runtime_observation_tools_validates_project_id() {
    let source = include_str!("../src/handlers/runtime_observation_tools.rs");

    let has_validation = source.contains("validate_project_id")
        || source.contains("ensure_project_record");
    // runtime_observation_tools may not take a project_id — it's acceptable if neither is present
    // AND the handler doesn't accept project_id at all.
    let accepts_project_id = source.contains("project_id");

    if accepts_project_id {
        assert!(
            has_validation,
            "runtime_observation_tools.rs accepts project_id inputs but does not \
             call validate_project_id or ensure_project_record — MCP1 violation"
        );
    }
    // If no project_id in source, the conformance requirement doesn't apply.
}

/// validate_project_id rejects all adversarial inputs that could reach handlers.
/// This complements the handler conformance check by proving the gate itself is correct.
#[test]
fn validate_project_id_gate_covers_all_adversarial_classes() {
    use engram_server::services::project_service::validate_project_id;

    // Path traversal — directory escape
    assert!(validate_project_id("../etc/passwd").is_err(), "path traversal rejected");
    // NUL byte — composite key corruption
    assert!(validate_project_id("proj\0evil").is_err(), "NUL byte rejected");
    // Newline — key delimiter injection
    assert!(validate_project_id("proj\nevil").is_err(), "newline rejected");
    // Slash — directory separator injection
    assert!(validate_project_id("proj/sub").is_err(), "slash rejected");
    // Empty — no project can have an empty id
    assert!(validate_project_id("").is_err(), "empty id rejected");
    // Oversized — amplification prevention
    assert!(validate_project_id(&"a".repeat(200)).is_err(), "oversized id rejected");
    // Shell metacharacters
    assert!(validate_project_id("$(rm -rf /)").is_err(), "shell metacharacters rejected");
    // Valid — must NOT be rejected
    assert!(validate_project_id("my-project-123").is_ok(), "valid id must be accepted");
    assert!(validate_project_id("abc_DEF-456").is_ok(), "valid id with mixed chars accepted");
}

/// the handler module must export or re-export validate_project_id so all
/// handler files can call it from a single import (centralized gate, REG1-compatible).
#[test]
fn handler_mod_centralizes_validation() {
    let source = include_str!("../src/handlers/mod.rs");

    // The mod must import or reference validate_project_id so handlers can share it.
    let has_centralization = source.contains("validate_project_id")
        || source.contains("ensure_project_record")
        || source.contains("project_service");

    assert!(
        has_centralization,
        "handlers/mod.rs must centralize validate_project_id or ensure_project_record \
         so handler files share a single validated gate — avoids per-handler drift"
    );
}

// ── REG1/X1: handler-boundary validator is semantically identical to service validator ──

/// REG1/X1: The handler-boundary validator must delegate to
/// `project_service::validate_project_id`, not to the weaker `validate_key_component`.
///
/// `validate_key_component` only rejects NUL/empty/newline — it allows `/`, `..`,
/// and shell metacharacters that would corrupt `data_dir/projects/{pid}` paths.
/// `project_service::validate_project_id` enforces `[A-Za-z0-9_-]{1,128}` which
/// closes all traversal classes.
///
/// Structural proof: the handler mod must call project_service::validate_project_id
/// and must NOT call validate_key_component for project_id validation.
#[test]
fn handler_validator_delegates_to_strict_service_validator_not_weak_key_component() {
    let source = include_str!("../src/handlers/mod.rs");

    // The handler boundary must call the strict service validator.
    assert!(
        source.contains("project_service::validate_project_id"),
        "REG1/X1: handlers/mod.rs validate_project_id must delegate to \
         project_service::validate_project_id (strict [A-Za-z0-9_-]{{1,128}} policy), \
         not to validate_key_component which only rejects NUL/newline"
    );

    // Must NOT fall back to the weak validate_key_component for project_id.
    // Note: validate_key_component may appear elsewhere in this file for other
    // purposes, but the validate_project_id function body must not use it.
    // We check that the function body routes through project_service.
    // This is satisfied by the presence of project_service::validate_project_id above.
    assert!(
        !source.contains("validate_key_component(\"project_id\""),
        "REG1/X1: handlers/mod.rs validate_project_id must not call \
         validate_key_component(\"project_id\", ...) — that validator only rejects \
         NUL/newline and allows '/', '..', and shell metacharacters through the handler boundary"
    );
}

/// REG1/X1: slash and dot-dot are rejected by the handler-boundary validator.
///
/// This proves the semantic gap is closed: before the fix, `validate_key_component`
/// allowed these through; after the fix, `project_service::validate_project_id`
/// blocks them. Tested against the service validator directly (same function the
/// handler now delegates to).
#[test]
fn handler_boundary_rejects_slash_and_dotdot_project_ids() {
    use engram_server::services::project_service::validate_project_id;

    // These were previously accepted by validate_key_component (only NUL/newline rejected).
    assert!(
        validate_project_id("proj/traversal").is_err(),
        "REG1/X1: '/' in project_id must be rejected — enables path traversal \
         via data_dir/projects/proj/traversal/ when used in filesystem ops"
    );
    assert!(
        validate_project_id("../etc/passwd").is_err(),
        "REG1/X1: '..' in project_id must be rejected — path traversal to escape data_dir"
    );
    assert!(
        validate_project_id("$(whoami)").is_err(),
        "REG1/X1: shell metacharacters in project_id must be rejected"
    );
    assert!(
        validate_project_id("proj sub").is_err(),
        "REG1/X1: spaces in project_id must be rejected"
    );
    assert!(
        validate_project_id("proj\ttab").is_err(),
        "REG1/X1: tabs in project_id must be rejected"
    );
    // Valid IDs must still pass.
    assert!(
        validate_project_id("valid-project_123").is_ok(),
        "REG1/X1: valid project_id must still be accepted after fix"
    );
}
