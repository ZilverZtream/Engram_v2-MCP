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

#[test]
fn capped_empty_receiver_scan_keeps_executable_member_recovery_beside_known_callers() -> Result<()> {
    let owned = tempfile::TempDir::new()?;
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    runtime.block_on(async {
        let handler = "src/ts/images/imageUploadHandler.ts";
        let manager = "src/ts/images/uploadBatchManager.ts";
        let mut body = String::from("class ImageUploadHandler {\n");
        for (line, method) in [
            (190, " deleteFromServer() { this.resetState(); }\n"),
            (600, " cancelUpload() {}\n"),
            (900, " deleteImage() { this.resetState(); this.deleteFromServer(); }\n"),
            (1400, " loadImage() { this.resetState(); }\n"),
            (1700, " resetState() { this.cancelUpload(); }\n"),
        ] {
            while body.lines().count() + 1 < line { body.push('\n'); }
            body.push_str(method);
        }
        body.push_str("}\n");
        // A real source directory larger than the unresolved inspection budget.
        // Its unrelated calls sort before the useful collection consumer.
        let noise = format!("class EarlierDeclarations {{\n{}\n}}\n",
            (0..600).map(|n| format!(" earlier{n:04}(other: any) {{ other.touch(); }}\n")).collect::<String>());
        let consumer = "class UploadBatchManager {\n cancelAll(groups: any[]) {\n  for (const group of groups) {\n   for (const handler of group.imageHandlers) {\n    handler.cancelUpload();\n   }\n  }\n }\n}\n";
        for (file, text) in [(handler, body.as_str()), (manager, consumer),
            ("src/ts/images/aaaEarlierDeclarations.ts", noise.as_str())] {
            let path = owned.path().join("repo").join(file);
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::write(path, text)?;
        }
        let app = open(owned.path()).await?;
        let indexed = app.handle_index_project(serde_json::from_value(json!({
            "directory":owned.path().join("repo"),"project_name":"Capped receiver recovery",
            "project_type":"general","wait":true
        }))?).await?;
        ensure!(indexed.is_error != Some(true));
        settle(&app).await?;
        let pid = app.state.registry.list_projects()?[0].project_id.clone();
        let target = app.state.graph.query_nodes(&pid, Some("function"), None, Some(handler), 32)?
            .into_iter().find(|node| node.name == "cancelUpload").unwrap();
        let before = serde_json::to_value(app.state.graph.list_edges(&pid, None)?)?;
        for tool in ["impact_analysis", "compute_blast_radius"] {
            let mut args = json!({"project_id":pid,"symbol_fqn":target.node_id,
                "max_depth":6,"detail":"summary","include_guidance":false});
            let reply = if tool == "compute_blast_radius" {
                app.handle_compute_blast_radius(serde_json::from_value(args)?).await?
            } else {
                args.as_object_mut().unwrap().remove("include_guidance");
                app.handle_impact_analysis(serde_json::from_value(args)?).await?
            };
            ensure!(reply.is_error != Some(true));
            let text = &reply.content[0].as_text().unwrap().text;
            ensure!(text.len() <= 12 * 1024);
            let report = packet(text);
            ensure!(report["direct_dependents_observed"] == 1
                && report["transitive_dependents_observed"] == 3);
            ensure!(report["immediate_consumers"][0]["name"] == "resetState");
            let scan = &report["unbound_member_call_inspection"];
            ensure!(scan["coverage"]["truncated"] == true && scan["distinct_callers_observed"] == 0,
                "This scenario requires an empty capped scan, not absence of source consumers: {scan}");
            let inspection = &report["terminal_member_inspection"];
            ensure!(inspection["scope"].as_str().is_some_and(|s| s.contains("does not bind")),
                "Recovery must remain explicitly separate from admitted callers: {report}");
            let action = &inspection["action"];
            ensure!(action["tool"] == "grep_project" && action["arguments"]["pattern"] == ".cancelUpload");
            let hits = app.handle_grep_project(serde_json::from_value(action["arguments"].clone())?).await?;
            ensure!(hits.is_error != Some(true));
            let hits: Value = serde_json::from_str(&hits.content[0].as_text().unwrap().text)?;
            let hit = hits["matches"].as_array().unwrap().iter().find(|row|
                row["file_path"] == manager && row["line"] == 5).unwrap();
            let source = app.handle_get_full_method_body(serde_json::from_value(
                hit["current_source_recovery"]["arguments"].clone())?).await?;
            ensure!(source.is_error != Some(true));
            let source: Value = serde_json::from_str(&source.content[0].as_text().unwrap().text)?;
            ensure!(source["source_file_hash"] == ContentHash::compute(consumer.as_bytes()).0);
            ensure!(source["source_code"].as_str().unwrap().contains("handler.cancelUpload();"));
            ensure!(report["change_breakage_proven"] == false && report["exhaustive_codebase"] == false);
        }
        ensure!(before == serde_json::to_value(app.state.graph.list_edges(&pid, None)?)?);
        settle(&app).await?;
        drop(app);
        Ok::<(), anyhow::Error>(())
    })?;
    drop(runtime);
    owned.close()?;
    Ok(())
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
            ensure!(action["arguments"]["pattern"] == ".resetState"
                && action["arguments"]["path_prefix"] == "src/images/"
                && action["arguments"]["freshness"] == "off");
            ensure!(inspection["unrestricted_fallback"]["arguments"]["pattern"] == "resetState");
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
async fn property_read_recovery_reaches_editor_before_bare_name_noise_without_claiming_callers() -> Result<()> {
    let owned = tempfile::TempDir::new()?;
    let getter = "class Layer {\n get policy() { return { automaticZoom: 8 }; }\n}\n";
    let editor = format!("class Editor {{\n threshold(layer: Layer) {{\n{}  return layer.policy.automaticZoom;\n }}\n}}\n",
        "  // policy belongs to the layer; ordinary prose is not a member read.\n".repeat(12));
    for (path, body) in [("src/map/layer.ts", getter), ("src/map/editor.ts", editor.as_str())] {
        let file = owned.path().join("repo").join(path);
        std::fs::create_dir_all(file.parent().unwrap())?;
        std::fs::write(file, body)?;
    }
    let app = open(owned.path()).await?;
    let indexed = app.handle_index_project(serde_json::from_value(json!({
        "directory":owned.path().join("repo"),"project_name":"Property read recovery",
        "project_type":"general","wait":true
    }))?).await?;
    ensure!(indexed.is_error != Some(true));
    settle(&app).await?;
    let pid = app.state.registry.list_projects()?[0].project_id.clone();
    let target = app.state.graph.query_nodes(&pid, Some("function"), None, Some("src/map/layer.ts"), 8)?
        .into_iter().find(|node| node.name == "policy").unwrap();
    for blast in [false, true] {
        for detail in ["summary", "graph"] {
            let mut args = json!({"project_id":pid,"symbol_fqn":target.node_id,"max_depth":6,"detail":detail});
            let reply = if blast {
                args["include_guidance"] = json!(false);
                app.handle_compute_blast_radius(serde_json::from_value(args)?).await?
            } else {
                app.handle_impact_analysis(serde_json::from_value(args)?).await?
            };
            ensure!(reply.is_error != Some(true));
            let text = &reply.content[0].as_text().unwrap().text;
            ensure!(text.len() <= if detail == "summary" { 12 * 1024 } else { 64 * 1024 });
            let report = packet(text);
            ensure!(report["target_revision_admitted"] == true && report["selected_dependents_observed"] == 0);
            ensure!(report["exhaustive_codebase"] == false && report["change_breakage_proven"] == false);
            let inspection = &report["terminal_member_inspection"];
            let hits = app.handle_grep_project(serde_json::from_value(inspection["action"]["arguments"].clone())?).await?;
            ensure!(hits.is_error != Some(true));
            let hits: Value = serde_json::from_str(&hits.content[0].as_text().unwrap().text)?;
            let hit = hits["matches"].as_array().unwrap().iter().find(|hit|
                hit["file_path"] == "src/map/editor.ts" && hit["line_text"].as_str().unwrap().contains("layer.policy.automaticZoom"))
                .ok_or_else(|| anyhow::anyhow!("First recovery must reach the member read before bare-name comments fill its result budget: {hits}"))?;
            let read = app.handle_get_full_method_body(serde_json::from_value(hit["current_source_recovery"]["arguments"].clone())?).await?;
            ensure!(read.is_error != Some(true));
            let read: Value = serde_json::from_str(&read.content[0].as_text().unwrap().text)?;
            ensure!(read["source_file_hash"] == ContentHash::compute(editor.as_bytes()).0);
            ensure!(read["source_code"].as_str().unwrap().contains("return layer.policy.automaticZoom;"));
            let broad = app.handle_grep_project(serde_json::from_value(inspection["unrestricted_fallback"]["arguments"].clone())?).await?;
            ensure!(broad.is_error != Some(true));
            let broad: Value = serde_json::from_str(&broad.content[0].as_text().unwrap().text)?;
            ensure!(broad["result_limit_reached"] == true);
            ensure!(!broad["matches"].as_array().unwrap().iter().any(|hit|
                hit["line_text"].as_str().unwrap().contains("layer.policy.automaticZoom")),
                "Fixture must exercise a crowded broad search rather than merely compare action spelling");
        }
    }
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

#[test]
fn code_map_safety_actions_disclose_unassessed_state_and_cancellation_without_losing_callers() -> Result<()> {
    let owned = tempfile::TempDir::new()?;
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    runtime.block_on(async {
        const FILE: &str = "src/state.ts";
        const SOURCE: &str = "class ImageState {\n private generation = 0;\n private request: any = null;\n private preview = \"\";\n resetState() {\n  this.generation += 1;\n  this.request?.abort();\n  this.request = null;\n  this.preview = \"\";\n }\n deleteOne() { this.resetState(); }\n reload() { this.resetState(); }\n}\n";
        std::fs::create_dir_all(owned.path().join("repo/src"))?;
        std::fs::write(owned.path().join("repo").join(FILE), SOURCE)?;
        let (app, pid) = indexed(owned.path()).await?;
        let target = app.state.graph.query_nodes(&pid, Some("function"), Some("resetState"), Some(FILE), 16)?
            .into_iter().find(|node| node.file_path.as_str() == FILE && node.name == "resetState").unwrap();
        let navigation = app.handle_get_codebase_overview(serde_json::from_value(json!({
            "project_id":pid,"scope":FILE,"symbol_node_id":target.node_id,"limit":1
        }))?).await?;
        ensure!(navigation.is_error != Some(true));
        let nav: Value = serde_json::from_str(navigation.content[0].as_text().unwrap().text
            .split("## CodeMap Navigation").nth(1).unwrap()
            .split("```json").nth(1).unwrap().split("```").next().unwrap())?;
        let declaration = &nav["files"][0]["symbols"][0];
        ensure!(declaration["node_id"] == target.node_id);
        let actions = declaration["actions"].as_array().unwrap();
        let read = actions.iter().find(|action| action["tool"] == "get_full_method_body").unwrap();
        let response = app.handle_get_full_method_body(serde_json::from_value(read["arguments"].clone())?).await?;
        ensure!(response.is_error != Some(true));
        let source: Value = serde_json::from_str(&response.content[0].as_text().unwrap().text)?;
        ensure!(source["file_path"] == FILE && source["language"] == "typescript");
        ensure!(source["source_file_hash"] == blake3::hash(SOURCE.as_bytes()).to_hex().to_string());
        ensure!(source["line_start"] == 5 && source["line_end"] == 10);
        ensure!(source["source_code"].as_str().unwrap().contains("this.request?.abort()"));
        ensure!(source["source_code"].as_str().unwrap().contains("this.generation += 1"));
        let inspect = actions.iter().find(|action| action["tool"] == "get_method_edit_context").unwrap();
        let response = app.handle_get_method_edit_context(serde_json::from_value(inspect["arguments"].clone())?).await?;
        ensure!(response.is_error != Some(true));
        ensure!(response.content[0].as_text().unwrap().text.len()<=20*1024,
            "This two-caller agent context must preserve its evidence within20KiB");
        let context: Value = serde_json::from_str(&response.content[0].as_text().unwrap().text)?;
        ensure!(context["method_info"]["return_type"] == "unknown" && context["method_info"]["access_level"] == "unknown",
            "Unavailable signature facts must remain unknown");
        ensure!(context["method_info"]["called_by"].as_array().unwrap().len() == 2);
        ensure!(context["risk_band_scope"].as_str().unwrap().contains("one-hop"));
        let response = app.handle_check_edit_safety(serde_json::from_value(json!({
            "project_id":pid,"file_path":FILE,"method_name":"resetState","line":5,"output_json":true
        }))?).await?;
        ensure!(response.is_error != Some(true));
        ensure!(response.content[0].as_text().unwrap().text.len()<=16*1024,
            "This two-caller standalone safety evidence must fit16KiB");
        let standalone: Value = serde_json::from_str(&response.content[0].as_text().unwrap().text)?;
        ensure!(context["edit_safety"] == standalone, "Both safety tools must use the same evidence and uncertainty");
        ensure!(standalone["verdict"] == "yellow" && standalone["confidence"].as_f64().unwrap() <= 0.5);
        ensure!(standalone["completeness"]["callers"]["status"] == "complete");
        ensure!(standalone["completeness"]["method_semantics"]["status"] == "not_run");
        ensure!(standalone["completeness"]["sync_hazards"]["status"] == "not_run");
        let reasons = standalone["reasons"].to_string();
        ensure!(reasons.contains("method semantics") && reasons.contains("unassessed"));
        ensure!(!reasons.contains("no complex state detected"));
        let downstream = &standalone["downstream_impact"];
        ensure!(downstream["target_revision_admitted"] == true && downstream["selected_dependents_observed"] == 2);
        ensure!(downstream["source_generation"] == nav["source_generation"]);
        let impact = actions.iter().find(|action| action["tool"] == "impact_analysis").unwrap();
        let response = app.handle_impact_analysis(serde_json::from_value(impact["arguments"].clone())?).await?;
        ensure!(response.is_error != Some(true));
        let impact = packet(&response.content[0].as_text().unwrap().text);
        ensure!(impact["target_revision_admitted"] == true && impact["target"]["node_id"] == target.node_id);
        ensure!(impact["source_generation"] == downstream["source_generation"]);
        ensure!(impact["selected_dependents_observed"] == downstream["selected_dependents_observed"]);
        // The other owned source's collection receiver remains unbound.
        check_receivers(&app, &pid)?;
        settle(&app).await?;
        drop(app);
        Ok::<(), anyhow::Error>(())
    })?;
    drop(runtime);
    owned.close()?;
    Ok(())
}

#[test]
fn unbound_same_terminal_calls_are_inspectable_without_becoming_primary_callers() -> Result<()> {
    fn target_id(report: &Value) -> Option<&str> {
        report["target"]["node_id"].as_str().or_else(|| report["nodes"].as_array()?
            .iter().find(|row| row["ref"] == report["target_ref"])?["node_id"].as_str())
    }
    let owned = tempfile::TempDir::new()?;
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    runtime.block_on(async {
        const OTHER: &str = "src/images/unrelated.ts";
        const ROOT_SOURCE: &str = "class Image {\n resetState() {}\n deleteOne() { this.resetState(); }\n reload() { this.resetState(); }\n openImage() { this.resetState(); }\n}\n";
        const OTHER_SOURCE: &str = "class Other {\n resetState() {}\n probe(other: any) { other.resetState(); }\n}\n";
        for (file, text) in [(IMAGE, ROOT_SOURCE), (GROUP, GROUP_BODY), (OTHER, OTHER_SOURCE)] {
            let path = owned.path().join("repo").join(file);
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::write(path, text)?;
        }
        let app = open(owned.path()).await?;
        let indexed = app.handle_index_project(serde_json::from_value(json!({
            "directory":owned.path().join("repo"),"project_name":"Unbound inspection",
            "project_type":"general","wait":true
        }))?).await?;
        ensure!(indexed.is_error != Some(true));
        settle(&app).await?;
        let pid = app.state.registry.list_projects()?[0].project_id.clone();
        let nodes = app.state.graph.query_nodes(&pid, Some("function"), None, None, 32)?;
        let named = |file: &str, name: &str| nodes.iter()
            .find(|node| node.file_path.as_str() == file && node.name == name).unwrap();
        let target = named(IMAGE, "resetState");
        let group_caller = named(GROUP, "clear");
        let other_caller = named(OTHER, "probe");
        let stored = app.state.graph.list_edges(&pid, Some(EdgeKind::Calls))?;
        let primary: std::collections::BTreeSet<_> = stored.iter().filter(|edge| edge.target_id == target.node_id)
            .map(|edge| edge.source_id.as_str()).collect();
        let wanted: std::collections::BTreeSet<_> = ["deleteOne", "reload", "openImage"]
            .map(|name| named(IMAGE, name).node_id.as_str()).into_iter().collect();
        ensure!(primary == wanted && primary.len() == 3);
        let before = serde_json::to_value(app.state.graph.list_edges(&pid, None)?)?;
        check_receivers(&app, &pid)?;
        for tool in ["impact_analysis", "compute_blast_radius"] {
            for detail in ["summary", "graph"] {
                let mut arguments = json!({"project_id":pid,"symbol_fqn":target.node_id,
                    "max_depth":6,"detail":detail,"include_guidance":false});
                let reply = if tool == "compute_blast_radius" {
                    app.handle_compute_blast_radius(serde_json::from_value(arguments)?).await?
                } else {
                    arguments.as_object_mut().unwrap().remove("include_guidance");
                    app.handle_impact_analysis(serde_json::from_value(arguments)?).await?
                };
                ensure!(reply.is_error != Some(true));
                let text = &reply.content[0].as_text().unwrap().text;
                ensure!(text.len() <= if detail == "summary" { 12 * 1024 } else { 64 * 1024 });
                let mut report = packet(text);
                ensure!(report["target_revision_admitted"] == true && target_id(&report) == Some(target.node_id.as_str()));
                ensure!(report["selected_dependents_observed"] == 3);
                let generation = report["source_generation"].clone();
                if report["unbound_member_call_inspection"]["candidates"].as_array().map_or(0, Vec::len) != 2 {
                    ensure!(detail == "summary", "Full graph must retain the small witness fixture: {report}");
                    let next = &report["graph_continuation"];
                    ensure!(next["tool"] == "impact_analysis");
                    let expanded = app.handle_impact_analysis(serde_json::from_value(next["arguments"].clone())?).await?;
                    ensure!(expanded.is_error != Some(true));
                    let expanded = &expanded.content[0].as_text().unwrap().text;
                    ensure!(expanded.len() <= 64 * 1024);
                    report = packet(expanded);
                    ensure!(report["source_generation"] == generation && report["selected_dependents_observed"] == 3);
                }
                let inspection = &report["unbound_member_call_inspection"];
                ensure!(inspection["scope"].as_str().unwrap().contains("does not bind"));
                let candidates = inspection["candidates"].as_array().unwrap();
                ensure!(candidates.len() == 2);
                for (caller, receiver, lines, text) in [
                    (group_caller, "h", json!([7,8]), GROUP_BODY),
                    (other_caller, "other", json!([3]), OTHER_SOURCE),
                ] {
                    let row = candidates.iter().find(|row| row["caller"]["node_id"] == caller.node_id).unwrap();
                    ensure!(row["caller"]["file_path"] == caller.file_path.as_str() && row["caller"]["generation"] == caller.generation);
                    ensure!(row["receiver"] == receiver && row["reference_lines"] == lines);
                    let unresolved = row["unresolved_target_id"].as_str().unwrap();
                    ensure!(unresolved.starts_with(engram_graph::store::UNBOUND_RECEIVER_PREFIX)
                        && unresolved.ends_with(&format!("{receiver}.resetState")));
                    ensure!(matches!(app.state.graph.resolve_symbol(&pid, unresolved, None, None)?, engram_graph::store::ResolveResult::NotFound));
                    let actions = row["actions"].as_array().unwrap();
                    ensure!(actions.len()==3, "File context is separate from the existing witness/context/impact actions");
                    let read = actions.iter().find(|action| action["tool"] == "get_full_method_body").unwrap();
                    let read = app.handle_get_full_method_body(serde_json::from_value(read["arguments"].clone())?).await?;
                    ensure!(read.is_error != Some(true));
                    let read: Value = serde_json::from_str(&read.content[0].as_text().unwrap().text)?;
                    ensure!(read["file_path"] == caller.file_path.as_str() && read["source_file_hash_algorithm"] == "blake3");
                    ensure!(read["source_file_hash"] == blake3::hash(text.as_bytes()).to_hex().to_string());
                    ensure!(read["source_code"].as_str().unwrap().contains(&format!("{receiver}.resetState()")));
                    let context = actions.iter().find(|action| action["tool"] == "get_method_edit_context").unwrap();
                    let context = app.handle_get_method_edit_context(serde_json::from_value(context["arguments"].clone())?).await?;
                    ensure!(context.is_error != Some(true));
                    let context: Value = serde_json::from_str(&context.content[0].as_text().unwrap().text)?;
                    ensure!(context["method_info"]["file_path"] == caller.file_path.as_str()
                        && context["method_info"]["line_start"] == caller.start_line);
                    let impact = actions.iter().find(|action| action["tool"] == "impact_analysis").unwrap();
                    ensure!(impact["arguments"]["symbol_fqn"] == caller.node_id);
                    let impact = app.handle_impact_analysis(serde_json::from_value(impact["arguments"].clone())?).await?;
                    ensure!(impact.is_error != Some(true));
                    let impact = packet(&impact.content[0].as_text().unwrap().text);
                    ensure!(impact["target_revision_admitted"] == true && target_id(&impact) == Some(caller.node_id.as_str())
                        && impact["source_generation"] == generation);
                }
            }
        }
        ensure!(before == serde_json::to_value(app.state.graph.list_edges(&pid, None)?)?);
        check_receivers(&app, &pid)?;
        // Type/construction context can precede the caller while its actual
        // invocation and escaped-array getter live on later source pages.
        let mut group_source=String::from("/// <reference path=\"image.ts\" />\nclass Group {\n private images: Array<Image> = [];\n constructor() { this.images.push(new Image()); }\n resetState() { this.clear(); }\n");
        while group_source.lines().count()<128 {group_source.push('\n');}
        group_source.push_str(" clear() {\n  for (const h of this.images) {\n   h.resetState();\n   h.resetState();\n  }\n }\n loadAll() { this.clear(); }\n");
        while group_source.lines().count()<260 {group_source.push('\n');}
        group_source.push_str(" get handlers(): Array<Image> { return this.images; }\n}\n");
        std::fs::write(owned.path().join("repo").join(GROUP),&group_source)?;
        let updated=app.handle_update_project(serde_json::from_value(json!({
            "project_id":pid,"only_paths":[GROUP],"wait":true,"index_antipatterns":false}))?).await?;
        ensure!(updated.is_error!=Some(true));settle(&app).await?;
        let generation=engram_server::services::project_service::get_active_generation(&app.state,&pid).await?;
        let current=app.state.graph.query_nodes(&pid,Some("function"),None,Some(GROUP),64)?;
        let caller=current.iter().find(|node|node.name=="clear" && node.generation==generation).unwrap();
        let before_context=serde_json::to_value(app.state.graph.list_edges(&pid,None)?)?;
        for tool in ["impact_analysis","compute_blast_radius"] {
            for detail in ["summary","graph"] {
                let mut args=json!({"project_id":pid,"symbol_fqn":target.node_id,"max_depth":6,"detail":detail,"include_guidance":false});
                let response=if tool=="compute_blast_radius" {app.handle_compute_blast_radius(serde_json::from_value(args)?).await?}
                    else {args.as_object_mut().unwrap().remove("include_guidance");app.handle_impact_analysis(serde_json::from_value(args)?).await?};
                ensure!(response.is_error!=Some(true));
                let text=&response.content[0].as_text().unwrap().text;
                ensure!(text.len()<=if detail=="summary" {12*1024}else{64*1024});
                let mut report=packet(text);
                ensure!(report["selected_dependents_observed"]==3 && report["source_generation"]==generation);
                let has_context=report["unbound_member_call_inspection"]["candidates"].as_array().is_some_and(|rows|
                    rows.iter().any(|row|row["caller"]["node_id"]==caller.node_id && row.get("caller_file_start_inspection").is_some()));
                if !has_context {
                    ensure!(detail=="summary","Small graph retains source-context recovery");
                    let next=&report["graph_continuation"];ensure!(next["tool"]=="impact_analysis");
                    let expanded=app.handle_impact_analysis(serde_json::from_value(next["arguments"].clone())?).await?;
                    ensure!(expanded.is_error!=Some(true));report=packet(&expanded.content[0].as_text().unwrap().text);
                }
                let inspection=&report["unbound_member_call_inspection"];
                ensure!(serde_json::to_vec(inspection)?.len()<=8*1024);
                let row=inspection["candidates"].as_array().unwrap().iter().find(|row|row["caller"]["node_id"]==caller.node_id).unwrap();
                ensure!(serde_json::to_vec(row)?.len()<=4096 && row["actions"].as_array().unwrap().len()==3);
                let header=&row["caller_file_start_inspection"];
                ensure!(header["scope"].as_str().unwrap().contains("does not bind"));
                let action=&header["action"];ensure!(action["tool"]=="get_full_method_body");
                ensure!(action["arguments"]["file_path"]==GROUP && action["arguments"]["line_start"]==1
                    && action["arguments"].get("line_end").is_none() && action["arguments"].get("fqn").is_none());
                let page=app.handle_get_full_method_body(serde_json::from_value(action["arguments"].clone())?).await?;
                ensure!(page.is_error!=Some(true));let page:Value=serde_json::from_str(&page.content[0].as_text().unwrap().text)?;
                ensure!(page["retrieval_scope"]=="current_file_excerpt" && page["line_start"]==1
                    && page["file_excerpt"]["returned_lines"].as_u64().unwrap()<=120);
                ensure!(page["source_file_hash_algorithm"]=="blake3" && page["source_file_hash"]==ContentHash::compute(group_source.as_bytes()).0);
                let source=page["source_code"].as_str().unwrap();
                ensure!(source.contains("Array<Image>") && source.contains("new Image()") && !source.contains("h.resetState()"));
                let next=&page["file_excerpt"]["next_page"];ensure!(next["tool"]=="get_full_method_body");
                let next_page=app.handle_get_full_method_body(serde_json::from_value(next["arguments"].clone())?).await?;
                ensure!(next_page.is_error!=Some(true));let next_page:Value=serde_json::from_str(&next_page.content[0].as_text().unwrap().text)?;
                ensure!(next_page["source_file_hash"]==page["source_file_hash"] && next_page["source_code"].as_str().unwrap().contains("h.resetState()"));
                let witness=&row["actions"][0];ensure!(witness["tool"]=="get_full_method_body");
                let witness=app.handle_get_full_method_body(serde_json::from_value(witness["arguments"].clone())?).await?;
                ensure!(witness.is_error!=Some(true));let witness:Value=serde_json::from_str(&witness.content[0].as_text().unwrap().text)?;
                ensure!(witness["source_file_hash"]==page["source_file_hash"] && witness["source_code"].as_str().unwrap().contains("h.resetState()"));
                ensure!(matches!(app.state.graph.resolve_symbol(&pid,row["unresolved_target_id"].as_str().unwrap(),None,None)?,engram_graph::store::ResolveResult::NotFound));
            }
        }
        ensure!(before_context==serde_json::to_value(app.state.graph.list_edges(&pid,None)?)?);
        settle(&app).await?;
        drop(app);
        Ok::<(), anyhow::Error>(())
    })?;
    drop(runtime);
    owned.close()?;
    Ok(())
}
