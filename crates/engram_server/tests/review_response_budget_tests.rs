//! Large reviews retain every selected finding and exact original evidence.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{Engram, state::AppState};
use serde_json::{Value, json};

const POLICY: &str = "# Review policy\nException: demonstration values are allowed in fixtures only; production guards remain required.\n";
fn text(r: &rmcp::model::CallToolResult) -> String { r.content[0].as_text().unwrap().text.clone() }

fn run(json_output: bool) -> Result<()> {
    // Runtime drops before directory even if the assertion future unwinds.
    let temporary = tempfile::tempdir()?;
    let runtime = tokio::runtime::Runtime::new()?;
    let result = runtime.block_on(async {
        let root = temporary.path().join("repo");
        std::fs::create_dir_all(root.join("Site/scripts"))?;
        let example = concat!("AKIA", "IOSFODNN7EXAMPLE"); // Published example, never a credential.
        let mut diff = String::new();
        for i in 0..40 {
            let path = format!("Site/scripts/config{i}.js");
            std::fs::write(root.join(&path),format!("const config{i} = {{}};\nconst key = \"{example}\";\n"))?;
            diff.push_str(&format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1 +1,2 @@\n const config{i} = {{}};\n+const key = \"{example}\";\n"));
        }
        std::fs::write(root.join("AGENTS.md"),POLICY)?;
        let (state, _) = AppState::new(Config { data_dir:temporary.path().join("data"),
            allowed_roots:vec![root.clone()],embedding_backend:"fts_only".into(),llm_backend:"none".into(),..Default::default() })?;
        let engram = Engram::new(state);
        let indexed = engram.handle_index_project(serde_json::from_value(json!({"directory":root,"project_name":"Review delivery","project_type":"general","wait":true}))?).await?;
        let pid = text(&indexed).split("project_id: ").nth(1).unwrap().lines().next().unwrap().trim().to_owned();
        engram.handle_record_review_decisions(serde_json::from_value(json!({"project_id":pid,"review_id":"fixture-review","decisions":[{
            "event_id":"accepted-fixture-only","finding_id":"fixture-example","kind":"accepted_exception","supersedes":null,
            "source_url":"https://example.invalid/review","author":"Fixture Owner","recorded_at":"2026-01-01T12:00:00Z",
            "rationale":"Site/scripts/config0.js deliberately uses a demonstration value only; preserve production guards.","verification":null
        }]}))?).await?;
        let skip:Vec<_> = engram_server::services::pre_commit_review_service::all_gates().into_iter()
            .filter(|g|g.name()!="secret_leakage").map(|g|g.name().to_string()).collect();
        let request = json!({"project_id":pid,"diff":diff,"skip_gates":skip,"max_findings":100,
            "detail_level":"full","output_json":json_output,"max_tokens":8192});
        let first = text(&engram.handle_pre_commit_review(serde_json::from_value(request)?).await?);
        ensure!(first.len()<=8192*4);
        let manifest:Value = if json_output {serde_json::from_str(&first)?}
            else {serde_json::from_str(first.split("```json\n").nth(1).unwrap().split("\n```").next().unwrap())?};
        ensure!(manifest["verdict"]=="red" && manifest["summary"]["total_findings"]==40);
        ensure!(manifest["finding_ledger"].as_array().unwrap().len()==40);
        ensure!(manifest["coverage"]["compilation"]=="not_run" && manifest["coverage"]["test_execution"]=="not_run");
        for f in manifest["finding_ledger"].as_array().unwrap() {
            ensure!(f["severity"]=="critical" && f["gate"]=="secret_leakage");
        }
        let id=manifest["response_delivery"]["response_id"].as_str().unwrap();
        let original=std::fs::read(engram.state.cfg.data_dir.join(format!("responses/review/{pid}/{id}.txt")))?;
        ensure!(original.len()>8192*4);
        std::fs::write(root.join("AGENTS.md"),"Changed after review; not the original evidence.")?;
        std::fs::write(root.join("Site/scripts/config0.js"),"// Value removed after review.\n")?;
        let mut next=manifest["response_delivery"]["next_call"].clone();
        let mut recovered=String::new();
        while !next.is_null() {
            ensure!(next["tool"]=="pre_commit_review");
            // An invalid fresh-review selector proves recovery does not rerun gates/diff resolution.
            next["arguments"]["diff"]=json!("missing-review-source.patch");
            let page=text(&engram.handle_pre_commit_review(serde_json::from_value(next["arguments"].clone())?).await?);
            ensure!(page.len()<=8192*4);
            let page:Value=serde_json::from_str(&page)?;
            ensure!(page["start_byte"].as_u64().unwrap() as usize==recovered.len());
            recovered.push_str(page["content"].as_str().unwrap());next=page["next_call"].clone();
        }
        ensure!(recovered.as_bytes()==original);
        ensure!(blake3::hash(recovered.as_bytes()).to_hex().as_str()==id);
        ensure!(recovered.contains(POLICY.trim_end()) || recovered.contains(&serde_json::to_string(POLICY)?[1..serde_json::to_string(POLICY)?.len()-1]));
        ensure!(recovered.contains("accepted-fixture-only") && recovered.contains("preserve production guards"));
        ensure!(!recovered.contains("Changed after review"));
        if json_output {
            let full:Value=serde_json::from_str(&recovered)?;
            for row in manifest["finding_ledger"].as_array().unwrap() {
                let f=&full["findings"][row["finding_index"].as_u64().unwrap() as usize];
                ensure!(row["file_path"]==f["file_path"] && row["title"]==f["title"] && row["severity"]==f["severity"]);
            }
        }
        let empty=text(&engram.handle_pre_commit_review(serde_json::from_value(json!({"project_id":pid,"diff":"","output_json":true}))?).await?);
        let empty:Value=serde_json::from_str(&empty)?;ensure!(empty["verdict"]=="NO_CHANGES" && empty.get("response_delivery").is_none());
        let tiny=engram.handle_pre_commit_review(serde_json::from_value(json!({"project_id":pid,"diff":"","max_tokens":1}))?).await;
        ensure!(tiny.unwrap_err().message.contains("at least"));
        drop(engram);
        Ok(())
    });
    drop(runtime);
    let cleanup=temporary.close();
    result?;cleanup?;Ok(())
}

#[test]
fn json_review_budget_preserves_findings_decisions_and_exact_original_recovery() -> Result<()> {run(true)}
#[test]
fn markdown_review_budget_preserves_findings_decisions_and_exact_original_recovery() -> Result<()> {run(false)}
