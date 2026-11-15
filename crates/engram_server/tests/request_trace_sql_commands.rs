//! Public request-trace behavior over source-fingerprinted owned graph fixtures.
use engram_core::RelPath;
use engram_graph::{GraphStore, Node};
use engram_server::services::request_trace::{
    execution,
    routing::{self, Source},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

fn node(id: &str, kind: &str, path: &str, start: u32, end: u32) -> Node {
    Node {
        node_id: id.into(),
        node_type: kind.into(),
        name: if kind == "function" {
            "Entry.Save".into()
        } else {
            path.into()
        },
        namespace: "memory".into(),
        file_path: RelPath::new(path),
        start_line: start,
        end_line: end,
        language: "vbnet".into(),
        generation: 1,
        metadata: None,
    }
}
fn trace(body: &str, stale: bool, include_db: bool) -> Value {
    // Declaration order closes the database before its owned directory.
    struct Fixture {
        graph: Arc<GraphStore>,
        _temp: tempfile::TempDir,
    }
    let temp = tempfile::tempdir().unwrap();
    let fixture = Fixture {
        graph: Arc::new(GraphStore::open(&temp.path().join("graph.redb")).unwrap()),
        _temp: temp,
    };
    let markup = "<%@ Page CodeFile=\"Entry.aspx.vb\" %>\n<asp:Button ID=\"btnSave\" runat=\"server\" OnClick=\"Save\" />";
    let vb = format!("Public Class Entry\nPublic Sub Save()\n{body}End Sub\nEnd Class\n");
    let sql = "CREATE PROCEDURE dbo.ExistingSave AS UPDATE dbo.Expected SET Value = 1;\n";
    let sources = vec![
        Source {
            path: "Entry.aspx",
            content: markup,
        },
        Source {
            path: "Entry.aspx.vb",
            content: &vb,
        },
        Source {
            path: "existing.sql",
            content: sql,
        },
    ];
    let mut selected = BTreeMap::new();
    let mut nodes = vec![node(
        "handler",
        "function",
        "Entry.aspx.vb",
        2,
        vb.lines().count() as u32 - 1,
    )];
    for source in &sources {
        let mut file = node(
            &format!("file:{}", source.path),
            "file",
            source.path,
            1,
            source.content.lines().count() as u32,
        );
        file.metadata = Some(
            json!({"file_hash":if stale && source.path=="Entry.aspx.vb" {"old".into()} else {blake3::hash(source.content.as_bytes()).to_hex().to_string()}}),
        );
        nodes.push(file);
        selected.insert(source.path.to_owned(), 1);
    }
    fixture.graph.upsert_nodes("owned", &nodes).unwrap();
    let route = routing::resolve("Entry.aspx", &sources)
        .unwrap()
        .candidates
        .remove(0);
    let result = execution::trace_page(
        &fixture.graph,
        "owned",
        &route,
        Some("click btnSave"),
        include_db,
        false,
        &sources,
        &selected,
    )
    .unwrap();
    serde_json::to_value(result).unwrap()
}
fn tables(trace: &Value) -> Vec<(&str, &str)> {
    let mut tables = trace["paths"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|hop| {
            let kind = hop["kind"].as_str()?;
            kind.starts_with("inline_sql_")
                .then_some((kind, hop["target_name"].as_str().unwrap()))
        })
        .collect::<Vec<_>>();
    tables.sort();
    tables
}

#[test]
fn static_sql_command_keeps_insert_select_and_nested_read_evidence_with_all_source_locations() {
    let body = "Static sql As String =\n\"INSERT INTO dbo.Events (Id) \" _\n & \"SELECT p.Id FROM dbo.Projects p LEFT JOIN dbo.Mappings m ON m.Id=p.Id \" _\n & \"WHERE p.Id NOT IN (SELECT Id FROM dbo.Events)\"\nUsing cmd As New System.Data.SqlClient.SqlCommand(sql, connection)\ncmd.Parameters.AddWithValue(\"@id\", id)\ncmd.ExecuteNonQuery()\nEnd Using\n";
    let result = trace(body, false, true);
    assert_eq!(
        tables(&result),
        vec![
            ("inline_sql_reads_table", "Events"),
            ("inline_sql_reads_table", "Mappings"),
            ("inline_sql_reads_table", "Projects"),
            ("inline_sql_writes_table", "Events")
        ]
    );
    for hop in result["paths"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|hop| hop["kind"].as_str().unwrap().starts_with("inline_sql_"))
    {
        assert_eq!(hop["target_source_verified"], false);
        let evidence = &hop["evidence"];
        assert_eq!(
            evidence["origin"],
            "current_method_literal_sql_command_text"
        );
        assert_eq!(evidence["sql_source"]["declaration_kind"], "Static");
        assert_eq!(evidence["sql_source"]["line"], 3);
        assert_eq!(evidence["command_declaration_line"], 7);
        assert_eq!(evidence["line"], 9);
        assert_eq!(evidence["command_type"], "Text");
        assert_eq!(evidence["source_blake3"].as_str().unwrap().len(), 64);
        assert!(!evidence["sql"].as_str().unwrap().contains(" _"));
    }
}

#[test]
fn literal_constructor_and_command_text_assignment_are_tied_to_the_executed_receiver() {
    for body in [
        "Using cmd As New SqlCommand(\"UPDATE dbo.Expected SET Value = @v\", connection)\ncmd.ExecuteNonQuery()\nEnd Using\n",
        "Dim cmd = New SqlCommand()\ncmd.CommandText = \"UPDATE dbo.Expected \" & \"SET Value = @v\"\ncmd.CommandType = System.Data.CommandType.Text\ncmd.ExecuteNonQuery()\n",
    ] {
        assert_eq!(
            tables(&trace(body, false, true)),
            vec![("inline_sql_writes_table", "Expected")]
        );
    }
    let result = trace(
        "Dim unused As New SqlCommand(\"DELETE FROM dbo.Unused\")\nDim active As New SqlCommand(\"SELECT * FROM dbo.Expected\")\nactive.ExecuteReader()\n",
        false,
        true,
    );
    assert_eq!(
        tables(&result),
        vec![("inline_sql_reads_table", "Expected")]
    );
}

#[test]
fn constant_and_explicit_continuation_sql_keep_existing_execute_query_and_command_behavior() {
    for body in [
        "Const sql As String = \"SELECT * FROM dbo.Expected\"\nUsing cmd As New SqlCommand(sql)\nDim count = cmd.ExecuteScalar()\nEnd Using\n",
        "Const sql As String = \"SELECT * \" _\n & \"FROM dbo.Expected\"\nDim row = db.ExecuteQuery(Of Integer)(sql).FirstOrDefault()\n",
    ] {
        assert_eq!(
            tables(&trace(body, false, true)),
            vec![("inline_sql_reads_table", "Expected")]
        );
    }
    assert_eq!(
        tables(&trace(
            "db.ExecuteCommand(\"UPDATE dbo.Expected SET Value={0}\", value)\n",
            false,
            true
        )),
        vec![("inline_sql_writes_table", "Expected")]
    );
}

#[test]
fn unknown_changed_or_unexecuted_command_text_is_not_invented() {
    for body in [
        "Dim cmd As New SqlCommand(BuildQuery())\ncmd.ExecuteReader()\n",
        "Static sql As String = \"SELECT * FROM dbo.Old\"\nsql = BuildQuery()\nDim cmd As New SqlCommand(sql)\ncmd.ExecuteReader()\n",
        "Dim cmd As New SqlCommand(\"SELECT * FROM dbo.Old\")\ncmd.CommandText = BuildQuery()\ncmd.ExecuteReader()\n",
        "Dim cmd As New SqlCommand(\"DELETE FROM dbo.Unused\")\n' cmd.ExecuteNonQuery()\n",
    ] {
        assert!(tables(&trace(body, false, true)).is_empty());
    }
}

#[test]
fn stored_procedure_mode_remains_owned_by_the_existing_source_procedure_reader() {
    let result = trace(
        "Dim cmd As New SqlCommand(\"dbo.ExistingSave\")\ncmd.CommandType = CommandType.StoredProcedure\ncmd.ExecuteNonQuery()\n",
        false,
        true,
    );
    assert!(tables(&result).is_empty());
    assert!(
        result["paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|hop| hop["target_name"] == "dbo.ExistingSave")
    );
}

#[test]
fn text_command_source_must_be_current_and_database_expansion_must_be_requested() {
    let body =
        "Dim cmd As New SqlCommand(\"UPDATE dbo.Expected SET Value = 1\")\ncmd.ExecuteNonQuery()\n";
    assert!(tables(&trace(body, true, true)).is_empty());
    assert!(tables(&trace(body, false, false)).is_empty());
}
