//! Literal state relationships belong in the ordinary natural-language evidence path.
use anyhow::{Result, ensure};
use engram_core::{RelPath, memory::MemoryBudget};
use engram_graph::{Edge, EdgeKind, GraphStore, Node};
use engram_server::services::ask_engine::{literal_state as state, status::ProviderStatus};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

const FILE: &str = "Site/state.vb";
const SOURCE: &str = "Public Class StateDemo\nPublic Sub SetProject()\nSession(\"K\") = 7\nEnd Sub\nPublic Function GetProject() As Object\nReturn Session(\"K\")\nEnd Function\nEnd Class\n";
struct Fixture {
    graph: Arc<GraphStore>,
    selected: BTreeMap<String, u64>,
    budget: MemoryBudget,
    // The sole database owner drops before the directory, including unwinding.
    owned: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new().prefix("st-").tempdir_in(parent)?;
        std::fs::create_dir(owned.path().join("Site"))?;
        std::fs::write(owned.path().join(FILE), SOURCE)?;
        let graph = Arc::new(GraphStore::open(&owned.path().join("graph.redb"))?);
        let state_id = engram_core::ids::NodeId::state("Session", "K").0;
        let mut nodes = Vec::new();
        for (id, kind, name, first, last) in [
            (format!("file:{FILE}"), "file", FILE, 0, 0),
            ("writer".into(), "function", "StateDemo.SetProject", 2, 4),
            ("reader".into(), "function", "StateDemo.GetProject", 5, 7),
            (state_id, "global_state", "Session:K", 1, 1),
        ] {
            nodes.push(Node {
                node_id: id,
                node_type: kind.into(),
                name: name.into(),
                namespace: "memory".into(),
                language: "vbnet".into(),
                file_path: RelPath::new(FILE),
                start_line: first,
                end_line: last,
                generation: 1,
                metadata: Some(
                    json!({"file_hash":blake3::hash(SOURCE.as_bytes()).to_hex().to_string()}),
                ),
            });
        }
        graph.upsert_nodes("state-owned", &nodes)?;
        let fixture = Self {
            graph,
            selected: BTreeMap::from([(FILE.into(), 1)]),
            budget: MemoryBudget::new(128 * 1024 * 1024),
            owned,
        };
        fixture.edge("writer", EdgeKind::WritesState, 1)?;
        fixture.edge("reader", EdgeKind::ReadsState, 1)?;
        Ok(fixture)
    }
    fn edge(&self, from: &str, kind: EdgeKind, generation: u64) -> Result<()> {
        self.graph.upsert_edges(
            "state-owned",
            &[Edge {
                source_id: from.into(),
                target_id: engram_core::ids::NodeId::state("Session", "K").0,
                edge_kind: kind,
                namespace: "memory".into(),
                language: "vbnet".into(),
                weight: 1,
                generation,
                metadata: None,
                updated_at_ms: 1,
            }],
        )?;
        Ok(())
    }
    fn collect(&self, scopes: &[String]) -> Result<state::StateEvidence> {
        state::collect(
            &self.graph,
            "state-owned",
            self.owned.path(),
            &state::queries("Where is Session(\"K\") written and read?"),
            &self.selected,
            scopes,
            &self.budget,
        )
    }
}

#[test]
fn only_explicit_literal_state_mentions_activate_the_provider_without_merging_stores() {
    let queries =
        state::queries("Compare Session(\"K\") and ViewState['K']; session(\"K\") again.");
    assert_eq!(queries.len(), 2);
    assert_eq!(queries[0].state_type, "Session");
    assert_eq!(queries[1].state_type, "ViewState");
    assert!(
        state::queries("Where is Session(variable) written? localStorage.getItem('K')").is_empty()
    );
    assert_eq!(
        state::queries("Session('1') Session('2') Session('3') Session('4') Session('5')").len(),
        4
    );
}

#[test]
fn admitted_reader_and_writer_facts_keep_real_source_hashes_spans_and_graph_endpoints() -> Result<()>
{
    let fixture = Fixture::new()?;
    let result = fixture.collect(&[])?;
    ensure!(result.provider.status == ProviderStatus::Hit && result.items.len() == 2);
    let facts: Vec<Value> = result
        .items
        .iter()
        .map(|item| serde_json::from_str(&item.content))
        .collect::<Result<_, _>>()?;
    ensure!(facts.iter().any(|fact| fact["roles"] == json!(["writer"]) && fact["indexed_symbol_id"] == "writer"));
    ensure!(facts.iter().any(|fact| fact["roles"] == json!(["reader"]) && fact["indexed_symbol_id"] == "reader"));
    for item in &result.items {
        ensure!(item.path.as_deref() == Some(FILE) && item.document_id.is_none());
        ensure!(
            item.source_verification
                .as_ref()
                .unwrap()
                .contains(blake3::hash(SOURCE.as_bytes()).to_hex().as_str())
        );
        let fact: Value = serde_json::from_str(&item.content)?;
        ensure!(
            fact["source_excerpt"]
                .as_str()
                .unwrap()
                .contains("Session(\"K\")")
        );
        let recovery: engram_server::models::GetFullMethodBodyRequest =
            serde_json::from_value(fact["source_recovery"]["arguments"].clone())?;
        ensure!(recovery.file_path.as_deref() == Some(FILE));
        ensure!(
            recovery.expected_file_blake3.as_deref()
                == Some(blake3::hash(SOURCE.as_bytes()).to_hex().as_str())
        );
        ensure!(
            item.warnings
                .iter()
                .any(|warning| warning.contains("runtime")
                    && warning.contains("not a verified access-site"))
        );
    }
    ensure!(fixture.collect(&["Elsewhere".into()])?.items.is_empty());
    fixture.edge(&format!("file:{FILE}"), EdgeKind::ReadsState, 1)?;
    let with_file = fixture.collect(&[])?;
    let file_item = with_file
        .items
        .iter()
        .find(|item| item.title.as_deref().unwrap_or("").ends_with(FILE))
        .expect("File-scoped reader must remain recoverable");
    let file_fact: Value = serde_json::from_str(&file_item.content)?;
    ensure!(file_item.lines.is_none() && file_fact["source_scope"] == "file");
    ensure!(file_fact["source_excerpt"].is_null() && file_fact["declaration_lines"].is_null());
    ensure!(file_fact["source_recovery"]["arguments"]["line_start"] == 1);
    ensure!(
        with_file
            .qualify_status(engram_server::services::ask_engine::status::AnswerStatus::Answered)
            == engram_server::services::ask_engine::status::AnswerStatus::Partial
    );
    Ok(())
}

#[test]
fn changed_source_or_unselected_revisions_withhold_facts_and_mismatched_edges_stay_partial()
-> Result<()> {
    let mut fixture = Fixture::new()?;
    std::fs::write(
        fixture.owned.path().join(FILE),
        format!("{SOURCE}\n' changed\n"),
    )?;
    let changed = fixture.collect(&[])?;
    ensure!(changed.items.is_empty() && changed.provider.status == ProviderStatus::Stale);
    std::fs::write(fixture.owned.path().join(FILE), SOURCE)?;
    fixture.selected.insert(FILE.into(), 2);
    ensure!(fixture.collect(&[])?.items.is_empty());
    fixture.selected.insert(FILE.into(), 1);
    fixture.edge("writer", EdgeKind::WritesState, 2)?;
    let current = fixture.collect(&[])?;
    ensure!(current.items.len() == 1 && current.provider.truncated);
    let fact: Value = serde_json::from_str(&current.items[0].content)?;
    ensure!(fact["roles"] == json!(["reader"]));
    Ok(())
}
