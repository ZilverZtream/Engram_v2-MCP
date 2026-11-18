//! Memory decisions retain provenance and full conditions without invented approval.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{AppState, Engram};
use serde_json::{Value, json};
use std::path::PathBuf;

struct Fixture {
    app: Engram,
    pid: String,
    original: String,
    owned: tempfile::TempDir,
}
impl Fixture {
    async fn new() -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new()
            .prefix("memory-")
            .tempdir_in(parent)?;
        let root = owned.path().join("p");
        std::fs::create_dir_all(root.join("Site"))?;
        std::fs::write(
            root.join("Site/invoice.ts"),
            "export function AuditPreviewRuleProbe() { return true; }\n",
        )?;
        let (state, _) = AppState::new(Config {
            data_dir: owned.path().join("d"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            ..Default::default()
        })?;
        let app = Engram::new(state);
        let indexed=app.handle_index_project(serde_json::from_value(json!({"directory":root,"project_name":"Owned memory provenance","project_type":"general","wait":true}))?).await?;
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
        let original = format!(
            "AuditPreviewRuleProbe is a proposal, not an accepted requirement.\n{}Exception: manual replay remains allowed.\n",
            "Background context. ?.\n".repeat(1000)
        );
        let fixture = Self {
            app,
            pid,
            original,
            owned,
        };
        fixture
            .write("first-agent", vec!["Site/invoice.ts"])
            .await?;
        fixture.app.handle_update_memory_bank(serde_json::from_value(json!({"project_id":fixture.pid,"section_id":"reference-record-2","section":"Unreviewed reference","kind":"reference","author":"source-collector","content":"UnreviewedReferenceProbe is unverified material, not approval.\n"}))?).await?;
        Ok(fixture)
    }
    async fn write(&self, author: &str, related: Vec<&str>) -> Result<()> {
        let reply=self.app.handle_update_memory_bank(serde_json::from_value(json!({"project_id":self.pid,"section_id":"proposal-record-1","section":"Preview proposal","kind":"decision","author":author,"related_files":related,"tags":["proposal"],"content":self.original}))?).await?;
        ensure!(reply.is_error != Some(true));
        Ok(())
    }
    async fn ask(&self, format: &str) -> Result<String> {
        let reply=self.app.handle_ask_codebase(serde_json::from_value(json!({"project_id":self.pid,"question":"Explain AuditPreviewRuleProbe and UnreviewedReferenceProbe","output_format":format,"depth":"deep","deadline_ms":1000,"max_tokens":65536}))?).await?;
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
fn memories(report: &Value) -> Vec<&Value> {
    report["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["provider"] == "memory")
        .collect()
}
fn proposal(report: &Value) -> &Value {
    memories(report)
        .into_iter()
        .find(|row| row["memory_record"]["section_id"] == "proposal-record-1")
        .unwrap()
}

#[tokio::test]
async fn decision_and_reference_records_keep_declared_provenance_and_full_guarded_recovery_without_approval()
-> Result<()> {
    let f = Fixture::new().await?;
    let result = async {
        let report: Value = serde_json::from_str(&f.ask("json").await?)?;
        let rows = memories(&report);
        ensure!(rows.len() == 2, "{report}");
        for row in rows {
            ensure!(row["authority"] == "agent_memory");
            ensure!(row["warnings"].to_string().contains("not approval"));
            ensure!(row["memory_record"]["project_id"] == f.pid);
            ensure!(row["memory_record"]["read"]["tool"] == "read_memory_bank");
        }
        let record = &proposal(&report)["memory_record"];
        ensure!(
            record["recorded_kind"] == "decision" && record["recorded_author"] == "first-agent"
        );
        ensure!(record["declared_related_files"] == json!(["Site/invoice.ts"]));
        ensure!(
            record["captured_content_bytes"].as_u64().unwrap()
                < record["content_bytes"].as_u64().unwrap()
        );
        let mut args = record["read"]["arguments"].clone();
        let mut restored = String::new();
        let mut pages = 0;
        loop {
            let reply = f
                .app
                .handle_read_memory_bank(serde_json::from_value(args.clone())?)
                .await?;
            let raw = &reply.content[0].as_text().unwrap().text;
            ensure!(raw.len() <= 8192);
            let page: Value = serde_json::from_str(raw)?;
            ensure!(page["identity"]["expected_record_hash"] == record["record_hash"]);
            ensure!(page["returned_range"]["byte_start"].as_u64() == Some(restored.len() as u64));
            restored.push_str(page["content"].as_str().unwrap());
            pages += 1;
            if page["continuation"].is_null() {
                break;
            }
            args["citation"] = page["continuation"].clone();
        }
        ensure!(pages > 1 && restored == f.original);
        let markdown = f.ask("markdown").await?;
        ensure!(
            markdown.contains("Recorded memory provenance")
                && markdown.contains("proposal-record-1")
                && markdown.contains(record["record_hash"].as_str().unwrap())
        );
        ensure!(
            !report["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|row| row["provider"] == "memory")
                .any(|row| row["authority"] == "approved_requirement")
        );
        Ok(())
    }
    .await;
    f.close()?;
    result
}

#[tokio::test]
async fn metadata_only_changes_refuse_old_record_guard_while_body_only_and_fresh_reads_remain_usable()
-> Result<()> {
    let f = Fixture::new().await?;
    let result=async{
        let report:Value=serde_json::from_str(&f.ask("json").await?)?;let old=&proposal(&report)["memory_record"];
        f.write("second-agent",vec!["Site/other.ts"]).await?;
        let error=f.app.handle_read_memory_bank(serde_json::from_value(old["read"]["arguments"].clone())?).await.unwrap_err();ensure!(error.to_string().contains("memory_record_changed"));
        // The unchanged content hash alone cannot bind changed authorship/scope.
        let reply=f.app.handle_read_memory_bank(serde_json::from_value(json!({"project_id":f.pid,"section":"proposal-record-1","output_json":true,"max_tokens":2048,"citation":{"unit":"utf8_bytes","start":0,"expected_raw_hash":old["raw_content_hash"]}}))?).await?;
        let body:Value=serde_json::from_str(&reply.content[0].as_text().unwrap().text)?;ensure!(body["metadata"]["author"]=="second-agent");
        let fresh:Value=serde_json::from_str(&f.ask("json").await?)?;let new=&proposal(&fresh)["memory_record"];
        ensure!(new["record_hash"]!=old["record_hash"] && new["raw_content_hash"]==old["raw_content_hash"]);
        ensure!(new["recorded_author"]=="second-agent" && new["declared_related_files"]==json!(["Site/other.ts"]));
        f.app.handle_read_memory_bank(serde_json::from_value(new["read"]["arguments"].clone())?).await?;
        Ok(())
    }.await;
    f.close()?;
    result
}
