#![allow(clippy::unwrap_used)]
use engram_core::RelPath;
use engram_graph::{Edge, EdgeKind, GraphStore, Node};
use engram_server::services::n_plus_one::Analyzer;
use serde_json::json;
use std::collections::BTreeMap;

struct ParserOwner;
impl Drop for ParserOwner {
    fn drop(&mut self) {
        if let Err(error) = engram_index::vb_extractor::shutdown_idle_parser() {
            eprintln!("Owned test parser teardown failed: {error}");
        }
    }
}
fn file(path: &str, source: &str) -> Node {
    Node {
        node_id: format!("file:{path}"),
        node_type: "file".into(),
        name: path.into(),
        namespace: "memory".into(),
        language: "vb".into(),
        file_path: RelPath::new(path),
        start_line: 1,
        end_line: source.lines().count() as u32,
        generation: 7,
        metadata: Some(json!({"file_hash":blake3::hash(source.as_bytes()).to_hex().to_string()})),
    }
}

#[test]
#[ignore = "Requires the paired Roslyn sidecar; run with ENGRAM_VB_SIDECAR_PATH and --ignored"]
fn n_plus_one_local_helpers_rows_and_stale_sources() {
    assert!(std::env::var_os("ENGRAM_VB_SIDECAR_PATH").is_some());
    let temp = tempfile::tempdir().unwrap();
    let _parser = ParserOwner;
    let source = r#"Imports System.Data.SqlClient
Public Class Page
 Private Grid As Object
 Public Sub Run()
  For i=1 To 10
   Dim x=GetDetail(i)
   Dim y=PureHelper(i)
  Next
  Dim z=GetDetail(0)
 End Sub
 Public Function GetDetail(id As Integer) As Integer
  Using cmd As New SqlCommand("SELECT Price FROM dbo.Items WHERE Id=@Id")
   Return CInt(cmd.ExecuteScalar())
  End Using
 End Function
 Private Function PureHelper(id As Integer) As Integer
  Return id+1
 End Function
 Protected Sub EachRow() Handles Grid.RowDataBound
  Dim result=GetDetail(1)
 End Sub
End Class
"#;
    std::fs::write(temp.path().join("Page.vb"), source).unwrap();
    {
        let graph = GraphStore::open(&temp.path().join("graph.redb")).unwrap();
        graph.upsert_nodes("p", &[file("Page.vb", source)]).unwrap();
        let methods: Vec<_> = [("Run", 4, 10), ("GetDetail", 11, 15), ("EachRow", 19, 21)]
            .into_iter()
            .map(|(name, start, end)| Node {
                node_id: format!("sym:{name}"),
                node_type: "function".into(),
                name: format!("Page.{name}"),
                namespace: "memory".into(),
                language: "vb".into(),
                file_path: RelPath::new("Page.vb"),
                start_line: start,
                end_line: end,
                generation: 7,
                metadata: None,
            })
            .collect();
        graph.upsert_nodes("p", &methods).unwrap();
        let calls: Vec<_> = [("Run", vec![6, 9]), ("EachRow", vec![20])]
            .into_iter()
            .map(|(owner, lines)| Edge {
                source_id: format!("sym:{owner}"),
                target_id: "sym:GetDetail".into(),
                namespace: "memory".into(),
                language: "vb".into(),
                edge_kind: EdgeKind::Calls,
                weight: 1,
                generation: 7,
                metadata: Some(json!({"call_site_lines":lines})),
                updated_at_ms: 1,
            })
            .collect();
        graph.upsert_edges("p", &calls).unwrap();
        let selected = BTreeMap::from([("Page.vb".into(), 7)]);
        let mut analyzer = Analyzer::new(&graph, "p", temp.path(), &selected);
        analyzer.analyze_file("Page.vb");
        assert_eq!(analyzer.findings.len(), 2, "{}", analyzer.coverage());
        assert_eq!(analyzer.findings[0]["repeated_call"]["start_line"], 6);
        assert_eq!(analyzer.findings[1]["repeated_call"]["start_line"], 20);
        for finding in &analyzer.findings {
            assert_eq!(finding["database_call"]["site"]["start_line"], 13);
            assert!(
                finding["database_call"]["evidence"]["literal_command"]
                    .as_str()
                    .unwrap()
                    .contains("dbo.Items")
            );
            assert_eq!(finding["call_path"].as_array().unwrap().len(), 2);
            assert!(finding["estimated_iterations"].is_null());
        }
        std::fs::write(
            temp.path().join("Page.vb"),
            source.replace("SELECT Price", "SELECT Name"),
        )
        .unwrap();
        let mut stale = Analyzer::new(&graph, "p", temp.path(), &selected);
        stale.analyze_file("Page.vb");
        assert!(stale.findings.is_empty());
        assert!(
            stale
                .gaps
                .iter()
                .any(|g| g.contains("changed since indexing"))
        );
    }
    engram_index::vb_extractor::shutdown_idle_parser().unwrap();
    temp.close().unwrap();
}

#[test]
#[ignore = "Requires the paired Roslyn sidecar; run with ENGRAM_VB_SIDECAR_PATH and --ignored"]
fn n_plus_one_markup_binding_requires_authored_current_markup() {
    assert!(std::env::var_os("ENGRAM_VB_SIDECAR_PATH").is_some());
    let temp = tempfile::tempdir().unwrap();
    let _parser = ParserOwner;
    let source = r#"Imports System.Data.SqlClient
Public Class Page
 Public Sub Bound()
  Dim cmd As New SqlCommand("SELECT Name FROM dbo.Items")
  cmd.ExecuteReader()
 End Sub
 Public Sub Outside()
  Dim cmd As New SqlCommand("SELECT Name FROM dbo.Items")
  cmd.ExecuteReader()
 End Sub
End Class
"#;
    let markup = r#"<%@ Page Language="VB" CodeBehind="Page.aspx.vb" Inherits="Page" %>
<asp:GridView ID="rows" runat="server" OnRowDataBound="Bound" />
<%-- <asp:GridView ID="fake" runat="server" OnRowDataBound="Outside" /> --%>
"#;
    std::fs::write(temp.path().join("Page.aspx.vb"), source).unwrap();
    std::fs::write(temp.path().join("Page.aspx"), markup).unwrap();
    {
        let graph = GraphStore::open(&temp.path().join("graph.redb")).unwrap();
        graph
            .upsert_nodes(
                "p",
                &[file("Page.aspx.vb", source), file("Page.aspx", markup)],
            )
            .unwrap();
        let selected = BTreeMap::from([("Page.aspx.vb".into(), 7), ("Page.aspx".into(), 7)]);
        let mut analyzer = Analyzer::new(&graph, "p", temp.path(), &selected);
        analyzer.analyze_file("Page.aspx.vb");
        assert_eq!(analyzer.findings.len(), 1, "{}", analyzer.coverage());
        assert_eq!(analyzer.findings[0]["contexts"][0]["file"], "Page.aspx");
        assert_eq!(analyzer.findings[0]["repeated_call"]["start_line"], 5);
        std::fs::write(
            temp.path().join("Page.aspx"),
            markup.replace("Bound\"", "Outside\""),
        )
        .unwrap();
        let mut stale = Analyzer::new(&graph, "p", temp.path(), &selected);
        stale.analyze_file("Page.aspx.vb");
        assert!(stale.findings.is_empty());
        assert!(
            stale
                .gaps
                .iter()
                .any(|g| g.contains("changed since indexing"))
        );
    }
    engram_index::vb_extractor::shutdown_idle_parser().unwrap();
    temp.close().unwrap();
}

#[test]
#[ignore = "Requires the paired Roslyn sidecar; run with ENGRAM_VB_SIDECAR_PATH and --ignored"]
fn n_plus_one_linq_context_queries_exclude_cached_collections() {
    assert!(std::env::var_os("ENGRAM_VB_SIDECAR_PATH").is_some());
    let temp = tempfile::tempdir().unwrap();
    let _parser = ParserOwner;
    let source = r#"Public Class Page
 Public Sub Run(rows As Object)
  Using db As New TidDataContext
   For Each row In rows
    Dim direct = db.Items.FirstOrDefault()
    Dim filtered = db.Items.Where(Function(item) item.Id = 1).ToList()
    Dim q = From item In db.Items Select item
    Dim listed = q.ToList()
    Dim cached = db.Items.ToList()
    Dim local = cached.FirstOrDefault()
    Dim changed = From item In db.Items Select item
    changed = New List(Of Object)()
    Dim wrong = changed.ToList()
    Dim deferred = db.Items.Where(Function(item) item.Id = 1)
    db.SubmitChanges()
   Next
  End Using
 End Sub
End Class
"#;
    std::fs::write(temp.path().join("Page.vb"), source).unwrap();
    {
        let graph = GraphStore::open(&temp.path().join("graph.redb")).unwrap();
        graph.upsert_nodes("p", &[file("Page.vb", source)]).unwrap();
        let selected = BTreeMap::from([("Page.vb".into(), 7)]);
        let mut analyzer = Analyzer::new(&graph, "p", temp.path(), &selected);
        analyzer.analyze_file("Page.vb");
        let lines: Vec<_> = analyzer
            .findings
            .iter()
            .map(|f| f["database_call"]["site"]["start_line"].as_u64().unwrap())
            .collect();
        assert_eq!(lines, vec![5, 6, 8, 9, 15], "{}", analyzer.coverage());
        assert!(
            analyzer.findings[4]["suggested_fix"]
                .as_str()
                .unwrap()
                .starts_with("Batch the writes")
        );
        assert!(analyzer.findings[0]["database_call"]["evidence"]["literal_command"].is_null());
    }
    engram_index::vb_extractor::shutdown_idle_parser().unwrap();
    temp.close().unwrap();
}
