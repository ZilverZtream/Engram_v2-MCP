//! A source document's symbol bridge respects exact file identity and physical revisions.
use anyhow::{Result, ensure};
use engram_core::{Config, RelPath};
use engram_graph::Node;
use engram_server::{state::AppState, tools::Engram};
use serde_json::{Value, json};

fn run(inherited: bool) -> Result<()> {
    let fixture_root = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let owned = tempfile::Builder::new()
        .prefix("id-")
        .tempdir_in(fixture_root)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let result=runtime.block_on(async {
        let root=owned.path().join("r");std::fs::create_dir_all(root.join("submodule"))?;
        git2::Repository::init(&root)?;
        std::fs::write(root.join("Config.vb"),"Public Class PrimaryConfig\nPublic Function OwnValue() As String\nReturn \"doc_source_marker\"\nEnd Function\nEnd Class\n")?;
        std::fs::write(root.join("submodule/Config.vb"),"Public Class OtherConfig\nPublic Function ForeignValue() As String\nReturn \"foreign_marker\"\nEnd Function\nEnd Class\n")?;
        std::fs::write(root.join("Changed.vb"),"Public Class Before\nEnd Class\n")?;
        let (state,_)=AppState::new(Config{data_dir:owned.path().join("data"),allowed_roots:vec![root.clone()],embedding_backend:"fts_only".into(),llm_backend:"none".into(),..Default::default()})?;
        let engram=Engram::new(state);
        let indexed=engram.handle_index_project(serde_json::from_value(json!({"directory":root,"project_name":"Owned document symbol bridge","project_type":"general","wait":true}))?).await?;
        let text=&indexed.content[0].as_text().ok_or_else(||anyhow::anyhow!("Missing index result"))?.text;
        let pid=text.split("project_id: ").nth(1).and_then(|text|text.lines().next()).ok_or_else(||anyhow::anyhow!("Missing project ID"))?.trim().to_owned();
        let old:u64=engram.state.registry.get_meta(&pid,"active_generation")?.unwrap().parse()?;
        let searched=engram.handle_search_memory(serde_json::from_value(json!({"project_id":pid,"query":"doc_source_marker","output_json":true}))?).await?;
        let search:Value=serde_json::from_str(&searched.content[0].as_text().unwrap().text)?;
        let hit=search["results"].as_array().unwrap().iter().find(|hit|hit["path"]=="Config.vb").unwrap();
        let doc=hit["doc_id"].as_str().unwrap().to_owned();
        let mut current=old;
        if inherited {
            std::fs::write(root.join("Changed.vb"),"Public Class AfterChange\nEnd Class\n")?;
            let updated=engram.handle_update_project(serde_json::from_value(json!({"project_id":pid,"wait":true}))?).await?;
            ensure!(updated.is_error!=Some(true));
            current=engram.state.registry.get_meta(&pid,"active_generation")?.unwrap().parse()?;
            ensure!(current>old);
        }
        // More rejected observations than the display cap must not hide the
        // selected declaration. Logical-head equality cannot authorize a newer
        // graph revision of an inherited file.
        let wrong_revision=if inherited{current}else{old+1};
        let nodes:Vec<_>=(0..6).map(|index|Node{node_id:format!("a-future-{index}"),node_type:"function".into(),name:format!("FutureValue{index}"),namespace:"memory".into(),language:"vbnet".into(),file_path:RelPath::new("Config.vb"),start_line:2,end_line:4,generation:wrong_revision,metadata:None}).collect();
        engram.state.graph.upsert_nodes(&pid,&nodes)?;
        let resolved=engram.handle_resolve_id(serde_json::from_value(json!({"project_id":pid,"id":doc}))?).await?;
        let text=&resolved.content[0].as_text().unwrap().text;
        ensure!(text.contains("resolved: search doc (chunk)") && text.contains("file: Config.vb"),"Document identity lost: {text}");
        ensure!(text.contains("OwnValue") && !text.contains("ForeignValue") && !text.contains("OtherConfig") && !text.contains("FutureValue"),"Wrong source symbols bridged or selected declaration hidden: {text}");
        let withheld:usize=text.split("unselected_observations=").nth(1).and_then(|tail|tail.split(';').next()).ok_or_else(||anyhow::anyhow!("Missing source coverage: {text}"))?.parse()?;
        ensure!(text.contains("symbol_bridge: selected_source") && text.contains(&format!("source_generation={current}")) && text.contains(&format!("physical revision={old}")) && withheld>=6,"Selected physical source coverage missing: {text}");
        Ok::<(),anyhow::Error>(())
    });
    drop(runtime);
    match (result, owned.close()) {
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("Fixture cleanup also failed: {cleanup}")))
        }
        (Err(error), _) => Err(error),
        (Ok(()), cleanup) => cleanup.map_err(Into::into),
    }
}
#[test]
fn source_document_bridge_excludes_same_basename_files_and_future_graph_symbols() -> Result<()> {
    run(false)
}
#[test]
fn source_document_bridge_keeps_inherited_physical_symbols_after_head_advance() -> Result<()> {
    run(true)
}
