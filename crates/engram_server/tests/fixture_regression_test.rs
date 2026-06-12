#![allow(clippy::unwrap_used)]
use engram_core::Config;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use std::path::PathBuf;

#[tokio::test]
async fn test_fixture_dotnet_webforms_cs() {
    let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("tests")
        .join("fixtures")
        .join("dotnet_webforms_cs");

    let tmp_root = tempfile::tempdir().unwrap();
    let data_dir = tmp_root.path().join("engram_data");

    let cfg = Config {
        allowed_roots: vec![fixture_dir.clone()],
        data_dir,
        max_project_files: Some(100),
        max_project_bytes: Some(10 * 1024 * 1024),
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());

    // 1. Index project
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: fixture_dir.to_string_lossy().to_string(),
            project_name: "WebFormsCsFixture".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsCs,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();

    let projects = state.registry.list_projects().unwrap();
    let project_id = &projects[0].project_id;

    // Resolve symbol edges
    state.graph.resolve_symbol_edges(project_id).unwrap();

    // 2. Assert path exists: page -> handler -> DAL -> SQL
    // btnSubmit -> btnSubmit_Click -> InsertLog -> SQL
    let res = engram
        .trace_ui_event(Parameters(engram_server::TraceUiEventRequest {
            project_id: project_id.clone(),
            page_path: "Default.aspx".to_string(),
            control_id: Some("btnSubmit".to_string()),
            handler_fqn: None,
            max_hops: 5,
            max_paths: 1,
        }))
        .await
        .unwrap();

    let text = &res.content[0].as_text().unwrap().text;

    assert!(
        text.contains("LegacyApp.DefaultPage.btnSubmit_Click"),
        "Should find handler"
    );
    assert!(
        text.contains("LegacyApp.DataAccess.InsertLog"),
        "Should find DAL method"
    );
    assert!(text.contains("sql:inline:"), "Should reach SQL");

    // 3. Check stable control ID from designer
    // The class contains the designer field, which should link to the control.
    let nodes = state
        .graph
        .query_nodes(project_id, Some("control"), Some("gvData"), None, 1)
        .unwrap();
    assert!(!nodes.is_empty(), "Should find gvData control node");
    assert_eq!(nodes[0].node_id, "control:Default.aspx:gvData");
}

#[tokio::test]
async fn test_fixture_dotnet_webforms_vb() {
    let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("tests")
        .join("fixtures")
        .join("dotnet_webforms_vb");

    let tmp_root = tempfile::tempdir().unwrap();
    let data_dir = tmp_root.path().join("engram_data");

    let cfg = Config {
        allowed_roots: vec![fixture_dir.clone()],
        data_dir,
        max_project_files: Some(100),
        max_project_bytes: Some(10 * 1024 * 1024),
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());

    // 1. Index project
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: fixture_dir.to_string_lossy().to_string(),
            project_name: "WebFormsVbFixture".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsVb,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();

    let projects = state.registry.list_projects().unwrap();
    let project_id = &projects[0].project_id;

    // Resolve symbol edges
    state.graph.resolve_symbol_edges(project_id).unwrap();

    // 2. Assert path exists: btnSave (Handles) -> SaveOrder -> Stored Proc
    let res = engram
        .trace_ui_event(Parameters(engram_server::TraceUiEventRequest {
            project_id: project_id.clone(),
            page_path: "Order.aspx".to_string(),
            control_id: Some("btnSave".to_string()),
            handler_fqn: None,
            max_hops: 5,
            max_paths: 1,
        }))
        .await
        .unwrap();

    let text = &res.content[0].as_text().unwrap().text;
    println!("VB FIXTURE TRACE OUTPUT:\n{text}");

    assert!(
        text.contains("LegacyApp.OrderPage.btnSave_Click"),
        "Should find Handles handler"
    );
    assert!(
        text.contains("LegacyApp.DataLayer.SaveOrder"),
        "Should find DAL method"
    );
    assert!(
        text.contains("sql:stored_proc:proc_SaveOrder"),
        "Should reach Stored Proc"
    );

    // 3. Assert path exists: lbCancel (OnClick) -> CancelOrder -> Inline SQL
    let res2 = engram
        .trace_ui_event(Parameters(engram_server::TraceUiEventRequest {
            project_id: project_id.clone(),
            page_path: "Order.aspx".to_string(),
            control_id: Some("lbCancel".to_string()),
            handler_fqn: None,
            max_hops: 5,
            max_paths: 1,
        }))
        .await
        .unwrap();

    let text2 = &res2.content[0].as_text().unwrap().text;

    assert!(
        text2.contains("LegacyApp.OrderPage.lbCancel_Click"),
        "Should find OnClick handler"
    );
    assert!(
        text2.contains("LegacyApp.DataLayer.CancelOrder"),
        "Should find DAL method"
    );
    assert!(text2.contains("sql:inline:"), "Should reach Inline SQL");
}

#[tokio::test]
async fn test_fixture_dotnet_webforms_cs_frontend_bridge() {
    let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("tests")
        .join("fixtures")
        .join("dotnet_webforms_cs_frontend_bridge");

    let tmp_root = tempfile::tempdir().unwrap();
    let data_dir = tmp_root.path().join("engram_data");

    let cfg = Config {
        allowed_roots: vec![fixture_dir.clone()],
        data_dir,
        max_project_files: Some(200),
        max_project_bytes: Some(10 * 1024 * 1024),
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());

    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: fixture_dir.to_string_lossy().to_string(),
            project_name: "WebFormsCsFrontendBridgeFixture".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsCs,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();

    let projects = state.registry.list_projects().unwrap();
    let project_id = &projects[0].project_id;

    state.graph.resolve_symbol_edges(project_id).unwrap();

    // 1) Frontend postback trigger should connect to control and then reach handler/DAL/SQL.
    let postback_edges = state
        .graph
        .list_edges(project_id, Some(engram_graph::EdgeKind::TriggersPostback))
        .unwrap();
    assert!(
        postback_edges
            .iter()
            .any(|e| e.source_id.contains("pageMethods.ts")
                && e.target_id == "control:Search.aspx:btnSearch"),
        "TypeScript __doPostBack should point to btnSearch control"
    );

    let ui_trace = engram
        .trace_ui_event(Parameters(engram_server::TraceUiEventRequest {
            project_id: project_id.clone(),
            page_path: "Search.aspx".to_string(),
            control_id: Some("btnSearch".to_string()),
            handler_fqn: None,
            max_hops: 6,
            max_paths: 1,
        }))
        .await
        .unwrap();
    let ui_text = &ui_trace.content[0].as_text().unwrap().text;
    println!("BRIDGE TRACE OUTPUT:\n{ui_text}");
    assert!(
        ui_text.contains("LegacyApp.FrontendBridgePage.btnSearch_Click"),
        "Should reach postback click handler"
    );
    assert!(
        ui_text.contains("LegacyApp.CustomerDal.LookupCustomer"),
        "Should reach DAL method from handler"
    );
    assert!(ui_text.contains("sql:inline:"), "Should reach inline SQL");

    // 2) Frontend AJAX calls should be captured for both ASMX and API endpoint shapes.
    let api_edges = state
        .graph
        .list_edges(project_id, Some(engram_graph::EdgeKind::ApiCall))
        .unwrap();
    assert!(
        api_edges.iter().any(|e| {
            e.source_id.contains("uiTriggers.js") && e.target_id.contains("CustomerService.asmx")
        }),
        "Expected AJAX edge to ASMX service from external JS"
    );
    assert!(
        api_edges
            .iter()
            .any(|e| e.source_id.contains("uiTriggers.js")
                && e.target_id.contains("/api/customer/search")),
        "Expected AJAX edge to API endpoint from external JS"
    );

    // 3) PageMethods trigger should resolve to method and continue into DAL/SQL.
    let method_trace = engram
        .trace_ui_event(Parameters(engram_server::TraceUiEventRequest {
            project_id: project_id.clone(),
            page_path: "Search.aspx".to_string(),
            control_id: None,
            handler_fqn: Some("LegacyApp.FrontendBridgePage.GetCustomer".to_string()),
            max_hops: 6,
            max_paths: 1,
        }))
        .await
        .unwrap();
    let method_text = &method_trace.content[0].as_text().unwrap().text;
    assert!(
        method_text.contains("LegacyApp.FrontendBridgePage.GetCustomer"),
        "Should find PageMethods target handler"
    );
    assert!(
        method_text.contains("LegacyApp.CustomerDal.LookupCustomer"),
        "Should reach DAL from PageMethods handler"
    );
    assert!(
        method_text.contains("sql:inline:"),
        "Should reach SQL from PageMethods handler"
    );
}

#[tokio::test]
async fn test_fixture_dotnet_webforms_cs_extractor_enrichment() {
    let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("tests")
        .join("fixtures")
        .join("dotnet_webforms_cs_extractor");

    let tmp_root = tempfile::tempdir().unwrap();
    let data_dir = tmp_root.path().join("engram_data");

    let cfg = Config {
        allowed_roots: vec![fixture_dir.clone()],
        data_dir,
        max_project_files: Some(200),
        max_project_bytes: Some(10 * 1024 * 1024),
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());

    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: fixture_dir.to_string_lossy().to_string(),
            project_name: "WebFormsCsExtractorFixture".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsCs,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();

    let projects = state.registry.list_projects().unwrap();
    let project_id = &projects[0].project_id;

    state.graph.resolve_symbol_edges(project_id).unwrap();

    let ctor_nodes = state
        .graph
        .query_nodes(project_id, Some("constructor"), Some("OrdersPage"), None, 5)
        .unwrap();
    assert!(!ctor_nodes.is_empty(), "should extract constructor symbol");

    let local_fn_nodes = state
        .graph
        .query_nodes(
            project_id,
            Some("local_function"),
            Some("LocalAudit"),
            None,
            5,
        )
        .unwrap();
    assert!(
        !local_fn_nodes.is_empty(),
        "should extract local function symbol"
    );

    let wiring_edges = state.graph.list_edges(project_id, None).unwrap();
    assert!(
        wiring_edges
            .iter()
            .any(|e| e.target_id.contains("btnSave_Click") || e.target_id.contains("Page_Load")),
        "should extract += event wiring edges"
    );

    let sql_edge_to_proc = state
        .graph
        .find_incoming_edges(
            project_id,
            Some(engram_graph::EdgeKind::SqlCalls),
            "sql:stored_proc:proc_SaveOrder",
            10,
        )
        .unwrap();
    assert!(
        !sql_edge_to_proc.is_empty(),
        "should extract EXEC stored proc sql edge"
    );
}
