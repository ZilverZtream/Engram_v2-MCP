//! Wanted HTTP-handler entry behavior, independent of ASPX lifecycle conventions.
use anyhow::{Result, ensure};
use engram_core::RelPath;
use engram_graph::{GraphStore, Node};
use engram_server::services::request_trace::{
    execution,
    routing::{self, Source},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

const VB: &str = "Public Class Download\nImplements System.Web.IHttpHandler\nPublic Sub Deliver(context As System.Web.HttpContext) Implements System.Web.IHttpHandler.ProcessRequest\nDim db As New DataContext()\ndb.ExecuteCommand(\"UPDATE dbo.DownloadLog SET Requested=1\")\nEnd Sub\nPublic Sub Page_Load()\nEnd Sub\nPublic ReadOnly Property IsReusable As Boolean Implements System.Web.IHttpHandler.IsReusable\nGet\nReturn False\nEnd Get\nEnd Property\nEnd Class\nPublic Class Other\nPublic Sub Deliver()\nEnd Sub\nEnd Class\n";

struct Fixture {
    // Fields drop in declaration order, including during unwinding.
    graph: Arc<GraphStore>,
    owned: tempfile::TempDir,
    files: Vec<(String, String)>,
    selected: BTreeMap<String, u64>,
}

impl Fixture {
    fn new(binding: bool, expected: &str, stale: bool, declaration_only: bool) -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new().prefix("hh-").tempdir_in(parent)?;
        let graph = Arc::new(GraphStore::open(&owned.path().join("graph.redb"))?);
        let vb = if binding {
            VB.to_owned()
        } else {
            VB.replace(" Implements System.Web.IHttpHandler.ProcessRequest", "")
        };
        let files = vec![
            (
                "Site/Global.asax".to_owned(),
                "<%@ Application %>\n".to_owned(),
            ),
            (
                "Site/Download.ashx".to_owned(),
                format!("<%@ WebHandler Class='{expected}' CodeBehind='Download.ashx.vb' %>\n"),
            ),
            ("Site/Download.ashx.vb".to_owned(), vb),
        ];
        let mut selected = BTreeMap::new();
        let mut nodes = Vec::new();
        for (path, source) in &files {
            selected.insert(path.clone(), 1);
            nodes.push(Node {
                node_id: format!("file:{path}"),
                node_type: "file".into(),
                name: path.clone(),
                namespace: "memory".into(),
                file_path: RelPath::new(path),
                language: "vbnet".into(),
                start_line: 1,
                end_line: source.lines().count() as u32,
                generation: 1,
                metadata: Some(json!({"file_hash": if stale && path.ends_with(".vb") {
                    "stale".to_owned()
                } else { blake3::hash(source.as_bytes()).to_hex().to_string() }})),
            });
        }
        let path = "Site/Download.ashx.vb";
        for (kind, name, owner, start, end) in [
            (
                "class",
                "Download",
                "Download",
                1,
                if declaration_only { 1 } else { 14 },
            ),
            ("function", "Deliver", "Download", 3, 6),
            ("function", "Page_Load", "Download", 7, 8),
            ("class", "Other", "Other", 15, 18),
            ("function", "Deliver", "Other", 16, 17),
        ] {
            let qualified = if kind == "class" {
                name.to_owned()
            } else {
                format!("{owner}.{name}")
            };
            nodes.push(Node {
                node_id: engram_core::NodeId::symbol(kind, None, path, &qualified, start).0,
                node_type: kind.into(),
                name: name.into(),
                namespace: "code".into(),
                file_path: RelPath::new(path),
                language: "vbnet".into(),
                start_line: start,
                end_line: end,
                generation: 1,
                metadata: None,
            });
        }
        graph.upsert_nodes("owned", &nodes)?;
        Ok(Self {
            graph,
            owned,
            files,
            selected,
        })
    }

    fn trace(&self) -> Result<Value> {
        let sources: Vec<_> = self
            .files
            .iter()
            .map(|(path, content)| Source { path, content })
            .collect();
        let route = routing::resolve("GET /Download.ashx", &sources)?
            .candidates
            .remove(0);
        Ok(serde_json::to_value(execution::trace_page(
            &self.graph,
            "owned",
            &route,
            None,
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

#[test]
fn http_handler_interface_entry_retains_source_and_database_path_without_page_lifecycle()
-> Result<()> {
    for declaration_only in [false, true] {
        let fixture = Fixture::new(true, "Download", false, declaration_only)?;
        let result = (|| {
            let report = fixture.trace()?;
            let handlers = report["handlers"].as_array().unwrap();
            ensure!(
                handlers.len() == 1 && handlers[0]["name"] == "Deliver",
                "{report}"
            );
            ensure!(
                handlers[0]["binding"]
                    .as_str()
                    .unwrap()
                    .contains("IHttpHandler.ProcessRequest")
            );
            ensure!(
                report["paths"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|hop| hop["kind"] == "inline_sql_writes_table"
                        && hop["target_name"] == "DownloadLog"),
                "{report}"
            );
            ensure!(
                report["source_members"].as_array().unwrap().iter().any(
                    |member| member["source_blake3"]
                        == blake3::hash(VB.as_bytes()).to_hex().as_str()
                        && member["excerpt"]
                            .as_str()
                            .unwrap()
                            .contains("UPDATE dbo.DownloadLog")
                ),
                "{report}"
            );
            ensure!(
                report["gaps"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|gap| gap.as_str().unwrap().contains("Hosting"))
            );
            Ok(())
        })();
        fixture.close()?;
        result?;
    }
    Ok(())
}

#[test]
fn handler_names_cannot_replace_directive_contract_ownership_or_current_source() -> Result<()> {
    for (binding, expected, stale) in [
        (false, "Download", false),
        (true, "Other", false),
        (true, "Missing", false),
        (true, "Download", true),
    ] {
        let fixture = Fixture::new(binding, expected, stale, false)?;
        let result = (|| {
            let report = fixture.trace()?;
            ensure!(
                report["handlers"].as_array().unwrap().is_empty(),
                "{report}"
            );
            ensure!(!report["gaps"].as_array().unwrap().is_empty());
            Ok(())
        })();
        fixture.close()?;
        result?;
    }
    Ok(())
}
