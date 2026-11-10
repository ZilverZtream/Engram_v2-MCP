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

async fn forward_codemap(
    app: &Engram,
    pid: &str,
    name: &str,
    source: bool,
    exclude: &[&str],
) -> Result<Value> {
    let response = app.handle_get_codebase_overview(serde_json::from_value(json!({
        "project_id":pid,"scope":"Caller.vb","symbol_name":name,
        "include_source":source,"include_declarations":true,
        "include_retained_context":false,"exclude_paths":exclude,"limit":4
    }))?).await?;
    ensure!(response.is_error != Some(true), "CodeMap failed: {response:?}");
    let text = response.content.iter().filter_map(|part|part.as_text())
        .map(|part|part.text.as_str()).collect::<String>();
    ensure!(text.len() <= if source {12*1024} else {64*1024},
        "Interface recovery must fit the existing complete-response cap");
    let json = text.split_once("## CodeMap Navigation").and_then(|(_,body)|body.split_once("```json"))
        .and_then(|(_,body)|body.split_once("```"))
        .ok_or_else(||anyhow::anyhow!("Missing CodeMap navigation: {text}"))?.0;
    Ok(serde_json::from_str(json)?)
}

// Starting at a caller must reach implementation source without asking the
// agent to guess a concrete service name or read an entire service file.
// The original Calls target remains the contract; these are potential routes.
async fn verify_forward_codemap(app: &Engram, pid: &str) -> Result<()> {
    for (name, expected) in [
        ("First", vec![(4u64,4u64)]),
        ("Second", vec![(7,7)]),
        ("Both", vec![(4,10),(7,11)]),
    ] {
        let source = forward_codemap(app,pid,name,true,&[]).await?;
        let metadata = forward_codemap(app,pid,name,false,&[]).await?;
        let primary = metadata["called_declarations"]["items"].as_array()
            .ok_or_else(||anyhow::anyhow!("Missing direct contract call: {metadata}"))?;
        ensure!(!primary.is_empty() && primary.iter().all(|row|row["file_path"]=="Contracts.vb"),
            "Potential implementation navigation must not invent direct calls to Service.vb");
        ensure!(source["called_declarations"]["admitted_observed"]==primary.len(),
            "Source-view direct callee count must still describe indexed Calls only");
        ensure!(source["current_source"]["source_code"].as_str().is_some_and(|body|
            body.contains(&format!(" {name}("))), "Caller source must remain available");
        for page in [&source,&metadata] {
            let dispatch = &page["called_declarations"]["potential_implementations"];
            ensure!(dispatch["runtime_dispatch_proven"]==false,
                "Explicit Implements evidence is not runtime selection: {dispatch}");
            let routes = dispatch["routes"].as_array()
                .ok_or_else(||anyhow::anyhow!("Implementation bodies need direct source routes: {dispatch}"))?;
            ensure!(routes.len()==expected.len() && dispatch["routes_omitted_from_display"]==0,
                "This small caller needs all exact overload/constructed-type routes in its first source reply: {dispatch}");
            let observed: std::collections::BTreeSet<_> = routes.iter().map(|row| {
                (row["implementation"]["start_line"].as_u64().unwrap(),
                 row["reference_lines"][0].as_u64().unwrap())
            }).collect();
            ensure!(observed==expected.iter().copied().collect(),
                "Do not cross Alpha/Beta overloads or constructed IBox call positions: {dispatch}");
            for route in routes {
                ensure!(route["caller"]["name"]==format!("Example.Controller.{name}"));
                ensure!(primary.iter().any(|row|row["node_id"]==route["contract"]["node_id"]),
                    "Each potential route must retain its actual direct contract target");
                ensure!(route["implementation"]["file_path"]=="Service.vb");
                ensure!(route["reference_lines"].as_array().is_some_and(|lines|lines.len()==1)
                    && route["reference_lines_truncated"]==false);
                ensure!(route["clause_line"]==route["implementation"]["start_line"]);
                let action = &route["implementation_source_read"];
                ensure!(action["tool"]=="get_full_method_body");
                let read = app.handle_get_full_method_body(
                    serde_json::from_value(action["arguments"].clone())?).await?;
                ensure!(read.is_error!=Some(true), "Advertised implementation read must execute unchanged");
                let body:Value=serde_json::from_str(&read.content[0].as_text().unwrap().text)?;
                let start=route["implementation"]["start_line"].as_u64().unwrap() as usize;
                let expected_body=SERVICE.lines().skip(start-1).take(3).collect::<Vec<_>>().join("\n");
                ensure!(body["source_code"]==expected_body
                    && body["source_file_hash"]==engram_core::ContentHash::compute(SERVICE.as_bytes()).0,
                    "The literal action must recover the exact three-line implementation and current file hash, not its overload or a signature: {body}");
            }
            if name=="Both" {
                ensure!(routes[0]["contract_type_key"]!=routes[1]["contract_type_key"],
                    "Calls to two constructed contracts cannot collapse to the same dispatch proof");
            }
        }
        let excluded=forward_codemap(app,pid,name,true,&["Service.vb"]).await?;
        ensure!(excluded["current_source"]==source["current_source"],
            "Excluding implementations cannot displace caller source/hash/pagination");
        let dispatch=&excluded["called_declarations"]["potential_implementations"];
        ensure!(dispatch["routes"].as_array().is_some_and(Vec::is_empty)
            && dispatch["excluded_nodes"].as_u64().is_some_and(|count|count>0),
            "Explicit target exclusions must survive the forward interface hop: {dispatch}");
        let again=forward_codemap(app,pid,name,false,&[]).await?;
        ensure!(again["called_declarations"]["items"]==metadata["called_declarations"]["items"],
            "Following implementation source actions must not modify primary call facts");
    }
    Ok(())
}

async fn verify(app: &Engram, pid: &str) -> Result<()> {
    verify_forward_codemap(app,pid).await?;
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
    // The default display can reuse its target identity, but an agent must be
    // able to recover both exact constructed-contract routes and read callers.
    for tool in ["impact_analysis", "compute_blast_radius"] {
        let args=json!({"project_id":pid,"symbol_fqn":implementation.node_id,"max_depth":4,"detail":"summary"});
        let result=if tool=="impact_analysis" {
            app.handle_impact_analysis(serde_json::from_value(args)?).await?
        } else { app.handle_compute_blast_radius(serde_json::from_value(args)?).await? };
        ensure!(result.is_error!=Some(true));
        let summary=packet(&result.content[0].as_text().unwrap().text)?;
        let routes=summary["potential_interface_callers"]["routes"].as_array().unwrap();
        ensure!(routes.len()==2 && summary["potential_interface_callers"]["runtime_dispatch_proven"]==false);
        for route in routes {
            ensure!(route["implementation_ref"]=="target" && route.get("implementation").is_none());
            let mut expanded=route.clone();expanded.as_object_mut().unwrap().remove("implementation_ref");
            let mut endpoint=serde_json::Map::new();
            for key in ["node_id","name","file_path","generation","start_line","end_line"] {
                ensure!(!summary["target"][key].is_null(),"Reference cannot require another lookup to identify its implementation");
                endpoint.insert(key.into(),summary["target"][key].clone());
            }
            expanded["implementation"]=Value::Object(endpoint);
            ensure!(expected.as_ref().unwrap()["potential_interface_callers"]["routes"].as_array().unwrap().contains(&expanded),
                "Expanded summary must retain the exact graph route, positions and literal reads");
            let action=&route["caller_source_read"];
            let read=app.handle_get_full_method_body(serde_json::from_value(action["arguments"].clone())?).await?;
            ensure!(read.is_error!=Some(true));
            let body:Value=serde_json::from_str(&read.content[0].as_text().unwrap().text)?;
            let first=action["arguments"]["line_start"].as_u64().unwrap() as usize;
            let last=action["arguments"]["line_end"].as_u64().unwrap() as usize;
            ensure!(body["source_code"]==CALLER.lines().skip(first-1).take(last-first+1).collect::<Vec<_>>().join("\n"),
                "The compact summary must still recover actual caller source with its literal action");
        }
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
                ensure!(row["caller_to_contract_admission"]=="source_revision_admitted_indexed_call",
                    "Admitted contract calls must be distinguishable from unverified concrete dispatch");
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
            let receiver_id=format!("sym:self:{dashboard}:Me:{}",parent.start_line);
            let parent_id=parent.node_id.clone();
            app.state.graph.upsert_edges(&pid,&[engram_graph::Edge {
                source_id:receiver_id.clone(),
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
                let unresolved_scope=summary["enclosing_declaration_scope"].as_str()
                    .or_else(||summary["limitations"].as_array()?.iter().filter_map(Value::as_str)
                        .find(|text|text.contains("Enclosing recovery leaves receiver/edge unresolved")));
                ensure!(unresolved_scope.is_some_and(|scope|scope.contains("unresolved")),
                    "Receiver context uncertainty must stay explicit without crowding consumer files: {summary}");
                let files:std::collections::BTreeSet<_>=summary["immediate_consumers"].as_array().unwrap().iter().map(|row|row["file_path"].as_str().unwrap()).collect();
                // Compact schema3 retains the unresolved caveat and a counted
                // frontier omission. Its literal graph action must still expose
                // the exact receiver as unresolved and recover its enclosing body.
                ensure!(summary["frontier"].as_array().is_some_and(|rows|!rows.is_empty())
                    || summary["frontier_rows_omitted_from_summary"].as_u64().is_some_and(|count|count>0));
                let continuation=&summary["graph_continuation"];
                ensure!(continuation["tool"]=="impact_analysis");
                let full=app.handle_impact_analysis(serde_json::from_value(continuation["arguments"].clone())?).await?;
                ensure!(full.is_error!=Some(true));
                let expanded=packet(&full.content[0].as_text().unwrap().text)?;
                let receiver=expanded["nodes"].as_array().unwrap().iter().find(|node|node["node_id"]==receiver_id)
                    .ok_or_else(||anyhow::anyhow!("Unresolved receiver was lost from graph continuation: {expanded}"))?;
                ensure!(receiver["admission"]=="unresolved");
                ensure!(expanded["frontier"].as_array().unwrap().iter().any(|row|
                    row["source_ref"]==receiver["ref"] && row["reason"]=="missing_graph_node"));
                let recovery=expanded["recoveries"].as_array().unwrap().iter().find(|row|row["ref"]==receiver["recovery_ref"]).unwrap();
                ensure!(recovery["scope_ref"]=="enclosing_declaration" && recovery["tool"]=="get_codebase_overview"
                    && recovery["arguments"]["symbol_node_id"]==parent_id,
                    "An enclosing source action cannot resolve the receiver or select another method: {recovery}");
                let enclosing=app.handle_get_codebase_overview(serde_json::from_value(recovery["arguments"].clone())?).await?;
                ensure!(enclosing.is_error!=Some(true),"Literal enclosing navigation must remain executable");
                ensure!(files.len()==3 && files.contains(provider) && files.contains(dashboard),
                    "Potential dispatch and optional gap rows must not hide distinct ordinary consumers: {summary}");
                let dispatch=&summary["potential_interface_callers"];ensure!(dispatch["routes_observed"]==1 && dispatch["routes"].as_array().unwrap().len()==1);
                ensure!(dispatch["coverage"]["truncated"]==true,"Legacy dispatch remains explicitly unknown");
                let route=&dispatch["routes"][0];let action=&route["caller_source_read"];
                ensure!(dispatch.get("configuration_inspection").is_none()
                    && (dispatch["configuration_inspection_in_graph"]==true
                        || dispatch["configuration_inspection_actions_omitted"].as_u64().is_some_and(|count|count>0)),
                    "A moved or previously budget-omitted plan needs explicit recovery/omission disclosure: {dispatch}");
                // A crowded summary keeps all three consumer files and its
                // unknown frontiers; follow its literal graph action for an
                // optional configuration hint that did not fit.
                let inspection=dispatch.get("configuration_inspection")
                    .unwrap_or(&expanded["potential_interface_callers"]["configuration_inspection"]);
                ensure!(inspection["actions"].as_array().is_some_and(|actions|actions.len()==1)
                    && inspection["actions_omitted"]==0,
                    "A usable contract route needs bounded configuration discovery without losing consumer files: {dispatch}");
                let search=&inspection["actions"][0];ensure!(search["tool"]=="grep_project");
                ensure!(search["arguments"]["path_prefix"]=="Site/App_Code/api-v2/"
                    && search["arguments"]["pattern"]=="IReader"
                    && search["arguments"]["regex"]==false);
                if let Some(fallback)=inspection.get("unrestricted_fallback") {
                    let omitted:std::collections::BTreeSet<_>=search["arguments"]["exclude_path_prefixes"].as_array().unwrap()
                        .iter().map(|path|path.as_str().unwrap()).collect();
                    ensure!(omitted==std::collections::BTreeSet::from([caller,
                        "Site/App_Code/api-v2/Services/common/UserRightsService.vb",
                        "Site/App_Code/api-v2/Services/common/interfaces/IUserRightsService.vb"]),
                        "The first search must skip only the listed route-file prefixes");
                    ensure!(fallback["actions"][0]["arguments"].get("exclude_path_prefixes").is_none(),
                        "The unrestricted fallback must still cover setup in declaration files");
                } else {ensure!(inspection["narrowed_actions_omitted"].as_u64().is_some_and(|count|count>0));}
                if let Some(discovery)=inspection.get("path_discovery") {
                    let action=&discovery["actions"][0];
                    ensure!(action["tool"]=="get_codebase_overview" && action["arguments"]["path_contains"]=="Config");
                    let page=app.handle_get_codebase_overview(serde_json::from_value(action["arguments"].clone())?).await?;
                    ensure!(page.is_error!=Some(true));
                    let page:Value=serde_json::from_str(page.content[0].as_text().unwrap().text.split("```json\n").nth(1).unwrap().split("```").next().unwrap())?;
                    ensure!(page["coverage"]["matching_files"]==0 && page["files"].as_array().unwrap().is_empty(),
                        "No Config-named file must remain an honest empty path lookup; the broader binding search stays usable");
                }
                let found=app.handle_grep_project(serde_json::from_value(search["arguments"].clone())?).await?;
                ensure!(found.is_error!=Some(true),"Advertised configuration search must execute unchanged");
                let found:Value=serde_json::from_str(&found.content[0].as_text().unwrap().text)?;
                ensure!(found["matches"].as_array().unwrap().iter().any(|hit|
                    hit["file_path"]=="Site/App_Code/api-v2/Bindings.vb" && hit["source"]=="index"
                        && hit["line_text"].as_str().is_some_and(|line|line.contains("CreateReader"))),
                    "The agent must discover the real binding factory without a guessed configuration filename: {found}");
                let binding=found["matches"].as_array().unwrap().iter().find(|hit|
                    hit["file_path"]=="Site/App_Code/api-v2/Bindings.vb" && hit["line_text"].as_str().is_some_and(|line|line.contains("CreateReader"))).unwrap();
                let recovery=&binding["current_source_recovery"];
                let current=app.handle_get_full_method_body(serde_json::from_value(recovery["arguments"].clone())?).await?;
                ensure!(current.is_error!=Some(true));let current:Value=serde_json::from_str(&current.content[0].as_text().unwrap().text)?;
                let raw=std::fs::read(repo.join("Site/App_Code/api-v2/Bindings.vb"))?;
                ensure!(current["source_code"].as_str().is_some_and(|source|source.contains("CreateReader"))
                    && current["source_file_hash"]==engram_core::ContentHash::compute(&raw).0,
                    "The discovered non-Config factory must have a working current-source/hash follow-up");
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
            let config_path="Site/App_Code/api-v2/WebApiConfig.vb";
            let configuration="Namespace Example\nFriend Module WebApiConfig\n Public Function Configure() As IReader\n Return New Reader()\n End Function\nEnd Module\nEnd Namespace\n";
            std::fs::write(repo.join(config_path),configuration)?;
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
            let full=app.handle_compute_blast_radius(serde_json::from_value(json!({"project_id":pid,"file_path":helper,"detail":"graph","max_depth":6,"include_guidance":false}))?).await?;
            ensure!(full.is_error!=Some(true));let report=packet(&full.content[0].as_text().unwrap().text)?;
            ensure!(report["selected_dependents_observed"]==3 && report["potential_interface_callers"]["routes_observed"]==1
                && report["potential_interface_callers"]["runtime_dispatch_proven"]==false,
                "Configuration filename discovery must not promote its contents into primary callers or runtime proof: {report}");
            let inspection=&report["potential_interface_callers"]["configuration_inspection"];
            ensure!(inspection["actions"][0]["tool"]=="grep_project","Keep the broader fallback for other configuration names");
            let discovery=&inspection["path_discovery"];ensure!(discovery["actions"].as_array().is_some_and(|actions|actions.len()==1));
            let action=&discovery["actions"][0];
            let page=app.handle_get_codebase_overview(serde_json::from_value(action["arguments"].clone())?).await?;
            ensure!(page.is_error!=Some(true));let text=&page.content[0].as_text().unwrap().text;
            ensure!(text.len()<8*1024,"A one-file configuration lookup must leave room to inspect its source");
            let page:Value=serde_json::from_str(text.split("```json\n").nth(1).unwrap().split("```").next().unwrap())?;
            ensure!(page["detail"]=="filtered_paths" && page["coverage"]["matching_files"]==1 && page["files"][0]["path"]==config_path);
            let file=&page["files"][0];
            let read=file["symbols"].as_array().unwrap().iter().flat_map(|symbol|symbol["actions"].as_array().unwrap())
                .chain(file["actions"].as_array().unwrap()).find(|action|action["tool"]=="get_full_method_body").unwrap();
            let body=app.handle_get_full_method_body(serde_json::from_value(read["arguments"].clone())?).await?;
            ensure!(body.is_error!=Some(true));let body:Value=serde_json::from_str(&body.content[0].as_text().unwrap().text)?;
            ensure!(body["source_code"].as_str().is_some_and(|source|source.contains("Return New Reader()"))
                && body["source_file_hash"]==engram_core::ContentHash::compute(configuration.as_bytes()).0,
                "Literal path and source follow-ups must recover current configuration bytes with an exact file hash");
            // Configuration can live beside an implementation. A fast search
            // outside the known prefixes is a first attempt, never absence.
            let service_path="Site/App_Code/api-v2/Services/common/UserRightsService.vb";
            let concrete=app.state.graph.query_nodes(&pid,Some("function"),None,Some(service_path),16)?
                .into_iter().find(|node|node.file_path.as_str()==service_path && node.start_line==4).unwrap();
            let colocated=SERVICE.replace("End Class"," Public Shared Function CreateCoLocated() As IReader\n Return New Reader()\n End Function\nEnd Class");
            std::fs::write(repo.join(service_path),&colocated)?;
            std::fs::remove_file(repo.join("Site/App_Code/api-v2/Bindings.vb"))?;
            std::fs::remove_file(repo.join(config_path))?;
            let updated=app.handle_update_project(serde_json::from_value(json!({"project_id":pid,"wait":true,"max_commits":1,"index_antipatterns":false}))?).await?;
            ensure!(updated.is_error!=Some(true));
            let args=json!({"project_id":pid,"symbol_fqn":concrete.node_id,"detail":"graph","max_depth":6,"include_guidance":false});
            let response=app.handle_compute_blast_radius(serde_json::from_value(args.clone())?).await?;
            ensure!(response.is_error!=Some(true));let facts=packet(&response.content[0].as_text().unwrap().text)?;
            ensure!(facts["selected_dependents_observed"]==0 && facts["potential_interface_callers"]["routes_observed"]==1
                && facts["potential_interface_callers"]["runtime_dispatch_proven"]==false);
            let inspection=&facts["potential_interface_callers"]["configuration_inspection"];
            ensure!(inspection.get("unrestricted_fallback").is_some(),"A simple route must offer bounded fast search plus unrestricted fallback");
            let first=app.handle_grep_project(serde_json::from_value(inspection["actions"][0]["arguments"].clone())?).await?;
            ensure!(first.is_error!=Some(true));let first:Value=serde_json::from_str(&first.content[0].as_text().unwrap().text)?;
            ensure!(first["matches"].as_array().unwrap().is_empty(),"Known-prefix exclusions must be applied before capped matching");
            let fallback=app.handle_grep_project(serde_json::from_value(inspection["unrestricted_fallback"]["actions"][0]["arguments"].clone())?).await?;
            ensure!(fallback.is_error!=Some(true));let fallback:Value=serde_json::from_str(&fallback.content[0].as_text().unwrap().text)?;
            let hit=fallback["matches"].as_array().unwrap().iter().find(|hit|hit["file_path"]==service_path
                && hit["line_text"].as_str().is_some_and(|line|line.contains("CreateCoLocated"))).unwrap();
            let read=&hit["current_source_recovery"];
            ensure!(read["tool"]=="get_full_method_body");
            let body=app.handle_get_full_method_body(serde_json::from_value(read["arguments"].clone())?).await?;
            ensure!(body.is_error!=Some(true));let body:Value=serde_json::from_str(&body.content[0].as_text().unwrap().text)?;
            ensure!(body["source_code"].as_str().is_some_and(|source|source.contains("CreateCoLocated"))
                && body["source_file_hash"]==engram_core::ContentHash::compute(colocated.as_bytes()).0,
                "Fallback must recover current co-located setup with a file hash");
            // Different callers of one contract must retain distinct listed
            // exclusions, rather than silently borrowing the first route's.
            let other_path="Site/App_Code/api-v2/Controllers/common/OtherController.vb";
            std::fs::write(repo.join(other_path),"Namespace Example\nPublic Class OtherController\n Public Function GetAll(service As IReader, value As Alpha) As Alpha\n Return service.Read(value)\n End Function\nEnd Class\nEnd Namespace\n")?;
            let updated=app.handle_update_project(serde_json::from_value(json!({"project_id":pid,"wait":true,"max_commits":1,"index_antipatterns":false}))?).await?;
            ensure!(updated.is_error!=Some(true));
            let response=app.handle_compute_blast_radius(serde_json::from_value(args)?).await?;
            ensure!(response.is_error!=Some(true));let facts=packet(&response.content[0].as_text().unwrap().text)?;
            ensure!(facts["selected_dependents_observed"]==0 && facts["potential_interface_callers"]["routes_observed"]==2
                && facts["potential_interface_callers"]["runtime_dispatch_proven"]==false);
            let searches=facts["potential_interface_callers"]["configuration_inspection"]["actions"].as_array().unwrap();
            ensure!(searches.len()==2,"Different route-file exclusions must not collapse under a shared contract/scope");
            let exclusions:std::collections::BTreeSet<_>=searches.iter().map(|action|action["arguments"]["exclude_path_prefixes"].as_array().unwrap()
                .iter().map(|path|path.as_str().unwrap().to_owned()).collect::<Vec<_>>()).collect();
            ensure!(exclusions.len()==2 && exclusions.iter().any(|paths|paths.contains(&caller.to_owned()))
                && exclusions.iter().any(|paths|paths.contains(&other_path.to_owned())));
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
            ("IdleContract.vb", "Namespace Example\nPublic Interface IUnused\n Function Idle() As Integer\nEnd Interface\nEnd Namespace\n"),
            ("IdleImplementation.vb", "Namespace Example\nPublic Class UnusedService\n Implements IUnused\n Public Function Idle() As Integer Implements IUnused.Idle\n Return 1\n End Function\nEnd Class\nEnd Namespace\n"),
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
            // A selected implementation with a corrupt physical fingerprint
            // cannot be offered as an admitted body. The direct interface call
            // and caller source must still be usable when this optional hop fails.
            let before=forward_codemap(&app,&pid,"First",false,&[]).await?;
            let original_service=app.state.graph.get_node(&pid,"file:Service.vb")?.unwrap();
            let mut invalid_service=original_service.clone();
            invalid_service.metadata.as_mut().unwrap()["file_hash"]=json!("invalid");
            app.state.graph.upsert_nodes(&pid,&[invalid_service])?;
            let invalid=forward_codemap(&app,&pid,"First",false,&[]).await?;
            ensure!(invalid["called_declarations"]["items"]==before["called_declarations"]["items"],
                "Unusable implementation evidence must preserve the admitted direct contract call");
            let dispatch=&invalid["called_declarations"]["potential_implementations"];
            ensure!(dispatch["routes"].as_array().is_some_and(Vec::is_empty)
                && dispatch["coverage"]["truncated"]==true,
                "A changed implementation fingerprint is uncertainty, not an admitted body or proof of no implementations: {dispatch}");
            let source=forward_codemap(&app,&pid,"First",true,&[]).await?;
            ensure!(source["current_source"]["source_code"].as_str().is_some_and(|body|body.contains("Return service.Read(value)")));
            app.state.graph.upsert_nodes(&pid,&[original_service])?;
            let contract=app.state.graph.query_nodes(&pid,Some("function"),None,Some("IdleContract.vb"),8)?
                .into_iter().find(|node|node.name.ends_with(".Idle")).unwrap();
            for detail in ["graph","summary"] {
                let response=app.handle_compute_blast_radius(serde_json::from_value(json!({"project_id":pid,"symbol_fqn":contract.node_id,"detail":detail,"max_depth":6,"include_guidance":false}))?).await?;
                ensure!(response.is_error!=Some(true));let text=&response.content[0].as_text().unwrap().text;
                let prefix=text.split("## Revision-admitted change impact").next().unwrap();
                ensure!(prefix.contains("Relationship: implements_interface") && prefix.contains("declaration/implementation dependency"),
                    "An implementation dependency must be visibly typed before an agent reads it as an invocation: {prefix}");
                let facts=packet(text)?;ensure!(facts["selected_dependents_observed"]==1);
            }
            // A selected caller with an invalid physical fingerprint is an
            // inspectable unknown, never a verified interface consumer.
            let original=app.state.graph.get_node(&pid,"file:Caller.vb")?.unwrap();
            let mut invalid=original.clone(); invalid.metadata.as_mut().unwrap()["file_hash"]=json!("invalid");
            app.state.graph.upsert_nodes(&pid,&[invalid])?;
            let report=impact(&app,&pid,"impact_analysis","summary",4).await?;
            let dispatch=&report["potential_interface_callers"];
            ensure!(dispatch["routes_observed"]==0 && dispatch["coverage"]["truncated"]==true);
            ensure!(dispatch["frontier"].as_array().unwrap().iter().all(|row|row.get("caller_to_contract_admission").is_none()),
                "Rejected/unknown locators must not claim admitted contract calls");
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
