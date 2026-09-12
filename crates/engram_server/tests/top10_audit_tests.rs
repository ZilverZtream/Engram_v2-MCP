#![allow(clippy::unwrap_used)]
use engram_core::RelPath;
use engram_core::config::Config;
use engram_graph::{Edge, EdgeKind, Node};
use engram_server::state::AppState;
use engram_server::tools::Engram;
use serde_json::json;

const PID: &str = "symref-caps-test";

fn build_state() -> (tempfile::TempDir, AppState) {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();
    let cfg = Config {
        data_dir,
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 1,
        ..Default::default()
    };
    let (state, _rx) = AppState::new(cfg).unwrap();
    state
        .registry
        .put_project(&engram_core::ProjectRecord {
            project_id: PID.into(),
            project_name: PID.into(),
            directory: project_dir.to_string_lossy().into_owned(),
            project_type: "dotnet_webforms_vb".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            reindex_required_since_ms: None,
        })
        .unwrap();
    state
        .registry
        .set_meta(PID, "active_generation", "1")
        .unwrap();
    (tmp, state)
}

fn func(path: &str, class: &str, name: &str) -> Node {
    Node {
        node_id: format!("sym:function:{path}:{class}.{name}:1"),
        node_type: "function".into(),
        name: name.into(),
        namespace: class.into(),
        language: "vbnet".into(),
        file_path: RelPath::new(path),
        start_line: 1,
        end_line: 3,
        generation: 1,
        metadata: None,
    }
}

fn calls(src: &str, tgt: &str) -> Edge {
    Edge {
        source_id: src.into(),
        target_id: tgt.into(),
        namespace: "test".into(),
        language: "vbnet".into(),
        edge_kind: EdgeKind::Calls,
        weight: 1,
        generation: 1,
        metadata: None,
        updated_at_ms: 1,
    }
}

#[tokio::test]
async fn settings_report_cooccurrence_without_claiming_enforcement_or_boolean_values() {
    let (tmp, state) = build_state();
    let mut setting = func("web.config", "", "StoragePath");
    setting.node_id = "setting:StoragePath".into();
    setting.node_type = "app_setting".into();
    let mut reader = func("Rules.vb", "Rules", "Save");
    reader.metadata = Some(json!({"permission_checks":"CheckOwner"}));
    state
        .graph
        .upsert_nodes(PID, &[setting.clone(), reader.clone()])
        .unwrap();
    let mut edges = Vec::new();
    for i in 0..501 {
        let mut edge = calls(
            if i == 0 { &reader.node_id } else { "unused" },
            &setting.node_id,
        );
        edge.source_id = if i == 0 {
            reader.node_id.clone()
        } else {
            format!("missing:{i:03}")
        };
        edge.edge_kind = EdgeKind::ReadsSetting;
        edge.weight = if i == 0 { 100 } else { 1 };
        edges.push(edge);
    }
    state.graph.upsert_edges(PID, &edges).unwrap();
    let engram = Engram::new(state.clone());
    let result = engram
        .handle_get_setting(
            serde_json::from_value(json!({"project_id":PID,"name":"StoragePath"})).unwrap(),
        )
        .await
        .unwrap();
    let text = &result.content[0].as_text().unwrap().text;
    assert!(text.contains("truncated at 500"), "{text}");
    assert!(text.contains("do not assume a boolean"), "{text}");
    assert!(text.contains("This does not prove"), "{text}");
    assert!(!text.contains("ALSO gated"), "{text}");
    let mut second = setting;
    second.node_id = "setting:OtherStoragePath".into();
    second.name = "OtherStoragePath".into();
    state.graph.upsert_nodes(PID, &[second]).unwrap();
    let result = engram
        .handle_describe_setting(
            serde_json::from_value(json!({"project_id":PID,"name":"Path"})).unwrap(),
        )
        .await;
    assert!(result.unwrap_err().message.contains("AMBIGUOUS"));
    let mut file = func("Rules.vb", "", "Rules.vb");
    file.node_id = "file:Rules.vb".into();
    file.node_type = "file".into();
    file.metadata = Some(json!({"file_hash":engram_core::ContentHash::compute(b"old source").0}));
    state.graph.upsert_nodes(PID, &[file]).unwrap();
    std::fs::write(tmp.path().join("project/Rules.vb"), "changed source").unwrap();
    let result = engram
        .handle_describe_setting(
            serde_json::from_value(json!({"project_id":PID,"name":"StoragePath"})).unwrap(),
        )
        .await;
    assert!(result.unwrap_err().message.contains("Stale"));
}

#[tokio::test]
async fn state_trace_reports_independent_caps_and_indexed_only_positions() {
    let (_tmp, state) = build_state();
    let mut state_node = func("State.vb", "", "Token");
    state_node.node_id = engram_core::NodeId::state("Session", "Token").0;
    state_node.node_type = "global_state".into();
    state
        .graph
        .upsert_nodes(PID, &[state_node.clone()])
        .unwrap();
    let mut edges = Vec::new();
    for i in 0..3 {
        for kind in [EdgeKind::ReadsState, EdgeKind::WritesState] {
            let mut edge = calls(&format!("missing:{i}"), &state_node.node_id);
            edge.edge_kind = kind;
            edges.push(edge);
        }
    }
    state.graph.upsert_edges(PID, &edges).unwrap();
    let result = Engram::new(state)
        .handle_trace_state_usage(
            serde_json::from_value(
                json!({"project_id":PID,"state_type":"Session","state_key":"Token","limit":1}),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let text = &result.content[0].as_text().unwrap().text;
    assert!(text.contains("writers truncated at 1"), "{text}");
    assert!(text.contains("readers truncated at 1"), "{text}");
    assert!(
        text.contains("not verified access-site positions"),
        "{text}"
    );
    assert!(text.contains("indexed node unavailable"), "{text}");
}

#[tokio::test]
async fn setting_accessors_keep_their_qualified_identity() {
    let (_tmp, state) = build_state();
    let nodes: Vec<_> = ["ConfigSettings.A", "ConfigSettings.B"]
        .iter()
        .map(|owner| {
            let mut n = func("Config.vb", owner, "Enabled");
            n.node_type = "property".into();
            n
        })
        .collect();
    state.graph.upsert_nodes(PID, &nodes).unwrap();
    let engram = Engram::new(state);
    for owner in ["ConfigSettings.A", "ConfigSettings.B"] {
        let name = format!("{owner}.Enabled");
        let result = engram
            .handle_describe_setting(
                serde_json::from_value(json!({"project_id":PID,"name":name})).unwrap(),
            )
            .await
            .unwrap();
        assert!(result.content[0].as_text().unwrap().text.contains(&name));
    }
    assert!(
        engram
            .handle_describe_setting(
                serde_json::from_value(json!({"project_id":PID,"name":"Enabled"})).unwrap()
            )
            .await
            .unwrap_err()
            .message
            .contains("AMBIGUOUS")
    );
}

#[tokio::test]
async fn validator_mapping_handles_quotes_comments_and_missing_declared_codebehind() {
    let (tmp, state) = build_state();
    let markup = r#"<%@ Page CodeFile='Custom.vb' %>
<%-- <asp:RequiredFieldValidator ID='ghost' ControlToValidate='ghost' /> --%>
<asp:RequiredFieldValidator ClientID='wrong' ID='required' ControlToValidate='quantity' ErrorMessage='must be > 0' />
<asp:Button ID='save' ValidationGroup='Main' CausesValidation='false' />"#;
    std::fs::write(tmp.path().join("project/Form.aspx"), markup).unwrap();
    let engram = Engram::new(state);
    let request = || {
        serde_json::from_value(json!({"project_id":PID,"file_path":"Form.aspx","output_json":true}))
            .unwrap()
    };
    let result = engram
        .handle_map_validation_controls(request())
        .await
        .unwrap();
    let value: serde_json::Value =
        serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
    assert_eq!(value["total_validators"], 1, "{value}");
    assert_eq!(value["validators"][0]["validator_id"], "required");
    assert_eq!(value["validators"][0]["error_message"], "must be > 0");
    assert_eq!(
        value["causes_validation_buttons"][0]["causes_validation"],
        false
    );
    assert!(
        value["coverage"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str().unwrap().contains("Code-behind unavailable")),
        "{value}"
    );
    std::fs::write(
        tmp.path().join("project/Custom.vb"),
        "Class Form\nEnd Class",
    )
    .unwrap();
    let result = engram
        .handle_map_validation_controls(request())
        .await
        .unwrap();
    assert!(
        result.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("Code-behind source hash")
    );
    std::fs::create_dir_all(tmp.path().join("project/pages")).unwrap();
    std::fs::write(
        tmp.path().join("project/pages/Form.aspx"),
        markup.replace("Custom.vb", "../Custom.vb"),
    )
    .unwrap();
    let result = engram
        .handle_map_validation_controls(
            serde_json::from_value(
                json!({"project_id":PID,"file_path":"pages/Form.aspx","output_json":true}),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        result.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("Code-behind source hash")
    );
}

#[tokio::test]
async fn test_discovery_requires_exact_target_and_separates_graph_evidence_from_names() {
    let (_tmp, state) = build_state();
    let target = func("src/Rules.vb", "Rules", "Save");
    let direct = func("tests/RulesTests.vb", "RulesTests", "SaveRejectsNegative");
    let named = func("tests/RulesTests.vb", "RulesTests", "SaveHappyPath");
    let unrelated = func("src/contest.vb", "Contest", "SaveCandidate");
    let sibling = func("src/Rules.vb.old", "Rules", "Save");
    state
        .graph
        .upsert_nodes(
            PID,
            &[
                target.clone(),
                direct.clone(),
                named,
                unrelated.clone(),
                sibling,
            ],
        )
        .unwrap();
    state
        .graph
        .upsert_edges(
            PID,
            &[
                calls(&direct.node_id, &target.node_id),
                calls(&unrelated.node_id, &target.node_id),
            ],
        )
        .unwrap();
    let engram = Engram::new(state.clone());
    let request = |name: &str, path: Option<&str>| {
        serde_json::from_value(
            json!({"project_id": PID, "method_name": name, "file_path": path, "output_json": true}),
        )
        .unwrap()
    };
    let result = engram
        .handle_find_tests_for_method(request("Save", Some("src/Rules.vb")))
        .await
        .unwrap();
    let value: serde_json::Value =
        serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
    let hits = value["test_hits"].as_array().unwrap();
    assert_eq!(hits.len(), 2, "{value}");
    assert_eq!(hits[0]["match_type"], "dependency_edge");
    assert_eq!(hits[1]["match_type"], "name_match_heuristic");
    assert_eq!(value["test_files_searched"], 1);
    assert!(
        value["warnings"][0]
            .as_str()
            .unwrap()
            .contains("execution are unverified")
    );
    assert!(
        engram
            .handle_find_tests_for_method(request("Sav", Some("src/Rules.vb")))
            .await
            .is_err()
    );
    assert!(
        engram
            .handle_find_tests_for_method(request("Save", None))
            .await
            .is_err()
    );
    assert!(
        engram
            .handle_find_tests_for_method(request(" ", None))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn test_discovery_selects_overloads_without_promoting_sibling_callers() {
    for extension in ["vb", "cs", "rs"] {
        let (_tmp, state) = build_state();
        let path = format!("src/Rules.{extension}");
        let first = func(&path, "Rules", "Save");
        let mut second = first.clone();
        second.start_line = 10;
        second.end_line = 15;
        second.node_id.push_str(":overload");
        let a = func(
            &format!("tests/RulesTests.{extension}"),
            "Tests",
            "SaveInteger",
        );
        let b = func(
            &format!("tests/RulesTests.{extension}"),
            "Tests",
            "SaveString",
        );
        state
            .graph
            .upsert_nodes(PID, &[first.clone(), second.clone(), a.clone(), b.clone()])
            .unwrap();
        state
            .graph
            .upsert_edges(
                PID,
                &[
                    calls(&a.node_id, &first.node_id),
                    calls(&b.node_id, &second.node_id),
                ],
            )
            .unwrap();
        let engram = Engram::new(state);
        for line in [None, Some(0), Some(9)] {
            let result = engram.handle_find_tests_for_method(serde_json::from_value(json!({"project_id":PID,"method_name":"Save","file_path":path,"start_line":line,"output_json":true})).unwrap()).await;
            assert!(
                result.is_err(),
                "ambiguous/invalid selector accepted: {line:?}"
            );
        }
        let result = engram.handle_find_tests_for_method(serde_json::from_value(json!({"project_id":PID,"method_name":"Save","file_path":path,"start_line":10,"output_json":true})).unwrap()).await.unwrap();
        let body: serde_json::Value =
            serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
        assert_eq!(body["target_node_id"], second.node_id);
        let direct: Vec<_> = body["test_hits"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["match_type"] == "dependency_edge")
            .collect();
        assert_eq!(direct.len(), 1);
        assert_eq!(direct[0]["test_name"], "SaveString");
        assert!(
            engram
                .handle_find_tests_for_method(
                    serde_json::from_value(
                        json!({"project_id":PID,"method_name":"Save","start_line":10})
                    )
                    .unwrap()
                )
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn review_decisions_preserve_attestations_and_invalidate_changed_heads() {
    let (tmp, state) = build_state();
    let root = tmp.path().join("project");
    let repo = git2::Repository::init(&root).unwrap();
    std::fs::write(root.join("rules.txt"), "first\n").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(std::path::Path::new("rules.txt")).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let sig = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
    let commit = repo
        .commit(Some("HEAD"), &sig, &sig, "fixture", &tree, &[])
        .unwrap()
        .to_string();
    let engram = Engram::new(state.clone());
    let mut event = json!({"event_id":"e1","finding_id":"f1","supersedes":null,"kind":"claimed_fix","source_url":"https://example.invalid/review/1","author":"reviewer","recorded_at":"2026-09-09T12:00:00Z","rationale":"Thread resolved by comment","verification":null});
    let record = |event: serde_json::Value| {
        serde_json::from_value(json!({"project_id":PID,"review_id":"PR-7","decisions":[event]}))
            .unwrap()
    };
    engram
        .handle_record_review_decisions(record(event.clone()))
        .await
        .unwrap();
    engram
        .handle_record_review_decisions(record(event.clone()))
        .await
        .unwrap(); // idempotent replay
    event["kind"] = "accepted_exception".into();
    assert!(
        engram
            .handle_record_review_decisions(record(event.clone()))
            .await
            .is_err()
    );
    event["event_id"] = "e2".into();
    event["supersedes"] = "e1".into();
    event["kind"] = "verified_fix".into();
    assert!(
        engram
            .handle_record_review_decisions(record(event.clone()))
            .await
            .is_err()
    );
    event["verification"] = json!({"commit":commit,"check":"fixture regression passed","evidence_url":"https://example.invalid/artifacts/check","artifact_sha256":"a".repeat(64),"verifier":"test runner"});
    engram
        .handle_record_review_decisions(record(event))
        .await
        .unwrap();
    let current = decision_snapshot(&engram).await;
    assert_eq!(
        current["current"][0]["effective_status"],
        "externally_verified_fix_at_current_clean_head"
    );
    std::fs::write(root.join("rules.txt"), "changed\n").unwrap();
    let changed = decision_snapshot(&engram).await;
    assert_eq!(
        changed["current"][0]["effective_status"],
        "verification_not_current"
    );
    assert_eq!(changed["events"].as_array().unwrap().len(), 2);
    assert_eq!(changed["automatic_finding_suppression"], false);
    let mut index = repo.index().unwrap();
    index.add_path(std::path::Path::new("rules.txt")).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let parent = repo.head().unwrap().peel_to_commit().unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "changed head", &tree, &[&parent])
        .unwrap();
    let advanced = decision_snapshot(&engram).await;
    assert_eq!(advanced["working_tree_clean"], true);
    assert_eq!(
        advanced["current"][0]["effective_status"],
        "verification_not_current"
    );
}

async fn decision_snapshot(engram: &Engram) -> serde_json::Value {
    let result = engram
        .handle_get_review_decisions(
            serde_json::from_value(json!({"project_id":PID,"review_id":"PR-7"})).unwrap(),
        )
        .await
        .unwrap();
    serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap()
}

#[tokio::test]
async fn review_coverage_names_binary_and_text_scope_and_never_claims_execution() {
    let (tmp, state) = build_state();
    std::fs::write(tmp.path().join("project/Rules.cs"), "class Rules {}\n").unwrap();
    std::fs::write(tmp.path().join("project/image.bin"), [0, 1, 2]).unwrap();
    let engram = Engram::new(state);
    let diff = "diff --git a/Rules.cs b/Rules.cs\n--- a/Rules.cs\n+++ b/Rules.cs\n@@ -1 +1 @@\n-class Rules { }\n+class Rules {}\ndiff --git a/image.bin b/image.bin\nindex 1111111..2222222 100644\nBinary files a/image.bin and b/image.bin differ\n";
    let result = engram
        .handle_pre_commit_review(
            serde_json::from_value(json!({"project_id":PID,"diff":diff,"output_json":true}))
                .unwrap(),
        )
        .await
        .unwrap();
    let body: serde_json::Value =
        serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
    assert_eq!(
        body["coverage"]["submitted_files"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(body["coverage"]["textual_diff_files"], json!(["Rules.cs"]));
    assert_eq!(body["coverage"]["unexamined_files"][0]["path"], "image.bin");
    assert_eq!(body["coverage"]["compilation"], "not_run");
    assert_eq!(body["coverage"]["test_execution"], "not_run");
    assert_eq!(body["coverage"]["source_snapshot_complete"], true);
    assert_eq!(body["coverage"]["changed_during_review"], false);
}

#[tokio::test]
async fn temporal_global_frequency_and_cap_are_enforced_and_history_gaps_are_visible() {
    let (_tmp, state) = build_state();
    let nodes: Vec<_> = ["a", "b", "c"]
        .iter()
        .map(|name| {
            let mut n = func(name, "", name);
            n.node_type = "file".into();
            n.node_id = format!("file:{name}");
            n
        })
        .collect();
    state.graph.upsert_nodes(PID, &nodes).unwrap();
    let mut edges = Vec::new();
    for (target, weight) in [("file:b", 10), ("file:c", 2)] {
        for (src, dst) in [("file:a", target), (target, "file:a")] {
            let mut e = calls(src, dst);
            e.edge_kind = EdgeKind::TemporalCoupling;
            e.weight = weight;
            edges.push(e);
        }
    }
    state.graph.upsert_edges(PID, &edges).unwrap();
    let engram = Engram::new(state);
    for (minimum, cap, expected, excluded, truncated) in [
        (5, 10, "weight=10", "weight=2", false),
        (1, 1, "weight=10", "weight=2", true),
        (20, 10, "No temporal neighbors", "weight=10", false),
    ] {
        let result = engram
            .handle_analyze_temporal_couplings(
                serde_json::from_value(
                    json!({"project_id":PID,"min_frequency":minimum,"limit":cap}),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let text = &result.content[0].as_text().unwrap().text;
        assert!(text.contains(expected), "{text}");
        assert!(!text.contains(excluded), "{text}");
        assert_eq!(text.contains("Results truncated"), truncated, "{text}");
        assert!(
            text.contains("watermark: unavailable; backfill complete: false"),
            "{text}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn schema_reports_missing_columns_and_actual_caps() {
    let (_tmp, state) = build_state();
    let mut table = func("schema.sql", "", "Wide");
    table.node_id = "table:wide".into();
    table.node_type = "db_table".into();
    let mut edges = Vec::new();
    for i in 0..201 {
        let mut e = calls(&table.node_id, &format!("column:wide:c{i:03}"));
        e.edge_kind = EdgeKind::HasColumn;
        edges.push(e);
    }
    state.graph.upsert_nodes(PID, &[table]).unwrap();
    state.graph.upsert_edges(PID, &edges).unwrap();
    let result = Engram::new(state)
        .handle_get_table_schema(
            serde_json::from_value(json!({"project_id": PID, "table_name": "WIDE"})).unwrap(),
        )
        .await
        .unwrap();
    let out = &result.content[0].as_text().unwrap().text;
    assert!(out.contains("truncated to the first 200"), "{out}");
    assert!(out.contains("column node unavailable"), "{out}");
    assert!(out.contains("Source: schema.sql"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_matrix_reports_capped_axes_and_excludes_sibling_paths() {
    let (_tmp, state) = build_state();
    let method = func("Site/test.vb", "C", "Target");
    let sibling = func("Site/test.vb.old", "C", "Sibling");
    let mut edges = Vec::new();
    for i in 0..21 {
        let mut e = calls(&method.node_id, &format!("setting:s{i:02}"));
        e.edge_kind = EdgeKind::ReadsSetting;
        edges.push(e);
    }
    let mut e = calls(&sibling.node_id, "setting:SIBLING_ONLY");
    e.edge_kind = EdgeKind::ReadsSetting;
    edges.push(e);
    state.graph.upsert_nodes(PID, &[method, sibling]).unwrap();
    state.graph.upsert_edges(PID, &edges).unwrap();
    let result = Engram::new(state)
        .handle_derive_test_matrix(
            serde_json::from_value(json!({"project_id": PID, "files": ["Site/test.vb"]})).unwrap(),
        )
        .await
        .unwrap();
    let out = &result.content[0].as_text().unwrap().text;
    assert!(out.contains("settings truncated at 20"), "{out}");
    assert!(!out.contains("SIBLING_ONLY"), "{out}");
    assert!(out.contains("Test discovery: not_run"), "{out}");
    assert!(out.contains("Test execution: not_run"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_matrix_withholds_known_stale_axes_and_labels_legacy_evidence() {
    let (tmp, state) = build_state();
    // File fingerprints hash raw bytes, unlike normalized chunk ContentHash.
    // Exercise Windows line endings as well as a real subsequent source edit.
    let source = "Class Rules\r\nSub Save()\r\nCheckOwner()\r\nEnd Sub\r\nEnd Class\r\n";
    std::fs::write(tmp.path().join("project/Rules.vb"), source).unwrap();
    let mut method = func("Rules.vb", "Rules", "Save");
    method.metadata = Some(json!({"permission_checks": "CheckOwner"}));
    let mut file = func("Rules.vb", "", "Rules.vb");
    file.node_id = "file:Rules.vb".into();
    file.node_type = "file".into();
    file.metadata =
        Some(json!({"file_hash": blake3::hash(source.as_bytes()).to_hex().to_string()}));
    state
        .graph
        .upsert_nodes(PID, &[file.clone(), method])
        .unwrap();
    let engram = Engram::new(state.clone());
    let request =
        || serde_json::from_value(json!({"project_id": PID, "files": ["Rules.vb"]})).unwrap();
    let current = engram.handle_derive_test_matrix(request()).await.unwrap();
    assert!(
        current.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("[gate] CheckOwner")
    );
    std::fs::write(
        tmp.path().join("project/Rules.vb"),
        source.replace("CheckOwner()", "Return"),
    )
    .unwrap();
    let stale = engram.handle_derive_test_matrix(request()).await.unwrap();
    let text = &stale.content[0].as_text().unwrap().text;
    assert!(text.contains("STALE graph axes withheld"), "{text}");
    assert!(!text.contains("[gate] CheckOwner"), "{text}");
    // None means preserve existing metadata in GraphStore's upsert contract.
    file.metadata = Some(json!({}));
    state.graph.upsert_nodes(PID, &[file]).unwrap();
    let legacy = engram.handle_derive_test_matrix(request()).await.unwrap();
    let text = &legacy.content[0].as_text().unwrap().text;
    assert!(text.contains("indexed axes are UNVERIFIED"), "{text}");
    assert!(text.contains("[gate] CheckOwner"), "{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_rejects_invalid_controls_and_returns_json_for_empty_patch() {
    let (tmp, state) = build_state();
    std::fs::write(tmp.path().join("project/empty.patch"), "").unwrap();
    std::fs::write(
        tmp.path().join("project/invalid.patch"),
        "This is not a unified diff",
    )
    .unwrap();
    let engram = Engram::new(state);
    for extra in [
        json!({"min_severity": "critcial"}),
        json!({"skip_gates": ["imune"]}),
        json!({"diff": "invalid.patch"}),
    ] {
        let mut req = json!({"project_id": PID, "diff": "empty.patch", "output_json": true});
        req.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(
            engram
                .handle_pre_commit_review(serde_json::from_value(req).unwrap())
                .await
                .is_err()
        );
    }
    let result = engram
        .handle_pre_commit_review(
            serde_json::from_value(
                json!({"project_id": PID, "diff": "empty.patch", "output_json": true}),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let value: serde_json::Value =
        serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
    assert_eq!(value["verdict"], "NO_CHANGES");
    assert_eq!(value["summary"]["gates_run"], 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_rejects_impossible_dates_and_rules_reject_blank_queries() {
    let (_tmp, state) = build_state();
    let engram = Engram::new(state);
    for date in ["2026-02-30", "2026-13-01", "2025-02-29"] {
        let result = engram.handle_find_merged_work(serde_json::from_value(json!({"project_id": PID, "story": "create change request", "merged_before": date})).unwrap()).await;
        assert!(result.is_err(), "invalid date accepted: {date}");
    }
    assert!(
        engram
            .handle_query_business_logic(
                serde_json::from_value(json!({"project_id": PID, "query": "  "})).unwrap()
            )
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_scope_obeys_file_and_directory_boundaries() {
    let (_tmp, state) = build_state();
    let wanted = func("Site/ata/a.vb", "C", "Wanted");
    let sibling = func("Site/ata-old/a.vb", "C", "UnrelatedSibling");
    state.graph.upsert_nodes(PID, &[wanted, sibling]).unwrap();
    let result = Engram::new(state)
        .handle_map_guards_and_settings(
            serde_json::from_value(
                json!({"project_id": PID, "scope": "Site/ata", "output_json": true}),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let out = &result.content[0].as_text().unwrap().text;
    let value: serde_json::Value = serde_json::from_str(out).unwrap();
    assert_eq!(value["functions"].as_array().unwrap().len(), 1, "{out}");
    assert!(out.contains("Wanted"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ddl_ingestion_and_existing_index_repair_link_real_table_and_column_nodes() {
    let (_tmp, state) = build_state();
    let path = std::sync::Arc::new(RelPath::new("schema.sql"));
    let ddl = "CREATE TABLE [dbo].[Parent] (\n [id] INT NOT NULL\n);\nCREATE TABLE [dbo].[Child] (\n [id] INT NOT NULL,\n [parent_id] INT NULL,\n CONSTRAINT [FK_Child_Parent] FOREIGN KEY ([parent_id]) REFERENCES [dbo].[Parent] ([id])\n);";
    let (symbols, edges) = engram_index::ddl_extractor::extract_ddl(&path, ddl);
    let mut stats = engram_index::IngestStats::default();
    stats.symbols = symbols.into_iter().map(|s| (path.clone(), s)).collect();
    stats.edges = edges.into_iter().map(|e| (path.clone(), e)).collect();
    let declared_edges = std::mem::take(&mut stats.edges);
    engram_server::services::ingest_service::process_ingest_stats(&state, PID, 1, &stats)
        .await
        .unwrap();
    assert!(
        state
            .graph
            .neighbors(PID, EdgeKind::HasColumn, "table:child", 10)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        engram_server::services::ingest_service::repair_schema_links(&state.graph, PID).unwrap(),
        4
    );
    assert_eq!(
        state
            .graph
            .neighbors(PID, EdgeKind::HasColumn, "table:child", 10)
            .unwrap()
            .len(),
        2
    );
    stats.edges = declared_edges;
    engram_server::services::ingest_service::process_ingest_stats(&state, PID, 1, &stats)
        .await
        .unwrap();
    let columns = state
        .graph
        .neighbors(PID, EdgeKind::HasColumn, "table:child", 10)
        .unwrap();
    assert_eq!(columns.len(), 2, "{columns:?}");
    for (id, _) in &columns {
        assert!(state.graph.get_node(PID, id).unwrap().is_some(), "{id}");
    }
    let fk = state
        .graph
        .neighbors(PID, EdgeKind::ForeignKey, "column:child:parent_id", 10)
        .unwrap();
    assert_eq!(fk[0].0, "column:parent:id");
    let old_freshness = state
        .registry
        .get_meta(PID, "last_index_completed_ms")
        .unwrap();
    // Re-running the repair is idempotent and does not mark source reindexed.
    for _ in 0..2 {
        assert_eq!(
            engram_server::services::ingest_service::repair_schema_links(&state.graph, PID)
                .unwrap(),
            4
        );
    }
    assert_eq!(
        state
            .graph
            .neighbors(PID, EdgeKind::HasColumn, "table:child", 10)
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        state
            .registry
            .get_meta(PID, "last_index_completed_ms")
            .unwrap(),
        old_freshness
    );
    let out = Engram::new(state)
        .handle_get_table_schema(
            serde_json::from_value(json!({"project_id": PID, "table_name": "Child"})).unwrap(),
        )
        .await
        .unwrap();
    let out = &out.content[0].as_text().unwrap().text;
    assert!(out.contains("### Columns"), "{out}");
    assert!(out.contains("### Foreign Keys"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sql_validator_checks_exact_bare_and_aliased_columns_without_certifying_unsupported_sql() {
    let (_tmp, state) = build_state();
    let mut table = func("schema.sql", "", "Orders");
    table.node_id = "table:orders".into();
    table.node_type = "db_table".into();
    let mut col = func("schema.sql", "", "id");
    col.node_id = "column:orders:id".into();
    col.node_type = "db_column".into();
    state.graph.upsert_nodes(PID, &[table, col]).unwrap();
    let engram = Engram::new(state);
    for (sql, verdict) in [
        ("SELECT id FROM Orders", "PASS"),
        ("SELECT o.id FROM Orders AS o", "PASS"),
        (
            "SELECT [o].[id] AS [key] FROM [dbo].[Orders] AS [o]",
            "INSUFFICIENT",
        ),
        ("SELECT missing FROM Orders", "WARN"),
        ("SELECT o.missing FROM Orders o", "WARN"),
        ("SELECT wrong.id FROM Orders o", "FAIL"),
        ("SELECT id FROM Orders WHERE missing = 1", "WARN"),
        ("SELECT COUNT(id) FROM Orders", "PASS"),
        (
            "SELECT 'prefix' + CAST(id AS varchar(20)) FROM Orders",
            "PASS",
        ),
        (
            "SELECT id FROM Orders -- SELECT *; NOLOCK; String.Format()",
            "PASS",
        ),
        ("WITH x AS (SELECT id FROM Orders) SELECT id FROM x", "PASS"),
        (
            "WITH x AS (SELECT id FROM Orders) SELECT missing FROM x",
            "WARN",
        ),
        ("INSERT INTO Orders (id) VALUES (1)", "PASS"),
        ("INSERT INTO Orders (id) VALUES (1, 2)", "FAIL"),
        ("UPDATE Orders SET id = 1 WHERE missing = 1", "WARN"),
        ("SELECT [id FROM Orders", "FAIL"),
        ("this is not SQL", "FAIL"),
    ] {
        let result = engram
            .handle_validate_sql_fragment(
                serde_json::from_value(json!({"project_id": PID, "sql": sql, "output_json": true}))
                    .unwrap(),
            )
            .await
            .unwrap();
        let out: serde_json::Value =
            serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
        assert_eq!(out["verdict"], verdict, "{sql}: {out}");
        assert!(out.get("coverage").is_some());
    }
}
