//! A normal save request exposes declared trigger effects without execution claims.
use engram_core::Config;
use engram_server::{AppState, Engram};
use rmcp::handler::server::tool::Parameters;
use serde_json::{Value, json};

#[tokio::test]
async fn reached_update_includes_matching_source_trigger_and_body_references() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir(&source).unwrap();
    let markup = "<%@ Page Language=\"VB\" CodeFile=\"Entry.aspx.vb\" Inherits=\"Entry\" %>\n<asp:Button ID=\"btnSave\" runat=\"server\" OnClick=\"Save\" />\n";
    let vb = "Public Class Entry\n    Public Sub Save()\n        Using cmd As New SqlCommand(\"UPDATE dbo.Orders SET Name=@name WHERE Id=@id\", connection)\n            cmd.ExecuteNonQuery()\n        End Using\n    End Sub\nEnd Class\n";
    let sql = "CREATE TRIGGER dbo.OrderAudit ON dbo.Orders AFTER UPDATE AS\nBEGIN\nINSERT INTO dbo.AuditRows (OrderId) SELECT Id FROM inserted;\nEND\nGO\nCREATE TRIGGER archive.OtherSchema ON archive.Orders AFTER UPDATE AS INSERT INTO archive.Unrelated VALUES (1);\nGO\nCREATE TRIGGER dbo.DeleteOnly ON dbo.Orders AFTER DELETE AS INSERT INTO dbo.Deletions VALUES (1);\nGO\n";
    for (name, text) in [
        ("Entry.aspx", markup),
        ("Entry.aspx.vb", vb),
        ("triggers.sql", sql),
    ] {
        std::fs::write(source.join(name), text).unwrap();
    }
    let (state, _) = AppState::new(Config {
        data_dir: temp.path().join("data"),
        allowed_roots: vec![source.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })
    .unwrap();
    let engram = Engram::new(state.clone());
    engram.index_project(Parameters(serde_json::from_value(json!({
        "directory":source,"project_name":"Trigger path source","project_type":"dotnet_webforms_vb","wait":true
    })).unwrap())).await.unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    for (include_db, include_triggers) in [(true, true), (true, false), (false, true)] {
        let result = engram
            .handle_trace_user_request(
                serde_json::from_value(json!({
                    "project_id":pid,"entry_point":"Entry.aspx","action":"click btnSave",
                    "include_db":include_db,"include_triggers":include_triggers,"max_tokens":262144
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        let report: Value =
            serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
        assert_eq!(report["coverage"]["runtime_execution_proven"], false);
        let page = &report["pages"][0];
        let paths = page["paths"].as_array().unwrap();
        let triggers: Vec<_> = paths
            .iter()
            .filter(|path| path["kind"] == "declared_trigger_candidate")
            .collect();
        if !include_db || !include_triggers {
            assert!(triggers.is_empty());
            continue;
        }
        assert_eq!(triggers.len(), 1, "{report}");
        let trigger = triggers[0];
        assert_eq!(trigger["target_name"], "[dbo].[OrderAudit]");
        assert_eq!(trigger["evidence"]["write_operation"], "UPDATE");
        assert_eq!(trigger["evidence"]["events"], json!(["UPDATE"]));
        assert_eq!(trigger["evidence"]["timing"], "AFTER");
        assert_eq!(
            trigger["evidence"]["citation"]["source_hash"],
            blake3::hash(sql.as_bytes()).to_hex().to_string()
        );
        let original_write = paths
            .iter()
            .find(|path| path["kind"] == "inline_sql_writes_table")
            .unwrap();
        assert_eq!(trigger["from"], original_write["to"]);
        assert!(paths.iter().any(|path| path["from"] == trigger["to"]
            && path["kind"] == "trigger_body_writes_table"
            && path["target_name"] == "AuditRows"));
        assert!(!paths.iter().any(|path| path["target_name"] == "Unrelated"
            || path["target_name"] == "Deletions"
            || path["target_name"] == "inserted"));
        let member = page["source_members"]
            .as_array()
            .unwrap()
            .iter()
            .find(|member| member["node_id"] == trigger["to"])
            .unwrap();
        assert!(
            member["excerpt"]
                .as_str()
                .unwrap()
                .contains("CREATE TRIGGER dbo.OrderAudit")
        );
        assert_eq!(
            page["trigger_coverage"],
            "source_candidates_only_runtime_unknown"
        );
    }
    drop(engram);
    drop(state);
    temp.close().unwrap();
}
