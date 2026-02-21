#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityStatus {
    Implemented,
    Partial,
    Experimental,
    Planned,
}

pub struct CapabilityFlag {
    pub key: &'static str,
    pub status: CapabilityStatus,
}

#[allow(dead_code)]
pub const CAPABILITY_FLAGS: &[CapabilityFlag] = &[
    CapabilityFlag {
        key: "index_project",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "update_project",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "list_projects",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "project_info",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "project_health",
        status: CapabilityStatus::Partial,
    },
    CapabilityFlag {
        key: "repair_project",
        status: CapabilityStatus::Partial,
    },
    CapabilityFlag {
        key: "delete_project",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "watch_project",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "unwatch_project",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "search_memory",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "get_chunk",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "update_memory_bank",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "list_memory_bank",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "read_memory_bank",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "delete_memory_bank",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "add_repo_rule",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "list_repo_rules",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "delete_repo_rule",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "query_graph_nodes",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "find_references",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "graph_search",
        status: CapabilityStatus::Partial,
    },
    CapabilityFlag {
        key: "traverse_graph",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "index_git_history",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "ingest_zip_history",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "search_history",
        status: CapabilityStatus::Partial,
    },
    CapabilityFlag {
        key: "analyze_temporal_couplings",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "analyze_reverts",
        status: CapabilityStatus::Partial,
    },
    CapabilityFlag {
        key: "impact_analysis",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "get_table_schema",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "trace_state_usage",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "trace_ui_event",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "trace_ui_action",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "export_capture_pack",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "get_ui_blueprint",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "get_codebase_overview",
        status: CapabilityStatus::Partial,
    },
    CapabilityFlag {
        key: "find_symbol_references",
        status: CapabilityStatus::Partial,
    },
    CapabilityFlag {
        key: "analyze_error_stack",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "dream_project",
        status: CapabilityStatus::Experimental,
    },
    CapabilityFlag {
        key: "trigger_rem_cycle",
        status: CapabilityStatus::Experimental,
    },
    CapabilityFlag {
        key: "analyze_file_coding_style",
        status: CapabilityStatus::Experimental,
    },
    CapabilityFlag {
        key: "list_jobs",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "cancel_job",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "get_job_status",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "immune_check",
        status: CapabilityStatus::Experimental,
    },
    CapabilityFlag {
        key: "anti_pattern_guard",
        status: CapabilityStatus::Experimental,
    },
    CapabilityFlag {
        key: "get_instrumentation_pack",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "suggest_migration_boundaries",
        status: CapabilityStatus::Experimental,
    },
    CapabilityFlag {
        key: "ingest_instrumentation_logs",
        status: CapabilityStatus::Implemented,
    },
    CapabilityFlag {
        key: "generate_migration_blueprint",
        status: CapabilityStatus::Partial,
    },
    CapabilityFlag {
        key: "ast_dependency_graph",
        status: CapabilityStatus::Partial,
    },
    CapabilityFlag {
        key: "vector_search",
        status: CapabilityStatus::Experimental,
    },
    CapabilityFlag {
        key: "incremental_indexing_gc",
        status: CapabilityStatus::Partial,
    },
    CapabilityFlag {
        key: "dedicated_antipattern_index",
        status: CapabilityStatus::Partial,
    },
    CapabilityFlag {
        key: "graph_centrality_rerank",
        status: CapabilityStatus::Planned,
    },
];
