//! Ownership evidence must be exact and read-only; matching names or text
//! must never turn an old enforced row into accepted source-owned policy.
use engram_core::{Config, ProjectRecord, Registry, registry::RepoRule};
use engram_server::{AppState, Engram};
use serde_json::{Value, json};

fn fixture(legacy_identity: bool) -> (tempfile::TempDir, Engram) {
    fixture_with_raw_meta(legacy_identity, &[])
}

fn fixture_with_raw_meta(
    legacy_identity: bool,
    raw_meta: &[(String, Vec<u8>)],
) -> (tempfile::TempDir, Engram) {
    fixture_with_raw_state(legacy_identity, raw_meta, &[])
}

fn fixture_with_raw_state(
    legacy_identity: bool,
    raw_meta: &[(String, Vec<u8>)],
    raw_rules: &[RepoRule],
) -> (tempfile::TempDir, Engram) {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("repo");
    let data = temporary.path().join("data");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(data.join("registry")).unwrap();
    let project = ProjectRecord {
        project_id: "ownership".into(),
        project_name: "Ownership behavior".into(),
        project_type: "general".into(),
        directory: root.to_string_lossy().into_owned(),
        created_at_ms: 1,
        updated_at_ms: 1,
        reindex_required_since_ms: None,
    };
    if legacy_identity || !raw_meta.is_empty() || !raw_rules.is_empty() {
        let path = data.join("registry").join("registry.redb");
        drop(Registry::open(&path).unwrap());
        let database = redb::Database::create(&path).unwrap();
        let transaction = database.begin_write().unwrap();
        {
            let table: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new("projects");
            transaction
                .open_table(table)
                .unwrap()
                .insert(
                    "ownership",
                    serde_json::to_vec(&project).unwrap().as_slice(),
                )
                .unwrap();
            let metadata: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new("meta");
            let mut table = transaction.open_table(metadata).unwrap();
            for (key, bytes) in raw_meta {
                table
                    .insert(format!("ownership\0{key}").as_str(), bytes.as_slice())
                    .unwrap();
            }
            let rules: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new("repo_rules");
            let mut table = transaction.open_table(rules).unwrap();
            for row in raw_rules {
                table.insert(format!("ownership\0{}", row.rule_id).as_str(),
                    serde_json::to_vec(row).unwrap().as_slice()).unwrap();
            }
        }
        transaction.commit().unwrap();
        drop(database);
    }
    let (state, _) = AppState::new(Config {
        data_dir: data,
        allowed_roots: vec![root],
        embedding_backend: "fts_only".into(),
        ..Default::default()
    })
    .unwrap();
    if !legacy_identity {
        state.registry.put_project(&project).unwrap();
    }
    (temporary, Engram::new(state))
}

fn rule(id: &str, provenance: Option<&str>) -> RepoRule {
    RepoRule {
        rule_id: id.into(),
        file_pattern: "Site/**".into(),
        rule_text:
            "Å policy.\r\nException: read-only checks do not prompt.\nPreserve this final line."
                .into(),
        priority: 19,
        updated_at_ms: 42,
        introduced_at: Some("2025-09-12".into()),
        provenance: provenance.map(str::to_owned),
    }
}

async fn inventory(engram: &Engram, paths: &[&str]) -> Value {
    let result = engram
        .handle_list_repo_rules_with_ownership(
            serde_json::from_value(json!({
                "project_id":"ownership", "ownership_inventory":true, "source_paths":paths,
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap()
}

fn row<'a>(report: &'a Value, id: &str) -> &'a Value {
    report["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["rule_id"] == id)
        .unwrap()
}

fn receipt(
    engram: &Engram,
    origin: &str,
    rules: &[RepoRule],
    pending: bool,
    body: &[u8],
) -> String {
    let key = format!(
        "quality_source:{}",
        blake3::hash(origin.as_bytes()).to_hex()
    );
    let value = json!({"origin":origin, "source_hash":blake3::hash(body).to_hex().to_string(),
        "authored":true, "pending":pending, "doc_ids":[], "rules":rules,"cleanup":[],"corpus_revision":""});
    engram
        .state
        .registry
        .set_meta("ownership", &key, &value.to_string())
        .unwrap();
    key
}

#[tokio::test]
async fn exact_multiline_rows_and_missing_identity_are_observed_without_implicit_adoption() {
    let (temporary, engram) = fixture(true);
    let legacy = rule("coding-rules:legacy", None);
    let manual = rule("qg-board:manual", Some("manual"));
    for r in [&legacy, &manual] {
        engram.state.registry.put_repo_rule("ownership", r).unwrap();
    }
    let root = temporary.path().join("repo");
    std::fs::write(root.join("coding-rules.md"), &legacy.rule_text).unwrap();
    let before = engram
        .state
        .registry
        .snapshot_rule_ownership("ownership")
        .unwrap();
    assert_eq!(before.identity_epoch().unwrap(), None);
    let report = inventory(&engram, &["coding-rules.md"]).await;
    let observed = row(&report, &legacy.rule_id);
    assert_eq!(
        observed["decoded"]["rule_text"], legacy.rule_text,
        "The API must preserve real newlines instead of flattening display text"
    );
    assert_eq!(observed["decoded"]["file_pattern"], legacy.file_pattern);
    assert_eq!(observed["decoded"]["priority"], legacy.priority);
    assert_eq!(observed["decoded"]["updated_at_ms"], 42);
    assert_eq!(observed["authority"], "legacy_unverified");
    assert_eq!(
        observed["registry_enforcement"],
        "retained_unfiltered_registry_constraint"
    );
    assert_eq!(
        observed["ownership_status"], "no_receipt_claim_explicit_decision_required",
        "Matching basename and exact source text are not source ownership"
    );
    assert_eq!(
        row(&report, &manual.rule_id)["authority"],
        "accepted_manual",
        "A source-like ID must not override explicit manual provenance"
    );
    assert_eq!(
        report["identity_status"],
        "missing_epoch_adoption_requires_explicit_identity_establishment"
    );
    assert!(report["identity_epoch"].is_null());
    assert_eq!(
        report["sources"][0]["blake3"],
        blake3::hash(legacy.rule_text.as_bytes())
            .to_hex()
            .to_string()
    );
    assert_eq!(report["adoption_performed"], false);
    assert_eq!(report["authored_files_written"], 0);
    assert_eq!(
        engram
            .state
            .registry
            .snapshot_rule_ownership("ownership")
            .unwrap(),
        before
    );
    assert!(
        engram.state.projects.is_empty(),
        "Inventory must not initialize native project runtimes"
    );
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test]
async fn all_receipt_claims_and_malformed_or_pending_evidence_remain_visible() {
    let (temporary, engram) = fixture(false);
    let legacy = rule("legacy", None);
    engram
        .state
        .registry
        .put_repo_rule("ownership", &legacy)
        .unwrap();
    let root = temporary.path().join("repo");
    let body = b"Source existence does not settle ownership.\n";
    std::fs::write(root.join("a.md"), body).unwrap();
    std::fs::write(root.join("b.md"), body).unwrap();
    receipt(&engram, "a.md", std::slice::from_ref(&legacy), false, body);
    let other = receipt(&engram, "b.md", std::slice::from_ref(&legacy), false, body);
    let report = inventory(&engram, &[]).await;
    assert_eq!(
        row(&report, "legacy")["receipt_claims"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        row(&report, "legacy")["ownership_status"],
        "ambiguous_multiple_receipt_claims"
    );
    assert_eq!(
        row(&report, "legacy")["registry_enforcement"],
        "retained_unfiltered_registry_constraint"
    );
    let pending = receipt(&engram, "b.md", std::slice::from_ref(&legacy), true, body);
    assert_eq!(other, pending);
    let malformed = "{broken receipt; do not discard it";
    engram
        .state
        .registry
        .set_meta("ownership", "quality_source:broken", malformed)
        .unwrap();
    let before = engram
        .state
        .registry
        .snapshot_rule_ownership("ownership")
        .unwrap();
    let report = inventory(&engram, &[]).await;
    assert_eq!(report["receipts"].as_array().unwrap().len(), 3);
    let broken = report["receipts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["metadata_key"] == "quality_source:broken")
        .unwrap();
    assert_eq!(broken["status"], "unreadable_claims");
    assert_eq!(broken["raw"]["utf8"], malformed);
    assert!(
        report["unresolved_claim_keys"]
            .as_array()
            .unwrap()
            .contains(&json!(pending))
    );
    assert!(
        report["unresolved_claim_keys"]
            .as_array()
            .unwrap()
            .contains(&json!("quality_source:broken"))
    );
    assert_eq!(
        row(&report, "legacy")["ownership_status"],
        "unresolved_global_receipt_evidence"
    );
    assert_eq!(
        engram
            .state
            .registry
            .snapshot_rule_ownership("ownership")
            .unwrap(),
        before
    );
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test]
async fn inventory_discloses_source_staleness_without_changing_stored_enforcement_or_claims() {
    let (temporary, engram) = fixture(false);
    let root = temporary.path().join("repo");
    let original = b"Accepted source condition.\n";
    std::fs::write(root.join("policy.md"), original).unwrap();
    let managed = rule("managed", Some("quality-gate source: policy.md"));
    engram
        .state
        .registry
        .put_repo_rule("ownership", &managed)
        .unwrap();
    receipt(
        &engram,
        "policy.md",
        std::slice::from_ref(&managed),
        false,
        original,
    );
    let before = engram
        .state
        .registry
        .snapshot_rule_ownership("ownership")
        .unwrap();
    let current = inventory(&engram, &[]).await;
    assert_eq!(
        row(&current, "managed")["registry_enforcement"],
        "retained_unfiltered_registry_constraint"
    );
    assert_eq!(
        current["receipts"][0]["declared_source_blake3"],
        current["sources"][0]["blake3"]
    );
    assert_eq!(
        current["receipts"][0]["source_fingerprint_status"],
        "matches_declared_bytes_not_ownership"
    );
    std::fs::write(root.join("policy.md"), "Human changed the policy source.\n").unwrap();
    let stale = inventory(&engram, &[]).await;
    assert_eq!(
        stale["receipts"][0]["source_fingerprint_status"],
        "differs_from_declared_source"
    );
    assert_eq!(
        row(&stale, "managed")["registry_enforcement"],
        "withheld_by_current_source_verification"
    );
    assert_ne!(
        current["sources"][0]["blake3"],
        stale["sources"][0]["blake3"]
    );
    assert_eq!(
        current["registry_snapshot_blake3"], stale["registry_snapshot_blake3"],
        "Filesystem observations must not be misrepresented as an atomic Registry snapshot"
    );
    assert!(
        stale["enforcement_notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().unwrap().contains("source changed"))
    );
    assert_eq!(
        engram
            .state
            .registry
            .snapshot_rule_ownership("ownership")
            .unwrap(),
        before
    );
    assert!(engram.state.projects.is_empty());
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test]
async fn invalid_identity_is_not_repaired_and_default_listing_stays_read_only() {
    let (temporary, engram) = fixture(false);
    let legacy = rule("legacy", None);
    engram
        .state
        .registry
        .put_repo_rule("ownership", &legacy)
        .unwrap();
    engram
        .state
        .registry
        .set_meta("ownership", "source_identity_epoch_v1", "broken identity")
        .unwrap();
    let before = engram
        .state
        .registry
        .snapshot_rule_ownership("ownership")
        .unwrap();
    let report = inventory(&engram, &[]).await;
    assert_eq!(report["identity_status"], "invalid_epoch_adoption_blocked");
    assert_eq!(report["identity_epoch_raw"]["utf8"], "broken identity");
    assert_eq!(
        engram
            .state
            .registry
            .snapshot_rule_ownership("ownership")
            .unwrap(),
        before
    );
    let listing = engram
        .handle_list_repo_rules_with_ownership(
            serde_json::from_value(json!({"project_id":"ownership"})).unwrap(),
        )
        .await
        .unwrap();
    assert!(
        listing.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("# Repository rules")
    );
    assert!(engram.state.projects.is_empty());
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test]
async fn empty_or_relative_legacy_project_roots_never_resolve_source_candidates_from_cwd() {
    for directory in ["", "relative-legacy-directory"] {
        let (temporary, engram) = fixture(false);
        let mut project = engram
            .state
            .registry
            .get_project("ownership")
            .unwrap()
            .unwrap();
        project.directory = directory.into();
        engram.state.registry.put_project(&project).unwrap();
        let managed = rule("managed", Some("quality-gate source: Cargo.toml"));
        engram
            .state
            .registry
            .put_repo_rule("ownership", &managed)
            .unwrap();
        receipt(
            &engram,
            "Cargo.toml",
            &[managed],
            false,
            b"Unrelated process-CWD files are not this project's source",
        );
        let before = engram
            .state
            .registry
            .snapshot_rule_ownership("ownership")
            .unwrap();
        let report = inventory(&engram, &["Cargo.toml"]).await;
        assert_eq!(
            report["sources"][0]["status"],
            "project_filesystem_root_unavailable"
        );
        assert!(report["sources"][0]["blake3"].is_null());
        assert_eq!(
            row(&report, "managed")["registry_enforcement"],
            "withheld_by_current_source_verification"
        );
        assert_eq!(
            engram
                .state
                .registry
                .snapshot_rule_ownership("ownership")
                .unwrap(),
            before
        );
        assert!(engram.state.projects.is_empty());
        drop(engram);
        temporary.close().unwrap();
    }
}

#[cfg(windows)]
#[tokio::test]
async fn redirected_policy_source_is_neither_fingerprinted_nor_treated_as_verified_enforcement() {
    use std::os::windows::process::CommandExt;
    let (temporary, engram) = fixture(false);
    let outside = temporary.path().join("private");
    let junction = temporary.path().join("repo").join("escape");
    std::fs::create_dir(&outside).unwrap();
    let body =
        b"Outside the registered project, even though this whole directory is an owned fixture.\n";
    std::fs::write(outside.join("policy.md"), body).unwrap();
    let command = std::process::Command::new("C:\\Windows\\System32\\cmd.exe")
        .args(["/D", "/C", "mklink", "/J"])
        .arg(&junction)
        .arg(&outside)
        .creation_flags(0x08000000)
        .output()
        .unwrap();
    assert!(
        command.status.success(),
        "Owned junction fixture setup failed"
    );
    let managed = rule("managed", Some("quality-gate source: escape/policy.md"));
    engram
        .state
        .registry
        .put_repo_rule("ownership", &managed)
        .unwrap();
    receipt(&engram, "escape/policy.md", &[managed], false, body);
    let result = engram
        .handle_list_repo_rules_with_ownership(
            serde_json::from_value(json!({"project_id":"ownership","ownership_inventory":true}))
                .unwrap(),
        )
        .await;
    // Remove only the link, before inspecting outcomes or tearing down stores.
    let removed = std::fs::remove_dir(&junction);
    let preserved = std::fs::read(outside.join("policy.md")).unwrap() == body;
    drop(engram);
    let closed = temporary.close();
    removed.unwrap();
    closed.unwrap();
    let response = result.unwrap();
    let report: Value = serde_json::from_str(&response.content[0].as_text().unwrap().text).unwrap();
    assert_eq!(report["sources"][0]["status"], "unavailable_or_unsafe");
    assert!(report["sources"][0]["blake3"].is_null());
    assert_eq!(
        row(&report, "managed")["registry_enforcement"],
        "withheld_by_current_source_verification"
    );
    assert!(preserved);
}

#[tokio::test]
async fn duplicate_keys_unknown_shapes_and_duplicate_claim_occurrences_never_prove_unique_ownership()
 {
    for case in [
        "duplicate_key",
        "unknown_top_field",
        "unknown_rule_field",
        "malformed_array",
        "duplicate_claim",
        "conflicting_claim",
    ] {
        let (temporary, engram) = fixture(false);
        let legacy = rule("legacy", None);
        engram
            .state
            .registry
            .put_repo_rule("ownership", &legacy)
            .unwrap();
        let body = b"Source evidence.\n";
        std::fs::write(temporary.path().join("repo/policy.md"), body).unwrap();
        let key = receipt(
            &engram,
            "policy.md",
            std::slice::from_ref(&legacy),
            false,
            body,
        );
        let mut value: Value = serde_json::from_str(
            &engram
                .state
                .registry
                .get_meta("ownership", &key)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        let text = match case {
            "duplicate_key" => {
                let mut foreign = legacy.clone();
                foreign.rule_id = "foreign-row".into();
                format!(
                    "{{\"origin\":\"policy.md\",\"source_hash\":\"{}\",\"authored\":true,\"pending\":false,\"doc_ids\":[],\"rules\":{},\"rules\":{}}}",
                    blake3::hash(body).to_hex(),
                    serde_json::to_string(&[legacy.clone()]).unwrap(),
                    serde_json::to_string(&[foreign]).unwrap()
                )
            }
            "unknown_top_field" => {
                value["other_ownership_claims"] = json!(["foreign-row"]);
                value.to_string()
            }
            "unknown_rule_field" => {
                value["rules"][0]["other_owner"] = json!("foreign");
                value.to_string()
            }
            "malformed_array" => {
                value["rules"].as_array_mut().unwrap().push(json!(17));
                value.to_string()
            }
            "duplicate_claim" => {
                let first = value["rules"][0].clone();
                value["rules"].as_array_mut().unwrap().push(first);
                value.to_string()
            }
            _ => {
                let mut conflict = legacy.clone();
                conflict.file_pattern = "Other/**".into();
                value["rules"]
                    .as_array_mut()
                    .unwrap()
                    .push(serde_json::to_value(conflict).unwrap());
                value.to_string()
            }
        };
        engram
            .state
            .registry
            .set_meta("ownership", &key, &text)
            .unwrap();
        let before = engram
            .state
            .registry
            .snapshot_rule_ownership("ownership")
            .unwrap();
        let report = inventory(&engram, &[]).await;
        assert_eq!(report["receipts"][0]["raw"]["utf8"], text);
        if matches!(case, "duplicate_claim" | "conflicting_claim") {
            let claims = row(&report, "legacy")["receipt_claims"].as_array().unwrap();
            assert_eq!(claims.len(), 2);
            assert_eq!(claims[0]["claim_index"], 0);
            assert_eq!(claims[1]["claim_index"], 1);
            assert_eq!(
                row(&report, "legacy")["claim_status"],
                if case == "duplicate_claim" {
                    "duplicate_claim_occurrences"
                } else {
                    "conflicting_claim_occurrences"
                }
            );
            assert_eq!(report["receipts"][0]["status"], "invalid_or_conflicting_claims");
            assert_eq!(row(&report, "legacy")["ownership_status"], "unresolved_global_receipt_evidence");
        } else {
            assert_eq!(report["receipts"][0]["status"], "unreadable_claims");
            assert!(
                row(&report, "legacy")["receipt_claims"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                row(&report, "legacy")["ownership_status"],
                "unresolved_global_receipt_evidence",
                "Zero parsed claims must not mean unclaimed when raw claims were not understood: {case}"
            );
        }
        assert_eq!(
            row(&report, "legacy")["registry_enforcement"],
            "retained_unfiltered_registry_constraint"
        );
        assert_eq!(
            engram
                .state
                .registry
                .snapshot_rule_ownership("ownership")
                .unwrap(),
            before
        );
        drop(engram);
        temporary.close().unwrap();
    }
}

#[tokio::test]
async fn inventory_and_normal_rule_consumers_agree_when_verification_budget_depends_on_order() {
    use std::collections::BTreeSet;
    let (temporary, engram) = fixture(false);
    let body = b"Policy.\n";
    for n in 0..65 {
        let origin = format!("policy{n:03}.md");
        std::fs::write(temporary.path().join("repo").join(&origin), body).unwrap();
        let mut managed = rule(
            &format!("rule{n:03}"),
            Some(&format!("quality-gate source: {origin}")),
        );
        managed.file_pattern = format!("Scope{:03}/**", 64 - n);
        engram
            .state
            .registry
            .put_repo_rule("ownership", &managed)
            .unwrap();
        receipt(&engram, &origin, &[managed], false, body);
    }
    let normal = engram_server::services::quality_source::effective_repo_rules(
        &engram.state,
        "ownership",
        None,
    )
    .unwrap();
    let expected: BTreeSet<_> = normal.rules.iter().map(|r| r.rule_id.clone()).collect();
    assert_eq!(expected.len(), 64);
    assert!(!expected.contains("rule000"));
    assert!(expected.contains("rule064"));
    let before = engram
        .state
        .registry
        .snapshot_rule_ownership("ownership")
        .unwrap();
    let report = inventory(&engram, &[]).await;
    let observed: BTreeSet<_> = report["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["registry_enforcement"] == "retained_unfiltered_registry_constraint")
        .map(|row| row["rule_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        observed, expected,
        "Inventory must not change which constraints fit the normal shared verification budget"
    );
    assert!(
        report["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["status"] == "source_path_budget_omitted")
    );
    assert_eq!(
        engram
            .state
            .registry
            .snapshot_rule_ownership("ownership")
            .unwrap(),
        before
    );
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test]
async fn escaped_evidence_overflow_is_refused_without_truncation_or_rewriting_the_row() {
    let (temporary, engram) = fixture(false);
    let mut legacy = rule("large-legacy", None);
    legacy.rule_text = "\\".repeat(512 * 1024);
    engram
        .state
        .registry
        .put_repo_rule("ownership", &legacy)
        .unwrap();
    let before = engram
        .state
        .registry
        .snapshot_rule_ownership("ownership")
        .unwrap();
    let result = engram
        .handle_list_repo_rules_with_ownership(
            serde_json::from_value(json!({"project_id":"ownership","ownership_inventory":true}))
                .unwrap(),
        )
        .await;
    let error = result.unwrap_err();
    assert!(error.message.contains("response budget"), "{error}");
    assert_eq!(
        engram
            .state
            .registry
            .snapshot_rule_ownership("ownership")
            .unwrap(),
        before,
        "Encoded-size refusal must not truncate, normalize or remove stored policy"
    );
    assert!(engram.state.projects.is_empty());
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test]
async fn unreadable_receipt_bytes_consume_the_same_verification_allowance_as_valid_receipts() {
    let raw = ["bad0.md", "bad1.md"]
        .into_iter()
        .map(|origin| {
            (
                format!(
                    "quality_source:{}",
                    blake3::hash(origin.as_bytes()).to_hex()
                ),
                vec![0xff; 4 * 1024 * 1024],
            )
        })
        .collect::<Vec<_>>();
    let mut rows = Vec::new();
    for (n, origin) in ["bad0.md", "bad1.md"].into_iter().enumerate() {
        let mut bad = rule(
            &format!("bad{n}"),
            Some(&format!("quality-gate source: {origin}")),
        );
        bad.file_pattern = format!("Bad{n}/**");
        rows.push(bad);
    }
    let body = b"Accepted policy stays intact.\n";
    let mut good = rule("good", Some("quality-gate source: good.md"));
    good.file_pattern = "Good/**".into();
    rows.push(good.clone());
    // Corrupt persisted state is installed before opening AppState. Production
    // mutation correctly refuses changes while global ownership is unreadable.
    let (temporary, engram) = fixture_with_raw_state(false, &raw, &rows);
    std::fs::write(temporary.path().join("repo/good.md"), body).unwrap();
    receipt(&engram, "good.md", &[good], false, body);
    let before = engram.state.registry.list_repo_rules("ownership").unwrap();
    let all = engram_server::services::quality_source::effective_repo_rules(
        &engram.state,
        "ownership",
        None,
    )
    .unwrap();
    assert!(
        all.rules.is_empty(),
        "After admitting two unreadable4MiB values, no additional receipt may use the exhausted8MiB budget"
    );
    assert!(
        all.notes
            .iter()
            .any(|n| n.contains("good.md") && n.contains("byte budget exhausted"))
    );
    let fresh = engram_server::services::quality_source::effective_repo_rules(
        &engram.state,
        "ownership",
        Some(&["Good/feature.rs"]),
    )
    .unwrap();
    assert_eq!(
        fresh
            .rules
            .iter()
            .map(|r| r.rule_id.as_str())
            .collect::<Vec<_>>(),
        vec!["good"],
        "A fresh bounded request must still verify the unaffected good source"
    );
    assert_eq!(
        engram.state.registry.list_repo_rules("ownership").unwrap(),
        before
    );
    for (key, bytes) in &raw {
        let same = engram
            .state
            .registry
            .map_meta_bounded("ownership", key, 4 * 1024 * 1024, |stored| {
                Ok(stored == bytes.as_slice())
            })
            .unwrap();
        assert_eq!(
            same,
            Some(true),
            "Refusal must not repair or delete unreadable evidence"
        );
    }
    drop(engram);
    temporary.close().unwrap();
}
