pub mod access_layer_tools;
pub mod ask_tools;
pub mod code_review_tools;
pub mod cognitive_tools;
pub mod explain_change_tools;
pub mod git_tools;
pub mod graph_tools;
pub mod grep_tools;
pub mod migration_tools;
pub mod planning_tools;
pub mod pr_history_tools;
pub mod project_tools;
pub mod quality_gate_tools;
pub mod review_tools;
pub mod search_tools;
pub mod settings_tools;
pub mod support_kb_tools;

pub mod runtime_observation_tools;

/// Result cap for UNFILTERED full-graph node scans. 50k silently truncated
/// on OciusX gen-3 (the settings-store extraction grew the graph past it) —
/// recall loss on the catalog/footprint/guards tools. query_nodes scans the
/// whole table regardless once no filter early-exits, so a higher cap costs
/// only the extra deserialization of rows that were previously dropped.
pub(crate) const NODE_SCAN_LIMIT: usize = 200_000;

/// Edge kinds that mean "someone calls this method". Call edges arrive as
/// `EdgeKind::Calls` from the Roslyn/raw call-graph path and as
/// `EdgeKind::Dependency` from the heuristic extractors — a caller count
/// that queries only one kind is blind to the other. (Dependency-only
/// counting hid all 51k recovered Calls edges: get_method_info,
/// find_dead_methods, check_edit_safety and detect_incomplete_changes all
/// reported 0 callers for methods whose incoming edges were `Calls`.)
pub(crate) const CALLER_EDGE_KINDS: [engram_graph::EdgeKind; 2] = [
    engram_graph::EdgeKind::Calls,
    engram_graph::EdgeKind::Dependency,
];

/// Incoming CALLER edges for a node across all kinds that mean "calls this"
/// (see [`CALLER_EDGE_KINDS`]). Deduplicates by source node id — the same
/// caller can carry both a `Calls` and a `Dependency` edge — keeping the
/// higher-weight entry, then sorts by weight descending (matching
/// `find_incoming_edges_with_kind` ordering) and truncates to `limit`.
pub(crate) fn incoming_caller_edges(
    graph: &engram_graph::GraphStore,
    project_id: &str,
    node_id: &str,
    limit: usize,
) -> Vec<(String, engram_graph::EdgeKind, u32)> {
    let mut out: Vec<(String, engram_graph::EdgeKind, u32)> = Vec::new();
    for kind in CALLER_EDGE_KINDS {
        out.extend(
            graph
                .find_incoming_edges_with_kind(project_id, Some(kind), node_id, limit)
                .unwrap_or_default(),
        );
    }
    // Dedup by source id, keeping the higher-weight edge for each caller.
    out.sort_by(|a, b| a.0.cmp(&b.0).then(b.2.cmp(&a.2)));
    out.dedup_by(|next, kept| next.0 == kept.0);
    out.sort_by(|a, b| b.2.cmp(&a.2));
    out.truncate(limit);
    out
}

// ─── REG1/MCP1: Shared handler-boundary validation ───────────────────────────

/// Validate a user-supplied project_id at the MCP handler boundary.
///
/// Delegates to the canonical strict validator in `project_service` which enforces
/// `[A-Za-z0-9_-]{1,128}`. This is the single source of truth for project_id
/// policy — both the handler boundary and every service call use identical rules,
/// closing the REG1/X1 trust-boundary gap where the weaker `validate_key_component`
/// (NUL/newline-only) allowed `/`, `..`, and shell metacharacters through the
/// handler layer before reaching filesystem-sensitive operations.
pub(super) fn validate_project_id(project_id: &str) -> Result<(), rmcp::ErrorData> {
    crate::services::project_service::validate_project_id(project_id)
        .map_err(|e| rmcp::ErrorData::invalid_params(e.to_string(), None))
}
