//! Authored modern state through indexing and the public typed tool.
use anyhow::{Context, Result, ensure};
use engram_core::Config;
use engram_server::{AppState, Engram};
use rmcp::handler::server::tool::Parameters;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::path::Path;

/// Own the async runtime too: on assertion failure it still drains index jobs,
/// closes database owners and waits for blocking workers before temp teardown.
struct Fixture {
    server: Option<Engram>,
    runtime: Option<tokio::runtime::Runtime>,
    temporary: Option<tempfile::TempDir>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let (Some(server), Some(runtime)) = (&self.server, &self.runtime) {
            runtime.block_on(async {
                loop {
                    let jobs: Vec<_> = server
                        .state
                        .active_jobs
                        .write()
                        .await
                        .drain()
                        .map(|(_, task)| task)
                        .collect();
                    if jobs.is_empty() {
                        break;
                    }
                    for job in jobs {
                        let _finished = job.await;
                    }
                }
            });
        }
        drop(self.server.take());
        drop(self.runtime.take());
        if let Some(temporary) = self.temporary.take() {
            let path = temporary.path().to_owned();
            if let Err(error) = temporary.close() {
                if std::thread::panicking() {
                    eprintln!(
                        "Owned language fixture cleanup failed at {}: {error}",
                        path.display()
                    );
                } else {
                    panic!(
                        "Owned language fixture cleanup failed at {}: {error}",
                        path.display()
                    );
                }
            }
        }
    }
}

fn text(result: CallToolResult) -> Result<String> {
    ensure!(
        result.is_error != Some(true),
        "Tool returned an error: {result:?}"
    );
    Ok(result
        .content
        .iter()
        .filter_map(|item| item.as_text().map(|text| text.text.as_str()))
        .collect::<Vec<_>>()
        .join("\n"))
}

fn git_commit(root: &Path) -> Result<()> {
    let repository = git2::Repository::init(root)?;
    let mut index = repository.index()?;
    index.add_all(["*"], git2::IndexAddOption::DEFAULT, None)?;
    index.write()?;
    let tree_id = index.write_tree()?;
    let tree = repository.find_tree(tree_id)?;
    let identity = git2::Signature::now("Owned Fixture", "fixture@example.invalid")?;
    repository.commit(
        Some("HEAD"),
        &identity,
        &identity,
        "Authored language corpus",
        &tree,
        &[],
    )?;
    Ok(())
}

impl Fixture {
    fn new() -> Result<Self> {
        let temporary = tempfile::Builder::new().prefix("es-").tempdir()?;
        let owned = temporary.path().to_owned();
        let mut fixture = Self {
            server: None,
            runtime: None,
            temporary: Some(temporary),
        };
        fixture.runtime = Some(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()?,
        );
        fixture.server = Some(
            fixture
                .runtime
                .as_ref()
                .context("runtime")?
                .block_on(async {
                    let (state, _) = AppState::new(Config {
                        data_dir: owned.join("data"),
                        allowed_roots: vec![owned],
                        embedding_backend: "fts_only".into(),
                        llm_backend: "none".into(),
                        max_project_files: Some(32),
                        max_project_bytes: Some(64 * 1024),
                        ..Default::default()
                    })?;
                    Ok::<_, anyhow::Error>(Engram::new(state))
                })?,
        );
        Ok(fixture)
    }

    async fn index(&self, name: &str, files: &[(&str, &str)]) -> Result<String> {
        let root = self.temporary.as_ref().context("owner")?.path().join(name);
        std::fs::create_dir(&root)?;
        for (file, content) in files {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().context("source parent")?)?;
            std::fs::write(path, content)?;
        }
        git_commit(&root)?;
        let output = text(
            self.server
                .as_ref()
                .context("server")?
                .handle_index_project(serde_json::from_value(json!({
                    "directory":root.to_string_lossy(),"project_name":name,"wait":true
                }))?)
                .await?,
        )?;
        Ok(output
            .split_once("project_id: ")
            .and_then(|(_, tail)| tail.lines().next())
            .with_context(|| format!("Missing project identity: {output}"))?
            .trim()
            .to_owned())
    }
}

async fn trace(server: &Engram, project: &str, category: &str, key: &str) -> Result<String> {
    text(
        server
            .trace_state_usage(Parameters(serde_json::from_value(json!({
                "project_id":project,"state_type":category,"state_key":key,"limit":30
            }))?))
            .await?,
    )
}

const STATE: &str = r#"import {createContext, useContext} from "react";
import {createSlice, configureStore} from "@reduxjs/toolkit";
let total = 0;
export const cache = {current: 0};
export const Theme = createContext("light");
const orders = createSlice({name:"orders",initialState:{total:0},reducers:{
 add(state) { state.total += 1; }
}});
export const store = configureStore({reducer:{orders:orders.reducer}});
export function readMode() { return process.env.ORDER_MODE; }
export function setMode() { process.env.ORDER_MODE = "owned"; }
export function increment() { total += 1; }
export function remember() { cache.current = 7; }
export function recall() { return cache.current; }
export function useTheme() { return useContext(Theme); }
export function readTotal() { return store.getState().orders.total; }
export function submit() { store.dispatch(orders.actions.add()); }
export function decoy(process, total, cache, store, Theme) {
 process.env.ORDER_MODE = "decoy"; total += 1; cache.current = 3;
 return store.getState().orders.total;
}
"#;

#[test]
fn modern_state_public_trace_reports_authored_readers_writers_and_source_evidence() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture
        .runtime
        .as_ref()
        .context("runtime")?
        .block_on(async {
            let server = fixture.server.as_ref().context("server")?;
            let project = fixture
                .index(
                    "state",
                    &[
                        ("package.json", "{}"),
                        ("state.ts", STATE),
                        (
                            "other.ts",
                            "let total=0;\nexport function otherTotal(){return total;}\n",
                        ),
                        (
                            "reader.ts",
                            "export function secondReader(){return process.env.ORDER_MODE;}\n",
                        ),
                    ],
                )
                .await?;
            for (category, key, readers, writers) in [
                (
                    "Environment",
                    "ORDER_MODE",
                    vec!["readMode", "secondReader"],
                    vec!["setMode"],
                ),
                (
                    "Module",
                    "state.ts::total",
                    vec!["increment"],
                    vec!["increment"],
                ),
                (
                    "Singleton",
                    "state.ts::cache.current",
                    vec!["recall"],
                    vec!["remember"],
                ),
                ("ReactContext", "state.ts::Theme", vec!["useTheme"], vec![]),
                (
                    "Redux",
                    "state.ts::store.orders.total",
                    vec!["readTotal"],
                    vec!["submit"],
                ),
            ] {
                let output = trace(server, &project, category, key).await?;
                let (writes, reads) = output
                    .split_once("### Readers")
                    .with_context(|| format!("No state trace: {output}"))?;
                for name in readers {
                    ensure!(reads.contains(name), "Missing reader {name}: {output}");
                }
                for name in writers {
                    ensure!(writes.contains(name), "Missing writer {name}: {output}");
                }
                ensure!(
                    !output.contains("decoy") && !output.contains("otherTotal"),
                    "Unrelated binding: {output}"
                );
                ensure!(
                    output.contains("partial_static_coverage")
                        && output.contains("static_syntax_v1"),
                    "Missing coverage: {output}"
                );
                ensure!(
                    output.contains(&blake3::hash(STATE.as_bytes()).to_hex().to_string()),
                    "Missing source identity: {output}"
                );
                ensure!(
                    !output.contains("\"access_line\":null"),
                    "Missing indexed access: {output}"
                );
            }
            let unsupported: serde_json::Value =
                serde_json::from_str(&trace(server, &project, "Session", "ORDER_MODE").await?)?;
            ensure!(
                unsupported["status"] == "not_applicable"
                    && unsupported["analysis_performed"] == false,
                "Legacy category incorrectly enabled"
            );
            let missing = trace(server, &project, "Environment", "ABSENT").await?;
            ensure!(
                missing.contains("not proof of no access")
                    && missing.contains("update_project")
                    && !missing.contains("C#/VB"),
                "Misleading missing-key guidance: {missing}"
            );
            Ok(())
        })
}

#[test]
fn modern_state_environment_and_mutable_modules_work_in_python_rust_and_go() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.runtime.as_ref().context("runtime")?.block_on(async {
        let server=fixture.server.as_ref().context("server")?;
        for (name,file,source,key,writer,reader) in [
            ("python","state.py","import os\ntotal=0\ndef read_mode():\n return os.getenv('ORDER_MODE')\ndef set_mode():\n os.environ['ORDER_MODE']='owned'\ndef increment():\n global total\n total+=1\n", "total","set_mode","read_mode"),
            ("rust","state.rs","static mut TOTAL:u32=0;\nfn read_mode()->String{std::env::var(\"ORDER_MODE\").unwrap()}\nfn set_mode(){unsafe{std::env::set_var(\"ORDER_MODE\",\"owned\")}}\nfn increment(){unsafe{TOTAL+=1}}\n", "TOTAL","set_mode","read_mode"),
            ("go","state.go","package state\nimport \"os\"\nvar total=0\nfunc readMode() string {return os.Getenv(\"ORDER_MODE\")}\nfunc setMode(){os.Setenv(\"ORDER_MODE\",\"owned\")}\nfunc increment(){total++}\n", "total","setMode","readMode"),
        ] {
            let project=fixture.index(name,&[(file,source)]).await?;
            let output=trace(server,&project,"Environment","ORDER_MODE").await?;
            let (writes,reads)=output.split_once("### Readers").with_context(||format!("No {name} trace: {output}"))?;
            ensure!(writes.contains(writer) && reads.contains(reader),"Missing authored {name} environment access: {output}");
            let output=trace(server,&project,"Module",&format!("{file}::{key}")).await?;
            let (writes,reads)=output.split_once("### Readers").with_context(||format!("No module trace: {output}"))?;
            ensure!(writes.contains("increment") && reads.contains("increment"),"Missing authored {name} module access: {output}");
            let unsupported:serde_json::Value=serde_json::from_str(&trace(server,&project,"ReactContext","state::Theme").await?)?;
            ensure!(unsupported["status"]=="not_applicable","React enabled on {name}");
        }
        Ok(())
    })
}

async fn refresh(server: &Engram, project: &str) -> Result<()> {
    let output = text(
        server
            .handle_update_project(serde_json::from_value(json!({
                "project_id":project,"wait":true
            }))?)
            .await?,
    )?;
    ensure!(!output.contains("Failed"), "Update failed: {output}");
    Ok(())
}

#[test]
fn modern_state_edits_rename_delete_and_reopen_preserve_unrelated_accesses() -> Result<()> {
    let mut fixture = Fixture::new()?;
    let root = fixture
        .temporary
        .as_ref()
        .context("owner")?
        .path()
        .join("changes");
    let project = fixture.runtime.as_ref().context("runtime")?.block_on(async {
        let server = fixture.server.as_ref().context("server")?;
        let source = "let total=0;\nexport function increment(){total+=1;}\nexport function readMode(){return process.env.MODE;}\nexport function writeMode(){process.env.MODE='owned';}\n";
        let project = fixture.index("changes", &[("package.json","{}"),("alpha.ts",source),
            ("other.ts","export function otherReader(){return process.env.MODE;}\n"),
            ("keep.ts","export function keepReader(){return process.env.KEEP;}\n")]).await?;
        let before = trace(server,&project,"Environment","MODE").await?;
        ensure!(before.contains("readMode") && before.contains("writeMode") && before.contains("otherReader"),"Missing initial shared state: {before}");
        std::fs::write(root.join("alpha.ts"),source.replace("total","freshTotal").replace("MODE","NEW_MODE"))?;
        refresh(server,&project).await?;
        let old = trace(server,&project,"Environment","MODE").await?;
        ensure!(old.contains("otherReader") && !old.contains("readMode") && !old.contains("writeMode"),"Edit lost unchanged reader or retained stale access: {old}");
        let old = trace(server,&project,"Module","alpha.ts::total").await?;
        ensure!(!old.contains("increment"),"Old declaration access survived edit: {old}");
        std::fs::rename(root.join("alpha.ts"),root.join("moved.ts"))?;
        refresh(server,&project).await?;
        let moved = trace(server,&project,"Module","moved.ts::freshTotal").await?;
        ensure!(moved.contains("increment") && moved.contains("moved.ts:"),"Rename missing new declaration: {moved}");
        let old = trace(server,&project,"Module","alpha.ts::freshTotal").await?;
        ensure!(!old.contains("increment"),"Rename retained old access: {old}");
        std::fs::remove_file(root.join("other.ts"))?;
        refresh(server,&project).await?;
        let removed = trace(server,&project,"Environment","MODE").await?;
        ensure!(!removed.contains("otherReader") && !removed.contains("writeMode"),"Delete retained access: {removed}");
        ensure!(trace(server,&project,"Environment","KEEP").await?.contains("keepReader"),"Unrelated access lost");
        // Close all test-owned jobs before releasing the server/database handles.
        loop {
            let jobs:Vec<_> = server.state.active_jobs.write().await.drain().map(|(_,task)|task).collect();
            if jobs.is_empty(){break;}
            for job in jobs {job.await?;}
        }
        Ok::<_,anyhow::Error>(project)
    })?;
    drop(fixture.server.take());
    let owned = fixture
        .temporary
        .as_ref()
        .context("owner")?
        .path()
        .to_owned();
    fixture.server = Some(
        fixture
            .runtime
            .as_ref()
            .context("runtime")?
            .block_on(async {
                let (state, _) = AppState::new(Config {
                    data_dir: owned.join("data"),
                    allowed_roots: vec![owned],
                    embedding_backend: "fts_only".into(),
                    llm_backend: "none".into(),
                    ..Default::default()
                })?;
                Ok::<_, anyhow::Error>(Engram::new(state))
            })?,
    );
    fixture
        .runtime
        .as_ref()
        .context("runtime")?
        .block_on(async {
            let server = fixture.server.as_ref().context("server")?;
            let reopened = trace(server, &project, "Environment", "NEW_MODE").await?;
            ensure!(
                reopened.contains("readMode")
                    && reopened.contains("writeMode")
                    && reopened.contains("moved.ts:"),
                "Reopen lost moved state: {reopened}"
            );
            ensure!(
                trace(server, &project, "Module", "moved.ts::freshTotal")
                    .await?
                    .contains("increment"),
                "Reopen lost module state"
            );
            std::fs::remove_file(root.join("moved.ts"))?;
            refresh(server, &project).await?;
            let removed = trace(server, &project, "Environment", "NEW_MODE").await?;
            ensure!(
                !removed.contains("readMode") && !removed.contains("writeMode"),
                "Delete after reopen retained accesses: {removed}"
            );
            ensure!(
                trace(server, &project, "Environment", "KEEP")
                    .await?
                    .contains("keepReader"),
                "Delete after reopen lost unrelated state"
            );
            Ok(())
        })
}

#[test]
fn modern_state_imports_follow_exports_and_rebind_after_target_changes() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.runtime.as_ref().context("runtime")?.block_on(async {
        let server=fixture.server.as_ref().context("server")?;
        let source=STATE.replace("let total =", "export let total =");
        let barrel="export {total, cache, Theme, store} from './state';\n";
        let caller=r#"import {total as count, cache as memo, Theme as Context, store as ordersStore} from './barrel';
import {useContext} from 'react';
import * as original from './state';
import {lookalike} from './fake';
export function crossCount(){return count;}
export function invalidAssignment(){count+=1;}
export function unknownDispatch(){ordersStore.dispatch({type:"unbound"});}
export function fakeDispatch(){lookalike.dispatch({type:"unbound"});}
export function crossMemo(){memo.current=10;return memo.current;}
export function crossTheme(){return useContext(Context);}
export function crossRedux(){return ordersStore.getState().orders.total;}
export function crossProvider(){return <Context.Provider value="dark"/>;}
export function originalMemo(){return original.cache.current;}
export function shadowed(count,memo,Context,ordersStore){memo.current=3;return count+ordersStore.getState().orders.total;}
"#;
        let project=fixture.index("imports",&[("package.json","{}"),("state.ts",&source),("other.ts",&source),
            ("barrel.ts",barrel),("caller.tsx",caller),("fake.ts","export const lookalike={dispatch(action){return action;}};\n")]).await?;
        for (category,key,reader) in [("Module","total","crossCount"),("Singleton","cache.current","crossMemo"),
            ("ReactContext","Theme","crossTheme"),("Redux","store.orders.total","crossRedux")] {
            let output=trace(server,&project,category,&format!("state.ts::{key}")).await?;
            ensure!(output.contains(reader) && output.contains("static_import_v1") && !output.contains("shadowed"),"Missing or incorrect imported state: {output}");
            let other=trace(server,&project,category,&format!("other.ts::{key}")).await?;
            ensure!(!other.contains(reader),"Unrelated same-name declaration matched: {other}");
        }
        let module = trace(server,&project,"Module","state.ts::total").await?;
        ensure!(!module.contains("invalidAssignment"),"Assignment to read-only import claimed a state write: {module}");
        let dispatch = server.state.graph.query_nodes(&project,Some("function"),Some("unknownDispatch"),None,10)?;
        let dispatch = dispatch.iter().find(|node|node.name=="unknownDispatch").context("dispatch source")?;
        let edges = server.state.graph.edges_touching(&project,&dispatch.node_id,100)?;
        ensure!(edges.iter().any(|edge|edge.edge_kind==engram_graph::EdgeKind::UnresolvedStateWrite
            && edge.metadata.as_ref().is_some_and(|metadata|metadata.to_string().contains("imported_redux_dispatch_effect_unknown"))),"Imported dispatch effects should be explicitly unknown: {edges:?}");
        let fake = server.state.graph.query_nodes(&project,Some("function"),Some("fakeDispatch"),None,10)?;
        let fake = fake.iter().find(|node|node.name=="fakeDispatch").context("fake dispatch source")?;
        ensure!(!server.state.graph.edges_touching(&project,&fake.node_id,100)?.iter().any(|edge|
            matches!(edge.edge_kind,engram_graph::EdgeKind::WritesState|engram_graph::EdgeKind::UnresolvedStateWrite)),"An unrelated imported dispatch helper became Redux state");
        let context=trace(server,&project,"ReactContext","state.ts::Theme").await?;
        ensure!(context.contains("crossProvider"),"Imported provider missing: {context}");
        let singleton=trace(server,&project,"Singleton","state.ts::cache.current").await?;
        ensure!(singleton.contains("originalMemo"),"Namespace import missing: {singleton}");
        let root=fixture.temporary.as_ref().context("owner")?.path().join("imports");
        std::fs::write(root.join("barrel.ts"),barrel.replace("./state","./other"))?;
        refresh(server,&project).await?;
        for (file,present) in [("state.ts",false),("other.ts",true)] {
            let output=trace(server,&project,"Module",&format!("{file}::total")).await?;
            ensure!(output.contains("crossCount")==present,"Changed reexport did not rebind unchanged caller: {output}");
        }
        std::fs::remove_file(root.join("other.ts"))?;
        refresh(server,&project).await?;
        let output=trace(server,&project,"Module","other.ts::total").await?;
        ensure!(!output.contains("crossCount"),"Deleted declaration retained state reference: {output}");
        let callers=server.state.graph.query_nodes(&project,Some("function"),Some("crossCount"),None,10)?;
        let caller=callers.iter().find(|node|node.name=="crossCount").context("retained caller")?;
        let edges=server.state.graph.edges_touching(&project,&caller.node_id,100)?;
        ensure!(edges.iter().any(|edge|edge.edge_kind==engram_graph::EdgeKind::UnresolvedStateRead
            && edge.metadata.as_ref().is_some_and(|metadata|metadata.to_string().contains("module_not_indexed"))),"Deleted import did not retain unknown state evidence: {edges:?}");
        std::fs::write(root.join("other.ts"),&source)?;
        refresh(server,&project).await?;
        ensure!(trace(server,&project,"Module","other.ts::total").await?.contains("crossCount"),"Restored declaration failed to rebind retained state observation");
        Ok(())
    })
}

#[test]
fn modern_state_javascript_imports_keep_project_relative_state_identity() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.runtime.as_ref().context("runtime")?.block_on(async {
        let server=fixture.server.as_ref().context("server")?;
        let project=fixture.index("scripts",&[("package.json","{}"),
            ("state.ts","export let total=99;\nexport const cache={current:99};\n"),
            ("state.js","export let total=0;\nexport const cache={current:0};\n"),
            ("caller.js","import {total,cache} from './state.js';\nexport function readCount(){return total;}\nexport function writeCache(){cache.current=7;}\n")]).await?;
        ensure!(trace(server,&project,"Module","state.js::total").await?.contains("readCount"),"Missing JS imported mutable module state");
        ensure!(trace(server,&project,"Singleton","state.js::cache.current").await?.contains("writeCache"),"Missing JS imported singleton property write");
        let root=fixture.temporary.as_ref().context("owner")?.path().join("scripts");
        std::fs::write(root.join("caller.js"),"import {total} from './state.js';\nexport function readCount(){return total;}\n")?;
        refresh(server,&project).await?;
        ensure!(server.state.graph.get_node(&project,&engram_core::NodeId::state("Singleton","state.js::cache.current").0)?.is_none(),"Unreferenced imported property node survived refresh");
        ensure!(server.state.graph.get_node(&project,&engram_core::NodeId::state("Singleton","state.js::cache").0)?.is_some(),"Cleanup removed the source-owned singleton declaration");
        Ok(())
    })
}


#[test]
fn browser_storage_public_trace_preserves_store_identity_and_unchanged_readers() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.runtime.as_ref().context("runtime")?.block_on(async {
        let server = fixture.server.as_ref().context("server")?;
        let writer = "export function expand(){localStorage.setItem('panelExpanded','true');}\nexport function reset(){window.localStorage.removeItem('panelExpanded');}\n";
        let reader = "export function isExpanded(){return localStorage.getItem('panelExpanded');}\n";
        let session = "export function sessionReader(){return sessionStorage.getItem('panelExpanded');}\nexport function sessionWriter(){sessionStorage.setItem('panelExpanded','session');}\n";
        let project = fixture.index("storage", &[
            ("package.json", "{}"), ("writer.ts", &writer), ("reader.js", &reader), ("session.js", &session),
            ("decoy.ts", "export function decoy(localStorage: any){localStorage.setItem('panelExpanded','decoy');}\n"),
        ]).await?;
        let before = trace(server, &project, "LocalStorage", "panelExpanded").await?;
        ensure!(before.contains("expand") && before.contains("reset") && before.contains("isExpanded"), "Missing browser state relationships: {before}");
        ensure!(!before.contains("sessionReader") && !before.contains("sessionWriter") && !before.contains("decoy"), "Different store or local object leaked: {before}");
        ensure!(before.contains("Indexed access:") && before.contains(blake3::hash(reader.as_bytes()).to_hex().as_str()) && before.contains(blake3::hash(writer.as_bytes()).to_hex().as_str()), "Missing independently authored source hashes: {before}");
        ensure!(before.contains("Shared origin") && before.contains("not proven"), "Runtime limit missing: {before}");
        let session_before = trace(server, &project, "SessionStorage", "panelExpanded").await?;
        ensure!(session_before.contains("sessionReader") && session_before.contains("sessionWriter") && !session_before.contains("isExpanded"), "Stores merged: {session_before}");
        let root = fixture.temporary.as_ref().context("owner")?.path().join("storage");
        std::fs::write(root.join("writer.ts"), writer.replace("panelExpanded", "newPreference"))?;
        refresh(server, &project).await?;
        let retained = trace(server, &project, "LocalStorage", "panelExpanded").await?;
        ensure!(retained.contains("isExpanded") && !retained.contains("writer.ts:"), "Edit lost unchanged reader or kept old writer: {retained}");
        ensure!(trace(server, &project, "LocalStorage", "newPreference").await?.contains("expand"), "Changed key missing");
        std::fs::remove_file(root.join("writer.ts"))?;
        refresh(server, &project).await?;
        ensure!(trace(server, &project, "LocalStorage", "panelExpanded").await?.contains("isExpanded"), "Deletion lost unrelated reader");
        let removed = trace(server, &project, "LocalStorage", "newPreference").await?;
        ensure!(!removed.contains("writer.ts:"), "Removed writer retained: {removed}");
        ensure!(trace(server, &project, "SessionStorage", "panelExpanded").await?.contains("sessionReader"), "Local-store changes lost session state");
        Ok(())
    })
}
