use engram_core::Config;
use engram_server::{AppState, Engram};
use serde_json::{Value, json};
#[path = "sql_parameter_workflow_tests.rs"]
mod parameter_regressions;
const SOURCE: &str = "Imports System.Data\nImports System.Data.SqlClient\nPartial Class Hook\n Inherits System.Web.UI.Page\n Public Sub Page_Load(sender As Object,e As System.EventArgs) Handles Me.Load\n  For Each id As Integer In New Integer() {1,2}\n   If id > 0 Then\n    Dim sql = \"SET IDENTITY_INSERT dbo.Expected ON; INSERT INTO dbo.Expected (Id) VALUES (@id); SET IDENTITY_INSERT dbo.Expected OFF\"\n    Using connection As New SqlConnection()\n     If connection.State <> ConnectionState.Open Then\n      connection.Open()\n     End If\n     Using cmd = New SqlCommand(sql,connection)\n      cmd.Parameters.AddWithValue(\"@id\",id)\n      cmd.ExecuteNonQuery()\n     End Using\n    End Using\n   End If\n  Next\n End Sub\nEnd Class\n";
struct Fixture {
    e: Engram,
    pid: String,
    root: std::path::PathBuf,
    _temp: tempfile::TempDir,
}
async fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("p");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("Hook.aspx"),"<%@ Page Language=\"VB\" CodeFile=\"Hook.aspx.vb\" Inherits=\"Hook\" %>\n<form runat=\"server\"></form>\n").unwrap();
    std::fs::write(root.join("Hook.aspx.vb"), SOURCE).unwrap();
    let (state, _) = AppState::new(Config {
        data_dir: temp.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })
    .unwrap();
    let e = Engram::new(state);
    let indexed=text(e.handle_index_project(serde_json::from_value(json!({"directory":root,"project_name":"Identity wrapper","project_type":"dotnet_webforms_vb","wait":true})).unwrap()).await.unwrap());
    let pid = indexed
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .trim()
        .into();
    Fixture {
        e,
        pid,
        root,
        _temp: temp,
    }
}
fn text(r: rmcp::model::CallToolResult) -> String {
    assert_ne!(r.is_error, Some(true));
    r.content[0].as_text().unwrap().text.clone()
}
fn collect<'a>(v: &'a Value, key: &str, out: &mut Vec<&'a Value>) {
    match v {
        Value::Object(map) => {
            for (k, value) in map {
                if k == key {
                    out.push(value);
                } else {
                    collect(value, key, out);
                }
            }
        }
        Value::Array(rows) => {
            for row in rows {
                collect(row, key, out);
            }
        }
        _ => {}
    }
}
#[tokio::test]
async fn three_workflows_expose_identity_batch_source_facts_without_session_guarantees() {
    let f = fixture().await;
    let hash = blake3::hash(SOURCE.as_bytes()).to_hex().to_string();
    let trace:Value=serde_json::from_str(&text(f.e.handle_trace_user_request(serde_json::from_value(json!({"project_id":f.pid,"entry_point":"Hook.aspx","include_db":true,"include_triggers":false,"max_tokens":65536})).unwrap()).await.unwrap())).unwrap();
    let mut batches = vec![];
    collect(&trace, "identity_insert_batch", &mut batches);
    assert!(!batches.is_empty(), "{trace}");
    assert_eq!(batches[0]["identity_target_spelling"], "dbo.Expected");
    assert!(trace.to_string().contains(&hash));
    let repeated: Value = serde_json::from_str(&text(
        f.e.handle_detect_n_plus_one(
            serde_json::from_value(
                json!({"project_id":f.pid,"file_path":"Hook.aspx.vb","max_tokens":65536}),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    ))
    .unwrap();
    let mut batches = vec![];
    collect(&repeated, "source_sql_batch", &mut batches);
    assert_eq!(batches.len(), 1, "{repeated}");
    assert_eq!(batches[0]["source_hash"], hash);
    assert_eq!(
        batches[0]["observations"]["observations"][0]["batch"]["identity_target_spelling"],
        "dbo.Expected"
    );
    let matrix = text(
        f.e.handle_derive_test_matrix(
            serde_json::from_value(json!({"project_id":f.pid,"files":["Hook.aspx.vb"]})).unwrap(),
        )
        .await
        .unwrap(),
    );
    for value in [
        "Identity-insert batch axis",
        "IDENTITY_INSERT dbo.Expected",
        "cleanup after an error",
        &hash,
    ] {
        assert!(matrix.contains(value), "{matrix}");
    }
    assert_eq!(
        std::fs::read_to_string(f.root.join("Hook.aspx.vb")).unwrap(),
        SOURCE
    );
}
#[tokio::test]
async fn historical_stale_and_database_exclusion_controls_keep_batch_evidence_qualified() {
    let f = fixture().await;
    let historical=text(f.e.handle_derive_test_matrix(serde_json::from_value(json!({"project_id":f.pid,"files":["Hook.aspx.vb"],"knowledge_before":"2026-01-01"})).unwrap()).await.unwrap());
    assert!(!historical.contains("Identity-insert batch axis"));
    let excluded=text(f.e.handle_trace_user_request(serde_json::from_value(json!({"project_id":f.pid,"entry_point":"Hook.aspx","include_db":false,"include_triggers":false,"max_tokens":65536})).unwrap()).await.unwrap());
    assert!(!excluded.contains("identity_insert_batch"));
    std::fs::write(
        f.root.join("Hook.aspx.vb"),
        format!("{SOURCE}\n' changed owned source\n"),
    )
    .unwrap();
    let stale = text(
        f.e.handle_detect_n_plus_one(
            serde_json::from_value(
                json!({"project_id":f.pid,"file_path":"Hook.aspx.vb","max_tokens":65536}),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    assert!(!stale.contains("source_sql_batch"));
    assert!(stale.contains("Source changed since indexing"));
    let matrix = text(
        f.e.handle_derive_test_matrix(
            serde_json::from_value(json!({"project_id":f.pid,"files":["Hook.aspx.vb"]})).unwrap(),
        )
        .await
        .unwrap(),
    );
    assert!(!matrix.contains("Identity-insert batch axis"));
}
