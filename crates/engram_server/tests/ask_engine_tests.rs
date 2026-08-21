#![allow(clippy::unwrap_used)]
//! ask_engine (ask_codebase brain, Milestone 1) — unit + integration tests.
//! Tasks append their own tests below their section marker.

// ─── Task 1: typed model ─────────────────────────────────────────────────────
use engram_server::services::ask_engine::evidence::{Authority, EvidenceItem, EvidenceKind};

#[test]
fn authority_orders_strongest_first_and_weight_is_monotonic() {
    // Declaration order = Ord order: RuntimeEvidence is the smallest (strongest).
    assert!(Authority::RuntimeEvidence < Authority::CurrentCode);
    assert!(Authority::CurrentCode < Authority::SemanticSimilarity);
    // weight() is the inverse: strongest authority => highest weight.
    assert!(Authority::CurrentCode.weight() > Authority::SemanticSimilarity.weight());
    assert!(Authority::RuntimeEvidence.weight() >= Authority::CurrentCode.weight());
}

#[test]
fn evidence_item_serializes_with_snake_case_kind() {
    let ev = EvidenceItem {
        evidence_id: "ev_1".into(),
        kind: EvidenceKind::SourceCode,
        authority: Authority::CurrentCode,
        path: Some("a.vb".into()),
        lines: Some((10, 20)),
        symbol_id: None,
        title: None,
        content: "x".into(),
        generation: Some(3),
        commit: None,
        timestamp: None,
        confidence: 0.9,
        relevance: 0.8,
        extraction_method: "fts".into(),
        warnings: vec![],
        provider: "code".into(),
        score: None,
        directness: None,
    };
    let j = serde_json::to_value(&ev).unwrap();
    assert_eq!(j["kind"], "source_code");
    assert_eq!(j["authority"], "current_code");
    assert_eq!(j["evidence_id"], "ev_1");
    // skip_serializing_if hides empty warnings + None score.
    assert!(j.get("warnings").is_none());
    assert!(j.get("score").is_none());
}

// ─── Task 2: deterministic multi-intent planner ──────────────────────────────
use engram_server::services::ask_engine::plan::{EntityKind, Intent};
use engram_server::services::ask_engine::planner::{extract_entities, plan_query};

#[test]
fn compound_question_yields_multiple_intents() {
    let p = plan_query("How does authentication work, and what would break if we changed it?");
    let intents: Vec<Intent> = p.intents.iter().map(|(i, _)| *i).collect();
    assert!(intents.contains(&Intent::Explain), "{intents:?}");
    assert!(intents.contains(&Intent::Impact), "{intents:?}");
}

#[test]
fn extracts_file_entity_with_kind() {
    let ents = extract_entities(
        "What breaks if we change marker serialization in ImportService.vb from XML to JSON?",
    );
    assert!(
        ents.iter()
            .any(|e| e.text == "ImportService.vb" && e.guessed_kind == EntityKind::File),
        "{ents:?}"
    );
}

#[test]
fn why_is_rationale_not_history() {
    let p = plan_query("Why is customer status enforced on the server?");
    assert_eq!(p.intents.first().unwrap().0, Intent::Rationale);
}

#[test]
fn bare_topic_defaults_to_explain() {
    let p = plan_query("marker clustering");
    assert_eq!(p.intents.first().unwrap().0, Intent::Explain);
}

#[test]
fn change_verb_qualifier_from_x_to_y() {
    let p = plan_query("change serialization from XML to JSON");
    assert_eq!(p.qualifiers.change, Some(("XML".into(), "JSON".into())));
}

#[test]
fn captures_single_pascalcase_symbol_but_not_question_words() {
    let ents = extract_entities("How does Authenticate work and where is Run used?");
    assert!(ents.iter().any(|e| e.text == "Authenticate"), "{ents:?}");
    assert!(ents.iter().any(|e| e.text == "Run"), "{ents:?}");
    assert!(!ents.iter().any(|e| e.text.eq_ignore_ascii_case("How")), "{ents:?}");
}

// ─── Task 3: entity resolver ─────────────────────────────────────────────────
use engram_core::RelPath;
use engram_core::config::Config;
use engram_graph::Node;
use engram_server::services::ask_engine::resolver::resolve_entities;
use engram_server::state::AppState;

/// Build an fts_only AppState with a registered project and seed graph nodes
/// directly (no indexing). Returns the TempDir guard (keep it alive), state, pid.
fn seed_project(nodes: &[(&str, &str, &str, &str)]) -> (tempfile::TempDir, AppState, String) {
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
    let pid = "ask-engine-test".to_string();
    let rec = engram_core::ProjectRecord {
        project_id: pid.clone(),
        project_name: pid.clone(),
        directory: project_dir.to_string_lossy().into_owned(),
        project_type: "general".into(),
        created_at_ms: 0,
        updated_at_ms: 0,
        reindex_required_since_ms: None,
    };
    state.registry.put_project(&rec).unwrap();
    state
        .registry
        .set_meta(&pid, "active_generation", "1")
        .unwrap();
    let gnodes: Vec<Node> = nodes
        .iter()
        .map(|(id, ty, name, file)| Node {
            node_id: (*id).into(),
            node_type: (*ty).into(),
            name: (*name).into(),
            namespace: "test".into(),
            language: "vbnet".into(),
            file_path: RelPath::new(file),
            start_line: 1,
            end_line: 10,
            generation: 1,
            metadata: None,
        })
        .collect();
    state.graph.upsert_nodes(&pid, &gnodes).unwrap();
    (tmp, state, pid)
}

#[test]
fn resolves_unique_symbol_and_marks_ambiguous() {
    let (_tmp, state, pid) = seed_project(&[
        ("sym:SaveMarker@a.vb", "function", "SaveMarker", "a.vb"),
        ("sym:SaveMarker@b.vb", "function", "SaveMarker", "b.vb"),
        ("sym:Run@import.vb", "function", "Run", "import.vb"),
    ]);
    let mut plan = plan_query("where is SaveMarker used and how does Run work?");
    resolve_entities(&state.graph, &pid, &mut plan);

    let sm = plan.entities.iter().find(|e| e.text == "SaveMarker").unwrap();
    assert_eq!(sm.resolved.len(), 2, "SaveMarker ambiguous: {:?}", sm.resolved);
    let run = plan.entities.iter().find(|e| e.text == "Run").unwrap();
    assert!(
        run.resolved.iter().any(|r| r.node_id.is_some()),
        "Run resolved: {:?}",
        run.resolved
    );
    assert_eq!(run.resolved.len(), 1, "Run is unique");
}

// ─── Task 4: search-backed evidence providers ────────────────────────────────
use engram_server::services::ask_engine::providers;
use engram_server::services::ask_engine::status::ProviderStatus;
use engram_server::services::project_service::{ensure_project_runtime, get_active_generation};
use rmcp::handler::server::tool::Parameters;

/// Build an fts_only AppState, write + index the given files, return the guard,
/// state, and project_id. General project_type indexes .vb/.cs/etc. lexically
/// without needing a language sidecar.
async fn index_fixture(files: &[(&str, &str)]) -> (tempfile::TempDir, AppState, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let proj = tmp.path().join("project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&proj).unwrap();
    for (name, content) in files {
        let p = proj.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, content).unwrap();
    }
    let cfg = Config {
        data_dir,
        allowed_roots: vec![proj.clone()],
        max_project_files: Some(100),
        max_project_bytes: Some(1 << 20),
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: proj.to_string_lossy().to_string(),
            project_name: "askfix".into(),
            project_type: engram_server::models::ProjectType::General,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0].project_id.clone();
    (tmp, state, pid)
}

#[tokio::test]
async fn code_evidence_returns_typed_items_with_lines() {
    let (_tmp, state, pid) = index_fixture(&[(
        "Auth.vb",
        "Public Function Authenticate(user As String) As Boolean\n  Return True\nEnd Function\n",
    )])
    .await;
    let ps = ensure_project_runtime(&state, &pid).await.unwrap();
    let gen_ = get_active_generation(&state, &pid).await.unwrap();
    let mut id = 0usize;
    let cancel = tokio_util::sync::CancellationToken::new();
    let (items, outcome) =
        providers::code_evidence(&ps.search, &pid, gen_, "Authenticate", 5, &cancel, &mut id).await;
    assert_eq!(outcome.status, ProviderStatus::Hit, "note: {:?}", outcome.note);
    assert!(!items.is_empty());
    assert_eq!(items[0].kind, EvidenceKind::SourceCode);
    assert_eq!(items[0].authority, Authority::CurrentCode);
    assert_eq!(items[0].path.as_deref(), Some("Auth.vb"));
    assert!(items[0].lines.is_some());
    assert!(items[0].evidence_id.starts_with("ev_"));
}

#[tokio::test]
async fn code_evidence_empty_is_not_failed() {
    let (_tmp, state, pid) = index_fixture(&[("Auth.vb", "Public Sub Noop()\nEnd Sub\n")]).await;
    let ps = ensure_project_runtime(&state, &pid).await.unwrap();
    let gen_ = get_active_generation(&state, &pid).await.unwrap();
    let mut id = 0usize;
    let cancel = tokio_util::sync::CancellationToken::new();
    let (items, outcome) = providers::code_evidence(
        &ps.search,
        &pid,
        gen_,
        "zzznothingmatcheszzz",
        5,
        &cancel,
        &mut id,
    )
    .await;
    assert!(items.is_empty());
    assert_eq!(outcome.status, ProviderStatus::Empty); // NOT Failed
}
