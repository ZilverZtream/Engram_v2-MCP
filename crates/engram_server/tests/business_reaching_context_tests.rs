#![allow(clippy::unwrap_used)]
use engram_core::{Config, ContentHash, DocIdStr, RelPath};
use engram_index::IndexDoc;
use engram_server::services::{
    business_logic_service as logic, business_outcome_dependencies as outcomes,
};
use engram_server::{AppState, Engram};
use rmcp::handler::server::tool::Parameters;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn persisted_prerequisites_reach_matrix_query_and_ask_without_rewriting_rule() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let source = "Public Class Rules\n Public Sub Save()\n  ValidateFirst()\n  ValidateSecond()\n  Dim filter = BuildFilter()\n  Fetch(filter)\n End Sub\nEnd Class\n";
    std::fs::write(root.join("Rules.vb"), source).unwrap();
    let (state, _) = AppState::new(Config {
        data_dir: temp.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })
    .unwrap();
    let e = Engram::new(state.clone());
    e.index_project(Parameters(serde_json::from_value(json!({"directory":root,"project_name":"Reaching","project_type":"dotnet_webforms_vb","wait":true})).unwrap())).await.unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    let body = source
        .lines()
        .skip(1)
        .take(6)
        .collect::<Vec<_>>()
        .join("\n")
        .trim_start()
        .to_owned();
    let hash = ContentHash::compute(body.as_bytes()).0;
    let mut analysis=logic::parse_llm_response(&json!({"purpose":"Save policy","business_rules":[{"when":"execution enters Save","then":"ValidateFirst() and ValidateSecond() run","source_line":3,"refs":["ValidateFirst()","ValidateSecond()"]}],"steps":[]}).to_string(),"Rules.vb","Save","Rules.Save",&hash);
    let raw_rule = analysis.business_rules[0].clone();
    let mut evidence = outcomes::collect(&body, "Rules", "vb", 2, "Rules.vb", None);
    assert!(
        evidence.dependencies.is_empty(),
        "fixture isolates reaching context from immediate-return helpers"
    );
    evidence.fingerprint(&body, "fixture");
    analysis.outcome_evidence = Some(evidence);
    let mut content = logic::render_method_as_doc(&analysis);
    // Production persistence appends this source identity after rendering.
    content.push_str(&format!("\n_Source: {}_\n", analysis.file_path));
    assert!(content.contains(&raw_rule));
    let recovered = outcomes::from_document(&content).unwrap();
    assert_eq!(recovered.reaching_context.unwrap().runs[0].len(), 4);
    let content_hash = ContentHash::compute(content.as_bytes());
    let path = "__business_logic/Rules.vb/Save.md";
    let doc_id = DocIdStr::compute(path, 0, 0, &content_hash).0;
    state
        .get_project_cached(&pid)
        .unwrap()
        .search
        .index_docs(
            &pid,
            &[IndexDoc {
                generation: 0,
                chunk_id: 99,
                path: RelPath::new(path),
                language: "markdown".into(),
                content,
                namespace: "business_logic".into(),
                author: None,
                timestamp: None,
                start_line: 0,
                end_line: 0,
                doc_id,
                content_hash: content_hash.0,
            }],
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let matrix = e
        .handle_derive_test_matrix(
            serde_json::from_value(json!({"project_id":pid,"files":["Rules.vb"]})).unwrap(),
        )
        .await
        .unwrap();
    let matrix = &matrix.content[0].as_text().unwrap().text;
    assert!(matrix.contains(&raw_rule), "{matrix}");
    assert!(
        matrix.contains("BLOCKED pending reaching-prerequisite review"),
        "{matrix}"
    );
    assert!(
        matrix.contains("exact_expression_lexical_conditional"),
        "{matrix}"
    );
    assert!(matrix.contains("normal_completion_unverified"), "{matrix}");
    assert!(
        matrix.contains("Outcome status: blocked_pending_reaching_prerequisite_review"),
        "{matrix}"
    );
    let query = e
        .handle_query_business_logic(
            serde_json::from_value(json!({"project_id":pid,"query":"Save policy","top_k":5}))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        query.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("Reaching context: prerequisites_require_review")
    );
    let ask=e.handle_ask_codebase(serde_json::from_value(json!({"project_id":pid,"question":"What business rules govern Rules.Save?","depth":"standard","output_format":"json","include_insights":false})).unwrap()).await.unwrap();
    let report: Value = serde_json::from_str(&ask.content[0].as_text().unwrap().text).unwrap();
    let business = report["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .find(|ev| ev["provider"] == "business_logic")
        .unwrap();
    assert!(
        business["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning
                .as_str()
                .is_some_and(|s| s.contains("Reaching context: prerequisites_require_review"))),
        "{business}"
    );
    // Moving the caller preserves its method hash but makes recorded line prerequisites stale.
    std::fs::write(root.join("Rules.vb"), format!("\n{source}")).unwrap();
    let moved = e
        .handle_query_business_logic(
            serde_json::from_value(json!({"project_id":pid,"query":"Save policy","top_k":5}))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        moved.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("STALE_ANCHORS")
    );
}
