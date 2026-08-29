#![allow(clippy::unwrap_used)]
//! Row-4 audit (docs/audits/04-concept-and-consumer-discovery.md) A4/D5:
//! concept morphology was English-only (`'s'`, `ies→y`, `es`); a Swedish
//! plural / definite concept ("redovisningskategorier", "projekten") only
//! matched when the singular happened to be a literal prefix of the query.
//! On a Swedish codebase (VB.NET first-class, Swedish domain) the concept
//! footprint must find the identifier in the other form — behaviourally,
//! through `get_concept_footprint`, not by inspecting the stem list.

use engram_core::RelPath;
use engram_core::config::Config;
use engram_graph::Node;
use engram_server::models::GetConceptFootprintRequest;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use serde_json::json;

const PID: &str = "footprint-swedish-test";

fn build_state() -> (tempfile::TempDir, AppState) {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();
    let cfg = Config {
        data_dir,
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 1,
        ..Default::default()
    };
    let (state, _rx) = AppState::new(cfg).unwrap();
    state
        .registry
        .put_project(&engram_core::ProjectRecord {
            project_id: PID.into(),
            project_name: PID.into(),
            directory: project_dir.to_string_lossy().into_owned(),
            project_type: "dotnet_webforms_vb".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            reindex_required_since_ms: None,
        })
        .unwrap();
    state
        .registry
        .set_meta(PID, "active_generation", "1")
        .unwrap();
    (tmp, state)
}

fn table(name: &str) -> Node {
    Node {
        node_id: format!("db:table:{name}"),
        node_type: "db_table".into(),
        name: name.into(),
        namespace: "dbo".into(),
        language: "sql".into(),
        file_path: RelPath::new("Site/App_Code/db.dbml"),
        start_line: 1,
        end_line: 1,
        generation: 1,
        metadata: None,
    }
}

fn class(name: &str) -> Node {
    Node {
        node_id: format!("sym:class:Site/App_Code/gd/{name}.vb:{name}:1"),
        node_type: "class".into(),
        name: name.into(),
        namespace: "gd".into(),
        language: "vbnet".into(),
        file_path: RelPath::new(&format!("Site/App_Code/gd/{name}.vb")),
        start_line: 1,
        end_line: 40,
        generation: 1,
        metadata: None,
    }
}

async fn footprint(engram: &Engram, concept: &str) -> String {
    let req: GetConceptFootprintRequest =
        serde_json::from_value(json!({"project_id": PID, "concept": concept})).unwrap();
    let res = engram.handle_get_concept_footprint(req).await.unwrap();
    res.content[0].as_text().unwrap().text.clone()
}

fn engram_with(nodes: &[Node]) -> (tempfile::TempDir, Engram) {
    let (tmp, state) = build_state();
    state.graph.upsert_nodes(PID, nodes).unwrap();
    (tmp, Engram::new(state))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_swedish_plural_concept_finds_the_singular_identifier() {
    let (_tmp, engram) = engram_with(&[
        table("redovisningskategori"),
        class("InstallationsObjekt"),
        table("projekt"),
    ]);
    // Assert on the rendered node line, never on the name alone — the
    // header echoes the concept, which contains the singular as a substring.
    for (concept, expect) in [
        (
            "redovisningskategorier",
            "node_id=db:table:redovisningskategori ",
        ),
        (
            "installationsobjekten",
            "node_id=sym:class:Site/App_Code/gd/InstallationsObjekt.vb:InstallationsObjekt:1",
        ),
        ("projekten", "node_id=db:table:projekt "),
    ] {
        let out = footprint(&engram, concept).await;
        assert!(
            out.contains(expect),
            "concept `{concept}` must reach `{expect}` (Swedish plural/definite → base), got:\n{out}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_swedish_definite_singular_finds_the_bare_identifier() {
    let (_tmp, engram) = engram_with(&[table("objekt"), table("kategori")]);
    for (concept, expect) in [
        ("objektet", "node_id=db:table:objekt "),
        ("kategorin", "node_id=db:table:kategori "),
    ] {
        let out = footprint(&engram, concept).await;
        assert!(
            out.contains(expect),
            "concept `{concept}` must reach `{expect}`, got:\n{out}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn short_bases_are_never_produced() {
    // "order" → "ord" would over-match everything starting with "ord";
    // the base must stay ≥ 4 characters.
    let (_tmp, engram) = engram_with(&[table("ordning"), table("order")]);
    let out = footprint(&engram, "order").await;
    assert!(
        out.contains("node_id=db:table:order "),
        "the literal still matches:\n{out}"
    );
    assert!(
        !out.contains("node_id=db:table:ordning"),
        "no 3-char stem may be derived (`ord` would match `ordning`):\n{out}"
    );
}
