//! An agent must see interface-mediated consumers of a reached implementation
//! without being told they are direct callers or that runtime dispatch is proven.
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

const CONTRACTS: &str = "Namespace Example\nPublic Class Alpha\nEnd Class\nPublic Class Beta\nEnd Class\nPublic Interface IReader\n Function Read(value As Alpha) As Alpha\n Function Read(value As Beta) As Beta\nEnd Interface\nPublic Interface IBox(Of T)\n Function Read(value As T) As T\nEnd Interface\nEnd Namespace\n";
const HELPER: &str = "Namespace Example\nPublic Class Policy\n Public Shared Function Resolve(value As Alpha) As Alpha\n Return value\n End Function\nEnd Class\nEnd Namespace\n";
const SERVICE: &str = "Namespace Example\nPublic Class Reader\n Implements IReader, IBox(Of Alpha), IBox(Of Beta)\n Public Function Fetch(value As Alpha) As Alpha Implements IReader.Read, IBox(Of Alpha).Read\n Return Policy.Resolve(value)\n End Function\n Public Function Fetch(value As Beta) As Beta Implements IReader.Read, IBox(Of Beta).Read\n Return value\n End Function\nEnd Class\nEnd Namespace\n";
const CALLER: &str = "Namespace Example\nPublic Class Controller\n Public Function First(service As IReader, value As Alpha) As Alpha\n Return service.Read(value)\n End Function\n Public Function Second(service As IReader, value As Beta) As Beta\n Return service.Read(value)\n End Function\n Public Sub Both(first As IBox(Of Alpha), second As IBox(Of Beta), a As Alpha, b As Beta)\n Dim one = first.Read(a)\n Dim two = second.Read(b)\n End Sub\nEnd Class\nEnd Namespace\n";

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

fn packet(text: &str) -> Result<Value> {
    let text = text
        .split_once("## Revision-admitted change impact")
        .ok_or_else(|| anyhow::anyhow!("Missing impact section: {text}"))?
        .1;
    let text = text
        .split_once("```json")
        .ok_or_else(|| anyhow::anyhow!("Missing JSON"))?
        .1
        .split_once("```")
        .unwrap()
        .0;
    Ok(serde_json::from_str(text)?)
}

async fn impact(app: &Engram, pid: &str, tool: &str, detail: &str, depth: usize) -> Result<Value> {
    let args = json!({"project_id":pid,"symbol_fqn":"Example.Policy.Resolve",
        "max_depth":depth,"detail":detail});
    let response = match tool {
        "impact_analysis" => {
            app.handle_impact_analysis(serde_json::from_value(args)?)
                .await?
        }
        _ => {
            app.handle_compute_blast_radius(serde_json::from_value(args)?)
                .await?
        }
    };
    ensure!(
        response.is_error != Some(true),
        "Impact failed: {response:?}"
    );
    let text = response
        .content
        .iter()
        .filter_map(|part| part.as_text())
        .map(|part| part.text.as_str())
        .collect::<String>();
    if detail == "summary" {
        ensure!(
            text.len() <= 12 * 1024,
            "Agent summary exceeded its context budget"
        );
    }
    packet(&text)
}

async fn verify(app: &Engram, pid: &str) -> Result<()> {
    for tool in ["impact_analysis", "compute_blast_radius"] {
        for detail in ["summary", "graph"] {
            let report = impact(app, pid, tool, detail, 4).await?;
            ensure!(
                report["direct_dependents_observed"] == 1
                    || detail == "graph" && report["selected_dependents_observed"] == 1,
                "Primary callers must stay separate from interface potential callers: {report}"
            );
            let dispatch = &report["potential_interface_callers"];
            ensure!(
                dispatch["runtime_dispatch_proven"] == false
                    && dispatch["non_additive_to_primary_counts"] == true
            );
            ensure!(
                dispatch["routes_observed"] == 2,
                "Expected exact IReader Alpha and IBox Alpha consumers: {dispatch}"
            );
            let routes = dispatch["routes"].as_array().unwrap();
            ensure!(
                routes.len() == 2,
                "Both representative potential callers must be usable in default output: {dispatch}"
            );
            ensure!(
                routes
                    .iter()
                    .any(|row| row["caller"]["name"] == "Example.Controller.First")
            );
            ensure!(
                routes
                    .iter()
                    .any(|row| row["caller"]["name"] == "Example.Controller.Both")
            );
            ensure!(
                !routes
                    .iter()
                    .any(|row| row["caller"]["name"] == "Example.Controller.Second"),
                "A different overload is not a potential caller"
            );
            for row in routes {
                ensure!(row["implementation_primary_depth"] == 1 && row["potential_depth"] == 3);
                ensure!(
                    row["connections"][0]["source"] == "implementation"
                        && row["connections"][0]["target"] == "contract"
                );
                ensure!(
                    row["connections"][1]["source"] == "caller"
                        && row["connections"][1]["target"] == "contract"
                );
                if row["caller"]["name"] == "Example.Controller.Both" {
                    ensure!(
                        row["connections"][1]["reference_lines"] == json!([10]),
                        "Do not attribute the Beta call at line11 to the Alpha implementation: {row}"
                    );
                }
                ensure!(row["caller_source_read"]["tool"] == "get_full_method_body");
                let read = app
                    .handle_get_full_method_body(serde_json::from_value(
                        row["caller_source_read"]["arguments"].clone(),
                    )?)
                    .await?;
                ensure!(
                    read.is_error != Some(true),
                    "The advertised agent source action must execute unchanged"
                );
            }
        }
    }
    let shallow = impact(app, pid, "impact_analysis", "summary", 2).await?;
    ensure!(shallow["potential_interface_callers"]["routes_observed"] == 0);
    ensure!(
        shallow["potential_interface_callers"]["coverage"]["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| reason == "interface_dispatch_depth_limit"),
        "Depth-limited dispatch is an explicit boundary, not evidence of no consumers"
    );
    for lead in shallow["potential_interface_callers"]["frontier"]
        .as_array()
        .unwrap()
    {
        ensure!(
            lead["inspection"]["tool"] == "get_full_method_body",
            "A known boundary needs an executable source action: {lead}"
        );
        let read = app
            .handle_get_full_method_body(serde_json::from_value(
                lead["inspection"]["arguments"].clone(),
            )?)
            .await?;
        ensure!(read.is_error != Some(true));
    }
    Ok(())
}

#[test]
fn actual_native_interface_callers_survive_incremental_refresh_and_cold_reopen() -> Result<()> {
    // If a runner explicitly supplies a companion, absence or fallback is a
    // failure. Companion-free developer runs may skip this native-only case.
    if std::env::var_os("ENGRAM_VB_SIDECAR_PATH").is_none() {
        return Ok(());
    }
    let owned = tempfile::TempDir::new()?;
    let parser = ParserOwner;
    let result = (|| -> Result<()> {
        let root = owned.path();
        std::fs::create_dir_all(root.join("repo"))?;
        for (path, source) in [
            ("Contracts.vb", CONTRACTS),
            ("Helper.vb", HELPER),
            ("Service.vb", SERVICE),
            ("Caller.vb", CALLER),
        ] {
            std::fs::write(root.join("repo").join(path), source)?;
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let phase=runtime.block_on(async {
            let (state,_)=AppState::new(config(root))?; let app=Engram::new(state);
            let indexed=app.handle_index_project(serde_json::from_value(json!({"directory":root.join("repo"),"project_name":"Interface dispatch behavior","project_type":"general","wait":true}))?).await?;
            ensure!(indexed.is_error!=Some(true),"Native indexing failed: {indexed:?}");
            let pid=app.state.registry.list_projects()?[0].project_id.clone();
            verify(&app,&pid).await?;
            // A selected caller with an invalid physical fingerprint is an
            // inspectable unknown, never a verified interface consumer.
            let original=app.state.graph.get_node(&pid,"file:Caller.vb")?.unwrap();
            let mut invalid=original.clone(); invalid.metadata.as_mut().unwrap()["file_hash"]=json!("invalid");
            app.state.graph.upsert_nodes(&pid,&[invalid])?;
            let report=impact(&app,&pid,"impact_analysis","summary",4).await?;
            let dispatch=&report["potential_interface_callers"];
            ensure!(dispatch["routes_observed"]==0 && dispatch["coverage"]["truncated"]==true);
            ensure!(dispatch["frontier"].as_array().unwrap().iter().any(|lead|lead["source_id"].as_str().unwrap_or("").contains("Controller.First")
                && lead["inspection"]["tool"]=="get_full_method_body"),"Rejected caller needs a named working inspection lead: {dispatch}");
            app.state.graph.upsert_nodes(&pid,&[original])?;
            verify(&app,&pid).await?;
            let unchanged=app.state.graph.get_node(&pid,"file:Contracts.vb")?.unwrap().generation;
            let before=app.state.registry.active_source_generation(&pid)?;
            std::fs::write(root.join("repo/Caller.vb"),format!("{CALLER}' caller-only edit\n"))?;
            let updated=app.handle_update_project(serde_json::from_value(json!({"project_id":pid,"wait":true,"max_commits":1,"index_antipatterns":false}))?).await?;
            ensure!(updated.is_error!=Some(true),"Incremental update failed: {updated:?}");
            ensure!(app.state.registry.active_source_generation(&pid)?>before);
            ensure!(app.state.graph.get_node(&pid,"file:Contracts.vb")?.unwrap().generation==unchanged,
                "Caller-only update must retain the selected contract revision");
            verify(&app,&pid).await?;
            // Replacing the one shared Calls row must retire the removed
            // constructed-type support, while retaining the Beta call.
            std::fs::write(root.join("repo/Caller.vb"),CALLER.replace(" Dim one = first.Read(a)"," ' Alpha call removed"))?;
            let removed=app.handle_update_project(serde_json::from_value(json!({"project_id":pid,"wait":true,"max_commits":1,"index_antipatterns":false}))?).await?;
            ensure!(removed.is_error!=Some(true));
            let report=impact(&app,&pid,"impact_analysis","summary",4).await?;
            let dispatch=&report["potential_interface_callers"];
            ensure!(dispatch["routes_observed"]==1 && dispatch["routes"][0]["caller"]["name"]=="Example.Controller.First",
                "A removed Alpha call must not survive through old merged evidence: {dispatch}");
            std::fs::write(root.join("repo/Caller.vb"),format!("{CALLER}' restored Alpha call\n"))?;
            let restored=app.handle_update_project(serde_json::from_value(json!({"project_id":pid,"wait":true,"max_commits":1,"index_antipatterns":false}))?).await?;
            ensure!(restored.is_error!=Some(true));
            verify(&app,&pid).await?;
            settle(&app).await?;
            Ok::<_,anyhow::Error>(pid)
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
