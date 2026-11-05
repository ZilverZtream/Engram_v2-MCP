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
    let implementation=app.state.graph.query_nodes(pid,Some("function"),None,Some("Service.vb"),16)?
        .into_iter().find(|node|node.start_line==4).ok_or_else(||anyhow::anyhow!("Missing exact Alpha implementation"))?;
    let mut expected=None;
    for guidance in [Some(false),Some(true),None] {
        let mut args=json!({"project_id":pid,"symbol_fqn":implementation.node_id,"max_depth":4,"detail":"graph"});
        if let Some(flag)=guidance {args["include_guidance"]=json!(flag);}
        let result=app.handle_compute_blast_radius(serde_json::from_value(args)?).await?;
        ensure!(result.is_error!=Some(true));
        let text=result.content[0].as_text().unwrap().text.as_str();
        let prefix=text.split_once("## Revision-admitted change impact").unwrap().0;
        ensure!(prefix.contains("Admitted indexed dependents: 0.")
            && prefix.contains("Separate potential interface callers: 2 (2 recorded routes)"),
            "Zero primary count must not conceal useful potential consumers: {prefix}");
        ensure!(text.contains("[engram]"),"Skipping optional advice must retain freshness");
        ensure!(text.contains("## Unverified one-hop advisory profile")==guidance.unwrap_or(true),
            "False omits the entire separate advisory; true/default preserve compatibility");
        let facts=packet(text)?;
        ensure!(facts["selected_dependents_observed"]==0 && facts["potential_interface_callers"]["routes_observed"]==2);
        if let Some(old)=&expected {ensure!(&facts==old,"Advice must not change admitted evidence or potential routes");}
        else {expected=Some(facts);}
    }
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
                ensure!(dispatch.get("configuration_inspection").is_none(),
                    "Root-level declarations must not invent a shared configuration directory");
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
                let body: Value = serde_json::from_str(&read.content[0].as_text().unwrap().text)?;
                ensure!(
                    body["source_code"]
                        .as_str()
                        .unwrap()
                        .contains("Public Class Controller"),
                    "Caller context should expose the nearby declaration, not just the invocation"
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
fn compact_dispatch_keeps_distinct_consumer_files_and_inspectable_caller_context() -> Result<()> {
    if std::env::var_os("ENGRAM_VB_SIDECAR_PATH").is_none() {
        return Ok(());
    }
    let owned = tempfile::TempDir::new()?;
    let parser = ParserOwner;
    let result = (|| -> Result<()> {
        let root = owned.path();
        let repo = root.join("repo");
        let helper = "Site/App_Code/shared-code/UserRightsHelper.vb";
        let caller = "Site/App_Code/api-v2/Controllers/common/UserRightsController.vb";
        let provider = "Site/App_Code/shared-code/UserRightsDataProvider.vb";
        let dashboard = "Site/modules/dashboard/pages/admin/system/userrights/user_rights.aspx.vb";
        let controller = "Namespace Example\nPublic Class HttpGetAttribute\n Inherits System.Attribute\nEnd Class\nPublic Class Controller\n Private ReadOnly _service As IReader\n Public Sub New(service As IReader)\n _service = service\n End Sub\n <HttpGet>\n Public Function GetAll(value As Alpha) As Alpha\n Return _service.Read(value)\n End Function\nEnd Class\nEnd Namespace\n";
        let peer = |name: &str| {
            format!(
                "Namespace Example\nPublic Class {name}\n Public Function BuildRow(value As Alpha) As Alpha\n Return Policy.Resolve(value)\n End Function\nEnd Class\nEnd Namespace\n"
            )
        };
        let provider_source = peer("UserRightsDataProvider");
        let dashboard_source = peer("UserRightsDashboard");
        for (path, source) in [
            (helper, HELPER),
            (
                "Site/App_Code/api-v2/Services/common/interfaces/IUserRightsService.vb",
                CONTRACTS,
            ),
            (
                "Site/App_Code/api-v2/Services/common/UserRightsService.vb",
                SERVICE,
            ),
            (caller, controller),
            ("Site/App_Code/api-v2/Bindings.vb", "Namespace Example\nFriend Module Bindings\n Public Function CreateReader() As IReader\n Return New Reader()\n End Function\nEnd Module\nEnd Namespace\n"),
            (provider, provider_source.as_str()),
            (dashboard, dashboard_source.as_str()),
        ] {
            let path = repo.join(path);
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::write(path, source)?;
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let tested=runtime.block_on(async {
            let (state,_)=AppState::new(config(root))?;let app=Engram::new(state);
            let indexed=app.handle_index_project(serde_json::from_value(json!({"directory":repo,"project_name":"Agent dispatch summary","project_type":"general","wait":true}))?).await?;
            ensure!(indexed.is_error!=Some(true));let pid=app.state.registry.list_projects()?[0].project_id.clone();
            // Real stores can contain older/fallback declarations alongside
            // new exact mappings. Their unknown-dispatch leads must stay
            // visible as uncertainty without hiding confirmed consumer files.
            for path in [provider,dashboard] {
                let mut nodes=app.state.graph.query_nodes(&pid,Some("function"),None,Some(path),16)?;
                ensure!(!nodes.is_empty());
                for node in &mut nodes { if let Some(meta)=node.metadata.as_mut().and_then(Value::as_object_mut) {meta.remove("interface_dispatch_status");} }
                app.state.graph.upsert_nodes(&pid,&nodes)?;
            }
            let parent=app.state.graph.query_nodes(&pid,Some("function"),None,Some(dashboard),16)?
                .into_iter().find(|node|node.name.ends_with(".BuildRow")).unwrap();
            app.state.graph.upsert_edges(&pid,&[engram_graph::Edge {
                source_id:format!("sym:self:{dashboard}:Me:{}",parent.start_line),
                target_id:parent.node_id,namespace:"memory".into(),language:"vb".into(),
                edge_kind:engram_graph::EdgeKind::Calls,weight:1,generation:parent.generation,
                metadata:None,updated_at_ms:1,
            }])?;
            for tool in ["impact_analysis","compute_blast_radius"] {
                let args=json!({"project_id":pid,"file_path":helper,"detail":"summary","max_depth":6});
                let response=if tool=="impact_analysis" {app.handle_impact_analysis(serde_json::from_value(args)?).await?}
                    else {app.handle_compute_blast_radius(serde_json::from_value(args)?).await?};
                ensure!(response.is_error!=Some(true));
                let text=response.content.iter().filter_map(|p|p.as_text()).map(|p|p.text.as_str()).collect::<String>();
                ensure!(text.len()<=12*1024);let summary=packet(&text)?;
                ensure!(summary["direct_dependents_observed"]==3);
                ensure!(summary["enclosing_declaration_scope"].as_str().is_some_and(|scope|scope.contains("unresolved")),
                    "Receiver context uncertainty must stay explicit without crowding consumer files");
                let files:std::collections::BTreeSet<_>=summary["immediate_consumers"].as_array().unwrap().iter().map(|row|row["file_path"].as_str().unwrap()).collect();
                ensure!(files.len()==3 && files.contains(provider) && files.contains(dashboard),
                    "Potential dispatch and optional gap rows must not hide distinct ordinary consumers: {summary}");
                let dispatch=&summary["potential_interface_callers"];ensure!(dispatch["routes_observed"]==1 && dispatch["routes"].as_array().unwrap().len()==1);
                ensure!(dispatch["coverage"]["truncated"]==true,"Legacy dispatch remains explicitly unknown");
                let route=&dispatch["routes"][0];let action=&route["caller_source_read"];
                // A crowded summary keeps all three consumer files and its
                // unknown frontiers; follow its literal graph action for an
                // optional configuration hint that did not fit.
                let expanded;
                let inspection=if let Some(inspection)=dispatch.get("configuration_inspection") { inspection } else {
                    ensure!(dispatch["configuration_inspection_actions_omitted"]==1);
                    let continuation=&summary["graph_continuation"];
                    let full=app.handle_impact_analysis(serde_json::from_value(continuation["arguments"].clone())?).await?;
                    ensure!(full.is_error!=Some(true));
                    expanded=packet(&full.content[0].as_text().unwrap().text)?;
                    &expanded["potential_interface_callers"]["configuration_inspection"]
                };
                ensure!(inspection["actions"].as_array().is_some_and(|actions|actions.len()==1)
                    && inspection["actions_omitted"]==0,
                    "A usable contract route needs bounded configuration discovery without losing consumer files: {dispatch}");
                let search=&inspection["actions"][0];ensure!(search["tool"]=="grep_project");
                ensure!(search["arguments"]["path_prefix"]=="Site/App_Code/api-v2/"
                    && search["arguments"]["pattern"]=="IReader"
                    && search["arguments"]["regex"]==false);
                let found=app.handle_grep_project(serde_json::from_value(search["arguments"].clone())?).await?;
                ensure!(found.is_error!=Some(true),"Advertised configuration search must execute unchanged");
                let found:Value=serde_json::from_str(&found.content[0].as_text().unwrap().text)?;
                ensure!(found["matches"].as_array().unwrap().iter().any(|hit|
                    hit["file_path"]=="Site/App_Code/api-v2/Bindings.vb" && hit["source"]=="index"
                        && hit["line_text"].as_str().is_some_and(|line|line.contains("CreateReader"))),
                    "The agent must discover the real binding factory without a guessed configuration filename: {found}");
                ensure!(dispatch["runtime_dispatch_proven"]==false && dispatch["non_additive_to_primary_counts"]==true,
                    "A text search suggestion must not promote a potential route into runtime proof");
                let read=app.handle_get_full_method_body(serde_json::from_value(action["arguments"].clone())?).await?;
                let body:Value=serde_json::from_str(&read.content[0].as_text().unwrap().text)?;
                let source=body["source_code"].as_str().unwrap();
                ensure!(source.contains("<HttpGet>") && source.contains("Public Sub New") && source.contains("Private ReadOnly _service") && source.contains("Return _service.Read(value)"),
                    "One advertised read must expose nearby entry/receiver context and the invocation");
                ensure!(body["source_file_hash"]==engram_core::ContentHash::compute(controller.as_bytes()).0);
                ensure!(route["reference_samples_outside_context"]==0 && route["recorded_position_read"].is_null());
            }
            let padding=" ' unrelated work\n".repeat(120);
            let long=controller.replace(" Return _service.Read(value)",&format!("{padding} Return _service.Read(value)"));
            std::fs::write(repo.join(caller),&long)?;
            let updated=app.handle_update_project(serde_json::from_value(json!({"project_id":pid,"wait":true,"max_commits":1,"index_antipatterns":false}))?).await?;
            ensure!(updated.is_error!=Some(true));
            let response=app.handle_impact_analysis(serde_json::from_value(json!({"project_id":pid,"file_path":helper,"detail":"summary","max_depth":6}))?).await?;
            let text=response.content[0].as_text().unwrap().text.clone();let summary=packet(&text)?;
            let route=&summary["potential_interface_callers"]["routes"][0];
            let expected=long.lines().position(|line|line.contains("Return _service.Read(value)")).unwrap()+1;
            ensure!(route["connections"][1]["reference_lines"]==json!([expected]) && route["reference_samples_outside_context"]==1);
            let context=app.handle_get_full_method_body(serde_json::from_value(route["caller_source_read"]["arguments"].clone())?).await?;
            let context:Value=serde_json::from_str(&context.content[0].as_text().unwrap().text)?;
            ensure!(!context["source_code"].as_str().unwrap().contains("Return _service.Read(value)"));
            let reference=app.handle_get_full_method_body(serde_json::from_value(route["recorded_position_read"]["arguments"].clone())?).await?;
            let reference:Value=serde_json::from_str(&reference.content[0].as_text().unwrap().text)?;
            ensure!(reference["source_code"].as_str().unwrap().contains("Return _service.Read(value)"),"A distant callsite still needs a literal read action");
            settle(&app).await?;Ok::<_,anyhow::Error>(())
        });
        drop(runtime);
        tested
    })();
    let stopped = engram_index::vb_extractor::shutdown_idle_parser();
    drop(parser);
    let cleanup = owned.close();
    result?;
    stopped?;
    cleanup?;
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
