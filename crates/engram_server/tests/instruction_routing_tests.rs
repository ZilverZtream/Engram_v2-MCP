//! Health/update expose a routing gap without declaring authored policy wrong.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{Engram, state::AppState, services::instruction_maintenance};
use serde_json::{Value, json};
use std::path::Path;
const GUIDE: &str = ".github/instructions/map.instructions.md";
const EMPTY: &str = "---\napplyTo: 'Site/maps/**'\n---\nPreserve authorized map access; future routes may be intentional.\n";

fn commit(root: &Path) -> Result<String> {
    let repo=git2::Repository::init(root)?;
    let mut index=repo.index()?;index.add_all(["*"],git2::IndexAddOption::DEFAULT,None)?;index.write()?;
    let tree=repo.find_tree(index.write_tree()?)?;
    let signature=git2::Signature::now("Fixture","fixture@example.invalid")?;
    Ok(repo.commit(Some("HEAD"),&signature,&signature,"Routing baseline",&tree,&[])?.to_string())
}
fn text(r: &rmcp::model::CallToolResult) -> String {r.content.iter().filter_map(|c|c.as_text().map(|t|t.text.clone())).collect::<Vec<_>>().join("\n")}
async fn observe(engram:&Engram,pid:&str,maintenance:&Value,update:bool) -> Result<Value> {
    let response=if update {
        engram.handle_update_project(serde_json::from_value(json!({"project_id":pid,"wait":true,"instruction_maintenance":maintenance}))?).await?
    } else {
        engram.handle_project_health_with_instructions(serde_json::from_value(json!({"project_id":pid,"instruction_maintenance":maintenance}))?).await?
    };
    for content in response.content {
        if let Some(t)=content.as_text() {
            if let Ok(v)=serde_json::from_str::<Value>(&t.text) {
                if v.get("instruction_reference_observation").is_some() {return Ok(v["instruction_reference_observation"].clone());}
            }
        }
    }
    anyhow::bail!("Missing public instruction observation")
}
fn run(update:bool) -> Result<()> {
    let temporary=tempfile::tempdir()?;
    let runtime=tokio::runtime::Runtime::new()?;
    let result:Result<()>=runtime.block_on(async {
        let root=temporary.path().join("repo");std::fs::create_dir_all(root.join("Site/map"))?;std::fs::create_dir_all(root.join(".github/instructions"))?;
        std::fs::write(root.join(GUIDE),EMPTY)?;
        std::fs::write(root.join("Site/map/layer.js"),"export function layer() { return 1; }\n")?;
        std::fs::write(root.join(".gitignore"),"Site/maps/ignored.js\n")?;
        let base=commit(&root)?;
        let (state,_)=AppState::new(Config{data_dir:temporary.path().join("data"),allowed_roots:vec![root.clone()],embedding_backend:"fts_only".into(),llm_backend:"none".into(),..Default::default()})?;
        let engram=Engram::new(state);
        let indexed=engram.handle_index_project(serde_json::from_value(json!({"directory":root,"project_name":"Routing coverage","project_type":"general","wait":true}))?).await?;
        let pid=text(&indexed).split("project_id: ").nth(1).unwrap().lines().next().unwrap().trim().to_string();
        let request=json!({"instruction_paths":[GUIDE],"evidence_paths":["Site/map/layer.js"],"base_commit":base});
        let first=observe(&engram,&pid,&request,update).await?;
        ensure!(first["status"]=="observed");
        let route=&first["routing_coverage"];ensure!(route["inventory_complete"]==true,"{route}");
        let row=&route["instructions"][0];ensure!(row["status"]=="no_matching_files" && row["matching_files"]==0);
        ensure!(row["detail"].as_str().unwrap().contains("does not mean the instruction is wrong"));
        ensure!(row["source_blake3"]==blake3::hash(EMPTY.as_bytes()).to_hex().to_string());
        ensure!(first["policy_correctness"]=="not_established_by_reference_observation");
        let packet_before=instruction_maintenance::capture(&root,&pid,&serde_json::from_value(request.clone())?)?;
        // A new, unindexed source falls under the existing explicit scope.
        std::fs::create_dir_all(root.join("Site/maps"))?;
        std::fs::write(root.join("Site/maps/New.vb"),"Public Class NewMap\nEnd Class\n")?;
        std::fs::write(root.join("Site/maps/ignored.js"),"// ignored source\n")?;
        let second=observe(&engram,&pid,&request,false).await?;
        let row=&second["routing_coverage"]["instructions"][0];
        ensure!(row["status"]=="matches_observed" && row["matching_files"]==1 && row["matching_files_is_exact"]==true,"{row}");
        ensure!(row["examples"]==json!(["Site/maps/New.vb"]));
        let packet_after=instruction_maintenance::capture(&root,&pid,&serde_json::from_value(request.clone())?)?;
        ensure!(packet_before==packet_after,"Routing-only inventory must not invalidate proposal packet identity");
        ensure!(std::fs::read_to_string(root.join(GUIDE))?==EMPTY);
        std::fs::write(root.join(GUIDE),"---\napplyTo: ['Site/map/*.js', 'Site/maps/*.vb']\n---\nKeep the authored exception.\n")?;
        let corrected=observe(&engram,&pid,&request,false).await?;
        ensure!(corrected["routing_coverage"]["instructions"][0]["matching_files"]==2);
        std::fs::write(root.join(GUIDE),"---\napplyTo: 7\n---\nUnresolved scope is not zero coverage.\n")?;
        let invalid=observe(&engram,&pid,&request,false).await?;
        let row=&invalid["routing_coverage"]["instructions"][0];ensure!(row["status"]=="invalid_scope" && row["matching_files"].is_null());
        std::fs::write(root.join(GUIDE),"Manual guidance with no explicit scope.\n")?;
        let manual=observe(&engram,&pid,&request,false).await?;
        ensure!(manual["routing_coverage"]["instructions"][0]["status"]=="not_explicitly_scoped");
        ensure!(manual["authored_files_written"]==0);
        drop(engram);Ok(())
    });
    drop(runtime);let cleanup=temporary.close();result?;cleanup?;Ok(())
}
#[test]
fn health_reports_zero_matching_scope_and_live_unindexed_matches_without_policy_edits() -> Result<()> {run(false)}
#[test]
fn completed_update_includes_routing_observation_without_changing_proposal_identity() -> Result<()> {run(true)}
