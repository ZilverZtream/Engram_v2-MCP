//! Multiple AI source reads retain the normal source and guidance contracts.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{AppState, Engram};
use serde_json::{Value, json};

const SOURCE: &str = "export function saveOrder() {\n  return true;\n}\n";
const GUIDE: &str = "Preserve ownership checks. Exception: read-only previews never save.\n";

struct Fixture {
    server: Engram,
    owned: tempfile::TempDir,
    project: String,
}
impl Fixture {
    async fn new() -> Result<Self> {
        let owned = tempfile::tempdir()?;
        let root = owned.path().join("source");
        std::fs::create_dir(&root)?;
        std::fs::write(root.join("first.ts"), SOURCE)?;
        std::fs::write(
            root.join("second.ts"),
            SOURCE.replace("saveOrder", "previewOrder"),
        )?;
        std::fs::write(root.join("AGENTS.md"), GUIDE)?;
        let (state, _) = AppState::new(Config {
            data_dir: owned.path().join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            advertise_all_tools: true,
            ..Default::default()
        })?;
        let server = Engram::new(state);
        let indexed = server.handle_index_project(serde_json::from_value(json!({
            "directory":root,"project_name":"Owned batched source","project_type":"general","wait":true
        }))?).await?;
        let text = &indexed.content[0].as_text().unwrap().text;
        let project = text
            .split("project_id: ")
            .nth(1)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .trim()
            .to_owned();
        Ok(Self {
            server,
            owned,
            project,
        })
    }
    fn arguments(&self, path: &str, output_json: bool) -> Value {
        json!({"project_id":self.project,"file_path":path,"line_start":1,"line_end":3,
            "context_lines":0,"expected_file_blake3":blake3::hash(if path=="first.ts" { SOURCE.to_owned() } else { SOURCE.replace("saveOrder","previewOrder") }.as_bytes()).to_hex().to_string(),"output_json":output_json})
    }
    fn close(self) -> Result<()> {
        let Self { server, owned, .. } = self;
        drop(server);
        owned.close()?;
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batched_source_matches_normal_json_markdown_and_independent_hash_errors() -> Result<()> {
    let fixture = Fixture::new().await?;
    let first = fixture.arguments("first.ts", true);
    let second = fixture.arguments("second.ts", false);
    let mut stale = first.clone();
    stale["expected_file_blake3"] = json!("0".repeat(64));
    let mut expected = Vec::new();
    for args in [&first, &second] {
        let reply = fixture
            .server
            .handle_get_full_method_body(serde_json::from_value(args.clone())?)
            .await?;
        let text = &reply.content[0].as_text().unwrap().text;
        ensure!(text.contains("return true") && text.contains("read-only previews never save"));
        expected.push(serde_json::to_value(reply)?);
    }
    let error = fixture
        .server
        .handle_get_full_method_body(serde_json::from_value(stale.clone())?)
        .await
        .unwrap_err();
    let reply = fixture
        .server
        .handle_batch_read(serde_json::from_value(json!({"requests":[
            {"request_id":"first","request":{"tool":"get_full_method_body","arguments":first}},
            {"request_id":"stale","request":{"tool":"get_full_method_body","arguments":stale}},
            {"request_id":"second","request":{"tool":"get_full_method_body","arguments":second}}
        ]}))?)
        .await?;
    let body: Value = serde_json::from_str(&reply.content[0].as_text().unwrap().text)?;
    ensure!(body["complete"] == true);
    ensure!(body["results"][0]["response"] == expected[0]);
    ensure!(body["results"][1]["status"] == "request_error");
    ensure!(body["results"][1]["response"] == serde_json::to_value(error)?);
    ensure!(body["results"][2]["response"] == expected[1]);
    fixture.close()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_range_arguments_survive_whole_result_omission_and_executable_recovery() -> Result<()>
{
    let fixture = Fixture::new().await?;
    let args = json!({"project_id":fixture.project,"file_path":"first.ts",
        "source_ranges":[{"line_start":1,"line_end":1},{"line_start":3,"line_end":3}],
        "context_lines":0,"output_json":true,"expected_file_blake3":blake3::hash(SOURCE.as_bytes()).to_hex().to_string()});
    let original = fixture
        .server
        .handle_get_full_method_body(serde_json::from_value(args.clone())?)
        .await?;
    let reply = fixture
        .server
        .handle_batch_read(serde_json::from_value(
            json!({"max_tokens":400,"requests":[
                {"request_id":"ranges","request":{"tool":"get_full_method_body","arguments":args}}
            ]}),
        )?)
        .await?;
    let text = &reply.content[0].as_text().unwrap().text;
    ensure!(text.len() <= 1600);
    let body: Value = serde_json::from_str(text)?;
    let row = &body["results"][0];
    ensure!(row["status"] == "omitted_shared_response_budget", "{body}");
    ensure!(row["response"].is_null());
    ensure!(row["recovery"]["tool"] == "get_full_method_body");
    let recovered = fixture
        .server
        .handle_get_full_method_body(serde_json::from_value(
            row["recovery"]["arguments"].clone(),
        )?)
        .await?;
    ensure!(serde_json::to_value(recovered)? == serde_json::to_value(original)?);
    fixture.close()
}
