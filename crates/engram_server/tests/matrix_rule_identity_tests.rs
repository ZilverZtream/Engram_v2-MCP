#![allow(clippy::unwrap_used)]
use engram_core::{Config, ContentHash, ProjectRecord, RelPath};
use engram_index::IndexDoc;
use engram_server::services::{
    business_logic_service as logic, business_outcome_dependencies as outcomes,
};
use engram_server::{AppState, Engram};
use serde_json::{Value, json};

fn raw_rule(label: &str, reference: &str, line: u32) -> Value {
    json!({"when":"row.owner.project_id > 0","then":label,"source_line":line,"refs":[reference]})
}
fn block<'a>(text: &'a str, rule: &str) -> &'a str {
    let start = text
        .find(&format!(": {rule}\n   Outcome status:"))
        .expect("raw case text");
    let rest = &text[start..];
    &rest[..rest.find("\n\n").unwrap_or(rest.len())]
}

#[tokio::test]
async fn source_warning_identity_blocks_exact_rule_not_ordinals_or_other_fields() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let source = "Class Rules\nPublic Function Save(row As Object) As String\nIf row.owner.project_id > 0 Then Return \"ok\"\nReturn \"no\"\nEnd Function\nEnd Class\n";
    let helper_source = "Class HelperCaller\nPublic Function Save() As Object\nFirst()\nSecond()\nReturn Nothing\nEnd Function\nEnd Class\n";
    std::fs::write(root.join("Rules.vb"), source).unwrap();
    std::fs::write(root.join("HelperCaller.vb"), helper_source).unwrap();
    let (state, _) = AppState::new(Config {
        data_dir: tmp.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })
    .unwrap();
    let pid = "rule-identity-fixture";
    state
        .registry
        .put_project(&ProjectRecord {
            project_id: pid.into(),
            project_name: pid.into(),
            directory: root.to_string_lossy().into_owned(),
            project_type: "general".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            reindex_required_since_ms: None,
        })
        .unwrap();
    state
        .registry
        .set_meta(pid, "active_generation", "1")
        .unwrap();
    let runtime = engram_server::services::project_service::ensure_project_runtime(&state, pid)
        .await
        .unwrap();
    let mut many = raw_rule("many_refs", "row.owner.missing", 3);
    many["refs"] = json!(
        (0..25)
            .map(|i| format!("row.owner.missing_{i}"))
            .collect::<Vec<_>>()
    );
    let multiline = json!({"when":"ready","then":"first\r\n- continuation\n## Data Flow\nlast","source_line":3,"refs":["row.owner.wrong\nnext  "]});
    let inputs = vec![
        ("multiline", "Rules.vb", json!({"purpose":"Multiline","business_rules":[multiline]}), false),
        (
            "a",
            "Rules.vb",
            json!({"purpose":"A","business_rules":[raw_rule("a_clean","row.owner.project_id",3),raw_rule("a_bad","row.owner.owner_id",3)]}),
            false,
        ),
        (
            "b",
            "Rules.vb",
            json!({"purpose":"B","data_flow":"row.owner.unknown_field","business_rules":[raw_rule("b_first","row.owner.project_id",3),raw_rule("b_second","row.owner.project_id",3)]}),
            false,
        ),
        (
            "skips",
            "Rules.vb",
            json!({"purpose":"Skipped","business_rules":["",{"source_line":3,"refs":["row.owner.project_id"]},raw_rule("skip_bad","row.owner.owner_id",3),raw_rule("skip_clean","row.owner.project_id",3)]}),
            false,
        ),
        (
            "capped",
            "Rules.vb",
            json!({"purpose":"Caps","business_rules":[many,raw_rule("after_display_cap","row.owner.owner_id",3)]}),
            false,
        ),
        (
            "legacy",
            "Rules.vb",
            json!({"purpose":"Legacy","business_rules":[raw_rule("legacy_bad","row.owner.owner_id",3)]}),
            true,
        ),
        (
            "combined",
            "HelperCaller.vb",
            json!({"purpose":"Combined","business_rules":[{"when":"Second is reached","then":"combined_bad","source_line":4,"refs":["absent.owner"]}]}),
            false,
        ),
    ];
    let mut docs = Vec::new();
    let mut originals = std::collections::HashMap::new();
    for (id, file, raw, legacy) in inputs {
        let full = if file == "Rules.vb" {
            source
        } else {
            helper_source
        };
        let body = full
            .lines()
            .skip(1)
            .take(full.lines().count() - 2)
            .collect::<Vec<_>>()
            .join("\n");
        let owner = if file == "Rules.vb" {
            "Rules"
        } else {
            "HelperCaller"
        };
        let raw = raw.to_string();
        let mut analysis = logic::parse_llm_response(
            &raw,
            file,
            "Save",
            &format!("{owner}.Save"),
            &ContentHash::compute(body.as_bytes()).0,
        );
        let original = analysis.business_rules.clone();
        logic::attach_method_source_diagnostics(&mut analysis, &raw, &body, 2, "vb");
        assert_eq!(analysis.business_rules, original);
        if legacy {
            analysis.rule_source_diagnostics = None;
        }
        if id == "combined" {
            analysis.outcome_evidence = Some(outcomes::collect(&body, owner, "vb", 2, file, None));
        }
        if id == "skips" {
            assert_eq!(
                analysis
                    .rule_source_diagnostics
                    .as_ref()
                    .unwrap()
                    .rules
                    .iter()
                    .map(|r| r.source_rule_ordinal)
                    .collect::<Vec<_>>(),
                vec![3, 4]
            );
        }
        if id == "capped" {
            assert!(analysis.validation_warnings.len() > 20);
        }
        let content = format!(
            "{}\n_Source: {file}_\n",
            logic::render_method_as_doc(&analysis)
        );
        for rule in &original {
            assert!(content.contains(rule));
        }
        originals.insert(id, original);
        docs.push(IndexDoc {
            generation: 0,
            chunk_id: docs.len() as u64,
            path: RelPath::new(&format!("__business_logic/{file}/{id}.md")),
            language: "markdown".into(),
            content_hash: ContentHash::compute(content.as_bytes()).0,
            content,
            namespace: "business_logic".into(),
            author: None,
            timestamp: None,
            start_line: 0,
            end_line: 0,
            doc_id: id.into(),
        });
    }
    runtime
        .search
        .index_docs(pid, &docs, &tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    let e = Engram::new(state);
    let result = e
        .handle_derive_test_matrix(
            serde_json::from_value(
                json!({"project_id":pid,"files":["Rules.vb","HelperCaller.vb"]}),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let text = &result.content[0].as_text().unwrap().text;
    for (id, index) in [("a", 1), ("skips", 0), ("capped", 1), ("combined", 0), ("multiline", 0)] {
        let case = block(text, &originals[id][index]);
        assert!(
            case.contains("Outcome status: blocked_pending_source_validation"),
            "{case}"
        );
    }
    for (id, index) in [("a", 0), ("b", 0), ("b", 1), ("skips", 1)] {
        let case = block(text, &originals[id][index]);
        assert!(
            !case.contains("Outcome status: blocked_pending_source_validation"),
            "{case}"
        );
        assert!(case.contains("mapped_no_rule_specific_diagnostics_not_verified"));
    }
    let multiline_case = block(text, &originals["multiline"][0]);
    assert!(multiline_case.contains("original rule 1:"));
    assert_eq!(text.matches(&format!(": {}\n   Outcome status:", originals["multiline"][0])).count(), 1);
    assert!(!text.contains(": continuation\n   Outcome status:"));
    let skipped = block(text, &originals["skips"][0]);
    assert!(skipped.contains("original rule 3:"));
    let capped = block(text, &originals["capped"][1]);
    assert!(capped.contains("original rule 2:"));
    let legacy = block(text, &originals["legacy"][0]);
    assert!(legacy.contains("association_unknown"));
    assert!(!legacy.contains("Outcome status: blocked_pending_source_validation"));
    let combined = block(text, &originals["combined"][0]);
    assert!(combined.contains("Other outcome qualification: blocked_pending_helper_outcome"));
    assert!(combined.contains("Reaching context: prerequisites_require_review"));
    assert!(combined.contains("Outcome dependencies:"));
    // Multiple documents reuse local ordinal2. Assertions select exact rule
    // text/document identities, never a global matrix position or search order.
    assert!(text.contains("additional source-check warnings omitted"));
}
