//! Literal instance navigation keeps page context and independently cited control source.
use engram_core::RelPath;
use engram_graph::{GraphStore, Node};
use engram_server::services::request_trace::{
    execution,
    routing::{self, Source},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

const PAGE: &str = "<%@ Page CodeFile='Host.aspx.vb' %>\n<%@ Register TagPrefix='demo' TagName='Outer' Src='Controls/Outer.ascx' %>\n<demo:Outer ID='outer' runat='server' Visible='false' />\n<!-- <demo:Outer ID='ghost' runat='server' /> -->\n";
const OUTER: &str = "<%@ Control CodeFile='Outer.ascx.vb' %>\n<%@ Register TagPrefix='demo' TagName='Inner' Src='Inner.ascx' %>\n<demo:Inner ID='inner' runat='server' />\n";
const INNER: &str = "<%@ Control CodeFile='Inner.ascx.vb' %>\n<asp:DropDownList ID='pick' runat='server' OnSelectedIndexChanged='SelectPlan' />\n";
struct Fixture {
    graph: Arc<GraphStore>,
    _owned: tempfile::TempDir,
    files: Vec<(String, String)>,
    selected: BTreeMap<String, u64>,
}
impl Fixture {
    fn new(ambiguous: bool) -> Self {
        let owned = tempfile::tempdir().unwrap();
        let graph = Arc::new(GraphStore::open(&owned.path().join("graph.redb")).unwrap());
        let mut files: Vec<(String, String)> = vec![
            ("Site/Global.asax".into(),"<%@ Application %>\n".into()),
            ("Site/Host.aspx".into(),PAGE.into()),
            ("Site/Host.aspx.vb".into(),"Public Class Host\nPublic Sub RootLoad() Handles Me.Load\nEnd Sub\nEnd Class\n".into()),
            ("Site/Controls/Outer.ascx".into(),OUTER.into()),
            ("Site/Controls/Outer.ascx.vb".into(),"Public Class Outer\nPublic Sub OuterLoad() Handles Me.Load\nEnd Sub\nEnd Class\n".into()),
            ("Site/Controls/Inner.ascx".into(),INNER.into()),
            ("Site/Controls/Inner.ascx.vb".into(),"Public Class Inner\nPublic Sub InnerLoad() Handles Me.Load\nEnd Sub\nPublic Sub SelectPlan()\nDim db As New DataContext()\ndb.ExecuteCommand(\"UPDATE dbo.Preferences SET Plan=1\")\nEnd Sub\nEnd Class\n".into()),
        ];
        if ambiguous {
            files[1].1.push_str(
                "<%@ Register TagPrefix='demo' TagName='Outer' Src='Controls/Alternate.ascx' %>\n",
            );
            files.push(("Site/Controls/Alternate.ascx".into(), OUTER.into()));
        }
        let make = |id: &str, kind: &str, name: &str, file: &str, start, end| Node {
            node_id: id.into(),
            node_type: kind.into(),
            name: name.into(),
            namespace: "memory".into(),
            language: "vbnet".into(),
            file_path: RelPath::new(file),
            start_line: start,
            end_line: end,
            generation: 1,
            metadata: None,
        };
        let mut nodes = vec![];
        let mut selected = BTreeMap::new();
        for (path, text) in &files {
            let mut file = make(
                &format!("file:{path}"),
                "file",
                path,
                path,
                1,
                text.lines().count() as u32,
            );
            file.metadata =
                Some(json!({"file_hash":blake3::hash(text.as_bytes()).to_hex().to_string()}));
            nodes.push(file);
            selected.insert(path.clone(), 1);
            if path.ends_with(".vb") {
                let lines: Vec<_> = text.lines().collect();
                for (start, line) in lines.iter().enumerate() {
                    let Some(tail) = line.strip_prefix("Public Sub ") else {
                        continue;
                    };
                    let name = tail.split('(').next().unwrap();
                    let end = start
                        + lines[start..]
                            .iter()
                            .position(|line| line.trim() == "End Sub")
                            .unwrap();
                    nodes.push(make(
                        &format!("{path}:{name}"),
                        "function",
                        name,
                        path,
                        start as u32 + 1,
                        end as u32 + 1,
                    ));
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
    fn trace(&self, action: &str) -> Value {
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
                Some(action),
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

#[test]
fn nested_control_event_keeps_root_loads_literal_visibility_and_database_source() {
    let fixture = Fixture::new(false);
    let value = fixture.trace("event outer/inner/pick.SelectedIndexChanged");
    assert_eq!(value["page"], "Site/Host.aspx");
    assert_eq!(value["codebehind"], "Site/Host.aspx.vb");
    for name in ["RootLoad", "OuterLoad", "InnerLoad", "SelectPlan"] {
        assert!(
            value["handlers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|handler| handler["name"] == name),
            "{value}"
        );
    }
    let routes = value["control_routes"]["bindings"].as_array().unwrap();
    assert_eq!(routes.len(), 2);
    assert!(
        routes
            .iter()
            .any(|binding| binding["instance_id"] == "outer"
                && binding["instance_line"] == 3
                && binding["registration_line"] == 2
                && binding["declared_visible"] == "false"
                && binding["source_blake3"] == blake3::hash(OUTER.as_bytes()).to_hex().to_string())
    );
    assert!(
        value["paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|path| path["kind"] == "inline_sql_writes_table"
                && path["target_name"] == "Preferences")
    );
    assert!(
        value["control_routes"]["interpretation"]
            .as_str()
            .unwrap()
            .contains("effective visibility")
    );
}

#[test]
fn instance_ambiguity_remains_visible_and_stale_missing_or_other_app_controls_are_withheld() {
    let ambiguous = Fixture::new(true).trace("event outer/inner/pick.SelectedIndexChanged");
    assert!(
        ambiguous["control_routes"]["bindings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|binding| binding["file"] == "Site/Controls/Alternate.ascx")
    );
    assert!(
        ambiguous["control_routes"]["gaps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|gap| gap.as_str().unwrap().contains("no unique runtime instance"))
    );
    for mode in ["markup", "code", "missing", "application", "comment"] {
        let mut fixture = Fixture::new(false);
        match mode {
            "markup" => fixture
                .files
                .iter_mut()
                .find(|(path, _)| path == "Site/Controls/Outer.ascx")
                .unwrap()
                .1
                .push_str("<!-- changed -->"),
            "code" => fixture
                .files
                .iter_mut()
                .find(|(path, _)| path == "Site/Controls/Inner.ascx.vb")
                .unwrap()
                .1
                .push_str("' changed\n"),
            "missing" => fixture
                .files
                .retain(|(path, _)| path != "Site/Controls/Inner.ascx"),
            "application" => fixture.files.push((
                "Site/Controls/Global.asax".into(),
                "<%@ Application %>\n".into(),
            )),
            _ => {}
        }
        let value = fixture.trace(if mode == "comment" {
            "event ghost/inner/pick.SelectedIndexChanged"
        } else {
            "event outer/inner/pick.SelectedIndexChanged"
        });
        assert!(
            !value["handlers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|handler| handler["name"] == "SelectPlan"),
            "{mode}: {value}"
        );
        assert!(
            value["handlers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|handler| handler["name"] == "RootLoad"),
            "{mode}: page context lost"
        );
    }
}
