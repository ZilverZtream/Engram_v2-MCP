//! Expected module identities are authored in these fixtures, never generated
//! by the resolver under test. Each fixture drops its stores before cleanup.
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
        .query_nodes("pymodules", Some("function"), Some(caller), None, 100)?;
    let node = nodes.iter().find(|n| n.name == caller).unwrap();
    Ok(state
        .graph
        .edges_touching("pymodules", &node.node_id, 100)?
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
            .starts_with(engram_core::python_modules::UNBOUND_PREFIX),
        "{caller}: {edges:?}"
    );
    assert_eq!(
        edges[0].metadata.as_ref().unwrap()["python_module_bindings_v1"][0]["reason"],
        reason,
        "{caller}"
    );
    Ok(())
}

#[tokio::test]
async fn python_modules_bind_alias_packages_scopes_and_public_source_evidence() -> anyhow::Result<()>
{
    let temporary = tempfile::tempdir()?;
    let result=async {
        let state=state(temporary.path())?;
        let app_source="from pkg.left import save as persist\nimport pkg.right as right\nimport pkg.left\nfrom pkg import public\nfrom pkg import right as child\nfrom sourcepkg.api import save as source_save\ndef RunNamed():\n    persist()\n    return persist()\ndef RunModule():\n    return right.save()\ndef RunDotted():\n    return pkg.left.save()\ndef RunPackage():\n    return public()\ndef RunChild():\n    return child.save()\ndef RunSource():\n    return source_save()\ndef RunLocal():\n    from pkg.right import save as persist\n    return persist()\ndef helper():\n    return 1\ndef RunHelper():\n    return helper()\nclass Holder:\n    persist = None\n    def RunMethod(self):\n        return persist()\n";
        process_ingest_stats(&state,"pymodules",1,&stats(&[
            ("pkg/left.py","def save():\n    return 11\n"),
            ("pkg/right.py","def save():\n    return 22\n"),
            ("pkg/__init__.py","from .left import save as public\n"),
            ("pkg/sub/app.py","from ..left import save as parent_save\nfrom .. import public\ndef RunRelative():\n    return parent_save()\ndef RunParent():\n    return public()\n"),
            ("src/sourcepkg/api.py","def save():\n    return 33\n"),
            ("decoy.py","def persist():\n    return 99\n"),("app.py",app_source),
        ])).await?;
        state.graph.resolve_symbol_edges("pymodules")?;
        let app=crate::tools::Engram::new(state.clone());
        for (caller,path,name,line) in [
            ("RunNamed","pkg/left.py","save",1),("RunModule","pkg/right.py","save",1),
            ("RunDotted","pkg/left.py","save",1),("RunPackage","pkg/left.py","save",1),
            ("RunChild","pkg/right.py","save",1),("RunSource","src/sourcepkg/api.py","save",1),
            ("RunLocal","pkg/right.py","save",1),("RunHelper","app.py","helper",23),
            ("RunMethod","pkg/left.py","save",1),("RunRelative","pkg/left.py","save",1),
            ("RunParent","pkg/left.py","save",1),
        ] {
            let edges=outgoing(&state,caller)?;assert_eq!(edges.len(),1,"{caller}: {edges:?}");
            let target=NodeId::symbol("function",None,path,name,line).0;
            assert_eq!(edges[0].target_id,target,"{caller}");
            let rows=edges[0].metadata.as_ref().unwrap()["python_module_bindings_v1"].as_array().unwrap();
            assert_eq!(rows.len(),if caller=="RunNamed" {2} else {1});
            let reply=app.handle_find_references(serde_json::from_value(serde_json::json!({
                "project_id":"pymodules","node_id":edges[0].source_id,"edge_kind":"calls","direction":"out"
            }))?).await.map_err(|e|anyhow::anyhow!("{e}"))?;
            let shown=&reply.content[0].as_text().unwrap().text;
            assert!(shown.contains(&target) && shown.contains("syntax_only_not_compiler_or_runtime_proof") && shown.contains("binding_site"),"{shown}");
            assert!(!shown.contains("decoy.py"),"{shown}");
            if caller=="RunNamed" {
                for row in rows {
                    let call=&row["call"];let start=call["site"]["offset"].as_u64().unwrap() as usize;
                    let end=call["site"]["end_offset"].as_u64().unwrap() as usize;
                    assert_eq!(&app_source[start..end],"persist");
                    assert_eq!(call["import_site"]["line"],1);
                }
            }
        }
        assert_eq!(state.graph.resolve_symbol_edges("pymodules")?,0,"Unchanged facts are idempotent");
        Ok(())
    }.await;
    temporary.close()?;
    result
}

#[tokio::test]
async fn python_modules_do_not_guess_shadowed_conditional_star_external_or_cyclic_targets()
-> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let result=async {
        let state=state(temporary.path())?;
        process_ingest_stats(&state,"pymodules",1,&stats(&[
            ("api.py","def save():\n    return 1\n"),
            ("a.py","from b import save\n"),("b.py","from a import save\n"),
            ("star.py","from api import *\ndef RunStar():\n    return save()\n"),
            ("app.py","from api import save\nfrom a import save as cyclic\nfrom external import save as missing\ndef RunParameter(save):\n    return save()\ndef RunAssignment():\n    save()\n    save = None\ndef RunConditional(flag):\n    if flag:\n        from api import save\n    return save()\ndef RunCycle():\n    return cyclic()\ndef RunExternal():\n    return missing()\ndef RunReceiver(item):\n    return item.save()\ndef RunGlobal():\n    global save\n    return save()\n"),
            ("decoy.py","def missing():\n    return 99\n"),
        ])).await?;
        state.graph.resolve_symbol_edges("pymodules")?;
        for (caller,reason) in [
            ("RunParameter","shadowed_or_nonmodule_binding"),("RunAssignment","shadowed_or_nonmodule_binding"),
            ("RunConditional","conditional_import"),("RunCycle","cyclic_import"),
            ("RunExternal","module_not_indexed"),("RunStar","wildcard_import"),
            ("RunReceiver","shadowed_or_nonmodule_binding"),("RunGlobal","unsupported_scope_directive"),
        ] {unbound(&state,caller,reason)?;}
        Ok(())
    }.await;
    temporary.close()?;
    result
}

#[tokio::test]
async fn python_modules_rebind_unchanged_callers_after_reexports_and_target_edits()
-> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let result = async {
        let state = state(temporary.path())?;
        process_ingest_stats(
            &state,
            "pymodules",
            1,
            &stats(&[
                ("pkg/__init__.py", "from .left import save\n"),
                ("pkg/left.py", "def save():\n    return 1\n"),
                ("pkg/right.py", "def save():\n    return 2\n"),
                (
                    "app.py",
                    "from pkg import save as persist\ndef Run():\n    return persist()\n",
                ),
            ]),
        )
        .await?;
        state.graph.resolve_symbol_edges("pymodules")?;
        let source = state.graph.get_node("pymodules", "file:app.py")?.unwrap();
        assert_eq!(
            outgoing(&state, "Run")?[0].target_id,
            "sym:function:pkg/left.py:save:1"
        );
        process_ingest_stats(
            &state,
            "pymodules",
            2,
            &stats(&[("pkg/__init__.py", "from .right import save\n")]),
        )
        .await?;
        state.graph.resolve_symbol_edges("pymodules")?;
        assert_eq!(
            outgoing(&state, "Run")?[0].target_id,
            "sym:function:pkg/right.py:save:1"
        );
        process_ingest_stats(
            &state,
            "pymodules",
            3,
            &stats(&[("pkg/right.py", "save = None\n")]),
        )
        .await?;
        state.graph.resolve_symbol_edges("pymodules")?;
        unbound(&state, "Run", "shadowed_or_nonmodule_binding")?;
        process_ingest_stats(
            &state,
            "pymodules",
            4,
            &stats(&[("pkg/right.py", "\ndef save():\n    return 3\n")]),
        )
        .await?;
        state.graph.resolve_symbol_edges("pymodules")?;
        assert_eq!(
            outgoing(&state, "Run")?[0].target_id,
            "sym:function:pkg/right.py:save:2"
        );
        assert_eq!(
            bincode::serialize(&source)?,
            bincode::serialize(&state.graph.get_node("pymodules", "file:app.py")?.unwrap())?
        );
        Ok(())
    }
    .await;
    temporary.close()?;
    result
}

#[tokio::test]
async fn python_modules_conflicting_source_roots_remain_ambiguous() -> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let result = async {
        let state = state(temporary.path())?;
        process_ingest_stats(
            &state,
            "pymodules",
            1,
            &stats(&[
                ("api.py", "def save():\n    return 1\n"),
                ("src/api.py", "def save():\n    return 2\n"),
                (
                    "app.py",
                    "from api import save\ndef Run():\n    return save()\n",
                ),
            ]),
        )
        .await?;
        state.graph.resolve_symbol_edges("pymodules")?;
        unbound(&state, "Run", "ambiguous_module_root")?;
        Ok(())
    }
    .await;
    temporary.close()?;
    result
}
