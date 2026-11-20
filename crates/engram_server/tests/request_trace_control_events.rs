//! Authored control events lead to admitted handler and existing database evidence.
use engram_core::RelPath;
use engram_graph::{GraphStore, Node};
use engram_server::services::request_trace::{
    control_events, execution,
    routing::{self, Source},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

const PAGE: &str = r#"<%@ Page CodeFile='Entry.aspx.vb' AutoEventWireup='false' %>
<asp:DropDownList ID='plan' Text="<%$ Resources: text, Plan %>" runat='server' OnSelectedIndexChanged='PickPlan' />
<asp:CheckBox ID='enabled' runat='server' />
<asp:Button ID='save' Text="<%$ Resources: text, Save %>" runat='server' OnClick='Save' />
<asp:DropDownList ID='plan' runat='server' title="example OnSelectedIndexChanged='Phantom'" />
<!-- <asp:DropDownList ID='plan' runat='server' OnSelectedIndexChanged='Phantom' /> -->
"#;
const VB: &str = "Public Class Entry\nPublic Sub PageLoad() Handles Me.Load\nEnd Sub\nPublic Sub PickPlan()\nDim db As New DataContext()\ndb.ExecuteCommand(\"UPDATE dbo.Preferences SET Plan=1\")\nEnd Sub\nPublic Sub Toggle() Handles enabled.CheckedChanged\nEnd Sub\nPublic Sub Save()\nEnd Sub\nPublic Sub Phantom()\nEnd Sub\nEnd Class\n";

fn trace(action: &str, stale: bool) -> Value {
    // Database owners close before the directory on both success and unwinding.
    struct Fixture {
        graph: Arc<GraphStore>,
        _owned: tempfile::TempDir,
    }
    let owned = tempfile::tempdir().unwrap();
    let fixture = Fixture {
        graph: Arc::new(GraphStore::open(&owned.path().join("graph.redb")).unwrap()),
        _owned: owned,
    };
    let sources = [
        Source {
            path: "Entry.aspx",
            content: PAGE,
        },
        Source {
            path: "Entry.aspx.vb",
            content: VB,
        },
    ];
    let mut selected = BTreeMap::new();
    let make = |id: &str, kind: &str, name: &str, file: &str, start, end| Node {
        node_id: id.into(),
        node_type: kind.into(),
        name: name.into(),
        namespace: "memory".into(),
        file_path: RelPath::new(file),
        start_line: start,
        end_line: end,
        language: "vbnet".into(),
        generation: 1,
        metadata: None,
    };
    let mut nodes = vec![];
    for source in &sources {
        let mut node = make(
            &format!("file:{}", source.path),
            "file",
            source.path,
            source.path,
            1,
            source.content.lines().count() as u32,
        );
        node.metadata = Some(
            json!({"file_hash":if stale && source.path.ends_with(".vb") {"stale".into()} else {blake3::hash(source.content.as_bytes()).to_hex().to_string()}}),
        );
        nodes.push(node);
        selected.insert(source.path.to_owned(), 1);
    }
    let lines: Vec<_> = VB.lines().collect();
    for name in ["PageLoad", "PickPlan", "Toggle", "Save", "Phantom"] {
        let start = lines
            .iter()
            .position(|line| line.contains(&format!("Sub {name}(")))
            .unwrap();
        let end = start
            + lines[start..]
                .iter()
                .position(|line| line.trim() == "End Sub")
                .unwrap();
        nodes.push(make(
            name,
            "function",
            &format!("Entry.{name}"),
            "Entry.aspx.vb",
            start as u32 + 1,
            end as u32 + 1,
        ));
    }
    fixture.graph.upsert_nodes("owned", &nodes).unwrap();
    let route = routing::resolve("Entry.aspx", &sources)
        .unwrap()
        .candidates
        .remove(0);
    serde_json::to_value(
        execution::trace_page(
            &fixture.graph,
            "owned",
            &route,
            Some(action),
            true,
            false,
            &sources,
            &selected,
        )
        .unwrap(),
    )
    .unwrap()
}

#[test]
fn declared_dropdown_event_keeps_markup_and_handler_sources_and_reaches_sql() {
    let value = trace("event plan.SelectedIndexChanged", false);
    let handlers = value["handlers"].as_array().unwrap();
    assert!(handlers.iter().any(|handler| {
        handler["name"] == "PickPlan"
            && handler["binding"]
                .as_str()
                .unwrap()
                .contains("OnSelectedIndexChanged")
    }));
    assert!(handlers.iter().any(|handler| handler["name"] == "PageLoad"));
    assert!(
        !handlers
            .iter()
            .any(|handler| handler["name"] == "Phantom" || handler["name"] == "Save")
    );
    let bindings = value["control_event"]["markup_bindings"]
        .as_array()
        .unwrap();
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0]["line"], 2);
    assert_eq!(
        bindings[0]["source_blake3"],
        blake3::hash(PAGE.as_bytes()).to_hex().to_string()
    );
    assert!(
        value["source_members"]
            .as_array()
            .unwrap()
            .iter()
            .any(|member| member["file"] == "Entry.aspx.vb"
                && member["excerpt"].as_str().unwrap().contains("PickPlan"))
    );
    assert!(
        value["paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|path| path["target_name"] == "Preferences"
                && path["kind"] == "inline_sql_writes_table")
    );
    assert!(
        value["control_event"]["interpretation"]
            .as_str()
            .unwrap()
            .contains("actual execution are not established")
    );
}

#[test]
fn explicit_handles_events_preserve_source_admission_and_existing_click_actions() {
    let value = trace("event enabled.CheckedChanged", false);
    assert!(value["handlers"].as_array().unwrap().iter().any(|handler| {
        handler["name"] == "Toggle"
            && handler["binding"]
                .as_str()
                .unwrap()
                .contains("Handles enabled.CheckedChanged")
    }));
    let wrong = trace("event plan.CheckedChanged", false);
    assert!(
        !wrong["handlers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|handler| handler["name"] == "PickPlan" || handler["name"] == "Toggle")
    );
    assert!(
        trace("event plan.SelectedIndexChanged", true)["handlers"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        trace("click save", false)["handlers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|handler| handler["name"] == "Save")
    );
    assert!(
        control_events::requested(Some("event plan.SelectedIndexChanged"))
            .unwrap()
            .is_some()
    );
    assert!(control_events::requested(Some("event plan")).is_err());
}
