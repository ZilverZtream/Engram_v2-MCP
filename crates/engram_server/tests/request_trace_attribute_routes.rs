//! Attribute routing must retain its declared action and source admission.
use anyhow::{Result, ensure};
use engram_core::RelPath;
use engram_graph::{GraphStore, Node};
use engram_server::services::request_trace::{
    api,
    routing::{self, Source},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

const PATH: &str = "Site/App_Code/ApprovalsController.vb";
const VB: &str = "<RoutePrefix(\"api/v2/projects/{project_id}/approvals\")>\nPublic Class ApprovalsController\nInherits ApiController\n<HttpGet, Route(\"\")>\nPublic Function GetAll(project_id As Integer) As String\nReturn \"all\"\nEnd Function\n<HttpGet, Route(\"{id:int}\")>\nPublic Function GetById(project_id As Integer, id As Integer) As String\nDim db As New DataContext()\ndb.ExecuteCommand(\"UPDATE dbo.Approvals SET Seen = 1\")\nReturn \"one\"\nEnd Function\nEnd Class\n";

struct Fixture {
    graph: Arc<GraphStore>,
    owned: tempfile::TempDir,
    files: Vec<(String, String)>,
    selected: BTreeMap<String, u64>,
}

impl Fixture {
    fn new(enabled: bool, stale: bool) -> Result<Self> {
        Self::with_source(enabled, stale, VB)
    }

    fn with_source(enabled: bool, stale: bool, source: &str) -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new().prefix("atr-").tempdir_in(parent)?;
        let graph = Arc::new(GraphStore::open(&owned.path().join("graph.redb"))?);
        let registration = if enabled {
            "config.MapHttpAttributeRoutes()"
        } else {
            "' config.MapHttpAttributeRoutes()"
        };
        let files: Vec<(String, String)> = vec![
            (
                "Site/Global.asax".into(),
                "<%@ Application Language='VB' %>\n".into(),
            ),
            (
                "Site/App_Code/WebApiConfig.vb".into(),
                format!(
                    "Public Module WebApiConfig\n{registration}\nconfig.Routes.MapHttpRoute(\"default\", \"api/{{controller}}/{{id}}\")\nEnd Module\n"
                ),
            ),
            (PATH.into(), source.into()),
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
                metadata: Some(
                    json!({"file_hash":if stale && path==PATH { "stale".to_owned() }
                    else { blake3::hash(source.as_bytes()).to_hex().to_string() }}),
                ),
            });
        }
        for (kind, name, first, last) in [
            ("class", "ApprovalsController", 1, 14),
            ("function", "ApprovalsController.GetAll", 4, 7),
            ("function", "ApprovalsController.GetById", 8, 13),
        ] {
            nodes.push(Node {
                node_id: engram_core::NodeId::symbol(kind, None, PATH, name, first).0,
                node_type: kind.into(),
                name: name.into(),
                namespace: "code".into(),
                file_path: RelPath::new(PATH),
                language: "vbnet".into(),
                start_line: first,
                end_line: last,
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

    fn sources(&self) -> Vec<Source<'_>> {
        self.files
            .iter()
            .map(|(path, content)| Source { path, content })
            .collect()
    }

    fn resolve(&self, entry: &str) -> Result<routing::Routing> {
        let sources = self.sources();
        let mut routes = routing::resolve(entry, &sources)?;
        api::extend_attribute_routes(&self.graph, "owned", &mut routes, &sources, &self.selected)?;
        Ok(routes)
    }

    fn trace(&self, routes: &routing::Routing, action: Option<&str>) -> Result<Value> {
        ensure!(routes.api_candidates.len() == 1);
        api::trace(
            &self.graph,
            "owned",
            &routes.api_candidates[0],
            routes.method.as_deref(),
            action,
            true,
            false,
            &self.sources(),
            &self.selected,
        )
    }

    fn close(self) -> Result<()> {
        let Self { graph, owned, .. } = self;
        drop(graph);
        owned.close()?;
        Ok(())
    }
}

#[test]
fn attribute_collection_and_item_routes_keep_their_exact_action_and_database_source() -> Result<()>
{
    let fixture = Fixture::new(true, false)?;
    for (url, name, database) in [
        ("GET /api/v2/projects/42/approvals", "GetAll", false),
        ("GET /api/v2/projects/42/approvals/7", "GetById", true),
    ] {
        let routes = fixture.resolve(url)?;
        ensure!(routes.api_candidates.len() == 1);
        let route = &routes.api_candidates[0];
        let declared = route.declared_action.as_ref().unwrap();
        ensure!(
            declared.name == name
                && declared
                    .registration
                    .source_path
                    .ends_with("WebApiConfig.vb")
        );
        ensure!(
            declared
                .registration
                .evidence
                .contains("config.MapHttpAttributeRoutes()")
        );
        ensure!(route.parameters["project_id"] == "42");
        ensure!(route.source_blake3 == blake3::hash(VB.as_bytes()).to_hex().to_string());
        let trace = fixture.trace(&routes, None)?;
        let handlers = trace["controllers"][0]["handlers"].as_array().unwrap();
        ensure!(
            handlers.len() == 1 && handlers[0]["name"].as_str().unwrap().ends_with(name),
            "{trace}"
        );
        if database {
            ensure!(route.parameters["id"] == "7" && route.pattern.ends_with("{id:int}"));
            ensure!(
                trace["controllers"][0]["paths"]
                    .to_string()
                    .contains("dbo.Approvals"),
                "{trace}"
            );
        }
        ensure!(
            !routes
                .gaps
                .iter()
                .any(|gap| gap.starts_with("No route/page candidate"))
        );
    }
    fixture.close()
}

#[test]
fn registration_verbs_and_current_source_gate_attributes_while_conventional_candidates_remain()
-> Result<()> {
    let fixture = Fixture::new(true, false)?;
    for url in [
        "POST /api/v2/projects/42/approvals",
        "GET /api/v2/projects/42/approvals/not-an-int",
    ] {
        ensure!(fixture.resolve(url)?.api_candidates.is_empty());
    }
    let conventional = fixture.resolve("GET /api/approvals/7")?;
    ensure!(
        conventional.api_candidates.len() == 1
            && conventional.api_candidates[0].declared_action.is_none()
    );
    let trace = fixture.trace(&conventional, Some("GetById"))?;
    ensure!(
        trace["controllers"][0]["handlers"]
            .as_array()
            .unwrap()
            .len()
            == 1
    );
    fixture.close()?;
    for (enabled, stale) in [(false, false), (true, true)] {
        let fixture = Fixture::new(enabled, stale)?;
        ensure!(
            fixture
                .resolve("GET /api/v2/projects/42/approvals")?
                .api_candidates
                .is_empty()
        );
        fixture.close()?;
    }
    Ok(())
}

#[test]
fn literal_method_routes_work_without_a_class_prefix() -> Result<()> {
    let source = VB
        .replace(
            "<RoutePrefix(\"api/v2/projects/{project_id}/approvals\")>",
            "' Full method routes",
        )
        .replace(
            "Route(\"\")",
            "Route(\"api/v2/projects/{project_id}/approvals\")",
        )
        .replace(
            "Route(\"{id:int}\")",
            "Route(\"api/v2/projects/{project_id}/approvals/{id:int}\")",
        );
    let fixture = Fixture::with_source(true, false, &source)?;
    let routes = fixture.resolve("GET /api/v2/projects/42/approvals/12")?;
    ensure!(routes.api_candidates.len() == 1);
    let action = routes.api_candidates[0].declared_action.as_ref().unwrap();
    ensure!(action.name == "GetById" && action.route_prefix.is_empty());
    ensure!(routes.api_candidates[0].parameters["id"] == "12");
    fixture.close()
}
