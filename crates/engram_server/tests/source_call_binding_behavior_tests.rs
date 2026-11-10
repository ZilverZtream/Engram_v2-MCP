//! Exact compiler-selected overloads must remain usable through the public tools.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{AppState, Engram};
use serde_json::{Value, json};
use std::path::Path;

struct ParserOwner;
impl Drop for ParserOwner {
    fn drop(&mut self) {
        let _ = engram_index::vb_extractor::shutdown_idle_parser();
    }
}
const LOGGER: &str = "Namespace Example\nPublic Class Logger\n Public Shared Sub Create(m As String, f As String, e As String, ok As Boolean, current As Boolean, Optional message As String = \"\", Optional extra As String = \"\")\n ' Boolean logging path\n End Sub\n Public Shared Sub Create(m As String, f As String, e As String, ok As Boolean, user As System.Guid, Optional message As String = \"\")\n ' Guid logging path\n End Sub\nEnd Class\nEnd Namespace\n";
const CALLER: &str = "Namespace Example\nPublic Class Controller\n Public Sub DeleteLayers()\n Logger.Create(\"map\", \"delete\", \"done\", True, True)\n End Sub\n Public Sub DeleteForUser(user As System.Guid)\n Logger.Create(\"map\", \"delete\", \"done\", True, user)\n End Sub\nEnd Class\nEnd Namespace\n";
fn config(root: &Path) -> Config {
    Config {
        data_dir: root.join("data"),
        allowed_roots: vec![root.join("repo")],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    }
}
async fn settle(app: &Engram) -> Result<()> {
    loop {
        let jobs = app
            .state
            .active_jobs
            .write()
            .await
            .drain()
            .map(|(_, job)| job)
            .collect::<Vec<_>>();
        if jobs.is_empty() {
            return Ok(());
        }
        for job in jobs {
            job.await?;
        }
    }
}
fn packet(text: &str, heading: &str) -> Result<Value> {
    let rest = text
        .split_once(heading)
        .ok_or_else(|| anyhow::anyhow!("Missing {heading}: {text}"))?
        .1;
    let body = rest
        .split_once("```json")
        .ok_or_else(|| anyhow::anyhow!("Missing JSON: {rest}"))?
        .1
        .split_once("```")
        .unwrap()
        .0;
    Ok(serde_json::from_str(body)?)
}
async fn verify(app: &Engram, pid: &str) -> Result<()> {
    let callers =
        app.state
            .graph
            .query_nodes(pid, Some("function"), None, Some("Caller.vb"), 16)?;
    let targets =
        app.state
            .graph
            .query_nodes(pid, Some("function"), None, Some("Logger.vb"), 16)?;
    for (caller_name, line, target_line, body_marker, other_name) in [
        (
            "Example.Controller.DeleteLayers",
            4,
            3,
            "Boolean logging path",
            "DeleteForUser",
        ),
        (
            "Example.Controller.DeleteForUser",
            7,
            6,
            "Guid logging path",
            "DeleteLayers",
        ),
    ] {
        let caller = callers.iter().find(|n| n.name == caller_name).unwrap();
        let target = targets
            .iter()
            .find(|n| n.start_line == target_line)
            .unwrap();
        let result = app
            .handle_get_codebase_overview(serde_json::from_value(json!({
                "project_id":pid,"scope":"Caller.vb","symbol_node_id":caller.node_id
            }))?)
            .await?;
        ensure!(result.is_error != Some(true), "CodeMap failed: {result:?}");
        let text = result
            .content
            .iter()
            .filter_map(|p| p.as_text())
            .map(|p| p.text.as_str())
            .collect::<String>();
        let map = packet(&text, "## CodeMap Navigation")?;
        let items = map["called_declarations"]["items"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("No callable recovery: {map}"))?;
        let row = items
            .iter()
            .find(|row| row["node_id"] == target.node_id)
            .ok_or_else(|| anyhow::anyhow!("Exact overload missing: {map}"))?;
        ensure!(
            items.len() == 1 && row["reference_lines"] == json!([line]),
            "Wrong overload or witness: {items:?}"
        );
        ensure!(row["source_read"]["tool"] == "get_full_method_body");
        let read = app
            .handle_get_full_method_body(serde_json::from_value(
                row["source_read"]["arguments"].clone(),
            )?)
            .await?;
        ensure!(read.is_error != Some(true));
        let body: Value = serde_json::from_str(&read.content[0].as_text().unwrap().text)?;
        ensure!(
            body["source_code"].as_str().unwrap().contains(body_marker),
            "Exact recovery must read the selected overload: {body}"
        );
        for tool in ["impact_analysis", "compute_blast_radius"] {
            let mut args = json!({"project_id":pid,"symbol_fqn":target.node_id,"max_depth":2,"detail":"graph"});
            if tool == "compute_blast_radius" {
                args["include_guidance"] = json!(false);
            }
            let result = if tool == "impact_analysis" {
                app.handle_impact_analysis(serde_json::from_value(args)?)
                    .await?
            } else {
                app.handle_compute_blast_radius(serde_json::from_value(args)?)
                    .await?
            };
            ensure!(result.is_error != Some(true));
            let text = &result.content[0].as_text().unwrap().text;
            let impact = packet(text, "## Revision-admitted change impact")?;
            ensure!(
                impact["selected_dependents_observed"] == 1,
                "Exactly the compiler-selected caller must be reached: {impact}"
            );
            ensure!(
                impact.to_string().contains(caller_name)
                    && !impact.to_string().contains(other_name),
                "Overload callers crossed: {impact}"
            );
        }
    }
    Ok(())
}

#[test]
fn actual_native_source_overloads_survive_caller_refresh_and_reopen() -> Result<()> {
    if !cfg!(windows) || std::env::var_os("ENGRAM_VB_SIDECAR_PATH").is_none() {
        return Ok(());
    }
    let owned = tempfile::TempDir::new()?;
    let parser = ParserOwner;
    let result = (|| -> Result<()> {
        let root = owned.path();
        std::fs::create_dir_all(root.join("repo"))?;
        for (path, body) in [
            ("Logger.vb", LOGGER),
            ("Caller.vb", CALLER),
            (
                "web.config",
                r#"<configuration>
<system.web><compilation targetFramework="4.8" defaultLanguage="vb" debug="true"><assemblies>
<add assembly="System.Data.Linq, Version=4.0.0.0, Culture=neutral, PublicKeyToken=b77a5c561934e089"/>
</assemblies></compilation><pages><namespaces><clear/>
<add namespace="System"/><add namespace="System.Collections.Generic"/><add namespace="System.Linq"/>
</namespaces></pages></system.web>
<system.codedom><compilers><compiler language="vb;vbs;visualbasic;vbscript" extension=".vb"
type="Microsoft.CodeDom.Providers.DotNetCompilerPlatform.VBCodeProvider, Microsoft.CodeDom.Providers.DotNetCompilerPlatform, Version=4.1.0.0, Culture=neutral, PublicKeyToken=31bf3856ad364e35"
compilerOptions="/optioninfer+"><providerOption name="UseAspNetSettings" value="true"/>
</compiler></compilers></system.codedom></configuration>"#,
            ),
        ] {
            std::fs::write(root.join("repo").join(path), body)?;
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let phase=runtime.block_on(async {
            let (state,_)=AppState::new(config(root))?;let app=Engram::new(state);
            let indexed=app.handle_index_project(serde_json::from_value(json!({"directory":root.join("repo"),"project_name":"Source call behavior","project_type":"general","wait":true}))?).await?;
            ensure!(indexed.is_error!=Some(true),"Native indexing failed: {indexed:?}");
            let pid=app.state.registry.list_projects()?[0].project_id.clone();verify(&app,&pid).await?;
            let inherited=app.state.graph.get_node(&pid,"file:Logger.vb")?.unwrap().generation;
            std::fs::write(root.join("repo/Caller.vb"),format!("{CALLER}' caller-only edit\n"))?;
            let updated=app.handle_update_project(serde_json::from_value(json!({"project_id":pid,"wait":true,"max_commits":1,"index_antipatterns":false}))?).await?;
            ensure!(updated.is_error!=Some(true),"Incremental update failed: {updated:?}");
            ensure!(app.state.graph.get_node(&pid,"file:Logger.vb")?.unwrap().generation==inherited,"Target must remain an inherited selected revision");
            verify(&app,&pid).await?;settle(&app).await?;Ok::<_,anyhow::Error>(pid)
        });
        drop(runtime);
        let pid = phase?;
        engram_index::vb_extractor::shutdown_idle_parser()?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let phase = runtime.block_on(async {
            let (state, _) = AppState::new(config(root))?;
            let app = Engram::new(state);
            engram_server::services::project_service::ensure_project_runtime(&app.state, &pid)
                .await?;
            verify(&app, &pid).await?;
            settle(&app).await?;
            Ok::<_, anyhow::Error>(())
        });
        drop(runtime);
        phase
    })();
    let stopped = engram_index::vb_extractor::shutdown_idle_parser();
    drop(parser);
    let cleanup = owned.close();
    result?;
    stopped?;
    cleanup?;
    Ok(())
}
