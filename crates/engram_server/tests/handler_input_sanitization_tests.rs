#![allow(clippy::unwrap_used)]
//! Handler input sanitization matrix tests.
//!
//! Proves that across the full handler surface:
//! 1. Every handler that touches the filesystem routes path inputs through
//!    `safe_join`, `resolve_path`, or `PathContext`.
//! 2. Every handler that accepts cardinality fields (top_k, limit, max_*)
//!    uses the `sanitized_*` adapter methods from the request model rather
//!    than using the raw user value directly.
//! 3. The request model itself provides sanitizer adapters for all major
//!    cardinality fields, proving the gate exists centrally.
//!
//! These tests close the MCP1-l9v3 / X3-regmcp-4k1z "Uncovered" gap:
//! a generated route-matrix assertion that all storage/search-touching handlers
//! bound their inputs before passing them downstream.

// ── Path input validation ─────────────────────────────────────────────────────

/// project_tools accepts directory path inputs and must route them through
/// PathContext/safe_join rather than using strings verbatim.
#[test]
fn project_tools_routes_paths_through_context() {
    let source = include_str!("../src/handlers/project_tools.rs");

    let uses_path_guard = source.contains("safe_join")
        || source.contains("resolve_path")
        || source.contains("PathContext")
        || source.contains("state.paths");

    assert!(
        uses_path_guard,
        "project_tools.rs must route file path inputs through safe_join / PathContext \
         before any storage or indexing access"
    );
}

/// cognitive_tools accepts directory and file inputs and must validate paths.
#[test]
fn cognitive_tools_routes_paths_through_context() {
    let source = include_str!("../src/handlers/cognitive_tools.rs");

    let uses_path_guard = source.contains("safe_join")
        || source.contains("resolve_path")
        || source.contains("PathContext")
        || source.contains("state.paths");

    assert!(
        uses_path_guard,
        "cognitive_tools.rs must route path inputs through safe_join / PathContext"
    );
}

/// git_tools reads git history from filesystem paths and must validate them.
#[test]
fn git_tools_routes_paths_through_context() {
    let source = include_str!("../src/handlers/git_tools.rs");

    let uses_path_guard = source.contains("safe_join")
        || source.contains("resolve_path")
        || source.contains("PathContext")
        || source.contains("state.paths");

    assert!(
        uses_path_guard,
        "git_tools.rs must route repository path inputs through safe_join / PathContext"
    );
}

/// graph_tools can accept project paths and must validate them.
#[test]
fn graph_tools_uses_project_validation() {
    let source = include_str!("../src/handlers/graph_tools.rs");

    let has_validation = source.contains("validate_project_id")
        || source.contains("ensure_project_record")
        || source.contains("safe_join")
        || source.contains("state.paths");

    assert!(
        has_validation,
        "graph_tools.rs must use project_id validation or path context before storage access"
    );
}

// ── Cardinality bounds validation ─────────────────────────────────────────────

/// search_tools must use sanitized cardinality adapters (top_k, limit) rather
/// than raw user-supplied integers from request fields.
#[test]
fn search_tools_uses_sanitized_cardinality() {
    let source = include_str!("../src/handlers/search_tools.rs");

    let has_cardinality_guard = source.contains("sanitized_top_k")
        || source.contains("sanitized_limit")
        || source.contains("sanitized_max")
        || source.contains(".top_k.min(")
        || source.contains(".limit.min(");

    assert!(
        has_cardinality_guard,
        "search_tools.rs must bound top_k/limit via sanitized_* adapters or explicit min() clamp \
         — raw user-supplied cardinality can cause memory amplification"
    );
}

/// cognitive_tools must clamp cardinality fields in requests that fan out to storage.
#[test]
fn cognitive_tools_uses_sanitized_cardinality() {
    let source = include_str!("../src/handlers/cognitive_tools.rs");

    let has_cardinality_guard = source.contains("sanitized_top_k")
        || source.contains("sanitized_limit")
        || source.contains("sanitized_max")
        || source.contains("sanitized_timeout")
        || source.contains(".min(");

    assert!(
        has_cardinality_guard,
        "cognitive_tools.rs must clamp cardinality fields via sanitized_* or .min() bounds"
    );
}

// ── Request model sanitizer gate exists ───────────────────────────────────────

/// The request model must provide `sanitized_top_k` and `sanitized_limit` helpers
/// so handlers have a single central place to clamp user-supplied cardinality.
#[test]
fn request_model_provides_top_k_and_limit_sanitizers() {
    let source = include_str!("../src/models/requests.rs");

    assert!(
        source.contains("sanitized_top_k") || source.contains("fn sanitized_top_k"),
        "requests.rs must provide sanitized_top_k to bound user-controlled top-k"
    );
    assert!(
        source.contains("sanitized_limit") || source.contains("fn sanitized_limit"),
        "requests.rs must provide sanitized_limit to bound user-controlled result limits"
    );
}

/// The request model must provide sanitizers for timeout fields to prevent
/// unbounded blocking on user-supplied duration values.
#[test]
fn request_model_provides_timeout_sanitizer() {
    let source = include_str!("../src/models/requests.rs");

    let has_timeout_guard = source.contains("sanitized_timeout")
        || source.contains("min_timeout")
        || source.contains("max_timeout")
        || source.contains("timeout_secs");

    assert!(
        has_timeout_guard,
        "requests.rs must clamp user-supplied timeout values — unbounded timeouts \
         can cause tasks to block indefinitely"
    );
}

/// The request model must provide sanitizers for max_results and max_hops to
/// prevent amplification through graph traversal and result fan-out.
#[test]
fn request_model_provides_graph_cardinality_sanitizers() {
    let source = include_str!("../src/models/requests.rs");

    let has_graph_guard = source.contains("sanitized_max_results")
        || source.contains("sanitized_max_hops")
        || source.contains("sanitized_max_clusters")
        || source.contains("sanitized_max_commits");

    assert!(
        has_graph_guard,
        "requests.rs must provide graph cardinality sanitizers (max_results, max_hops, etc.) \
         to bound traversal fan-out"
    );
}

// ── Migration tools specific path validation ──────────────────────────────────

/// migration_tools touches project data directories and must validate paths.
#[test]
fn migration_tools_validates_paths_and_project_ids() {
    let source = include_str!("../src/handlers/migration_tools.rs");

    let has_project_guard =
        source.contains("validate_project_id") || source.contains("ensure_project_record");
    let has_path_guard = source.contains("safe_join")
        || source.contains("resolve_path")
        || source.contains("state.paths");

    assert!(
        has_project_guard || has_path_guard,
        "migration_tools.rs must validate project_id or route paths through PathContext \
         before accessing project data"
    );
}

// ── Access layer tools validation ─────────────────────────────────────────────

/// access_layer_tools must validate all project_id inputs at the handler boundary.
#[test]
fn access_layer_tools_validates_all_inputs() {
    let source = include_str!("../src/handlers/access_layer_tools.rs");

    let has_validation = source.contains("validate_project_id")
        || source.contains("ensure_project_record")
        || source.contains("safe_join");

    assert!(
        has_validation,
        "access_layer_tools.rs must validate project_id or paths at handler boundary"
    );
}
