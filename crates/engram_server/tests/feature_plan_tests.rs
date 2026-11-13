use engram_server::{
    models::{OneShotFeaturePlanRequest, PlanDetailLevel},
    services::feature_plan::*,
};
use serde_json::{Value, json};

#[test]
fn feature_plan_public_handler_reports_disabled_provider_without_generating_a_plan() {
    // Database owners and runtime are dropped before the owned directory,
    // including assertion unwinding.
    let temp = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let directory = temp.path().join("project");
        std::fs::create_dir_all(&directory).unwrap();
        let (state, _) = engram_server::AppState::new(engram_core::Config {
            data_dir: temp.path().join("data"),
            allowed_roots: vec![directory.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            ..Default::default()
        })
        .unwrap();
        state
            .registry
            .put_project(&engram_core::ProjectRecord {
                project_id: "feature-plan".into(),
                project_name: "Feature fixture".into(),
                directory: directory.to_string_lossy().into(),
                project_type: "dotnet_webforms_vb".into(),
                created_at_ms: 1,
                updated_at_ms: 1,
                reindex_required_since_ms: None,
            })
            .unwrap();
        let server = engram_server::Engram::new(state);
        assert!(
            server
                .tool_router
                .list_all()
                .iter()
                .any(|t| t.name == "one_shot_feature_plan")
        );
        let result = server
            .handle_one_shot_feature_plan(OneShotFeaturePlanRequest {
                project_id: "feature-plan".into(),
                feature_description: "Minimum photos".into(),
                model_after: None,
                area_hint: None,
                target_stack: "webforms_vbnet".into(),
                detail_level: PlanDetailLevel::ReadyToCode,
            })
            .await
            .unwrap();
        let value: Value =
            serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
        assert_eq!(value["status"], "INCOMPLETE");
        assert_eq!(value["reason"], "llm_backend_unavailable");
        assert_eq!(value["provenance"]["provider_calls"], 0);
        assert!(value["plan"].is_null());
        assert!(std::fs::read_dir(directory).unwrap().next().is_none());
    });
}

fn intent() -> FeatureIntent {
    parse_intent(&json!({
        "requirements":[
            {"id":"R1","area":"ui","description":"Admin can set a minimum photo count"},
            {"id":"R2","area":"database","description":"Persist the count"},
            {"id":"R3","area":"auth","description":"Define which administrators can edit the setting"}
        ], "concepts":["photos"], "schema_names":["dbo.PhotoSettings"],
        "unknowns":["Which administrator roles may change this setting?"]
    }).to_string()).unwrap()
}

fn evidence() -> Vec<SourceEvidence> {
    vec![
        SourceEvidence {
            id: "E1".into(),
            file_path: "Site/admin/photos.aspx.vb".into(),
            source_hash: "test-source".into(),
            start_line: 10,
            end_line: 16,
            excerpt: "10: Protected Sub Save_Click()\n11: End Sub".into(),
            methods: vec![MethodTarget {
                method_name: "Save_Click".into(),
                line: 10,
            }],
        },
        SourceEvidence {
            id: "E2".into(),
            file_path: "Database/PhotoSettings.sql".into(),
            source_hash: "test-schema".into(),
            start_line: 1,
            end_line: 2,
            excerpt: "1: CREATE TABLE dbo.PhotoSettings (Id int NOT NULL)".into(),
            methods: vec![],
        },
    ]
}

fn proposal() -> Value {
    json!({
        "feature_name":"Minimum photo count","summary":"Propose a validated and persisted photo threshold",
        "estimated_complexity":"small",
        "schema_changes":[{"file_path":"Database/photo-minimum.sql","description":"Add proposed threshold column",
            "ddl":"ALTER TABLE dbo.PhotoSettings ADD MinimumPhotos int NULL;","evidence_ids":["E2"]}],
        "new_files":[{"file_path":"Database/photo-minimum.sql","description":"Versioned SQL proposal"}],
        "modified_files":[{"file_path":"Site/admin/photos.aspx.vb","description":"Validate and save threshold",
            "methods":[{"method_name":"Save_Click","line":10}],"evidence_ids":["E1"]}],
        "implementation_order":["Add storage column","Validate then persist threshold"],
        "existing_patterns_to_follow":[{"description":"Reuse the existing save event","evidence_ids":["E1"]}],
        "risks":["Authorization policy is unresolved"],"pre_conditions":["Resolve permitted admin roles"],
        "acceptance_criteria":["Reject a negative threshold; a permitted admin can reload a saved value"],
        "requirement_dispositions":[
            {"requirement_id":"R1","disposition":"planned","rationale":"Extend Save_Click"},
            {"requirement_id":"R2","disposition":"planned","rationale":"Add schema proposal and persist"},
            {"requirement_id":"R3","disposition":"unresolved","rationale":"Requires role policy"}
        ],
        "ready_to_code_steps":[
            {"step_number":1,"file_path":"Database/photo-minimum.sql","action":"Propose DDL",
                "description":"Add optional count pending default/backfill decision","requirement_ids":["R2"],
                "code_template":"ALTER TABLE dbo.PhotoSettings ADD MinimumPhotos int NULL;",
                "validation":"Review null/default behavior against existing rows before applying"},
            {"step_number":2,"file_path":"Site/admin/photos.aspx.vb","action":"Modify Save_Click",
                "description":"Parse and validate the threshold before saving","requirement_ids":["R1","R2"],
                "code_template":"If Not Integer.TryParse(rawValue, minimum) OrElse minimum < 0 Then\n Return\nEnd If",
                "validation":"Exercise valid, negative, malformed and unauthorized requests"}
        ]
    })
}

#[test]
fn feature_plan_retains_schema_order_validation_and_unresolved_product_decision() {
    let plan = parse_plan(&proposal().to_string(), &intent(), &evidence()).unwrap();
    assert_eq!(
        plan.schema_changes[0].ddl,
        "ALTER TABLE dbo.PhotoSettings ADD MinimumPhotos int NULL;"
    );
    assert_eq!(
        plan.ready_to_code_steps[1].file_path,
        "Site/admin/photos.aspx.vb"
    );
    assert_eq!(plan.requirement_dispositions[2].disposition, "unresolved");
    assert_eq!(plan.modified_files[0].methods[0].line, 10);
    assert!(
        plan.ready_to_code_steps
            .iter()
            .all(|s| !s.validation.is_empty())
    );
}

#[test]
fn feature_plan_withholds_invented_source_files_spans_and_patterns() {
    for (pointer, invalid) in [
        ("/modified_files/0/evidence_ids/0", json!("E999")),
        ("/modified_files/0/evidence_ids/0", json!("E2")),
        (
            "/modified_files/0/file_path",
            json!("Site/invented.aspx.vb"),
        ),
        ("/modified_files/0/methods/0/line", json!(11)),
        (
            "/modified_files/0/methods/0/method_name",
            json!("NeverIndexed"),
        ),
        (
            "/existing_patterns_to_follow/0/evidence_ids/0",
            json!("E999"),
        ),
    ] {
        let mut value = proposal();
        *value.pointer_mut(pointer).unwrap() = invalid;
        assert!(
            parse_plan(&value.to_string(), &intent(), &evidence()).is_err(),
            "{pointer}"
        );
    }
}

#[test]
fn feature_plan_rejects_dropped_requirements_and_unexecutable_order() {
    for (pointer, invalid) in [
        ("/requirement_dispositions", json!([])),
        ("/ready_to_code_steps/0/step_number", json!(2)),
        ("/ready_to_code_steps/1/file_path", json!("other.vb")),
        ("/ready_to_code_steps/1/validation", json!("")),
        ("/ready_to_code_steps/1/requirement_ids", json!(["unknown"])),
        ("/ready_to_code_steps/1/requirement_ids", json!(["R2"])),
        ("/schema_changes/0/file_path", json!("unplanned.sql")),
        ("/acceptance_criteria", json!([])),
    ] {
        let mut value = proposal();
        *value.pointer_mut(pointer).unwrap() = invalid;
        assert!(
            parse_plan(&value.to_string(), &intent(), &evidence()).is_err(),
            "{pointer}"
        );
    }
}

#[test]
fn feature_plan_rejects_new_file_overwrite_claims_and_path_escapes() {
    for path in [
        "../outside.sql",
        "/absolute.sql",
        "C:/outside.sql",
        "a\\b.sql",
        "a//b.sql",
        "a/./b.sql",
        "Site/admin/photos.aspx.vb",
    ] {
        let mut value = proposal();
        value["new_files"][0]["file_path"] = json!(path);
        assert!(
            parse_plan(&value.to_string(), &intent(), &evidence()).is_err(),
            "{path}"
        );
    }
    let mut value = proposal();
    value["modified_files"][0]["methods"]
        .as_array_mut()
        .unwrap()
        .push(json!({"method_name":"Save_Click","line":10}));
    assert!(parse_plan(&value.to_string(), &intent(), &evidence()).is_err());
}

#[test]
fn feature_plan_detail_modes_preserve_required_work_and_hide_only_templates() {
    let original = parse_plan(&proposal().to_string(), &intent(), &evidence()).unwrap();
    for detail in [
        PlanDetailLevel::Sketch,
        PlanDetailLevel::Detailed,
        PlanDetailLevel::ReadyToCode,
    ] {
        let mut plan = original.clone();
        project_detail(&mut plan, detail);
        assert_eq!(plan.implementation_order, original.implementation_order);
        assert_eq!(plan.acceptance_criteria, original.acceptance_criteria);
        assert_eq!(
            plan.ready_to_code_steps[0].code_template.is_some(),
            detail == PlanDetailLevel::ReadyToCode
        );
    }
}

#[test]
fn feature_plan_rejects_unbounded_or_unrelated_intake() {
    let mut req = OneShotFeaturePlanRequest {
        project_id: "project".into(),
        feature_description: "Minimum photos".into(),
        model_after: None,
        area_hint: None,
        target_stack: "webforms_vbnet".into(),
        detail_level: PlanDetailLevel::Detailed,
    };
    assert!(validate_request(&req).is_ok());
    req.target_stack = "java".into();
    assert!(validate_request(&req).is_err());
    req.target_stack = "webforms_vbnet".into();
    req.feature_description = "x".repeat(16 * 1024 + 1);
    assert!(validate_request(&req).is_err());
    assert!(parse_intent("{\"requirements\":[]}").is_err());
    assert!(parse_plan(&"x".repeat(MAX_MODEL_BYTES + 1), &intent(), &evidence()).is_err());
}
