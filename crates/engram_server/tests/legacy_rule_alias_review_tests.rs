//! Actual reviewed ownership workflows; source evidence is never auto-accepted.
use anyhow::{ensure, Result};
use engram_core::{Config, ProjectRecord, Registry, registry::{RepoRule, RuleSourceReceipt}};
use engram_server::{AppState, Engram};
use serde_json::{json, Value};
use redb::ReadableTable;
use std::{path::Path, future::Future, pin::Pin};

const PID: &str = "alias-review";
const A: &str = "Never add AlphaProbe tokens to production code; preserve the documented read-only exception.";
const B: &str = "Always retain BetaProbe authorization checks before updating production records.";

fn source(swapped: bool, omit_a: bool) -> String {
    let a = json!({"rule":A,"path_scope":"Site/**"});
    let b = json!({"rule":B,"path_scope":"Api/**"});
    serde_json::to_string(&if omit_a {vec![b]} else if swapped {vec![b,a]} else {vec![a,b]}).unwrap()
}
fn receipt_key() -> String { format!("quality_source:{}",blake3::hash(b"policy.json").to_hex()) }
fn receipt(e: &Engram) -> Result<RuleSourceReceipt> {
    let key=receipt_key();
    let raw=e.state.registry.get_meta(PID,&key)?.unwrap();
    RuleSourceReceipt::decode(&key,raw.as_bytes())
}
fn fixture(parent: &Path) -> Result<(Engram,std::path::PathBuf)> {
    let root=parent.join("repo"); let data=parent.join("data");
    std::fs::create_dir_all(&root)?;
    std::fs::create_dir_all(data.join("registry"))?;
    std::fs::write(root.join("policy.json"),source(false,false))?;
    let database=data.join("registry/registry.redb");
    {
        let registry=Registry::open(&database)?;
        registry.put_project(&ProjectRecord {project_id:PID.into(),project_name:"Alias lifecycle".into(),
            project_type:"general".into(),directory:root.to_string_lossy().into_owned(),
            created_at_ms:1,updated_at_ms:1,reindex_required_since_ms:None})?;
        registry.capture_source_head(PID)?;
        for (id,text,manual) in [("legacy-a","Accepted alpha policy; retain its read-only exception.",false),
            ("legacy-b","Accepted beta policy; retain restricted scope.",false),("manual-control","Manual unrelated policy.",true)] {
            registry.put_repo_rule(PID,&RepoRule {rule_id:id.into(),rule_text:text.into(),
                file_pattern:"Site/narrow/**".into(),priority:19,updated_at_ms:42,
                introduced_at:Some("2025-09-12".into()),provenance:manual.then(||"manual".into())})?;
        }
    }
    // Owned legacy bytes include an opaque field; review/rebind must not rewrite it.
    {
        let db=redb::Database::create(&database)?; let tx=db.begin_write()?;
        let table:redb::TableDefinition<&str,&[u8]>=redb::TableDefinition::new("repo_rules");
        {
            let mut rows=tx.open_table(table)?;
            let key=format!("{PID}\0legacy-a");
            let raw=rows.get(key.as_str())?.unwrap().value().to_vec();
            let mut value:Value=serde_json::from_slice(&raw)?;
            value["opaque_legacy_number"]=json!("900719925474099312345");
            rows.insert(key.as_str(),serde_json::to_vec_pretty(&value)?.as_slice())?;
        }
        tx.commit()?;
    }
    let (state,_)=AppState::new(Config {data_dir:data,allowed_roots:vec![root.clone()],
        embedding_backend:"fts_only".into(),..Default::default()})?;
    Ok((Engram::new(state),root))
}
async fn call(e:&Engram,request:Value)->Result<Value> {
    let response=e.handle_ingest_quality_gates(serde_json::from_value(request)?).await?;
    Ok(serde_json::from_str(&response.content[0].as_text().unwrap().text)?)
}
async fn import(e:&Engram)->Result<()> {
    e.handle_ingest_quality_gates(serde_json::from_value(json!({
        "project_id":PID,"source_path":"policy.json","source_type":"rules"
    }))?).await?;
    Ok(())
}
async fn adopt(e:&Engram)->Result<()> {
    let snap=e.state.registry.snapshot_rule_ownership(PID)?;
    let parsed=engram_index::quality_gates::parse_quality_source_checked(&source(false,false),
        engram_index::quality_gates::QualitySource::CodingRulesMd,"policy.json").map_err(anyhow::Error::msg)?;
    let bindings=["legacy-a","legacy-b"].iter().enumerate().map(|(n,id)| json!({
        "rule_id":id,"expected_raw_blake3":blake3::hash(&snap.rules()[*id]).to_hex().to_string(),
        "source_rule_id":parsed[n].id,"decision":"adopt_unverified"
    })).collect::<Vec<_>>();
    let mut req=json!({"project_id":PID,"source_path":"policy.json","source_type":"rules","adoption":{
        "mode":"preview","expected_source_blake3":blake3::hash(source(false,false).as_bytes()).to_hex().to_string(),
        "bindings":bindings,"decision_ref":"caller-review:original-owned-fixture"}});
    let preview=call(e,req.clone()).await?;
    req["adoption"]["mode"]=json!("apply");
    req["adoption"]["expected_plan_blake3"]=preview["plan_blake3"].clone();
    call(e,req).await?;
    Ok(())
}
fn review(e:&Engram,body:Option<&str>,actions:&[(&str,Value)])->Result<Value> {
    let snap=e.state.registry.snapshot_rule_ownership(PID)?;
    let old=receipt(e)?;
    let bindings=actions.iter().map(|(id,action)| {
        let alias=old.legacy_aliases.iter().find(|a|a.rule_id==*id).unwrap();
        json!({"rule_id":id,"expected_raw_blake3":blake3::hash(&snap.rules()[*id]).to_hex().to_string(),
            "expected_alias_blake3":blake3::hash(&serde_json::to_vec(alias).unwrap()).to_hex().to_string(),
            "disposition":action})
    }).collect::<Vec<_>>();
    Ok(json!({"project_id":PID,"source_path":"policy.json","source_type":"rules","alias_review":{
        "mode":"preview","source":body.map(|body|json!({"state":"present","blake3":blake3::hash(body.as_bytes()).to_hex().to_string()}))
            .unwrap_or_else(||json!({"state":"missing"})),
        "bindings":bindings,"decision_ref":"caller-review:explicit-alias-fixture"}}))
}
async fn apply(e:&Engram,mut req:Value)->Result<Value> {
    let before=e.state.registry.snapshot_rule_ownership(PID)?;
    let preview=call(e,req.clone()).await?;
    ensure!(e.state.registry.snapshot_rule_ownership(PID)?==before,"Preview mutated ownership");
    req["alias_review"]["mode"]=json!("apply");
    req["alias_review"]["expected_plan_blake3"]=preview["review"]["plan_blake3"].clone();
    call(e,req).await
}
fn actions_for(swapped:bool)->Vec<(&'static str,Value)> {
    let parsed=engram_index::quality_gates::parse_quality_source_checked(&source(swapped,false),
        engram_index::quality_gates::QualitySource::CodingRulesMd,"policy.json").unwrap();
    vec![("legacy-a",json!({"action":"rebind_only_unchanged","source_rule_id":parsed[usize::from(swapped)].id})),
        ("legacy-b",json!({"action":"rebind_only_unchanged","source_rule_id":parsed[usize::from(!swapped)].id}))]
}
async fn old_search_is_advisory(e:&Engram)->Result<()> {
    let out=e.handle_pre_push_audit(serde_json::from_value(json!({
        "project_id":PID,"code":"AlphaProbe BetaProbe authorization checks","top_k":50
    }))?).await?;
    let text=&out.content[0].as_text().unwrap().text;
    ensure!(text.contains("INCOMPLETE policy coverage; advisory evidence only"),"{text}");
    ensure!(!text.contains("Evidence status: current:"),"{text}");
    Ok(())
}
fn owned_case(test:impl for<'a> FnOnce(&'a Path)->Pin<Box<dyn Future<Output=Result<()>>+'a>>)->Result<()> {
    let owned=tempfile::tempdir()?;
    let runtime=tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let result=runtime.block_on(test(owned.path()));
    drop(runtime);
    let cleanup=owned.close();
    match (result,cleanup) {
        (Err(error),Err(cleanup))=>Err(error.context(format!("Owned cleanup failed: {cleanup}"))),
        (Err(error),_)=>Err(error),(Ok(()),cleanup)=>cleanup.map_err(Into::into),
    }
}

#[test]
fn reviewed_alias_swap_reimports_and_retirement_preserve_raw_ids_manual_policy_and_evidence()->Result<()> {
    owned_case(|parent|Box::pin(async move {
        let (e,root)=fixture(parent)?; adopt(&e).await?; import(&e).await?;
        let adopted=e.state.registry.snapshot_rule_ownership(PID)?;
        let previous=receipt(&e)?;
        let reordered=source(true,false);
        std::fs::write(root.join("policy.json"),&reordered)?;
        ensure!(import(&e).await.is_err(),"Unreviewed positional rebinding succeeded");
        let effective=engram_server::services::quality_source::effective_repo_rules(&e.state,PID,None)?;
        ensure!(effective.rules.len()==1 && effective.rules[0].rule_id=="manual-control");
        let mut first=review(&e,Some(&reordered),&actions_for(true))?;
        first["alias_review"]["bindings"][0].as_object_mut().unwrap().remove("expected_alias_blake3");
        let applied=apply(&e,first).await?;
        ensure!(applied["status"]=="alias_review_applied");
        let pending=receipt(&e)?;
        ensure!(pending.pending && pending.source_hash==previous.source_hash
            && pending.doc_ids==previous.doc_ids && pending.corpus_revision==previous.corpus_revision);
        ensure!(e.state.registry.snapshot_rule_ownership(PID)?.rules()==adopted.rules(),
            "Rebinding rewrote legacy bytes/dates/scope/opaque fields");
        old_search_is_advisory(&e).await?;
        import(&e).await?;
        ensure!(!receipt(&e)?.pending && receipt(&e)?.source_hash==blake3::hash(reordered.as_bytes()).to_hex().to_string());
        ensure!(e.state.registry.snapshot_rule_ownership(PID)?.rules()==adopted.rules());
        let effective=engram_server::services::quality_source::effective_repo_rules(&e.state,PID,None)?;
        ensure!(effective.rules.len()==3,"Normal import did not restore verified enforcement");
        // A real source change after reviewed metadata commit must not strand
        // pending state: a fresh explicit review can resolve the next mapping.
        let original=source(false,false);
        std::fs::write(root.join("policy.json"),&original)?;
        apply(&e,review(&e,Some(&original),&actions_for(false))?).await?;
        std::fs::write(root.join("policy.json"),&reordered)?;
        ensure!(import(&e).await.is_err());
        ensure!(receipt(&e)?.pending);
        apply(&e,review(&e,Some(&reordered),&actions_for(true))?).await?;
        import(&e).await?;
        ensure!(!receipt(&e)?.pending && e.state.registry.snapshot_rule_ownership(PID)?.rules()==adopted.rules());
        let remaining=source(true,true);
        std::fs::write(root.join("policy.json"),&remaining)?;
        let retired=apply(&e,review(&e,Some(&remaining),&[("legacy-a",json!({"action":"retire"}))])?).await?;
        let audit=e.state.registry.get_meta(PID,retired["outcome"]["audit_key"].as_str().unwrap())?.unwrap();
        let audit:Value=serde_json::from_str(&audit)?;
        ensure!(audit["changes"][0]["before_raw_utf8"].as_str().unwrap().as_bytes()==adopted.rules()["legacy-a"].as_slice());
        ensure!(receipt(&e)?.pending && !e.state.registry.snapshot_rule_ownership(PID)?.rules().contains_key("legacy-a"));
        old_search_is_advisory(&e).await?;
        import(&e).await?;
        let rows=e.state.registry.snapshot_rule_ownership(PID)?;
        ensure!(!rows.rules().contains_key("legacy-a") && rows.rules()["legacy-b"]==adopted.rules()["legacy-b"]
            && rows.rules()["manual-control"]==adopted.rules()["manual-control"]);
        ensure!(!receipt(&e)?.pending && receipt(&e)?.legacy_aliases.len()==1);
        std::fs::remove_file(root.join("policy.json"))?;
        apply(&e,review(&e,None,&[("legacy-b",json!({"action":"retire"}))])?).await?;
        ensure!(receipt(&e)?.pending && receipt(&e)?.legacy_aliases.is_empty());
        let effective=engram_server::services::quality_source::effective_repo_rules(&e.state,PID,None)?;
        ensure!(effective.rules.len()==1 && effective.rules[0].rule_id=="manual-control");
        old_search_is_advisory(&e).await?;
        ensure!(import(&e).await.is_err(),"Missing source falsely completed publication");
        std::fs::write(root.join("policy.json"),"[]")?;
        import(&e).await?;
        ensure!(!receipt(&e)?.pending && receipt(&e)?.rules.is_empty());
        ensure!(e.state.registry.list_repo_rules(PID)?.len()==1);
        Ok(())
    }))
}

#[test]
fn alias_review_refuses_stale_source_competing_manual_or_receipt_changes_and_unselected_collisions()->Result<()> {
    owned_case(|parent|Box::pin(async move {
        for conflict in ["source","manual","receipt","collision","policy"] {
            let (e,root)=fixture(&parent.join(conflict))?; adopt(&e).await?;
            let reordered=source(true,false);
            std::fs::write(root.join("policy.json"),&reordered)?;
            let actions=actions_for(true);
            let mut req=review(&e,Some(&reordered),&actions)?;
            if conflict=="collision" { req["alias_review"]["bindings"].as_array_mut().unwrap().truncate(1); }
            if conflict=="policy" {
                let changed=reordered.replace("AlphaProbe","ChangedPolicyProbe");
                std::fs::write(root.join("policy.json"),&changed)?;
                req=review(&e,Some(&changed),&actions)?;
            }
            if matches!(conflict,"collision"|"policy") {
                let before=e.state.registry.snapshot_rule_ownership(PID)?;
                ensure!(call(&e,req).await.is_err());
                ensure!(e.state.registry.snapshot_rule_ownership(PID)?==before);
            } else {
                let preview=call(&e,req.clone()).await?;
                req["alias_review"]["mode"]=json!("apply");
                req["alias_review"]["expected_plan_blake3"]=preview["review"]["plan_blake3"].clone();
                match conflict {
                    "source"=>std::fs::write(root.join("policy.json"),format!("{reordered}\n"))?,
                    "manual"=>{
                        let mut manual=e.state.registry.list_repo_rules(PID)?.into_iter()
                            .find(|r|r.rule_id=="manual-control").unwrap();
                        manual.rule_text.push_str(" Competing human amendment.");
                        e.state.registry.put_repo_rule(PID,&manual)?;
                    }
                    "receipt"=>{
                        let mut old=receipt(&e)?;
                        old.cleanup.push("competing-source-cleanup".into());
                        e.state.registry.set_meta(PID,&receipt_key(),&old.encode()?)?;
                    }
                    _=>unreachable!(),
                }
                let before=e.state.registry.snapshot_rule_ownership(PID)?;
                ensure!(call(&e,req).await.is_err(),"Stale lifecycle approval was reused");
                ensure!(e.state.registry.snapshot_rule_ownership(PID)?==before);
            }
            ensure!(e.state.projects.is_empty(),"Review initialized a search runtime");
        }
        Ok(())
    }))
}
