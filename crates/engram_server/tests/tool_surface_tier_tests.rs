#![allow(clippy::unwrap_used)]
//! Full discovery by default, with an explicit limited core opt-in.

use engram_core::config::Config;
use engram_server::state::AppState;
use engram_server::tool_surface::{CORE_TOOLS, advertised};
use engram_server::tools::Engram;
use std::collections::BTreeSet;

fn engram() -> (tempfile::TempDir, Engram) {
    engram_with_surface(true)
}

fn engram_with_surface(advertise_all_tools: bool) -> (tempfile::TempDir, Engram) {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("proj")).unwrap();
    let cfg = Config {
        allowed_roots: vec![tmp.path().join("proj")],
        data_dir: tmp.path().join("data"),
        embedding_backend: "fts_only".into(),
        advertise_all_tools,
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    (tmp, Engram::new(state))
}

#[test]
fn every_core_tool_exists_and_covers_the_ten_capabilities() {
    let (_tmp, engram) = engram();
    let all: BTreeSet<String> = engram
        .tool_router
        .list_all()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    let missing: Vec<&str> = CORE_TOOLS
        .iter()
        .copied()
        .filter(|t| !all.contains(*t))
        .collect();
    assert!(
        missing.is_empty(),
        "core tools that do not exist: {missing:?}"
    );
    for must in [
        "get_change_set",
        "get_method_edit_context",
        "get_page_context",
        "pre_commit_review",
        "get_concept_footprint",
        "find_symbol_references",
        "find_implementation_pattern",
        "ask_codebase",
        "trace_ui_event",
        "trace_data_flow",
        "map_guards_and_settings",
        "immune_check",
        "detect_incomplete_changes",
        "impact_analysis",
        "check_edit_safety",
        "get_review_decisions",
        "list_projects",
        "project_info",
        "get_chunk",
        "get_full_method_body",
        "read_memory_bank",
        "get_job_status",
        "cancel_job",
    ] {
        assert!(CORE_TOOLS.contains(&must), "{must} must be a core tool");
    }
    assert!(
        CORE_TOOLS.len() <= 32,
        "the core tier stays small: {}",
        CORE_TOOLS.len()
    );
}

#[test]
fn full_and_core_surfaces_cover_their_registered_tools() {
    let (_tmp, engram) = engram();
    let all = engram.tool_router.list_all();
    assert!(
        all.len() > 100,
        "the router still holds the whole surface ({})",
        all.len()
    );

    let core = advertised(all.clone(), false);
    let core_names: BTreeSet<String> = core.iter().map(|t| t.name.to_string()).collect();
    assert_eq!(
        core_names,
        CORE_TOOLS
            .iter()
            .map(|s| s.to_string())
            .collect::<BTreeSet<_>>(),
        "advertise_all_tools=false advertises exactly the core tier"
    );

    assert!(!core_names.contains("project_health"), "Detailed health is optional; current-source recovery stays callable within32tools");
    assert!(core_names.contains("get_full_method_body") && core_names.contains("get_index_freshness"));
    assert!(all.iter().any(|tool|tool.name=="project_health"),"Full discovery retains detailed health diagnostics");
    let full = advertised(all.clone(), true);
    assert_eq!(
        full.len(),
        all.len(),
        "advertise_all_tools=true advertises everything"
    );
}

#[test]
fn advertise_all_tools_defaults_to_full_for_rust_and_yaml() {
    let cfg = Config::default();
    assert!(cfg.advertise_all_tools);
    let mut value = serde_yaml::to_value(&cfg).unwrap();
    value
        .as_mapping_mut()
        .unwrap()
        .remove(serde_yaml::Value::String("advertise_all_tools".into()));
    let yaml: Config = serde_yaml::from_value(value.clone()).unwrap();
    assert!(yaml.advertise_all_tools);
    value["advertise_all_tools"] = serde_yaml::Value::Bool(false);
    let core: Config = serde_yaml::from_value(value).unwrap();
    assert!(!core.advertise_all_tools);
}

#[tokio::test]
async fn protocol_discovery_includes_pilotapp_agent_dependencies_with_schemas() {
    use rmcp::ServiceExt;
    let (_tmp, engram) = engram();
    let expected: BTreeSet<String> = engram
        .tool_router
        .list_all()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    let (client_io, server_io) = tokio::io::duplex(1024 * 1024);
    let server = tokio::spawn(async move { engram.serve(server_io).await.unwrap() });
    let client = ().serve(client_io).await.unwrap();
    let server = server.await.unwrap();
    let listed = client.list_all_tools().await.unwrap();
    let names: BTreeSet<String> = listed.iter().map(|t| t.name.to_string()).collect();
    assert_eq!(
        names, expected,
        "tools/list must not silently remove .NET tools"
    );
    fn unresolved_schema(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::Object(map) => map.keys().any(|key| matches!(key.as_str(),"$ref"|"$defs"|"definitions"))
                || map.values().any(unresolved_schema),
            serde_json::Value::Array(array) => array.iter().any(unresolved_schema),
            _ => false,
        }
    }
    for tool in &listed {
        assert!(!unresolved_schema(&serde_json::Value::Object(tool.input_schema.as_ref().clone())),
            "Every advertised tool needs a complete reference-free schema: {}",tool.name);
    }
    let audit = listed.iter().find(|tool|tool.name=="pre_push_audit").unwrap();
    let maintenance = &audit.input_schema["properties"]["instruction_maintenance"];
    let decision = &maintenance["properties"]["draft"]["properties"]["decisions"]["items"];
    assert_eq!(decision["additionalProperties"],false,"Deep proposals retain their actual constraints");
    assert_eq!(decision["properties"]["evidence"]["items"]["properties"]["version"]["enum"],serde_json::json!(["before","after"]));
    assert_eq!(decision["properties"]["edits"]["items"]["properties"]["old_text"]["type"],"string");
    assert!(decision["required"].as_array().unwrap().contains(&serde_json::json!("instruction_path")));
    for name in [
        "get_full_method_body",
        "get_chunk",
        "get_full_method_body",
        "get_table_schema",
        "describe_setting",
        "find_merged_work",
        "query_business_logic",
        "read_memory_bank",
        "validate_generated_code",
        "validate_sql_fragment",
        "derive_test_matrix",
        "find_tests_for_method",
        "map_validation_controls",
        "trace_state_usage",
        "analyze_business_logic",
        "list_memory_bank",
        "list_repo_rules",
        "get_setting",
        "list_settings",
        "find_references",
        "analyze_temporal_couplings",
        "list_projects",
        "compute_blast_radius",
        "find_connection_path",
        "analyze_file_coding_style",
        "find_similar_changes",
        "pre_push_audit",
        "produce_claude_md",
    ] {
        let tool = listed
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("missing {name}"));
        assert_eq!(
            tool.input_schema.get("type"),
            Some(&serde_json::json!("object")),
            "{name}"
        );
        let schema = serde_json::to_string(&tool.input_schema).unwrap();
        assert!(
            !schema.contains("\"$ref\""),
            "unresolved reference in {name}"
        );
    }
    client.cancel().await.unwrap();
    server.cancel().await.unwrap();
}

#[tokio::test]
async fn core_client_can_choose_a_project_and_recover_search_and_memory_citations() {
    use anyhow::Context;
    use engram_core::{ContentHash, ProjectRecord, RelPath};
    use engram_index::IndexDoc;
    use engram_server::services::project_service;
    use rmcp::{
        ServiceExt,
        model::{CallToolRequestParam, CallToolResult},
    };
    use serde_json::{Value, json};
    use tokio_util::sync::CancellationToken;

    const PID: &str = "core-client-project";
    const BODY: &str = "// discoverable citation content\npub fn core_marker() {}\n";
    const MEMORY: &str = "Preserve this exact project memory.";
    let (temporary, engram) = engram_with_surface(false);
    let state = engram.state.clone();
    let project = temporary.path().join("proj");
    std::fs::write(project.join("sample.rs"), BODY).unwrap();
    state
        .registry
        .put_project(&ProjectRecord {
            project_id: PID.into(),
            project_name: "Core client fixture".into(),
            project_type: "general".into(),
            directory: project.to_string_lossy().into_owned(),
            created_at_ms: 0,
            updated_at_ms: 0,
            reindex_required_since_ms: None,
        })
        .unwrap();
    engram.handle_update_memory_bank(serde_json::from_value(json!({
        "project_id":PID,"section_id":"core-note","section":"Core memory","content":MEMORY
    })).unwrap()).await.unwrap();
    let runtime = project_service::ensure_project_runtime(&state, PID)
        .await
        .unwrap();
    runtime
        .search
        .index_docs(
            PID,
            &[IndexDoc {
                generation: 1,
                chunk_id: 1,
                path: RelPath::new("sample.rs"),
                language: "rust".into(),
                content: BODY.into(),
                namespace: "memory".into(),
                author: None,
                timestamp: None,
                start_line: 1,
                end_line: 2,
                doc_id: "core-citation".into(),
                content_hash: ContentHash::compute(BODY.as_bytes()).0,
            }],
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    // This owned legacy producer wrote its one complete source file. Attest
    // completion explicitly; an unexplained numeric HEAD cannot prove it.
    assert!(state.registry.get_meta(PID,"active_generation").unwrap().is_none(),
        "Legacy producer setup must retain a genuinely absent HEAD");
    let legacy=state.registry.capture_source_head(PID).unwrap();
    assert_eq!(legacy.generation(),1);
    assert!(state.registry.get_meta(PID,"active_generation").unwrap().is_none(),
        "Capturing legacy authority must not publish a source HEAD");
    assert!(state.registry.attest_legacy_source_head(&legacy).unwrap());
    assert!(state.registry.get_meta(PID,"active_generation").unwrap().is_none(),
        "Legacy completion attestation must not manufacture a numeric HEAD");
    drop(runtime);
    let registry_owners_after_server_close = std::sync::Arc::strong_count(&state.registry) - 1;
    let mut queued_project_lock = Some(state.acquire_project_update_lock(PID).await);

    fn text(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|value| value.as_text().map(|value| value.text.as_str()))
            .collect::<Vec<_>>()
            .join("\n")
    }
    let (client_io, server_io) = tokio::io::duplex(1024 * 1024);
    let server_start = tokio::spawn(async move { engram.serve(server_io).await });
    let client = match ().serve(client_io).await {
        Ok(client) => client,
        Err(error) => {
            server_start.abort();
            let _ = server_start.await;
            panic!("client initialization failed: {error}");
        }
    };
    let server = server_start.await.unwrap().unwrap();
    macro_rules! call {
        ($name:expr, $arguments:expr) => {
            client
                .call_tool(CallToolRequestParam {
                    name: $name.into(),
                    arguments: $arguments.as_object().cloned(),
                })
                .await?
        };
    }
    // Always close both protocol services before reporting a failed assertion.
    let scenario: anyhow::Result<()> = async {
        let advertised = client.list_all_tools().await?;
        let instructions = client
            .peer_info()
            .context("server initialization info missing")?
            .instructions
            .as_deref()
            .context("core workflow instructions missing")?;
        let mentions = regex::Regex::new(r"`([a-z][a-z0-9_]+)(?:\([^`]*\))?`")?;
        for mention in mentions.captures_iter(instructions) {
            let tool = &mention[1];
            anyhow::ensure!(
                advertised.iter().any(|entry| entry.name == tool),
                "core workflow requires an unadvertised tool: {tool}"
            );
        }
        anyhow::ensure!(
            advertised.len() <= 32,
            "limited client exceeded its tool ceiling"
        );
        for required in [
            "list_projects",
            "project_info",
            "get_chunk",
        "get_full_method_body",
            "read_memory_bank",
            "get_job_status",
            "cancel_job",
            "get_review_decisions",
        ] {
            anyhow::ensure!(
                advertised.iter().any(|tool| tool.name == required),
                "core client cannot call {required}"
            );
        }
        anyhow::ensure!(
            !advertised
                .iter()
                .any(|tool| tool.name == "compute_blast_radius")
        );
        let projects = call!("list_projects", json!({}));
        anyhow::ensure!(
            text(&projects).contains(PID),
            "project discovery omitted the fixture"
        );
        let identity = call!("project_info", json!({"project_id":PID}));
        anyhow::ensure!(
            text(&identity).contains(PID) && text(&identity).contains("Core client fixture")
        );

        let search = call!(
            "search_memory",
            json!({"project_id":PID, "query":"discoverable", "semantic":false, "max_results":1})
        );
        let search_text = text(&search);
        let recovery = search_text
            .lines()
            .find_map(|line| line.strip_prefix("citation_chunk: get_chunk("))
            .and_then(|line| line.strip_suffix(')'))
            .context("search did not provide recoverable citation arguments")?;
        let recovery: Value = serde_json::from_str(recovery)?;
        anyhow::ensure!(recovery["doc_id"] == "core-citation" && recovery["project_id"] == PID);
        let chunk = call!("get_chunk", recovery);
        let chunk: Value = serde_json::from_str(&text(&chunk))?;
        anyhow::ensure!(chunk["content"] == BODY && chunk["identity"]["project_id"] == PID);
        anyhow::ensure!(chunk["identity"]["doc_id"] == "core-citation");
        let found=call!("grep_project",json!({"project_id":PID,"pattern":"core_marker","freshness":"off","output_json":true}));
        let found:Value=serde_json::from_str(&text(&found))?;
        let action=&found["matches"][0]["current_source_recovery"];
        anyhow::ensure!(action["tool"]=="get_full_method_body");
        let current=call!("get_full_method_body",action["arguments"].clone());
        let current:Value=serde_json::from_str(&text(&current))?;
        anyhow::ensure!(current["retrieval_scope"]=="current_file_excerpt" && current["line_start"]==1 && current["line_end"]==2);
        anyhow::ensure!(current["source_file_hash"]==ContentHash::compute(BODY.as_bytes()).0);
        anyhow::ensure!(current["source_code"]==BODY.trim_end_matches('\n'));
        let recalled = call!("search_memory",json!({"project_id":PID,"query":"exact project memory",
            "search_scope":"knowledge","semantic":false,"max_results":1}));
        let recalled = text(&recalled);
        let memory_recovery = recalled.lines().find_map(|line| line.strip_prefix("citation_section: read_memory_bank("))
            .and_then(|line| line.strip_suffix(')')).context("A core client must discover a memory section without knowing its name")?;
        let memory_recovery: Value = serde_json::from_str(memory_recovery)?;
        anyhow::ensure!(memory_recovery["project_id"]==PID && memory_recovery["section"]=="core-note");
        let memory = call!("read_memory_bank",memory_recovery);
        let memory: Value = serde_json::from_str(&text(&memory))?;
        anyhow::ensure!(memory["content"] == MEMORY && memory["identity"]["project_id"] == PID);
        anyhow::ensure!(!advertised.iter().any(|tool|tool.name=="list_advanced_tools"),
            "The optional catalog must not displace Core's actionable recovery dependency");
        anyhow::ensure!(instructions.contains("advertise_all_tools=true"));

        let started = call!(
            "update_project",
            json!({"project_id":PID,"wait":false,"max_commits":1})
        );
        let started = text(&started);
        let job_id = started
            .lines()
            .find_map(|line| line.strip_prefix("job_id: "))
            .context("asynchronous update did not return a job identity")?;
        let running = state
            .active_jobs
            .read()
            .await
            .get(job_id)
            .map(tokio::task::JoinHandle::abort_handle)
            .context("owned update handle missing")?;
        let status = call!("get_job_status", json!({"job_id":job_id}));
        anyhow::ensure!(text(&status).contains("status: running"));
        let token = state.cancellation_tokens.read().await.get(job_id).cloned()
            .context("owned update cancellation token missing")?;
        let cancellation = client.call_tool(CallToolRequestParam {
            name: "cancel_job".into(),
            arguments: json!({"job_id":job_id}).as_object().cloned(),
        });
        tokio::pin!(cancellation);
        tokio::select! {
            result = &mut cancellation => anyhow::bail!("Cancellation returned before the held worker settled: {result:?}"),
            _ = token.cancelled() => {},
            _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => anyhow::bail!("Cancellation did not signal the owned worker"),
        }
        // Cooperative cancellation waits for the producer to settle. Release
        // this fixture's lane after observing the signal so it can do so.
        drop(queued_project_lock.take());
        let cancelled = tokio::time::timeout(std::time::Duration::from_secs(10), cancellation).await??;
        anyhow::ensure!(text(&cancelled).contains("status: cancelled"));
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !running.is_finished() {
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "cancelled work did not stop"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let terminal = call!("get_job_status", json!({"job_id":job_id}));
        anyhow::ensure!(text(&terminal).contains("status: cancelled"));
        drop(queued_project_lock.take());

        // A cancellation after completion must not change a completed job into
        // a fictitious cancellation. This update processes just the owned file.
        let started = call!(
            "update_project",
            json!({"project_id":PID,"wait":false,"max_commits":1})
        );
        let started = text(&started);
        let job_id = started
            .lines()
            .find_map(|line| line.strip_prefix("job_id: "))
            .context("second update did not return a job identity")?;
        let running = state
            .active_jobs
            .read()
            .await
            .get(job_id)
            .map(tokio::task::JoinHandle::abort_handle);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        let terminal = loop {
            let status = call!("get_job_status", json!({"job_id":job_id}));
            let status = text(&status);
            if !status.contains("status: running") {
                break status;
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "owned update did not complete"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        anyhow::ensure!(
            terminal.contains("status: done") || terminal.contains("status: degraded"),
            "{terminal}"
        );
        if let Some(running) = running {
            while !running.is_finished() {
                anyhow::ensure!(
                    tokio::time::Instant::now() < deadline,
                    "completion status preceded unfinished work"
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
        let _ = call!("cancel_job", json!({"job_id":job_id}));
        let after_cancel = call!("get_job_status", json!({"job_id":job_id}));
        anyhow::ensure!(
            text(&after_cancel) == terminal,
            "late cancellation changed a completed outcome"
        );
        Ok(())
    }
    .await;
    drop(queued_project_lock);
    for token in state.cancellation_tokens.read().await.values() {
        token.cancel();
    }
    let owned_jobs: Vec<_> = state
        .active_jobs
        .write()
        .await
        .drain()
        .map(|(_, job)| job)
        .collect();
    for job in owned_jobs {
        job.abort();
        let _ = job.await;
    }
    let client_closed = client.cancel().await;
    let server_closed = server.cancel().await;
    let cleanup_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::sync::Arc::strong_count(&state.registry) > registry_owners_after_server_close
        && tokio::time::Instant::now() < cleanup_deadline
    {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    drop(state);
    client_closed.unwrap();
    server_closed.unwrap();
    scenario.unwrap();
}


#[test]
fn core_review_client_can_follow_advertised_discussion_recovery_without_suppressing_findings() -> anyhow::Result<()> {
    use anyhow::{Context, ensure};
    use rmcp::{ServiceExt, model::{CallToolRequestParam, CallToolResult}};
    use serde_json::{Value, json};
    use std::{path::Path, time::Duration};

    fn text(result:&CallToolResult)->String {
        result.content.iter().filter_map(|value|value.as_text().map(|value|value.text.as_str()))
            .collect::<Vec<_>>().join("\n")
    }
    const BEFORE:&str="pub fn export() {}\n";
    // Deliberately invented fixture text, never an actual credential.
    const AFTER:&str="pub fn export() { let password = \"fixture_dummy_value_only\"; }\n";
    const DIFF:&str="diff --git a/src/export.rs b/src/export.rs\n--- a/src/export.rs\n+++ b/src/export.rs\n@@ -1 +1 @@\n-pub fn export() {}\n+pub fn export() { let password = \"fixture_dummy_value_only\"; }\n";
    let (temporary,producer)=engram_with_surface(false);
    let project=temporary.path().join("proj");
    let runtime=tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let result:anyhow::Result<()>=runtime.block_on(async move {
        std::fs::create_dir_all(project.join("src"))?;
        std::fs::write(project.join("src/export.rs"),BEFORE)?;
        {
            let repository=git2::Repository::init(&project)?;
            let mut index=repository.index()?;
            index.add_path(Path::new("src/export.rs"))?;
            let tree=repository.find_tree(index.write_tree()?)?;
            let author=git2::Signature::now("Owned Core fixture","fixture@example.invalid")?;
            repository.commit(Some("HEAD"),&author,&author,"baseline",&tree,&[])?;
        }
        std::fs::write(project.join("src/export.rs"),AFTER)?;
        // Producers are deliberately separate from Core consumption: import
        // remains an administrative/full-profile operation.
        let indexed=producer.handle_index_project(serde_json::from_value(json!({
            "directory":project,"project_name":"Core review recovery","project_type":"rust","wait":true
        }))?).await?;
        let pid=text(&indexed).split("project_id: ").nth(1).context("Missing actual project identity")?
            .lines().next().context("Missing project identity line")?.trim().to_owned();
        let state=producer.state.clone();
        let server_app=Engram::new(state.clone());
        let (client_io,server_io)=tokio::io::duplex(1024*1024);
        let mut server_start=tokio::spawn(async move {server_app.serve(server_io).await});
        let client=match tokio::time::timeout(Duration::from_secs(10),().serve(client_io)).await {
            Ok(Ok(client))=>client,
            error=>{
                server_start.abort();let _=server_start.await;
                let reason=match error {
                    Ok(Err(error))=>error.to_string(),Err(error)=>error.to_string(),Ok(Ok(_))=>unreachable!(),
                };
                anyhow::bail!("Core client initialization failed: {reason}");
            }
        };
        let server=match tokio::time::timeout(Duration::from_secs(10),&mut server_start).await {
            Ok(Ok(Ok(server)))=>server,
            error=>{
                server_start.abort();let _=server_start.await;
                let _=tokio::time::timeout(Duration::from_secs(10),client.cancel()).await;
                let reason=match error {
                    Ok(Ok(Err(error)))=>error.to_string(),Ok(Err(error))=>error.to_string(),
                    Err(error)=>error.to_string(),Ok(Ok(Ok(_)))=>unreachable!(),
                };
                anyhow::bail!("Core server initialization failed: {reason}");
            }
        };
        macro_rules! call {
            ($name:expr,$arguments:expr)=>{
                {
                    let result=tokio::time::timeout(Duration::from_secs(20),client.call_tool(CallToolRequestParam {
                        name:$name.to_owned().into(),arguments:$arguments.as_object().cloned(),
                    })).await??;
                    ensure!(result.is_error!=Some(true),"Core tool {} failed: {}",$name,text(&result));
                    result
                }
            };
        }
        let scenario:anyhow::Result<()>=async {
            let advertised=client.list_all_tools().await?;
            ensure!(advertised.len()<=32);
            let recovery_schema=advertised.iter().find(|tool|tool.name=="get_review_decisions")
                .context("Core cannot follow the recovery call emitted by its ordinary review")?;
            ensure!(recovery_schema.input_schema["type"]=="object");
            ensure!(recovery_schema.input_schema["properties"].get("discussion").is_some(),
                "Core needs the actual discussion selector schema, not a text catalog");
            let baseline=call!("pre_commit_review",json!({"project_id":pid,"diff":DIFF,"output_json":true}));
            let baseline:Value=serde_json::from_str(&text(&baseline))?;
            let before=baseline["findings"].as_array().context("Missing baseline findings")?.iter()
                .find(|finding|finding["gate"]=="secret_leakage")
                .context("Fixture must produce a real current-code finding")?.clone();
            let imported=json!({"pr_id":2077,"thread_id":1,"pr_title":"Export password demonstration",
                "pr_author":"Fixture author","pr_date":"2026-01-01T12:00:00Z","status":"fixed",
                "file_path":"/src/export.rs","line":1,"unknown_thread_metadata":{"preserve":true},
                "comments":[{"author":"Reviewer","text":format!("The export password uses a hardcoded fixture value. {}", "historical finding detail ".repeat(70))},
                    {"author":"Owner","text":format!("This is the intended behaviour for the old demonstration only. {} EXCEPTION_TAIL_é🙂: production safeguards remain required.", "qualified historical explanation ".repeat(80))},
                    {"author":"System","text":"","commentType":"system","unknown_blank_field":"retain"}]});
            let exact=format!("  {imported}\r\n");
            std::fs::write(project.join("review-fixture.jsonl"),&exact)?;
            producer.handle_ingest_review_verdicts(serde_json::from_value(json!({
                "project_id":pid,"source":"json_file","file_path":"review-fixture.jsonl","decision_makers":["Owner"]
            }))?).await?;
            let review=call!("pre_commit_review",json!({"project_id":pid,"diff":DIFF,"output_json":true}));
            let review:Value=serde_json::from_str(&text(&review))?;
            let after=review["findings"].as_array().context("Missing current findings")?.iter()
                .find(|finding|finding["gate"]=="secret_leakage")
                .context("Historical workflow closure suppressed the current defect")?;
            ensure!(after["severity"]==before["severity"]);
            let prior=&review["prior_review_context"];
            ensure!(prior["automatic_finding_suppression"]==false);
            let item=prior["items"].as_array().context("Missing ordinary-review evidence")?.iter()
                .find(|item|item["review_id"]=="PR-2077").context("Relevant imported discussion was not surfaced")?;
            ensure!(item["retrieval_relation"]=="exact_file_reference");
            ensure!(item["scope_and_revision_applicability"].as_str().is_some_and(|value|value.contains("not_established")));
            ensure!(!item["stored_text"].as_str().context("Missing indexed summary")?.contains("EXCEPTION_TAIL_"),
                "The fixture must require exact recovery of the omitted exception");
            let lead=&item["discussion_recovery"];
            ensure!(lead["status"]=="capture_reference");
            let capture_hash=lead["capture_blake3"].as_str().context("Missing capture identity")?;
            let mut next=lead["recovery"].clone();
            let mut assembled=String::new();let mut recovery_hash=None;let mut complete=false;
            for _ in 0..8 {
                let tool=next["tool"].as_str().context("Recovery tool missing")?;
                ensure!(tool=="get_review_decisions" && advertised.iter().any(|entry|entry.name==tool),
                    "A Core client cannot follow an unadvertised recovery call");
                ensure!(next["arguments"]["project_id"]==pid);
                let response=call!(tool,next["arguments"].clone());
                let page:Value=serde_json::from_str(&text(&response))?;
                ensure!(page["capture_blake3"]==capture_hash && page["start_byte"]==assembled.len());
                let hash=page["recovery_blake3"].as_str().context("Missing complete recovery hash")?.to_owned();
                if let Some(expected)=&recovery_hash {ensure!(expected==&hash);}else{recovery_hash=Some(hash);}
                assembled.push_str(page["capture_json"].as_str().context("Missing exact recovery page")?);
                ensure!(page["end_byte"]==assembled.len());
                if page["complete"]==true {ensure!(page["next_call"].is_null());complete=true;break;}
                next=page["next_call"].clone();
            }
            ensure!(complete,"Bounded Core recovery did not finish");
            ensure!(blake3::hash(assembled.as_bytes()).to_hex().as_str()==recovery_hash.as_deref().unwrap());
            let envelope:Value=serde_json::from_str(&assembled)?;
            let metadata=envelope["capture_metadata_json"].as_str().context("Missing exact capture metadata")?;
            ensure!(blake3::hash(metadata.as_bytes()).to_hex().as_str()==capture_hash);
            let metadata:Value=serde_json::from_str(metadata)?;
            ensure!(envelope["raw_utf8"]==exact);
            ensure!(metadata["raw_utf8_bytes"]==exact.len()
                && metadata["raw_utf8_blake3"]==blake3::hash(exact.as_bytes()).to_hex().as_str());
            ensure!(serde_json::from_str::<Value>(envelope["raw_utf8"].as_str().unwrap())?==imported,
                "Recovery lost qualified exceptions, blank/system records or unknown fields");
            Ok(())
        }.await;
        // Finish every actual store worker before the enclosing runtime and
        // owned directory close, on both success and assertion failure.
        for token in state.cancellation_tokens.read().await.values() {token.cancel();}
        let jobs=state.active_jobs.write().await.drain().map(|(_,job)|job).collect::<Vec<_>>();
        let mut unfinished=false;
        for job in jobs {
            job.abort();
            if tokio::time::timeout(Duration::from_secs(10),job).await.is_err() {unfinished=true;}
        }
        let client_closed=tokio::time::timeout(Duration::from_secs(10),client.cancel()).await;
        let server_closed=tokio::time::timeout(Duration::from_secs(10),server.cancel()).await;
        drop(producer);drop(state);
        scenario?;ensure!(!unfinished,"Owned Core job failed to finish");
        client_closed??;server_closed??;
        Ok(())
    });
    drop(runtime);
    let cleanup=temporary.close();
    match(result,cleanup) {
        (Err(error),Err(cleanup))=>Err(error.context(format!("Owned Core fixture cleanup failed: {cleanup}"))),
        (Err(error),_)=>Err(error),(Ok(()),cleanup)=>cleanup.map_err(Into::into),
    }
}
