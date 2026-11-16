//! Actual state-report selection is preserved by complete-result delivery.
use anyhow::{Result, ensure};
use engram_core::{Config, NodeId, ProjectRecord, RelPath};
use engram_graph::{Edge, EdgeKind, Node};
use engram_server::{AppState, Engram, read_report_delivery as delivery};
use rmcp::model::{CallToolRequestParam, CallToolResult};
use serde_json::{Value, json};

const PROJECT: &str = "owned-state-report";
const KEY: &str = "tenant_preferences";

// Database/search owners drop before the directory, including assertion unwinding.
struct Fixture {
    server: Engram,
    owned: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Result<Self> {
        let fixture_root = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new()
            .prefix("s")
            .rand_bytes(2)
            .tempdir_in(fixture_root)?;
        let root = owned.path().join("r");
        std::fs::create_dir(&root)?;
        let read_call = format!("localStorage.getItem('{KEY}')");
        let write_call = format!("localStorage.setItem('{KEY}', value || '{{}}')");
        let source = (0..180).map(|i|format!("export function preference_{i:03}() {{ const value = {read_call}; {write_call}; return value; }}\n")).collect::<String>();
        std::fs::write(root.join("preferences.ts"), &source)?;
        let (state, _) = AppState::new(Config {
            data_dir: owned.path().join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            ..Default::default()
        })?;
        state.registry.put_project(&ProjectRecord {
            project_id: PROJECT.into(),
            project_name: PROJECT.into(),
            project_type: "general".into(),
            directory: root.to_string_lossy().into(),
            created_at_ms: 1,
            updated_at_ms: 1,
            reindex_required_since_ms: None,
        })?;
        state.registry.set_meta(PROJECT, "active_generation", "1")?;
        let node =
            |id: String, kind: &str, name: String, line: u32, metadata: Option<Value>| Node {
                node_id: id,
                node_type: kind.into(),
                name,
                namespace: "code".into(),
                language: "typescript".into(),
                file_path: RelPath::new("preferences.ts"),
                start_line: line,
                end_line: line,
                generation: 1,
                metadata,
            };
        let state_id = NodeId::state("LocalStorage", KEY).0;
        let hash = blake3::hash(source.as_bytes()).to_hex().to_string();
        let mut file_metadata = serde_json::Map::new();
        file_metadata.insert(
            engram_index::source_contract::MODERN_STATE_KEY.into(),
            json!(json!({"source_hash":hash,"status":"complete","issues":[]}).to_string()),
        );
        let mut nodes = vec![
            node(state_id.clone(), "global_state", KEY.into(), 1, None),
            node(
                NodeId::file("preferences.ts").0,
                "file",
                "preferences.ts".into(),
                1,
                Some(Value::Object(file_metadata)),
            ),
        ];
        let mut edges = Vec::new();
        let mut line_byte = 0;
        for i in 0..180 {
            let name = format!("preference_{i:03}");
            let id = format!("owned-function:{i}");
            nodes.push(node(id.clone(), "function", name, i + 1, None));
            let line = source.lines().nth(i as usize).unwrap();
            for (kind, call) in [
                (EdgeKind::ReadsState, &read_call),
                (EdgeKind::WritesState, &write_call),
            ] {
                let byte = line_byte + line.find(call).unwrap();
                edges.push(Edge {source_id:id.clone(),target_id:state_id.clone(),namespace:"code".into(),language:"typescript".into(),
                    edge_kind:kind,weight:1,generation:1,updated_at_ms:1,
                    metadata:Some(json!({"state_access_line":i+1,"state_access_column":byte-line_byte+1,"state_access_byte":byte.to_string(),"state_access_end_byte":(byte+call.len()).to_string(),"state_evidence":call}))});
            }
            line_byte += line.len() + 1;
        }
        state.graph.upsert_nodes(PROJECT, &nodes)?;
        state.graph.upsert_edges(PROJECT, &edges)?;
        Ok(Self {
            server: Engram::new(state),
            owned,
        })
    }
}

fn text(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}
fn prepared(mut arguments: Value) -> Result<delivery::Report> {
    arguments["project_id"] = json!(PROJECT);
    let mut request: CallToolRequestParam =
        serde_json::from_value(json!({"name":"trace_state_usage","arguments":arguments}))?;
    Ok(delivery::prepare(&mut request, &Engram::tool_registry().list_all())?.unwrap())
}

#[tokio::test]
async fn state_report_recovers_exact_original_after_graph_owners_close_and_source_disappears()
-> Result<()> {
    let f = Fixture::new()?;
    let original = f
        .server
        .handle_trace_state_usage(serde_json::from_value(json!({
            "project_id":PROJECT,"state_type":"LocalStorage","state_key":KEY,"limit":180
        }))?)
        .await?;
    ensure!(text(&original).len() > 40960);
    for i in 0..180 {
        ensure!(
            text(&original)
                .matches(&format!("preference_{i:03} [function]"))
                .count()
                == 2
        );
    }
    ensure!(text(&original).contains("Current source and runtime behavior are unverified"));
    ensure!(text(&original).contains("Shared origin"));
    let data = f.server.state.cfg.data_dir.clone();
    let default = prepared(json!({"state_type":"LocalStorage","state_key":KEY}))?
        .finish(&data, original.clone())?;
    ensure!(text(&default).len() <= 40960);
    ensure!(
        serde_json::from_str::<Value>(text(&default))?["report_content"] == "DEFERRED_NOT_SUPPLIED"
    );
    let deferred =
        prepared(json!({"state_type":"LocalStorage","state_key":KEY,"max_tokens":1024}))?
            .finish(&data, original.clone())?;
    ensure!(text(&deferred).len() <= 4096);
    let manifest: Value = serde_json::from_str(text(&deferred))?;
    let fitting = f
        .server
        .handle_trace_state_usage(serde_json::from_value(json!({
            "project_id":PROJECT,"state_type":"LocalStorage","state_key":KEY,"limit":1
        }))?)
        .await?;
    ensure!(
        text(&fitting).len() < 4096
            && text(&fitting).contains("writers truncated at 1")
            && text(&fitting).contains("readers truncated at 1")
    );
    let kept = prepared(json!({"state_type":"LocalStorage","state_key":KEY,"max_tokens":1024}))?
        .finish(&data, fitting.clone())?;
    ensure!(serde_json::to_value(kept)? == serde_json::to_value(fitting)?);
    let Fixture { server, owned } = f;
    drop(server);
    std::fs::remove_file(owned.path().join("r/preferences.ts"))?;
    let mut action = manifest["response_delivery"]["next_call"].clone();
    let mut restored = String::new();
    for _ in 0..1000 {
        ensure!(
            action["arguments"].get("state_type").is_none()
                && action["arguments"].get("state_key").is_none()
        );
        let page = prepared(action["arguments"].clone())?
            .recover(&data)?
            .unwrap();
        ensure!(text(&page).len() <= 4096);
        let page: Value = serde_json::from_str(text(&page))?;
        ensure!(page["start_byte"] == restored.len());
        restored.push_str(page["content"].as_str().unwrap());
        if page["complete"] == true {
            break;
        }
        action = page["next_call"].clone();
    }
    ensure!(restored == serde_json::to_string(&original)?);
    ensure!(
        blake3::hash(restored.as_bytes()).to_hex().as_str()
            == manifest["response_delivery"]["response_id"]
                .as_str()
                .unwrap()
    );
    owned.close()?;
    Ok(())
}
