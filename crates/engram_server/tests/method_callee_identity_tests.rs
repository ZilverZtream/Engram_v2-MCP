//! Method metadata must expose indexed callees through reusable canonical identities.
use anyhow::{Result, ensure};
use engram_core::{Config, ProjectRecord, RelPath};
use engram_graph::{Edge, EdgeKind, Node};
use engram_server::{AppState, Engram};
use serde_json::{Value, json};

const PROJECT: &str = "method-identity-fixture";
const OWNER: &str = "function:Orders.saveOrder";
fn node(id: &str, name: &str, line: u32) -> Node {
    Node {
        node_id: id.into(),
        node_type: "function".into(),
        name: name.into(),
        namespace: "memory".into(),
        language: "typescript".into(),
        file_path: RelPath::new("entry.ts"),
        start_line: line,
        end_line: line,
        generation: 1,
        metadata: None,
    }
}
fn edge(target: &str, kind: EdgeKind) -> Edge {
    Edge {
        source_id: OWNER.into(),
        target_id: target.into(),
        namespace: "memory".into(),
        language: "typescript".into(),
        edge_kind: kind,
        weight: 1,
        generation: 1,
        metadata: None,
        updated_at_ms: 1,
    }
}
struct Fixture {
    // Drop database/runtime owners before TempDir on normal return and unwinding.
    server: Engram,
    owned: tempfile::TempDir,
}
impl Fixture {
    fn new(nodes: &[Node], edges: &[Edge]) -> Result<Self> {
        let owned = tempfile::tempdir()?;
        let root = owned.path().join("source");
        std::fs::create_dir(&root)?;
        std::fs::write(
            root.join("entry.ts"),
            "export function saveOrder() { return validateOrder(); }\nexport function validateOrder() { return true; }\n",
        )?;
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
            directory: root.to_string_lossy().into_owned(),
            project_type: "general".into(),
            created_at_ms: 1,
            updated_at_ms: 1,
            reindex_required_since_ms: None,
        })?;
        state.graph.upsert_nodes(PROJECT, nodes)?;
        state.graph.upsert_edges(PROJECT, edges)?;
        Ok(Self {
            server: Engram::new(state),
            owned,
        })
    }
    async fn read(&self, output_json: bool, max_tokens: usize) -> Result<String> {
        let result = self
            .server
            .handle_get_method_info(serde_json::from_value(json!({
                "project_id":PROJECT,"fqn_or_name":"Orders.saveOrder","file_path":"entry.ts",
                "output_json":output_json,"max_tokens":max_tokens
            }))?)
            .await?;
        Ok(result.content[0].as_text().unwrap().text.clone())
    }
    fn close(self) -> Result<()> {
        let Self { server, owned } = self;
        drop(server);
        owned.close()?;
        Ok(())
    }
}

#[tokio::test]
async fn calls_and_heuristic_edges_supply_one_canonical_target_and_an_executable_identity_action()
-> Result<()> {
    let target = "function:Orders.validateOrder";
    let fixture = Fixture::new(
        &[
            node(OWNER, "Orders.saveOrder", 1),
            node(target, "Orders.validateOrder", 2),
        ],
        &[
            edge(target, EdgeKind::Calls),
            edge(target, EdgeKind::Dependency),
        ],
    )?;
    let body: Value = serde_json::from_str(&fixture.read(true, 65536).await?)?;
    let owner = &body[0];
    ensure!(owner["node_id"] == OWNER && owner["indexed_generation"] == 1);
    ensure!(owner["calls_methods"] == json!(["Orders.validateOrder"]));
    let evidence = &owner["callee_evidence"];
    ensure!(evidence["status"]["status"] == "complete");
    ensure!(evidence["targets"].as_array().unwrap().len() == 1);
    let callee = &evidence["targets"][0];
    ensure!(callee["node_id"] == target && callee["file_path"] == "entry.ts");
    ensure!(callee["fqn"] == "Orders.validateOrder" && callee["line_start"] == 2);
    ensure!(callee["edge_kinds"] == json!(["calls", "dependency"]));
    ensure!(owner["indexed_coverage"]["callers"]["status"] == "complete");
    let resolved = fixture
        .server
        .handle_resolve_id(serde_json::from_value(
            callee["resolve_action"]["arguments"].clone(),
        )?)
        .await?;
    let text = &resolved.content[0].as_text().unwrap().text;
    ensure!(text.contains(target) && text.contains("Orders.validateOrder"));
    let markdown = fixture.read(false, 65536).await?;
    ensure!(
        markdown.contains("Indexed callee identities")
            && markdown.contains(target)
            && markdown.contains("resolve_id")
    );
    ensure!(
        markdown.contains("not an atomic source snapshot")
            && markdown.contains("runtime execution")
    );
    fixture.close()?;
    Ok(())
}

#[tokio::test]
async fn missing_nonmethod_and_capped_targets_are_disclosed_without_losing_legacy_hints()
-> Result<()> {
    let mut owner = node(OWNER, "Orders.saveOrder", 1);
    owner.metadata = Some(json!({"calls_methods":"Legacy.Unverified"}));
    let mut nodes = vec![owner];
    let mut edges = vec![];
    for i in 0..53 {
        let id = format!("function:target{i:03}");
        edges.push(edge(&id, EdgeKind::Calls));
        if i == 0 {
            continue;
        }
        let mut target = node(&id, &format!("Orders.target{i:03}"), i + 2);
        if i == 1 {
            target.node_type = "file".into();
        }
        nodes.push(target);
    }
    let fixture = Fixture::new(&nodes, &edges)?;
    let value: Value = serde_json::from_str(&fixture.read(true, 65536).await?)?;
    let evidence = &value[0]["callee_evidence"];
    ensure!(evidence["status"]["status"] == "truncated");
    ensure!(evidence["status"]["known_total"].is_null());
    ensure!(evidence["missing_targets"] == 1 && evidence["non_method_targets"] == 1);
    ensure!(evidence["uninspected_targets"] == 3);
    ensure!(evidence["targets"].as_array().unwrap().len() == 48);
    ensure!(evidence["metadata_hints"] == json!(["Legacy.Unverified"]));
    ensure!(
        value[0]["calls_methods"]
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name == "Legacy.Unverified")
    );
    ensure!(
        !evidence["targets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|target| target["fqn"] == "Legacy.Unverified")
    );
    fixture.close()?;
    Ok(())
}

#[tokio::test]
async fn budgeted_metadata_recovers_exact_canonical_identities_after_the_graph_is_removed()
-> Result<()> {
    let mut nodes = vec![node(OWNER, "Orders.saveOrder", 1)];
    let mut edges = Vec::new();
    for i in 0..50 {
        let name = format!("Orders.{}Target{i:03}", "SourceIdentity".repeat(24));
        let id = format!("function:{name}");
        nodes.push(node(&id, &name, i + 2));
        edges.push(edge(&id, EdgeKind::Calls));
    }
    let fixture = Fixture::new(&nodes, &edges)?;
    let complete = fixture.read(true, 65536).await?;
    ensure!(complete.len() > 40960);
    let deferred = fixture.read(true, 2048).await?;
    ensure!(deferred.len() <= 8192);
    let manifest: Value = serde_json::from_str(&deferred)?;
    ensure!(manifest["response_delivery"]["original_bytes"] == complete.len());
    fixture.server.state.graph.delete_project_data(PROJECT)?;
    let mut action = manifest["response_delivery"]["next_call"].clone();
    let mut recovered = String::new();
    for _ in 0..100 {
        let result = fixture
            .server
            .handle_get_method_info(serde_json::from_value(action["arguments"].clone())?)
            .await?;
        let page: Value = serde_json::from_str(&result.content[0].as_text().unwrap().text)?;
        ensure!(page["start_byte"] == recovered.len());
        recovered.push_str(page["content"].as_str().unwrap());
        if page["complete"] == true {
            break;
        }
        action = page["next_call"].clone();
    }
    ensure!(recovered == complete);
    ensure!(
        blake3::hash(recovered.as_bytes()).to_hex().as_str()
            == manifest["response_delivery"]["response_id"]
                .as_str()
                .unwrap()
    );
    let value: Value = serde_json::from_str(&recovered)?;
    ensure!(
        value[0]["callee_evidence"]["targets"]
            .as_array()
            .unwrap()
            .len()
            == 50
    );
    fixture.close()?;
    Ok(())
}
