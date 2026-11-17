//! Source-name leads improve generation/QA input without certifying call outcomes.
use anyhow::{Result, ensure};
use engram_core::RelPath;
use engram_graph::{GraphStore, Node};
use engram_server::services::{
    business_helper_context::Audit,
    business_outcome_dependencies::{self as outcomes, OutcomeEvidence, SourceContext},
};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

const CALLER: &str = "Namespace Ui\nPublic Class Page\nPublic Function ReadItem(id As Integer) As Boolean\nDim allowed = Support.Helpers.Permitted(id)\nReturn allowed\nEnd Function\nEnd Class\nEnd Namespace\n";
const HELPER: &str = "Namespace Support\nPublic Class Helpers\nPublic Shared Function Permitted(id As Integer) As Boolean\nReturn id > 0\nEnd Function\nEnd Class\nEnd Namespace\n";
struct Fixture {
    graph: Arc<GraphStore>,
    root: PathBuf,
    owned: tempfile::TempDir,
}
fn index(graph: &GraphStore, root: &Path, path: &str, source: &str) -> Result<()> {
    std::fs::write(root.join(path), source)?;
    let file = Node {
        node_id: format!("file:{path}"),
        node_type: "file".into(),
        name: path.into(),
        namespace: "memory".into(),
        language: "vb".into(),
        file_path: RelPath::new(path),
        start_line: 1,
        end_line: source.lines().count() as u32,
        generation: 1,
        metadata: Some(json!({"file_hash":blake3::hash(source.as_bytes()).to_hex().to_string()})),
    };
    let mut nodes = vec![file.clone()];
    // These owned inputs contain one known declaration each. Seed its
    // independently specified identity/range without importing a private parser.
    let (name, start, end) = if source == HELPER {
        ("Support.Helpers.Permitted", 3, 5)
    } else {
        let (_, start, end) = caller_body(source)?;
        ("Ui.Page.ReadItem", start, end)
    };
    let mut node = file;
    node.node_id = format!("{path}:{start}");
    node.node_type = "function".into();
    node.name = name.into();
    node.start_line = start;
    node.end_line = end;
    node.metadata = None;
    nodes.push(node);
    graph.upsert_nodes("owned", &nodes)?;
    Ok(())
}
fn caller_body(source: &str) -> Result<(String, u32, u32)> {
    let lines: Vec<_> = source.lines().collect();
    let start = lines
        .iter()
        .position(|line| line.starts_with("Public Function ReadItem("))
        .ok_or_else(|| anyhow::anyhow!("Owned caller declaration missing"))?;
    let end = lines
        .iter()
        .position(|line| *line == "End Function")
        .ok_or_else(|| anyhow::anyhow!("Owned caller end missing"))?;
    Ok((
        lines[start..=end].join("\n"),
        start as u32 + 1,
        end as u32 + 1,
    ))
}
impl Fixture {
    fn new() -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new().prefix("bh-").tempdir_in(parent)?;
        let root = owned.path().join("r");
        std::fs::create_dir(&root)?;
        let graph = Arc::new(GraphStore::open(&owned.path().join("graph.redb"))?);
        index(&graph, &root, "Caller.vb", CALLER)?;
        index(&graph, &root, "Helpers.vb", HELPER)?;
        Ok(Self { graph, root, owned })
    }
    fn collect(&self, source: &str) -> Result<OutcomeEvidence> {
        index(&self.graph, &self.root, "Caller.vb", source)?;
        let (body, start, _) = caller_body(source)?;
        Ok(outcomes::collect(
            &body,
            "Ui.Page",
            "vb",
            start,
            "Caller.vb",
            Some(&SourceContext {
                graph: &self.graph,
                project_id: "owned",
                root: &self.root,
            }),
        ))
    }
    fn close(self) -> Result<()> {
        drop(self.graph);
        self.owned.close()?;
        Ok(())
    }
}

#[test]
fn qualified_assignment_supplies_source_leads_and_current_qa_without_certifying_outcomes()
-> Result<()> {
    let fixture = Fixture::new()?;
    let evidence = fixture.collect(CALLER)?;
    ensure!(
        evidence.dependencies.is_empty(),
        "ordinary assignment is not an immediate-return outcome dependency"
    );
    let context = evidence.helper_context.as_ref().unwrap();
    ensure!(context.candidates.len() == 1);
    let candidate = &context.candidates[0];
    ensure!(candidate.identity == "Support.Helpers.Permitted" && candidate.call_lines == vec![4]);
    ensure!(candidate.file_blake3 == blake3::hash(HELPER.as_bytes()).to_hex().to_string());
    ensure!(
        context.prompt_context().contains("Return id > 0")
            && context.interpretation.contains("call binding")
    );
    let stored = serde_json::to_string(&evidence)?;
    ensure!(
        !stored.contains("Return id > 0"),
        "supporting body must not be duplicated in stored analysis"
    );
    let stored: OutcomeEvidence = serde_json::from_str(&stored)?;
    let context = stored.helper_context.as_ref().unwrap();
    ensure!(context.candidates[0].body.is_none());
    let caller_hash = blake3::hash(CALLER.as_bytes()).to_hex().to_string();
    let current = context.qa_evidence(&fixture.root, Some(&caller_hash), &mut Audit::default());
    ensure!(
        current.contains("Return id > 0")
            && current.contains("Helpers.vb:3-5")
            && current.contains(&candidate.body_blake3)
    );
    std::fs::write(
        fixture.root.join("Helpers.vb"),
        HELPER.replace("Return id > 0", "Return id > 1"),
    )?;
    let stale = context.qa_evidence(&fixture.root, Some(&caller_hash), &mut Audit::default());
    ensure!(stale.contains("STALE_OR_UNAVAILABLE_HELPER_SOURCE") && !stale.contains("Return id >"));
    let changed = context.qa_evidence(
        &fixture.root,
        Some("different-caller"),
        &mut Audit::default(),
    );
    ensure!(
        changed.contains("STALE_OR_UNAVAILABLE_CALLER_SUPPORTING_SOURCE")
            && !changed.contains("Return id >")
    );
    fixture.close()
}

#[test]
fn ambiguous_sources_and_shadowable_simple_receivers_do_not_gain_helper_binding() -> Result<()> {
    let fixture = Fixture::new()?;
    index(&fixture.graph, &fixture.root, "Duplicate.vb", HELPER)?;
    let evidence = fixture.collect(CALLER)?;
    let context = evidence.helper_context.as_ref().unwrap();
    ensure!(
        context.candidates.is_empty()
            && context
                .omissions
                .iter()
                .any(|gap| gap.contains("ambiguous"))
    );
    let source = "Namespace Ui\nPublic Class Page\nPrivate Helpers As Object\nPublic Function ReadItem() As Boolean\nHelpers.Reject()\nReturn False\nEnd Function\nEnd Class\nEnd Namespace\n";
    let evidence = fixture.collect(source)?;
    ensure!(evidence.helper_context.is_none());
    ensure!(
        evidence.dependencies.len() == 1
            && evidence.dependencies[0].evidence_status == "unavailable"
            && evidence.dependencies[0].helper_body.is_none()
    );
    fixture.close()
}
