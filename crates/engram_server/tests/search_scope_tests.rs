#![allow(clippy::unwrap_used)]
//! `search_memory` must be able to recall across knowledge namespaces.
//!
//! Knowledge lives in several namespaces — `memory_bank`, `insights`,
//! `business_logic`, `antipattern`, `wontfix_patterns`, `quality_gate` — and
//! `search_memory` queried exactly one per call, defaulting to code. An agent
//! had to already know a memory existed, and which bucket it was in, to find
//! it. The `search_scope` param lets one search span all knowledge
//! namespaces, fused by rank and labelled by source.
//!
//! `search_scope: "code"` (the default) must be byte-for-byte the old
//! behaviour, so existing callers are untouched.

use engram_core::Config;
use engram_index::IndexDoc;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

async fn setup() -> (tempfile::TempDir, AppState, engram_server::Engram, String) {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("repo");
    std::fs::create_dir_all(root.join("src")).unwrap();
    // Code hit: contains "widget".
    std::fs::write(
        root.join("src/render.rs"),
        "pub fn render_widget() -> u8 {\n    // draws the widget\n    1\n}\n",
    )
    .unwrap();

    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: tmp.path().join("data"),
        max_project_files: Some(20),
        max_project_bytes: Some(256 * 1024),
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "ScopeTest".into(),
            project_type: engram_server::models::ProjectType::General,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();

    // memory_bank hit, via the real write path.
    engram
        .update_memory_bank(Parameters(engram_server::UpdateMemoryBankRequest {
            project_id: pid.clone(),
            section_id: Some("widget-cache".into()),
            section: "Widget cache".into(),
            content: "The widget cache warms lazily on first render.".into(),
            ..Default::default()
        }))
        .await
        .unwrap();

    (tmp, state, engram, pid)
}

/// Index a doc straight into a namespace (for `insights`, which has no public
/// write tool — it is produced by the dreamer).
async fn index_into_namespace(
    state: &AppState,
    pid: &str,
    namespace: &str,
    id: &str,
    content: &str,
    ts: u64,
) {
    let engine = state.get_project_cached(pid).unwrap().search;
    let doc = IndexDoc {
        generation: 0, // GlobalMutable knowledge namespaces are written at gen 0
        chunk_id: 0,
        doc_id: format!("{namespace}:{id}"),
        content_hash: format!("hash_{namespace}_{id}"),
        path: engram_core::RelPath::new(&format!("__{namespace}/{id}.md")),
        content: content.to_string(),
        language: "markdown".into(),
        namespace: namespace.to_string(),
        author: None,
        timestamp: Some(ts),
        start_line: 0,
        end_line: 0,
    };
    engine
        .index_docs(pid, &[doc], &CancellationToken::new())
        .await
        .unwrap();
}

fn req(pid: &str, scope: &str) -> engram_server::SearchMemoryRequest {
    engram_server::SearchMemoryRequest {
        project_id: pid.to_string(),
        query: "widget".into(),
        max_results: 20,
        semantic: false, // fts_only: exercise the lexical path deterministically
        search_scope: scope.to_string(),
        ..Default::default()
    }
}

async fn search(engram: &engram_server::Engram, r: engram_server::SearchMemoryRequest) -> String {
    let res = engram.search_memory(Parameters(r)).await.unwrap();
    res.content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The default scope is code-only and must not change.
#[tokio::test]
async fn default_scope_is_code_only_and_unlabelled() {
    let (_t, state, engram, pid) = setup().await;
    index_into_namespace(
        &state,
        &pid,
        "insights",
        "w1",
        "Insight: widget rendering clusters with layout.",
        1000,
    )
    .await;

    let out = search(&engram, req(&pid, "code")).await;
    assert!(out.contains("src/render.rs"), "code hit missing:\n{out}");
    assert!(
        !out.contains("memory_bank") && !out.contains("insights"),
        "code scope must not pull knowledge namespaces:\n{out}"
    );
    assert!(
        !out.contains("source:"),
        "code scope output must be unchanged — no source label:\n{out}"
    );
}

/// `knowledge` spans the curated namespaces and excludes code.
#[tokio::test]
async fn knowledge_scope_spans_memory_and_insights_and_labels_source() {
    let (_t, state, engram, pid) = setup().await;
    index_into_namespace(
        &state,
        &pid,
        "insights",
        "w1",
        "Insight: the widget cache is a hot path.",
        1000,
    )
    .await;

    let out = search(&engram, req(&pid, "knowledge")).await;
    assert!(
        out.contains("memory_bank:widget-cache"),
        "memory_bank hit missing:\n{out}"
    );
    assert!(
        out.contains("__insights/w1"),
        "insights hit missing:\n{out}"
    );
    assert!(
        !out.contains("src/render.rs"),
        "knowledge scope must not return code:\n{out}"
    );
    assert!(
        out.contains("source: memory_bank") && out.contains("source: insights"),
        "each hit must be labelled with its source namespace:\n{out}"
    );
}

/// `all` returns code and knowledge together.
#[tokio::test]
async fn all_scope_returns_code_and_knowledge() {
    let (_t, state, engram, pid) = setup().await;
    index_into_namespace(
        &state,
        &pid,
        "insights",
        "w1",
        "Insight: widget layout coupling.",
        1000,
    )
    .await;

    let out = search(&engram, req(&pid, "all")).await;
    assert!(out.contains("src/render.rs"), "code hit missing:\n{out}");
    assert!(
        out.contains("memory_bank:widget-cache"),
        "memory hit missing:\n{out}"
    );
    assert!(out.contains("__insights/w1"), "insight hit missing:\n{out}");
}

/// An unknown scope is rejected, not silently treated as code.
#[tokio::test]
async fn unknown_scope_is_rejected() {
    let (_t, _state, engram, pid) = setup().await;
    let err = engram
        .search_memory(Parameters(req(&pid, "everythingg")))
        .await
        .expect_err("an invalid scope must error");
    let msg = format!("{err}");
    assert!(
        msg.contains("search_scope") && msg.contains("code"),
        "the error must name the param and the valid values:\n{msg}"
    );
}

/// Date filters (already in the engine, never exposed) now reach the caller.
#[tokio::test]
async fn date_before_excludes_newer_knowledge() {
    let (_t, state, engram, pid) = setup().await;
    index_into_namespace(
        &state,
        &pid,
        "insights",
        "old",
        "Insight: widget cache from the past.",
        1_000,
    )
    .await;
    index_into_namespace(
        &state,
        &pid,
        "insights",
        "new",
        "Insight: widget cache from the future.",
        9_000,
    )
    .await;

    let mut r = req(&pid, "knowledge");
    r.date_before = Some(5_000);
    let out = search(&engram, r).await;
    assert!(
        out.contains("__insights/old"),
        "the older insight must survive:\n{out}"
    );
    assert!(
        !out.contains("__insights/new"),
        "date_before must exclude the newer insight:\n{out}"
    );
}

// Search identities are project/namespace/doc triples, not paths or doc IDs alone.
#[tokio::test]
async fn recovery_routes_colliding_namespace_documents_and_knowledge_has_no_code_symbols() {
    let (_tmp, state, engram, pid) = setup().await;
    let engine = state.get_project_cached(&pid).unwrap().search;
    for (ns, marker) in [
        ("memory_bank", "BANK_ONLY_PAYLOAD"),
        ("insights", "INSIGHT_ONLY_PAYLOAD"),
    ] {
        let content = format!("routeprobe {marker}");
        engine
            .index_docs(
                &pid,
                &[IndexDoc {
                    generation: 0,
                    chunk_id: 777,
                    doc_id: "shared-route-id".into(),
                    content_hash: engram_core::ContentHash::compute(content.as_bytes()).0,
                    path: engram_core::RelPath::new("src/render.rs"),
                    content,
                    language: "markdown".into(),
                    namespace: ns.into(),
                    author: None,
                    timestamp: None,
                    start_line: 1,
                    end_line: 4,
                }],
                &CancellationToken::new(),
            )
            .await
            .unwrap();
    }
    let mut request = req(&pid, "knowledge");
    request.query = "routeprobe".into();
    request.include_user_memory = false;
    let out = search(&engram, request).await;
    assert!(
        !out.contains("symbols:"),
        "Knowledge path collided with code symbols: {out}"
    );
    let mut namespaces = std::collections::BTreeSet::new();
    for line in out.lines().filter_map(|l| {
        l.strip_prefix("full_chunk: get_chunk(")
            .and_then(|s| s.strip_suffix(')'))
    }) {
        let request: engram_server::GetChunkRequest = serde_json::from_str(line).unwrap();
        assert_eq!(request.project_id, pid);
        assert_eq!(request.doc_id, "shared-route-id");
        let expected = match request.namespace.as_str() {
            "memory_bank" => "BANK_ONLY_PAYLOAD",
            "insights" => "INSIGHT_ONLY_PAYLOAD",
            other => panic!("wrong namespace {other}"),
        };
        namespaces.insert(request.namespace.clone());
        let result = engram.get_chunk(Parameters(request)).await.unwrap();
        let content = result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            content.contains(expected),
            "Recovery returned wrong document: {content}"
        );
    }
    assert_eq!(
        namespaces.len(),
        2,
        "Both colliding identities must be recoverable: {out}"
    );
    let code = search(&engram, req(&pid, "code")).await;
    assert!(
        code.contains("symbols:") && code.contains("render_widget"),
        "Real code symbol navigation should survive: {code}"
    );
}

#[tokio::test]
async fn code_hit_symbols_exclude_learning_nodes_at_identical_source_path() {
    let (_tmp, state, engram, pid) = setup().await;
    for (kind, name) in [
        ("search_document", "LEARNED_DOCUMENT_NOT_SYMBOL"),
        ("chunk", "LEARNED_CHUNK_NOT_SYMBOL"),
    ] {
        state
            .graph
            .upsert_nodes(
                &pid,
                &[engram_graph::Node {
                    node_id: format!("fixture:{kind}"),
                    node_type: kind.into(),
                    name: name.into(),
                    namespace: "memory".into(),
                    language: "rust".into(),
                    file_path: engram_core::RelPath::new("src/render.rs"),
                    start_line: 1,
                    end_line: 4,
                    generation: 0,
                    metadata: None,
                }],
            )
            .unwrap();
    }
    let out = search(&engram, req(&pid, "code")).await;
    assert!(out.contains("render_widget"), "Real symbol lost: {out}");
    assert!(
        !out.contains("LEARNED_DOCUMENT_NOT_SYMBOL") && !out.contains("LEARNED_CHUNK_NOT_SYMBOL"),
        "Learning nodes misrepresented as source symbols: {out}"
    );
}

#[tokio::test]
async fn explicit_single_knowledge_namespace_recovery_never_uses_default_memory() {
    let (_tmp, state, engram, pid) = setup().await;
    index_into_namespace(
        &state,
        &pid,
        "insights",
        "single-route",
        "singleroute unique content",
        0,
    )
    .await;
    let mut request = req(&pid, "code");
    request.namespace = "insights".into();
    request.query = "singleroute".into();
    request.include_user_memory = false;
    let out = search(&engram, request).await;
    let line = out
        .lines()
        .find_map(|l| {
            l.strip_prefix("full_chunk: get_chunk(")
                .and_then(|s| s.strip_suffix(')'))
        })
        .expect("Actual complete recovery arguments");
    let recovery: engram_server::GetChunkRequest = serde_json::from_str(line).unwrap();
    assert_eq!(recovery.namespace, "insights");
    assert_eq!(recovery.project_id, pid);
    assert!(engram.get_chunk(Parameters(recovery)).await.is_ok());
}

// Append to search_scope_tests.rs, reusing its real indexed generic fixture.
#[tokio::test]
async fn exact_source_symbol_hints_survive_more_than_200_namesake_nodes() {
    let (_tmp, state, engram, pid) = setup().await;
    let nodes: Vec<_> = (0..210)
        .map(|i| engram_graph::Node {
            // Node keys sort ahead of the fixture's indexed file/function nodes.
            node_id: format!("000-crowd:{i:03}"),
            node_type: "function".into(),
            name: format!("unrelated_{i}"),
            namespace: "memory".into(),
            language: "rust".into(),
            file_path: engram_core::RelPath::new(&format!("shadow/{i:03}/src/render.rs")),
            start_line: 1,
            end_line: 4,
            generation: 0,
            metadata: None,
        })
        .collect();
    state.graph.upsert_nodes(&pid, &nodes).unwrap();
    let substring = state
        .graph
        .query_nodes(&pid, None, None, Some("src/render.rs"), 200)
        .unwrap();
    assert_eq!(substring.len(), 200);
    assert!(
        substring
            .iter()
            .all(|n| n.file_path.as_str() != "src/render.rs")
    );
    let exact = state
        .graph
        .query_nodes_in_file(&pid, None, "src/render.rs", 200)
        .unwrap();
    assert!(exact.iter().any(|n| n.name == "render_widget"));
    assert!(
        exact
            .iter()
            .all(|n| n.file_path.as_str() == "src/render.rs")
    );
    let output = search(&engram, req(&pid, "code")).await;
    let symbols: Vec<_> = output
        .lines()
        .filter(|line| line.starts_with("symbols:"))
        .collect();
    assert!(
        symbols.iter().any(|line| line.contains("render_widget")),
        "Correct source hint was crowded out: {output}"
    );
    assert!(
        symbols.iter().all(|line| !line.contains("unrelated_")),
        "Namesake hints leaked: {output}"
    );
}


#[tokio::test]
async fn advertised_citation_recovery_roundtrips_exact_colliding_namespace_content() {
    let (_tmp, state, engram, pid) = setup().await;
    let engine = state.get_project_cached(&pid).unwrap().search;
    let fixtures = [
        ("memory_bank", "citationprobe bank \u{00c5}\r\nsecond line\r\n"),
        ("insights", "citationprobe insight \u{65e5}\u{672c}\u{8a9e}\nsecond line\n"),
    ];
    for (namespace, content) in fixtures {
        engine
            .index_docs(
                &pid,
                &[IndexDoc {
                    generation: 0,
                    chunk_id: 991,
                    doc_id: "citation-shared-id".into(),
                    content_hash: engram_core::ContentHash::compute(content.as_bytes()).0,
                    path: engram_core::RelPath::new("notes/citation.md"),
                    content: content.into(),
                    language: "markdown".into(),
                    namespace: namespace.into(),
                    author: None,
                    timestamp: None,
                    start_line: 1,
                    end_line: 2,
                }],
                &CancellationToken::new(),
            )
            .await
            .unwrap();
    }
    let mut request = req(&pid, "knowledge");
    request.query = "citationprobe".into();
    request.include_user_memory = false;
    let out = search(&engram, request).await;
    assert!(out.contains("pass each returned continuation as citation"));
    let mut observed = std::collections::BTreeSet::new();
    let mut legacy_count = 0;
    for line in out.lines() {
        if let Some(arguments) = line.strip_prefix("full_chunk: get_chunk(").and_then(|s| s.strip_suffix(')')) {
            let value: serde_json::Value = serde_json::from_str(arguments).unwrap();
            assert!(value.get("citation").is_none(), "Legacy recovery changed");
            legacy_count += 1;
        }
        let Some(arguments) = line.strip_prefix("citation_chunk: get_chunk(").and_then(|s| s.strip_suffix(')')) else {
            continue;
        };
        let advertised: serde_json::Value = serde_json::from_str(arguments).unwrap();
        assert_eq!(advertised["citation"], serde_json::json!({}));
        let request: engram_server::GetChunkRequest = serde_json::from_value(advertised.clone()).unwrap();
        assert!(request.citation.is_some());
        let expected = fixtures.iter().find(|(ns, _)| *ns == request.namespace).unwrap().1;
        observed.insert(request.namespace.clone());
        let result = engram.get_chunk(Parameters(request)).await.unwrap();
        let raw = result.content.iter().filter_map(|c| c.as_text().map(|t| t.text.as_str())).collect::<Vec<_>>().join("\n");
        let page: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(page["identity"], serde_json::json!({
            "project_id": pid, "namespace": advertised["namespace"], "doc_id": "citation-shared-id"
        }));
        assert_eq!(page["content"], expected);
        assert_eq!(page["total_bytes"], expected.len());
        assert_eq!(page["raw_content_hash"], format!("blake3-raw-utf8:{}", blake3::hash(expected.as_bytes()).to_hex()));
        assert_eq!(page["hash_scope"], "raw_stored_utf8_no_normalization");
        assert_eq!(page["metadata"]["path"], "notes/citation.md");
        assert!(page.get("continuation").is_none_or(serde_json::Value::is_null));
    }
    assert_eq!(observed.len(), 2, "{out}");
    assert_eq!(legacy_count, 2);
}
