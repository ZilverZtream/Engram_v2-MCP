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
    assert!(
        !ents.iter().any(|e| e.text.eq_ignore_ascii_case("How")),
        "{ents:?}"
    );
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

    let sm = plan
        .entities
        .iter()
        .find(|e| e.text == "SaveMarker")
        .unwrap();
    assert_eq!(
        sm.resolved.len(),
        2,
        "SaveMarker ambiguous: {:?}",
        sm.resolved
    );
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
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
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
    assert_eq!(
        outcome.status,
        ProviderStatus::Hit,
        "note: {:?}",
        outcome.note
    );
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

// ─── Task 5: graph-backed evidence providers ─────────────────────────────────
use engram_graph::{Edge, EdgeKind};

fn seed_project_with_edges(
    nodes: &[(&str, &str, &str, &str)],
    edges: &[(&str, &str, EdgeKind, u32)],
) -> (tempfile::TempDir, AppState, String) {
    let (tmp, state, pid) = seed_project(nodes);
    let gedges: Vec<Edge> = edges
        .iter()
        .map(|(s, t, k, w)| Edge {
            source_id: (*s).into(),
            target_id: (*t).into(),
            namespace: "test".into(),
            language: "vbnet".into(),
            edge_kind: k.clone(),
            weight: *w,
            generation: 1,
            metadata: None,
            updated_at_ms: 0,
        })
        .collect();
    state.graph.upsert_edges(&pid, &gedges).unwrap();
    (tmp, state, pid)
}

#[test]
fn impact_evidence_surfaces_incoming_callers_as_graph_relations() {
    let (_tmp, state, pid) = seed_project_with_edges(
        &[
            ("sym:Save@a.vb", "function", "Save", "a.vb"),
            ("sym:Caller@b.vb", "function", "Caller", "b.vb"),
        ],
        &[("sym:Caller@b.vb", "sym:Save@a.vb", EdgeKind::Calls, 3)],
    );
    let mut id = 0usize;
    let (items, outcome) =
        providers::impact_evidence(&state.graph, &pid, "sym:Save@a.vb", 50, &mut id);
    assert_eq!(outcome.status, ProviderStatus::Hit);
    assert!(
        items.iter().any(|e| e.kind == EvidenceKind::GraphRelation
            && e.symbol_id.as_deref() == Some("sym:Caller@b.vb")
            && e.authority == Authority::CurrentCode
            && e.directness == Some(0.9)),
        "{items:?}"
    );
}

#[test]
fn symbol_ref_evidence_finds_usages() {
    let (_tmp, state, pid) = seed_project_with_edges(
        &[
            ("sym:Widget@w.vb", "function", "Widget", "w.vb"),
            ("sym:Uses@u.vb", "function", "Uses", "u.vb"),
        ],
        &[("sym:Uses@u.vb", "sym:Widget@w.vb", EdgeKind::Calls, 1)],
    );
    let mut id = 0usize;
    let (items, outcome) =
        providers::symbol_ref_evidence(&state.graph, &pid, "Widget", None, 25, &mut id);
    assert_eq!(outcome.status, ProviderStatus::Hit, "{items:?}");
    assert!(
        items
            .iter()
            .any(|e| e.symbol_id.as_deref() == Some("sym:Uses@u.vb"))
    );
}

// ─── Task 6: parallel intent-DAG retrieval ───────────────────────────────────
use engram_server::services::ask_engine::retrieval::{Depth, RetrievalCtx, gather_evidence};
use std::time::Duration;

fn func_node(id: &str, name: &str, file: &str) -> Node {
    Node {
        node_id: id.into(),
        node_type: "function".into(),
        name: name.into(),
        namespace: "test".into(),
        language: "vbnet".into(),
        file_path: RelPath::new(file),
        start_line: 1,
        end_line: 3,
        generation: 1,
        metadata: None,
    }
}

#[tokio::test]
async fn gather_runs_arms_for_intents_and_reports_per_provider() {
    let (_tmp, state, pid) = index_fixture(&[(
        "Save.vb",
        "Public Function Save(id As Integer) As Boolean\n Return True\nEnd Function\n",
    )])
    .await;
    // Seed a caller edge into the indexed project's graph.
    state
        .graph
        .upsert_nodes(
            &pid,
            &[
                func_node("sym:Save@Save.vb", "Save", "Save.vb"),
                func_node("sym:Caller@c.vb", "Caller", "c.vb"),
            ],
        )
        .unwrap();
    state
        .graph
        .upsert_edges(
            &pid,
            &[Edge {
                source_id: "sym:Caller@c.vb".into(),
                target_id: "sym:Save@Save.vb".into(),
                namespace: "test".into(),
                language: "vbnet".into(),
                edge_kind: EdgeKind::Calls,
                weight: 2,
                generation: 1,
                metadata: None,
                updated_at_ms: 0,
            }],
        )
        .unwrap();

    let ps = ensure_project_runtime(&state, &pid).await.unwrap();
    let gen_ = get_active_generation(&state, &pid).await.unwrap();
    let mut plan = plan_query("what breaks if I change Save?");
    resolve_entities(&state.graph, &pid, &mut plan);

    let ctx = RetrievalCtx {
        search: ps.search.clone(),
        graph: state.graph.clone(),
        registry: state.registry.clone(),
        project_id: pid.clone(),
        generation: gen_,
    };
    let (evidence, reports) = gather_evidence(
        &ctx,
        &plan,
        "what breaks if I change Save?",
        Depth::Standard,
        Duration::from_secs(10),
        tokio_util::sync::CancellationToken::new(),
    )
    .await;

    // Impact intent → the impact arm ran and surfaced the caller.
    assert!(
        reports
            .iter()
            .any(|r| r.provider == "impact" && r.status == ProviderStatus::Hit),
        "reports: {reports:?}"
    );
    assert!(
        evidence
            .iter()
            .any(|e| e.symbol_id.as_deref() == Some("sym:Caller@c.vb")),
        "evidence: {evidence:?}"
    );
    // A code arm always runs; its report is present (Hit or Empty, never missing).
    assert!(
        reports.iter().any(|r| r.provider == "code"),
        "reports: {reports:?}"
    );
    // Evidence ids are globally sequential.
    assert!(evidence.iter().all(|e| e.evidence_id.starts_with("ev_")));
}

// ─── Task 7: ranking + conflict detection ────────────────────────────────────
use engram_server::services::ask_engine::ranking::{detect_conflicts, rank_and_select};

#[allow(clippy::too_many_arguments)]
fn mk_ev(
    id: &str,
    kind: EvidenceKind,
    authority: Authority,
    path: Option<&str>,
    lines: Option<(u32, u32)>,
    symbol_id: Option<&str>,
    title: Option<&str>,
    content: &str,
    relevance: f32,
) -> EvidenceItem {
    EvidenceItem {
        evidence_id: id.into(),
        kind,
        authority,
        path: path.map(|s| s.into()),
        lines,
        symbol_id: symbol_id.map(|s| s.into()),
        title: title.map(|s| s.into()),
        content: content.into(),
        generation: None,
        commit: None,
        timestamp: None,
        confidence: 0.8,
        relevance,
        extraction_method: "test".into(),
        warnings: vec![],
        provider: "test".into(),
        score: None,
        directness: None,
    }
}

#[test]
fn one_direct_code_relation_outranks_ten_weak_semantic_hits() {
    let mut items = vec![mk_ev(
        "ev_x",
        EvidenceKind::GraphRelation,
        Authority::CurrentCode,
        Some("svc.vb"),
        Some((10, 20)),
        Some("sym:x"),
        None,
        "Caller calls Save",
        0.3,
    )];
    for i in 0..10 {
        items.push(mk_ev(
            &format!("ev_{i}"),
            EvidenceKind::SourceCode,
            Authority::SemanticSimilarity,
            Some(&format!("f{i}.vb")),
            Some((1, 2)),
            None,
            None,
            "vaguely related text",
            0.6,
        ));
    }
    let ranked = rank_and_select(items, 3);
    assert_eq!(ranked[0].evidence_id, "ev_x", "ranked: {ranked:?}");
}

#[test]
fn dedup_collapses_same_path_and_lines() {
    let ranked = rank_and_select(
        vec![
            mk_ev(
                "ev_1",
                EvidenceKind::SourceCode,
                Authority::CurrentCode,
                Some("a.vb"),
                Some((10, 20)),
                None,
                None,
                "x",
                0.5,
            ),
            mk_ev(
                "ev_2",
                EvidenceKind::SourceCode,
                Authority::CurrentCode,
                Some("a.vb"),
                Some((10, 20)),
                None,
                None,
                "x",
                0.9,
            ),
        ],
        5,
    );
    assert_eq!(ranked.len(), 1);
}

#[test]
fn requirement_contradicting_code_is_flagged() {
    let items = vec![
        mk_ev(
            "ev_req",
            EvidenceKind::MemoryNote,
            Authority::ApprovedRequirement,
            None,
            None,
            None,
            Some("Access rule"),
            "Tenant admins must be rejected before import",
            0.5,
        ),
        mk_ev(
            "ev_code",
            EvidenceKind::SourceCode,
            Authority::CurrentCode,
            Some("Import.vb"),
            Some((1, 5)),
            Some("sym:imp"),
            None,
            "tenant admins are allowed to import",
            0.5,
        ),
    ];
    let conflicts = detect_conflicts(&items, 3);
    assert!(
        conflicts.iter().any(|c| c.kind == "authority_disagreement"),
        "conflicts: {conflicts:?}"
    );
}

// ─── Task 8: freshness snapshot + honest status ──────────────────────────────
use engram_server::services::ask_engine::plan::ResolvedEntity;
use engram_server::services::ask_engine::status::{
    AnswerStatus, FreshnessSnapshot, ProviderReport, assess_status, has_adequate_support,
};

fn rep(p: &str, s: ProviderStatus) -> ProviderReport {
    ProviderReport {
        provider: p.into(),
        status: s,
        count: if s == ProviderStatus::Hit { 1 } else { 0 },
        note: None,
    }
}
fn re(id: &str) -> ResolvedEntity {
    ResolvedEntity {
        kind: EntityKind::Symbol,
        canonical: id.into(),
        node_id: Some(format!("sym:{id}")),
        confidence: 0.5,
    }
}

#[test]
fn empty_everything_is_unsupported_not_answered() {
    let plan = plan_query("how does frobnicate work");
    let s = assess_status(
        &plan,
        &[],
        &[
            rep("code", ProviderStatus::Empty),
            rep("doc", ProviderStatus::Empty),
        ],
        &FreshnessSnapshot::default(),
        false,
    );
    assert_eq!(s, AnswerStatus::Unsupported);
}

#[test]
fn all_failed_providers_is_failed() {
    let plan = plan_query("frobnicate the thing");
    let s = assess_status(
        &plan,
        &[],
        &[rep("code", ProviderStatus::Failed)],
        &FreshnessSnapshot::default(),
        false,
    );
    assert_eq!(s, AnswerStatus::Failed);
}

#[test]
fn ambiguous_entity_yields_ambiguous_status() {
    let mut plan = plan_query("where is SaveMarker used");
    for e in plan.entities.iter_mut() {
        if e.text == "SaveMarker" {
            e.resolved = vec![re("a"), re("b")];
        }
    }
    let ev = vec![mk_ev(
        "ev_1",
        EvidenceKind::GraphRelation,
        Authority::CurrentCode,
        Some("a.vb"),
        Some((1, 2)),
        Some("sym:a"),
        None,
        "x",
        0.5,
    )];
    let s = assess_status(
        &plan,
        &ev,
        &[rep("usage", ProviderStatus::Hit)],
        &FreshnessSnapshot::default(),
        true,
    );
    assert_eq!(s, AnswerStatus::Ambiguous);
}

// ─── Task 9: request envelope ────────────────────────────────────────────────
#[test]
fn legacy_request_still_deserializes() {
    let r: engram_server::AskCodebaseRequest =
        serde_json::from_str(r#"{"project_id":"p","question":"q"}"#).unwrap();
    assert_eq!(r.depth, "standard");
    assert_eq!(r.output_format, "markdown");
    assert_eq!(r.freshness_policy, "best_effort");
    assert!(r.as_of.is_none());
    assert!(r.session_id.is_none());
    assert!(r.deadline_ms.is_none());
}

#[test]
fn full_envelope_deserializes() {
    let r: engram_server::AskCodebaseRequest = serde_json::from_str(
        r#"{
          "project_id":"p","question":"q","session_id":"s","task_context":"t",
          "as_of":{"branch":"main"},"audience":{"role":"developer","permissions":[]},
          "depth":"deep","freshness_policy":"require_current","output_format":"both","deadline_ms":15000
        }"#,
    )
    .unwrap();
    assert_eq!(r.depth, "deep");
    assert_eq!(r.as_of.unwrap().branch.as_deref(), Some("main"));
    assert_eq!(r.deadline_ms, Some(15000));
    assert_eq!(r.output_format, "both");
}

// ─── Task 10: end-to-end orchestrator ────────────────────────────────────────
#[tokio::test]
async fn ask_returns_typed_report_with_citations_on_indexed_project() {
    let (_tmp, state, pid) = index_fixture(&[(
        "Auth.vb",
        "Public Function Authenticate() As Boolean\n Return True\nEnd Function\n",
    )])
    .await;
    let engram = engram_server::Engram::new(state.clone());
    let res = engram
        .handle_ask_codebase(engram_server::AskCodebaseRequest {
            project_id: pid,
            question: "how does Authenticate work?".into(),
            output_format: "both".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let out = res.content[0].as_text().unwrap().text.clone();
    assert!(out.contains("retrieval_only"), "{out}");
    assert!(out.contains("Auth.vb"), "citation missing:\n{out}");
    assert!(out.to_lowercase().contains("status:"), "{out}");
    // Not the old concatenation format.
    assert!(!out.contains("#1"), "old concat format leaked:\n{out}");
}

#[tokio::test]
async fn ask_abstains_when_knowledge_is_absent() {
    let (_tmp, state, pid) = index_fixture(&[("a.vb", "Public Sub Noop()\nEnd Sub\n")]).await;
    let engram = engram_server::Engram::new(state.clone());
    let res = engram
        .handle_ask_codebase(engram_server::AskCodebaseRequest {
            project_id: pid,
            question: "what is the flux capacitor calibration policy?".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let out = res.content[0].as_text().unwrap().text.clone();
    assert!(
        out.to_lowercase().contains("unsupported"),
        "should abstain, not fabricate:\n{out}"
    );
}

// ─── Review fixes: anti-anchoring + determinism regressions ──────────────────
#[test]
fn provider_directness_preserved_so_cochange_does_not_outrank_direct() {
    // #1: the ranker must keep a provider's directness (companion=0.5), not
    // recompute 0.9 from kind, else a co-change correlation outranks a real edge.
    let mut companion = mk_ev(
        "ev_comp",
        EvidenceKind::GraphRelation,
        Authority::MergedHistory,
        Some("far.vb"),
        Some((1, 2)),
        Some("sym:far"),
        None,
        "co-changes with X 12 times",
        0.15,
    );
    companion.directness = Some(0.5);
    let mut direct = mk_ev(
        "ev_dir",
        EvidenceKind::GraphRelation,
        Authority::CurrentCode,
        Some("svc.vb"),
        Some((10, 20)),
        Some("sym:dir"),
        None,
        "Caller calls X",
        0.03,
    );
    direct.directness = Some(0.9);
    let ranked = rank_and_select(vec![companion, direct], 5);
    assert_eq!(
        ranked[0].evidence_id, "ev_dir",
        "a direct relation must outrank co-change: {ranked:?}"
    );
}

#[test]
fn distinct_same_name_callers_are_kept() {
    // #4: two callers named Page_Load in different files are distinct evidence.
    let a = mk_ev(
        "ev_a",
        EvidenceKind::GraphRelation,
        Authority::CurrentCode,
        Some("f1.vb"),
        Some((10, 12)),
        Some("sym:a"),
        Some("Page_Load"),
        "x",
        0.03,
    );
    let b = mk_ev(
        "ev_b",
        EvidenceKind::GraphRelation,
        Authority::CurrentCode,
        Some("f2.vb"),
        Some((20, 22)),
        Some("sym:b"),
        Some("Page_Load"),
        "y",
        0.03,
    );
    let ranked = rank_and_select(vec![a, b], 5);
    assert_eq!(
        ranked.len(),
        2,
        "distinct callers sharing a name must both survive: {ranked:?}"
    );
}

#[test]
fn ranking_tie_break_is_deterministic() {
    // #6: identical-score items order stably by id (ascending).
    let build = || {
        vec![
            mk_ev(
                "ev_z",
                EvidenceKind::GraphRelation,
                Authority::CurrentCode,
                Some("z.vb"),
                Some((1, 2)),
                Some("sym:z"),
                None,
                "x",
                0.03,
            ),
            mk_ev(
                "ev_a",
                EvidenceKind::GraphRelation,
                Authority::CurrentCode,
                Some("a.vb"),
                Some((1, 2)),
                Some("sym:a"),
                None,
                "x",
                0.03,
            ),
        ]
    };
    let r1 = rank_and_select(build(), 5);
    let r2 = rank_and_select(build(), 5);
    assert_eq!(r1[0].evidence_id, "ev_a");
    let ids1: Vec<_> = r1.iter().map(|e| e.evidence_id.clone()).collect();
    let ids2: Vec<_> = r2.iter().map(|e| e.evidence_id.clone()).collect();
    assert_eq!(ids1, ids2, "ranking must be deterministic");
}

// ─── Live-eval fix: honest abstention on weak coincidental matches ───────────
#[test]
fn weak_single_term_match_abstains_not_partial() {
    // The OciusX eval showed nonsense questions returning partial: loose FTS
    // finds a coincidental keyword on a big codebase. A multi-term question
    // whose only evidence covers ONE query term must abstain (unsupported).
    let q = "what is the flux capacitor calibration policy";
    let plan = plan_query(q);
    let ev = vec![mk_ev(
        "ev_1",
        EvidenceKind::SourceCode,
        Authority::CurrentCode,
        Some("Settings.vb"),
        Some((1, 2)),
        None,
        None,
        "public policy setting for uploads",
        0.03,
    )];
    assert!(
        !has_adequate_support(q, &ev),
        "one coincidental term is not support"
    );
    let s = assess_status(
        &plan,
        &ev,
        &[rep("code", ProviderStatus::Hit)],
        &FreshnessSnapshot::default(),
        has_adequate_support(q, &ev),
    );
    assert_eq!(s, AnswerStatus::Unsupported);
}

#[test]
fn multi_term_match_is_supported_and_answered() {
    let q = "how does marker clustering work";
    let plan = plan_query(q);
    let ev = vec![mk_ev(
        "ev_1",
        EvidenceKind::SourceCode,
        Authority::CurrentCode,
        Some("Map.vb"),
        Some((1, 5)),
        None,
        None,
        "marker clustering groups nearby markers on the map",
        0.03,
    )];
    assert!(
        has_adequate_support(q, &ev),
        "a hit covering marker+clustering is adequate"
    );
    let s = assess_status(
        &plan,
        &ev,
        &[rep("code", ProviderStatus::Hit)],
        &FreshnessSnapshot::default(),
        true,
    );
    assert_eq!(s, AnswerStatus::Answered); // Explanation primary = SourceCode, present
}

#[test]
fn a_lone_concept_match_is_not_adequate_support() {
    // Single-stem concept match (a "…Policy" node) must NOT support a nonsense
    // multi-term question — the OciusX eval's second abstain bug.
    let q = "what is the flux capacitor calibration policy";
    let mut ev = vec![mk_ev(
        "ev_1",
        EvidenceKind::GraphRelation,
        Authority::CurrentCode,
        Some("UploadPolicy.vb"),
        Some((1, 2)),
        Some("sym:UploadPolicy"),
        Some("UploadPolicy"),
        "UploadPolicy (class)",
        0.03,
    )];
    ev[0].provider = "concept".into();
    assert!(
        !has_adequate_support(q, &ev),
        "a lone concept match is not support: {ev:?}"
    );
}

#[test]
fn resolved_entity_graph_relation_is_adequate_support() {
    let q = "what breaks if I change SaveMarker";
    let mut ev = vec![mk_ev(
        "ev_1",
        EvidenceKind::GraphRelation,
        Authority::CurrentCode,
        Some("a.vb"),
        Some((1, 2)),
        Some("sym:Caller"),
        Some("Caller"),
        "Caller calls SaveMarker",
        0.03,
    )];
    ev[0].provider = "impact".into();
    assert!(has_adequate_support(q, &ev));
}

#[test]
fn compound_terms_across_different_hits_are_adequate() {
    // "authentication" in one file + "changed" in another: the evidence SET
    // covers both distinctive terms, so it must NOT falsely abstain (the OciusX
    // compound_1/bug_1 finding where per-hit coverage wrongly returned unsupported).
    let q = "how does authentication work and what would break if we changed it";
    let ev = vec![
        mk_ev(
            "ev_1",
            EvidenceKind::SourceCode,
            Authority::CurrentCode,
            Some("Auth.vb"),
            Some((1, 3)),
            None,
            None,
            "Public Function Authenticate handles authentication",
            0.03,
        ),
        mk_ev(
            "ev_2",
            EvidenceKind::SourceCode,
            Authority::CurrentCode,
            Some("Grid.vb"),
            Some((1, 3)),
            None,
            None,
            "SelectedIndexChanged handler updates the grid",
            0.03,
        ),
    ];
    assert!(
        has_adequate_support(q, &ev),
        "the set covers authentication + changed"
    );
}

#[test]
fn unsupported_and_stale_guidance_recommends_grep_fallback() {
    // The OciusX finding: a stale/empty index made agents report "cannot
    // determine" instead of grepping the working tree. The tool's own guidance
    // must push the grep fallback so agents without a prompt fix still recover.
    use engram_server::services::ask_engine::report::next_best;
    let plan = plan_query("does the flux capacitor exist");
    let unsup = next_best(&plan, &[], AnswerStatus::Unsupported);
    assert!(
        unsup.iter().any(|s| s.contains("grep_project")),
        "{unsup:?}"
    );
    assert!(
        unsup
            .iter()
            .any(|s| s.to_lowercase().contains("cannot determine")),
        "{unsup:?}"
    );
    let stale = next_best(&plan, &[], AnswerStatus::Stale);
    assert!(
        stale.iter().any(|s| s.contains("grep_project")),
        "{stale:?}"
    );
}
