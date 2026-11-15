use engram_core::{Config, ContentHash, RelPath};
use engram_server::{AppState, Engram};
use serde_json::{Value, json};
#[path = "identity_batch_workflow_tests.rs"]
mod existing_workflows;
const SOURCE: &str = include_str!("fixtures/member-workflows/Hook.aspx.vb");
struct Fixture {
    e: Engram,
    pid: String,
    root: std::path::PathBuf,
    _temp: tempfile::TempDir,
}
fn text(r: rmcp::model::CallToolResult) -> String {
    assert_ne!(r.is_error, Some(true));
    r.content[0].as_text().unwrap().text.clone()
}
fn line(name: &str) -> u32 {
    SOURCE.lines().position(|s| s.contains(name)).unwrap() as u32 + 1
}
async fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("p");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("Hook.aspx"),"<%@ Page Language=\"VB\" CodeFile=\"Hook.aspx.vb\" Inherits=\"Hook\" %>\n<form runat=\"server\"></form>\n").unwrap();
    std::fs::write(root.join("Hook.aspx.vb"), SOURCE).unwrap();
    std::fs::create_dir_all(root.join(".github/instructions")).unwrap();
    std::fs::write(root.join(".github/instructions/member.instructions.md"),"---\napplyTo: \"**/*.vb\"\n---\nPreserve caller ordering. Exception: reviewed asynchronous imports may use their documented ordering.\n").unwrap();
    let (state, _) = AppState::new(Config {
        data_dir: temp.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        advertise_all_tools: true,
        ..Default::default()
    })
    .unwrap();
    let e = Engram::new(state);
    let indexed=text(e.handle_index_project(serde_json::from_value(json!({"directory":root,"project_name":"Method workflow","project_type":"dotnet_webforms_vb","wait":true})).unwrap()).await.unwrap());
    let pid = indexed
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .trim()
        .to_string();
    Fixture {
        e,
        pid,
        root,
        _temp: temp,
    }
}
async fn repeated(f: &Fixture, scope: Option<u32>) -> Value {
    let mut args = json!({"project_id":f.pid,"file_path":"Hook.aspx.vb","max_tokens":65536});
    if let Some(scope) = scope {
        args["method_start_line"] = json!(scope);
    }
    serde_json::from_str(&text(
        f.e.handle_detect_n_plus_one(serde_json::from_value(args).unwrap())
            .await
            .unwrap(),
    ))
    .unwrap()
}
async fn matrix(f: &Fixture, scope: Option<u32>) -> String {
    let mut args = json!({"project_id":f.pid,"files":["Hook.aspx.vb"]});
    if let Some(scope) = scope {
        args["methods"] = json!([{"file_path":"Hook.aspx.vb","line_start":scope}]);
    }
    text(
        f.e.handle_derive_test_matrix(serde_json::from_value(args).unwrap())
            .await
            .unwrap(),
    )
}
#[tokio::test]
async fn selected_roots_keep_reached_helpers_and_exclude_sibling_database_calls() {
    let f = fixture().await;
    let all = repeated(&f, None).await;
    let scoped = repeated(&f, Some(line("Public Sub Page_Load"))).await;
    assert!(
        all["findings"].as_array().unwrap().len() > scoped["findings"].as_array().unwrap().len()
    );
    let calls = scoped["findings"].as_array().unwrap();
    assert_eq!(calls.len(), 2, "{scoped}");
    assert!(
        calls
            .iter()
            .all(|v| v["repeated_call"]["method"].as_str() == Some("Page_Load"))
    );
    assert!(
        calls
            .iter()
            .any(|v| v["database_call"]["site"]["method"].as_str() == Some("ReadHelper")),
        "{scoped}"
    );
    assert!(!scoped.to_string().contains("@noise"));
    assert!(
        scoped["analysis_scope"]
            .as_str()
            .unwrap()
            .contains("sibling")
    );
}
#[tokio::test]
async fn scoped_qa_keeps_policy_exceptions_and_only_selected_axes_and_rule_documents() {
    let f = fixture().await;
    let runtime =
        engram_server::services::project_service::ensure_project_runtime(&f.e.state, &f.pid)
            .await
            .unwrap();
    let mut docs = vec![];
    for (name, label) in [("Page_Load", "wanted_rule"), ("Unrelated", "sibling_rule")] {
        let lines: Vec<_> = SOURCE.lines().collect();
        let start = lines
            .iter()
            .position(|s| s.contains(&format!("Public Sub {name}(")))
            .unwrap();
        let end = start
            + lines[start..]
                .iter()
                .position(|s| s.trim() == "End Sub")
                .unwrap();
        let body = lines[start..=end].join("\n").trim_start().to_owned();
        let hash = ContentHash::compute(body.as_bytes()).0;
        let analysis = engram_server::services::business_logic_service::parse_llm_response(
            &json!({"purpose":label,"business_rules":[label]}).to_string(),
            "Hook.aspx.vb",
            name,
            &format!("Hook.{name}"),
            &hash,
        );
        let content = format!(
            "{}\n_Source: Hook.aspx.vb_\n",
            engram_server::services::business_logic_service::render_method_as_doc(&analysis)
        );
        docs.push(engram_index::IndexDoc {
            generation: 0,
            chunk_id: docs.len() as u64,
            path: RelPath::new(&format!("__business_logic/Hook.aspx.vb/{name}.md")),
            language: "markdown".into(),
            content_hash: ContentHash::compute(content.as_bytes()).0,
            content,
            namespace: "business_logic".into(),
            author: None,
            timestamp: None,
            start_line: 0,
            end_line: 0,
            doc_id: name.into(),
        });
    }
    runtime
        .search
        .index_docs(&f.pid, &docs, &tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    let all = matrix(&f, None).await;
    let scoped = matrix(&f, Some(line("Public Sub Page_Load"))).await;
    for value in [
        "@wanted",
        "@noise",
        "@helper",
        "wanted_rule",
        "sibling_rule",
    ] {
        assert!(all.contains(value), "{all}");
    }
    for value in [
        "@wanted",
        "wanted_rule",
        "Preserve caller ordering.",
        "Exception: reviewed asynchronous imports",
    ] {
        assert!(scoped.contains(value), "{scoped}");
    }
    for value in ["@noise", "@helper", "sibling_rule"] {
        assert!(!scoped.contains(value), "{scoped}");
    }
    assert!(scoped.contains("not whole-file, companion or transitive-helper QA"));
}
#[tokio::test]
async fn method_context_actions_preserve_identity_and_missing_or_stale_scope_does_not_widen() {
    let f = fixture().await;
    let wanted = line("Public Sub Page_Load");
    let raw=text(f.e.handle_get_method_edit_context(serde_json::from_value(json!({"project_id":f.pid,"file_path":"Hook.aspx.vb","method_name":"Page_Load","include_history":false,"include_business_logic":false,"output_json":true})).unwrap()).await.unwrap());
    let ctx: Value = serde_json::from_str(&raw).unwrap();
    let checks = ctx["focused_checks"].as_array().unwrap();
    assert_eq!(checks.len(), 2, "{ctx}");
    for call in checks {
        if call["tool"] == "detect_n_plus_one" {
            assert_eq!(call["arguments"]["method_start_line"], wanted);
            let result = text(
                f.e.handle_detect_n_plus_one(
                    serde_json::from_value(call["arguments"].clone()).unwrap(),
                )
                .await
                .unwrap(),
            );
            assert!(!result.contains("@noise"));
        } else {
            assert_eq!(call["arguments"]["methods"][0]["line_start"], wanted);
            let result = text(
                f.e.handle_derive_test_matrix(
                    serde_json::from_value(call["arguments"].clone()).unwrap(),
                )
                .await
                .unwrap(),
            );
            assert!(!result.contains("@noise"));
        }
    }
    let missing = repeated(&f, Some(wanted + 1)).await;
    assert!(missing["findings"].as_array().unwrap().is_empty());
    assert!(
        missing
            .to_string()
            .contains("does not identify one indexed method")
    );
    let missing_qa = matrix(&f, Some(wanted + 1)).await;
    assert!(missing_qa.contains("method scope unresolved"));
    assert!(!missing_qa.contains("SQL parameter @noise"));
    std::fs::write(
        f.root.join("Hook.aspx.vb"),
        format!("{SOURCE}\n' changed owned source\n"),
    )
    .unwrap();
    let stale = repeated(&f, Some(wanted)).await;
    assert!(stale["findings"].as_array().unwrap().is_empty());
    assert!(stale.to_string().contains("Source changed since indexing"));
    let stale_qa = matrix(&f, Some(wanted)).await;
    assert!(stale_qa.contains("method-scoped axes withheld"));
    assert!(!stale_qa.contains("SQL parameter @wanted"));
}
