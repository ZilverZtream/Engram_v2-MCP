//! Existing project-base source is context, not a guarantee that a lifecycle path runs.
use anyhow::{Result, ensure};
use engram_core::RelPath;
use engram_graph::{Edge, EdgeKind, GraphStore, Node};
use engram_server::services::request_trace::{
    execution,
    routing::{self, Source},
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
    fn new() -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new().prefix("bc-").tempdir_in(parent)?;
        let graph = Arc::new(GraphStore::open(&owned.path().join("graph.redb"))?);
        let files:Vec<(String,String)>=vec![
            ("Site/Global.asax".into(),"<%@ Application %>\n".into()),
            ("Site/Host.aspx".into(),"<%@ Page CodeFile='Host.aspx.vb' Inherits='Root' %>\n<asp:Button ID='save' OnClick='Save' runat='server' />\n".into()),
            ("Site/Host.aspx.vb".into(),"Public Class Root\nInherits Base\nPublic Sub RootLoad() Handles Me.Load\nEnd Sub\nPublic Sub Save()\nEnd Sub\nPublic Class Nested\nInherits Wrong\nEnd Class\nEnd Class\n".into()),
            ("Site/Base.vb".into(),"Public Class Base\nInherits Grand\nPublic Sub Prepare() Handles Me.PreInit\nDim db As New DataContext()\ndb.ExecuteCommand(\"UPDATE dbo.Audit SET Prepared=1\")\nIf useAlternate Then Me.MasterPageFile = \"~/Other.master\"\nEnd Sub\nEnd Class\n".into()),
            ("Site/Grand.vb".into(),"Public Class Grand\nPublic Sub GrandLoad() Handles Me.Load\nEnd Sub\nEnd Class\n".into()),
            ("Site/Wrong.vb".into(),"Public Class Wrong\nPublic Sub WrongPreInit() Handles Me.PreInit\nEnd Sub\nEnd Class\n".into()),
        ];
        let mut nodes = Vec::new();
        let mut selected = BTreeMap::new();
        for (path, text) in &files {
            selected.insert(path.clone(), 1);
            nodes.push(Node {
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
            });
        }
        for (id, kind, name, path, start, end) in [
            ("root-type", "class", "Root", "Site/Host.aspx.vb", 1, 10),
            (
                "root-nested",
                "class",
                "Root.Nested",
                "Site/Host.aspx.vb",
                7,
                9,
            ),
            ("base-type", "class", "Base", "Site/Base.vb", 1, 8),
            ("grand-type", "class", "Grand", "Site/Grand.vb", 1, 4),
            ("wrong-type", "class", "Wrong", "Site/Wrong.vb", 1, 4),
            (
                "root-load",
                "function",
                "Root.RootLoad",
                "Site/Host.aspx.vb",
                3,
                4,
            ),
            (
                "root-save",
                "function",
                "Root.Save",
                "Site/Host.aspx.vb",
                5,
                6,
            ),
            (
                "base-prepare",
                "function",
                "Base.Prepare",
                "Site/Base.vb",
                3,
                7,
            ),
            (
                "grand-load",
                "function",
                "Grand.GrandLoad",
                "Site/Grand.vb",
                2,
                3,
            ),
            (
                "wrong-preinit",
                "function",
                "Wrong.WrongPreInit",
                "Site/Wrong.vb",
                2,
                3,
            ),
        ] {
            nodes.push(Node {
                node_id: id.into(),
                node_type: kind.into(),
                name: name.into(),
                namespace: "memory".into(),
                language: "vbnet".into(),
                file_path: RelPath::new(path),
                start_line: start,
                end_line: end,
                generation: 1,
                metadata: None,
            });
        }
        graph.upsert_nodes("owned", &nodes)?;
        let fixture = Self {
            graph,
            owned,
            files,
            selected,
        };
        fixture.edge("root-type", "base-type", 1)?;
        fixture.edge("base-type", "grand-type", 1)?;
        fixture.edge("root-nested", "wrong-type", 1)?;
        Ok(fixture)
    }
    fn edge(&self, from: &str, to: &str, generation: u64) -> Result<()> {
        self.graph.upsert_edges(
            "owned",
            &[Edge {
                source_id: from.into(),
                target_id: to.into(),
                edge_kind: EdgeKind::InheritsFrom,
                namespace: "memory".into(),
                language: "vbnet".into(),
                weight: 1,
                generation,
                metadata: None,
                updated_at_ms: 1,
            }],
        )?;
        Ok(())
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
fn has(value: &Value, name: &str) -> bool {
    value["handlers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|handler| handler["name"] == name)
}

#[test]
fn selected_page_base_chain_adds_authored_context_and_sql_without_selecting_nested_type_bases()
-> Result<()> {
    let fixture = Fixture::new()?;
    let result =
        (|| {
            for action in ["load page", "event save.Click"] {
                let value = fixture.trace(action)?;
                ensure!(
                    has(&value, "RootLoad")
                        && has(&value, "Prepare")
                        && has(&value, "GrandLoad")
                        && !has(&value, "WrongPreInit"),
                    "Wrong root/base scope: {value}"
                );
                ensure!(has(&value, "Save") == action.starts_with("event"));
                ensure!(
                    value["handlers"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|handler| handler["name"] == "Prepare")
                        .count()
                        == 1,
                    "Base context duplicated: {value}"
                );
                let links = value["base_context"]["links"].as_array().unwrap();
                ensure!(links.len() == 2);
                ensure!(links.iter().any(
                    |link| link["target_file"] == "Site/Base.vb" && link["base_type"] == "Base"
                ));
                ensure!(
                    value["paths"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|hop| hop["kind"] == "inline_sql_writes_table"
                            && hop["target_name"] == "Audit"),
                    "Base database traversal missing: {value}"
                );
                ensure!(
                    value["source_members"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|member| member["excerpt"]
                            .as_str()
                            .is_some_and(|source| source.contains("Other.master")))
                );
                ensure!(
                    value["base_context"]["interpretation"]
                        .as_str()
                        .unwrap()
                        .contains("effective master")
                );
            }
            Ok(())
        })();
    let cleanup = fixture.close();
    result.and(cleanup)
}

#[test]
fn changed_or_unselected_base_and_mismatched_edge_revision_withhold_inherited_handlers()
-> Result<()> {
    for mode in ["source", "selection", "edge"] {
        let mut fixture = Fixture::new()?;
        match mode {
            "source" => fixture
                .files
                .iter_mut()
                .find(|(path, _)| path == "Site/Base.vb")
                .unwrap()
                .1
                .push_str("' changed\n"),
            "selection" => {
                fixture.selected.insert("Site/Base.vb".into(), 2);
            }
            _ => fixture.edge("root-type", "base-type", 2)?,
        }
        let result = (|| {
            let value = fixture.trace("load page")?;
            ensure!(
                has(&value, "RootLoad") && !has(&value, "Prepare") && !has(&value, "GrandLoad"),
                "{mode} admitted stale/unselected inheritance: {value}"
            );
            ensure!(!value["base_context"]["gaps"].as_array().unwrap().is_empty());
            Ok(())
        })();
        let cleanup = fixture.close();
        result.and(cleanup)?;
    }
    Ok(())
}
