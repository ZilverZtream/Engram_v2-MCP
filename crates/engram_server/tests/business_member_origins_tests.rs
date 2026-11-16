//! Supporting constructor source survives method-only analysis without an LLM.
use anyhow::{Result, ensure};
use engram_core::RelPath;
use engram_graph::{GraphStore, Node};
use engram_server::services::{business_member_origins, business_outcome_dependencies as outcome};

const BODY: &str = "Public Function Accept(region As String) As Boolean\n  Return Not denied.Contains(region)\n End Function";
fn source(setting: &str) -> String {
    format!(
        "Public Class Rules\n Private denied As String\n Public Sub New()\n  denied = Settings.GetText(\"{setting}\")\n End Sub\n {BODY}\nEnd Class\n"
    )
}
fn publish(graph: &GraphStore, source: &str) -> Result<()> {
    graph.upsert_nodes("owned", &[Node {
        node_id: "file:Rules.vb".into(), node_type: "file".into(), name: "Rules.vb".into(),
        namespace: "memory".into(), language: "vb".into(), file_path: RelPath::new("Rules.vb"),
        start_line: 1, end_line: source.lines().count() as u32, generation: 1,
        metadata: Some(serde_json::json!({"file_hash":blake3::hash(source.as_bytes()).to_hex().to_string()})),
    }])?;
    Ok(())
}

#[test]
fn method_only_analysis_retains_configuration_origin_and_detects_changed_constructor() -> Result<()>
{
    // The directory owner outlives the database, including assertion unwinding.
    let owned = tempfile::tempdir()?;
    let graph = GraphStore::open(&owned.path().join("graph.redb"))?;
    let original = source("blocked_regions");
    std::fs::write(owned.path().join("Rules.vb"), &original)?;
    publish(&graph, &original)?;
    let context = outcome::SourceContext {
        graph: &graph,
        project_id: "owned",
        root: owned.path(),
    };
    let mut evidence = outcome::collect(BODY, "Rules", "vb", 6, "Rules.vb", Some(&context));
    ensure!(
        evidence.dependencies.is_empty(),
        "constructor context must not require an immediate-return dependency"
    );
    let origins = evidence.member_origins.as_ref().unwrap();
    ensure!(origins.observations.len() == 1);
    ensure!(origins.observations[0].line == 4);
    ensure!(origins.observations[0].source == original.lines().nth(3).unwrap());
    ensure!(evidence.prompt_context().contains("blocked_regions"));
    ensure!(
        evidence
            .prompt_context()
            .contains("No compiler field binding")
    );
    evidence.fingerprint(BODY, "owned-contract");
    let prior_fingerprint = evidence.analysis_fingerprint.clone();
    let stored: outcome::OutcomeEvidence =
        serde_json::from_str(&serde_json::to_string(&evidence)?)?;
    ensure!(stored.prompt_context().contains("blocked_regions"));

    let changed = source("disabled_regions");
    std::fs::write(owned.path().join("Rules.vb"), &changed)?;
    let changed_hash = blake3::hash(changed.as_bytes()).to_hex().to_string();
    ensure!(
        business_member_origins::qualification(stored.member_origins.as_ref(), Some(&changed_hash))
            .contains("STALE_SUPPORTING_SOURCE")
    );
    let refused = outcome::collect(BODY, "Rules", "vb", 6, "Rules.vb", Some(&context));
    ensure!(
        refused
            .member_origins
            .as_ref()
            .unwrap()
            .observations
            .is_empty()
    );
    ensure!(
        !refused.prompt_context().contains("disabled_regions"),
        "changed source must not bypass the indexed fingerprint"
    );
    publish(&graph, &changed)?;
    let mut refreshed = outcome::collect(BODY, "Rules", "vb", 6, "Rules.vb", Some(&context));
    refreshed.fingerprint(BODY, "owned-contract");
    ensure!(refreshed.analysis_fingerprint != prior_fingerprint);
    ensure!(refreshed.prompt_context().contains("disabled_regions"));
    Ok(())
}

#[test]
fn overloaded_constructor_observations_stay_separate_from_unrelated_names() -> Result<()> {
    let text = "Class Rules\n Sub New()\n  denied = Settings.GetText(\"first\")\n End Sub\n Sub New(mode As Boolean)\n  Me.denied = Settings.GetText(\"second\")\n End Sub\n Function Accept() As Boolean\n  ' unrelated\n  Return denied.Contains(\"unrelated\")\n End Function\nEnd Class\nClass Other\n Sub New()\n  denied = Settings.GetText(\"wrong_owner\")\n End Sub\nEnd Class\n";
    let body = "Function Accept() As Boolean\n  ' unrelated\n  Return denied.Contains(\"unrelated\")\n End Function";
    let context = business_member_origins::collect("Rules.vb", text, body, "Rules", 8);
    ensure!(context.observations.len() == 2);
    ensure!(context.observations[0].constructor_line == 2);
    ensure!(context.observations[1].constructor_line == 5);
    ensure!(!context.prompt_context().contains("wrong_owner"));
    ensure!(context.interpretation.contains("constructor selection"));
    Ok(())
}
