//! Opt-in generation never removes the usable deterministic evidence report.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{Engram, state::AppState};
use serde_json::{Value, json};

fn text(result: &rmcp::model::CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}

#[test]
fn default_is_retrieval_only_and_unavailable_generation_preserves_evidence() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let runtime = tokio::runtime::Runtime::new()?;
    let result: Result<()> = runtime.block_on(async {
        let root = temporary.path().join("repo");
        std::fs::create_dir_all(root.join("Site"))?;
        std::fs::write(root.join("Site/Zoom.vb"), "Public Module Zoom\n Public Function MinimumZoom() As Integer\n  Return 6\n End Function\nEnd Module\n")?;
        let (state, _) = AppState::new(Config {
            data_dir: temporary.path().join("data"), allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(), llm_backend: "none".into(), ..Default::default()
        })?;
        let engram = Engram::new(state);
        let indexed = engram.handle_index_project(serde_json::from_value(json!({
            "directory":root,"project_name":"Cited answer","project_type":"general","wait":true
        }))?).await?;
        let pid = text(&indexed).split("project_id: ").nth(1).unwrap().lines().next().unwrap().trim();
        let mut request = json!({"project_id":pid,"question":"What does MinimumZoom return in Site/Zoom.vb?","output_format":"json"});
        let default: engram_server::AskCodebaseRequest = serde_json::from_value(request.clone())?;
        ensure!(!default.synthesize && default.synthesis_timeout_ms.is_none());
        let first: Value = serde_json::from_str(text(&engram.handle_ask_codebase(default).await?))?;
        ensure!(first["mode"] == "retrieval_only" && first.get("synthesis").is_none());
        ensure!(!first["evidence"].as_array().unwrap().is_empty());
        request["synthesize"] = json!(true);
        let opted: Value = serde_json::from_str(text(&engram.handle_ask_codebase(serde_json::from_value(request.clone())?).await?))?;
        ensure!(opted["mode"] == "retrieval_only");
        ensure!(opted["synthesis"]["reason"] == "llm_backend_unavailable");
        ensure!(opted["synthesis"]["provider_requests"] == 0);
        for key in ["evidence", "status", "unknowns", "conflicts"] {
            ensure!(first[key] == opted[key], "Changed {key} during unavailable synthesis");
        }
        request["output_format"] = json!("markdown");
        let rendered = engram.handle_ask_codebase(serde_json::from_value(request)?).await?;
        ensure!(text(&rendered).contains("llm_backend_unavailable") && text(&rendered).contains("Site/Zoom.vb"));
        Ok(())
    });
    drop(runtime);
    temporary.close()?;
    result
}
