//! Batch recovery preserves executed evidence instead of replaying mutable reads.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{AppState, Engram};
use serde_json::{Value, json};
use std::path::PathBuf;

const SOURCE: &str = "export const invoicePreview = true;\n";
const GUIDANCE: &str = "Preserve the approved manual replay exception for invoice previews.\n";
struct Fixture {
    app: Engram,
    pid: String,
    root: PathBuf,
    owned: tempfile::TempDir,
}
impl Fixture {
    async fn new() -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new()
            .prefix("batch-recovery-")
            .tempdir_in(parent)?;
        let root = owned.path().join("project");
        std::fs::create_dir_all(&root)?;
        std::fs::write(root.join("entry.ts"), SOURCE)?;
        std::fs::write(root.join("AGENTS.md"), GUIDANCE)?;
        let (state, _) = AppState::new(Config {
            data_dir: owned.path().join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            ..Default::default()
        })?;
        let app = Engram::new(state);
        let indexed=app.handle_index_project(serde_json::from_value(json!({"directory":root,"project_name":"Owned batch recovery","project_type":"general","wait":true}))?).await?;
        let pid = indexed.content[0]
            .as_text()
            .unwrap()
            .text
            .split("project_id: ")
            .nth(1)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .trim()
            .to_owned();
        Ok(Self {
            app,
            pid,
            root,
            owned,
        })
    }
    async fn memory(&self, content: &str) -> Result<()> {
        let reply=self.app.handle_update_memory_bank(serde_json::from_value(json!({"project_id":self.pid,"section":"batch-original","content":content,"kind":"reference"}))?).await?;
        ensure!(reply.is_error != Some(true));
        Ok(())
    }
    async fn batch(&self, request: Value) -> Result<String> {
        let reply = self
            .app
            .handle_batch_read(serde_json::from_value(request)?)
            .await?;
        ensure!(reply.is_error != Some(true));
        Ok(reply.content[0].as_text().unwrap().text.clone())
    }
    fn close(self) -> Result<()> {
        let Self { app, owned, .. } = self;
        drop(app);
        owned.close()?;
        Ok(())
    }
}

#[tokio::test]
async fn mixed_oversized_batch_retains_exact_original_memory_source_qualifications_and_errors()
-> Result<()> {
    let fixture = Fixture::new().await?;
    let result=async {
        let original="Invoice evidence: keep the approved manual replay exception. ?.\n".repeat(800);
        fixture.memory(&original).await?;
        let freshness_args = json!({"project_id":fixture.pid,"source_files":["entry.ts"]});
        let request=json!({"max_tokens":4096,"requests":[
            {"request_id":"memory","request":{"tool":"read_memory_bank","arguments":{"project_id":fixture.pid,"section":"batch-original"}}},
            {"request_id":"source","request":{"tool":"get_full_method_body","arguments":{"project_id":fixture.pid,"file_path":"entry.ts","line_start":1,"line_end":1,"context_lines":0}}},
            {"request_id":"invalid","request":{"tool":"get_chunk","arguments":{"project_id":fixture.pid,"doc_id":"missing","max_tokens":0}}},
            {"request_id":"freshness","request":{"tool":"get_index_freshness","arguments":freshness_args}},
            {"request_id":"bad-freshness","request":{"tool":"get_index_freshness","arguments":{"project_id":fixture.pid,"source_files":["entry.ts"],"check_disk":false}}}
        ]});
        let raw=fixture.batch(request).await?; ensure!(raw.len()<=16384);
        let bounded:Value=serde_json::from_str(&raw)?;
        ensure!(bounded["complete"]==false && bounded["results"].as_array().unwrap().len()==5);
        ensure!(bounded["results"][0]["status"]=="omitted_shared_response_budget");
        ensure!(bounded["results"][2]["status"]=="request_error");
        ensure!(bounded["results"][0]["recovery"]["tool"]=="read_memory_bank");
        let delivery=&bounded["response_delivery"];
        ensure!(delivery["status"]=="DEFERRED_BATCH_RESULTS");
        fixture.memory("Replacement evidence after the batch.\n").await?;
        std::fs::write(fixture.root.join("entry.ts"),"export const replacement = false;\n")?;
        std::fs::write(fixture.root.join("AGENTS.md"),"Changed current guidance.\n")?;
        let mut restored=String::new(); let mut next=delivery["next_call"].clone();
        loop {
            ensure!(next["tool"]=="batch_read");
            ensure!(next["arguments"].get("requests").is_none());
            let raw=fixture.batch(next["arguments"].clone()).await?; ensure!(raw.len()<=16384);
            let page:Value=serde_json::from_str(&raw)?;
            ensure!(page["start_byte"].as_u64()==Some(restored.len() as u64));
            restored.push_str(page["content"].as_str().unwrap());
            if page["complete"]==true {break;} next=page["next_call"].clone();
        }
        ensure!(blake3::hash(restored.as_bytes()).to_hex().as_str()==delivery["response_id"].as_str().unwrap());
        ensure!(restored.len() as u64==delivery["original_bytes"].as_u64().unwrap());
        let full:Value=serde_json::from_str(&restored)?;
        ensure!(full["complete"]==true && full["omitted_count"]==0);
        ensure!(full.get("response_delivery").is_none());
        ensure!(full["results"].as_array().unwrap().iter().map(|r|r["request_id"].as_str().unwrap()).collect::<Vec<_>>()==["memory","source","invalid","freshness","bad-freshness"]);
        let memory=full["results"][0]["response"]["content"][0]["text"].as_str().unwrap();
        ensure!(memory.ends_with(&original));
        let source=full["results"][1]["response"]["content"][0]["text"].as_str().unwrap();
        ensure!(full["results"][1]["status"]=="ok");
        ensure!(source.contains(SOURCE.trim()) && source.contains(GUIDANCE.trim()));
        ensure!(!source.contains("Changed current guidance") && !memory.contains("Replacement evidence"));
        ensure!(full["results"][2]["response"]==bounded["results"][2]["response"]);
        let freshness = &full["results"][3];
        ensure!(freshness["status"] == "ok");
        ensure!(freshness["recovery"]["tool"] == "get_index_freshness");
        let captured: Value = serde_json::from_str(
            freshness["response"]["content"][0]["text"].as_str().unwrap()
        )?;
        let stored_row = &captured["files"][0];
        ensure!(stored_row["path"] == "entry.ts" && stored_row["status"] == "matching_fingerprint");
        ensure!(stored_row["disk_blake3"] == blake3::hash(SOURCE.as_bytes()).to_hex().as_str());
        ensure!(full["results"][4]["status"] == "request_error");
        let live = fixture.app.handle_get_index_freshness(serde_json::from_value(
            freshness["recovery"]["arguments"].clone(),
        )?).await?;
        let live: Value = serde_json::from_str(&live.content[0].as_text().unwrap().text)?;
        ensure!(live["files"][0]["status"] == "disk_differs");
        ensure!(live["files"][0]["physical_revision"] == stored_row["physical_revision"]);
        ensure!(full["consistency"].as_str().unwrap().contains("no atomic batch snapshot"));
        Ok(())
    }.await;
    fixture.close()?;
    result
}

#[tokio::test]
async fn fitting_batch_keeps_ordinary_results_without_allocating_a_saved_response() -> Result<()> {
    let fixture = Fixture::new().await?;
    let result=async {
        fixture.memory("Small complete source reference.\n").await?;
        let raw=fixture.batch(json!({"requests":[{"request_id":"small","request":{"tool":"read_memory_bank","arguments":{"project_id":fixture.pid,"section":"batch-original"}}}]})).await?;
        let reply:Value=serde_json::from_str(&raw)?;
        ensure!(reply["complete"]==true && reply["results"][0]["status"]=="ok");
        ensure!(reply.get("response_delivery").is_none());
        ensure!(reply["results"][0]["response"]["content"][0]["text"].as_str().unwrap().contains("Small complete source reference."));
        ensure!(!fixture.owned.path().join("data/responses/batch-read").exists());
        Ok(())
    }.await;
    fixture.close()?;
    result
}
