use engram_core::{Config, ProjectRecord};
use engram_server::{AppState, Engram};
use serde_json::{Value, json};

#[path = "literal_settings_workflow_tests.rs"]
mod indexed_workflows;

const PID: &str = "current-settings-without-index";
const SOURCE: &str = "Public Class ReportReader\n Public Sub New()\n  Dim locale = SystemSettings.GetText(\"report_locale\")\n  Dim count = SystemSettings.GetInt(\"report_count\")\n  Dim derived = SystemSettings.GetText(\"computed_\" & tenant)\n  ' SystemSettings.GetText(\"comment_only\")\n End Sub\nEnd Class\n";
fn fixture() -> (tempfile::TempDir, AppState, Engram) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("Reader.vb"), SOURCE).unwrap();
    let (state, _) = AppState::new(Config {
        data_dir: temp.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })
    .unwrap();
    state
        .registry
        .put_project(&ProjectRecord {
            project_id: PID.into(),
            project_name: PID.into(),
            project_type: "general".into(),
            directory: root.to_string_lossy().into_owned(),
            created_at_ms: 0,
            updated_at_ms: 0,
            reindex_required_since_ms: None,
        })
        .unwrap();
    state
        .registry
        .set_meta(PID, "active_generation", "1")
        .unwrap();
    let e = Engram::new(state.clone());
    (temp, state, e)
}
fn text(value: rmcp::model::CallToolResult) -> String {
    value.content[0].as_text().unwrap().text.clone()
}

#[tokio::test]
async fn current_files_supply_all_three_workflows_without_index_publication() {
    let (temp, state, e) = fixture();
    let before = state.registry.get_meta(PID, "active_generation").unwrap();
    let catalog = text(
        e.handle_list_settings(
            serde_json::from_value(json!({"project_id":PID,"source_files":["Reader.vb"]})).unwrap(),
        )
        .await
        .unwrap(),
    );
    let catalog: Value = serde_json::from_str(&catalog).unwrap();
    assert_eq!(catalog["observations"].as_array().unwrap().len(), 2);
    assert_eq!(
        catalog["source_files"][0]["raw_blake3"],
        blake3::hash(SOURCE.as_bytes()).to_hex().to_string()
    );
    assert!(
        !catalog.to_string().contains("computed_") && !catalog.to_string().contains("comment_only")
    );
    let lookup = text(
        e.handle_get_setting(
            serde_json::from_value(
                json!({"project_id":PID,"name":"report_locale","source_files":["Reader.vb"]}),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    let lookup: Value = serde_json::from_str(&lookup).unwrap();
    assert_eq!(lookup["observations"][0]["getter"], "GetText");
    assert_eq!(lookup["observations"][0]["line"], 3);
    let matrix = text(
        e.handle_derive_test_matrix(
            serde_json::from_value(json!({"project_id":PID,"files":["Reader.vb"]})).unwrap(),
        )
        .await
        .unwrap(),
    );
    assert!(
        matrix.contains("report_locale")
            && matrix.contains("report_count")
            && matrix.contains("GetInt"),
        "{matrix}"
    );
    assert!(
        matrix.contains("current code observation only")
            && matrix.contains("indexed role/state/business axes remain incomplete"),
        "{matrix}"
    );
    assert!(
        matrix.contains(&blake3::hash(SOURCE.as_bytes()).to_hex().to_string()),
        "{matrix}"
    );
    assert_eq!(
        before,
        state.registry.get_meta(PID, "active_generation").unwrap()
    );
    assert!(
        state
            .graph
            .query_nodes(PID, None, None, None, 10)
            .unwrap()
            .is_empty()
    );
    let changed = SOURCE.replace("report_locale", "current_locale");
    std::fs::write(temp.path().join("project/Reader.vb"), &changed).unwrap();
    let fresh = text(
        e.handle_list_settings(
            serde_json::from_value(json!({"project_id":PID,"source_files":["Reader.vb"]})).unwrap(),
        )
        .await
        .unwrap(),
    );
    assert!(fresh.contains("current_locale") && !fresh.contains("report_locale"));
    assert!(fresh.contains(&blake3::hash(changed.as_bytes()).to_hex().to_string()));
}

#[tokio::test]
async fn indexed_catalog_continuation_reaches_each_scoped_row_once() {
    let (_temp, state, e) = fixture();
    let make = |kind: &str, path: &str, name: String| engram_graph::Node {
        node_id: format!("{kind}:{path}:{name}"),
        node_type: kind.into(),
        name,
        namespace: "memory".into(),
        file_path: engram_core::RelPath::new(path),
        start_line: 1,
        end_line: 1,
        language: "vbnet".into(),
        generation: 1,
        metadata: None,
    };
    let expected = [
        ("app_setting", "Site/Reports/web.config", "setting", 13),
        ("global_state", "Site/Reports/Reader.vb", "state", 6),
    ];
    let mut rows = Vec::new();
    let mut names = std::collections::BTreeSet::new();
    for (kind, path, prefix, count) in expected {
        for index in 0..count {
            let name = format!("{prefix}_{index:02}");
            names.insert(name.clone());
            rows.push(make(kind, path, name));
        }
    }
    rows.push(make("app_setting", "Site/Admin/web.config", "other_scope".into()));
    let reader = make("function", "Site/Admin/Reader.vb", "ReadReportFlag".into());
    let reader_id = reader.node_id.clone();
    rows.push(reader);
    state.graph.upsert_nodes(PID, &rows).unwrap();
    state.graph.upsert_edges(PID, &[engram_graph::Edge {
        source_id: reader_id,
        target_id: "app_setting:Site/Reports/web.config:setting_00".into(),
        namespace: "memory".into(),
        language: "vbnet".into(),
        edge_kind: engram_graph::EdgeKind::ReadsSetting,
        weight: 1,
        generation: 1,
        metadata: None,
        updated_at_ms: 0,
    }]).unwrap();

    let prefix = "Next catalog page: list_settings(";
    let mut args = json!({"project_id":PID,"scope":"Site/Reports","max_per_category":5});
    let mut seen = std::collections::BTreeSet::new();
    for (page, expected_rows) in [10, 6, 3].into_iter().enumerate() {
        let report = text(
            e.handle_list_settings(serde_json::from_value(args.clone()).unwrap())
                .await
                .unwrap(),
        );
        assert!(report.starts_with("# Settings catalog — 19 setting(s) (scope: Site/Reports)"));
        assert!(!report.contains("other_scope"));
        if page == 0 {
            assert!(report.contains("**setting_00** — 1 reader(s): ReadReportFlag:1"));
        }
        let entries: Vec<_> = report
            .lines()
            .filter_map(|line| line.strip_prefix("- **").and_then(|row| row.split_once("**")))
            .collect();
        assert_eq!(entries.len(), expected_rows);
        for (name, _) in entries {
            assert!(seen.insert(name.to_owned()), "{name}");
        }
        let next = report.lines().find_map(|line| line.strip_prefix(prefix));
        if page == 2 {
            assert!(next.is_none());
        } else {
            args = serde_json::from_str(next.unwrap().strip_suffix(')').unwrap()).unwrap();
            assert_eq!(args["project_id"], PID);
            assert_eq!(args["scope"], "Site/Reports");
            assert_eq!(args["max_per_category"], 5);
            assert_eq!(args["offset_per_category"], (page + 1) * 5);
        }
    }
    assert_eq!(seen, names);
}

#[tokio::test]
async fn invalid_current_scopes_refuse_before_project_or_source_access() {
    let (_temp, _state, e) = fixture();
    for files in [
        json!([]),
        json!(["../Reader.vb"]),
        json!(["C:/Reader.vb"]),
        json!(["Reader.js"]),
        json!(["./Reader.vb"]),
        json!(vec!["Reader.vb"; 17]),
    ] {
        let result = e
            .handle_list_settings(
                serde_json::from_value(
                    json!({"project_id":"missing-project","source_files":files}),
                )
                .unwrap(),
            )
            .await
            .unwrap_err();
        assert!(
            result.message.contains("source_files"),
            "{}",
            result.message
        );
    }
    assert!(
        e.handle_list_settings(
            serde_json::from_value(
                json!({"project_id":PID,"scope":"Reader","source_files":["Reader.vb"]})
            )
            .unwrap()
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn historical_cutoff_does_not_borrow_live_literal_settings() {
    let (_temp, _state, e) = fixture();
    let matrix = text(
        e.handle_derive_test_matrix(
            serde_json::from_value(
                json!({"project_id":PID,"files":["Reader.vb"],"knowledge_before":"2026-01-01"}),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    assert!(
        !matrix.contains("report_locale") && !matrix.contains("report_count"),
        "{matrix}"
    );
    assert!(
        matrix.contains("current literal-setting fallback withheld under knowledge_before"),
        "{matrix}"
    );
}
