//! Linked guidance is complete evidence without inherited policy authority.
use engram_server::services::project_policy::{self, PolicyBudget};
use std::path::{Path, PathBuf};

const ROOT: &str = "Read [shared](docs/shared.md#authorization) when relevant and [billing](docs/billing.md).\nException: preserve manual approval.\n";
const SHARED: &str = "---\napplyTo: Site/**\n---\nPreserve per-action authorization.\nException: read-only previews do not submit.\nSee [detail](nested.md).\n";
fn write(root: &Path, name: &str, body: &str) {
    let path = root.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let temp = tempfile::Builder::new()
        .prefix("")
        .rand_bytes(2)
        .tempdir_in(parent)
        .unwrap();
    let root = temp.path().join("r");
    write(&root, "AGENTS.md", ROOT);
    write(&root, "docs/shared.md", SHARED);
    write(
        &root,
        "docs/billing.md",
        "---\napplyTo: Billing/**\n---\nUNRELATED_BILLING_BODY\n",
    );
    write(&root, "docs/nested.md", "NESTED_BODY_MUST_NOT_BE_CRAWLED\n");
    (temp, root)
}
fn targets() -> Vec<String> {
    vec!["Site/Form.aspx.vb".into()]
}

#[tokio::test]
async fn prepared_context_binds_linked_recovery_to_its_project() {
    use engram_core::Config;
    use engram_server::{AppState, Engram};
    use serde_json::{Value, json};
    let (temp, root) = fixture();
    write(
        &root,
        "Site/Form.ts",
        "export function linkedPolicyTarget() { return true; }\n",
    );
    drop(git2::Repository::init(&root).unwrap());
    let (state, _) = AppState::new(Config {
        data_dir: temp.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })
    .unwrap();
    let e = Engram::new(state);
    let indexed = e.handle_index_project(serde_json::from_value(json!({"directory":root,"project_name":"Linked context","project_type":"general","wait":true})).unwrap()).await.unwrap();
    let indexed = &indexed.content[0].as_text().unwrap().text;
    let pid = indexed
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .trim();
    let prepared = e.handle_prepare_implementation_context(serde_json::from_value(json!({"project_id":pid,"file_path":"Site/Form.ts","method_name":"linkedPolicyTarget","include_style_profile":false,"output_json":true})).unwrap()).await.unwrap();
    let prepared: Value =
        serde_json::from_str(&prepared.content[0].as_text().unwrap().text).unwrap();
    let policy = &prepared["project_policy_sources"];
    let linked = policy["references"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["path"] == "docs/shared.md")
        .unwrap();
    assert_eq!(policy["linked_sources"][0]["content"], SHARED);
    assert_eq!(linked["read_action"]["tool"], "get_full_method_body");
    assert_eq!(linked["read_action"]["arguments"]["project_id"], pid);
    assert_eq!(
        linked["read_action"]["arguments"]["expected_file_blake3"],
        blake3::hash(SHARED.as_bytes()).to_hex().to_string()
    );
}

#[test]
fn ordinary_context_supplies_first_hop_guidance_without_adopting_scope_or_reading_nested_links() {
    let (_temp, root) = fixture();
    let context =
        project_policy::resolve(&root, &targets(), &[]).with_source_recovery("owned-project");
    assert_eq!(context.sources.len(), 1);
    assert_eq!(context.sources[0].content, ROOT);
    assert_eq!(context.linked_sources.len(), 1);
    let linked = &context.linked_sources[0];
    assert_eq!(linked.path, "docs/shared.md");
    assert_eq!(linked.content, SHARED);
    assert_eq!(
        linked.blake3,
        blake3::hash(SHARED.as_bytes()).to_hex().to_string()
    );
    assert!(!linked.truncated);
    assert!(linked.applicability[0].contains("No authority"));
    let reference = context
        .references
        .iter()
        .find(|r| r.path.as_deref() == Some("docs/shared.md"))
        .unwrap();
    assert_eq!(
        reference.status,
        "supplied_linked_document_applicability_unverified"
    );
    assert_eq!(reference.fragment.as_deref(), Some("authorization"));
    assert_eq!(reference.source_blake3, context.sources[0].blake3);
    assert_eq!(
        reference
            .read_action
            .as_ref()
            .unwrap()
            .arguments
            .expected_file_blake3,
        linked.blake3
    );
    assert!(
        context
            .references
            .iter()
            .any(|r| r.status == "linked_document_outside_declared_scope")
    );
    let markdown = context.render_markdown();
    let json = serde_json::to_string(&context).unwrap();
    assert!(markdown.contains(SHARED));
    assert!(markdown.contains("Linked document evidence") && markdown.contains("does not adopt"));
    assert!(
        !markdown.contains("UNRELATED_BILLING_BODY")
            && !markdown.contains("NESTED_BODY_MUST_NOT_BE_CRAWLED")
    );
    assert!(
        !json.contains("UNRELATED_BILLING_BODY")
            && !json.contains("NESTED_BODY_MUST_NOT_BE_CRAWLED")
    );
    assert_eq!(context.sources_shown + context.linked_sources.len(), 2);
    assert_eq!(
        std::fs::read_to_string(root.join("docs/shared.md")).unwrap(),
        SHARED
    );
}

#[test]
fn direct_policy_keeps_budget_priority_and_omitted_linked_evidence_has_exact_recovery() {
    let (_temp, root) = fixture();
    let budget = PolicyBudget {
        max_sources: 2,
        max_characters_per_source: 4096,
        max_characters_total: ROOT.chars().count() + SHARED.chars().count() - 1,
    };
    let omitted = project_policy::resolve_with_budget(&root, &targets(), &[], budget)
        .with_source_recovery("owned-project");
    assert_eq!(omitted.sources[0].content, ROOT);
    assert!(omitted.linked_sources.is_empty());
    let reference = omitted
        .references
        .iter()
        .find(|r| r.path.as_deref() == Some("docs/shared.md"))
        .unwrap();
    assert_eq!(reference.status, "linked_document_character_budget");
    assert_eq!(
        reference
            .read_action
            .as_ref()
            .unwrap()
            .arguments
            .expected_file_blake3,
        blake3::hash(SHARED.as_bytes()).to_hex().to_string()
    );
    let changed = SHARED.replace("Preserve per-action", "Always preserve per-action");
    write(&root, "docs/shared.md", &changed);
    let refreshed = project_policy::resolve(&root, &targets(), &[]);
    assert_eq!(refreshed.linked_sources[0].content, changed);
    assert_ne!(
        refreshed.linked_sources[0].blake3,
        reference.target_blake3.as_deref().unwrap()
    );
    assert_eq!(refreshed.sources[0].blake3, omitted.sources[0].blake3);
    std::fs::remove_file(root.join("docs/shared.md")).unwrap();
    let missing = project_policy::resolve(&root, &targets(), &[]);
    assert!(missing.linked_sources.is_empty());
    assert!(
        missing
            .references
            .iter()
            .any(|r| r.path.as_deref() == Some("docs/shared.md")
                && r.status == "linked_document_unavailable")
    );
    assert_eq!(missing.reference_coverage, "incomplete");
}

#[test]
fn historical_guidance_never_substitutes_current_linked_source_bytes() {
    let (_temp, root) = fixture();
    let base = {
        let repo = git2::Repository::init(&root).unwrap();
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "Owned policy history", &tree, &[])
            .unwrap()
    };
    write(&root, "docs/shared.md", "CURRENT_ONLY_DOCUMENT_BODY\n");
    let historical = project_policy::resolve_at_revision(&root, base, &targets(), &[])
        .unwrap()
        .with_source_recovery("owned-project");
    assert_eq!(historical.linked_sources.len(), 1);
    assert_eq!(historical.linked_sources[0].content, SHARED);
    assert_eq!(historical.sources[0].content, ROOT);
    assert!(
        historical
            .references
            .iter()
            .all(|r| r.read_action.is_none())
    );
    assert!(
        !serde_json::to_string(&historical)
            .unwrap()
            .contains("CURRENT_ONLY_DOCUMENT_BODY")
    );
    assert!(
        historical
            .references
            .iter()
            .any(|r| r.status == "supplied_linked_document_applicability_unverified")
    );
}
