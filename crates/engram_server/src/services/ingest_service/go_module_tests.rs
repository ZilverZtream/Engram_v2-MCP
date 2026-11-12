//! Authored Go source -> actual extraction/ingestion -> public references.
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
        .query_nodes("gomodules", Some("function"), Some(caller), None, 100)?;
    let node = nodes.iter().find(|n| n.name == caller).unwrap();
    Ok(state
        .graph
        .edges_touching("gomodules", &node.node_id, 100)?
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
            .starts_with(engram_core::go_modules::UNBOUND_PREFIX),
        "{caller}: {edges:?}"
    );
    assert_eq!(
        edges[0].metadata.as_ref().unwrap()["go_module_bindings_v1"][0]["reason"],
        reason,
        "{caller}"
    );
    Ok(())
}

const MOD: &str = "module example.com/owned\ngo 1.23\n";

#[tokio::test]
async fn go_modules_nested_modules_internal_visibility_and_ignored_sources_do_not_leak()
-> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let result = async {
        let state = state(temporary.path())?;
        process_ingest_stats(&state, "gomodules", 1, &stats(&[
            ("go.mod", MOD),
            ("app/app.go", "package app\nimport n \"example.com/owned/nested\"\nimport secret \"example.com/owned/internal/api\"\nimport api \"example.com/owned/api\"\nfunc RunNested(){n.Save()}\nfunc RunInternal(){secret.Save()}\nfunc RunIgnored(){api.Ignored()}\n"),
            ("nested/go.mod", "module example.com/separate\ngo 1.23\n"),
            ("nested/api.go", "package nested\nfunc Save(){}\n"),
            ("internal/api/api.go", "package secret\nfunc Save(){}\n"),
            ("api/api.go", "package api\nfunc Ordinary(){}\n"),
            ("api/_ignored.go", "package api\nfunc Ignored(){}\n"),
            ("external/go.mod", "module example.com/external\ngo 1.23\nrequire example.com/owned v1.0.0\nreplace example.com/owned => ..\n"),
            ("external/app.go", "package external\nimport secret \"example.com/owned/internal/api\"\nfunc RunForeignInternal(){secret.Save()}\n"),
        ])).await?;
        state.graph.resolve_symbol_edges("gomodules")?;
        unbound(&state, "RunNested", "nested_module_not_selected")?;
        unbound(&state, "RunForeignInternal", "internal_package_not_visible")?;
        unbound(&state, "RunIgnored", "build_constraints_not_evaluated")?;
        assert_eq!(outgoing(&state, "RunInternal")?[0].target_id, "sym:function:internal/api/api.go:Save:2");
        Ok(())
    }.await;
    temporary.close()?;
    result
}

#[tokio::test]
async fn go_modules_bind_file_imports_declared_names_and_cross_file_functions() -> anyhow::Result<()>
{
    let temporary = tempfile::tempdir()?;
    let result=async {
        let state=state(temporary.path())?;
        let source="package app\nimport \"example.com/owned/left\"\nimport r \"example.com/owned/right\"\nfunc RunDefault() int {return storage.Save()}\nfunc RunAlias() int {return r.Save()}\nfunc RunSibling() int {return helper()}\nfunc RunGeneric() int {return r.Generic[int]()}\nfunc RunParameter(storage func() int) int {return storage()}\nfunc RunOrder() {before:=storage.Save(); storage:=1; _=before; storage.Save()}\nfunc RunInitializer() {if storage:=1; storage>0 {storage.Save()}; storage.Save()}\n";
        process_ingest_stats(&state,"gomodules",1,&stats(&[
            ("go.mod",MOD),("app/app.go",source),
            ("app/helper.go","package app\nfunc helper() int {return 44}\n"),
            ("left/api.go","package storage\nfunc Save() int {return 11}\n"),
            ("right/api.go","package storage\nfunc Save() int {return 22}\nfunc Generic[T any]() int {return 33}\n"),
            ("decoy/api.go","package decoy\nfunc Save() int {return 99}\n"),
            ("app/dot.go","package app\nimport . \"example.com/owned/left\"\nfunc RunDot() int {return Save()}\n"),
            ("app/blank.go","package app\nimport _ \"example.com/owned/left\"\nfunc RunBlank() int {return storage.Save()}\n"),
        ])).await?;
        state.graph.resolve_symbol_edges("gomodules")?;
        let app=crate::tools::Engram::new(state.clone());
        for (caller,file,name,line) in [("RunDefault","left/api.go","Save",2),("RunAlias","right/api.go","Save",2),
            ("RunSibling","app/helper.go","helper",2),("RunGeneric","right/api.go","Generic",3),("RunDot","left/api.go","Save",2)] {
            let edges=outgoing(&state,caller)?;assert_eq!(edges.len(),1,"{caller}: {edges:?}");
            let target=NodeId::symbol("function",None,file,name,line).0;assert_eq!(edges[0].target_id,target,"{caller}");
            let reply=app.handle_find_references(serde_json::from_value(serde_json::json!({"project_id":"gomodules","node_id":edges[0].source_id,"edge_kind":"calls","direction":"out"}))?).await.map_err(|e|anyhow::anyhow!("{e}"))?;
            let shown=&reply.content[0].as_text().unwrap().text;assert!(shown.contains(&target) && shown.contains("syntax_only_not_compiler_or_runtime_proof"),"{shown}");
        }
        let proof_edges=outgoing(&state,"RunDefault")?;
        let call=&proof_edges[0].metadata.as_ref().unwrap()["go_module_bindings_v1"][0]["call"];
        let start=call["site"]["offset"].as_u64().unwrap() as usize;let end=call["site"]["end_offset"].as_u64().unwrap() as usize;
        assert_eq!(&source[start..end],"storage.Save");assert_eq!(call["import_site"]["line"],2);assert_eq!(call["site"]["line"],4);
        unbound(&state,"RunParameter","shadowed_local_binding")?;
        unbound(&state,"RunBlank","unknown_receiver_or_unindexed_import")?;
        for caller in ["RunOrder","RunInitializer"] {
            let edges=outgoing(&state,caller)?;assert_eq!(edges.len(),2,"{caller}: {edges:?}");
            assert!(edges.iter().any(|e|e.target_id=="sym:function:left/api.go:Save:2"));
            assert!(edges.iter().any(|e|e.metadata.as_ref().unwrap()["go_module_bindings_v1"][0]["reason"]=="shadowed_local_binding"));
        }
        assert_eq!(state.graph.resolve_symbol_edges("gomodules")?,0);Ok(())
    }.await;
    temporary.close()?;
    result
}

#[tokio::test]
async fn go_modules_keep_visibility_build_constraints_test_packages_and_receivers_distinct()
-> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let result=async {
        let state=state(temporary.path())?;
        process_ingest_stats(&state,"gomodules",1,&stats(&[
            ("go.mod",MOD),
            ("app/app.go","package app\nimport api \"example.com/owned/api\"\nimport ext \"external.invalid/pkg\"\nfunc RunPrivate(){api.hidden()}\nfunc RunPlatform(){api.Platform()}\nfunc RunReceiver(obj Thing){obj.Save()}\nfunc RunExternal(){ext.Save()}\nfunc RunProduction(){TestOnly()}\nfunc RunMethod(){api.Method()}\nfunc helper(){}\n"),
            ("app/app_test.go","package app\nfunc TestOnly(){}\nfunc RunTest(){TestOnly()}\n"),
            ("app/external_test.go","package app_test\nfunc RunIsolated(){helper()}\n"),
            ("api/api.go","package api\nfunc hidden(){}\ntype Thing struct{}\nfunc (t Thing) Method(){}\n"),
            ("api/platform_linux.go","package api\nfunc Platform(){}\n"),
            ("app/constrained.go","//go:build optional\n\npackage app\nfunc RunConditional(){helper()}\n"),
            ("app/dot.go","package app\nimport . \"example.com/owned/left\"\nimport . \"example.com/owned/right\"\nfunc RunAmbiguous(){Save()}\n"),
            ("left/api.go","package api\nfunc Save(){}\n"),("right/api.go","package api\nfunc Save(){}\n"),
        ])).await?;
        state.graph.resolve_symbol_edges("gomodules")?;
        for (caller,reason) in [("RunPrivate","unexported_package_binding"),("RunPlatform","build_constraints_not_evaluated"),
            ("RunReceiver","shadowed_local_binding"),("RunExternal","external_package_not_indexed"),("RunProduction","name_not_found"),
            ("RunMethod","name_not_found"),("RunIsolated","name_not_found"),("RunConditional","build_constraints_not_evaluated"),
            ("RunAmbiguous","ambiguous_dot_import")] {unbound(&state,caller,reason)?;}
        assert_eq!(outgoing(&state,"RunTest")?[0].target_id,"sym:function:app/app_test.go:TestOnly:2");Ok(())
    }.await;
    temporary.close()?;
    result
}

#[tokio::test]
async fn go_modules_manifest_replacements_and_workspace_edits_rebind_unchanged_callers()
-> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let result=async {
        let state=state(temporary.path())?;
        let manifest="module example.com/app\ngo 1.23\nrequire example.com/dep v1.0.0\nreplace example.com/dep => ../one\n";
        process_ingest_stats(&state,"gomodules",1,&stats(&[
            ("app/go.mod",manifest),
            ("app/app.go","package app\nimport storage \"example.com/dep\"\nfunc RunDependency(){storage.Save()}\n"),
            ("one/go.mod","module example.com/dep\ngo 1.23\n"),("one/api.go","package storage\nfunc Save(){}\n"),
            ("two/go.mod","module example.com/dep\ngo 1.23\n"),("two/api.go","package storage\nfunc Save(){}\n"),
        ])).await?;
        state.graph.resolve_symbol_edges("gomodules")?;
        assert_eq!(outgoing(&state,"RunDependency")?[0].target_id,"sym:function:one/api.go:Save:2");
        let before=state.graph.get_node("gomodules","file:app/app.go")?.unwrap();
        process_ingest_stats(&state,"gomodules",2,&stats(&[("app/go.mod",&manifest.replace("../one","../two"))])).await?;
        state.graph.resolve_symbol_edges("gomodules")?;
        assert_eq!(outgoing(&state,"RunDependency")?[0].target_id,"sym:function:two/api.go:Save:2");
        let workspace="go 1.23\nuse (\n ./app\n ./one // authored workspace member\n)\n";
        process_ingest_stats(&state,"gomodules",3,&stats(&[("app/go.mod",&manifest.replace("replace example.com/dep => ../one\n","")),("go.work",workspace)])).await?;
        state.graph.resolve_symbol_edges("gomodules")?;
        assert_eq!(outgoing(&state,"RunDependency")?[0].target_id,"sym:function:one/api.go:Save:2");
        process_ingest_stats(&state,"gomodules",4,&stats(&[("go.work",&workspace.replace("./one","./two"))])).await?;
        state.graph.resolve_symbol_edges("gomodules")?;
        assert_eq!(outgoing(&state,"RunDependency")?[0].target_id,"sym:function:two/api.go:Save:2");
        assert_eq!(bincode::serialize(&before)?,bincode::serialize(&state.graph.get_node("gomodules","file:app/app.go")?.unwrap())?);Ok(())
    }.await;
    temporary.close()?;
    result
}

#[tokio::test]
async fn go_modules_package_and_declaration_edits_retract_stale_import_targets()
-> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let result = async {
        let state = state(temporary.path())?;
        process_ingest_stats(
            &state,
            "gomodules",
            1,
            &stats(&[
                ("go.mod", MOD),
                (
                    "app/app.go",
                    "package app\nimport \"example.com/owned/api\"\nfunc Run(){storage.Save()}\n",
                ),
                ("api/api.go", "package storage\nfunc Save(){}\n"),
            ]),
        )
        .await?;
        state.graph.resolve_symbol_edges("gomodules")?;
        assert_eq!(
            outgoing(&state, "Run")?[0].target_id,
            "sym:function:api/api.go:Save:2"
        );
        let before = state
            .graph
            .get_node("gomodules", "file:app/app.go")?
            .unwrap();
        process_ingest_stats(
            &state,
            "gomodules",
            2,
            &stats(&[("api/api.go", "package changed\nfunc Save(){}\n")]),
        )
        .await?;
        state.graph.resolve_symbol_edges("gomodules")?;
        unbound(&state, "Run", "unknown_receiver_or_unindexed_import")?;
        process_ingest_stats(
            &state,
            "gomodules",
            3,
            &stats(&[("api/api.go", "package storage\nfunc Renamed(){}\n")]),
        )
        .await?;
        state.graph.resolve_symbol_edges("gomodules")?;
        unbound(&state, "Run", "name_not_found")?;
        process_ingest_stats(
            &state,
            "gomodules",
            4,
            &stats(&[("api/api.go", "package storage\n\nfunc Save(){}\n")]),
        )
        .await?;
        state.graph.resolve_symbol_edges("gomodules")?;
        assert_eq!(
            outgoing(&state, "Run")?[0].target_id,
            "sym:function:api/api.go:Save:3"
        );
        assert_eq!(
            bincode::serialize(&before)?,
            bincode::serialize(
                &state
                    .graph
                    .get_node("gomodules", "file:app/app.go")?
                    .unwrap()
            )?
        );
        Ok(())
    }
    .await;
    temporary.close()?;
    result
}
