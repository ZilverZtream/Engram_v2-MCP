//! Actual handler observations over inherited source and an independently changing graph.
use anyhow::{Result, ensure};
use engram_core::{Config, RelPath};
use engram_graph::{Edge, EdgeKind, Node};
use engram_server::{state::AppState, tools::Engram};
use serde_json::json;

fn node(id: &str, name: &str, path: &str, revision: u64) -> Node {
    Node {
        node_id: id.into(),
        node_type: if id.starts_with("file:") {
            "file"
        } else {
            "function"
        }
        .into(),
        name: name.into(),
        namespace: "memory".into(),
        language: "typescript".into(),
        file_path: RelPath::new(path),
        start_line: 1,
        end_line: 1,
        generation: revision,
        metadata: None,
    }
}
fn edge(source: &str, target: &str, revision: u64) -> Edge {
    Edge {
        source_id: source.into(),
        target_id: target.into(),
        namespace: "memory".into(),
        language: "typescript".into(),
        edge_kind: EdgeKind::Calls,
        weight: 1,
        generation: revision,
        metadata: None,
        updated_at_ms: 1,
    }
}
fn run(expansion: bool) -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let result=runtime.block_on(async {
        let root=temporary.path().join("repo"); std::fs::create_dir_all(root.join("src"))?;
        git2::Repository::init(&root)?;
        let keep="export function stable() { return 'sourcekeepmarker'; }\n";
        for (path,content) in [("keep.ts",keep),("replaced.ts","export function oldVersion() {}\n"),("deleted.ts","export function deletedVersion() {}\n"),("leaf.ts","export function leaf() {}\n")] {
            std::fs::write(root.join("src").join(path),content)?;
        }
        let (state,_)=AppState::new(Config {data_dir:temporary.path().join("data"),allowed_roots:vec![root.clone()],embedding_backend:"fts_only".into(),llm_backend:"none".into(),..Default::default()})?;
        let engram=Engram::new(state);
        let indexed=engram.handle_index_project(serde_json::from_value(json!({"directory":root,"project_name":"Owned graph boundary","project_type":"general","wait":true}))?).await?;
        let body=&indexed.content[0].as_text().ok_or_else(||anyhow::anyhow!("Missing index text"))?.text;
        let pid=body.split("project_id: ").nth(1).and_then(|text|text.lines().next()).ok_or_else(||anyhow::anyhow!("Missing project ID"))?.trim().to_string();
        let old:u64=engram.state.registry.get_meta(&pid,"active_generation")?.unwrap().parse()?;
        std::fs::write(root.join("src/replaced.ts"),"export function currentVersion() {}\n")?;
        std::fs::remove_file(root.join("src/deleted.ts"))?;
        let updated=engram.handle_update_project(serde_json::from_value(json!({"project_id":pid,"wait":true}))?).await?;
        ensure!(updated.is_error!=Some(true));
        let current:u64=engram.state.registry.get_meta(&pid,"active_generation")?.unwrap().parse()?;
        ensure!(current>old);
        let graph=&engram.state.graph;
        graph.upsert_nodes(&pid,&[
            node("fixture:stable","BoundaryNeedleStable","src/keep.ts",old),
            node("fixture:future","BoundaryNeedleFuture","src/replaced.ts",current+1),
            node("fixture:deleted","BoundaryNeedleDeleted","src/deleted.ts",old),
            node("fixture:validleaf","IndependentLeaf","src/leaf.ts",old),
            node("fixture:tail","UnreachableTail","src/leaf.ts",old),
            node("file:src/keep.ts","src/keep.ts","src/keep.ts",if expansion{current+1}else{old}),
        ])?;
        if expansion {
            graph.upsert_edges(&pid,&[
                edge("fixture:stable","fixture:future",old),
                edge("fixture:future","fixture:tail",current+1),
                edge("fixture:stable","fixture:validleaf",old),
                edge("file:src/keep.ts","fixture:tail",current+1),
            ])?;
        }
        let request=|query:&str| serde_json::from_value(json!({"project_id":pid,"query":query,"hop_depth":3,"max_results":30,"include_content":true,"max_content_chars":2000,"expansion_edge_kinds":["calls"]}));
        let result=engram.handle_graph_search(request(if expansion{"BoundaryNeedleStable"}else{"BoundaryNeedle"})?).await?;
        let text=&result.content[0].as_text().ok_or_else(||anyhow::anyhow!("Missing graph response"))?.text;
        ensure!(text.contains("fixture:stable"),"Inherited source symbol missing: {text}");
        ensure!(!text.contains("fixture:future") && !text.contains("fixture:deleted"),"Unselected graph source leaked: {text}");
        ensure!(text.contains(&format!("selected physical revision {old}")) && text.contains("Live graph edges are not an atomic historical snapshot"),"Source boundary missing: {text}");
        if expansion {
            ensure!(text.contains("fixture:validleaf") && !text.contains("fixture:tail"),"Expansion crossed an unselected bridge or lost an admitted neighbor: {text}");
            let lexical=engram.handle_graph_search(request("sourcekeepmarker")?).await?;
            let text=&lexical.content[0].as_text().unwrap().text;
            ensure!(text.contains("file:src/keep.ts") && text.contains(keep.trim()) && !text.contains("fixture:tail"),"Overwritten live file node replaced selected lexical evidence or authorized expansion: {text}");
        }
        Ok::<(),anyhow::Error>(())
    });
    // Runtime teardown waits for owned background work before fixture removal.
    drop(runtime);
    match (result, temporary.close()) {
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("Fixture cleanup also failed: {cleanup}")))
        }
        (Err(error), _) => Err(error),
        (Ok(()), cleanup) => cleanup.map_err(Into::into),
    }
}
#[test]
fn inherited_symbols_survive_while_future_and_deleted_graph_seeds_are_withheld() -> Result<()> {
    run(false)
}
#[test]
fn unselected_expansion_bridges_cannot_reach_selected_tails_and_lexical_source_survives()
-> Result<()> {
    run(true)
}
