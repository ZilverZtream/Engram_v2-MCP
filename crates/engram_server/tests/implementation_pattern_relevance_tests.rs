//! Real indexing/handler regression for specific patterns among chunk-rich files.
use engram_core::config::Config;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn specific_pattern_survives_many_broad_multichunk_files() {
    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    for file in 0..16 {
        let mut source = format!("Public Class General{file}\n");
        for method in 0..6 {
            source.push_str(&format!(
                "    ' Service project access download helper.\n    Public Function General{method}(project As Integer) As Integer\n        ' General project access download processing and service state.\n        If project < 0 Then\n            Return 0\n        End If\n        Return project\n    End Function\n"));
        }
        let padding = format!("        ' {}\n", "neutral explanatory context ".repeat(7)).repeat(8);
        source = source.replace("        If project < 0 Then", &(padding + "        If project < 0 Then"));
        source.push_str("End Class\n");
        std::fs::write(root.join(format!("General{file:02}.vb")), source).unwrap();
    }
    std::fs::write(root.join("Exact.vb"),
        "Public Class Exact\n    Public Function Download(stream As Stream) As HttpResponseMessage\n        Dim response As New HttpResponseMessage()\n        response.Content = New StreamContent(stream)\n        Return response\n    End Function\nEnd Class\n").unwrap();
    let config = Config { allowed_roots: vec![root.clone()], data_dir: temp.path().join("data"),
        embedding_backend: "fts_only".into(), llm_backend: "none".into(),
        max_project_files: Some(30), max_project_bytes: Some(1024 * 1024), ..Default::default() };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let (state, _rx) = AppState::new(config).unwrap();
    let engram = Engram::new(state.clone());
    engram.index_project(Parameters(engram_server::IndexProjectRequest {
        directory: root.to_string_lossy().into(), project_name: "PatternRelevance".into(),
        project_type: engram_server::models::ProjectType::DotnetWebformsVb,
        wait: true, dedupe_by_directory: false,
    })).await.unwrap();
    let pid = state.registry.list_projects().unwrap()[0].project_id.clone();
    let request = json!({"project_id": pid, "pattern_query": "service project access download HttpResponseMessage StreamContent",
        "max_examples": 3, "output_json": true});
    let result = engram.handle_find_implementation_pattern(serde_json::from_value(request.clone()).unwrap()).await.unwrap();
    let value: serde_json::Value = serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
    assert!(value["coverage"]["lexical_files"].as_u64().unwrap() > 15, "{value}");
    assert_eq!(value["coverage"]["lexical_status"], "complete");
    assert!(value["coverage"]["lexical_hits"].as_u64().unwrap() > 17, "{value}");
    assert!(value["coverage"]["lexical_hits"].as_u64().unwrap() < 200, "{value}");
    assert_eq!(value["coverage"]["candidates_considered"], 15);
    assert_eq!(value["exemplars"][0]["path"], "Exact.vb", "{value}");
    let repeated = engram.handle_find_implementation_pattern(serde_json::from_value(request).unwrap()).await.unwrap();
    let again: serde_json::Value = serde_json::from_str(&repeated.content[0].as_text().unwrap().text).unwrap();
    assert_eq!(value["exemplars"], again["exemplars"]);
}
