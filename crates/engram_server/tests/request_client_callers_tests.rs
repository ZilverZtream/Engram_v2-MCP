//! Browser callers belong in source-grounded end-to-end request navigation.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{AppState, Engram};
use rmcp::model::CallToolResult;
use serde_json::{Value, json};

fn text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|part| part.as_text().map(|t| t.text.as_str()))
        .collect()
}

struct Fixture {
    server: Engram,
    _owned: tempfile::TempDir,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_retains_authored_js_ts_callers_guidance_and_original_recovery() -> Result<()> {
    let owned = tempfile::tempdir()?;
    let root = owned.path().join("project");
    std::fs::create_dir_all(root.join(".github/instructions"))?;
    std::fs::write(
        root.join("Global.asax"),
        "<%@ Application Language=\"VB\" %>\n",
    )?;
    std::fs::write(
        root.join("Orders.asmx"),
        "<%@ WebService Language=\"VB\" CodeBehind=\"Orders.vb\" Class=\"Orders\" %>\n",
    )?;
    std::fs::write(
        root.join("Orders.vb"),
        "Public Class Orders\n<System.Web.Services.WebMethod()>\nPublic Function LoadOrders() As String\nReturn \"orders\"\nEnd Function\n<System.Web.Services.WebMethod()>\nPublic Function OtherOrders() As String\nReturn \"other\"\nEnd Function\nEnd Class\n",
    )?;
    let javascript = "function loadOrders() {\n return fetch(\"/Orders.asmx/LoadOrders\");\n}\nfunction otherOrders() {\n return fetch(\"/Orders.asmx/OtherOrders\");\n}\n";
    let typescript = "export function refreshOrders(): Promise<Response> {\n return fetch(\"/Orders.asmx/LoadOrders\");\n}\n";
    std::fs::write(root.join("client.js"), javascript)?;
    std::fs::write(root.join("client.ts"), typescript)?;
    let guidance =
        "Preserve client refresh behavior, including manual refresh exceptions.\n".repeat(400);
    std::fs::write(
        root.join(".github/instructions/client.instructions.md"),
        format!("---\napplyTo: '*.js,*.ts'\n---\n{guidance}"),
    )?;
    let (state, _) = AppState::new(Config {
        data_dir: owned.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        advertise_all_tools: true,
        ..Default::default()
    })?;
    let fixture = Fixture {
        server: Engram::new(state),
        _owned: owned,
    };
    let indexed = fixture.server.handle_index_project(serde_json::from_value(json!({
        "directory":root,"project_name":"Owned browser callers","project_type":"dotnet_webforms_vb","wait":true
    }))?).await?;
    let index_text = text(&indexed);
    let project = index_text
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .trim();
    let arguments = json!({"project_id":project,"entry_point":"POST /Orders.asmx/LoadOrders",
        "include_db":false,"include_triggers":false,"max_tokens":100000});
    let full = fixture
        .server
        .handle_trace_user_request(serde_json::from_value(arguments.clone())?)
        .await?;
    let full_text = text(&full);
    let report: Value = serde_json::from_str(&full_text)?;
    let handler = report["pages"][0]["handlers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| {
            entry["name"]
                .as_str()
                .unwrap_or_default()
                .ends_with("LoadOrders")
        })
        .unwrap();
    let callers = report["client_callers"].as_array().unwrap();
    ensure!(
        callers.len() == 2,
        "Both indexed script callers belong in request navigation: {report}"
    );
    for (path, source, name) in [
        ("client.js", javascript, "loadOrders"),
        ("client.ts", typescript, "refreshOrders"),
    ] {
        let caller = callers.iter().find(|c| c["file"] == path).unwrap();
        ensure!(caller["name"].as_str().unwrap_or_default().ends_with(name));
        ensure!(caller["source_blake3"] == blake3::hash(source.as_bytes()).to_hex().to_string());
        ensure!(caller["line"] == 2 && caller["target_node_id"] == handler["node_id"]);
        ensure!(caller["excerpt"].as_str().unwrap().contains("LoadOrders"));
        ensure!(caller["evidence"]["ajax_transport"] == "fetch");
        ensure!(
            caller["evidence"]["resolution"]
                .as_str()
                .unwrap()
                .starts_with("route_")
        );
    }
    ensure!(report["client_caller_coverage"]["runtime_execution_proven"] == false);
    ensure!(
        report["project_policy_sources"]
            .to_string()
            .contains("manual refresh exceptions")
    );
    let mut bounded_args = arguments.clone();
    bounded_args["max_tokens"] = json!(4096);
    let bounded = fixture
        .server
        .handle_trace_user_request(serde_json::from_value(bounded_args)?)
        .await?;
    let bounded_text = text(&bounded);
    ensure!(bounded_text.len() <= 4096 * 4);
    let bounded: Value = serde_json::from_str(&bounded_text)?;
    let response_id = bounded["response_delivery"]["response_id"]
        .as_str()
        .unwrap();
    ensure!(bounded["client_callers"].as_array().unwrap().len() == 2);
    ensure!(bounded["client_callers"][0]["content_deferred"] == true);
    ensure!(
        bounded["client_callers"][0]["source_blake3"]
            == report["client_callers"][0]["source_blake3"]
    );
    std::fs::write(root.join("client.js"), "// request removed\n")?;
    let stale = fixture
        .server
        .handle_trace_user_request(serde_json::from_value(arguments)?)
        .await?;
    let stale: Value = serde_json::from_str(&text(&stale))?;
    ensure!(
        stale["client_callers"].as_array().unwrap().len() == 1
            && stale["client_callers"][0]["file"] == "client.ts"
    );
    ensure!(stale["client_caller_coverage"]["status"] == "partial");
    ensure!(
        stale["client_caller_coverage"]["gaps"]
            .to_string()
            .contains("client.js")
    );
    let mut recovered = String::new();
    loop {
        let result = fixture.server.handle_trace_user_request(serde_json::from_value(json!({
            "project_id":project,"response_id":response_id,"response_offset":recovered.len(),"max_tokens":4096
        }))?).await?;
        let page: Value = serde_json::from_str(&text(&result))?;
        recovered.push_str(page["content"].as_str().unwrap());
        if page["complete"] == true {
            break;
        }
    }
    ensure!(
        recovered == full_text,
        "Saved recovery must retain the original caller evidence and complete guidance after edits"
    );
    Ok(())
}
