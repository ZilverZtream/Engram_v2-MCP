//! Authored AJAX URL -> VB entry -> existing source-bound SQL traversal.
use engram_core::RelPath;
use engram_graph::{GraphStore, Node};
use engram_server::services::request_trace::{
    execution,
    routing::{self, Source},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

struct Fixture {
    graph: Arc<GraphStore>,
    _owned: tempfile::TempDir,
    files: Vec<(String, String)>,
    selected: BTreeMap<String, u64>,
}

fn node(id: &str, kind: &str, name: &str, file: &str, start: usize, end: usize) -> Node {
    Node {
        node_id: id.into(),
        node_type: kind.into(),
        name: name.into(),
        namespace: "code".into(),
        file_path: RelPath::new(file),
        start_line: start as u32,
        end_line: end as u32,
        language: "vbnet".into(),
        generation: 1,
        metadata: None,
    }
}

impl Fixture {
    fn new(files: &[(&str, &str)], stale: Option<&str>) -> Self {
        let owned = tempfile::tempdir().unwrap();
        let graph = Arc::new(GraphStore::open(&owned.path().join("graph.redb")).unwrap());
        let mut selected = BTreeMap::new();
        let mut nodes = Vec::new();
        for &(path, text) in files {
            selected.insert(path.to_owned(), 1);
            let mut file = node(
                &format!("file:{path}"),
                "file",
                path,
                path,
                1,
                text.lines().count(),
            );
            file.metadata = Some(
                json!({"file_hash":if stale==Some(path) {"stale".into()} else {blake3::hash(text.as_bytes()).to_hex().to_string()}}),
            );
            nodes.push(file);
        }
        graph.upsert_nodes("owned", &nodes).unwrap();
        Self {
            graph,
            _owned: owned,
            files: files
                .iter()
                .map(|(p, s)| (p.to_string(), s.to_string()))
                .collect(),
            selected,
        }
    }

    fn class(&self, file: &str, name: &str, methods: &[&str]) {
        let source = &self.files.iter().find(|(p, _)| p == file).unwrap().1;
        let lines: Vec<_> = source.lines().collect();
        let start = lines
            .iter()
            .position(|s| s.contains(&format!("Class {name}")))
            .unwrap()
            + 1;
        // Match the existing VB fallback: class declaration lines, qualified members.
        let end = start;
        let mut nodes = vec![node(
            &format!("class:{file}:{name}"),
            "class",
            name,
            file,
            start,
            end,
        )];
        for method in methods {
            let start = lines
                .iter()
                .position(|s| {
                    s.contains(&format!("Function {method}("))
                        || s.contains(&format!("Sub {method}("))
                })
                .unwrap();
            let end = start
                + lines[start..]
                    .iter()
                    .position(|s| ["End Function", "End Sub"].contains(&s.trim()))
                    .unwrap();
            nodes.push(node(
                &engram_core::NodeId::symbol(
                    "function",
                    None,
                    file,
                    &format!("{name}.{method}"),
                    (start + 1) as u32,
                )
                .0,
                "function",
                method,
                file,
                start + 1,
                end + 1,
            ));
        }
        self.graph.upsert_nodes("owned", &nodes).unwrap();
    }

    fn trace(&self, url: &str) -> Value {
        let sources: Vec<_> = self
            .files
            .iter()
            .map(|(p, s)| Source {
                path: p,
                content: s,
            })
            .collect();
        let mut routes = routing::resolve(url, &sources).unwrap();
        assert_eq!(routes.candidates.len(), 1);
        let route = routes.candidates.remove(0);
        let value = execution::trace_page(
            &self.graph,
            "owned",
            &route,
            None,
            true,
            false,
            &sources,
            &self.selected,
        )
        .unwrap();
        json!({"route":route,"trace":value})
    }
}

const GLOBAL: &str = "<%@ Application Language='VB' %>\n";
const PAGE: &str = "<%@ Page Language='VB' Inherits='Language' CodeFile='language.aspx.vb' AutoEventWireup='true' %>\n";
const VB: &str = "Partial Class Language\n<WebMethod(True)>\n<Script.Services.ScriptMethod(ResponseFormat:=ResponseFormat.Json)>\nPublic Shared Function change() As String\nDim db As New DataContext()\ndb.ExecuteCommand(\"UPDATE dbo.Preferences SET Value=1\")\nReturn \"ok\"\nEnd Function\nPublic Sub Page_Load()\nResponse.Write(\"page-only\")\nEnd Sub\nEnd Class\n";

#[test]
fn page_method_url_keeps_the_exact_entry_and_existing_sql_path_without_page_lifecycle() {
    let f = Fixture::new(
        &[
            ("Site/Global.asax", GLOBAL),
            ("Site/language.aspx", PAGE),
            ("Site/language.aspx.vb", VB),
        ],
        None,
    );
    f.class(
        "Site/language.aspx.vb",
        "Language",
        &["change", "Page_Load"],
    );
    let value = f.trace("POST /language.aspx/change?source=menu");
    assert_eq!(value["route"]["page"], "Site/language.aspx");
    assert_eq!(value["route"]["web_method"], "change");
    assert_eq!(value["trace"]["handlers"].as_array().unwrap().len(), 1);
    assert_eq!(value["trace"]["handlers"][0]["name"], "change");
    assert!(value["trace"]["paths"].as_array().unwrap().iter().any(|p| {
        p["kind"] == "inline_sql_writes_table"
            && p["target_name"] == "Preferences"
            && p["evidence"]["sql"]
                .as_str()
                .is_some_and(|sql| sql.contains("UPDATE dbo.Preferences"))
    }));
    assert!(
        value["trace"]["source_members"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["excerpt"]
                .as_str()
                .is_some_and(|s| s.contains("UPDATE dbo.Preferences")))
    );
    // Complete type ranges from a richer parser retain the same selected entry.
    let complete = node(
        "class:Site/language.aspx.vb:Language",
        "class",
        "Language",
        "Site/language.aspx.vb",
        1,
        VB.lines().count(),
    );
    f.graph.upsert_nodes("owned", &[complete]).unwrap();
    let ranged = f.trace("POST /language.aspx/change");
    assert_eq!(
        ranged["trace"]["handlers"][0]["node_id"],
        value["trace"]["handlers"][0]["node_id"]
    );
    let page = f.trace("GET /language.aspx");
    assert!(page["route"].get("web_method").is_none());
    assert_eq!(page["trace"]["handlers"][0]["name"], "Page_Load");
}

#[test]
fn asmx_class_resolves_its_app_code_partial_method_and_excludes_unrelated_same_named_types() {
    let service = "<%@ WebService Language='VB' CodeBehind='api.vb' Class='api' %>\n";
    let vb = "Partial Class api\n<System.Web.Services.WebMethod(True)>\nPublic Function getimg() As String\nReturn \"owned image response\"\nEnd Function\nEnd Class\n";
    let wrong_type = vb.replace("Class api", "Class OtherApi");
    let f = Fixture::new(
        &[
            ("Site/Global.asax", GLOBAL),
            ("Site/api.asmx", service),
            ("Site/App_Code/api-json/api-images.vb", vb),
            ("Site/Other/api.vb", vb),
            ("Site/App_Code/OtherApi.vb", &wrong_type),
        ],
        None,
    );
    f.class("Site/App_Code/api-json/api-images.vb", "api", &["getimg"]);
    f.class("Site/Other/api.vb", "api", &["getimg"]);
    f.class("Site/App_Code/OtherApi.vb", "OtherApi", &["getimg"]);
    let value = f.trace("POST /api.asmx/getimg");
    assert_eq!(value["trace"]["handlers"].as_array().unwrap().len(), 1);
    assert_eq!(
        value["trace"]["source_members"][0]["source_blake3"],
        blake3::hash(vb.as_bytes()).to_hex().as_str()
    );
    assert_eq!(
        value["trace"]["handlers"][0]["file"],
        "Site/App_Code/api-json/api-images.vb"
    );
    assert!(
        value["trace"]["source_members"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["excerpt"]
                .as_str()
                .is_some_and(|s| s.contains("owned image response")))
    );
    assert!(
        value["trace"]["gaps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g.as_str().unwrap().contains("Hosting/configuration"))
    );
}

#[test]
fn a_url_suffix_does_not_promote_plain_instance_private_or_stale_page_methods_to_http_entries() {
    for (body, stale) in [
        (VB.replace("<WebMethod(True)>\n", ""), false),
        (
            VB.replace("Public Shared Function", "Public Function"),
            false,
        ),
        (
            VB.replace("Public Shared Function", "Private Shared Function"),
            false,
        ),
        (VB.to_owned(), true),
    ] {
        let f = Fixture::new(
            &[
                ("Site/Global.asax", GLOBAL),
                ("Site/language.aspx", PAGE),
                ("Site/language.aspx.vb", &body),
            ],
            stale.then_some("Site/language.aspx.vb"),
        );
        f.class(
            "Site/language.aspx.vb",
            "Language",
            &["change", "Page_Load"],
        );
        let value = f.trace("POST /language.aspx/change");
        assert!(value["trace"]["handlers"].as_array().unwrap().is_empty());
        assert!(value["trace"]["paths"].as_array().unwrap().is_empty());
    }
}
