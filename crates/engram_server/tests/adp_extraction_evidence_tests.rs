use anyhow::{Result, ensure};
use engram_server::{
    AppState, Engram,
    services::autonomous_decision_service::{AdpDecision, ConfigSnapshot, build_decision_report},
};
use serde_json::{Value, json};

fn text(result: rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|row| row.as_text().map(|row| row.text.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}
async fn decision(e: &Engram, pid: &str, files: Value, score: Value) -> Result<Value> {
    Ok(serde_json::from_str(&text(e.handle_autonomous_decision_gate(serde_json::from_value(json!({
        "project_id":pid,"proposed_change":"Review the concrete call change","target_files":files,
        "extraction_confidence":score,"evidence_depth":"standard","output_json":true
    }))?).await?))?)
}
fn gate(value: &Value) -> &Value {
    value["gate_results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|gate| gate["gate_id"] == "extraction_confidence")
        .unwrap()
}
fn config() -> ConfigSnapshot {
    serde_json::from_value(json!({"adp_min_extraction_confidence":0.7,"safety_min_confidence":0.7,
        "safety_min_coverage":0.8,"adp_max_blast_radius":3,"safety_policy_enabled":true,
        "gate_code_version":"test","evidence_schema_version":"1","evidence_hash":"","crate_version":"test"})).unwrap()
}

#[test]
fn adp_uses_observed_edge_quality_unknown_coverage_and_replayable_witnesses() -> Result<()> {
    let owned = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let result=runtime.block_on(async {
        let root=owned.path().join("repo");std::fs::create_dir_all(root.join("src"))?;
        drop(git2::Repository::init(&root)?);
        let body="pub fn first() {}\npub fn second() {}\npub fn caller() { first(); second(); }\n";
        std::fs::write(root.join("src/resolved.rs"),body)?;
        let unknown="pub fn unresolved() { missing_dependency(); }\n";
        std::fs::write(root.join("src/unknown.rs"),unknown)?;
        let (state,_)=AppState::new(engram_core::Config {data_dir:owned.path().join("data"),allowed_roots:vec![root.clone()],
            embedding_backend:if cfg!(feature="vector"){"local"}else{"fts_only"}.into(),llm_backend:"none".into(),
            adp_rollout_phase:"autonomous".into(),adp_min_extraction_confidence:0.7,..Default::default()})?;
        let e=Engram::new(state);
        let indexed=text(e.handle_index_project(serde_json::from_value(json!({"directory":root,"project_name":"Extraction evidence","project_type":"rust","wait":true}))?).await?);
        let pid=indexed.split("project_id: ").nth(1).unwrap().lines().next().unwrap().trim();
        let generation=e.state.registry.active_source_generation(pid)?;
        let files=json!(["src/resolved.rs"]);
        let resolved=decision(&e,pid,files.clone(),Value::Null).await?;
        let g=gate(&resolved);
        ensure!(g["passed"]==true && g["extraction_evidence"]["complete"]==true,"Known local calls must supply extraction evidence: {g}");
        ensure!(g["extraction_evidence"]["scored_edges"].as_u64().unwrap()>=2);
        ensure!(g["extraction_evidence"]["witnesses"].as_array().unwrap().iter().any(|w| w["kind"]=="calls"));
        let nodes=e.state.graph.query_nodes_in_file(pid,None,"src/resolved.rs",100)?;
        let caller=nodes.iter().find(|n|n.name=="caller" || n.name.ends_with(".caller") || n.name.ends_with("::caller")).unwrap();
        let (mut edges,truncated)=e.state.graph.outgoing_edges_of_kind(pid,engram_graph::EdgeKind::Calls,&caller.node_id,10)?;
        ensure!(!truncated && edges.len()==2);
        let original=edges[0].clone();
        edges[0].metadata.as_mut().unwrap()["confidence"]=json!(0.2);
        edges[0].metadata.as_mut().unwrap()["resolution"]=json!("controlled_weak_resolution");
        e.state.graph.upsert_edges(pid,&[edges[0].clone()])?;
        let weak=decision(&e,pid,files.clone(),json!(0.99)).await?;
        ensure!(gate(&weak)["confidence"]==0.2 && gate(&weak)["passed"]==false && weak["verdict"]=="Deny","Weak edge hidden by override: {weak}");
        let parsed:AdpDecision=serde_json::from_value(weak.clone())?;
        let first=build_decision_report(&parsed,pid,"change",&["src/resolved.rs".into()],"medium",json!({}),config(),"test");
        ensure!(first.gate_evidence[0].extraction_evidence.as_ref().unwrap().witnesses.iter().any(|w|w.confidence==Some(0.2)));
        let mut altered=parsed.clone();
        altered.gate_results[0].extraction_evidence.as_mut().unwrap().witnesses[0].resolution=Some("altered evidence".into());
        let second=build_decision_report(&altered,pid,"change",&["src/resolved.rs".into()],"medium",json!({}),config(),"test");
        ensure!(first.config_snapshot.evidence_hash!=second.config_snapshot.evidence_hash);
        let markdown=text(e.handle_autonomous_decision_gate(serde_json::from_value(json!({"project_id":pid,"proposed_change":"change","target_files":files,"extraction_confidence":0.99}))?).await?);
        ensure!(markdown.contains("controlled_weak_resolution") && markdown.contains("extraction_evidence:"));
        edges[0].metadata.as_mut().unwrap().as_object_mut().unwrap().remove("confidence");
        e.state.graph.upsert_edges(pid,&[edges[0].clone()])?;
        let missing=decision(&e,pid,files.clone(),json!(0.99)).await?;
        ensure!(gate(&missing)["skipped"]==true && missing["verdict"]!="Allow" && missing["failed_gates"].as_array().unwrap().contains(&json!("extraction_confidence")),"Unknown evidence did not force abstention: {missing}");
        let low=decision(&e,pid,files.clone(),json!(0.1)).await?;
        ensure!(low["verdict"]=="Deny" && gate(&low)["confidence"]==0.1);
        for invalid in [json!(1.2), json!("NaN"), json!("not-a-score")] {
            edges[0].metadata.as_mut().unwrap()["confidence"]=invalid;
            e.state.graph.upsert_edges(pid,&[edges[0].clone()])?;
            let invalid=decision(&e,pid,files.clone(),json!(0.99)).await?;
            ensure!(gate(&invalid)["skipped"]==true && invalid["verdict"]!="Allow");
        }
        edges[0]=original.clone();edges[0].generation=generation+1;e.state.graph.upsert_edges(pid,&[edges[0].clone()])?;
        let stale=decision(&e,pid,files.clone(),json!(0.99)).await?;
        ensure!(gate(&stale)["passed"]==false && gate(&stale)["extraction_evidence"]["complete"]==false);
        e.state.graph.upsert_edges(pid,&[original.clone()])?;
        let unresolved=decision(&e,pid,json!(["src/unknown.rs"]),json!(0.99)).await?;
        ensure!(gate(&unresolved)["passed"]==false && unresolved["verdict"]!="Allow");
        let absent=decision(&e,pid,json!(["absent.rs"]),json!(0.99)).await?;
        ensure!(gate(&absent)["passed"]==false && gate(&absent)["extraction_evidence"]["files_checked"]==0);
        let legacy=decision(&e,pid,json!([]),json!(0.99)).await?;
        ensure!(gate(&legacy)["passed"]==true && gate(&legacy).get("extraction_evidence").is_none());
        for score in [-0.01,1.01] {
            let error=e.handle_autonomous_decision_gate(serde_json::from_value(json!({"project_id":pid,"proposed_change":"change","extraction_confidence":score}))?).await.unwrap_err();
            ensure!(error.message.contains("0..=1"));
        }
        let many=(0..33).map(|i|format!("absent{i}.rs")).collect::<Vec<_>>();
        let capped=decision(&e,pid,json!(many),json!(0.99)).await?;
        ensure!(gate(&capped)["extraction_evidence"]["gaps"].to_string().contains("target_file_limit"));
        // A dense graph must report incomplete coverage, even when all inspected
        // producer scores are strong and the node count fits its separate cap.
        let dense_nodes=(0..65).map(|i| {
            let mut node=caller.clone();node.node_id=format!("dense:{i}");node.name=format!("dense{i}");node
        }).collect::<Vec<_>>();
        let mut dense_edges=Vec::new();
        for from in &dense_nodes { for to in &dense_nodes {
            if from.node_id==to.node_id {continue;}
            let mut edge=original.clone();edge.source_id=from.node_id.clone();edge.target_id=to.node_id.clone();dense_edges.push(edge);
        }}
        e.state.graph.upsert_nodes(pid,&dense_nodes)?;
        e.state.graph.upsert_edges(pid,&dense_edges)?;
        let capped=decision(&e,pid,files.clone(),json!(0.99)).await?;
        ensure!(gate(&capped)["extraction_evidence"]["edges_checked"]==4096 &&
            gate(&capped)["extraction_evidence"]["gaps"].to_string().contains("edge_limit") &&
            gate(&capped)["passed"]==false && capped["verdict"]!="Allow");
        let mut extra=Vec::new();
        for i in 0..513 {let mut n=caller.clone();n.node_id=format!("extra:{i}");n.name=format!("extra{i}");extra.push(n);}
        e.state.graph.upsert_nodes(pid,&extra)?;
        let capped=decision(&e,pid,files.clone(),json!(0.99)).await?;
        ensure!(gate(&capped)["extraction_evidence"]["gaps"].to_string().contains("node_limit") && gate(&capped)["passed"]==false);
        e.state.adp_kill_switch.store(true,std::sync::atomic::Ordering::Release);
        let killed=decision(&e,pid,files,Value::Null).await?;
        ensure!(killed["verdict"]=="Deny" && killed["failed_gates"].as_array().unwrap().contains(&json!("kill_switch")));
        ensure!(e.state.registry.active_source_generation(pid)?==generation && std::fs::read_to_string(root.join("src/resolved.rs"))?==body);
        Ok::<(),anyhow::Error>(())
    });
    drop(runtime);
    let cleanup = owned.close();
    match (result, cleanup) {
        (Err(error), Err(cleanup)) => Err(error.context(format!("ADP fixture cleanup: {cleanup}"))),
        (Err(error), _) => Err(error),
        (Ok(()), cleanup) => cleanup.map_err(Into::into),
    }
}
