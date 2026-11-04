//! Ownership review must be read-only until an exact, explicit plan is applied.
use engram_core::{Config, ProjectRecord, Registry, registry::RepoRule};
use engram_server::{AppState, Engram};
use serde_json::{Value, json};

const SOURCE: &str = r#"[{"rule":"Never add ScopeProbe tokens to production code. [check: forbid=ScopeProbe]","path_scope":"Site/**"}]"#;

fn fixture(epoch: bool, manual: bool) -> (tempfile::TempDir, Engram) {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("repo");
    let data = temporary.path().join("data");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(data.join("registry")).unwrap();
    std::fs::write(root.join("policy.json"), SOURCE).unwrap();
    let path = data.join("registry/registry.redb");
    let registry = Registry::open(&path).unwrap();
    let project = ProjectRecord {
        project_id: "adoption".into(),
        project_name: "Adoption behavior".into(),
        project_type: "general".into(),
        directory: root.to_string_lossy().into_owned(),
        created_at_ms: 1,
        updated_at_ms: 1,
        reindex_required_since_ms: None,
    };
    if epoch {
        registry.put_project(&project).unwrap();
        registry.capture_source_head("adoption").unwrap();
        drop(registry);
    } else {
        // A genuinely old stored project has no identity epoch. Preview must not
        // initialize it merely by opening a runtime or requesting a source head.
        drop(registry);
        let database = redb::Database::create(&path).unwrap();
        let tx = database.begin_write().unwrap();
        {
            let projects: redb::TableDefinition<&str, &[u8]> =
                redb::TableDefinition::new("projects");
            tx.open_table(projects)
                .unwrap()
                .insert("adoption", serde_json::to_vec(&project).unwrap().as_slice())
                .unwrap();
        }
        tx.commit().unwrap();
        drop(database);
    }
    let (state, _) = AppState::new(Config {
        data_dir: data,
        allowed_roots: vec![root],
        embedding_backend: "fts_only".into(),
        ..Default::default()
    })
    .unwrap();
    state
        .registry
        .put_repo_rule(
            "adoption",
            &RepoRule {
                rule_id: "old-policy-id".into(),
                file_pattern: "Site/maps/**".into(),
                rule_text: "Original policy\r\nException: read-only probes do not prompt.".into(),
                priority: 19,
                updated_at_ms: 42,
                introduced_at: Some("2025-09-12".into()),
                provenance: if manual { Some("manual".into()) } else { None },
            },
        )
        .unwrap();
    (temporary, Engram::new(state))
}

fn request(engram: &Engram, mode: &str, decision: &str) -> Value {
    let snapshot = engram
        .state
        .registry
        .snapshot_rule_ownership("adoption")
        .unwrap();
    let parsed = engram_index::quality_gates::parse_quality_source_checked(
        SOURCE,
        engram_index::quality_gates::QualitySource::CodingRulesMd,
        "policy.json",
    )
    .unwrap();
    json!({"project_id":"adoption","source_path":"policy.json","source_type":"rules",
        "adoption":{"mode":mode,"expected_source_blake3":blake3::hash(SOURCE.as_bytes()).to_hex().to_string(),
            "bindings":[{"rule_id":"old-policy-id","expected_raw_blake3":blake3::hash(&snapshot.rules()["old-policy-id"]).to_hex().to_string(),
                "source_rule_id":parsed[0].id,"decision":decision}],"decision_ref":"explicit-owner-review:fixture"}})
}

async fn call(engram: &Engram, request: Value) -> Result<Value, rmcp::ErrorData> {
    let result = engram
        .handle_ingest_quality_gates(serde_json::from_value(request).unwrap())
        .await?;
    Ok(serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap())
}

#[tokio::test]
async fn discovery_and_blocked_preview_leave_missing_identity_rows_and_runtime_untouched() {
    let (temporary, engram) = fixture(false, false);
    let before = engram
        .state
        .registry
        .snapshot_rule_ownership("adoption")
        .unwrap();
    let mut req = request(&engram, "preview", "adopt_unverified");
    req["adoption"]["bindings"] = json!([]);
    let discovery = call(&engram, req).await.unwrap();
    assert_eq!(discovery["status"], "selection_required");
    assert_eq!(discovery["source_entries"].as_array().unwrap().len(), 1);
    let blocked = call(&engram, request(&engram, "preview", "adopt_unverified"))
        .await
        .unwrap();
    assert_eq!(blocked["status"], "blocked");
    assert_eq!(blocked["adoption_performed"], false);
    assert!(
        call(&engram, request(&engram, "apply", "adopt_unverified"))
            .await
            .is_err()
    );
    assert_eq!(
        engram
            .state
            .registry
            .snapshot_rule_ownership("adoption")
            .unwrap(),
        before
    );
    assert!(engram.state.projects.is_empty());
    assert!(
        !temporary.path().join("data/projects").exists(),
        "Review must not create native project stores"
    );
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test]
async fn oversized_discovery_response_refuses_without_truncation_or_registry_writes() {
    let (temporary, engram) = fixture(true, false);
    let source = "a".repeat(4 * 1024 * 1024);
    std::fs::write(temporary.path().join("repo/policy.json"), &source).unwrap();
    let before = engram
        .state
        .registry
        .snapshot_rule_ownership("adoption")
        .unwrap();
    let mut req = request(&engram, "preview", "adopt_unverified");
    req["source_type"] = json!("text");
    req["adoption"]["bindings"] = json!([]);
    req["adoption"]["expected_source_blake3"] =
        json!(blake3::hash(source.as_bytes()).to_hex().to_string());
    let error = call(&engram, req).await.unwrap_err();
    assert!(error.message.contains("response exceeds 4 MiB"), "{error}");
    assert_eq!(
        engram
            .state
            .registry
            .snapshot_rule_ownership("adoption")
            .unwrap(),
        before
    );
    assert!(engram.state.projects.is_empty());
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test]
async fn a_source_identity_exceeding_the_receipt_limit_fails_before_pending_or_search_writes() {
    let (temporary, engram) = fixture(true, false);
    let relative = format!("{}/policy.json", vec!["a".repeat(135); 4].join("/"));
    let path = temporary.path().join("repo").join(&relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, SOURCE).unwrap();
    let before = engram
        .state
        .registry
        .snapshot_rule_ownership("adoption")
        .unwrap();
    let error = engram.handle_ingest_quality_gates(serde_json::from_value(json!({
        "project_id":"adoption","source_path":relative,"source_type":"rules","clear_existing":true
    })).unwrap()).await.unwrap_err();
    assert!(error.message.contains("quality source identity"), "{error}");
    let after = engram
        .state
        .registry
        .snapshot_rule_ownership("adoption")
        .unwrap();
    assert_eq!(after.rules(), before.rules());
    assert_eq!(after.receipts(), before.receipts());
    assert_eq!(
        engram
            .state
            .get_project_cached("adoption")
            .unwrap()
            .search
            .count_docs_by_namespace("adoption")
            .unwrap()
            .get("quality_gate"),
        None
    );
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test]
async fn source_identity_never_trims_a_real_directory_into_a_different_source() {
    let (temporary, engram) = fixture(true, false);
    let root = temporary.path().join("repo");
    for directory in [" area", "area"] {
        std::fs::create_dir(root.join(directory)).unwrap();
    }
    std::fs::write(root.join(" area/policy.json"), SOURCE).unwrap();
    std::fs::write(root.join("area/policy.json"), "[]").unwrap();
    let mut req = request(&engram, "preview", "adopt_unverified");
    req["source_path"] = json!(" area/policy.json");
    req["adoption"]["bindings"] = json!([]);
    let discovery = call(&engram, req).await.unwrap();
    assert_eq!(discovery["source"], " area/policy.json");
    assert_eq!(discovery["source_entries"].as_array().unwrap().len(), 1);
    let response = engram
        .handle_ingest_quality_gates(
            serde_json::from_value(json!({
                "project_id":"adoption","source_path":" area/policy.json","source_type":"rules"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        response.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("Ingested 1")
    );
    assert_eq!(
        engram
            .state
            .get_project_cached("adoption")
            .unwrap()
            .search
            .count_docs_by_namespace("adoption")
            .unwrap()
            .get("quality_gate"),
        Some(&1)
    );
    let rows = engram.state.registry.list_repo_rules("adoption").unwrap();
    assert!(
        rows.iter()
            .any(|r| r.provenance.as_deref() == Some("quality-gate source:  area/policy.json"))
    );
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test]
async fn exact_apply_and_reimports_preserve_existing_ids_content_scope_priority_and_dates() {
    let (temporary, engram) = fixture(true, false);
    let before = engram
        .state
        .registry
        .snapshot_rule_ownership("adoption")
        .unwrap();
    let mut req = request(&engram, "preview", "adopt_unverified");
    let preview = call(&engram, req.clone()).await.unwrap();
    assert_eq!(preview["status"], "ready_for_explicit_apply");
    assert_eq!(
        engram
            .state
            .registry
            .snapshot_rule_ownership("adoption")
            .unwrap(),
        before
    );
    assert!(engram.state.projects.is_empty());
    req["adoption"]["mode"] = json!("apply");
    req["adoption"]["expected_plan_blake3"] = preview["plan_blake3"].clone();
    let applied = call(&engram, req.clone()).await.unwrap();
    assert_eq!(applied["status"], "adopted");
    assert_eq!(applied["searchable_evidence_published"], false);
    let effective = engram_server::services::quality_source::effective_repo_rules(
        &engram.state,
        "adoption",
        None,
    )
    .unwrap();
    assert_eq!(
        effective.rules.len(),
        1,
        "Verified adopted rules retain their existing enforcement"
    );
    assert!(
        effective
            .notes
            .iter()
            .any(|note| note.contains("no searchable evidence was published")),
        "Recording ownership must not imply searchable evidence was imported"
    );
    assert!(
        engram.state.projects.is_empty(),
        "Ownership commit does not implicitly import searchable evidence"
    );
    let adopted = engram
        .state
        .registry
        .snapshot_rule_ownership("adoption")
        .unwrap();
    let mut expected: RepoRule = serde_json::from_slice(&before.rules()["old-policy-id"]).unwrap();
    expected.provenance = Some("quality-gate source: policy.json".into());
    assert_eq!(
        engram.state.registry.list_repo_rules("adoption").unwrap(),
        vec![expected.clone()]
    );
    assert!(
        call(&engram, req).await.is_err(),
        "An old approval cannot be reused after ownership changed"
    );
    for _ in 0..2 {
        let result = engram
            .handle_ingest_quality_gates(
                serde_json::from_value(json!({
                    "project_id":"adoption","source_path":"policy.json","source_type":"rules"
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            result.content[0]
                .as_text()
                .unwrap()
                .text
                .contains("Ingested 1")
        );
        assert_eq!(
            engram.state.registry.list_repo_rules("adoption").unwrap(),
            vec![expected.clone()]
        );
        assert_eq!(
            engram
                .state
                .registry
                .snapshot_rule_ownership("adoption")
                .unwrap()
                .rules(),
            adopted.rules(),
            "No-op reimport must preserve exact stored row bytes, not reset dates or rewrite opaque fields"
        );
    }
    let engine = engram.state.get_project_cached("adoption").unwrap();
    assert_eq!(
        engine
            .search
            .count_docs_by_namespace("adoption")
            .unwrap()
            .get("quality_gate"),
        Some(&1)
    );
    drop(engine);
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test]
async fn changed_source_row_or_claim_invalidates_approval_without_adoption_writes() {
    for change in ["source", "row", "claim"] {
        let (temporary, engram) = fixture(true, false);
        let mut req = request(&engram, "preview", "adopt_unverified");
        let preview = call(&engram, req.clone()).await.unwrap();
        req["adoption"]["mode"] = json!("apply");
        req["adoption"]["expected_plan_blake3"] = preview["plan_blake3"].clone();
        match change {
            "source" => std::fs::write(temporary.path().join("repo/policy.json"), "[]").unwrap(),
            "row" => {
                let mut row = engram
                    .state
                    .registry
                    .list_repo_rules("adoption")
                    .unwrap()
                    .remove(0);
                row.priority += 1;
                engram
                    .state
                    .registry
                    .put_repo_rule("adoption", &row)
                    .unwrap();
            }
            _ => engram
                .state
                .registry
                .set_meta("adoption", "quality_source:unreadable", "not a receipt")
                .unwrap(),
        }
        let changed = engram
            .state
            .registry
            .snapshot_rule_ownership("adoption")
            .unwrap();
        assert!(
            call(&engram, req).await.is_err(),
            "Stale {change} must fail"
        );
        assert_eq!(
            engram
                .state
                .registry
                .snapshot_rule_ownership("adoption")
                .unwrap(),
            changed
        );
        assert!(engram.state.projects.is_empty());
        drop(engram);
        temporary.close().unwrap();
    }
}

#[tokio::test]
async fn a_changed_positional_source_projection_conflicts_before_search_or_pending_receipt_writes()
{
    let (temporary, engram) = fixture(true, false);
    let mut req = request(&engram, "preview", "adopt_unverified");
    let preview = call(&engram, req.clone()).await.unwrap();
    req["adoption"]["mode"] = json!("apply");
    req["adoption"]["expected_plan_blake3"] = preview["plan_blake3"].clone();
    call(&engram, req).await.unwrap();
    // Open/import once, then prove the conflict causes no Registry or evidence mutation.
    let import = json!({"project_id":"adoption","source_path":"policy.json","source_type":"rules","clear_existing":true});
    engram
        .handle_ingest_quality_gates(serde_json::from_value(import.clone()).unwrap())
        .await
        .unwrap();
    let before = engram
        .state
        .registry
        .snapshot_rule_ownership("adoption")
        .unwrap();
    std::fs::write(
        temporary.path().join("repo/policy.json"),
        SOURCE.replace("ScopeProbe", "DifferentProbe"),
    )
    .unwrap();
    let error = engram
        .handle_ingest_quality_gates(serde_json::from_value(import).unwrap())
        .await
        .unwrap_err();
    assert!(
        error.message.contains("source projection changed")
            && error.message.contains("text")
            && error.message.contains("before BLAKE3")
            && error.message.contains("after BLAKE3"),
        "{error}"
    );
    assert_eq!(
        engram
            .state
            .registry
            .snapshot_rule_ownership("adoption")
            .unwrap(),
        before
    );
    assert_eq!(
        engram
            .state
            .get_project_cached("adoption")
            .unwrap()
            .search
            .count_docs_by_namespace("adoption")
            .unwrap()
            .get("quality_gate"),
        Some(&1)
    );
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test]
async fn an_existing_manual_rule_requires_the_specific_transfer_decision() {
    let (temporary, engram) = fixture(true, true);
    let before = engram
        .state
        .registry
        .snapshot_rule_ownership("adoption")
        .unwrap();
    let refused = call(&engram, request(&engram, "preview", "adopt_unverified"))
        .await
        .unwrap();
    assert_eq!(refused["status"], "blocked");
    assert_eq!(
        engram
            .state
            .registry
            .snapshot_rule_ownership("adoption")
            .unwrap(),
        before
    );
    let mut req = request(&engram, "preview", "transfer_manual");
    let preview = call(&engram, req.clone()).await.unwrap();
    assert_eq!(preview["status"], "ready_for_explicit_apply");
    req["adoption"]["mode"] = json!("apply");
    req["adoption"]["expected_plan_blake3"] = preview["plan_blake3"].clone();
    assert_eq!(call(&engram, req).await.unwrap()["status"], "adopted");
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test]
async fn an_adopted_target_colliding_with_a_new_source_promotion_is_rejected_without_writes() {
    let (temporary, engram) = fixture(true, false);
    let body = json!([
        {"rule":"Never add ScopeProbe tokens to production code. [check: forbid=ScopeProbe]","path_scope":"Site/**"},
        {"rule":"Always retain this additional boundary policy for every production endpoint.","path_scope":"Api/**"}
    ]).to_string();
    let parsed = engram_index::quality_gates::parse_quality_source_checked(
        &body,
        engram_index::quality_gates::QualitySource::CodingRulesMd,
        "policy.json",
    )
    .unwrap();
    let mut legacy = engram
        .state
        .registry
        .list_repo_rules("adoption")
        .unwrap()
        .remove(0);
    engram
        .state
        .registry
        .delete_repo_rule("adoption", &legacy.rule_id)
        .unwrap();
    legacy.rule_id = format!("qg-{}", parsed[1].id);
    engram
        .state
        .registry
        .put_repo_rule("adoption", &legacy)
        .unwrap();
    std::fs::write(temporary.path().join("repo/policy.json"), &body).unwrap();
    let snapshot = engram
        .state
        .registry
        .snapshot_rule_ownership("adoption")
        .unwrap();
    let mut req = json!({"project_id":"adoption","source_path":"policy.json","source_type":"rules",
        "adoption":{"mode":"preview","expected_source_blake3":blake3::hash(body.as_bytes()).to_hex().to_string(),
            "bindings":[{"rule_id":legacy.rule_id,"expected_raw_blake3":blake3::hash(&snapshot.rules()[&legacy.rule_id]).to_hex().to_string(),
                "source_rule_id":parsed[0].id,"decision":"adopt_unverified"}],"decision_ref":"explicit-owner-review:fixture"}});
    let preview = call(&engram, req.clone()).await.unwrap();
    req["adoption"]["mode"] = json!("apply");
    req["adoption"]["expected_plan_blake3"] = preview["plan_blake3"].clone();
    call(&engram, req).await.unwrap();
    let before = engram
        .state
        .registry
        .snapshot_rule_ownership("adoption")
        .unwrap();
    let error = engram.handle_ingest_quality_gates(serde_json::from_value(json!({
        "project_id":"adoption","source_path":"policy.json","source_type":"rules","clear_existing":true
    })).unwrap()).await.unwrap_err();
    assert!(
        error.message.contains("collides") && error.message.contains("No rules imported"),
        "{error}"
    );
    assert_eq!(
        engram
            .state
            .registry
            .snapshot_rule_ownership("adoption")
            .unwrap()
            .rules(),
        before.rules()
    );
    assert_eq!(
        engram
            .state
            .registry
            .snapshot_rule_ownership("adoption")
            .unwrap()
            .receipts(),
        before.receipts()
    );
    assert_eq!(
        engram
            .state
            .get_project_cached("adoption")
            .unwrap()
            .search
            .count_docs_by_namespace("adoption")
            .unwrap()
            .get("quality_gate"),
        None
    );
    drop(engram);
    temporary.close().unwrap();
}
