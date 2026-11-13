//! Real authored-reference forms, independent of the indexed current checkout.
use super::*;

fn root_state<'a>(manifest: &'a Value, path: &str) -> &'a Value {
    manifest["instructions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["instruction_path"] == path)
        .unwrap()
}

#[tokio::test]
async fn repository_relative_inline_code_path_supplies_complete_historical_and_current_source() {
    let (temporary, engram, mut request) = fixture();
    let root = temporary.path().join("repo");
    let before = "Public Class ReportModels\n    Public Value As Integer = 1\nEnd Class\n";
    let after = "Public Class ReportModels\n    Public Value As Integer = 2\nEnd Class\n";
    std::fs::write(root.join("rules/area.md"), "Paths are relative to the repository root.\nUse `Site/ReportModels.vb` for the document model.\n").unwrap();
    std::fs::write(root.join("Site/ReportModels.vb"), before).unwrap();
    request["instruction_maintenance"]["base_commit"] = json!(commit(&root));
    std::fs::write(root.join("Site/ReportModels.vb"), after).unwrap();
    let report = call(&engram, request).await.unwrap();
    let manifest = &report["packet"]["instruction_manifest"];
    let input = manifest["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["path"] == "Site/ReportModels.vb")
        .unwrap();
    assert_eq!(input["before"]["snapshot"]["content"], before);
    assert_eq!(input["after"]["snapshot"]["content"], after);
    let edge = manifest["edges"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["version"] == "after")
        .unwrap();
    assert_eq!(edge["kind"], "inline_code_path");
    assert_eq!(edge["normalized_path"], "Site/ReportModels.vb");
    assert_eq!(
        edge["path_basis"],
        "only_present_document_or_project_relative_candidate"
    );
    assert_eq!(
        root_state(manifest, "rules/area.md")["status"],
        "review_changed_dependencies"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("Site/ReportModels.vb")).unwrap(),
        after
    );
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test]
async fn conflicting_inline_path_bases_supply_both_candidates_without_selecting_authority() {
    let (temporary, engram, mut request) = fixture();
    let root = temporary.path().join("repo");
    std::fs::create_dir_all(root.join("common")).unwrap();
    std::fs::create_dir_all(root.join("rules/common")).unwrap();
    std::fs::write(
        root.join("common/shared.md"),
        "Root guidance with exception A.\n",
    )
    .unwrap();
    std::fs::write(
        root.join("rules/common/shared.md"),
        "Area guidance with exception B.\n",
    )
    .unwrap();
    std::fs::write(root.join("rules/area.md"), "Consult `common/shared.md`.\n").unwrap();
    request["instruction_maintenance"]["base_commit"] = json!(commit(&root));
    let report = call(&engram, request).await.unwrap();
    let manifest = &report["packet"]["instruction_manifest"];
    for edge in manifest["edges"].as_array().unwrap() {
        assert_eq!(edge["status"], "ambiguous_literal_path_base");
        assert!(edge["normalized_path"].is_null());
        assert_eq!(edge["candidate_paths"].as_array().unwrap().len(), 2);
    }
    for (path, content) in [
        ("common/shared.md", "Root guidance with exception A.\n"),
        (
            "rules/common/shared.md",
            "Area guidance with exception B.\n",
        ),
    ] {
        let d = manifest["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["path"] == path)
            .unwrap();
        assert_eq!(d["after"]["snapshot"]["content"], content);
    }
    assert_eq!(
        root_state(manifest, "rules/area.md")["status"],
        "needs_reference_evidence"
    );
    assert_eq!(report["authored_files_written"], 0);
    drop(engram);
    temporary.close().unwrap();
}

const TWO_FUNCTIONS: &str =
    "pub fn Important() -> i32 {\n    10\n}\n\npub fn Other() -> i32 {\n    20\n}\n";

#[tokio::test]
async fn javascript_and_typescript_guidance_only_tracks_authored_declarations() {
    for extension in ["js", "ts"] {
        for newline in ["", "\n"] {
            let (temporary, engram, mut request) = fixture();
            let root = temporary.path().join("repo");
            let path = format!("Site/logic.{extension}");
            let before = format!("export function Important() {{\n    return 10;\n}}\n\nexport function Other() {{\n    return 20;\n}}{newline}");
            for (name, text) in [
                ("area", "Preserve `Important`.\n"),
                ("other", "Other must retain its behavior.\n"),
                ("prose", "Keep this file readable.\n"),
            ] {
                std::fs::write(root.join(format!("rules/{name}.md")), text).unwrap();
            }
            std::fs::write(root.join(&path), &before).unwrap();
            request["instruction_maintenance"]["base_commit"] = json!(commit(&root));
            request["instruction_maintenance"]["evidence_paths"] = json!([path]);
            request["instruction_maintenance"]["instruction_paths"] =
                json!(["rules/area.md", "rules/other.md", "rules/prose.md"]);
            let after = before.replace("return 20", "return 30");
            std::fs::write(root.join(&path), &after).unwrap();
            let report = call(&engram, request).await.unwrap();
            let manifest = &report["packet"]["instruction_manifest"];
            let area = root_state(manifest, "rules/area.md");
            assert_eq!(area["status"], "no_collected_dependency_changes", "{extension} {newline:?}: {area}");
            assert_eq!(area["symbol_changes"][0]["status"], "same_observed_candidate_bytes");
            assert_eq!(root_state(manifest, "rules/other.md")["status"], "review_changed_dependencies");
            let prose = root_state(manifest, "rules/prose.md");
            assert_eq!(prose["status"], "no_collected_dependency_changes");
            assert!(prose["symbol_reference_indices"].as_array().unwrap().is_empty());
            let candidate = &area["symbol_changes"][0]["after_candidates"][0];
            let range = candidate["byte_range"].as_array().unwrap();
            let raw = &after[range[0].as_u64().unwrap() as usize..range[1].as_u64().unwrap() as usize];
            assert!(raw.contains("function Important") && !raw.contains("function Other"));
            assert_eq!(candidate["evidence_blake3"], blake3::hash(raw.as_bytes()).to_hex().to_string());
            assert_eq!(candidate["absence_proof"], false);
            assert_eq!(report["authored_files_written"], 0);
            assert_eq!(std::fs::read_to_string(root.join(&path)).unwrap(), after);
            drop(engram);
            temporary.close().unwrap();
        }
    }
}

#[tokio::test]
async fn actual_function_named_file_remains_instruction_evidence() {
    let (temporary, engram, mut request) = fixture();
    let root = temporary.path().join("repo");
    std::fs::write(root.join("rules/area.md"), "Preserve `file`.\n").unwrap();
    let before = "export function file() { return 1; }\n";
    std::fs::write(root.join("Site/logic.ts"), before).unwrap();
    request["instruction_maintenance"]["base_commit"] = json!(commit(&root));
    request["instruction_maintenance"]["evidence_paths"] = json!(["Site/logic.ts"]);
    let after = before.replace("return 1", "return 2");
    std::fs::write(root.join("Site/logic.ts"), &after).unwrap();
    let report = call(&engram, request).await.unwrap();
    let state = root_state(&report["packet"]["instruction_manifest"], "rules/area.md");
    assert_eq!(state["status"], "review_changed_dependencies");
    let change = &state["symbol_changes"][0];
    assert_eq!(change["status"], "changed_observed_candidate_bytes");
    assert_eq!(change["after_candidates"].as_array().unwrap().len(), 1);
    assert_eq!(change["after_candidates"][0]["kind"], "function");
    assert_eq!(change["after_candidates"][0]["name"], "file");
    assert_eq!(report["authored_files_written"], 0);
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test]
async fn prose_case_collisions_do_not_create_vb_dependencies_but_explicit_names_still_do() {
    let (temporary, engram, mut request) = fixture();
    let root = temporary.path().join("repo");
    let source = "Public Class Resolved\n    Public Value As Integer = 1\nEnd Class\n";
    for (name, text) in [
        ("prose", "Keep the resolved style when exporting.\n"),
        ("explicit", "Preserve `resolved` for callers.\n"),
        ("named", "Resolved carries the selected value.\n"),
    ] {
        std::fs::write(root.join(format!("rules/{name}.md")), text).unwrap();
    }
    std::fs::write(root.join("Site/model.vb"), source).unwrap();
    request["instruction_maintenance"]["base_commit"] = json!(commit(&root));
    request["instruction_maintenance"]["instruction_paths"] =
        json!(["rules/prose.md", "rules/explicit.md", "rules/named.md"]);
    request["instruction_maintenance"]["evidence_paths"] = json!(["Site/model.vb"]);
    std::fs::write(root.join("Site/model.vb"), source.replace("= 1", "= 2")).unwrap();

    let report = call(&engram, request).await.unwrap();
    let manifest = &report["packet"]["instruction_manifest"];
    let prose = root_state(manifest, "rules/prose.md");
    assert_eq!(prose["status"], "no_collected_dependency_changes");
    assert!(
        prose["symbol_reference_indices"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    for path in ["rules/explicit.md", "rules/named.md"] {
        let state = root_state(manifest, path);
        assert_eq!(state["status"], "review_changed_dependencies");
        assert_eq!(
            state["symbol_changes"][0]["status"],
            "candidate_file_changed_body_coverage_unavailable"
        );
        assert_eq!(
            state["symbol_changes"][0]["after_candidates"][0]["name"],
            "Resolved"
        );
    }
    assert_eq!(report["authored_files_written"], 0);
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test]
async fn exact_prose_candidate_does_not_pull_in_a_differently_cased_vb_declaration() {
    let (temporary, engram, mut request) = fixture();
    let root = temporary.path().join("repo");
    let vb = "Public Class Resolved\n    Public Value As Integer = 1\nEnd Class\n";
    std::fs::write(
        root.join("rules/area.md"),
        "Use resolved for this operation.\n",
    )
    .unwrap();
    std::fs::write(root.join("rules/explicit.md"), "Inspect `resolved`.\n").unwrap();
    std::fs::write(root.join("Site/model.vb"), vb).unwrap();
    std::fs::write(
        root.join("Site/logic.rs"),
        "pub fn resolved() -> i32 { 10 }\n",
    )
    .unwrap();
    request["instruction_maintenance"]["base_commit"] = json!(commit(&root));
    request["instruction_maintenance"]["instruction_paths"] =
        json!(["rules/area.md", "rules/explicit.md"]);
    request["instruction_maintenance"]["evidence_paths"] =
        json!(["Site/model.vb", "Site/logic.rs"]);
    std::fs::write(root.join("Site/model.vb"), vb.replace("= 1", "= 2")).unwrap();

    let report = call(&engram, request).await.unwrap();
    let manifest = &report["packet"]["instruction_manifest"];
    assert_eq!(
        root_state(manifest, "rules/area.md")["status"],
        "no_collected_dependency_changes"
    );
    let references = manifest["symbol_references"].as_array().unwrap();
    for path in ["rules/area.md", "rules/explicit.md"] {
        for version in ["before", "after"] {
            assert!(references.iter().any(|reference| {
                reference["source_path"] == path && reference["version"] == version
            }));
        }
    }
    for reference in references {
        let candidates = reference["candidates"].as_array().unwrap();
        if reference["source_path"] == "rules/area.md" {
            assert_eq!(candidates.len(), 1);
            assert_eq!(candidates[0]["path"], "Site/logic.rs");
        } else {
            assert_eq!(reference["source_path"], "rules/explicit.md");
            assert_eq!(candidates.len(), 2);
            assert_eq!(reference["status"], "ambiguous_symbol_candidates");
        }
    }
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test]
async fn symbol_body_evidence_distinguishes_a_referenced_change_from_an_unrelated_body_edit() {
    let (temporary, engram, mut request) = fixture();
    let root = temporary.path().join("repo");
    std::fs::write(root.join("rules/area.md"), "Preserve `Important`.\n").unwrap();
    std::fs::write(
        root.join("rules/other.md"),
        "Other must retain its behavior.\n",
    )
    .unwrap();
    std::fs::write(root.join("Site/logic.rs"), TWO_FUNCTIONS).unwrap();
    request["instruction_maintenance"]["base_commit"] = json!(commit(&root));
    request["instruction_maintenance"]["evidence_paths"] = json!(["Site/logic.rs"]);
    request["instruction_maintenance"]["instruction_paths"] =
        json!(["rules/area.md", "rules/other.md"]);
    std::fs::write(
        root.join("Site/logic.rs"),
        TWO_FUNCTIONS.replace("20", "30"),
    )
    .unwrap();
    let report = call(&engram, request.clone()).await.unwrap();
    let manifest = &report["packet"]["instruction_manifest"];
    let area = root_state(manifest, "rules/area.md");
    assert_eq!(area["status"], "no_collected_dependency_changes");
    assert_eq!(
        area["symbol_changes"][0]["status"],
        "same_observed_candidate_bytes"
    );
    let other = root_state(manifest, "rules/other.md");
    assert_eq!(other["status"], "review_changed_dependencies");
    assert_eq!(
        other["symbol_changes"][0]["status"],
        "changed_observed_candidate_bytes"
    );
    assert!(
        manifest["symbol_references"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["literal"] == "Other"
                && r["form"] == "plain_text_matching_collected_candidate")
    );
    let before = &area["symbol_changes"][0]["before_candidates"][0];
    let after = &area["symbol_changes"][0]["after_candidates"][0];
    assert_ne!(before["source_blake3"], after["source_blake3"]);
    assert_eq!(before["evidence_blake3"], after["evidence_blake3"]);
    let snapshot = report["packet"]["evidence"][0]["after"]["content"]
        .as_str()
        .unwrap();
    let span = after["byte_range"].as_array().unwrap();
    let actual = &snapshot[span[0].as_u64().unwrap() as usize..span[1].as_u64().unwrap() as usize];
    assert!(actual.contains("pub fn Important()") && !actual.contains("pub fn Other()"));
    assert_eq!(
        after["evidence_blake3"],
        blake3::hash(actual.as_bytes()).to_hex().to_string()
    );
    assert_eq!(after["absence_proof"], false);
    assert_eq!(
        call(&engram, request).await.unwrap()["packet_blake3"],
        report["packet_blake3"]
    );
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test]
async fn symbol_rename_preserves_before_evidence_and_reports_unresolved_current_reference() {
    let (temporary, engram, mut request) = fixture();
    let root = temporary.path().join("repo");
    std::fs::write(root.join("rules/area.md"), "Preserve `Important`.\n").unwrap();
    std::fs::write(root.join("Site/logic.rs"), TWO_FUNCTIONS).unwrap();
    request["instruction_maintenance"]["base_commit"] = json!(commit(&root));
    request["instruction_maintenance"]["evidence_paths"] = json!(["Site/logic.rs"]);
    std::fs::write(
        root.join("Site/logic.rs"),
        TWO_FUNCTIONS.replace("Important", "Renamed"),
    )
    .unwrap();
    let report = call(&engram, request).await.unwrap();
    let manifest = &report["packet"]["instruction_manifest"];
    let refs = manifest["symbol_references"].as_array().unwrap();
    let before = refs
        .iter()
        .find(|r| r["version"] == "before" && r["literal"] == "Important")
        .unwrap();
    let after = refs
        .iter()
        .find(|r| r["version"] == "after" && r["literal"] == "Important")
        .unwrap();
    assert_eq!(before["candidates"].as_array().unwrap().len(), 1);
    assert_eq!(after["status"], "unresolved_symbol_in_collected_evidence");
    assert!(after["candidates"].as_array().unwrap().is_empty());
    assert_eq!(
        root_state(manifest, "rules/area.md")["symbol_changes"][0]["status"],
        "candidate_not_located_after_no_absence_proof"
    );
    assert_eq!(
        root_state(manifest, "rules/area.md")["status"],
        "review_changed_dependencies"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("rules/area.md")).unwrap(),
        "Preserve `Important`.\n"
    );
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test]
async fn matching_names_in_multiple_files_remain_ambiguous_and_cannot_select_one_owner() {
    let (temporary, engram, mut request) = fixture();
    let root = temporary.path().join("repo");
    std::fs::write(root.join("rules/area.md"), "Preserve `Important`.\n").unwrap();
    for name in ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs", "f.rs"] {
        std::fs::write(root.join("Site").join(name), TWO_FUNCTIONS).unwrap();
    }
    request["instruction_maintenance"]["base_commit"] = json!(commit(&root));
    request["instruction_maintenance"]["evidence_paths"] = json!([
        "Site/a.rs",
        "Site/b.rs",
        "Site/c.rs",
        "Site/d.rs",
        "Site/e.rs",
        "Site/f.rs"
    ]);
    let report = call(&engram, request).await.unwrap();
    let manifest = &report["packet"]["instruction_manifest"];
    for reference in manifest["symbol_references"].as_array().unwrap() {
        assert_eq!(reference["status"], "ambiguous_symbol_candidates");
        assert_eq!(reference["candidates"].as_array().unwrap().len(), 4);
        assert_eq!(reference["observed_candidate_count"], 6);
        assert_eq!(reference["omitted_candidates"], 2);
    }
    assert_eq!(
        root_state(manifest, "rules/area.md")["status"],
        "needs_reference_evidence"
    );
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test]
async fn vb_candidates_supply_captured_bytes_without_certifying_bodies_or_absence() {
    let (temporary, engram, mut request) = fixture();
    let root = temporary.path().join("repo");
    let source = "Public Class Converter\n    Public Shared Function Convert() As String\n        Return \"old\"\n    End Function\nEnd Class\n";
    std::fs::write(
        root.join("rules/area.md"),
        "Preserve `Converter.Convert` and inspect `UnknownSymbol`.\n",
    )
    .unwrap();
    std::fs::write(root.join("Site/convert.vb"), source).unwrap();
    request["instruction_maintenance"]["base_commit"] = json!(commit(&root));
    request["instruction_maintenance"]["evidence_paths"] = json!(["Site/convert.vb"]);
    std::fs::write(root.join("Site/convert.vb"), source.replace("old", "new")).unwrap();
    let report = call(&engram, request).await.unwrap();
    let manifest = &report["packet"]["instruction_manifest"];
    let refs = manifest["symbol_references"].as_array().unwrap();
    let converter = refs
        .iter()
        .find(|r| r["literal"] == "Converter.Convert" && r["version"] == "after")
        .unwrap();
    assert!(!converter["candidates"].as_array().unwrap().is_empty());
    assert_eq!(
        converter["candidates"][0]["span_kind"],
        "regex_declaration_line_only"
    );
    assert_eq!(converter["candidates"][0]["absence_proof"], false);
    let unknown = refs
        .iter()
        .find(|r| r["literal"] == "UnknownSymbol" && r["version"] == "after")
        .unwrap();
    assert_eq!(unknown["status"], "unresolved_symbol_in_collected_evidence");
    assert!(
        manifest["symbol_inventory"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["status"] == "regex_declaration_line_candidates_no_absence_proof")
    );
    assert_eq!(report["packet"]["evidence"][0]["before"]["content"], source);
    assert_eq!(
        report["packet"]["evidence"][0]["after"]["content"],
        source.replace("old", "new")
    );
    assert_eq!(
        root_state(manifest, "rules/area.md")["coverage_status"],
        "incomplete_reference_coverage"
    );
    assert_eq!(report["authored_files_written"], 0);
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test]
async fn symbol_file_move_is_an_observed_binding_change_and_never_a_current_history_substitute() {
    let (temporary, engram, mut request) = fixture();
    let root = temporary.path().join("repo");
    std::fs::write(root.join("rules/area.md"), "Preserve `Important`.\n").unwrap();
    std::fs::write(root.join("Site/old.rs"), TWO_FUNCTIONS).unwrap();
    request["instruction_maintenance"]["base_commit"] = json!(commit(&root));
    request["instruction_maintenance"]["evidence_paths"] = json!(["Site/old.rs", "Site/new.rs"]);
    std::fs::rename(root.join("Site/old.rs"), root.join("Site/new.rs")).unwrap();
    let report = call(&engram, request).await.unwrap();
    let manifest = &report["packet"]["instruction_manifest"];
    let change = &root_state(manifest, "rules/area.md")["symbol_changes"][0];
    assert_eq!(change["status"], "observed_candidate_binding_changed");
    assert_eq!(change["before_candidates"][0]["path"], "Site/old.rs");
    assert_eq!(change["after_candidates"][0]["path"], "Site/new.rs");
    let old = report["packet"]["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["path"] == "Site/old.rs")
        .unwrap();
    assert_eq!(old["before"]["content"], TWO_FUNCTIONS);
    assert!(old["after"].is_null());
    assert_eq!(report["authored_files_written"], 0);
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test]
async fn truncated_symbol_inventory_cannot_hide_an_undiscovered_plain_text_reference() {
    let (temporary, engram, mut request) = fixture();
    let root = temporary.path().join("repo");
    std::fs::write(
        root.join("rules/area.md"),
        "Function299 has a required exception.\n",
    )
    .unwrap();
    let source = (0..300)
        .map(|n| format!("pub fn Function{n}() {{}}\n"))
        .collect::<String>();
    std::fs::write(root.join("Site/many.rs"), &source).unwrap();
    request["instruction_maintenance"]["base_commit"] = json!(commit(&root));
    request["instruction_maintenance"]["evidence_paths"] = json!(["Site/many.rs"]);
    let report = call(&engram, request).await.unwrap();
    let manifest = &report["packet"]["instruction_manifest"];
    assert!(manifest["symbol_references"].as_array().unwrap().is_empty());
    assert!(
        manifest["symbol_inventory"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["omitted_candidates"].as_u64().unwrap() > 0)
    );
    assert_eq!(
        root_state(manifest, "rules/area.md")["status"],
        "needs_reference_evidence"
    );
    assert!(
        manifest["coverage_issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["status"] == "symbol_inventory_incomplete_for_plain_text_discovery")
    );
    assert_eq!(report["packet"]["evidence"][0]["after"]["content"], source);
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test]
async fn unsupported_symbol_language_reports_a_gap_without_claiming_no_prose_references() {
    let (temporary, engram, mut request) = fixture();
    let root = temporary.path().join("repo");
    std::fs::write(
        root.join("rules/area.md"),
        "ScalarRule has a required exception.\n",
    )
    .unwrap();
    let source = "CREATE FUNCTION ScalarRule() RETURNS INT AS BEGIN RETURN 1; END\n";
    std::fs::write(root.join("Site/function.sql"), source).unwrap();
    request["instruction_maintenance"]["base_commit"] = json!(commit(&root));
    request["instruction_maintenance"]["evidence_paths"] = json!(["Site/function.sql"]);
    let report = call(&engram, request).await.unwrap();
    let manifest = &report["packet"]["instruction_manifest"];
    assert!(manifest["symbol_references"].as_array().unwrap().is_empty());
    assert_eq!(
        manifest["symbol_inventory"][0]["status"],
        "unsupported_symbol_extractor"
    );
    assert_eq!(
        root_state(manifest, "rules/area.md")["coverage_status"],
        "incomplete_reference_coverage"
    );
    assert_eq!(report["packet"]["evidence"][0]["after"]["content"], source);
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test]
async fn symbols_in_linked_guidance_supplied_as_explicit_evidence_reach_the_parent_instruction() {
    let (temporary, engram, mut request) = fixture();
    let root = temporary.path().join("repo");
    std::fs::write(
        root.join("rules/area.md"),
        "Keep the [shared guide](shared.md).\n",
    )
    .unwrap();
    std::fs::write(root.join("rules/shared.md"), "Preserve `Important`.\n").unwrap();
    std::fs::write(root.join("Site/logic.rs"), TWO_FUNCTIONS).unwrap();
    request["instruction_maintenance"]["base_commit"] = json!(commit(&root));
    request["instruction_maintenance"]["evidence_paths"] =
        json!(["Site/logic.rs", "rules/shared.md"]);
    std::fs::write(
        root.join("Site/logic.rs"),
        TWO_FUNCTIONS.replace("10", "11"),
    )
    .unwrap();
    let report = call(&engram, request).await.unwrap();
    let manifest = &report["packet"]["instruction_manifest"];
    assert!(
        manifest["symbol_references"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["source_path"] == "rules/shared.md" && r["literal"] == "Important")
    );
    assert_eq!(
        root_state(manifest, "rules/area.md")["status"],
        "review_changed_dependencies"
    );
    assert_eq!(
        root_state(manifest, "rules/area.md")["symbol_changes"][0]["status"],
        "changed_observed_candidate_bytes"
    );
    assert_eq!(
        report["packet"]["instructions"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        std::fs::read_to_string(root.join("rules/shared.md")).unwrap(),
        "Preserve `Important`.\n"
    );
    drop(engram);
    temporary.close().unwrap();
}
