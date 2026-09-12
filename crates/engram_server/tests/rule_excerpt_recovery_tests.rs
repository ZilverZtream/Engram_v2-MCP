#![allow(clippy::unwrap_used)]
//! Real seeded-store handlers must retain recovery identities when excerpting.
use engram_core::{Config, ContentHash, DocIdStr, ProjectRecord, RelPath};
use engram_index::IndexDoc;
use engram_server::{services::project_service, state::AppState, tools::Engram};
use serde_json::json;
use tokio_util::sync::CancellationToken;

const PID: &str = "rule-excerpts";

async fn fixture() -> (tempfile::TempDir, AppState, Engram) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let (state, _events) = AppState::new(Config {
        data_dir: temp.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        ..Default::default()
    })
    .unwrap();
    state
        .registry
        .put_project(&ProjectRecord {
            project_id: PID.into(),
            project_name: PID.into(),
            directory: root.to_string_lossy().into_owned(),
            project_type: "general".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            reindex_required_since_ms: None,
        })
        .unwrap();
    state
        .registry
        .set_meta(PID, "active_generation", "1")
        .unwrap();
    project_service::ensure_project_runtime(&state, PID)
        .await
        .unwrap();
    let engram = Engram::new(state.clone());
    (temp, state, engram)
}

#[tokio::test]
async fn tiny_native_business_lookup_retains_retrieved_required_evidence() {
    use engram_server::services::business_logic_service::{
        parse_llm_response, render_method_as_doc,
    };
    use rmcp::handler::server::tool::Parameters;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let method = " Public Function ReadItem(invalid As Boolean) As Object\n  If invalid Then\n   Global.Helpers.Reject()\n   Return Nothing\n  End If\n  Return New Object()\n End Function";
    std::fs::write(
        root.join("Caller.vb"),
        format!("Public Class Caller\n{method}\nEnd Class\n"),
    )
    .unwrap();
    std::fs::write(root.join("Helpers.vb"),"Public Class Helpers\n Public Shared Sub Reject()\n  Throw New InvalidOperationException()\n End Sub\nEnd Class\n").unwrap();
    let (state, _) = AppState::new(Config {
        data_dir: temp.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })
    .unwrap();
    let engram = Engram::new(state.clone());
    engram.index_project(Parameters(serde_json::from_value(json!({"directory":root,"project_name":"RequiredEvidence","project_type":"dotnet_webforms_vb","wait":true})).unwrap())).await.unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    let raw = json!({"purpose":"Reads an item","business_rules":[{"when":"invalid","then":"calls rejection helper and returns Nothing","source_line":3,"refs":[]}],"steps":[],"side_effects_detail":""}).to_string();
    let analysis = parse_llm_response(
        &raw,
        "Caller.vb",
        "ReadItem",
        "Caller.ReadItem",
        &ContentHash::compute(method.as_bytes()).0,
    );
    let content = render_method_as_doc(&analysis);
    let mut stored = doc(40, "business_logic", content);
    stored.path = RelPath::new("__business_logic/Caller.vb/ReadItem.md");
    state
        .get_project_cached(&pid)
        .unwrap()
        .search
        .index_docs(&pid, &[stored], &CancellationToken::new())
        .await
        .unwrap();
    let response = engram.handle_ask_codebase(serde_json::from_value(json!({"project_id":pid,"question":"What business rules govern Caller.ReadItem invalid input?","depth":"standard","output_format":"json","include_insights":false})).unwrap()).await.unwrap();
    let report: serde_json::Value =
        serde_json::from_str(&response.content[0].as_text().unwrap().text).unwrap();
    assert!(
        report["providers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["provider"] == "business_logic" && p["status"] == "hit"),
        "{report}"
    );
    assert!(
        report["plan"]["needed_evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|k| k == "business_rule")
    );
    assert!(
        report["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["provider"] == "business_logic"),
        "A hit for a required kind must survive reservation: {report}"
    );
}

fn doc(index: usize, namespace: &str, content: String) -> IndexDoc {
    let path = format!("rules/rule{index}.md");
    let hash = ContentHash::compute(content.as_bytes());
    IndexDoc {
        generation: 0,
        chunk_id: index as u64,
        path: RelPath::new(&path),
        language: "markdown".into(),
        content,
        namespace: namespace.into(),
        author: None,
        timestamp: None,
        start_line: 0,
        end_line: 0,
        doc_id: DocIdStr::compute(&path, 0, 0, &hash).0,
        content_hash: hash.0,
    }
}

fn recovery(text: &str, label: &str) -> serde_json::Value {
    let line = text
        .lines()
        .find_map(|line| line.trim().strip_prefix(label))
        .unwrap();
    serde_json::from_str(line.strip_suffix(')').unwrap()).unwrap()
}

#[tokio::test]
async fn business_rules_are_bounded_and_source_verified_with_working_recovery() {
    let (temp, state, engram) = fixture().await;
    let method = "Public Function Save() As Integer\nReturn 1\nEnd Function";
    let source = format!("Class Rules\n{method}\nEnd Class\n");
    std::fs::write(temp.path().join("project/Rules.vb"), &source).unwrap();
    let hash = ContentHash::compute(method.as_bytes()).0;
    let docs: Vec<_> = (0..12).map(|index| doc(index, "business_logic", format!(
        "# Rules.Save\n**Analysis method hash**: `{hash}`\n_Source: Rules.vb_\n## Business Rules\n- inventory rule {index}: {}\nTAIL-REQUIREMENT-{index}\n", "🦀".repeat(2600)
    ))).collect();
    state
        .get_project_cached(PID)
        .unwrap()
        .search
        .index_docs(PID, &docs, &CancellationToken::new())
        .await
        .unwrap();
    let response = engram
        .handle_query_business_logic(
            serde_json::from_value(json!({"project_id":PID,"query":"inventory","top_k":12}))
                .unwrap(),
        )
        .await
        .unwrap();
    let text = &response.content[0].as_text().unwrap().text;
    assert!(text.len() <= 48 * 1024, "{} bytes", text.len());
    assert!(text.contains("VERIFIED_METHOD_HASH"), "{text}");
    assert!(text.contains("matched=12") && text.contains("total response budget reached"));
    assert!(!text.contains("truncated_documents=0"));
    assert!(!text.contains("TAIL-REQUIREMENT"));
    assert!(!text.contains('\u{fffd}'));
    let request = recovery(text, "full_document: get_chunk(");
    assert_eq!(request["namespace"], "business_logic");
    assert_eq!(request["project_id"], PID);
    let full = engram
        .handle_get_chunk(serde_json::from_value(request).unwrap())
        .await
        .unwrap();
    assert!(
        full.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("TAIL-REQUIREMENT")
    );
}

#[tokio::test]
async fn pre_push_excerpts_offer_correct_full_rule_recovery() {
    let (_temp, state, engram) = fixture().await;
    let rule = doc(
        20,
        "quality_gate",
        "Inventory updates must validate permissions before saving. FULL-RULE-END".into(),
    );
    state
        .get_project_cached(PID)
        .unwrap()
        .search
        .index_docs(PID, &[rule.clone()], &CancellationToken::new())
        .await
        .unwrap();
    let response = engram
        .handle_pre_push_audit(
            serde_json::from_value(json!({"project_id":PID,"code":"inventory permissions saving"}))
                .unwrap(),
        )
        .await
        .unwrap();
    let text = &response.content[0].as_text().unwrap().text;
    assert!(
        text.contains("Search excerpt (not the full rule)"),
        "{text}"
    );
    let request = recovery(text, "full_rule: get_chunk(");
    assert_eq!(request["namespace"], "quality_gate");
    assert_eq!(request["doc_id"], rule.doc_id);
    let full = engram
        .handle_get_chunk(serde_json::from_value(request).unwrap())
        .await
        .unwrap();
    assert!(
        full.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("FULL-RULE-END")
    );
}

#[tokio::test]
async fn retrieval_promotes_warnings_and_outcomes_beyond_excerpts_and_rechecks_source_locations() {
    let (temp, state, engram) = fixture().await;
    let root = temp.path().join("project");
    let method = "Public Function Save() As Integer\nReturn 1\nEnd Function";
    let source = format!("Class Rules\n{method}\nEnd Class\n");
    std::fs::write(root.join("Rules.vb"), &source).unwrap();
    let helper =
        "Class Helpers\nPublic Shared Sub Reject()\nThrow New Exception()\nEnd Sub\nEnd Class";
    std::fs::write(root.join("Helpers.vb"), helper).unwrap();
    let hash = ContentHash::compute(method.as_bytes()).0;
    let dependency = json!({"callee_expression":"Global.Helpers.Reject","call_line":3,"return_line":4,"region_start_line":2,
        "evidence_status":"source_verified","outcome_status":"normal_completion_unverified","reason":"qualified lexical source; not compiler binding",
        "helper_file":"Helpers.vb","helper_file_hash":blake3::hash(helper.as_bytes()).to_hex().to_string()});
    let evidence = json!({"version":"immediate-return-dependencies-v1","scope":"immediate return only","caller_file":"Rules.vb","caller_owner":"Rules","caller_start_line":2,"language":"vb","dependencies":[dependency],"omissions":["full control flow unknown"],"analysis_fingerprint":"fixture"});
    let warning = "Step 2: qualified expression `Access.Checks.ReadItems` is not present in the supplied method";
    let padding = "provenance-padding ".repeat(650);
    // Retrieval intentionally omits provenance metadata; exercise the content budget
    // with substantive prose while retaining raw provenance recovery below.
    let substantive = "substantive-padding ".repeat(650);
    let document = format!(
        "# Rules.Save\n**Analysis method hash**: `{hash}`\n**Extraction provenance**: `{padding}`\n**Outcome dependencies v1**: `{evidence}`\n**Purpose**: inventory changes {substantive}\n## Source checks requiring review\n- {warning}\n## Business Rules\n- inventory Save requires helper review before treating the return as a test outcome. [line 3]\n_Source: Rules.vb_\n"
    );
    let indexed = doc(30, "business_logic", document.clone());
    state
        .get_project_cached(PID)
        .unwrap()
        .search
        .index_docs(PID, &[indexed], &CancellationToken::new())
        .await
        .unwrap();
    let query =
        || serde_json::from_value(json!({"project_id":PID,"query":"inventory","top_k":1})).unwrap();
    let response = engram.handle_query_business_logic(query()).await.unwrap();
    let text = &response.content[0].as_text().unwrap().text;
    assert!(
        text.contains(warning) && text.contains("blocked_pending_helper_outcome"),
        "{text}"
    );
    assert!(text.contains("excerpt truncated") && text.len() <= 48 * 1024);
    assert!(!text.contains("provenance-padding"));
    assert!(text.find(warning).unwrap() < text.find("substantive-padding").unwrap());
    let raw_request = recovery(text, "full_document: get_chunk(");
    let raw = engram.handle_get_chunk(serde_json::from_value(raw_request).unwrap()).await.unwrap();
    let raw_text = &raw.content[0].as_text().unwrap().text;
    assert!(raw_text.contains("provenance-padding") && raw_text.contains("substantive-padding"));
    assert!(text.contains("recorded caller declaration matches current location"));
    for format in ["json", "markdown"] {
        let response = engram.handle_ask_codebase(serde_json::from_value(json!({"project_id":PID,"question":"business rules inventory","output_format":format})).unwrap()).await.unwrap();
        let text = &response.content[0].as_text().unwrap().text;
        if format == "json" {
            let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
            let item = parsed["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["document_namespace"] == "business_logic")
                .expect("business evidence");
            assert!(
                item["warnings"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|w| w.as_str().unwrap().contains(warning)),
                "{item}"
            );
            assert!(
                item["warnings"]
                    .to_string()
                    .contains("blocked_pending_helper_outcome")
            );
            assert!(
                item["content"]
                    .as_str()
                    .unwrap()
                    .contains("inventory Save requires helper review")
            );
            assert!(
                !item["content"]
                    .as_str()
                    .unwrap()
                    .contains("provenance-padding")
            );
            assert!(
                item["source_verification"]
                    .as_str()
                    .unwrap()
                    .starts_with("VERIFIED_METHOD_HASH")
            );
        } else {
            assert!(
                text.contains(warning) && text.contains("blocked_pending_helper_outcome"),
                "{text}"
            );
            assert!(
                text.contains("inventory Save requires helper review"),
                "{text}"
            );
        }
    }
    std::fs::write(
        root.join("Helpers.vb"),
        helper.replace("Throw New Exception()", "Return"),
    )
    .unwrap();
    std::fs::write(root.join("Rules.vb"), format!("\n\n{source}")).unwrap();
    let response = engram.handle_query_business_logic(query()).await.unwrap();
    let text = &response.content[0].as_text().unwrap().text;
    assert!(
        text.contains("VERIFIED_METHOD_HASH") && text.contains("STALE_ANCHORS"),
        "{text}"
    );
    assert!(text.contains("1 stale_or_unavailable"), "{text}");
    let request = recovery(text, "source_body: get_full_method_body(");
    assert_eq!(request["line_start"], 4);
    let response = engram.handle_ask_codebase(serde_json::from_value(json!({"project_id":PID,"question":"business rules inventory","output_format":"json"})).unwrap()).await.unwrap();
    let text = &response.content[0].as_text().unwrap().text;
    assert!(
        text.contains("STALE_ANCHORS") && text.contains("1 stale_or_unavailable"),
        "{text}"
    );
}
