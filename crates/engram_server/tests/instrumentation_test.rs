#![allow(clippy::unwrap_used)]
use engram_core::Config;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;

#[tokio::test]
async fn test_instrumentation_pack_and_ingest() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    let cfg = Config {
        allowed_roots: vec![root.to_path_buf()],
        data_dir: root.join("engram_data"),
        max_project_files: Some(100),
        max_project_bytes: Some(1024 * 1024),
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());

    // Exercise each public pack; on Windows compile and execute its actual
    // returned helper rather than accepting a placeholder-shaped string.
    let mut emitted_logs = String::new();
    for language in ["csharp", "vb"] {
        let pack = engram.get_instrumentation_pack(Parameters(
            engram_server::GetInstrumentationPackRequest { language: language.into() },
        )).await.unwrap();
        let text = &pack.content[0].as_text().unwrap().text;
        assert!(text.contains("ingest_instrumentation_logs(project_id="));
        assert!(text.contains("does not derive hashes from SQL text"));
        #[cfg(windows)]
        emitted_logs.push_str(&compile_and_run_pack(root, language, text));
    }
    // Retain the importer contract on hosts without .NET Framework compilers.
    #[cfg(not(windows))]
    emitted_logs.push_str("ENGRAM_LOG|2026-02-19T15:00:00|Default.aspx|OnClick|btnSubmit|a4956f29a36e\nENGRAM_LOG|2026-02-19T15:01:00|Order.aspx|OnSave|btnSave|b4956f29a36e\n");

    // 2. Index dummy project
    let project_dir = root.join("my_project");
    std::fs::create_dir_all(&project_dir).unwrap();
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "InstTest".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsVb,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();

    let projects = state.registry.list_projects().unwrap();
    let project_id = &projects[0].project_id;

    // 3. Ingest logs
    let res_ingest = engram
        .ingest_instrumentation_logs(Parameters(
            engram_server::IngestInstrumentationLogsRequest {
                project_id: project_id.clone(),
                log_content: emitted_logs.clone(),
            },
        ))
        .await
        .unwrap();

    let ingest_text = &res_ingest.content[0].as_text().unwrap().text;
    assert!(
        ingest_text.contains(&format!("added {} runtime SQL call edges", emitted_logs.lines().count())),
        "Should show 2 edges added"
    );

    // 4. Verify in graph
    let edges = state
        .graph
        .list_edges(project_id, Some(engram_graph::EdgeKind::SqlCalls))
        .unwrap();
    assert!(
        edges
            .iter()
            .any(|e| e.source_id == "control:Default.aspx:btnSubmit"
                && e.target_id == "sql:inline:a4956f29a36e")
    );
    assert!(
        edges
            .iter()
            .any(|e| e.source_id == "control:Order.aspx:btnSave"
                && e.target_id == "sql:inline:b4956f29a36e")
    );
    drop(engram);
    drop(state);
    drop(_rx);
    tmp.close().unwrap();
}


#[cfg(windows)]
fn compile_and_run_pack(root: &std::path::Path, language: &str, response: &str) -> String {
    use std::process::Command;
    let directory = root.join(language);
    std::fs::create_dir(&directory).unwrap();
    let (extension, fence, program) = if language == "vb" {
        ("vb", "vbnet", r#"
Imports System
Module Program
    Sub Main()
        EngramInstrumentation.LogEngramEvent(AddressOf Console.WriteLine, "Default.aspx", "OnClick", "btnSubmit", "sql:inline:a4956f29a36e")
        EngramInstrumentation.LogEngramEvent(AddressOf Console.WriteLine, "Order.aspx", "OnSave", "btnSave", "sql:inline:b4956f29a36e")
        Dim rejected As Integer = 0
        Try
            EngramInstrumentation.LogEngramEvent(AddressOf Console.WriteLine, "Default.aspx", "Bad|Event", "", "sql:inline:a4956f29a36e")
        Catch ex As ArgumentException
            rejected += 1
        End Try
        Try
            EngramInstrumentation.LogEngramEvent(AddressOf Console.WriteLine, "Default.aspx", "OnClick", "", "sql:stored_proc:SavePhotos")
        Catch ex As ArgumentException
            rejected += 1
        End Try
        If rejected <> 2 Then Environment.Exit(2)
    End Sub
End Module
"#)
    } else {
        ("cs", "csharp", r#"
using System;
class Program {
    static int Main() {
        EngramInstrumentation.LogEngramEvent(Console.WriteLine, "Default.aspx", "OnClick", "btnSubmit", "sql:inline:a4956f29a36e");
        EngramInstrumentation.LogEngramEvent(Console.WriteLine, "Order.aspx", "OnSave", "btnSave", "sql:inline:b4956f29a36e");
        int rejected = 0;
        try { EngramInstrumentation.LogEngramEvent(Console.WriteLine, "Default.aspx", "Bad\nEvent", "", "sql:inline:a4956f29a36e"); }
        catch (ArgumentException) { rejected++; }
        try { EngramInstrumentation.LogEngramEvent(Console.WriteLine, "Default.aspx", "OnClick", "", "sql:stored_proc:SavePhotos"); }
        catch (ArgumentException) { rejected++; }
        return rejected == 2 ? 0 : 2;
    }
}
"#)
    };
    let code = response.split_once(&format!("```{fence}\n")).unwrap().1.split_once("```").unwrap().0;
    let helper = directory.join(format!("EngramInstrumentation.{extension}"));
    let program_path = directory.join(format!("Program.{extension}"));
    let binary = directory.join("capture.exe");
    std::fs::write(&helper, code).unwrap();
    std::fs::write(&program_path, program).unwrap();
    let framework = std::path::PathBuf::from(std::env::var_os("WINDIR").unwrap())
        .join("Microsoft.NET/Framework64/v4.0.30319");
    let mut command = Command::new(framework.join(if language == "vb" { "vbc.exe" } else { "csc.exe" }));
    command.args(["/nologo", "/target:exe"]).arg(format!("/out:{}", binary.display()));
    if language == "vb" { command.arg(format!("/sdkpath:{}", framework.display())); }
    let compiled = command.arg(&helper).arg(&program_path).output().unwrap();
    assert!(compiled.status.success(), "{language} compile: {} {}", String::from_utf8_lossy(&compiled.stdout), String::from_utf8_lossy(&compiled.stderr));
    let run = Command::new(&binary).output().unwrap();
    assert!(run.status.success(), "{language} runtime: {}", String::from_utf8_lossy(&run.stderr));
    let logs = String::from_utf8(run.stdout).unwrap();
    let rows: Vec<_> = logs.lines().map(|line| line.split('|').collect::<Vec<_>>()).collect();
    assert_eq!(rows.len(), 2, "invalid observations must not reach the sink");
    assert!(rows.iter().all(|row| row.len() == 6 && row[0] == "ENGRAM_LOG" && row[1].ends_with('Z')));
    assert_eq!(&rows[0][2..], &["Default.aspx", "OnClick", "btnSubmit", "a4956f29a36e"]);
    assert_eq!(&rows[1][2..], &["Order.aspx", "OnSave", "btnSave", "b4956f29a36e"]);
    logs
}
