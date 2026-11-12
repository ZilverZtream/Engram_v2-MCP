//! Authored Rust source -> actual extraction/ingestion -> public references.
use super::*;
use engram_core::{NodeId, RelPath};
use engram_graph::EdgeKind;
use std::sync::Arc;
fn stats(files: &[(&str, &str)]) -> engram_index::IngestStats {
    let mut result = engram_index::IngestStats::default();
    for &(name, source) in files {
        let path = Arc::new(RelPath::new(name));
        let (symbols, edges) = engram_index::parsing::SymbolExtractor::new()
            .extract(std::path::Path::new(name), source);
        result
            .fingerprints
            .push(engram_index::docstore::FileFingerprint {
                rel_path: name.into(),
                size: source.len() as u64,
                mtime_ms: 1,
                mtime_ns: None,
                file_hash: blake3::hash(source.as_bytes()).to_hex().to_string(),
            });
        result.all_files.push((*path).clone());
        result
            .symbols
            .extend(symbols.into_iter().map(|s| (path.clone(), s)));
        result
            .edges
            .extend(edges.into_iter().map(|e| (path.clone(), e)));
    }
    result
}
fn state(root: &std::path::Path) -> anyhow::Result<AppState> {
    Ok(AppState::new(engram_core::Config {
        data_dir: root.join("data"),
        allowed_roots: vec![root.to_path_buf()],
        embedding_backend: "fts_only".into(),
        ..Default::default()
    })?
    .0)
}
fn outgoing(state: &AppState, caller: &str) -> anyhow::Result<Vec<engram_graph::Edge>> {
    let nodes = state
        .graph
        .query_nodes("rsmodules", Some("function"), Some(caller), None, 100)?;
    let node = nodes.iter().find(|n| n.name == caller).unwrap();
    Ok(state
        .graph
        .edges_touching("rsmodules", &node.node_id, 100)?
        .into_iter()
        .filter(|e| e.edge_kind == EdgeKind::Calls && e.source_id == node.node_id)
        .collect())
}
fn unbound(state: &AppState, caller: &str, reason: &str) -> anyhow::Result<()> {
    let edges = outgoing(state, caller)?;
    assert_eq!(edges.len(), 1, "{caller}: {edges:?}");
    assert!(
        edges[0]
            .target_id
            .starts_with(engram_core::rust_modules::UNBOUND_PREFIX),
        "{caller}: {edges:?}"
    );
    assert_eq!(
        edges[0].metadata.as_ref().unwrap()["rust_module_bindings_v1"][0]["reason"],
        reason,
        "{caller}"
    );
    Ok(())
}
const CARGO: &str = "[package]\nname='owned'\nversion='0.1.0'\nedition='2021'\n";

#[tokio::test]
async fn rust_modules_bind_use_trees_qualified_generic_inline_and_file_calls() -> anyhow::Result<()>
{
    let temporary = tempfile::tempdir()?;
    let result=async {
        let state=state(temporary.path())?;
        let source="pub mod left;\npub mod right;\npub mod folder;\npub mod barrel;\n#[path=\"renamed.rs\"] pub mod custom;\npub mod inline { pub fn save()->i32 { 44 } pub fn RunSelf()->i32 {self::save()} pub mod nested { use super::save as outer; pub fn RunSuper()->i32 {outer()} } }\nuse crate::left::{self as l,save as persist};\nuse crate::barrel::public_save;\nuse crate::folder::*;\npub fn RunNamed()->i32 { persist() }\npub fn RunModule()->i32 {l::save()}\npub fn RunQualified()->i32 {crate::right::save()}\npub fn RunInline()->i32 {crate::inline::save()}\npub fn RunFolder()->i32 {from_folder()}\npub fn RunBarrel()->i32 {public_save()}\npub fn RunCustom()->i32 {custom::save()}\npub fn RunGeneric()->i32 {crate::left::generic::<u32>()}\npub fn RunLocal()->i32 {use crate::right::save as persist;persist()}\npub fn RunShadow(persist:fn()->i32)->i32 {persist()}\npub fn RunOrdering()->i32 {let before=persist();let persist=||0;before+persist()}\n";
        process_ingest_stats(&state,"rsmodules",1,&stats(&[
            ("Cargo.toml",CARGO),("src/lib.rs",source),
            ("src/left.rs","pub fn save()->i32 {11}\npub fn generic<T>()->i32 {12}\n"),
            ("src/right.rs","pub fn save()->i32 {22}\n"),("src/folder/mod.rs","pub fn from_folder()->i32 {33}\n"),
            ("src/barrel.rs","pub use crate::left::save as public_save;\n"),("src/renamed.rs","pub fn save()->i32 {55}\n"),
            ("decoy.rs","pub fn persist()->i32 {99}\n"),
        ])).await?;
        state.graph.resolve_symbol_edges("rsmodules")?;
        let app=crate::tools::Engram::new(state.clone());
        for (caller,file,name,line) in [
            ("RunNamed","src/left.rs","save",1),("RunModule","src/left.rs","save",1),
            ("RunQualified","src/right.rs","save",1),("RunInline","src/lib.rs","save",6),
            ("RunFolder","src/folder/mod.rs","from_folder",1),("RunBarrel","src/left.rs","save",1),
            ("RunCustom","src/renamed.rs","save",1),("RunGeneric","src/left.rs","generic",2),
            ("RunLocal","src/right.rs","save",1),("RunSelf","src/lib.rs","save",6),("RunSuper","src/lib.rs","save",6),
        ] {
            let edges=outgoing(&state,caller)?;assert_eq!(edges.len(),1,"{caller}: {edges:?}");
            let target=NodeId::symbol("function",None,file,name,line).0;assert_eq!(edges[0].target_id,target,"{caller}");
            let reply=app.handle_find_references(serde_json::from_value(serde_json::json!({"project_id":"rsmodules","node_id":edges[0].source_id,"edge_kind":"calls","direction":"out"}))?).await.map_err(|e|anyhow::anyhow!("{e}"))?;
            let shown=&reply.content[0].as_text().unwrap().text;
            assert!(shown.contains(&target) && shown.contains("syntax_only_not_compiler_or_runtime_proof") && shown.contains("call_site"),"{shown}");
            assert!(!shown.contains("decoy.rs"),"{shown}");
            let binding=&edges[0].metadata.as_ref().unwrap()["rust_module_bindings_v1"][0];
            let at=&binding["call"]["site"];let start=at["offset"].as_u64().unwrap() as usize;let end=at["end_offset"].as_u64().unwrap() as usize;
            assert!(!source[start..end].is_empty());assert_eq!(at["line"].as_u64().unwrap(),source[..start].bytes().filter(|b|*b==b'\n').count() as u64+1);
        }
        unbound(&state,"RunShadow","shadowed_value_binding")?;
        let ordering=outgoing(&state,"RunOrdering")?;assert_eq!(ordering.len(),2);
        assert!(ordering.iter().any(|e|e.target_id=="sym:function:src/left.rs:save:1"));
        assert!(ordering.iter().any(|e|e.target_id.starts_with(engram_core::rust_modules::UNBOUND_PREFIX)));
        assert_eq!(state.graph.resolve_symbol_edges("rsmodules")?,0,"Stable module evidence is idempotent");Ok(())
    }.await;
    temporary.close()?;
    result
}

#[tokio::test]
async fn rust_modules_keep_ambiguous_cyclic_cfg_private_and_receiver_calls_unbound()
-> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let result=async {
        let state=state(temporary.path())?;
        process_ingest_stats(&state,"rsmodules",1,&stats(&[
            ("Cargo.toml",CARGO),
            ("src/lib.rs","mod left;mod right;mod a;mod b;\n#[cfg(feature=\"optional\")] mod optional;\nuse crate::left::*;use crate::right::*;\npub fn RunGlob(){save();}\npub fn RunCycle(){a::cycle();}\npub fn RunCfg(){optional::save();}\npub fn RunPrivate(){left::private();}\npub fn RunReceiver(item:Thing){item.save();}\npub fn RunExternal(){external::save();}\n"),
            ("src/left.rs","pub fn save(){} fn private(){}"),("src/right.rs","pub fn save(){}"),
            ("src/optional.rs","pub fn save(){}"),
            ("src/a.rs","pub use crate::b::cycle;"),("src/b.rs","pub use crate::a::cycle;"),
        ])).await?;
        state.graph.resolve_symbol_edges("rsmodules")?;
        for (caller,reason) in [("RunGlob","ambiguous_glob"),("RunCycle","cyclic_import"),("RunCfg","conditional_compilation"),
            ("RunPrivate","private_binding"),("RunReceiver","unsupported_receiver_or_associated_call"),("RunExternal","name_not_found")] {
            unbound(&state,caller,reason)?;
        }
        Ok(())
    }.await;
    temporary.close()?;
    result
}

#[tokio::test]
async fn rust_modules_cargo_roots_path_dependencies_and_manifest_edits_preserve_callers()
-> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let result=async {
        let state=state(temporary.path())?;
        let manifest="[package]\nname='app'\nversion='0.1.0'\nedition.workspace=true\n[lib]\nname='app_library'\npath='code/start.rs'\n[dependencies]\nrenamed={package='dependency',path='../dependency'}\n";
        process_ingest_stats(&state,"rsmodules",1,&stats(&[
            ("Cargo.toml","[workspace]\nmembers=['app','dependency','other']\n[workspace.package]\nedition='2024'\n"),
            ("app/Cargo.toml",manifest),
            ("app/code/start.rs","pub mod api;\nuse renamed::api::save as persist;\npub fn helper()->i32 {7}\npub fn RunDependency()->i32 {persist()}\n"),
            ("app/code/api.rs","pub fn save()->i32 {99}"),
            ("app/src/main.rs","fn RunOwnLibrary()->i32 {app_library::helper()} fn main(){}"),
            ("dependency/Cargo.toml","[package]\nname='dependency'\nversion='0.1.0'\nedition='2021'\n[lib]\npath='code/core.rs'\n"),
            ("dependency/code/core.rs","pub mod api;"),("dependency/code/api.rs","pub fn save()->i32 {11}"),
            ("other/Cargo.toml","[package]\nname='other'\nversion='0.1.0'\nedition='2021'\n"),
            ("other/src/lib.rs","pub mod api;"),("other/src/api.rs","pub fn save()->i32 {22}"),
        ])).await?;
        state.graph.resolve_symbol_edges("rsmodules")?;
        assert_eq!(outgoing(&state,"RunDependency")?[0].target_id,"sym:function:dependency/code/api.rs:save:1");
        assert_eq!(outgoing(&state,"RunOwnLibrary")?[0].target_id,"sym:function:app/code/start.rs:helper:3");
        let before=state.graph.get_node("rsmodules","file:app/code/start.rs")?.unwrap();
        process_ingest_stats(&state,"rsmodules",2,&stats(&[("app/Cargo.toml",&manifest.replace("package='dependency',path='../dependency'","package='other',path='../other'"))])).await?;
        state.graph.resolve_symbol_edges("rsmodules")?;
        assert_eq!(outgoing(&state,"RunDependency")?[0].target_id,"sym:function:other/src/api.rs:save:1");
        let after=state.graph.get_node("rsmodules","file:app/code/start.rs")?.unwrap();assert_eq!(bincode::serialize(&before)?,bincode::serialize(&after)?);
        Ok(())
    }.await;
    temporary.close()?;
    result
}

#[tokio::test]
async fn rust_modules_reexport_and_module_source_changes_retract_old_targets() -> anyhow::Result<()>
{
    let temporary = tempfile::tempdir()?;
    let result=async {
        let state=state(temporary.path())?;
        process_ingest_stats(&state,"rsmodules",1,&stats(&[
            ("Cargo.toml",CARGO),("src/lib.rs","pub mod left;pub mod right;pub mod barrel;\nuse crate::barrel::save as persist;\npub fn Run(){persist();}\n"),
            ("src/left.rs","pub fn save(){}"),("src/right.rs","pub fn save(){}"),("src/barrel.rs","pub use crate::left::save;"),
        ])).await?;
        state.graph.resolve_symbol_edges("rsmodules")?;
        let before=state.graph.get_node("rsmodules","file:src/lib.rs")?.unwrap();
        assert_eq!(outgoing(&state,"Run")?[0].target_id,"sym:function:src/left.rs:save:1");
        process_ingest_stats(&state,"rsmodules",2,&stats(&[("src/barrel.rs","pub use crate::right::save;")])).await?;
        state.graph.resolve_symbol_edges("rsmodules")?;
        assert_eq!(outgoing(&state,"Run")?[0].target_id,"sym:function:src/right.rs:save:1");
        process_ingest_stats(&state,"rsmodules",3,&stats(&[("src/right.rs","pub fn renamed(){}")])).await?;
        state.graph.resolve_symbol_edges("rsmodules")?;unbound(&state,"Run","name_not_found")?;
        process_ingest_stats(&state,"rsmodules",4,&stats(&[("src/right.rs","\npub fn save(){}")])).await?;
        state.graph.resolve_symbol_edges("rsmodules")?;
        assert_eq!(outgoing(&state,"Run")?[0].target_id,"sym:function:src/right.rs:save:2");
        assert_eq!(bincode::serialize(&before)?,bincode::serialize(&state.graph.get_node("rsmodules","file:src/lib.rs")?.unwrap())?);
        Ok(())
    }.await;
    temporary.close()?;
    result
}

#[tokio::test]
async fn rust_modules_edition_changes_rebind_unchanged_nested_use_paths() -> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let result = async {
        let state = state(temporary.path())?;
        let source = "pub mod left {pub fn save()->i32 {11}}\npub mod child {\n    pub mod left {pub fn save()->i32 {22}}\n    use left::save as persist;\n    pub fn RunEdition()->i32 {persist()}\n}\n";
        process_ingest_stats(&state, "rsmodules", 1, &stats(&[
            ("Cargo.toml", &CARGO.replace("2021", "2015")), ("src/lib.rs", source),
        ])).await?;
        state.graph.resolve_symbol_edges("rsmodules")?;
        assert_eq!(outgoing(&state, "RunEdition")?[0].target_id, "sym:function:src/lib.rs:save:1");
        let before = state.graph.get_node("rsmodules", "file:src/lib.rs")?.unwrap();
        process_ingest_stats(&state, "rsmodules", 2, &stats(&[("Cargo.toml", CARGO)])).await?;
        state.graph.resolve_symbol_edges("rsmodules")?;
        assert_eq!(outgoing(&state, "RunEdition")?[0].target_id, "sym:function:src/lib.rs:save:3");
        assert_eq!(bincode::serialize(&before)?, bincode::serialize(&state.graph.get_node("rsmodules", "file:src/lib.rs")?.unwrap())?);
        Ok(())
    }.await;
    temporary.close()?;
    result
}
