//! Authored Java declarations through real ingestion and public references.
use super::*;
use engram_core::RelPath;
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
    let nodes =
        state
            .graph
            .query_nodes("javamodules", Some("function"), Some(caller), None, 100)?;
    let node = nodes.iter().find(|n| n.name == caller).unwrap();
    Ok(state
        .graph
        .edges_touching("javamodules", &node.node_id, 100)?
        .into_iter()
        .filter(|e| e.edge_kind == EdgeKind::Calls && e.source_id == node.node_id)
        .collect())
}
fn target(
    state: &AppState,
    caller: &str,
    file: &str,
    name: &str,
    line: u32,
) -> anyhow::Result<engram_graph::Edge> {
    let edges = outgoing(state, caller)?;
    assert_eq!(edges.len(), 1, "{caller}: {edges:?}");
    assert_eq!(
        edges[0].target_id,
        engram_core::NodeId::symbol("function", None, file, name, line).0,
        "{caller}: {edges:?}"
    );
    Ok(edges[0].clone())
}
fn unbound(state: &AppState, caller: &str, reason: &str) -> anyhow::Result<()> {
    let edges = outgoing(state, caller)?;
    assert_eq!(edges.len(), 1, "{caller}: {edges:?}");
    assert!(
        edges[0]
            .target_id
            .starts_with(engram_core::java_modules::UNBOUND_PREFIX),
        "{caller}: {edges:?}"
    );
    assert_eq!(
        edges[0].metadata.as_ref().unwrap()["java_module_bindings_v1"][0]["reason"],
        reason,
        "{caller}"
    );
    Ok(())
}
const LEFT: &str = "package owned.left;\npublic class Api {\n public static int Save(){return 11;}\n public int Read(){return 12;}\n public static class Nested {\n  public static int Save(){return 13;}\n }\n}\n";
const RIGHT: &str = "package owned.right;\npublic class Api {\n public static int Save(){return 22;}\n public int Read(){return 23;}\n}\n";

#[tokio::test]
async fn java_modules_scope_boundaries_and_unresolved_import_evidence() -> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let result=async{
        let state=state(temporary.path())?;
        process_ingest_stats(&state,"javamodules",1,&stats(&[
            ("left/Api.java",LEFT),
            ("app/App.java","package owned.app;\nimport owned.left.Api;\npublic class App {\n Api instance;\n static Api shared;\n public static int RunStaticField(){return shared.Read();}\n public static int RunInvalidField(){return instance.Read();}\n public static int RunInvalidThisField(){return this.shared.Read();}\n public void RunLocalType(){class Api{} Api.Save();}\n public void RunScope(){ {int Api=0; Api.Save();} Api.Save();}\n public void RunLambda(){java.util.function.Consumer<Object> f=(Api)->Api.Save();}\n public void RunMissing(){Api.Absent();}\n}\n"),
            ("app/Wild.java","package owned.app;\nimport owned.left.*;\nimport absent.library.*;\npublic class Wild {\n public void RunUnknownWildcard(){Api.Save();}\n}\n"),
        ])).await?;state.graph.resolve_symbol_edges("javamodules")?;
        target(&state,"RunStaticField","left/Api.java","Read",4)?;
        for (caller,reason) in [("RunInvalidField","instance_receiver_in_static_context"),("RunInvalidThisField","instance_receiver_in_static_context"),("RunLocalType","local_type_not_resolved"),("RunLambda","receiver_type_not_resolved"),("RunMissing","method_not_found"),("RunUnknownWildcard","on_demand_import_not_indexed")] {unbound(&state,caller,reason)?;}
        let scopes=outgoing(&state,"RunScope")?;assert_eq!(scopes.len(),2);assert!(scopes.iter().any(|e|e.target_id=="sym:function:left/Api.java:Save:3"));assert!(scopes.iter().any(|e|e.target_id.starts_with(engram_core::java_modules::UNBOUND_PREFIX)));
        let missing=outgoing(&state,"RunMissing")?;let call=&missing[0].metadata.as_ref().unwrap()["java_module_bindings_v1"][0]["call"];
        assert_eq!(call["import_site"]["line"],2);assert_eq!(call["module"],"owned.left.Api");assert_eq!(call["path_from_use"],true);Ok(())
    }.await;
    temporary.close()?;
    result
}

#[tokio::test]
async fn java_modules_imports_packages_nested_types_and_declared_receivers() -> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let result=async{
    let state=state(temporary.path())?;
    let app="package owned.app;\nimport owned.left.Api;\nimport static owned.right.Api.Save;\npublic class App {\n Api field;\n public static int RunType(){Api.Save(); return Api.Save();}\n public static int RunStatic(){return Save();}\n public static int RunQualified(){return owned.right.Api.Save();}\n public static int RunParameter(Api api){return api.Read();}\n public int RunField(){return field.Read();}\n public int RunThisField(){return this.field.Read();}\n public static int RunLocal(){Api api=null; return api.Read();}\n public static int RunConstructed(){return new Api().Read();}\n public static int RunNested(){return Api.Nested.Save();}\n public int Helper(){return 1;}\n public int RunThis(){return this.Helper();}\n public int RunBare(){return Helper();}\n}\n";
    process_ingest_stats(&state,"javamodules",1,&stats(&[("left/Api.java",LEFT),("right/Api.java",RIGHT),("app/App.java",app),
        ("right/Same.java","package owned.right;\npublic class Same {\n public static int RunSame(){return Api.Save();}\n}\n"),
        ("app/Wild.java","package owned.app;\nimport owned.right.*;\npublic class Wild {\n public static int RunWildcard(){return Api.Save();}\n}\n"),
        ("app/StaticWild.java","package owned.app;\nimport static owned.left.Api.*;\npublic class StaticWild {\n public static int RunStaticWildcard(){return Save();}\n}\n"),
        ("app/Nested.java","package owned.app;\nimport owned.left.Api.Nested;\npublic class Outer {\n public class Inner {\n  public int RunImportedNested(){return Nested.Save();}\n }\n}\n"),
    ])).await?;state.graph.resolve_symbol_edges("javamodules")?;
    for (caller,file,name,line) in [("RunType","left/Api.java","Save",3),("RunStatic","right/Api.java","Save",3),("RunQualified","right/Api.java","Save",3),
        ("RunParameter","left/Api.java","Read",4),("RunField","left/Api.java","Read",4),("RunThisField","left/Api.java","Read",4),("RunLocal","left/Api.java","Read",4),("RunConstructed","left/Api.java","Read",4),
        ("RunNested","left/Api.java","Save",6),("RunImportedNested","left/Api.java","Save",6),("RunThis","app/App.java","Helper",15),("RunBare","app/App.java","Helper",15),
        ("RunSame","right/Api.java","Save",3),("RunWildcard","right/Api.java","Save",3),("RunStaticWildcard","left/Api.java","Save",3)] {target(&state,caller,file,name,line)?;}
    let edge=target(&state,"RunType","left/Api.java","Save",3)?;
    let proof=&edge.metadata.as_ref().unwrap()["java_module_bindings_v1"];assert_eq!(proof.as_array().unwrap().len(),2);
    for row in proof.as_array().unwrap(){let site=&row["call"]["site"];let start=site["offset"].as_u64().unwrap() as usize;let end=site["end_offset"].as_u64().unwrap() as usize;assert_eq!(&app[start..end],"Api.Save()");assert_eq!(row["call"]["import_site"]["line"],2);}
    let api=crate::tools::Engram::new(state.clone());let reply=api.handle_find_references(serde_json::from_value(serde_json::json!({"project_id":"javamodules","node_id":edge.source_id,"edge_kind":"calls","direction":"out"}))?).await.map_err(|e|anyhow::anyhow!("{e}"))?;
    let shown=&reply.content[0].as_text().unwrap().text;assert!(shown.contains("sym:function:left/Api.java:Save:3")&&shown.contains("syntax_only_not_compiler_or_runtime_proof")&&shown.contains("Module evidence:"),"{shown}");
    assert_eq!(state.graph.resolve_symbol_edges("javamodules")?,0);Ok(())
 }.await;
    temporary.close()?;
    result
}

#[tokio::test]
async fn java_modules_do_not_guess_overloads_access_or_obscured_receivers() -> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let result=async{
    let state=state(temporary.path())?;
    process_ingest_stats(&state,"javamodules",1,&stats(&[
      ("left/Api.java","package owned.left;\npublic class Api {\n public static void Save(){}\n public void Read(){}\n private static void Hidden(){}\n static void PackageOnly(){}\n public static void Overload(int x){}\n public static void Overload(String x){}\n public static void Count(){}\n public static void Count(int x){}\n}\n"),
      ("right/Api.java",RIGHT),
      ("app/App.java","package owned.app;\nimport owned.left.Api;\npublic class App {\n public static void RunPrimitive(int Api){Api.Save();}\n public static void RunInferred(){var Api=unknown(); Api.Save();}\n public static void RunPrivate(){Api.Hidden();}\n public static void RunPackage(){Api.PackageOnly();}\n public static void RunOverload(){Api.Overload(1);}\n public static void RunInstance(){Api.Read();}\n public static void RunCount(){Api.Count(1);}\n public static void RunExternal(){external.Api.Save();}\n public static void RunPattern(Object x){if(x instanceof Object Api){Api.Save();}}\n public static <Api> void RunTypeParameter(){Api.Save();}\n}\n"),
      ("app/Wild.java","package owned.app;\nimport owned.left.*;\nimport owned.right.*;\npublic class Wild {\n public static void RunAmbiguous(){Api.Save();}\n}\n"),
      ("app/Static.java","package owned.app;\nimport static owned.left.Api.*;\nimport static owned.right.Api.*;\npublic class Static {\n public static void RunStaticAmbiguous(){Save();}\n}\n"),
      ("app/Other.java","package owned.app;\npublic class Other {\n public static void RunNoImport(){Api.Save();}\n}\n"),
    ])).await?;state.graph.resolve_symbol_edges("javamodules")?;
    for (caller,reason) in [("RunPrimitive","receiver_type_not_resolved"),("RunPrivate","method_not_accessible"),("RunPackage","method_not_accessible"),("RunOverload","ambiguous_method_overload"),("RunInstance","instance_method_requires_receiver"),("RunExternal","type_not_indexed"),("RunAmbiguous","ambiguous_type_import"),("RunStaticAmbiguous","ambiguous_static_import"),("RunNoImport","type_not_indexed"),("RunPattern","pattern_scope_not_resolved"),("RunTypeParameter","type_parameter_receiver_not_resolved")] {unbound(&state,caller,reason)?;}
    target(&state,"RunCount","left/Api.java","Count",10)?;
    let inferred=outgoing(&state,"RunInferred")?;assert!(inferred.iter().all(|e|e.target_id.starts_with(engram_core::java_modules::UNBOUND_PREFIX)));assert!(inferred.iter().any(|e|e.metadata.as_ref().unwrap()["java_module_bindings_v1"][0]["reason"]=="receiver_type_not_resolved"));Ok(())
 }.await;
    temporary.close()?;
    result
}

#[tokio::test]
async fn java_modules_incremental_package_import_and_declaration_changes_rebind_retained_callers()
-> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let result=async{
    let state=state(temporary.path())?;let app="package owned.app;\nimport owned.left.Api;\npublic class App {\n public static int Run(){return Api.Save();}\n}\n";
    process_ingest_stats(&state,"javamodules",1,&stats(&[("left/Api.java",LEFT),("right/Api.java",RIGHT),("app/App.java",app)])).await?;state.graph.resolve_symbol_edges("javamodules")?;target(&state,"Run","left/Api.java","Save",3)?;
    let before=state.graph.get_node("javamodules","file:app/App.java")?.unwrap();
    process_ingest_stats(&state,"javamodules",2,&stats(&[("left/Api.java",&LEFT.replace("owned.left","owned.renamed"))])).await?;state.graph.resolve_symbol_edges("javamodules")?;unbound(&state,"Run","type_not_indexed")?;
    process_ingest_stats(&state,"javamodules",3,&stats(&[("left/Api.java",&LEFT.replace("Save()","Changed()"))])).await?;state.graph.resolve_symbol_edges("javamodules")?;unbound(&state,"Run","method_not_found")?;
    process_ingest_stats(&state,"javamodules",4,&stats(&[("left/Api.java",&format!("\n{LEFT}"))])).await?;state.graph.resolve_symbol_edges("javamodules")?;target(&state,"Run","left/Api.java","Save",4)?;
    assert_eq!(bincode::serialize(&before)?,bincode::serialize(&state.graph.get_node("javamodules","file:app/App.java")?.unwrap())?);
    process_ingest_stats(&state,"javamodules",5,&stats(&[("app/App.java",&app.replace("owned.left","owned.right"))])).await?;state.graph.resolve_symbol_edges("javamodules")?;target(&state,"Run","right/Api.java","Save",3)?;Ok(())
 }.await;
    temporary.close()?;
    result
}

#[tokio::test]
async fn java_modules_named_module_duplicate_types_and_inheritance_are_explicit()
-> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let result=async{
    let state=state(temporary.path())?;
    process_ingest_stats(&state,"javamodules",1,&stats(&[
      ("one/module-info.java","module one {exports owned.one;}\n"),
      ("one/Api.java","package owned.one;\npublic class Api {\n public static void Save(){}\n}\n"),
      ("two/module-info.java","module two {requires one;}\n"),
      ("two/App.java","package owned.two;\nimport owned.one.Api;\npublic class App {\n public static void RunModule(){Api.Save();}\n}\n"),
      ("duplicate/A.java",LEFT),("duplicate/B.java",LEFT),
      ("app/App.java","package owned.app;\nimport owned.left.Api;\npublic class App {\n public static void RunDuplicate(){Api.Save();}\n}\n"),
      ("base/Base.java","package owned.base;\npublic class Base {\n public static void Save(){}\n}\n"),
      ("base/Child.java","package owned.base;\npublic class Child extends Base {\n public static void RunInherited(){Save();}\n}\n"),
    ])).await?;state.graph.resolve_symbol_edges("javamodules")?;
    unbound(&state,"RunModule","module_visibility_not_evaluated")?;unbound(&state,"RunDuplicate","ambiguous_type_identity")?;unbound(&state,"RunInherited","inherited_methods_not_resolved")?;Ok(())
 }.await;
    temporary.close()?;
    result
}
