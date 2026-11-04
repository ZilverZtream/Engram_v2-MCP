//! Desired behavior through the public maintenance workflow, using real Git
//! history and complete documents. No implementation-text assertions.
use super::*;

fn edges(manifest: &Value) -> &[Value] {
    manifest["edges"].as_array().unwrap()
}

#[tokio::test]
async fn commonmark_reference_forms_supply_guidance_and_preserve_exact_source_locations() {
    let (temporary, engram, mut request) = fixture();
    let root = temporary.path().join("repo");
    let policy = concat!(
        "\u{feff}---\r\napplyTo: Site/**\r\n---\r\n",
        "Å [shared rule][  AUTHORIZATION   Rules ].\r\n",
        "Read [authorization rules][] and [AUTHORIZATION RULES].\r\n",
        "Also [with title](<space (rules).md> \"shared guide\").\r\n\r\n",
        "[authorization rules]: shared.md \"first definition\"\r\n",
        "[authorization rules]: must-not-win.md\r\n",
        "[unused]: never-read.md\r\n\r\n",
        "`[code](inline-code.md)` and \\[escaped](escaped.md) are literals.\r\n\r\n",
        "```md\r\n[fenced][unknown]\r\n[unknown]: fenced-code.md\r\n```\r\n\r\n",
        "    [indented](indented-code.md)\r\n"
    );
    std::fs::write(root.join("rules/area.md"), policy).unwrap();
    std::fs::write(
        root.join("rules/space (rules).md"),
        "Additional complete shared guidance.\n",
    )
    .unwrap();
    request["instruction_maintenance"]["base_commit"] = json!(commit(&root));
    request["instruction_maintenance"]["evidence_paths"] = json!(["Site/Helper.vb"]);
    let report = call(&engram, request.clone()).await.unwrap();
    assert_eq!(report["packet"]["version"], 4);
    assert_eq!(
        report["packet"]["instructions"][0]["current"]["content"],
        policy
    );
    let manifest = &report["packet"]["instruction_manifest"];
    let dependencies = manifest["dependencies"].as_array().unwrap();
    assert_eq!(
        dependencies.len(),
        2,
        "Code, unused and shadowed definitions are not dependencies: {manifest}"
    );
    for path in ["rules/shared.md", "rules/space (rules).md"] {
        let dependency = dependencies.iter().find(|d| d["path"] == path).unwrap();
        assert_eq!(dependency["before"]["status"], "complete");
        assert_eq!(dependency["after"]["status"], "complete");
        assert!(
            !dependency["after"]["snapshot"]["content"]
                .as_str()
                .unwrap()
                .is_empty()
        );
    }
    let current = edges(manifest)
        .iter()
        .filter(|e| e["version"] == "after")
        .collect::<Vec<_>>();
    assert_eq!(current.len(), 4);
    let full = current
        .iter()
        .find(|e| e["byte_offset"] == policy.find("[shared rule]").unwrap())
        .unwrap();
    let start = full["byte_offset"].as_u64().unwrap() as usize;
    let end = full["byte_end"].as_u64().unwrap() as usize;
    assert_eq!(
        &policy[start..end],
        "[shared rule][  AUTHORIZATION   Rules ]"
    );
    let definition = full["definition_byte_range"].as_array().unwrap();
    let definition =
        &policy[definition[0].as_u64().unwrap() as usize..definition[1].as_u64().unwrap() as usize];
    assert!(
        definition.starts_with("[authorization rules]: shared.md"),
        "{definition}"
    );
    for edge in current {
        assert_eq!(edge["status"], "complete");
        assert_eq!(edge["kind"], "link");
    }
    assert_eq!(
        manifest["instructions"][0]["coverage_status"],
        "complete_for_supported_collected_references"
    );
    assert_eq!(
        call(&engram, request).await.unwrap()["packet_blake3"],
        report["packet_blake3"]
    );
    assert_eq!(
        std::fs::read_to_string(root.join("rules/area.md")).unwrap(),
        policy
    );
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test]
async fn reference_definitions_are_local_to_the_document_and_requested_history() {
    let (temporary, engram, mut request) = fixture();
    let root = temporary.path().join("repo");
    let old = "[required][rule]\n\n[rule]: old.md\n";
    let current = "[required][rule]\n\n[rule]: current.md\n";
    let other = "[required][rule]\n\n[rule]: other-shared.md\n";
    std::fs::write(root.join("rules/area.md"), old).unwrap();
    std::fs::write(root.join("rules/other.md"), other).unwrap();
    std::fs::write(
        root.join("rules/old.md"),
        "Historical accepted exception.\n",
    )
    .unwrap();
    std::fs::write(
        root.join("rules/other-shared.md"),
        "Unchanged distinct shared guidance.\n",
    )
    .unwrap();
    request["instruction_maintenance"]["base_commit"] = json!(commit(&root));
    request["instruction_maintenance"]["instruction_paths"] =
        json!(["rules/area.md", "rules/other.md"]);
    request["instruction_maintenance"]["evidence_paths"] = json!(["Site/Helper.vb"]);
    std::fs::write(root.join("rules/area.md"), current).unwrap();
    std::fs::write(
        root.join("rules/current.md"),
        "Current replacement requires review.\n",
    )
    .unwrap();
    std::fs::remove_file(root.join("rules/old.md")).unwrap();
    let report = call(&engram, request).await.unwrap();
    let manifest = &report["packet"]["instruction_manifest"];
    for (source, version, target) in [
        ("rules/area.md", "before", "rules/old.md"),
        ("rules/area.md", "after", "rules/current.md"),
        ("rules/other.md", "before", "rules/other-shared.md"),
        ("rules/other.md", "after", "rules/other-shared.md"),
    ] {
        let found = edges(manifest)
            .iter()
            .filter(|e| e["source_path"] == source && e["version"] == version)
            .collect::<Vec<_>>();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0]["normalized_path"], target);
        assert_eq!(found[0]["status"], "complete");
        assert!(found[0]["definition_byte_range"].is_array());
    }
    let old_dependency = manifest["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["path"] == "rules/old.md")
        .unwrap();
    assert_eq!(
        old_dependency["before"]["snapshot"]["content"],
        "Historical accepted exception.\n"
    );
    assert_eq!(old_dependency["after"]["status"], "absent");
    let states = manifest["instructions"].as_array().unwrap();
    let area = states
        .iter()
        .find(|s| s["instruction_path"] == "rules/area.md")
        .unwrap();
    assert_eq!(area["status"], "review_changed_dependencies");
    let other = states
        .iter()
        .find(|s| s["instruction_path"] == "rules/other.md")
        .unwrap();
    assert_eq!(other["status"], "no_collected_dependency_changes");
    assert!(other["dependent_changes"].as_array().unwrap().is_empty());
    assert_eq!(
        report["packet"]["instructions"].as_array().unwrap().len(),
        2
    );
    assert_eq!(
        std::fs::read_to_string(root.join("rules/area.md")).unwrap(),
        current
    );
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test]
async fn missing_explicit_references_and_unsupported_assets_stay_visible_without_invented_dependencies()
 {
    let (temporary, engram, mut request) = fixture();
    let root = temporary.path().join("repo");
    let policy = concat!(
        "[required][missing-rule]\n\n[collapsed][]\n\n[ordinary words]\n\n",
        "[[wiki]]\n\n<a href=\"not-selected.md\">HTML</a>\n\n",
        "![diagram with [literal alt link](not-guidance.md)](diagram.bin)\n"
    );
    std::fs::write(root.join("rules/area.md"), policy).unwrap();
    std::fs::write(root.join("rules/diagram.bin"), [0xff, 0xfe, 0x00]).unwrap();
    request["instruction_maintenance"]["base_commit"] = json!(commit(&root));
    request["instruction_maintenance"]["evidence_paths"] = json!(["Site/Helper.vb"]);
    let report = call(&engram, request).await.unwrap();
    let manifest = &report["packet"]["instruction_manifest"];
    assert!(manifest["dependencies"].as_array().unwrap().is_empty());
    let current = edges(manifest)
        .iter()
        .filter(|e| e["version"] == "after")
        .collect::<Vec<_>>();
    assert_eq!(
        current.len(),
        4,
        "Undefined bracketed prose is not a link: {manifest}"
    );
    for label in ["missing-rule", "collapsed"] {
        let missing = current
            .iter()
            .find(|e| e["reference_label"] == label)
            .unwrap();
        assert_eq!(missing["status"], "unresolved_reference_label");
        assert!(missing["normalized_path"].is_null());
        assert!(missing["definition_byte_range"].is_null());
    }
    assert!(
        current
            .iter()
            .any(|e| e["status"] == "wiki_reference_syntax_unverified")
    );
    assert!(
        current
            .iter()
            .any(|e| e["status"] == "image_reference_unverified" && e["kind"] == "image")
    );
    assert!(
        manifest["coverage_issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["status"] == "html_reference_syntax_unverified")
    );
    assert_eq!(
        manifest["instructions"][0]["status"],
        "needs_reference_evidence"
    );
    assert_eq!(
        manifest["instructions"][0]["coverage_status"],
        "incomplete_reference_coverage"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("rules/area.md")).unwrap(),
        policy
    );
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test]
async fn repeated_reference_links_obey_the_shared_edge_budget_without_truncating_guidance() {
    let (temporary, engram, mut request) = fixture();
    let root = temporary.path().join("repo");
    let policy = format!("{}\n[shared]: shared.md\n", "[shared]\n".repeat(300));
    std::fs::write(root.join("rules/area.md"), &policy).unwrap();
    request["instruction_maintenance"]["base_commit"] = json!(commit(&root));
    request["instruction_maintenance"]["evidence_paths"] = json!(["Site/Helper.vb"]);
    let report = call(&engram, request).await.unwrap();
    let manifest = &report["packet"]["instruction_manifest"];
    assert_eq!(edges(manifest).len(), 256);
    assert_eq!(manifest["dependencies"].as_array().unwrap().len(), 1);
    let omissions = manifest["coverage_issues"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["status"] == "edge_budget_omitted")
        .map(|i| i["omitted_occurrences"].as_u64().unwrap())
        .sum::<u64>();
    assert_eq!(
        omissions, 344,
        "Count every unretained before/after occurrence"
    );
    assert_eq!(
        report["packet"]["instructions"][0]["current"]["content"],
        policy
    );
    assert_eq!(
        manifest["instructions"][0]["coverage_status"],
        "incomplete_reference_coverage"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("rules/area.md")).unwrap(),
        policy
    );
    drop(engram);
    temporary.close().unwrap();
}
