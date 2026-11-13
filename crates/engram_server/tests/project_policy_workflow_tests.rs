#![allow(clippy::unwrap_used)]
//! Independent acceptance fixtures distilled from ExampleApp's instruction workflow.
//! These exercise public context output, without indexing the ExampleApp checkout.

use engram_core::{Config, ProjectRecord, RelPath};
use engram_graph::Node;
use engram_server::{AppState, Engram, GetChangeSetRequest};
use serde_json::{Value, json};
use std::path::Path;

const PID: &str = "example-policy-workflow";
const TARGET: &str = "Site/App_Code/layers/FileLayerZoomPolicy.vb";
const TOPIC: &str = ".github/instructions/map-file-layers.instructions.md";
const AUTHORITY: &str = "The original-project restriction is intentionally UI-only. Server saves still require creator-company ownership.";
const EXCLUSIONS: &str = "Generated report HTML, vendor files, and all of Site/Q/ are excluded, even when a file pattern matches.";

fn write(root: &Path, path: &str, text: &str) {
    let destination = root.join(path);
    std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
    std::fs::write(destination, text).unwrap();
}

fn topic(text: &str) -> String {
    format!(
        "---\napplyTo: \"Site/App_Code/layers/**,Site/modules/dashboard/ts/map/**\"\ndescription: \"Accepted uploaded-layer behavior\"\n---\n{text}\n"
    )
}

async fn fixture() -> (tempfile::TempDir, Engram) {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let (state, _receiver) = AppState::new(Config {
        data_dir: temporary.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 1,
        ..Default::default()
    })
    .unwrap();
    state
        .registry
        .put_project(&ProjectRecord {
            project_id: PID.into(),
            project_name: PID.into(),
            directory: root.to_string_lossy().into_owned(),
            project_type: "dotnet_webforms_vb".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            reindex_required_since_ms: None,
        })
        .unwrap();
    let node = |node_type: &str, name: &str| Node {
        node_id: format!("{node_type}:{TARGET}:{name}"),
        node_type: node_type.into(),
        name: name.into(),
        namespace: "memory".into(),
        language: "vbnet".into(),
        file_path: RelPath::new(TARGET),
        start_line: 0,
        end_line: 2,
        generation: 1,
        metadata: None,
    };
    state
        .graph
        .upsert_nodes(
            PID,
            &[
                node("file", "FileLayerZoomPolicy.vb"),
                node("function", "SaveFileLayerZoom"),
            ],
        )
        .unwrap();
    write(
        &root,
        TARGET,
        "Public Class FileLayerZoomPolicy\nEnd Class\n",
    );
    write(
        &root,
        "AGENTS.md",
        "Read .github/copilot-instructions.md and relevant topic files.\n",
    );
    write(
        &root,
        ".github/copilot-instructions.md",
        "Keep the application's accepted permission boundaries.\n",
    );
    // Publish this fixture's complete flat source generation before attesting
    // legacy authority. A numeric HEAD alone does not establish readable data.
    let runtime = engram_server::services::project_service::ensure_project_runtime(&state, PID).await.unwrap();
    let content = std::fs::read_to_string(root.join(TARGET)).unwrap();
    runtime.search.index_docs(PID, &[engram_index::IndexDoc {
        generation:1,chunk_id:1,path:RelPath::new(TARGET),language:"vbnet".into(),
        content:content.clone(),namespace:"memory".into(),author:None,timestamp:None,
        start_line:1,end_line:2,doc_id:"policy-fixture-source".into(),
        content_hash:engram_core::ContentHash::compute(content.as_bytes()).0,
    }], &tokio_util::sync::CancellationToken::new()).await.unwrap();
    let legacy = state.registry.capture_source_head(PID).unwrap();
    assert!(state.registry.attest_legacy_source_head(&legacy).unwrap());
    drop(runtime);
    (temporary, Engram::new(state))
}

async fn dossier(engram: &Engram, output_json: bool, historical: bool) -> String {
    let mut request = json!({
        "project_id": PID,
        "story": "Change the saved file layer zoom policy",
        "concepts": ["FileLayerZoom"],
        "output_json": output_json,
    });
    if historical {
        request["merged_before"] = json!("2026-07-01");
    }
    let request: GetChangeSetRequest = serde_json::from_value(request).unwrap();
    let result = engram.handle_get_change_set(request).await.unwrap();
    result.content[0].as_text().unwrap().text.clone()
}

fn source<'a>(context: &'a Value, path: &str) -> &'a Value {
    context["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["path"] == path)
        .unwrap_or_else(|| panic!("missing policy {path}: {context}"))
}

async fn ask(engram: &Engram, output: &str, as_of: Value) -> String {
    let request = serde_json::from_value(json!({
        "project_id": PID, "question": "Explain FileLayerZoomPolicy",
        "output_format": output, "as_of": as_of,
    })).unwrap();
    let result = engram.handle_ask_codebase(request).await.unwrap();
    result.content[0].as_text().unwrap().text.clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ask_returns_scoped_complete_policy_and_refreshes_without_reindexing() {
    let (temporary, engram) = fixture().await;
    let root = temporary.path().join("project");
    let content = topic(&format!("{AUTHORITY}\n{EXCLUSIONS}"));
    write(&root, TOPIC, &content);
    write(&root, ".github/instructions/billing.instructions.md",
        "---\napplyTo: 'Billing/**'\n---\nUNRELATED_BILLING_POLICY\n");
    let value: Value = serde_json::from_str(&ask(&engram, "json", Value::Null).await).unwrap();
    assert!(value["evidence"].as_array().unwrap().iter().any(|e| e["path"] == TARGET && e["kind"] == "source_code"), "{value}");
    let policy = &value["project_policy_sources"];
    assert_eq!(source(policy, TOPIC)["content"], content);
    assert_eq!(source(policy, TOPIC)["blake3"], blake3::hash(content.as_bytes()).to_hex().to_string());
    source(policy, "AGENTS.md");
    source(policy, ".github/copilot-instructions.md");
    assert!(!policy.to_string().contains("UNRELATED_BILLING_POLICY"));
    assert_eq!(value["mode"], "retrieval_only");
    let markdown = ask(&engram, "markdown", Value::Null).await;
    assert!(markdown.contains(&content));
    assert!(!markdown.contains("UNRELATED_BILLING_POLICY"));
    let updated = topic("Updated authored guidance; preserve manual zoom overrides.");
    write(&root, TOPIC, &updated);
    let next: Value = serde_json::from_str(&ask(&engram, "json", Value::Null).await).unwrap();
    assert_eq!(source(&next["project_policy_sources"], TOPIC)["content"], updated);
    assert_ne!(source(&next["project_policy_sources"], TOPIC)["blake3"], source(policy, TOPIC)["blake3"]);
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ask_withholds_current_guidance_for_revision_or_advisory_branch() {
    let (temporary, engram) = fixture().await;
    write(&temporary.path().join("project"), TOPIC, &topic("LIVE_POLICY_ONLY"));
    for as_of in [json!({"commit":"previous"}), json!({"branch":"release"})] {
        let value: Value = serde_json::from_str(&ask(&engram, "json", as_of).await).unwrap();
        assert_eq!(value["project_policy_sources"]["status"], "historical_unavailable");
        assert!(value["project_policy_sources"]["sources"].as_array().unwrap().is_empty());
    }
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn change_set_routes_live_topics_and_preserves_policy_in_both_formats() {
    let (temporary, engram) = fixture().await;
    let root = temporary.path().join("project");
    write(&root, TOPIC, &topic(AUTHORITY));
    write(
        &root,
        ".github/instructions/roq-price-list.instructions.md",
        "---\napplyTo: '**/accounting/**,**/RoqPriceList*'\n---\nUNRELATED_ROQ_POLICY\n",
    );
    write(
        &root,
        ".github/instructions/ui-ux.instructions.md",
        &format!("---\napplyTo: 'Site/**/*.vb'\n---\n{EXCLUSIONS}\n"),
    );
    let value: Value = serde_json::from_str(&dossier(&engram, true, false).await).unwrap();
    assert!(
        value["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["path"] == TARGET),
        "fixture must produce an actual candidate: {value}"
    );
    let policy = &value["project_policy_sources"];
    assert_eq!(policy["status"], "present");
    assert!(
        source(policy, TOPIC)["content"]
            .as_str()
            .unwrap()
            .contains(AUTHORITY)
    );
    source(policy, "AGENTS.md");
    source(policy, ".github/copilot-instructions.md");
    let markdown = dossier(&engram, false, false).await;
    for entry in policy["sources"].as_array().unwrap() {
        assert!(
            markdown.contains(entry["path"].as_str().unwrap()),
            "JSON policy source missing from Markdown: {entry}"
        );
    }
    assert!(
        markdown.contains(AUTHORITY),
        "accepted UI/server distinction must reach the planner"
    );
    assert!(
        markdown.contains(EXCLUSIONS),
        "prose exclusions must survive intact as policy evidence"
    );
    assert!(!policy.to_string().contains("UNRELATED_ROQ_POLICY"));
    assert!(!markdown.contains("UNRELATED_ROQ_POLICY"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_and_edited_instruction_files_take_effect_without_reindexing() {
    let (temporary, engram) = fixture().await;
    let root = temporary.path().join("project");
    let initial: Value = serde_json::from_str(&dossier(&engram, true, false).await).unwrap();
    assert!(
        !initial["project_policy_sources"]
            .to_string()
            .contains("NEW_LAYER_POLICY")
    );
    write(
        &root,
        TOPIC,
        &topic("NEW_LAYER_POLICY: preserve zoom overrides."),
    );
    let added: Value = serde_json::from_str(&dossier(&engram, true, false).await).unwrap();
    let added_source = source(&added["project_policy_sources"], TOPIC);
    assert!(
        added_source["content"]
            .as_str()
            .unwrap()
            .contains("NEW_LAYER_POLICY")
    );
    write(
        &root,
        TOPIC,
        &topic("EDITED_LAYER_POLICY: preserve source data too."),
    );
    let edited: Value = serde_json::from_str(&dossier(&engram, true, false).await).unwrap();
    let edited_source = source(&edited["project_policy_sources"], TOPIC);
    assert!(
        edited_source["content"]
            .as_str()
            .unwrap()
            .contains("EDITED_LAYER_POLICY")
    );
    assert_ne!(added_source["blake3"], edited_source["blake3"]);
    let bytes = std::fs::read(root.join(TOPIC)).unwrap();
    assert_eq!(
        edited_source["blake3"],
        blake3::hash(&bytes).to_hex().to_string()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn historical_change_set_reports_unavailable_policy_without_future_content() {
    let (temporary, engram) = fixture().await;
    let root = temporary.path().join("project");
    write(&root, TOPIC, &topic("FUTURE_POLICY_MUST_NOT_LEAK"));
    let json_text = dossier(&engram, true, true).await;
    let value: Value = serde_json::from_str(&json_text).unwrap();
    let policy = &value["project_policy_sources"];
    assert_eq!(policy["status"], "historical_unavailable");
    assert_eq!(policy["coverage"], "not_evaluated");
    assert!(policy["sources"].as_array().unwrap().is_empty());
    assert!(!json_text.contains("FUTURE_POLICY_MUST_NOT_LEAK"));
    let markdown = dossier(&engram, false, true).await;
    assert!(!markdown.contains("FUTURE_POLICY_MUST_NOT_LEAK"));
    assert!(
        markdown.to_ascii_lowercase().contains("historical"),
        "Markdown must disclose missing historical guidance"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revision_pinned_change_set_delivers_the_original_policy_after_live_deletion() {
    let (temporary, engram) = fixture().await;
    let root = temporary.path().join("project");
    let original = topic(AUTHORITY);
    write(&root, TOPIC, &original);
    let revision = {
        let repo = git2::Repository::init(&root).unwrap();
        let mut index = repo.index().unwrap();
        index.add_all(["*"],git2::IndexAddOption::DEFAULT,None).unwrap();index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
        let base = repo.commit(Some("HEAD"),&signature,&signature,"Original layer guidance",&tree,&[]).unwrap();
        write(&root, TOPIC, &topic("FUTURE_COMMITTED_POLICY"));
        index.add_path(Path::new(TOPIC)).unwrap();index.write().unwrap();
        let future_tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let parent = repo.find_commit(base).unwrap();
        repo.commit(Some("HEAD"),&signature,&signature,"Later policy",&future_tree,&[&parent]).unwrap();
        base.to_string()
    };
    std::fs::remove_file(root.join(TOPIC)).unwrap();
    write(&root,".github/copilot-instructions.md","FUTURE_UNCOMMITTED_POLICY\n");
    for output_json in [true,false] {
        let request: GetChangeSetRequest = serde_json::from_value(json!({
            "project_id":PID,"story":"Change saved file layer zoom","concepts":["FileLayerZoom"],
            "as_of_rev":revision,"output_json":output_json,
        })).unwrap();
        let result = engram.handle_get_change_set(request).await.unwrap();
        let text = &result.content[0].as_text().unwrap().text;
        assert!(!text.contains("FUTURE_COMMITTED_POLICY") && !text.contains("FUTURE_UNCOMMITTED_POLICY"));
        if output_json {
            let value: Value = serde_json::from_str(text).unwrap();
            let policy = &value["project_policy_sources"];
            assert_eq!(policy["source_revision"],revision);
            assert_eq!(source(policy,TOPIC)["content"],original);
            assert_eq!(source(policy,TOPIC)["blake3"],blake3::hash(original.as_bytes()).to_hex().to_string());
        } else {
            assert!(text.contains(AUTHORITY) && text.contains(&revision));
        }
    }
    drop(engram);
    temporary.close().unwrap();
}

#[test]
fn malformed_topic_scope_is_visible_and_does_not_become_global_authority() {
    let temporary = tempfile::tempdir().unwrap();
    write(
        temporary.path(),
        ".github/instructions/broken.instructions.md",
        "---\napplyTo: [\n---\nMALFORMED_POLICY_MUST_NOT_APPLY\n",
    );
    let context =
        engram_server::services::project_policy::resolve(temporary.path(), &[TARGET.into()], &[]);
    let value = serde_json::to_value(&context).unwrap();
    assert!(
        !value["sources"]
            .to_string()
            .contains("MALFORMED_POLICY_MUST_NOT_APPLY")
    );
    assert_eq!(value["coverage"], "incomplete");
    assert!(value.to_string().contains("broken.instructions.md"));
    assert!(context.render_markdown().contains("broken.instructions.md"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_dossier_recovers_exact_receipt_and_original_policy_after_checkout_changes() {
    let (temporary, engram) = fixture().await;
    let root = temporary.path().join("project");
    write(&root, TOPIC, &topic("ORIGINAL_POLICY_SNAPSHOT"));
    let request: GetChangeSetRequest = serde_json::from_value(json!({
        "project_id": PID,
        "story": format!("Change the saved file layer zoom policy. {}", "Preserve behavior. ".repeat(3_000)),
        "concepts": ["FileLayerZoom"],
        "output_json": true,
    }))
    .unwrap();
    let response = engram.handle_get_change_set(request).await.unwrap();
    let first = &response.content[0].as_text().unwrap().text;
    assert!(
        first.len() <= 40 * 1024,
        "the whole public response must fit its byte budget"
    );
    let manifest: Value = serde_json::from_str(first).unwrap();
    assert_eq!(manifest["response_delivery"]["status"], "DEFERRED_CONTEXT");
    assert_eq!(manifest["response_delivery"]["complete"], false);
    let id = manifest["response_delivery"]["response_id"]
        .as_str()
        .unwrap();
    assert!(!manifest["reconciliation"].is_null());
    write(
        &root,
        TOPIC,
        &topic("MUTATED_POLICY_AFTER_INITIAL_RESPONSE"),
    );

    let mut next = manifest["response_delivery"]["next_call"].clone();
    let mut original = String::new();
    for _ in 0..20 {
        assert_eq!(next["tool"], "get_change_set");
        let request: GetChangeSetRequest =
            serde_json::from_value(next["arguments"].clone()).unwrap();
        let response = engram.handle_get_change_set(request).await.unwrap();
        let text = &response.content[0].as_text().unwrap().text;
        assert!(text.len() <= 40 * 1024);
        let page: Value = serde_json::from_str(text).unwrap();
        assert_eq!(page["response_id"], id);
        assert_eq!(page["start_byte"], original.len());
        original.push_str(page["content"].as_str().unwrap());
        if page["complete"] == true {
            next = Value::Null;
            break;
        }
        next = page["next_call"].clone();
    }
    assert!(next.is_null(), "bounded recovery must eventually finish");
    assert_eq!(blake3::hash(original.as_bytes()).to_hex().to_string(), id);
    let recovered: Value = serde_json::from_str(&original).unwrap();
    assert_eq!(recovered["reconciliation"], manifest["reconciliation"]);
    assert_eq!(
        recovered["contract_checkpoint"],
        manifest["contract_checkpoint"]
    );
    let policy = source(&recovered["project_policy_sources"], TOPIC);
    assert!(
        policy["content"]
            .as_str()
            .unwrap()
            .contains("ORIGINAL_POLICY_SNAPSHOT")
    );
    assert!(!original.contains("MUTATED_POLICY_AFTER_INITIAL_RESPONSE"));
}

#[test]
fn oversized_and_over_cap_policies_have_explicit_recovery_evidence() {
    let temporary = tempfile::tempdir().unwrap();
    for number in 0..11 {
        let body = if number == 0 {
            "x".repeat(12_000)
        } else {
            format!("POLICY_{number}")
        };
        write(
            temporary.path(),
            &format!(".github/instructions/policy-{number:02}.instructions.md"),
            &topic(&body),
        );
    }
    let context =
        engram_server::services::project_policy::resolve(temporary.path(), &[TARGET.into()], &[]);
    let value = serde_json::to_value(&context).unwrap();
    assert_eq!(value["coverage"], "incomplete");
    assert!(value["sources_discovered"].as_u64().unwrap() >= 11);
    assert!(value["sources_shown"].as_u64().unwrap() < 11);
    let omissions = value["omissions"].as_array().unwrap();
    assert!(!omissions.is_empty());
    let oversized = omissions
        .iter()
        .find(|entry| entry["path"] == ".github/instructions/policy-00.instructions.md")
        .expect("oversized policy must have an explicit recovery entry");
    assert!(
        oversized["blake3"]
            .as_str()
            .is_some_and(|hash| hash.len() == 64)
    );
    assert!(
        value["sources"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["truncated"] == false),
        "policy exceptions must not be lost through partial excerpts"
    );
    for omitted in omissions {
        assert!(
            omitted["reason"]
                .as_str()
                .is_some_and(|text| !text.is_empty())
        );
        assert!(
            !omitted["refetch"].is_null(),
            "omitted policy needs a usable recovery lead: {omitted}"
        );
    }
    let markdown = context.render_markdown();
    assert!(markdown.to_ascii_lowercase().contains("incomplete"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn topic_guidance_remains_discoverable_in_a_large_legacy_source_directory() {
    let (temporary, engram) = fixture().await;
    let root = temporary.path().join("project");
    let directory = root.join("Site/App_Code");
    for number in 0..2_500 {
        std::fs::write(directory.join(format!("legacy-{number:04}.vb")), "").unwrap();
    }
    write(
        &root,
        TOPIC,
        &topic("KEEP_LAYER_PERMISSIONS: preserve accepted access behavior."),
    );
    let value: Value = serde_json::from_str(&dossier(&engram, true, false).await).unwrap();
    let policy = source(&value["project_policy_sources"], TOPIC);
    assert!(
        policy["content"]
            .as_str()
            .unwrap()
            .contains("KEEP_LAYER_PERMISSIONS")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pre_commit_review_supplies_scoped_guidance_without_claiming_prose_enforcement() {
    let (temporary, engram) = fixture().await;
    write(&temporary.path().join("project"), TOPIC, &topic(AUTHORITY));
    let diff = format!(
        "diff --git a/{TARGET} b/{TARGET}\n--- a/{TARGET}\n+++ b/{TARGET}\n@@ -1,2 +1,3 @@\n Public Class FileLayerZoomPolicy\n+    ' Preserve accepted behavior\n End Class\n"
    );
    let skip = engram_server::services::pre_commit_review_service::all_gates()
        .iter()
        .map(|gate| gate.name().to_string())
        .collect::<Vec<_>>();
    let request = json!({"project_id":PID,"diff":diff,"output_json":true,"skip_gates":skip});
    let response = engram
        .handle_pre_commit_review(serde_json::from_value(request.clone()).unwrap())
        .await
        .unwrap();
    let value: Value = serde_json::from_str(&response.content[0].as_text().unwrap().text).unwrap();
    assert!(
        source(&value["project_policy_sources"], TOPIC)["content"]
            .as_str()
            .unwrap()
            .contains(AUTHORITY)
    );
    assert!(
        value["coverage"]["repository_policy"]
            .as_str()
            .unwrap()
            .contains("not mechanically verified")
    );
    let mut request = request;
    request["output_json"] = json!(false);
    let response = engram
        .handle_pre_commit_review(serde_json::from_value(request).unwrap())
        .await
        .unwrap();
    assert!(
        response.content[0]
            .as_text()
            .unwrap()
            .text
            .contains(AUTHORITY)
    );
}
