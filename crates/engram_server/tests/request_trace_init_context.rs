//! Initialization source is useful request context, not proof of lifecycle execution.
use anyhow::{Result, ensure};
use engram_core::RelPath;
use engram_graph::{GraphStore, Node};
use engram_server::services::{
    business_logic_service::extract_logic_methods,
    request_trace::{
        execution,
        routing::{self, Source},
    },
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

struct Fixture {
    graph: Arc<GraphStore>,
    owned: tempfile::TempDir,
    files: Vec<(String, String)>,
    selected: BTreeMap<String, u64>,
}
impl Fixture {
    fn new(automatic: bool) -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new().prefix("ic-").tempdir_in(parent)?;
        let graph = Arc::new(GraphStore::open(&owned.path().join("graph.redb"))?);
        let files: Vec<(String, String)> = vec![
            ("Site/Global.asax".into(), "<%@ Application %>\n".into()),
            ("Site/Host.aspx".into(), format!("<%@ Page CodeFile='Host.aspx.vb' MasterPageFile='~/Layout.master' AutoEventWireup='{automatic}' %>\n<%@ Register Src='Child.ascx' TagPrefix='uc' TagName='Widget' %>\n<uc:Widget ID='widget' runat='server' />\n")),
            ("Site/Host.aspx.vb".into(), "Public Class Host\nPublic Sub RootInit() Handles Me.Init\nDim db As New DataContext()\ndb.ExecuteCommand(\"UPDATE dbo.Audit SET Prepared=1\")\nEnd Sub\nPublic Sub RootLoad() Handles Me.Load\nEnd Sub\nPublic Sub Page_Init()\nDim conventionalSetup = True\nEnd Sub\nEnd Class\n".into()),
            ("Site/Child.ascx".into(), "<%@ Control CodeFile='Child.ascx.vb' %>\n<asp:Button ID='save' runat='server' OnClick='Save' />\n".into()),
            ("Site/Child.ascx.vb".into(), "Public Class Child\nPublic Sub ChildInit() Handles MyBase.Init\nDim prepared = True\nEnd Sub\nPublic Sub ChildLoad() Handles Me.Load\nEnd Sub\nPublic Sub Save()\nEnd Sub\nEnd Class\n".into()),
            ("Site/Layout.master".into(), "<%@ Master CodeFile='Layout.master.vb' %>\n".into()),
            ("Site/Layout.master.vb".into(), "Public Class Layout\nPublic Sub MasterInit() Handles Me.Init\nEnd Sub\nPublic Sub MasterLoad() Handles Me.Load\nEnd Sub\nEnd Class\n".into()),
        ];
        let mut selected = BTreeMap::new();
        let mut nodes = Vec::new();
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
                for method in extract_logic_methods(text, "vb") {
                    let mut node = file.clone();
                    node.node_id = format!("{path}:{}", method.start_line);
                    node.node_type = "function".into();
                    node.name = method.name;
                    node.start_line = method.start_line;
                    node.end_line = method.start_line + method.body.lines().count() as u32 - 1;
                    node.metadata = None;
                    nodes.push(node);
                }
            }
        }
        graph.upsert_nodes("owned", &nodes)?;
        Ok(Self {
            graph,
            owned,
            files,
            selected,
        })
    }
    fn trace(&self, action: &str) -> Result<Value> {
        let sources: Vec<_> = self
            .files
            .iter()
            .map(|(path, text)| Source {
                path,
                content: text,
            })
            .collect();
        let route = routing::resolve("/Host.aspx", &sources)?
            .candidates
            .remove(0);
        Ok(serde_json::to_value(execution::trace_page(
            &self.graph,
            "owned",
            &route,
            Some(action),
            true,
            false,
            &sources,
            &self.selected,
        )?)?)
    }
    fn close(self) -> Result<()> {
        drop(self.graph);
        self.owned.close()?;
        Ok(())
    }
}
fn handler<'a>(value: &'a Value, name: &str) -> Option<&'a Value> {
    value["handlers"]
        .as_array()?
        .iter()
        .find(|handler| handler["name"] == name)
}

#[test]
fn page_control_and_master_initialization_join_the_requested_control_event() -> Result<()> {
    let mut fixture = Fixture::new(false)?;
    let result = (|| {
        let value = fixture.trace("event widget/save.Click")?;
        for name in [
            "RootInit",
            "RootLoad",
            "ChildInit",
            "ChildLoad",
            "MasterInit",
            "MasterLoad",
            "Save",
        ] {
            ensure!(handler(&value, name).is_some(), "Missing {name}: {value}");
        }
        ensure!(
            handler(&value, "Page_Init").is_none(),
            "Unwired convention admitted: {value}"
        );
        ensure!(
            handler(&value, "RootInit").unwrap()["binding"]
                .as_str()
                .unwrap()
                .contains("Handles Me.Init")
        );
        ensure!(
            value["paths"]
                .as_array()
                .unwrap()
                .iter()
                .any(
                    |hop| hop["kind"] == "inline_sql_writes_table" && hop["target_name"] == "Audit"
                ),
            "Initialization lost existing database traversal: {value}"
        );
        ensure!(
            value["master_pages"]["interpretation"]
                .as_str()
                .unwrap()
                .contains("not compiler hosting proof")
        );
        fixture
            .files
            .iter_mut()
            .find(|(path, _)| path == "Site/Child.ascx.vb")
            .unwrap()
            .1
            .push_str("' changed\n");
        let stale = fixture.trace("event widget/save.Click")?;
        ensure!(
            handler(&stale, "ChildInit").is_none() && handler(&stale, "Save").is_none(),
            "Changed source admitted: {stale}"
        );
        ensure!(
            handler(&stale, "RootInit").is_some() && handler(&stale, "MasterInit").is_some(),
            "Independent admitted context lost: {stale}"
        );
        Ok(())
    })();
    let cleanup = fixture.close();
    result.and(cleanup)
}

#[test]
fn page_init_convention_requires_automatic_wiring_or_explicit_source_selection() -> Result<()> {
    for automatic in [false, true] {
        let fixture = Fixture::new(automatic)?;
        let result = (|| {
            let value = fixture.trace("load page")?;
            ensure!(handler(&value, "RootInit").is_some() && handler(&value, "RootLoad").is_some());
            ensure!(
                handler(&value, "Page_Init").is_some() == automatic,
                "Automatic binding ignored authored value {automatic}: {value}"
            );
            if automatic {
                ensure!(
                    handler(&value, "Page_Init").unwrap()["binding"]
                        .as_str()
                        .unwrap()
                        .contains("AutoEventWireup=true")
                );
            } else {
                let explicit = fixture.trace("Page_Init")?;
                ensure!(
                    handler(&explicit, "Page_Init").unwrap()["binding"]
                        .as_str()
                        .unwrap()
                        .contains("HTTP event binding not established"),
                    "Explicit method fabricated dispatch: {explicit}"
                );
            }
            Ok(())
        })();
        let cleanup = fixture.close();
        result.and(cleanup)?;
    }
    Ok(())
}
