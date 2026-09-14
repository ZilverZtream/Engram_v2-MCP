#![allow(clippy::unwrap_used)]

use engram_core::Config;
use engram_graph::EdgeKind;
use engram_server::{AppState, Engram, GetChangeSetRequest};
use serde_json::json;

#[tokio::test]
async fn indexed_bundle_connects_rendering_markup_to_static_assets() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    std::fs::create_dir_all(root.join("App_Start")).unwrap();
    std::fs::create_dir_all(root.join("Views")).unwrap();
    std::fs::create_dir_all(root.join("Scripts")).unwrap();
    std::fs::create_dir_all(root.join("Content")).unwrap();
    std::fs::create_dir_all(root.join("Services")).unwrap();
    std::fs::create_dir_all(root.join("Pages")).unwrap();
    std::fs::write(
        root.join("App_Start/BundleConfig.cs"),
        r#"public static class BundleConfig {
 public static void RegisterBundles(BundleCollection bundles) {
  bundles.Add(new ScriptBundle("~/Bundles/App").Include("~/Scripts/app.js"));
  bundles.Add(new StyleBundle("~/Styles/App").Include("~/Content/site.css"));
 }
}"#,
    )
    .unwrap();
    std::fs::write(
        root.join("Views/_Layout.cshtml"),
        r#"@Styles.Render("~/styles/app")
<main>@RenderBody()</main>
@System.Web.Optimization.Scripts.Render("/bundles/app")"#,
    )
    .unwrap();
    std::fs::write(
        root.join("Scripts/app.js"),
        "function authenticationSession() { window.app = true; }",
    )
    .unwrap();
    std::fs::write(root.join("Content/site.css"), "body { color: black; }").unwrap();
    std::fs::write(
        root.join("Services/AuthenticationService.cs"),
        "public class AuthenticationService { public void AuthenticationSession() {} }",
    )
    .unwrap();
    std::fs::write(
        root.join("Pages/DashboardPage.cs"),
        "public class DashboardPage { public void Load() { new AuthenticationService().AuthenticationSession(); } }",
    )
    .unwrap();

    let (state, _) = AppState::new(Config {
        data_dir: temp.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })
    .unwrap();
    let engram = Engram::new(state.clone());
    engram
        .handle_index_project(
            serde_json::from_value(json!({
                "directory": root,
                "project_name": "asset-bundle-graph",
                "project_type": "dotnet_webforms_cs",
                "wait": true
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let project_id = &state.registry.list_projects().unwrap()[0].project_id;
    let graph = &state.graph;

    let script_bundle = "bundle:~/bundles/app";
    let style_bundle = "bundle:~/styles/app";
    let layout = "file:Views/_Layout.cshtml";
    assert!(graph.get_node(project_id, script_bundle).unwrap().is_some());
    assert!(graph.get_node(project_id, style_bundle).unwrap().is_some());

    let edges = graph
        .list_edges(project_id, Some(EdgeKind::IncludesFile))
        .unwrap();
    for (source, target) in [
        ("file:App_Start/BundleConfig.cs", script_bundle),
        (script_bundle, "file:Scripts/app.js"),
        (layout, script_bundle),
        (style_bundle, "file:Content/site.css"),
        (layout, style_bundle),
    ] {
        assert!(
            edges
                .iter()
                .any(|edge| edge.source_id == source && edge.target_id == target),
            "missing {source} -> {target}; edges={edges:#?}"
        );
    }

    // Reverse causal traversal can now move from an edited asset through its
    // bundle to every rendering artifact without filename heuristics.
    assert!(graph
        .find_incoming_edges(
            project_id,
            Some(EdgeKind::IncludesFile),
            "file:Scripts/app.js",
            10
        )
        .unwrap()
        .iter()
        .any(|(id, _)| id == script_bundle));
    assert!(graph
        .find_incoming_edges(project_id, Some(EdgeKind::IncludesFile), script_bundle, 10,)
        .unwrap()
        .iter()
        .any(|(id, _)| id == layout));

    let request: GetChangeSetRequest = serde_json::from_value(json!({
        "project_id": project_id,
        "story": "Change authentication session behavior",
        "concepts": ["authenticationSession"],
        "output_json": true,
        "detail": "full"
    }))
    .unwrap();
    let response = engram.handle_get_change_set(request).await.unwrap();
    let payload: serde_json::Value =
        serde_json::from_str(&response.content[0].as_text().unwrap().text).unwrap();
    let paths = payload["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|file| file["path"].as_str())
        .collect::<Vec<_>>();
    assert!(paths.contains(&"Scripts/app.js"), "{payload}");
    assert!(
        paths.contains(&"Services/AuthenticationService.cs"),
        "{payload}"
    );
    let dependency_paths = payload["asset_dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|file| file["path"].as_str())
        .collect::<Vec<_>>();
    assert!(
        dependency_paths.contains(&"Views/_Layout.cshtml"),
        "{payload}"
    );
    assert!(
        dependency_paths.contains(&"App_Start/BundleConfig.cs"),
        "{payload}"
    );
    assert!(
        payload["coverage"]["asset_graph"]["hits"]
            .as_u64()
            .is_some_and(|hits| hits >= 2),
        "{payload}"
    );
    let caller_paths = payload["caller_dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|file| file["path"].as_str())
        .collect::<Vec<_>>();
    assert!(caller_paths.contains(&"Pages/DashboardPage.cs"), "{payload}");
    assert!(
        payload["coverage"]["caller_graph"]["hits"]
            .as_u64()
            .is_some_and(|hits| hits >= 1),
        "{payload}"
    );
    let primary_count = payload["files"].as_array().unwrap().iter()
        .filter(|row| row["row_id"].is_string()).count() as u64;
    assert_eq!(payload["reconciliation"]["primary_rows"], primary_count);
    assert_eq!(
        payload["reconciliation"]["asset_rows"],
        payload["asset_dependencies"].as_array().unwrap().len() as u64
    );
    assert_eq!(
        payload["reconciliation"]["caller_rows"],
        payload["caller_dependencies"].as_array().unwrap().len() as u64
    );
    assert!(payload["reconciliation"]["receipt_id"].as_str()
        .is_some_and(|value| value.starts_with("sha256:") && value.len() == 71));
    assert!(payload["asset_dependencies"].as_array().unwrap().iter()
        .all(|row| row["row_id"].as_str().is_some_and(|id| id.starts_with('A'))
            && row["evidence_class"] == "structural_dependency"
            && row["causal_chain"].is_string()
            && row["impact_question"].is_string()
            && row["exclusion_evidence_required"].is_string()));
    assert!(payload["caller_dependencies"].as_array().unwrap().iter()
        .all(|row| row["row_id"].as_str().is_some_and(|id| id.starts_with('C'))
            && row["evidence_class"] == "direct_behavioral_consumer"
            && row["causal_chain"].is_string()
            && row["impact_question"].is_string()
            && row["exclusion_evidence_required"].is_string()));
}
