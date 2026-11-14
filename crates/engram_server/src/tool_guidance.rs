//! Advisory navigation after specialty-tool calls. Never execute suggestions,
//! rewrite an error, cut evidence, or replace a successful mutation with an error.
use rmcp::model::{CallToolResult, RawContent, Tool};
use serde_json::{Map, Value, json};

type Suggestions = &'static [(&'static str, &'static str)];

#[derive(Debug)]
pub enum Policy {
    Preserve,
    Suggest(Suggestions),
    Terminal(&'static str),
    Unknown,
}

pub fn policy(tool: &str) -> Policy {
    if crate::tool_surface::CORE_TOOLS.contains(&tool) {
        return Policy::Preserve;
    }
    use Policy::*;
    match tool {
        // Closed improvement areas and the unfinished planner remain untouched.
        "compute_blast_radius" | "get_codebase_overview" | "one_shot_feature_plan" => Preserve,
        "list_advanced_tools" => Terminal(
            "Choose a tool from the returned catalog; inspect its input schema and availability first.",
        ),
        "delete_project" => Suggest(&[(
            "list_projects",
            "Confirm the remaining registered projects; do not repeat deletion to inspect its result.",
        )]),
        "refresh_corpora"
        | "repair_project"
        | "index_git_history"
        | "ingest_zip_history"
        | "ingest_merged_prs"
        | "ingest_quality_gates"
        | "distill_quality_gates"
        | "ingest_review_findings"
        | "ingest_review_verdicts"
        | "ingest_code_review_history"
        | "ingest_instrumentation_logs"
        | "ingest_runtime_artifacts"
        | "dream_project"
        | "trigger_rem_cycle" => Suggest(&[
            (
                "get_job_status",
                "If a job ID was returned, poll that job instead of repeating the operation.",
            ),
            (
                "project_info",
                "Inspect the resulting project state and coverage before relying on new evidence.",
            ),
        ]),
        "list_jobs" => Suggest(&[(
            "get_job_status",
            "Inspect one returned job ID for its progress and terminal outcome.",
        )]),
        "watch_project" | "unwatch_project" | "incremental_indexing_gc" => Suggest(&[(
            "get_index_freshness",
            "Check published source freshness; watching or cleanup does not prove a fresh index.",
        )]),
        "project_health"
        | "check_integrity"
        | "get_extraction_confidence"
        | "get_checkpoint_status"
        | "get_memory_budget"
        | "get_metrics"
        | "benchmark_retrieval" => Suggest(&[(
            "project_info",
            "Inspect project capabilities and publication state before selecting a recovery action.",
        )]),
        "batch_read" => Suggest(&[
            (
                "get_method_edit_context",
                "For a returned method you intend to edit, inspect its context and dependencies.",
            ),
            (
                "check_edit_safety",
                "Assess a specific cited method before changing it; batch delivery is not a safety verdict.",
            ),
        ]),
        "trace_user_request"
        | "trace_ui_action"
        | "find_connection_path"
        | "resolve_graph_edges"
        | "query_graph_nodes"
        | "find_references"
        | "graph_search"
        | "traverse_graph"
        | "ast_dependency_graph"
        | "graph_centrality_rerank"
        | "find_dependency_cycles" => Suggest(&[
            (
                "get_method_edit_context",
                "Read a cited method and its immediate dependencies before choosing an edit.",
            ),
            (
                "check_edit_safety",
                "Check the intended method edit; navigation evidence is not runtime or safety proof.",
            ),
        ]),
        "query_schema" => Suggest(&[
            (
                "get_full_method_body",
                "Read a cited code-reference lead; its qualified database binding may still be unresolved.",
            ),
            (
                "find_missing_indexes",
                "Compare relevant query access patterns with source-declared indexes before suggesting DDL.",
            ),
        ]),
        "detect_n_plus_one" => Suggest(&[
            (
                "get_method_edit_context",
                "Inspect the cited loop or row-binding method before choosing a batching change.",
            ),
            (
                "query_schema",
                "Inspect the qualified tables and keys involved; static repetition does not establish runtime cost.",
            ),
        ]),
        "find_missing_indexes" => Suggest(&[
            (
                "query_schema",
                "Verify the candidate's qualified table, existing keys and indexes in source.",
            ),
            (
                "validate_sql_fragment",
                "If DDL was actually supplied, validate that exact proposal before review; do not execute it automatically.",
            ),
        ]),
        "get_table_schema"
        | "describe_table"
        | "analyze_database_intelligence"
        | "get_sp_details"
        | "list_triggers" => Suggest(&[
            (
                "query_schema",
                "Inspect a specific qualified declaration and its source coverage.",
            ),
            (
                "trace_user_request",
                "For a known URL or page, trace the application path to the database evidence.",
            ),
        ]),
        "find_similar_changes"
        | "find_merged_work"
        | "search_history"
        | "get_review_precedents"
        | "analyze_temporal_couplings"
        | "analyze_reverts"
        | "explain_change" => Suggest(&[
            (
                "get_method_edit_context",
                "Compare the historical lead with current source before adopting the pattern.",
            ),
            (
                "get_review_decisions",
                "Check retained review decisions and scoped exceptions for the current task.",
            ),
        ]),
        "vector_search" => Suggest(&[(
            "get_chunk",
            "Read a returned chunk ID before treating a similarity hit as supporting evidence.",
        )]),
        "list_settings"
        | "describe_setting"
        | "get_setting"
        | "trace_state_usage"
        | "get_session_workflows" => Suggest(&[(
            "get_method_edit_context",
            "Inspect a cited reader or writer and its guards before changing shared state.",
        )]),
        "update_memory_bank"
        | "import_memory_bank"
        | "list_memory_bank"
        | "delete_memory_bank"
        | "produce_support_kb"
        | "produce_claude_md"
        | "generate_agent_integration" => Suggest(&[(
            "read_memory_bank",
            "Read the relevant stored guidance and its scope before applying or publishing it.",
        )]),
        "add_repo_rule" | "list_repo_rules" | "delete_repo_rule" => Suggest(&[(
            "get_review_decisions",
            "Review applicable decisions and exceptions; storing a rule does not approve an edit.",
        )]),
        "record_review_decisions" => Suggest(&[(
            "get_review_decisions",
            "Read back the recorded decisions and their exact scope.",
        )]),
        "validate_feature_contract"
        | "derive_test_matrix"
        | "pre_push_audit"
        | "anti_pattern_guard"
        | "evaluate_safety"
        | "autonomous_decision_gate"
        | "dedicated_antipattern_index"
        | "validate_generated_code"
        | "validate_sql_fragment" => Suggest(&[(
            "pre_commit_review",
            "Review the actual scoped diff and unresolved findings; this result is not permission to merge or deploy.",
        )]),
        "get_ui_conformance"
        | "get_gis_inventory"
        | "get_ui_blueprint"
        | "export_capture_pack"
        | "get_jquery_inventory"
        | "map_validation_controls"
        | "map_auth_config"
        | "map_page_lifecycle"
        | "analyze_viewstate_deps"
        | "map_ajax_regions" => Suggest(&[
            (
                "get_page_context",
                "Inspect the relevant page, controls and code-behind before planning a UI change.",
            ),
            (
                "trace_ui_event",
                "Trace a concrete authored event to its handler; inventory alone does not establish runtime binding.",
            ),
        ]),
        "get_instrumentation_pack" => Suggest(&[(
            "ingest_instrumentation_logs",
            "After explicitly capturing events with your log sink, supply the indexed project_id and captured log_content; generating this helper collects no runtime evidence.",
        )]),
        "analyze_error_stack"
        | "generate_instrumentation_code"
        | "reconcile_runtime_evidence"
        | "analyze_sync_hazards" => Suggest(&[
            (
                "get_method_edit_context",
                "Read the implicated source method before treating a diagnostic lead as a cause.",
            ),
            (
                "find_tests_for_method",
                "Find tests for the implicated method to check the suspected behavior.",
            ),
        ]),
        "analyze_file_coding_style"
        | "detect_design_patterns"
        | "analyze_business_logic"
        | "query_business_logic"
        | "get_vb_translation_traps"
        | "get_csharp_diagnostics"
        | "get_c_diagnostics"
        | "get_cpp_diagnostics"
        | "get_rust_diagnostics"
        | "get_method_info"
        | "prepare_implementation_context"
        | "find_dead_methods"
        | "find_tests_for_method" => Suggest(&[
            (
                "get_method_edit_context",
                "Inspect the specific cited method before adopting a finding or proposing an edit.",
            ),
            (
                "check_edit_safety",
                "Assess the intended method change with current source and dependency evidence.",
            ),
        ]),
        "suggest_migration_boundaries"
        | "generate_migration_blueprint"
        | "generate_migration_plan"
        | "generate_migration_scaffold"
        | "suggest_state_migration"
        | "generate_characterization_tests"
        | "generate_strangler_fig_config"
        | "get_migration_dossier"
        | "check_migration_coverage"
        | "update_migration_status"
        | "get_migration_progress"
        | "suggest_migration_order"
        | "analyze_full_project_migration" => Suggest(&[
            (
                "get_method_edit_context",
                "Verify the selected source boundary before implementing a migration proposal.",
            ),
            (
                "derive_test_matrix",
                "Derive checks for the proposed scope; generated artifacts do not prove behavioral equivalence.",
            ),
        ]),
        _ => Unknown,
    }
}

/// Copy only routing/budget identifiers, never arbitrary code, secrets or input blobs.
pub fn context(arguments: Option<&Map<String, Value>>) -> Map<String, Value> {
    let mut result = Map::new();
    for key in [
        "project_id",
        "file_path",
        "method_name",
        "job_id",
        "max_tokens",
    ] {
        if let Some(value) = arguments.and_then(|a| a.get(key)) {
            result.insert(key.into(), value.clone());
        }
    }
    result
}

fn limit(tool: &str, arguments: &Map<String, Value>, registry: &[Tool]) -> Option<usize> {
    let explicit = arguments.get("max_tokens").and_then(Value::as_u64);
    let schema_default = registry
        .iter()
        .find(|t| t.name == tool)
        .and_then(|t| t.input_schema.get("properties"))
        .and_then(|p| p.pointer("/max_tokens/default"))
        .and_then(Value::as_u64);
    let default = match tool {
        "batch_read" => Some(8192),
        "get_change_set" | "ask_codebase" => Some(10240),
        "query_schema" | "detect_n_plus_one" | "find_missing_indexes" | "trace_user_request" | "analyze_database_intelligence" => {
            Some(16384)
        }
        _ => schema_default,
    };
    explicit.or(default).map(|tokens| {
        usize::try_from(tokens)
            .ok()
            .and_then(|n| n.checked_mul(4))
            .unwrap_or(0)
    })
}

fn arguments_for(
    target: &Tool,
    source_tool: &str,
    input: &Map<String, Value>,
    result: &Value,
) -> Option<Value> {
    let mut args = Map::new();
    for key in ["project_id", "file_path", "method_name", "job_id"] {
        if let Some(value) = input
            .get(key)
            .filter(|v| v.as_str().is_some_and(|s| !s.is_empty() && s.len() <= 1024))
        {
            args.insert(key.into(), value.clone());
        }
    }
    if target.name == "get_method_edit_context" && source_tool == "detect_n_plus_one" {
        if let Some(site) = result.pointer("/findings/0/repeated_call") {
            for (from, to) in [("file", "file_path"), ("method", "method_name")] {
                if let Some(value) = site
                    .get(from)
                    .filter(|v| v.as_str().is_some_and(|s| !s.is_empty() && s.len() <= 1024))
                {
                    args.insert(to.into(), value.clone());
                }
            }
        }
    }
    if target.name == "get_full_method_body" && source_tool == "query_schema" {
        let site = result.pointer("/code_reference_leads/results/0")?;
        args.insert("file_path".into(), site.get("file")?.clone());
        args.insert("line_start".into(), site.get("start_line")?.clone());
        args.insert("line_end".into(), site.get("end_line")?.clone());
    }
    if target.name == "get_full_method_body" && source_tool == "detect_n_plus_one" {
        let site = result.pointer("/findings/0/repeated_call")?;
        args.insert("file_path".into(), site.get("file")?.clone());
        args.insert("line_start".into(), site.get("start_line")?.clone());
    }
    if target.name == "get_job_status" {
        if let Some(id) = result.get("job_id").filter(|v| v.as_str().is_some()) {
            args.insert("job_id".into(), id.clone());
        }
    }
    let properties = target.input_schema.get("properties")?.as_object()?;
    args.retain(|name, _| properties.contains_key(name));
    if target
        .input_schema
        .get("required")
        .and_then(Value::as_array)
        .is_some_and(|keys| {
            keys.iter()
                .filter_map(Value::as_str)
                .any(|key| !args.contains_key(key))
        })
    {
        return None;
    }
    let value = Value::Object(args);
    (value.to_string().len() <= 512).then_some(value)
}

/// Returns an accounting status for logging/coverage. When no advisory envelope
/// fits, preserve the original result byte-for-byte rather than dropping data or
/// turning a successful operation into a retry-inducing error.
pub fn annotate(
    tool: &str,
    input: &Map<String, Value>,
    result: &mut CallToolResult,
    registry: &[Tool],
    full_profile: bool,
) -> &'static str {
    let selected = policy(tool);
    if matches!(selected, Policy::Preserve) {
        return "preserved";
    }
    if matches!(selected, Policy::Unknown) {
        return "unmapped";
    }
    if result.is_error == Some(true) {
        return "error_preserved";
    }
    if result.content.len() != 1 {
        return "content_shape_preserved";
    }
    let RawContent::Text(text) = &mut result.content[0].raw else {
        return "content_shape_preserved";
    };
    if text.text.len() > 1024 * 1024 {
        return "large_result_preserved";
    }
    let parsed = serde_json::from_str::<Value>(&text.text).ok();
    if parsed.as_ref().is_some_and(|v| !v.is_object()) {
        return "json_shape_preserved";
    }
    if parsed
        .as_ref()
        .is_some_and(|v| v.get("next_tools").is_some())
        || text.text.contains("\nnext_tools:")
    {
        return "existing_guidance_preserved";
    }
    let data = parsed.clone().unwrap_or(Value::Null);
    let status = data
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let unavailable = [
        "not_applicable",
        "unsupported",
        "unavailable",
        "incomplete",
        "failed",
        "error",
    ]
    .iter()
    .any(|s| status == *s || status.starts_with(&format!("{s}_")))
        || (tool == "detect_n_plus_one" && data["selected_vb_files"].as_u64() == Some(0))
        || (tool == "query_schema"
            && data
                .pointer("/coverage/selected_sql_files")
                .and_then(Value::as_u64)
                == Some(0));
    let empty = ["results", "findings", "suggestions", "matches"]
        .iter()
        .any(|k| {
            data.get(k)
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
        });
    let gaps = [
        "/coverage/gaps",
        "/coverage/unavailable_files",
        "/coverage/source_inventory/source_gaps",
    ]
    .iter()
    .any(|path| {
        data.pointer(path)
            .and_then(Value::as_array)
            .is_some_and(|v| !v.is_empty())
    });
    let (choices, reason): (Suggestions, &str) = if unavailable {
        (
            &[(
                "project_info",
                "Check project capabilities and reported prerequisites; no usable analysis was established.",
            )],
            "analysis_unavailable",
        )
    } else if gaps {
        (
            &[(
                "get_index_freshness",
                "Check the reported source gaps before relying on this partial analysis.",
            )],
            "source_coverage_gaps",
        )
    } else if status == "ambiguous" && tool == "query_schema" {
        (
            &[(
                "query_schema",
                "Choose a qualified identity from the returned candidates and repeat the lookup before acting on it.",
            )],
            "resolve_ambiguous_identity",
        )
    } else if empty {
        (
            &[(
                "grep_project",
                "Narrow the lookup against current source; an empty analysis does not prove absence.",
            )],
            "empty_result",
        )
    } else if tool == "detect_n_plus_one"
        && data
            .pointer("/findings/0/repeated_call/method")
            .and_then(Value::as_str)
            .is_none()
    {
        (
            &[
                (
                    "get_full_method_body",
                    "Read the cited repeated-call source before resolving its enclosing method; no method identity was supplied.",
                ),
                (
                    "query_schema",
                    "Inspect the qualified tables and keys involved before choosing a batching change.",
                ),
            ],
            "source_identity_requires_review",
        )
    } else {
        match selected {
            Policy::Suggest(choices) => (
                choices,
                "suggested; backend and project-data requirements still apply",
            ),
            Policy::Terminal(reason) => (&[], reason),
            _ => return "preserved",
        }
    };
    let mut hints = Vec::new();
    for (name, purpose) in choices.iter().take(3) {
        if tool == "query_schema"
            && *name == "get_full_method_body"
            && data.pointer("/code_reference_leads/results/0").is_none()
        {
            continue;
        }
        let Some(target) = registry.iter().find(|t| t.name == *name) else {
            continue;
        };
        if !full_profile && !crate::tool_surface::CORE_TOOLS.contains(name) {
            continue;
        }
        // The discovery filter uses actual project capabilities for legacy tools.
        // Do not assert their availability from a response's tool name alone.
        if target
            .description
            .as_deref()
            .is_some_and(|d| d.starts_with("[.NET legacy]"))
        {
            continue;
        }
        let mut hint = json!({"tool":name,"reason":purpose});
        if let Some(args) = arguments_for(target, tool, input, &data) {
            hint["arguments"] = args;
        }
        hints.push(hint);
    }
    let mut reason = if hints.is_empty() && !choices.is_empty() {
        "no_applicable_follow_up_in_active_profile"
    } else {
        reason
    };
    let budget = limit(tool, input, registry);
    loop {
        let envelope = json!({"next_tools":hints,"next_tools_status":reason});
        if envelope.to_string().len() > 2048 {
            hints.pop();
            reason = "suggestions_reduced_to_preserve_guidance_budget";
            continue;
        }
        let candidate = if let Some(mut value) = parsed.clone() {
            value["next_tools"] = envelope["next_tools"].clone();
            value["next_tools_status"] = envelope["next_tools_status"].clone();
            value.to_string()
        } else {
            format!(
                "{}\n\nnext_tools: {}\nnext_tools_status: {}\n",
                text.text, envelope["next_tools"], reason
            )
        };
        if budget.is_none_or(|limit| candidate.len() <= limit) {
            text.text = candidate;
            return "added";
        }
        if hints.pop().is_none() {
            return "budget_preserved";
        }
        reason = "suggestions_reduced_to_preserve_response_budget";
    }
}
