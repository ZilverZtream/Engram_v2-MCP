//! Parameter evidence must be usable across normal trace, repetition and QA calls.
use engram_core::Config;
use engram_server::{AppState, Engram, services::vb_sql_parameters};
use serde_json::{Value, json};

#[path = "request_trace_sql_commands.rs"]
mod text_command_regressions;

const SOURCE: &str = "Imports System.Data\nImports System.Data.SqlClient\nPartial Class Hook\n Inherits System.Web.UI.Page\n Public Sub Page_Load(sender As Object, e As System.EventArgs) Handles Me.Load\n  Save()\n End Sub\n Public Sub Save()\n  Using cmd As New SqlCommand(\"INSERT INTO dbo.events (id, label) VALUES (@id, @label)\")\n   cmd.Parameters.Add(\"@id\", SqlDbType.Int)\n   cmd.Parameters.Add(\"@label\", SqlDbType.NVarChar, 8)\n   For Each label As String In New String() {\"one\", \"two\"}\n    cmd.Parameters(\"@id\").Value = 1\n    If label Is Nothing Then\n     cmd.Parameters(\"@label\").Value = DBNull.Value\n    Else\n     cmd.Parameters(\"@label\").Value = label\n    End If\n    cmd.ExecuteNonQuery()\n   Next\n  End Using\n End Sub\nEnd Class\n";
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
fn text(r: rmcp::model::CallToolResult) -> String {
    assert_ne!(r.is_error, Some(true));
    r.content[0].as_text().unwrap().text.clone()
}
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
    let indexed=text(e.handle_index_project(serde_json::from_value(json!({"directory":root,"project_name":"Parameter fixture","project_type":"dotnet_webforms_vb","wait":true})).unwrap()).await.unwrap());
    let pid = indexed
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .trim()
        .to_owned();
    Fixture {
        e,
        pid,
        root,
        _temp: temp,
    }
}

#[test]
fn parameter_syntax_keeps_typed_sizes_and_assignments_without_inventing_values() {
    let body = "Using cmd As New SqlCommand(\"SELECT @label\")\n cmd.Parameters.Add(\"@label\", System.Data.SqlDbType.NVarChar, -1)\n cmd.Parameters(\"@label\").Value = DBNull.Value\n cmd.Parameters.AddWithValue(\"@id\", currentId)\n ' cmd.Parameters.Add(\"@comment\", SqlDbType.Int)\n Dim sample = \"cmd.Parameters.Add(\"\"@quoted\"\", SqlDbType.Int)\"\n cmd.Parameters.Add(\"@computed_\" & tenant, SqlDbType.Int)\n cmd.ExecuteReader()\nEnd Using\n";
    let report = vb_sql_parameters::observe(body, 40);
    assert!(!report.truncated);
    let command = &report.commands[0];
    assert_eq!(command.receiver, "cmd");
    assert_eq!(command.parameters.len(), 2);
    let parameter = command
        .parameters
        .iter()
        .find(|p| p.name == "@label")
        .unwrap();
    assert_eq!(
        parameter.declarations[0].sql_type_spelling.as_deref(),
        Some("NVarChar")
    );
    assert_eq!(
        parameter.declarations[0].size_literal.as_deref(),
        Some("-1")
    );
    assert_eq!(parameter.declarations[0].site.line, 41);
    assert_eq!(parameter.assignments[0].line, 42);
    assert!(
        parameter.assignments[0]
            .source_line
            .contains("DBNull.Value")
    );
    let id = command.parameters.iter().find(|p| p.name == "@id").unwrap();
    assert!(id.declarations[0].sql_type_spelling.is_none());
    assert!(id.assignments[0].source_line.contains("currentId"));
    assert_eq!(command.unsupported_parameter_uses, 1);
    assert!(report.interpretation.contains("branch reachability"));
}

#[tokio::test]
async fn three_workflows_retain_same_hash_bound_sql_parameter_facts() {
    let f = fixture().await;
    let hash = blake3::hash(SOURCE.as_bytes()).to_hex().to_string();
    let trace:Value=serde_json::from_str(&text(f.e.handle_trace_user_request(serde_json::from_value(json!({"project_id":f.pid,"entry_point":"Hook.aspx","include_db":true,"include_triggers":false,"max_tokens":65536})).unwrap()).await.unwrap())).unwrap();
    let mut reports = vec![];
    collect(&trace, "parameter_observations", &mut reports);
    assert!(!reports.is_empty(), "{trace}");
    let trace_parameter = reports
        .iter()
        .flat_map(|r| r["commands"].as_array().into_iter().flatten())
        .flat_map(|c| c["parameters"].as_array().into_iter().flatten())
        .find(|p| p["name"] == "@label")
        .unwrap();
    assert_eq!(
        trace_parameter["declarations"][0]["sql_type_spelling"],
        "NVarChar"
    );
    assert_eq!(trace_parameter["declarations"][0]["size_literal"], "8");
    assert!(
        trace_parameter["assignments"]
            .to_string()
            .contains("DBNull.Value")
    );
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
    let mut reports = vec![];
    collect(&repeated, "parameter_observations", &mut reports);
    assert_eq!(reports.len(), 1, "{repeated}");
    assert_eq!(reports[0]["source_hash"], hash);
    assert_eq!(
        reports[0]["observations"]["commands"][0]["parameters"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let matrix = text(
        f.e.handle_derive_test_matrix(
            serde_json::from_value(json!({"project_id":f.pid,"files":["Hook.aspx.vb"]})).unwrap(),
        )
        .await
        .unwrap(),
    );
    for expected in [
        "Database parameter axis",
        "SQL parameter @label",
        "NVarChar",
        "size literal 8",
        "DBNull.Value",
        &hash,
        "no application or database tests were executed",
    ] {
        assert!(matrix.contains(expected), "missing {expected}: {matrix}");
    }
    assert_eq!(
        std::fs::read_to_string(f.root.join("Hook.aspx.vb")).unwrap(),
        SOURCE
    );
}

#[tokio::test]
async fn historical_and_stale_source_do_not_receive_current_parameter_axes() {
    let f = fixture().await;
    let historical=text(f.e.handle_derive_test_matrix(serde_json::from_value(json!({"project_id":f.pid,"files":["Hook.aspx.vb"],"knowledge_before":"2026-01-01"})).unwrap()).await.unwrap());
    assert!(!historical.contains("Database parameter axis"));
    std::fs::write(
        f.root.join("Hook.aspx.vb"),
        format!("{SOURCE}\n' owned source changed\n"),
    )
    .unwrap();
    let repeated = text(
        f.e.handle_detect_n_plus_one(
            serde_json::from_value(
                json!({"project_id":f.pid,"file_path":"Hook.aspx.vb","max_tokens":65536}),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    assert!(!repeated.contains("parameter_observations"));
    assert!(
        repeated.contains("Source changed since indexing"),
        "{repeated}"
    );
    let matrix = text(
        f.e.handle_derive_test_matrix(
            serde_json::from_value(json!({"project_id":f.pid,"files":["Hook.aspx.vb"]})).unwrap(),
        )
        .await
        .unwrap(),
    );
    assert!(!matrix.contains("Database parameter axis"));
}
