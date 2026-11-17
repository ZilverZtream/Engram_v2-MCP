//! Literal master source context is separate from runtime hosting/lifecycle proof.
use engram_core::RelPath;
use engram_graph::{GraphStore, Node};
use engram_server::services::request_trace::{
    execution,
    routing::{self, Source},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

const PAGE: &str = "<%@ Page CodeFile='Host.aspx.vb' MasterPageFile='~/Frames/Inner.master' %>\n";
const INNER: &str = "<%@ Master CodeFile='Inner.master.vb' MasterPageFile='Outer.master' %>\n";
const OUTER: &str = "<%@ Master CodeFile='Outer.master.vb' %>\n";
struct Fixture {
    graph: Arc<GraphStore>,
    _owned: tempfile::TempDir,
    files: Vec<(String, String)>,
    selected: BTreeMap<String, u64>,
}
impl Fixture {
    fn new(inline_root: bool) -> Self {
        let fixture_root = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new()
            .prefix("mt-")
            .tempdir_in(fixture_root)
            .unwrap();
        let graph = Arc::new(GraphStore::open(&owned.path().join("graph.redb")).unwrap());
        let files:Vec<(String,String)>=vec![
            ("Site/Global.asax".into(),"<%@ Application %>\n".into()),
            ("Site/Host.aspx".into(),if inline_root{"<%@ Page MasterPageFile='~/Frames/Inner.master' %>\n".into()}else{PAGE.into()}),
            ("Site/Host.aspx.vb".into(),"Public Class Host\nPublic Sub RootLoad() Handles Me.Load\nEnd Sub\nEnd Class\n".into()),
            ("Site/Frames/Inner.master".into(),INNER.into()),
            ("Site/Frames/Inner.master.vb".into(),"Public Class InnerMaster\nPublic Sub InnerLoad() Handles Me.Load\nDim db As New DataContext()\ndb.ExecuteCommand(\"UPDATE dbo.Audit SET LastSeen=1\")\nEnd Sub\nEnd Class\n".into()),
            ("Site/Frames/Outer.master".into(),OUTER.into()),
            ("Site/Frames/Outer.master.vb".into(),"Public Class OuterMaster\nPublic Sub OuterLoad() Handles Me.Load\nEnd Sub\nEnd Class\n".into()),
        ];
        let mut nodes = Vec::new();
        let mut selected = BTreeMap::new();
        for (path, text) in &files {
            selected.insert(path.clone(), 1);
            let file = Node {
                node_id: format!("file:{path}"),
                node_type: "file".into(),
                name: path.clone(),
                namespace: "memory".into(),
                language: "vbnet".into(),
                file_path: RelPath::new(path),
                start_line: 1,
                end_line: text.lines().count() as u32,
                generation: 1,
                metadata: Some(
                    json!({"file_hash":blake3::hash(text.as_bytes()).to_hex().to_string()}),
                ),
            };
            nodes.push(file.clone());
            if path.ends_with(".vb") {
                let lines: Vec<_> = text.lines().collect();
                for (start, line) in lines.iter().enumerate() {
                    let Some(header) = line.strip_prefix("Public Sub ") else {
                        continue;
                    };
                    let mut method = file.clone();
                    let name = header.split('(').next().unwrap();
                    method.node_id = format!("{path}:{name}");
                    method.node_type = "function".into();
                    method.name = name.into();
                    method.metadata = None;
                    method.start_line = start as u32 + 1;
                    method.end_line = (start
                        + lines[start..]
                            .iter()
                            .position(|line| *line == "End Sub")
                            .unwrap()) as u32
                        + 1;
                    nodes.push(method);
                }
            }
        }
        graph.upsert_nodes("owned", &nodes).unwrap();
        Self {
            graph,
            _owned: owned,
            files,
            selected,
        }
    }
    fn trace(&self) -> Value {
        let sources: Vec<_> = self
            .files
            .iter()
            .map(|(path, text)| Source {
                path,
                content: text,
            })
            .collect();
        let route = routing::resolve("/Host.aspx", &sources)
            .unwrap()
            .candidates
            .remove(0);
        serde_json::to_value(
            execution::trace_page(
                &self.graph,
                "owned",
                &route,
                Some("load page"),
                true,
                false,
                &sources,
                &self.selected,
            )
            .unwrap(),
        )
        .unwrap()
    }
}
fn has(value: &Value, name: &str) -> bool {
    value["handlers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|handler| handler["name"] == name)
}
#[test]
fn nested_literal_masters_keep_root_identity_load_context_and_sql_source() {
    let value = Fixture::new(false).trace();
    assert_eq!(value["page"], "Site/Host.aspx");
    assert_eq!(value["codebehind"], "Site/Host.aspx.vb");
    assert!(
        has(&value, "RootLoad") && has(&value, "InnerLoad") && has(&value, "OuterLoad"),
        "{value}"
    );
    let links = value["master_pages"]["bindings"].as_array().unwrap();
    assert_eq!(links.len(), 2);
    assert!(
        links
            .iter()
            .any(|link| link["file"] == "Site/Frames/Inner.master"
                && link["directive_line"] == 1
                && link["source_blake3"] == blake3::hash(INNER.as_bytes()).to_hex().to_string())
    );
    assert!(value["paths"].as_array().unwrap().iter().any(|path|path["kind"]=="inline_sql_writes_table"&&path["target_name"]=="Audit"));
    assert!(
        value["master_pages"]["interpretation"]
            .as_str()
            .unwrap()
            .contains("execution/lifecycle order")
    );
}
#[test]
fn master_context_survives_inline_root_and_withholds_changed_or_missing_sources() {
    let inline = Fixture::new(true).trace();
    assert!(inline["codebehind"].is_null());
    assert!(has(&inline, "InnerLoad") && has(&inline, "OuterLoad"));
    assert!(!has(&inline, "RootLoad"));
    let mut missing_root = Fixture::new(false);
    missing_root
        .files
        .retain(|(path, _)| path != "Site/Host.aspx.vb");
    let value = missing_root.trace();
    assert!(has(&value, "InnerLoad") && has(&value, "OuterLoad"));
    assert!(!has(&value, "RootLoad"));
    for mode in ["markup", "code", "missing", "application"] {
        let mut fixture = Fixture::new(false);
        match mode {
            "markup" => fixture
                .files
                .iter_mut()
                .find(|(path, _)| path == "Site/Frames/Inner.master")
                .unwrap()
                .1
                .push_str("<!-- changed -->"),
            "code" => fixture
                .files
                .iter_mut()
                .find(|(path, _)| path == "Site/Frames/Inner.master.vb")
                .unwrap()
                .1
                .push_str("' changed\n"),
            "application" => fixture.files.push((
                "Site/Frames/Global.asax".into(),
                "<%@ Application %>\n".into(),
            )),
            _ => fixture
                .files
                .retain(|(path, _)| path != "Site/Frames/Inner.master"),
        }
        let value = fixture.trace();
        assert!(has(&value, "RootLoad"));
        assert!(!has(&value, "InnerLoad"), "{mode}: {value}");
        assert!(!value["master_pages"]["gaps"].as_array().unwrap().is_empty());
        if mode == "code" {
            assert!(has(&value, "OuterLoad"));
        } else {
            assert!(!has(&value, "OuterLoad"));
        }
    }
}
