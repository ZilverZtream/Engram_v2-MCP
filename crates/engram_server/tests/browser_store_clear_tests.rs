//! Auth-flow key traces must include admitted whole-store clear candidates.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{AppState, Engram, read_report_delivery};
use rmcp::model::{CallToolRequestParam, CallToolResult};
use serde_json::{Value, json};

struct Fixture {
    server: Engram,
    owned: tempfile::TempDir,
}
fn text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|part| part.as_text().map(|row| row.text.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}
fn clear(result: &CallToolResult) -> Result<Value> {
    Ok(serde_json::from_str(
        text(result)
            .split("store_wide_mutation_candidates: ")
            .nth(1)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )?)
}
fn delivery(project: &str, mut args: Value) -> Result<read_report_delivery::Report> {
    args["project_id"] = json!(project);
    let mut request: CallToolRequestParam =
        serde_json::from_value(json!({"name":"trace_state_usage","arguments":args}))?;
    Ok(read_report_delivery::prepare(&mut request, &Engram::tool_registry().list_all())?.unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browser_key_trace_retains_store_wide_clear_sources_and_original_recovery() -> Result<()> {
    let fixture_root = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let owned = tempfile::Builder::new()
        .prefix("b")
        .rand_bytes(2)
        .tempdir_in(fixture_root)?;
    let root = owned.path().join("r");
    std::fs::create_dir(&root)?;
    let callback =
        "const refresh = sessionStorage.getItem('refresh_token');\nsessionStorage.clear();\n";
    let login = "export function begin() { sessionStorage.setItem('refresh_token', 'owned'); }\nexport function finishLogin() { sessionStorage.clear(); }\n";
    for (path, source) in [
        ("callback.js", callback),
        ("login.ts", login),
        (
            "other.js",
            "function localReset() { localStorage.clear(); }\nfunction shadow(sessionStorage) { sessionStorage.clear(); }\n",
        ),
        (
            "AGENTS.md",
            "Preserve authentication callback behavior and its accepted exceptions.\n",
        ),
    ] {
        std::fs::write(root.join(path), source)?;
    }
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
        owned,
    };
    let indexed = fixture
        .server
        .handle_index_project(serde_json::from_value(json!({"directory":root,
        "project_name":"Owned auth storage","project_type":"dotnet_webforms_vb","wait":true}))?)
        .await?;
    let index_text = text(&indexed);
    let project = index_text
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .trim();
    let args = json!({"project_id":project,"state_type":"SessionStorage","state_key":"refresh_token","limit":20});
    let full = fixture
        .server
        .handle_trace_state_usage_with_clear(serde_json::from_value(args.clone())?)
        .await?;
    let report = clear(&full)?;
    let rows = report["candidates"].as_array().unwrap();
    ensure!(
        rows.len() == 2,
        "Both auth clear sources must be present: {report}"
    );
    for (path, source) in [("callback.js", callback), ("login.ts", login)] {
        let row = rows.iter().find(|row| row["file"] == path).unwrap();
        ensure!(
            row["line"] == 2 && row["store"] == "SessionStorage" && row["operation"] == "clear"
        );
        ensure!(row["source_blake3"] == blake3::hash(source.as_bytes()).to_hex().to_string());
        ensure!(row["excerpt"] == "sessionStorage.clear()");
    }
    ensure!(
        report["runtime_execution_proven"] == false
            && report["project_policy_sources"]
                .to_string()
                .contains("accepted exceptions")
    );
    let local = fixture
        .server
        .handle_trace_state_usage_with_clear(serde_json::from_value(json!({
        "project_id":project,"state_type":"LocalStorage","state_key":"never_named_elsewhere"}))?)
        .await?;
    let local = clear(&local)?;
    ensure!(
        local["candidates"].as_array().unwrap().len() == 1
            && local["candidates"][0]["file"] == "other.js",
        "Clear applies to queried keys even when they have no literal indexed access; shadowed SessionStorage is excluded"
    );
    let expected = serde_json::to_string(&full)?;
    let controls = delivery(project, json!({"max_tokens":512}))?;
    let bounded = controls.finish(&fixture.owned.path().join("data"), full)?;
    ensure!(text(&bounded).len() <= 512 * 4);
    let bounded: Value = serde_json::from_str(&text(&bounded))?;
    let id = bounded["response_delivery"]["response_id"]
        .as_str()
        .unwrap();
    std::fs::write(
        root.join("callback.js"),
        "const refresh = sessionStorage.getItem('refresh_token');\n",
    )?;
    let changed = fixture
        .server
        .handle_trace_state_usage_with_clear(serde_json::from_value(args)?)
        .await?;
    let changed = clear(&changed)?;
    ensure!(
        changed["candidates"].as_array().unwrap().len() == 1
            && changed["candidates"][0]["file"] == "login.ts"
    );
    ensure!(changed["gaps"].to_string().contains("callback.js"));
    let mut recovered = String::new();
    loop {
        let controls = delivery(
            project,
            json!({"max_tokens":512,"response_id":id,"response_offset":recovered.len()}),
        )?;
        let page = controls
            .recover(&fixture.owned.path().join("data"))?
            .unwrap();
        let page: Value = serde_json::from_str(&text(&page))?;
        recovered.push_str(page["content"].as_str().unwrap());
        if page["complete"] == true {
            break;
        }
    }
    ensure!(
        recovered == expected,
        "The existing immutable result recovery must retain clear evidence and guidance after source changes"
    );
    fixture
        .server
        .handle_update_project(serde_json::from_value(json!({
            "project_id":project,"wait":true
        }))?)
        .await?;
    let refreshed = fixture
        .server
        .handle_trace_state_usage_with_clear(serde_json::from_value(
            json!({"project_id":project,"state_type":"SessionStorage",
            "state_key":"refresh_token","limit":20}),
        )?)
        .await?;
    let refreshed = clear(&refreshed)?;
    ensure!(
        refreshed["candidates"].as_array().unwrap().len() == 1
            && refreshed["candidates"][0]["file"] == "login.ts",
        "Refreshing one script must preserve the other script's clear relationship"
    );
    Ok(())
}
