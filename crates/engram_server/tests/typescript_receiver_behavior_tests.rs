//! Real source must not acquire a same-name callee merely because its receiver
//! was not bound. Unknown receiver work remains inspectable, not proven impact.
use anyhow::{Result, ensure};
use engram_core::{Config, ContentHash};
use engram_graph::EdgeKind;
use engram_server::{AppState, Engram};
use serde_json::{Value, json};
use std::path::Path;

const IMAGE: &str = "src/images/image.ts";
const GROUP: &str = "src/images/group.ts";
const IMAGE_BODY: &str = "class Image {\n resetState() {}\n}\n";
const GROUP_BODY: &str = "/// <reference path=\"image.ts\" />\nclass Group {\n private images: Array<Image> = [];\n resetState() { this.clear(); }\n clear() {\n  for (const h of this.images) {\n   h.resetState();\n   h.resetState();\n  }\n }\n loadAll() { this.clear(); }\n}\n";

fn config(root: &Path) -> Config {
    Config { data_dir: root.join("data"), allowed_roots: vec![root.join("repo")],
        embedding_backend: "fts_only".into(), llm_backend: "none".into(), ..Default::default() }
}

async fn settle(app: &Engram) -> Result<()> {
    loop {
        let jobs = app.state.active_jobs.write().await.drain()
            .map(|(_, job)| job).collect::<Vec<_>>();
        if jobs.is_empty() { return Ok(()); }
        for job in jobs { job.await?; }
    }
}

async fn open(root: &Path) -> Result<Engram> {
    let (state, _) = AppState::new(config(root))?;
    Ok(Engram::new(state))
}

async fn indexed(root: &Path) -> Result<(Engram, String)> {
    for (path, body) in [(IMAGE, IMAGE_BODY), (GROUP, GROUP_BODY)] {
        let path = root.join("repo").join(path);
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(path, body)?;
    }
    let app = open(root).await?;
    let reply = app.handle_index_project(serde_json::from_value(json!({
        "directory":root.join("repo"),"project_name":"Receiver behavior","project_type":"general","wait":true
    }))?).await?;
    ensure!(reply.is_error != Some(true), "Index failed: {reply:?}");
    settle(&app).await?;
    let pid = app.state.registry.list_projects()?[0].project_id.clone();
    Ok((app, pid))
}

fn packet(text: &str) -> Value {
    serde_json::from_str(text.split("## Revision-admitted change impact").nth(1).unwrap()
        .split("```json").nth(1).unwrap().split("```").next().unwrap()).unwrap()
}

fn check_receivers(app: &Engram, pid: &str) -> Result<()> {
    let functions = app.state.graph.query_nodes(pid, Some("function"), None, None, 32)?;
    let named = |file: &str, name: &str| functions.iter()
        .find(|n| n.file_path.as_str() == file && n.name == name).unwrap();
    let image = named(IMAGE, "resetState");
    let group = named(GROUP, "resetState");
    let clear = named(GROUP, "clear");
    let load = named(GROUP, "loadAll");
    let edges = app.state.graph.list_edges(pid, Some(EdgeKind::Calls))?;
    let outgoing: Vec<_> = edges.iter().filter(|e| e.source_id == clear.node_id).collect();
    ensure!(!outgoing.iter().any(|e| e.target_id == group.node_id || e.target_id == image.node_id),
        "A typed collection declaration is not implemented receiver binding: {outgoing:?}");
    let unbound = outgoing.iter().find(|e| e.target_id.starts_with(engram_graph::store::UNBOUND_RECEIVER_PREFIX))
        .ok_or_else(|| anyhow::anyhow!("Unbound receiver must retain an inspectable Calls substrate"))?;
    ensure!(unbound.target_id.ends_with("h.resetState"));
    let metadata = unbound.metadata.as_ref().unwrap();
    ensure!(metadata["receiver_unbound"] == "true" && metadata["receiver"] == "h");
    ensure!(metadata["src_line"] == "5");
    ensure!(metadata["call_site_lines"] == json!([7, 8]), "Repeated actual invocation sites are retained: {metadata}");
    ensure!(edges.iter().any(|e| e.source_id == group.node_id && e.target_id == clear.node_id));
    ensure!(edges.iter().any(|e| e.source_id == load.node_id && e.target_id == clear.node_id));
    ensure!(matches!(app.state.graph.resolve_symbol(pid, &unbound.target_id, None, None)?,
        engram_graph::store::ResolveResult::NotFound));
    Ok(())
}

#[tokio::test]
async fn receiver_uncertainty_survives_normal_resolvers_scoped_refresh_and_cold_reopen() -> Result<()> {
    let owned = tempfile::TempDir::new()?;
    let (app, pid) = indexed(owned.path()).await?;
    check_receivers(&app, &pid)?;
    for _ in 0..2 {
        app.state.graph.resolve_symbol_edges(&pid)?;
        let generation = engram_server::services::project_service::get_active_generation(&app.state, &pid).await?;
        engram_server::services::graph_service::resolve_app_code_globals(&app.state.graph, &pid, generation)?;
        check_receivers(&app, &pid)?;
    }
    // Simulate an older index's wrong concrete edge. A deliberate scoped
    // re-extraction must retract it while preserving all other source paths.
    let nodes = app.state.graph.query_nodes(&pid, Some("function"), None, Some(GROUP), 16)?;
    let clear = nodes.iter().find(|n| n.name == "clear").unwrap();
    let reset = nodes.iter().find(|n| n.name == "resetState").unwrap();
    app.state.graph.upsert_edges(&pid, &[engram_graph::Edge {
        source_id: clear.node_id.clone(), target_id: reset.node_id.clone(), namespace: "memory".into(),
        language: "typescript".into(), edge_kind: EdgeKind::Calls, weight: 1,
        generation: clear.generation, metadata: None, updated_at_ms: 1,
    }])?;
    let image_before = app.state.graph.get_node(&pid, &format!("file:{IMAGE}"))?.unwrap().generation;
    let updated = app.handle_update_project(serde_json::from_value(json!({
        "project_id":pid,"only_paths":[GROUP],"wait":true,"index_antipatterns":false
    }))?).await?;
    ensure!(updated.is_error != Some(true), "Scoped refresh failed: {updated:?}");
    settle(&app).await?;
    ensure!(app.state.graph.get_node(&pid, &format!("file:{IMAGE}"))?.unwrap().generation == image_before);
    app.state.graph.resolve_symbol_edges(&pid)?;
    let generation = engram_server::services::project_service::get_active_generation(&app.state, &pid).await?;
    engram_server::services::graph_service::resolve_app_code_globals(&app.state.graph, &pid, generation)?;
    check_receivers(&app, &pid)?;
    drop(app);
    let reopened = open(owned.path()).await?;
    check_receivers(&reopened, &pid)?;
    settle(&reopened).await?;
    drop(reopened);
    Ok(())
}

#[tokio::test]
async fn unknown_member_inspection_finds_source_without_creating_a_callee_binding() -> Result<()> {
    let owned = tempfile::TempDir::new()?;
    let (app, pid) = indexed(owned.path()).await?;
    let target = app.state.graph.query_nodes(&pid, Some("function"), None, Some(IMAGE), 8)?
        .into_iter().find(|n| n.name == "resetState").unwrap();
    for tool in ["impact_analysis", "compute_blast_radius"] {
        for detail in ["summary", "graph"] {
            let mut args = json!({"project_id":pid,"symbol_fqn":target.node_id,"max_depth":6,
                "detail":detail,"include_guidance":false});
            let reply = if tool == "compute_blast_radius" {
                app.handle_compute_blast_radius(serde_json::from_value(args)?).await?
            } else {
                args.as_object_mut().unwrap().remove("include_guidance");
                app.handle_impact_analysis(serde_json::from_value(args)?).await?
            };
            ensure!(reply.is_error != Some(true));
            let text = &reply.content[0].as_text().unwrap().text;
            ensure!(text.len() <= if detail == "summary" { 12 * 1024 } else { 64 * 1024 });
            let report = packet(text);
            ensure!(report["target_revision_admitted"] == true && report["selected_dependents_observed"] == 0);
            let inspection = &report["terminal_member_inspection"];
            ensure!(inspection["scope"].as_str().unwrap().contains("does not bind"));
            let action = &inspection["action"];
            ensure!(action["tool"] == "grep_project");
            ensure!(action["arguments"]["pattern"] == "resetState"
                && action["arguments"]["path_prefix"] == "src/images/"
                && action["arguments"]["freshness"] == "off");
            let hits = app.handle_grep_project(serde_json::from_value(action["arguments"].clone())?).await?;
            ensure!(hits.is_error != Some(true));
            let hits: Value = serde_json::from_str(&hits.content[0].as_text().unwrap().text)?;
            let hit = hits["matches"].as_array().unwrap().iter().find(|h|
                h["file_path"] == GROUP && h["line"] == 7).unwrap();
            let source_action = &hit["current_source_recovery"];
            let source = app.handle_get_full_method_body(serde_json::from_value(source_action["arguments"].clone())?).await?;
            ensure!(source.is_error != Some(true));
            let source: Value = serde_json::from_str(&source.content[0].as_text().unwrap().text)?;
            ensure!(source["source_file_hash"] == ContentHash::compute(GROUP_BODY.as_bytes()).0);
            ensure!(source["source_code"].as_str().unwrap().contains("h.resetState()"));
        }
    }
    check_receivers(&app, &pid)?;
    settle(&app).await?;
    drop(app);
    Ok(())
}

#[tokio::test]
async fn current_source_ranges_report_their_actual_language_in_json_and_markdown() -> Result<()> {
    let owned = tempfile::TempDir::new()?;
    let (app, pid) = indexed(owned.path()).await?;
    for (file, language, fence) in [("sample.ts", "typescript", "typescript"),
        ("sample.tsx", "tsx", "tsx"), ("sample.js", "javascript", "javascript"),
        ("sample.cs", "csharp", "csharp"), ("sample.vb", "vbnet", "vb")] {
        std::fs::write(owned.path().join("repo").join(file), "// current source\n")?;
        for output_json in [true, false] {
            let reply = app.handle_get_full_method_body(serde_json::from_value(json!({
                "project_id":pid,"file_path":file,"line_start":1,"line_end":1,
                "context_lines":0,"output_json":output_json
            }))?).await?;
            ensure!(reply.is_error != Some(true));
            let text = &reply.content[0].as_text().unwrap().text;
            if output_json { ensure!(serde_json::from_str::<Value>(text)?["language"] == language, "{text}"); }
            else { ensure!(text.contains(&format!("```{fence}\n")), "{text}"); }
        }
    }
    let inline_path = "inline.aspx";
    std::fs::write(owned.path().join("repo").join(inline_path),
        "<script>\nfunction inlineCall() {}\n</script>\n")?;
    let inline = engram_graph::Node {
        node_id: "sym:function:inline.aspx:inlineCall:2".into(), node_type: "function".into(),
        name: "inlineCall".into(), namespace: "memory".into(), language: "javascript".into(),
        file_path: engram_core::RelPath::new(inline_path), start_line: 2, end_line: 2,
        generation: 1, metadata: None,
    };
    app.state.graph.upsert_nodes(&pid, &[inline.clone()])?;
    let reply = app.handle_get_full_method_body(serde_json::from_value(json!({
        "project_id":pid,"fqn":"inlineCall","output_json":true
    }))?).await?;
    ensure!(reply.is_error != Some(true));
    ensure!(serde_json::from_str::<Value>(&reply.content[0].as_text().unwrap().text)?["language"] == "javascript",
        "An indexed inline language must not become its markup file extension");
    settle(&app).await?;
    drop(app);
    Ok(())
}

#[tokio::test]
async fn unbound_receiver_wins_over_legacy_target_kind_line_and_later_unique_global_names() -> Result<()> {
    let owned = tempfile::TempDir::new()?;
    let (app, pid) = indexed(owned.path()).await?;
    let path = std::sync::Arc::new(engram_core::RelPath::new(GROUP));
    let (symbols, _) = engram_index::SymbolExtractor::new().extract(Path::new(GROUP), GROUP_BODY);
    let mut stats = engram_index::IngestStats::default();
    stats.symbols.extend(symbols.into_iter().map(|symbol| (path.clone(), symbol)));
    stats.edges.push((path, engram_index::ExtractedEdge {
        source_name: "Group.clear".into(), source_kind: "function".into(), source_start_line: 5,
        source_language: "ts".into(), target_name: "Group.resetState".into(),
        target_kind: Some("function".into()), target_start_line: Some(4), kind: "calls".into(),
        metadata: Some(std::collections::HashMap::from([
            ("receiver_unbound".into(), "true".into()), ("receiver".into(), "h".into()),
            ("call_site_line".into(), "7".into())])),
    }));
    let generation = engram_server::services::project_service::get_active_generation(&app.state, &pid).await?;
    engram_server::services::ingest_service::process_ingest_stats(&app.state, &pid, generation, &stats).await?;
    let calls = app.state.graph.list_edges(&pid, Some(EdgeKind::Calls))?;
    ensure!(calls.iter().any(|e| e.target_id == format!("{}Group.resetState", engram_graph::store::UNBOUND_RECEIVER_PREFIX)));
    let clear = app.state.graph.query_nodes(&pid, Some("function"), Some("clear"), Some(GROUP), 8)?
        .into_iter().find(|n| n.name == "clear").unwrap();
    // A legacy placeholder may carry the semantic marker without the newer
    // reserved identity. Neither resolver may revive its unique global name.
    let global = engram_graph::Node {
        node_id: "sym:function:Site/App_Code/External.vb:External.resetState:1".into(),
        node_type: "function".into(), name: "External.resetState".into(), namespace: "memory".into(),
        language: "vbnet".into(), file_path: engram_core::RelPath::new("Site/App_Code/External.vb"),
        start_line: 1, end_line: 1, generation, metadata: Some(json!({"fqn":"External.resetState"})),
    };
    app.state.graph.upsert_nodes(&pid, &[global.clone()])?;
    app.state.graph.upsert_edges(&pid, &[engram_graph::Edge {
        source_id: clear.node_id.clone(), target_id: "::External.resetState".into(),
        namespace: "memory".into(), language: "typescript".into(), edge_kind: EdgeKind::Calls,
        weight: 1, generation, metadata: Some(json!({"receiver_unbound":true,"receiver":"h"})),
        updated_at_ms: 1,
    }])?;
    app.state.graph.resolve_symbol_edges(&pid)?;
    engram_server::services::graph_service::resolve_app_code_globals(&app.state.graph, &pid, generation)?;
    let calls = app.state.graph.list_edges(&pid, Some(EdgeKind::Calls))?;
    ensure!(calls.iter().any(|e| e.source_id == clear.node_id && e.target_id == "::External.resetState"));
    ensure!(!calls.iter().any(|e| e.source_id == clear.node_id && e.target_id == global.node_id));
    let traversed = app.state.graph.traverse(&pid, &clear.node_id, 1, Some(vec![EdgeKind::Calls]), "out")?;
    ensure!(!traversed.iter().any(|(node, _)| node.node_id == global.node_id),
        "Legacy adjacency traversal must not revive a marked unknown receiver");
    settle(&app).await?;
    drop(app);
    Ok(())
}
